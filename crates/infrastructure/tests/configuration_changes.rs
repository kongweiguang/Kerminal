//! 文件 watcher 与动态只读恢复态的隔离契约测试。
//!
//! @author kongweiguang

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kerminal_application::configuration_changes::{
    ConfigurationChangeMessage, ConfigurationChangeService,
};
use kerminal_application::preferences::ConfigurationRepository;
use kerminal_domain::preferences::{ConfigDocumentPath, ConfigurationMode};
use kerminal_infrastructure::configuration::ConfigurationStore;
use kerminal_infrastructure::configuration_changes::ConfigurationChangeWatcher;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct TemporaryConfigRoot(PathBuf);

impl TemporaryConfigRoot {
    /// 每个 watcher 回归使用独立临时目录，避免读取或改写真实 `~/.kerminal`。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("kerminal-watch-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }
}

impl Drop for TemporaryConfigRoot {
    /// 仅删除本测试创建的随机临时根。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 读取变更流直到收到指定恢复态，过滤同一次文件替换产生的多条平台事件。
async fn wait_for_mode(
    subscription: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    expected: ConfigurationMode,
) {
    let cancellation = CancellationToken::new();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match subscription.recv(cancellation.clone()).await {
                ConfigurationChangeMessage::Changed(event)
                    if event.recovery_state.mode == expected =>
                {
                    return;
                }
                ConfigurationChangeMessage::Changed(_)
                | ConfigurationChangeMessage::RescanRequired => {}
                ConfigurationChangeMessage::Cancelled => panic!("测试取消 token 意外触发"),
                ConfigurationChangeMessage::Closed => panic!("配置变更通道意外关闭"),
            }
        }
    })
    .await
    .expect("等待配置状态变更");
}

#[tokio::test]
/// 外部编辑损坏文件时切入只读，修复后重新放开写入，应用退出会按 deadline join watcher。
async fn external_invalid_configuration_enters_and_leaves_recovery_mode() {
    let temp = TemporaryConfigRoot::new();
    let configuration = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let service = ConfigurationChangeService::new(Arc::new(configuration.clone()));
    let mut changes = service.subscribe();
    let mut watcher =
        ConfigurationChangeWatcher::start(configuration.clone()).expect("启动隔离配置 watcher");

    fs::write(temp.0.join("settings.toml"), "schema_version = [\n").expect("写入损坏设置 fixture");
    wait_for_mode(&mut changes, ConfigurationMode::ReadOnlyRecovery).await;
    let blocked = configuration
        .write(
            ConfigDocumentPath::Settings,
            "schema_version = 1\n".to_owned(),
            None,
        )
        .await
        .expect_err("损坏文件存在时阻止应用写入");
    assert_eq!(blocked.public_code().as_str(), "configuration_corrupt");

    fs::write(temp.0.join("settings.toml"), "schema_version = 1\n").expect("修复设置 fixture");
    wait_for_mode(&mut changes, ConfigurationMode::ReadWrite).await;
    let current = configuration
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读取修复后的设置")
        .expect("设置文件存在");
    configuration
        .write(
            ConfigDocumentPath::Settings,
            "schema_version = 1\n".to_owned(),
            Some(current.revision),
        )
        .await
        .expect("修复后重新允许配置写入");

    watcher
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("按关闭期限停止并 join watcher");
}

/// 真实 Linux watcher 忽略自身校验读取，同时保留直接写入与原子替换通知。
///
/// @author kongweiguang
#[cfg(target_os = "linux")]
#[tokio::test]
async fn snippet_read_access_does_not_loop_but_external_write_still_notifies() {
    let temp = TemporaryConfigRoot::new();
    let snippets = temp.0.join("snippets");
    fs::create_dir_all(&snippets).expect("创建隔离片段目录");
    let snippet_path = snippets.join("watcher-loop.toml");
    let original = "schema_version = 1\nid = \"watcher-loop\"\ntitle = \"Watcher loop\"\ncommand = \"echo one\"\nscope = \"any\"\nsort_order = 0\ncreated_at = \"2026-10-18T00:00:00Z\"\nupdated_at = \"2026-10-18T00:00:00Z\"\n";
    fs::write(&snippet_path, original).expect("创建有效片段 fixture");
    let configuration = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    assert_eq!(
        configuration.recovery_state().mode,
        ConfigurationMode::ReadWrite,
        "片段 fixture 必须先通过启动配置校验"
    );

    let service = ConfigurationChangeService::new(Arc::new(configuration.clone()));
    let mut changes = service.subscribe();
    let mut watcher =
        ConfigurationChangeWatcher::start(configuration).expect("启动隔离配置 watcher");

    let verification: Result<(), String> = async {
        let entries = fs::read_dir(&snippets)
            .map_err(|_| "无法枚举隔离片段目录".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "无法读取隔离片段目录项目".to_owned())?;
        if entries.len() != 1 {
            return Err("隔离片段目录内容与 fixture 不符".to_owned());
        }
        let contents =
            fs::read_to_string(&snippet_path).map_err(|_| "无法读取隔离片段 fixture".to_owned())?;
        if !contents.contains("echo one") {
            return Err("读取的片段内容与 fixture 不符".to_owned());
        }
        assert_configuration_watcher_quiet(&mut changes, Duration::from_millis(500)).await?;

        let updated = original.replace("echo one", "echo two");
        fs::write(&snippet_path, updated.as_bytes())
            .map_err(|_| "无法更新隔离片段 fixture".to_owned())?;
        wait_for_changed_configuration_path(&mut changes, "snippets/watcher-loop.toml").await?;

        // 写入触发的恢复检查也会读取同一片段；其读取访问不能形成第二轮配置变化。
        assert_configuration_watcher_quiet(&mut changes, Duration::from_millis(500)).await?;

        let atomic_temp_path = snippets.join("watcher-loop.atomic-tmp");
        let replaced = updated.replace("echo two", "echo three");
        fs::write(&atomic_temp_path, replaced)
            .map_err(|_| "无法写入原子替换的临时 fixture".to_owned())?;
        fs::rename(&atomic_temp_path, &snippet_path)
            .map_err(|_| "无法原子替换隔离片段 fixture".to_owned())?;
        wait_for_changed_configuration_path(&mut changes, "snippets/watcher-loop.toml").await?;

        // 原子替换后的 watcher 校验同样会读取目标文件，不应自激生成后续通知。
        assert_configuration_watcher_quiet(&mut changes, Duration::from_millis(500)).await?;
        Ok(())
    }
    .await;

    watcher
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("按关闭期限停止并 join watcher");
    verification.unwrap_or_else(|reason| panic!("{reason}"));
}

/// 在完整 debounce 窗口后确认没有只读访问通知泄漏到配置订阅。
///
/// @author kongweiguang
#[cfg(target_os = "linux")]
async fn assert_configuration_watcher_quiet(
    subscription: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    duration: Duration,
) -> Result<(), String> {
    let cancellation = CancellationToken::new();
    match tokio::time::timeout(duration, subscription.recv(cancellation)).await {
        Err(_) => Ok(()),
        Ok(ConfigurationChangeMessage::Changed(_))
        | Ok(ConfigurationChangeMessage::RescanRequired) => {
            Err("只读片段访问错误地产生了配置变化通知".to_owned())
        }
        Ok(ConfigurationChangeMessage::Cancelled) => Err("隔离配置订阅被意外取消".to_owned()),
        Ok(ConfigurationChangeMessage::Closed) => Err("隔离配置订阅意外关闭".to_owned()),
    }
}

/// 等待真实文件变更的精确相对路径，不能用 watcher 自己的全目录重扫冒充写入回执。
///
/// @author kongweiguang
#[cfg(target_os = "linux")]
async fn wait_for_changed_configuration_path(
    subscription: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    expected_path: &str,
) -> Result<(), String> {
    let cancellation = CancellationToken::new();
    let received = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match subscription.recv(cancellation.clone()).await {
                ConfigurationChangeMessage::Changed(event)
                    if event
                        .relative_paths
                        .iter()
                        .any(|path| path == expected_path) =>
                {
                    return Ok(());
                }
                ConfigurationChangeMessage::Changed(_)
                | ConfigurationChangeMessage::RescanRequired => {}
                ConfigurationChangeMessage::Cancelled => {
                    return Err("等待隔离配置文件变化时订阅被取消".to_owned());
                }
                ConfigurationChangeMessage::Closed => {
                    return Err("等待隔离配置文件变化时订阅关闭".to_owned());
                }
            }
        }
    })
    .await
    .map_err(|_| "真实片段写入后未收到对应路径的变化通知".to_owned())?;
    received
}
