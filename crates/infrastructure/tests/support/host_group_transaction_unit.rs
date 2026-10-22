//! 主机分组事务核心的私有恢复与 CAS 回归。
//!
//! @author kongweiguang

use std::{fs, path::Path};

use crate::storage::CommitMode;
use kerminal_application::PublicErrorCode;
use kerminal_application::connections::management::HostDocumentRevision;
use kerminal_application::preferences::ProfileDraft;
use kerminal_domain::{
    identity::HostId,
    preferences::{ConfigRevision, ConfigurationMode},
};

use super::*;

/// 仅在临时根中写入不含凭据的有效 schema-v1 分组样本。
fn groups_source() -> String {
    concat!(
        "schema_version = 1\n",
        "\n[[groups]]\n",
        "id = \"old\"\nname = \"Old\"\nsort_order = 0\n",
        "created_at = \"1\"\nupdated_at = \"1\"\n",
        "\n[[groups]]\n",
        "id = \"dest\"\nname = \"Destination\"\nsort_order = 1\n",
        "created_at = \"1\"\nupdated_at = \"1\"\n",
    )
    .to_owned()
}

/// 生成满足 host schema v3 的合成主机文件，避免测试夹带任何 secret。
fn host_source(id: &str, group_id: Option<&str>) -> String {
    let group = group_id
        .map(|group_id| format!("group_id = \"{group_id}\"\n"))
        .unwrap_or_default();
    format!(
        "schema_version = 3\nprotocol = \"ssh\"\nid = \"{id}\"\n{group}name = \"Synthetic\"\nhost = \"example.invalid\"\nport = 22\nusername = \"tester\"\nauth_type = \"agent\"\nsort_order = 0\ncreated_at = \"1\"\nupdated_at = \"1\"\n# retained extension\ncustom_field = \"preserve-me\"\n"
    )
}

/// 建隔离配置树并初始化 Store，使事务测试复用生产启动验证路径。
fn initialized_store() -> (tempfile::TempDir, ConfigurationStore) {
    let temp = tempfile::tempdir().expect("创建隔离配置根");
    fs::create_dir_all(temp.path().join("hosts")).expect("创建 hosts 目录");
    fs::write(temp.path().join("hosts/groups.toml"), groups_source())
        .expect("写入合成 groups 文档");
    fs::write(
        temp.path().join("hosts/server-a.toml"),
        host_source("server-a", Some("old")),
    )
    .expect("写入合成主机");
    let store = ConfigurationStore::initialize(temp.path()).expect("初始化隔离配置仓储");
    (temp, store)
}

/// 应用层完整 host revision 快照由实际受支持文件构造。
fn expected_host_revisions(root: &Path) -> Vec<HostDocumentRevision> {
    let source = fs::read_to_string(root.join("hosts/server-a.toml")).expect("读取合成主机");
    vec![HostDocumentRevision {
        host_id: HostId::parse("server-a").expect("合法合成 host ID"),
        revision: revision_for(&source),
    }]
}

/// 删除组前必须匹配完整 host 快照，并保留未知字段后再返回已提交文档。
#[test]
fn group_delete_reassigns_hosts_and_preserves_extensions() {
    let (temp, store) = initialized_store();
    let groups = read_groups_document(temp.path())
        .expect("读取 groups")
        .expect("groups 存在");
    let hosts = expected_host_revisions(temp.path());

    let receipt = store
        .delete_host_group_and_reassign(
            "old".to_owned(),
            Some("dest".to_owned()),
            groups.revision,
            hosts,
        )
        .expect("完成 host/group 原子事务");

    let updated_host =
        fs::read_to_string(temp.path().join("hosts/server-a.toml")).expect("读取迁移后的主机");
    assert!(updated_host.contains("group_id = \"dest\""));
    assert!(updated_host.contains("custom_field = \"preserve-me\""));
    let remaining_groups = parse_group_ids(&receipt.groups.source).expect("解析回执 groups");
    assert!(!remaining_groups.contains("old"));
    assert!(remaining_groups.contains("dest"));
    assert_eq!(receipt.reassigned_hosts, 1);
    assert!(!temp.path().join(HOST_GROUP_JOURNAL).exists());
}

/// 漏传一个 host revision 时事务在写 journal 之前冲突，原文件逐字节不变。
#[test]
fn group_delete_rejects_incomplete_host_snapshot() {
    let (temp, store) = initialized_store();
    let groups = read_groups_document(temp.path())
        .expect("读取 groups")
        .expect("groups 存在");
    let host_before = fs::read(temp.path().join("hosts/server-a.toml")).expect("保存原主机");

    let error = store
        .delete_host_group_and_reassign("old".to_owned(), None, groups.revision, Vec::new())
        .expect_err("不完整 snapshot 必须冲突");

    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        fs::read(temp.path().join("hosts/server-a.toml")).unwrap(),
        host_before
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("hosts/groups.toml")).unwrap(),
        groups_source()
    );
    assert!(!temp.path().join(HOST_GROUP_JOURNAL).exists());
}

/// 锁内检查须包含确认后新出现的本地主机；拒绝不写任何文件，显式移组后仍可继续删除。
#[test]
fn group_delete_rejects_local_references_until_reassigned() {
    let (temp, store) = initialized_store();
    let groups = read_groups_document(temp.path())
        .expect("读取 groups")
        .expect("groups 存在");
    let hosts = expected_host_revisions(temp.path());
    let local = store
        .save_profile_document(
            ProfileDraft {
                id: Some("local-a".to_owned()),
                name: "Local A".to_owned(),
                shell: "fixture-shell".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
                sidebar_group_id: Some("old".to_owned()),
                sort_order: 0,
            },
            None,
        )
        .expect("在分组确认快照后添加本地主机");
    let before = [
        "hosts/groups.toml",
        "hosts/server-a.toml",
        "profiles/local-a.toml",
    ]
    .map(|path| {
        (
            path,
            fs::read(temp.path().join(path)).expect("保存原始字节"),
        )
    });

    for destination in [None, Some("dest".to_owned())] {
        let error = store
            .delete_host_group_and_reassign(
                "old".to_owned(),
                destination,
                groups.revision.clone(),
                hosts.clone(),
            )
            .expect_err("不能删除仍被本地主机引用的组");
        assert_eq!(error.public_code(), PublicErrorCode::Conflict);
        assert_eq!(
            error.public_message(),
            "分组仍有本地主机，请先调整本地主机分组后再删除"
        );
        for (path, bytes) in &before {
            assert_eq!(&fs::read(temp.path().join(path)).unwrap(), bytes);
        }
        assert!(!temp.path().join(HOST_GROUP_JOURNAL).exists());
    }

    let profile = local.profile;
    store
        .save_profile_document(
            ProfileDraft {
                id: Some(profile.id),
                name: profile.name,
                shell: profile.shell,
                args: profile.args,
                cwd: profile.cwd,
                env: profile.env,
                sidebar_group_id: None,
                sort_order: profile.sort_order,
            },
            Some(local.revision),
        )
        .expect("通过现有 Profile CAS 将本地主机移到未分组");
    let receipt = store
        .delete_host_group_and_reassign(
            "old".to_owned(),
            Some("dest".to_owned()),
            groups.revision,
            hosts,
        )
        .expect("本地主机移组后可继续原删除流程");
    assert_eq!(receipt.reassigned_hosts, 1);
    assert!(
        !parse_group_ids(&receipt.groups.source)
            .expect("解析最终分组")
            .contains("old")
    );
    assert!(
        store
            .get_profile_document("local-a".to_owned())
            .expect("读回本地主机")
            .expect("本地主机仍存在")
            .profile
            .sidebar_group_id
            .is_none()
    );
    assert!(!temp.path().join(HOST_GROUP_JOURNAL).exists());
}

/// 现有 serde 可读取的 inline-table groups 文档也应原样风格更新，而不是被误判损坏。
#[test]
fn group_delete_preserves_inline_table_array_encoding() {
    let source = concat!(
        "schema_version = 1\n",
        "groups = [\n",
        "  { id = \"old\", name = \"Old\", sort_order = 0, created_at = \"1\", updated_at = \"1\" },\n",
        "  { id = \"dest\", name = \"Destination\", sort_order = 1, created_at = \"1\", updated_at = \"1\" },\n",
        "]\n",
    );

    let updated = remove_group_from_source(source, "old").expect("移除 inline-table 分组");
    let ids = parse_group_ids(&updated).expect("解析保留的 inline-table groups");

    assert!(!ids.contains("old"));
    assert!(ids.contains("dest"));
    assert!(updated.contains("groups = ["));
}

/// 过期 host revision 不得删文件；只有匹配 revision 的明确请求才能 unlink。
#[test]
fn host_delete_uses_compare_and_swap_revision() {
    let (temp, store) = initialized_store();
    let host_id = HostId::parse("server-a").expect("合法合成 host ID");
    let original = fs::read_to_string(temp.path().join("hosts/server-a.toml")).expect("读取原主机");
    let stale = ConfigRevision::from_sha256("0".repeat(64));

    let error = store
        .delete_host_document(&host_id, &stale)
        .expect_err("过期 revision 应冲突");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        fs::read_to_string(temp.path().join("hosts/server-a.toml")).unwrap(),
        original
    );

    assert!(
        store
            .delete_host_document(&host_id, &revision_for(&original))
            .expect("匹配 revision 的 host 删除成功")
    );
    assert!(!temp.path().join("hosts/server-a.toml").exists());
}

/// 启动恢复可幂等补完已迁移 host，再删除 groups entry 并清除 journal。
#[test]
fn startup_replays_partially_applied_group_journal() {
    let (temp, store) = initialized_store();
    let original_groups = read_groups_document(temp.path())
        .expect("读取 groups")
        .expect("groups 存在");
    let original_host =
        fs::read_to_string(temp.path().join("hosts/server-a.toml")).expect("读取主机");
    let after_host = rewrite_host_group(&original_host, Some("dest")).expect("改写主机");
    let after_groups = remove_group_from_source(&original_groups.source, "old").expect("删除分组");
    let journal = HostGroupJournal {
        schema_version: 1,
        operation: HostGroupOperation {
            deleted_group_id: "old".to_owned(),
            destination_group_id: Some("dest".to_owned()),
        },
        entries: vec![
            HostGroupJournalEntry {
                relative_path: "hosts/server-a.toml".to_owned(),
                before_revision: revision_for(&original_host).as_str().to_owned(),
                after_source: after_host.clone(),
            },
            HostGroupJournalEntry {
                relative_path: "hosts/groups.toml".to_owned(),
                before_revision: original_groups.revision.as_str().to_owned(),
                after_source: after_groups.clone(),
            },
        ],
    };
    write_host_group_journal(temp.path(), &journal).expect("原子写入合成恢复 journal");
    crate::configuration::atomic_replace(
        temp.path(),
        Path::new("hosts/server-a.toml"),
        after_host.as_bytes(),
        || Ok(CommitMode::ReplaceExisting),
    )
    .expect("模拟崩溃前已提交第一个 host");

    recover_host_group_transaction(temp.path(), &store.inner.storage)
        .expect("启动恢复补完 groups 并清理 journal");

    assert_eq!(
        fs::read_to_string(temp.path().join("hosts/server-a.toml")).unwrap(),
        after_host
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("hosts/groups.toml")).unwrap(),
        after_groups
    );
    assert!(!temp.path().join(HOST_GROUP_JOURNAL).exists());
    assert_eq!(store.recovery_state().mode, ConfigurationMode::ReadWrite);
}

/// 未来或损坏 journal 启动时进入可见只读模式，绝不覆盖原始字节。
#[test]
fn future_group_journal_is_preserved_and_read_only() {
    let temp = tempfile::tempdir().expect("创建隔离配置根");
    let marker = temp.path().join(HOST_GROUP_JOURNAL);
    fs::create_dir_all(marker.parent().unwrap()).expect("创建事务目录");
    let original = b"{\"schema_version\":99,\"future\":\"keep\"}";
    fs::write(&marker, original).expect("写入未来 journal fixture");

    let store = ConfigurationStore::initialize(temp.path()).expect("损坏事务不阻断本地启动");

    assert_eq!(
        store.recovery_state().mode,
        ConfigurationMode::ReadOnlyRecovery
    );
    assert!(
        store
            .recovery_state()
            .issues
            .iter()
            .any(|issue| issue.relative_path == HOST_GROUP_JOURNAL)
    );
    assert_eq!(fs::read(&marker).unwrap(), original);
    let error = store
        .ensure_configuration_transactions_absent()
        .expect_err("未恢复的 journal 必须阻断普通配置读写");
    assert_eq!(error.public_code(), PublicErrorCode::ConfigurationCorrupt);
}
