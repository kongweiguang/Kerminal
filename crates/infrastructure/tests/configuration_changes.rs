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

/// 单次平台事件等待窗口；超时后由调用方重写文件制造新事件，而不是继续空等。
const MODE_WAIT_WINDOW: Duration = Duration::from_secs(5);
/// 恢复态切换的总 deadline；超出后仍未见目标状态才判定 watcher 失败。
const MODE_WAIT_DEADLINE: Duration = Duration::from_secs(30);

/// 在给定窗口内等待指定恢复态，把超时作为 `Elapsed` 交还调用方决定是否重试。
///
/// 拆出非 panic 版本是因为 macOS FSEvents 在 `watch()` 注册初期可能漏掉紧接着的
/// 写入事件；固定 sleep 无法区分平台投递延迟和 watcher 缺陷，调用方需要有界重试。
async fn try_wait_for_mode(
    subscription: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    expected: ConfigurationMode,
    window: Duration,
) -> Result<(), tokio::time::error::Elapsed> {
    let cancellation = CancellationToken::new();
    tokio::time::timeout(window, async {
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
}

/// 在总 deadline 内等待指定恢复态；单次窗口超时就重写同一内容触发新的平台事件。
///
/// 重写只改变文件时间戳、不改变内容，因此不改变断言（仍必须观察到目标恢复态），
/// 也不会把损坏或修复后的配置语义带偏；总 deadline 到了仍失败才 panic。
async fn wait_for_mode_retrying(
    subscription: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    expected: ConfigurationMode,
    deadline: Instant,
    retrigger: impl Fn(),
) {
    loop {
        match try_wait_for_mode(subscription, expected, MODE_WAIT_WINDOW).await {
            Ok(()) => return,
            Err(_) if Instant::now() < deadline => retrigger(),
            Err(error) => panic!("等待配置状态变更超时: {error}"),
        }
    }
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

    let settings_path = temp.0.join("settings.toml");
    let corrupt = "schema_version = [\n";
    fs::write(&settings_path, corrupt).expect("写入损坏设置 fixture");
    // FSEvents 可能漏掉 watch 注册后紧接着的写入；重写同一损坏内容制造新事件，
    // 断言仍是必须进入只读恢复态，不把平台启动竞态当成 watcher 缺陷。
    wait_for_mode_retrying(
        &mut changes,
        ConfigurationMode::ReadOnlyRecovery,
        Instant::now() + MODE_WAIT_DEADLINE,
        || fs::write(&settings_path, corrupt).expect("重写损坏设置 fixture"),
    )
    .await;
    let blocked = configuration
        .write(
            ConfigDocumentPath::Settings,
            "schema_version = 1\n".to_owned(),
            None,
        )
        .await
        .expect_err("损坏文件存在时阻止应用写入");
    assert_eq!(blocked.public_code().as_str(), "configuration_corrupt");

    let repaired = "schema_version = 1\n";
    fs::write(&settings_path, repaired).expect("修复设置 fixture");
    // 离开恢复态同样依赖平台事件投递，用同一有界重写策略等待，断言不变。
    wait_for_mode_retrying(
        &mut changes,
        ConfigurationMode::ReadWrite,
        Instant::now() + MODE_WAIT_DEADLINE,
        || fs::write(&settings_path, repaired).expect("重写修复后的设置 fixture"),
    )
    .await;
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
/// 与恢复态等待同样只放宽环境上限：慢 runner 上平台事件投递可能明显晚于 8 秒，
/// 断言仍要求收到包含目标路径的真实变更事件。
///
/// @author kongweiguang
#[cfg(target_os = "linux")]
async fn wait_for_changed_configuration_path(
    subscription: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    expected_path: &str,
) -> Result<(), String> {
    let cancellation = CancellationToken::new();
    tokio::time::timeout(Duration::from_secs(30), async {
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
    .map_err(|_| "真实片段写入后未收到对应路径的变化通知".to_owned())??;
    Ok(())
}
