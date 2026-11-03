//! 远端和本地主机仅移动分组时的真实文件、revision 与加密环境契约。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, fs, sync::Arc};

use kerminal_application::{
    connections::{
        CredentialResolver,
        credentials::{CredentialService, CredentialVaultRepository},
        management::{
            CredentialCleanupStatus, HostAuthenticationMode, HostDraft, HostGroupDraft,
            HostManagementRepository, HostManagementService, LocalHostDraft,
            RecoverableLocalHostSaveOutcome,
        },
    },
    error::{AppError, PublicErrorCode},
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
    preferences::ConfigRevision,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
    vault::VaultStore,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// 用共享真实仓储装配隔离目录，移动前后均由同一应用服务读回。
///
/// @author kongweiguang
fn fixture() -> (TempDir, HostConfigurationRepository, HostManagementService) {
    let root = tempfile::tempdir().expect("创建隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化当前配置");
    let repository = HostConfigurationRepository::new(store);
    let service = HostManagementService::new(Arc::new(repository.clone()));
    (root, repository, service)
}

/// 创建真实分组并返回供后续 CAS 使用的最新整文件 revision。
///
/// @author kongweiguang
async fn create_group(
    service: &HostManagementService,
    id: &str,
    expected_revision: Option<ConfigRevision>,
) -> ConfigRevision {
    service
        .save_group(
            HostGroupDraft {
                id: Some(id.to_owned()),
                name: id.to_owned(),
                sort_order: 0,
            },
            expected_revision,
        )
        .await
        .expect("保存真实分组")
        .revision
        .expect("分组文件 revision")
}

/// 通过当前生产 writer 创建完整远端主机，测试只在已合法文档上附加扩展和装饰。
///
/// @author kongweiguang
fn remote_draft(id: HostId, group_id: &str) -> HostDraft {
    HostDraft {
        id: Some(id),
        group_id: Some(group_id.to_owned()),
        name: "构建节点".to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "example.invalid".to_owned(),
            port: 2222,
            username: "deploy".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: Default::default(),
        serial: None,
        tags: vec!["release".to_owned()],
        is_favorite: true,
        sort_order: 17,
        connect_timeout_ms: 45_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
    }
}

/// 断言移动冲突不会修改主机原字节，缺失目标也不得留下悬空引用。
///
/// @author kongweiguang
fn assert_conflict(result: Result<(), AppError>, path: &std::path::Path, before: &[u8]) {
    assert_eq!(
        result.expect_err("移动必须冲突").public_code(),
        PublicErrorCode::Conflict
    );
    if path.exists() {
        assert_eq!(fs::read(path).expect("读取冲突后的原文"), before);
    }
}

/// 远端移动只改变 group_id、updated_at 和 revision，注释、扩展与连接配置原样保留。
///
/// @author kongweiguang
#[tokio::test]
async fn remote_move_preserves_complete_document_except_group_and_timestamp() {
    let (root, repository, service) = fixture();
    let groups = create_group(&service, "source", None).await;
    create_group(&service, "target", Some(groups)).await;
    let host_id = HostId::parse("remote-move").expect("主机 ID");
    let path = root.path().join("hosts/remote-move.toml");
    service
        .save_host(remote_draft(host_id.clone(), "source"), None)
        .await
        .expect("保存完整远端主机");
    let source = fs::read_to_string(&path)
        .expect("读取生产主机")
        .replace("group_id = \"source\"", "group_id = \"source\" # 分组注释")
        + "\ncustom_extension = { enabled = true, values = [\"a\", \"b\"] }\n\n[third_party]\nowner = \"plugin\"\n";
    let source = source
        .lines()
        .map(|line| {
            if line.starts_with("updated_at = ") {
                format!("{line} # 时间注释")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&path, &source).expect("附加扩展与注释");
    let before = repository
        .load_host(&host_id)
        .await
        .expect("读取远端主机")
        .expect("远端主机存在");
    service
        .move_host_to_group(
            host_id.clone(),
            Some("target".to_owned()),
            before.revision.clone(),
        )
        .await
        .expect("移动远端主机");
    let after = repository
        .load_host(&host_id)
        .await
        .expect("移动后读回")
        .expect("远端主机仍存在");
    let mut expected = before.clone();
    expected.group_id = Some("target".to_owned());
    expected.updated_at = after.updated_at.clone();
    expected.revision = after.revision.clone();
    assert_eq!(after, expected);
    assert_ne!(after.revision, before.revision);

    let moved = fs::read_to_string(&path).expect("读取移动后的原文");
    assert!(moved.contains("group_id = \"target\" # 分组注释"));
    assert!(moved.contains("# 时间注释"));
    let expected_moved = source.replace("group_id = \"source\"", "group_id = \"target\"");
    let expected_source = expected_moved
        .lines()
        .filter(|line| !line.starts_with("updated_at = "))
        .collect::<Vec<_>>();
    let actual_source = moved
        .lines()
        .filter(|line| !line.starts_with("updated_at = "))
        .collect::<Vec<_>>();
    assert_eq!(actual_source, expected_source);
}

/// 带加密环境的本地主机连续移动不读环境、不换引用，vault 与 host 除两标量外保持不变。
///
/// @author kongweiguang
#[tokio::test]
async fn local_move_preserves_encrypted_environment_across_group_and_ungrouped() {
    let root = tempfile::tempdir().expect("创建隔离配置根");
    let configuration = ConfigurationStore::initialize(root.path()).expect("初始化配置");
    let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
    let credentials = CredentialService::new(vault.clone() as Arc<dyn CredentialVaultRepository>);
    let repository: Arc<dyn HostManagementRepository> =
        Arc::new(HostConfigurationRepository::new(configuration));
    let service = HostManagementService::with_credentials(repository, credentials)
        .with_local_environment_resolver(vault as Arc<dyn CredentialResolver>);
    let groups = create_group(&service, "local-source", None).await;
    create_group(&service, "local-target", Some(groups)).await;
    let id = HostId::parse("local-move").expect("本地主机 ID");
    let mut environment = BTreeMap::new();
    environment.insert("PASSWORD".to_owned(), "never-write-this-secret".to_owned());
    environment.insert("PROJECT".to_owned(), "unchanged-project".to_owned());
    let outcome = service
        .save_local_host_recoverable(
            LocalHostDraft {
                id: Some(id.clone()),
                group_id: Some("local-source".to_owned()),
                name: "本地构建".to_owned(),
                shell: "shell-test".to_owned(),
                args: vec!["--login".to_owned()],
                cwd: Some("test-directory".to_owned()),
                env: environment.clone(),
                sort_order: 12,
            },
            None,
            CancellationToken::new(),
        )
        .await
        .expect("保存本地主机");
    let before = match outcome {
        RecoverableLocalHostSaveOutcome::Saved {
            host,
            credential_cleanup: CredentialCleanupStatus::Complete,
        } => host,
        _ => panic!("本地主机未确认保存"),
    };
    let path = root.path().join("hosts/local-move.toml");
    let vault_before = fs::read(root.path().join("secrets/vault.toml")).expect("冻结 vault");

    service
        .move_host_to_group(
            id.clone(),
            Some("local-target".to_owned()),
            before.revision.clone(),
        )
        .await
        .expect("移动到另一分组");
    let grouped = service
        .load_local_host(&id)
        .await
        .expect("读回已分组本地主机")
        .expect("本地主机存在");
    assert_eq!(grouped.group_id.as_deref(), Some("local-target"));
    assert_eq!(grouped.environment_ref, before.environment_ref);
    assert_eq!(
        service
            .read_local_environment(&grouped, &CancellationToken::new())
            .await
            .expect("移动后环境仍可解密"),
        environment
    );

    service
        .move_host_to_group(id.clone(), None, grouped.revision.clone())
        .await
        .expect("移动到未分组");
    let ungrouped = service
        .load_local_host(&id)
        .await
        .expect("读回未分组本地主机")
        .expect("本地主机仍存在");
    assert_eq!(ungrouped.group_id, None);
    assert_eq!(ungrouped.environment_ref, before.environment_ref);
    assert_eq!(
        service
            .read_local_environment(&ungrouped, &CancellationToken::new())
            .await
            .expect("未分组环境仍可解密"),
        environment
    );
    assert_eq!(
        fs::read(root.path().join("secrets/vault.toml")).expect("重读 vault"),
        vault_before
    );
    let raw = fs::read_to_string(path).expect("读取本地主机原文");
    assert!(!raw.contains("never-write-this-secret"));
    assert!(!raw.contains("PASSWORD"));
    assert!(!raw.contains("unchanged-project"));
}

/// 过期 revision、缺失主机和写锁内发现的目标组删除都返回冲突且不产生配置写入。
///
/// @author kongweiguang
#[tokio::test]
async fn stale_missing_and_deleted_destination_never_write_host_documents() {
    let (root, repository, service) = fixture();
    let groups = create_group(&service, "source", None).await;
    create_group(&service, "target", Some(groups)).await;
    let id = HostId::parse("conflict-move").expect("主机 ID");
    let path = root.path().join("hosts/conflict-move.toml");
    service
        .save_host(remote_draft(id.clone(), "source"), None)
        .await
        .expect("保存冲突测试主机");
    let host = repository
        .load_host(&id)
        .await
        .expect("读取主机")
        .expect("主机存在");
    let original = fs::read(&path).expect("冻结原文字节");
    let stale = ConfigRevision::from_sha256("stale-revision".to_owned());
    assert_conflict(
        service
            .move_host_to_group(id.clone(), Some("target".to_owned()), stale)
            .await,
        &path,
        &original,
    );

    let missing_id = HostId::parse("missing-move").expect("缺失主机 ID");
    let missing_path = root.path().join("hosts/missing-move.toml");
    assert_conflict(
        service
            .move_host_to_group(missing_id, Some("target".to_owned()), host.revision.clone())
            .await,
        &missing_path,
        &original,
    );
    assert!(!missing_path.exists());

    let groups_path = root.path().join("hosts/groups.toml");
    let groups_source = fs::read_to_string(&groups_path).expect("读取分组原文");
    let target_start = groups_source
        .find("[[groups]]\nid = \"target\"")
        .expect("目标组条目存在");
    fs::write(&groups_path, &groups_source[..target_start]).expect("模拟目标组被删除");
    assert_conflict(
        service
            .move_host_to_group(id, Some("target".to_owned()), host.revision)
            .await,
        &path,
        &original,
    );
}
