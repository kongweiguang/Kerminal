//! 迁移恢复期只读目录必须继续提供旧外置主机 codec，避免数据迁移末段重新失败。
//!
//! @author kongweiguang

use std::path::{Path, PathBuf};

use kerminal_application::plugins::PluginId as RegistryPluginId;

/// 夹具来自仓库根；复制到隔离根后仍保持 journal 的相对路径与摘要一致。
fn fixture_root(name: &str) -> (tempfile::TempDir, PathBuf) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/plugins-layout-v1")
        .join(name);
    let temp = tempfile::tempdir().expect("隔离配置根");
    let root = temp.path().join("root");
    copy_tree(&source, &root);
    (temp, root)
}

fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("创建夹具目标");
    for entry in std::fs::read_dir(source).expect("读取夹具目录") {
        let entry = entry.expect("夹具目录项");
        let destination = target.join(entry.file_name());
        let metadata = entry.metadata().expect("夹具元数据");
        if metadata.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), &destination).expect("复制夹具文件");
        }
    }
}

/// Prepared、OldMoved、NewInstalled 都可能因旧 state 已移走或未写 schema 2 而暂停；
/// 三种时刻都必须从 journal/backup 恢复旧主机声明，不能退回内置目录。
#[test]
fn recovered_migration_keeps_legacy_host_codecs() {
    for fixture in [
        "recovery-prepared",
        "recovery-old-moved",
        "recovery-new-installed",
        "recovery-committed",
        "normal",
    ] {
        let (_temp, root) = fixture_root(fixture);
        let (_, hosts) = crate::plugin_catalogs::migration_catalogs(&root)
            .unwrap_or_else(|error| panic!("{fixture}: {error:?}"));
        assert!(
            hosts.get("acme.notes", "acme.notes/example").is_some(),
            "{fixture} 缺少旧外置主机 codec"
        );
    }
}

/// 持久停用只收窄可用视图，不能删掉声明：已有主机仍要按原类型解析，
/// 只是打开、探测和入口会被门禁拒绝。四个查询分别覆盖插件、工具、内容与主机，
/// 防止某个入口忘了读取 owner 而继续放行。
#[test]
fn disabled_builtin_keeps_declarations_but_hides_runtime_capabilities() {
    use std::collections::BTreeMap;

    let states = BTreeMap::from([(
        RegistryPluginId::new("kerminal.sftp").expect("sftp identity"),
        false,
    )]);
    let (registry, hosts) =
        crate::plugin_catalogs::catalogs_for_plugins_inner(&[], &[], "en", &states, false)
            .expect("停用内置单元后目录仍须可冻结");
    assert!(!registry.is_enabled("kerminal.sftp"));
    assert!(!registry.tool_enabled("sftp"));
    assert!(!registry.content_type_enabled("sftpTransfer"));
    assert!(!registry.host_type_enabled("sftp"));
    assert!(
        registry
            .host_types()
            .iter()
            .any(|host| host.configuration_type == "sftp"),
        "停用不能删除主机声明"
    );
    assert!(
        hosts.get("kerminal", "sftp").is_some(),
        "已有主机仍须能解析"
    );
    assert!(registry.is_enabled("kerminal.ssh"));
    assert!(registry.is_enabled("kerminal.core"));
    assert!(registry.content_type_enabled("terminal"));
}

/// 旧数据可能把 `kerminal.*` 保留身份写成普通插件；迁移目录必须跳过外部注册，
/// 同时保留内置声明。否则 provider 重复会让冻结失败，旧主机也无法在恢复期读取。
#[test]
fn legacy_reserved_plugin_identity_is_not_registered_as_external() {
    let temp = tempfile::tempdir().expect("隔离旧插件根");
    let root = temp.path();
    let plugins = root.join("plugins");
    let package = plugins.join("packages/kerminal.ssh/1.0.0");
    std::fs::create_dir_all(package.join("schemas")).expect("创建旧包目录");
    std::fs::write(
        plugins.join("state.json"),
        r#"{
  "schema": 1,
  "revision": 3,
  "plugins": {
    "kerminal.ssh": {
      "active": "1.0.0",
      "enabled": true,
      "source": "local",
      "installed_at": "2026-01-02T03:04:05Z"
    }
  },
  "pending": [],
  "failures": {},
  "pending_failures": [],
  "boot": null
}"#,
    )
    .expect("写入旧状态");
    std::fs::write(
        package.join("kerminal-plugin.json"),
        r#"{
  "schema": 1,
  "id": "kerminal.ssh",
  "name": { "en": "Legacy SSH" },
  "description": { "en": "Legacy SSH package" },
  "version": "1.0.0",
  "api": "^1.0",
  "publisher": "kerminal",
  "runtime": { "kind": "none" },
  "permissions": [],
  "contributes": {
    "host_types": [
      { "id": "example", "title": "Example SSH", "schema": "schemas/host.json" }
    ]
  },
  "views": {}
}"#,
    )
    .expect("写入旧清单");
    std::fs::write(
        package.join("schemas/host.json"),
        r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "properties": { "address": { "type": "string", "title": "Address" } },
  "additionalProperties": false
}"#,
    )
    .expect("写入旧主机 schema");

    let (registry, hosts) = crate::plugin_catalogs::legacy_catalogs(root)
        .expect("保留身份旧材料应被跳过而不是让冻结失败");
    // 内置 ssh 单元仍由内置声明拥有，旧材料不能替换或重复其身份。
    assert!(registry.is_enabled("kerminal.ssh"));
    assert!(
        registry
            .provider("kerminal.ssh")
            .is_some_and(|provider| provider.source.is_builtin())
    );
    assert_eq!(
        registry
            .owner_of_host_type("host.ssh")
            .map(RegistryPluginId::as_str),
        Some("kerminal.ssh")
    );
    assert!(hosts.get("kerminal", "ssh").is_some());
    // 跳过的外部 codec 不应进入目录；内置 codec 已覆盖同一协议。
    assert!(hosts.get("kerminal.ssh", "kerminal.ssh/example").is_none());
}

/// 迁移前目录必须纯只读：不建锁、不写状态、不改旧包或备份材料。
#[test]
fn migration_catalogs_never_write_to_the_root() {
    for fixture in ["normal", "recovery-prepared", "recovery-old-moved"] {
        let (_temp, root) = fixture_root(fixture);
        let before = snapshot(&root);
        let _ = crate::plugin_catalogs::migration_catalogs(&root)
            .unwrap_or_else(|error| panic!("{fixture}: {error:?}"));
        assert_eq!(snapshot(&root), before, "{fixture} 被写入");
        assert!(!root.join(".storage.lock").exists(), "{fixture} 创建了锁");
    }
}

fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, current: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(current).expect("读取快照目录") {
            let entry = entry.expect("快照项");
            let path = entry.path();
            let metadata = entry.metadata().expect("快照元数据");
            if metadata.is_dir() {
                walk(root, &path, entries);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("相对路径")
                    .to_string_lossy()
                    .replace('\\', "/");
                entries.push((relative, std::fs::read(&path).expect("快照内容")));
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

/// journal 仍在但备份包无法读取时必须原样报错；不能退回内置目录把证据缺口藏起来。
#[test]
fn broken_backup_manifest_is_reported_instead_of_falling_back() {
    let (_temp, root) = fixture_root("recovery-old-moved");
    let operation = std::fs::read_dir(root.join("plugins/.migration"))
        .expect("迁移目录")
        .next()
        .expect("操作目录")
        .expect("操作项")
        .path();
    std::fs::write(
        operation.join("before/packages/acme.notes/1.0.0/kerminal-plugin.json"),
        b"not json",
    )
    .expect("破坏备份清单");
    assert!(crate::plugin_catalogs::migration_catalogs(&root).is_err());
}

/// 新能力插件（api `>=1.1.0 <2.0.0`）必须按权威宿主契约被目录装配接受。
/// 本文件同时被主程序与 `kerminal-migrate` 两个编译入口以 `#[path]` 共享引入，
/// 测试在两个入口下都编译：任何入口退回 1.0 字面量都会在此失败，
/// 以此核对两个编译入口引用的是同一权威版本而不是仅替换测试数据。
///
/// @author kongweiguang
#[test]
fn external_catalogs_use_authoritative_host_api_version() {
    use std::{collections::BTreeMap, sync::Arc};

    use kerminal_application::external_contributions::PreparedPlugin;
    use kerminal_domain::plugins::{PluginId as ExternalPluginId, PluginVersion};
    use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};
    use serde_json::json;

    let document = json!({
        "schema":1,"id":"acme.qrcode","name":{"en":"QR Code"},
        "description":{"en":"QR tools"},"version":"1.0.0",
        "api":">=1.1.0 <2.0.0",
        "publisher":"acme","runtime":{"kind":"none"},
        "views":{},"contributes":{}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&document).expect("序列化清单"),
        ManifestPolicy::ThirdParty,
    )
    .expect("新能力清单有效");
    let prepared = PreparedPlugin {
        id: ExternalPluginId::parse("acme.qrcode").expect("插件身份"),
        version: PluginVersion::parse("1.0.0").expect("插件版本"),
        directory: std::path::PathBuf::from("unused"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::new(),
        resources: BTreeMap::new(),
    };
    let (registry, _) = crate::plugin_catalogs::catalogs_for_plugins_inner(
        &[Arc::new(prepared)],
        &[],
        "en",
        &BTreeMap::new(),
        false,
    )
    .expect("1.1 范围插件在权威宿主契约下必须可装配");
    assert!(
        registry.plugin("acme.qrcode").is_some(),
        "新能力插件必须进入目录"
    );
}
