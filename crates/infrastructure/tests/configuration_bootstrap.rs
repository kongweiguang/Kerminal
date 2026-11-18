//! 首次初始化补齐托管 `.gitignore` 与保护缺口可见性的隔离契约测试。
//!
//! 这些用例只使用临时根，不触碰真实用户数据目录；规则集合本身由实现维护，
//! 测试通过公开 validator 与字节幂等性验证行为，不复制第二份规则表。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use kerminal_application::AppError;
use kerminal_application::configuration_sync::ConfigurationSyncService;
use kerminal_application::connections::{
    ConnectionIntent, RemoteConnectionLease, RemoteConnectionService,
};
use kerminal_application::operations::OperationsCommandExecutor;
use kerminal_application::preferences::{
    ConfigurationValidationRepository, ConfigurationValidationScope, ConfigurationValidationService,
};
use kerminal_domain::identity::HostId;
use kerminal_domain::preferences::ConfigurationMode;
use kerminal_infrastructure::configuration::ConfigurationStore;
use kerminal_infrastructure::configuration_sync::GitConfigurationSyncRepository;
use kerminal_infrastructure::operations::StructuredOperationsCommandExecutor;
use kerminal_infrastructure::vault::VaultStore;
use tokio_util::sync::CancellationToken;

/// Git fixture 只执行本机命令；此 fake 明确拒绝远端连接，避免测试触碰任何 SSH 目标。
struct NoRemoteConnections;

#[async_trait::async_trait]
impl RemoteConnectionService for NoRemoteConnections {
    /// 本地 Git 提交门禁不应尝试建立远端连接。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> kerminal_application::AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }
}

/// 每项用例使用独立临时根，避免与并行测试或真实用户根互相污染。
struct TemporaryConfigRoot(PathBuf);

impl TemporaryConfigRoot {
    /// 以 UUID 命名隔离目录，测试退出后只清理自身数据。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kerminal-config-bootstrap-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }

    /// 暴露临时根供用例拼接固定相对布局；不额外创建目录，避免掩盖初始化自身的写入行为。
    ///
    /// @author kongweiguang
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryConfigRoot {
    /// 尽力清理本用例临时目录；清理失败不掩盖测试断言，也不保证失败时一定保留现场。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 用与 `--validate-config` 相同的离线全范围入口确认初始化结果可被外部 Agent 接受。
fn assert_offline_validator_accepts_all(root: &Path) {
    let report = ConfigurationStore::validate_root(root, ConfigurationValidationScope::All)
        .expect("初始化后的配置根应可离线校验");
    assert!(
        report.valid,
        "初始化后 All 校验应通过，诊断：{:?}",
        report.diagnostics
    );
    assert_eq!(report.error_count, 0);
}

/// 首次初始化必须自行建立托管忽略规则，否则 `--validate-config all` 会在 hosts 范围失败；
/// 同时加密 vault 仍需可同步，且初始化不得顺带生成 key 或 vault。
///
/// @author kongweiguang
#[test]
fn fresh_initialization_writes_managed_ignore_rules_and_keeps_vault_syncable() {
    let temp = TemporaryConfigRoot::new();
    ConfigurationStore::initialize(temp.path()).expect("首次初始化配置仓储");

    let ignore =
        fs::read_to_string(temp.path().join(".gitignore")).expect("首次初始化应建立 .gitignore");
    let lines = ignore.lines().map(str::trim).collect::<Vec<_>>();
    assert!(
        lines.contains(&"secrets/vault-key.toml"),
        "必须忽略本地密钥文件"
    );
    // 加密 vault 是设计上可同步的数据，托管规则不能把整个 secrets 目录排除。
    assert!(!lines.contains(&"secrets/vault.toml"));
    assert!(!lines.contains(&"secrets/"));
    assert!(!lines.contains(&"secrets/*"));
    assert!(
        !temp.path().join("secrets/vault-key.toml").exists(),
        "初始化不得创建 key"
    );
    assert!(
        !temp.path().join("secrets/vault.toml").exists(),
        "初始化不得创建 vault"
    );

    assert_offline_validator_accepts_all(temp.path());
}

/// 用户已有规则、注释和 CRLF 前缀必须原样保留，只补充缺失的托管规则；
/// 重复初始化不得再次改写文件，避免启动过程制造无意义的 Git 变更。
///
/// @author kongweiguang
#[test]
fn initialization_preserves_user_ignore_bytes_and_only_appends_missing_rules() {
    let temp = TemporaryConfigRoot::new();
    let user_rules = b"# user rules\r\nbuild/\r\nsecrets/vault-key.toml\r\n";
    fs::write(temp.path().join(".gitignore"), user_rules).expect("写入用户忽略规则");

    ConfigurationStore::initialize(temp.path()).expect("初始化配置仓储");
    let first = fs::read(temp.path().join(".gitignore")).expect("读取合并后的忽略规则");
    assert!(
        first.starts_with(user_rules),
        "用户字节必须原样保留在前缀中"
    );
    let text = String::from_utf8(first.clone()).expect("托管规则保持 UTF-8");
    assert_eq!(
        text.lines()
            .filter(|line| line.trim() == "secrets/vault-key.toml")
            .count(),
        1,
        "已存在的规则不应重复追加"
    );
    assert!(
        text.lines().any(|line| line.trim() == ".storage.lock"),
        "缺失的托管规则应被补充"
    );
    assert_offline_validator_accepts_all(temp.path());

    ConfigurationStore::initialize(temp.path()).expect("重复初始化配置仓储");
    assert_eq!(
        fs::read(temp.path().join(".gitignore")).unwrap(),
        first,
        "重复初始化不得改写已有字节"
    );
}

/// 预置完整托管规则时初始化必须保持字节不变，证明启动路径只做缺失补齐而非重写。
///
/// @author kongweiguang
#[test]
fn initialization_keeps_complete_managed_rules_byte_identical() {
    let baseline = TemporaryConfigRoot::new();
    ConfigurationStore::initialize(baseline.path()).expect("建立托管规则基线");
    let managed = fs::read(baseline.path().join(".gitignore")).expect("读取托管规则基线");

    let temp = TemporaryConfigRoot::new();
    fs::write(temp.path().join(".gitignore"), &managed).expect("预置完整托管规则");
    ConfigurationStore::initialize(temp.path()).expect("已有完整规则时初始化");

    assert_eq!(
        fs::read(temp.path().join(".gitignore")).unwrap(),
        managed,
        "完整规则不应被重写"
    );
    assert_offline_validator_accepts_all(temp.path());
}

/// 沿标准 Error 链查找 typed `AppError::Conflict`，证明保护门禁没有被字符串化包装吞掉。
///
/// 当前工具链的 `io::Error` 不把 `Error::other` 的 custom 原因暴露为 `source()`，
/// 而是保留在 `get_ref()`；两处都检查才能区分 typed 原因与字符串化包装。
///
/// @author kongweiguang
fn error_chain_contains_typed_conflict(error: &(dyn std::error::Error + 'static)) -> bool {
    let is_conflict = |app: &AppError| matches!(app, AppError::Conflict { .. });
    let mut current = Some(error);
    while let Some(node) = current {
        if node.downcast_ref::<AppError>().is_some_and(is_conflict) {
            return true;
        }
        if let Some(io_error) = node.downcast_ref::<std::io::Error>()
            && io_error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<AppError>())
                .is_some_and(is_conflict)
        {
            return true;
        }
        current = node.source();
    }
    false
}

/// `.gitignore` 是不安全路径时启动仍须可加载，并保留原路径与已有配置；
/// 缺失的密钥保护不能静默忽略，必须由离线 validator 和 Git 提交门禁继续严格报告。
///
/// @author kongweiguang
#[tokio::test]
async fn initialization_keeps_unsafe_gitignore_and_keeps_protection_gates_strict() {
    let temp = TemporaryConfigRoot::new();
    fs::create_dir(temp.path().join(".gitignore")).expect("建立不安全的 .gitignore 目录");
    let settings = b"schema_version = 1\nthemeMode = \"light\"\n";
    fs::write(temp.path().join("settings.toml"), settings).expect("写入原始设置");

    let configuration =
        ConfigurationStore::initialize(temp.path()).expect("不安全 .gitignore 不应阻断启动");
    assert_eq!(
        configuration.recovery_state().mode,
        ConfigurationMode::ReadWrite,
        "普通配置不应因保护规则缺失进入恢复态"
    );
    assert!(
        temp.path().join(".gitignore").is_dir(),
        "不得替换或删除原路径"
    );
    assert_eq!(
        fs::read(temp.path().join("settings.toml")).unwrap(),
        settings,
        "启动不得改写已有配置"
    );
    assert!(!temp.path().join("secrets/vault-key.toml").exists());
    assert!(!temp.path().join("secrets/vault.toml").exists());

    // 保护缺口必须留在现有诊断里；validator 的 hosts 范围仍要求密钥忽略规则。
    let report =
        ConfigurationStore::validate_root(temp.path(), ConfigurationValidationScope::Hosts)
            .expect("不安全 .gitignore 仍应返回只读诊断");
    assert!(!report.valid, "缺失密钥忽略规则不能被静默接受");
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == ".gitignore"),
        ".gitignore 诊断应可见：{:?}",
        report.diagnostics
    );

    // 重复启动仍可加载，且不会因为上次补齐失败而改动原路径。
    ConfigurationStore::initialize(temp.path()).expect("重复启动仍可加载");
    assert!(temp.path().join(".gitignore").is_dir());

    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("跳过 Git 提交门禁用例：当前环境未安装 Git");
        return;
    }
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(temp.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("启动隔离 Git 仓库");
    assert!(init.status.success(), "初始化隔离 Git 仓库失败");

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
    let error = service
        .commit(CancellationToken::new())
        .await
        .expect_err("不安全 .gitignore 必须继续阻断 Git 提交");
    assert!(
        error_chain_contains_typed_conflict(&error),
        "Git 提交门禁应保留 typed AppError::Conflict，而不是字符串化底层原因"
    );
    assert!(
        temp.path().join(".gitignore").is_dir(),
        "Git 提交门禁不得替换或删除原路径"
    );
}

/// 已有损坏设置时，初始化仍须进入既有只读恢复并保持原文字节；保护性忽略规则可以补齐，
/// 但不能把整根伪装成“无写入”或绕过恢复判定。该顺序与既有 guide 先播种一致。
///
/// @author kongweiguang
#[test]
fn initialization_keeps_corrupt_settings_bytes_and_read_only_recovery() {
    let temp = TemporaryConfigRoot::new();
    let original = b"schema_version = 99\nprivate_extension = \"untouched\"\n";
    fs::write(temp.path().join("settings.toml"), original).expect("写入损坏设置原文");

    let store = ConfigurationStore::initialize(temp.path()).expect("损坏设置不阻断仓储初始化");
    assert_eq!(
        store.recovery_state().mode,
        ConfigurationMode::ReadOnlyRecovery,
        "损坏设置必须进入既有只读恢复"
    );
    assert_eq!(
        fs::read(temp.path().join("settings.toml")).unwrap(),
        original,
        "恢复初始化不得改写损坏原文"
    );
    // 规则补齐发生在恢复扫描之前，允许发生；这里只确认它没有绕过恢复或改写设置。
    assert!(
        temp.path().join(".gitignore").is_file(),
        "保护性规则应已补齐"
    );
    let report =
        ConfigurationStore::validate_root(temp.path(), ConfigurationValidationScope::Hosts)
            .expect("恢复根仍可离线校验 hosts 范围");
    assert!(
        report.valid,
        "hosts 范围不应因缺少密钥规则失败，诊断：{:?}",
        report.diagnostics
    );
    assert!(
        report
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.path != ".gitignore"),
        "保护性规则应已生效，诊断：{:?}",
        report.diagnostics
    );
}

/// 两个初始化同时进入同一新根时，共享存储锁应串行化规则补齐；
/// 成功后每条托管规则只出现一次，validator 仍可通过。
///
/// @author kongweiguang
#[test]
fn concurrent_initialization_creates_managed_rules_once() {
    let temp = TemporaryConfigRoot::new();
    let root = temp.path().to_path_buf();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| ConfigurationStore::initialize(&root));
        let second = scope.spawn(|| ConfigurationStore::initialize(&root));
        first
            .join()
            .expect("第一个初始化线程不应 panic")
            .expect("第一个并发初始化应成功");
        second
            .join()
            .expect("第二个初始化线程不应 panic")
            .expect("第二个并发初始化应成功");
    });

    let ignore = fs::read_to_string(temp.path().join(".gitignore")).expect("读取托管规则");
    for line in ignore
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let occurrences = ignore
            .lines()
            .filter(|candidate| candidate.trim() == line)
            .count();
        assert_eq!(occurrences, 1, "托管规则不得重复：{line}");
    }
    assert_offline_validator_accepts_all(temp.path());
}

/// 直接使用校验服务（而非仅 `validate_root`）确认初始化后的根在应用内路径同样有效。
///
/// @author kongweiguang
#[tokio::test]
async fn initialized_root_passes_configuration_validation_service_all_scope() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(temp.path()).expect("初始化配置仓储");
    let repository: Arc<dyn ConfigurationValidationRepository> = Arc::new(store);
    let service = ConfigurationValidationService::new(repository);

    let report = service
        .validate(ConfigurationValidationScope::All)
        .await
        .expect("执行应用内全范围校验");
    assert!(
        report.valid,
        "应用内 All 校验应通过，诊断：{:?}",
        report.diagnostics
    );
    assert_eq!(report.error_count, 0);
}
