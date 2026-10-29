//! 配置 Git 同步的隔离仓库行为测试。
//!
//! @author kongweiguang

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::Arc,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use kerminal_application::configuration_sync::{
    ConfigurationSyncOutcome, ConfigurationSyncService,
};
use kerminal_application::connections::{
    ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest,
    RemoteConnectionLease, RemoteConnectionService,
};
use kerminal_application::operations::OperationsCommandExecutor;
use kerminal_application::{AppError, AppResult, preferences::ConfigurationRepository};
use kerminal_domain::identity::HostId;
use kerminal_domain::preferences::ConfigDocumentPath;
use kerminal_infrastructure::{
    configuration::ConfigurationStore, configuration_sync::GitConfigurationSyncRepository,
    operations::StructuredOperationsCommandExecutor, vault::VaultStore,
};

/// Git fixture 只执行本机命令；此 fake 明确拒绝远端连接，避免测试触碰任何 SSH 目标。
struct NoRemoteConnections;

#[async_trait]
impl RemoteConnectionService for NoRemoteConnections {
    /// fixture 的 Git 工作区操作不应建立远端 SSH lease。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }

    /// fixture 不读取或信任任何主机密钥。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }

    /// fixture 不写入 known_hosts。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

struct TemporaryGitRoot(PathBuf);

impl TemporaryGitRoot {
    /// 每个用例使用独立临时 Git 根，避免读取或修改真实用户配置和远端。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kerminal-config-sync-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("创建隔离 Git 根目录");
        Self(root)
    }
}

impl Drop for TemporaryGitRoot {
    /// 测试结束后只删除本用例 UUID 命名的临时根目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 运行 Git fixture 命令并仅在隔离仓库内检查退出状态。
fn git(root: &PathBuf, arguments: &[&str]) -> Output {
    Command::new("git")
        .args(arguments)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("Git 已安装且可启动")
}

/// 端到端确认提交保留用户 ignore，并排除密钥、运行态及即使 force-add 的本地迁移材料。
/// 提交后读取真实根目录状态，覆盖 Git show-prefix 返回换行时仍应识别仓库的边界。
#[tokio::test]
async fn commit_includes_sync_assets_and_excludes_local_only_state() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("跳过配置同步 Git fixture：当前环境未安装 Git");
        return;
    }

    let temp = TemporaryGitRoot::new();
    fs::write(
        temp.0.join(".gitignore"),
        "# user-owned rule\nlocal-only.txt\n",
    )
    .expect("写入用户忽略规则");
    let configuration = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let vault = VaultStore::open(configuration.storage_coordinator());
    let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(
        StructuredOperationsCommandExecutor::new(Arc::new(NoRemoteConnections)),
    );
    let service = ConfigurationSyncService::new(Arc::new(
        GitConfigurationSyncRepository::new(
            configuration.clone(),
            vault,
            executor,
            tokio::runtime::Handle::current(),
        )
        .expect("绑定隔离仓储根目录"),
    ));

    for arguments in [
        &(["init", "--quiet"][..]),
        &["config", "user.name", "Kerminal Fixture"],
        &["config", "user.email", "fixture@example.invalid"],
    ] {
        let output = git(&temp.0, arguments);
        assert!(output.status.success(), "Git fixture 初始化失败");
    }
    let branch = git(&temp.0, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    assert!(branch.status.success(), "设置 fixture 分支失败");

    fs::write(temp.0.join("settings.toml"), "theme = \"dark\"\n")
        .expect("写入合成 settings fixture");
    fs::create_dir_all(temp.0.join("snippets")).expect("创建 snippets fixture 目录");
    fs::write(
        temp.0.join("snippets").join("custom query.toml"),
        "id = \"custom-query\"\n",
    )
    .expect("写入含空格路径的片段 fixture");
    fs::create_dir_all(temp.0.join("secrets")).expect("创建 synthetic vault 目录");
    fs::write(
        temp.0.join("secrets/vault.toml"),
        b"synthetic encrypted fixture",
    )
    .expect("写入合成加密 vault 内容");
    fs::write(
        temp.0.join("secrets/vault-key.toml"),
        b"synthetic local key fixture",
    )
    .expect("写入合成本地 key fixture");
    fs::create_dir_all(temp.0.join("workspace")).expect("创建 workspace fixture 目录");
    fs::write(
        temp.0.join("workspace/session.json"),
        b"synthetic runtime fixture",
    )
    .expect("写入合成 workspace 运行态");
    fs::write(temp.0.join("local-only.txt"), "user ignored file").expect("写入用户自定义忽略文件");
    fs::write(
        temp.0.join(".kerminal-format.toml"),
        "synthetic local format marker",
    )
    .expect("写入本地格式标记");
    let migration = temp.0.join(".kerminal-migration");
    fs::create_dir_all(migration.join("before")).expect("创建迁移前备份目录");
    fs::create_dir_all(migration.join("staged")).expect("创建迁移暂存目录");
    fs::write(migration.join("active.json"), "synthetic migration marker")
        .expect("写入迁移 marker");
    fs::write(
        migration.join("journal.json"),
        "synthetic migration journal",
    )
    .expect("写入迁移 journal");
    fs::write(
        migration.join("before/settings.toml"),
        "synthetic before image",
    )
    .expect("写入迁移前文件");
    fs::write(
        migration.join("staged/settings.toml"),
        "synthetic staged image",
    )
    .expect("写入迁移暂存文件");
    let force_add = git(
        &temp.0,
        &[
            "add",
            "--force",
            "--",
            ".kerminal-format.toml",
            ".kerminal-migration/",
        ],
    );
    assert!(force_add.status.success(), "force-add 本地迁移材料失败");

    let outcome = service
        .commit(CancellationToken::new())
        .await
        .expect("提交隔离配置变更");
    assert_eq!(outcome.outcome, ConfigurationSyncOutcome::Succeeded);
    let status = service
        .status()
        .await
        .expect("根目录的换行输出不应误判为子目录");
    assert!(status.git_available);
    assert_eq!(
        status.repository,
        kerminal_application::configuration_sync::SyncRepositoryState::NoUpstream,
    );
    assert!(
        outcome
            .commit_hash
            .as_deref()
            .is_some_and(|hash| hash.len() >= 40)
    );

    let committed = git(&temp.0, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(committed.status.success(), "读取 fixture 提交树失败");
    let committed_paths = String::from_utf8_lossy(&committed.stdout);
    assert!(committed_paths.contains("settings.toml"));
    assert!(committed_paths.contains("snippets/custom query.toml"));
    assert!(committed_paths.contains("secrets/vault.toml"));
    assert!(committed_paths.contains(".gitignore"));
    assert!(!committed_paths.contains("vault-key.toml"));
    assert!(!committed_paths.contains("workspace/session.json"));
    assert!(!committed_paths.contains("local-only.txt"));
    assert!(!committed_paths.contains(".kerminal-format.toml"));
    for migration_path in [
        ".kerminal-migration/active.json",
        ".kerminal-migration/journal.json",
        ".kerminal-migration/before/settings.toml",
        ".kerminal-migration/staged/settings.toml",
    ] {
        assert!(
            !committed_paths.contains(migration_path),
            "受保护迁移材料不应进入提交：{migration_path}"
        );
    }

    let staged = git(&temp.0, &["diff", "--cached", "--name-only"]);
    assert!(staged.status.success(), "读取剩余暂存路径失败");
    let staged_paths = String::from_utf8_lossy(&staged.stdout);
    assert!(staged_paths.contains(".kerminal-format.toml"));
    for migration_path in [
        ".kerminal-migration/active.json",
        ".kerminal-migration/journal.json",
        ".kerminal-migration/before/settings.toml",
        ".kerminal-migration/staged/settings.toml",
    ] {
        assert!(
            staged_paths.contains(migration_path),
            "被 force-add 的受保护材料应留在索引中：{migration_path}"
        );
    }

    let ignore = fs::read_to_string(temp.0.join(".gitignore")).expect("读取合并后的 ignore");
    assert!(ignore.starts_with("# user-owned rule\nlocal-only.txt\n"));
    assert!(ignore.contains("secrets/vault-key.toml"));
    assert!(ignore.contains("secrets/.vault-transaction/"));
    assert!(
        ignore
            .lines()
            .any(|line| line.trim() == ".kerminal-format.toml")
    );
    assert!(
        ignore
            .lines()
            .any(|line| line.trim() == ".kerminal-migration/")
    );
    let config_read = configuration
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("回读测试设置")
        .expect("测试设置存在");
    assert_eq!(config_read.source, "theme = \"dark\"\n");
}

/// 配置状态快照与检出共享 `.storage.lock`，活动片段 marker 存在时必须保留文件并 fail closed。
#[tokio::test]
async fn status_rejects_unresolved_snippet_import_marker() {
    let temporary = TemporaryGitRoot::new();
    let configuration = ConfigurationStore::initialize(&temporary.0).expect("初始化隔离配置仓储");
    let vault = VaultStore::open(configuration.storage_coordinator());
    let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(
        StructuredOperationsCommandExecutor::new(Arc::new(NoRemoteConnections)),
    );
    let repository = GitConfigurationSyncRepository::new(
        configuration,
        vault,
        executor,
        tokio::runtime::Handle::current(),
    )
    .expect("绑定隔离仓储根目录");
    let service = ConfigurationSyncService::new(Arc::new(repository));
    let transaction = temporary.0.join(".storage-transactions/snippet-import");
    fs::create_dir_all(&transaction).expect("创建受保护事务目录");
    let marker = transaction.join("active.json");
    let original = b"unresolved synthetic transaction";
    fs::write(&marker, original).expect("写入合成活动 marker");

    let result = service.status().await;
    assert!(matches!(result, Err(AppError::OutcomeUnknown { .. })));
    assert_eq!(fs::read(marker).expect("活动 marker 保持原样"), original);
}
