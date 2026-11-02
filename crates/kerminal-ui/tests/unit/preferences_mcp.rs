//! MCP 设置页的生产按钮回归，覆盖停止超时后的恢复和共享状态变化。
//!
//! @author kongweiguang

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use gpui_kit::{ClipboardItem, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult,
    mcp_runtime::{
        Instant, McpClientConfig, McpRuntimeControl, McpRuntimeSnapshot, McpRuntimeState,
    },
    preferences::{
        SettingsRepository as _, VersionedSettings,
        settings::{McpBindMode, McpSettings},
    },
};
use kerminal_domain::preferences::ConfigRevision;
use tokio_util::sync::CancellationToken;

use super::{click, fixture_with_mcp, observed_element, pump_runtime};

struct RuntimeFixture {
    state: Mutex<RuntimeState>,
    repository: Mutex<Option<super::MemorySettingsRepository>>,
    fail_next_apply: AtomicBool,
    unknown_before_apply: AtomicBool,
    unknown_after_apply: AtomicBool,
}

struct RuntimeState {
    snapshot: McpRuntimeSnapshot,
    starts: usize,
    stops: usize,
    timeout_first_stop: bool,
}

impl RuntimeFixture {
    /// 用可控回执复现清理超时，不靠真实网络等待或伪造面板内部状态。
    fn new(snapshot: McpRuntimeSnapshot, timeout_first_stop: bool) -> Arc<Self> {
        Arc::new(Self {
            repository: Mutex::new(None),
            fail_next_apply: AtomicBool::new(false),
            unknown_before_apply: AtomicBool::new(false),
            unknown_after_apply: AtomicBool::new(false),
            state: Mutex::new(RuntimeState {
                snapshot,
                starts: 0,
                stops: 0,
                timeout_first_stop,
            }),
        })
    }

    /// 从端口计数证明点击选择了正确操作，而不是只检查按钮文案。
    fn counts(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        (state.starts, state.stops)
    }
}

#[async_trait]
impl McpRuntimeControl for RuntimeFixture {
    /// 共享快照模拟其它调用方更新运行态，测试不得直接更改面板缓存。
    fn snapshot(&self) -> McpRuntimeSnapshot {
        self.state.lock().unwrap().snapshot.clone()
    }

    /// 每次启动读取仓储已确认端口，证明草稿先保存、监听后启动的顺序。
    async fn start(&self, _: CancellationToken) -> AppResult<McpRuntimeSnapshot> {
        let repository = self.repository.lock().unwrap().clone();
        let (port, bind_mode) = if let Some(repository) = repository {
            let mcp = repository.load_settings().await?.settings.mcp;
            (mcp.port, mcp.bind_mode)
        } else {
            (37657, McpBindMode::Loopback)
        };
        let mut state = self.state.lock().unwrap();
        state.starts += 1;
        state.snapshot = running();
        state.snapshot.bind_mode = bind_mode;
        state.snapshot.endpoint = Some(format!("http://127.0.0.1:{port}/mcp"));
        Ok(state.snapshot.clone())
    }

    /// 用真实测试仓储的 CAS 模拟启动前保存，失败时不得触发 start。
    async fn apply_settings(
        &self,
        settings: McpSettings,
        expected_revision: Option<ConfigRevision>,
        _: CancellationToken,
    ) -> AppResult<VersionedSettings> {
        if self.unknown_before_apply.swap(false, Ordering::SeqCst) {
            return Err(AppError::outcome_unknown("MCP fixture before save"));
        }
        if self.fail_next_apply.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure(
                "fixture could not save MCP settings",
            ));
        }
        let repository = self
            .repository
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::unsupported("MCP fixture repository missing"))?;
        let mut loaded = repository.load_settings().await?.settings;
        loaded.mcp = settings;
        let saved = repository.save_settings(loaded, expected_revision).await?;
        if self.unknown_after_apply.swap(false, Ordering::SeqCst) {
            return Err(AppError::outcome_unknown("MCP fixture after save"));
        }
        Ok(saved)
    }

    /// 首次停止保留 owner 的地址但移除可复制配置；重试必须再次到达 stop 端口。
    async fn stop_until(&self, _: Instant) -> AppResult<McpRuntimeSnapshot> {
        let mut state = self.state.lock().unwrap();
        state.stops += 1;
        if state.timeout_first_stop && state.stops == 1 {
            state.snapshot.state = McpRuntimeState::Stopping;
            state.snapshot.client_json = None;
            state.snapshot.failure_reason = Some("停止超时，请重试停止。".to_owned());
            return Err(AppError::timeout("MCP fixture stop"));
        }
        state.snapshot = McpRuntimeSnapshot::stopped();
        Ok(state.snapshot.clone())
    }

    /// fixture 不拥有系统资源；仅复用 trait 的生命周期边界。
    fn begin_shutdown(&self) {}

    /// 运行中的 listener 只公布已加入白名单的实际网卡地址。
    async fn lan_addresses(&self) -> AppResult<Vec<std::net::Ipv4Addr>> {
        if self.snapshot().state != McpRuntimeState::Running {
            return Err(AppError::conflict("fixture MCP is stopped"));
        }
        Ok(vec![std::net::Ipv4Addr::new(192, 168, 1, 20)])
    }

    /// 仅复制路径读取令牌；预览使用掩码，测试用合成令牌验证复制内容。
    async fn lan_client_config(&self, address: std::net::Ipv4Addr) -> AppResult<McpClientConfig> {
        Ok(McpClientConfig::new(format!(
            "{{\"mcpServers\":{{\"kerminal\":{{\"type\":\"http\",\"url\":\"http://{address}:37657/mcp\",\"headers\":{{\"Authorization\":\"Bearer synthetic-token\"}}}}}}}}"
        )))
    }

    /// 不将测试结束计入用户操作次数，便于检测错误的启动路由。
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 合法配置只由 Running 返回，地址为测试数据，不启动真正的网络服务。
fn running() -> McpRuntimeSnapshot {
    McpRuntimeSnapshot {
        state: McpRuntimeState::Running,
        bind_mode: McpBindMode::Loopback,
        endpoint: Some("http://127.0.0.1:37657/mcp".to_owned()),
        client_json: Some(
            r#"{"mcpServers":{"kerminal":{"url":"http://127.0.0.1:37657/mcp"}}}"#.to_owned(),
        ),
        endpoint_sync_pending: false,
        unavailable_reason: None,
        failure_reason: None,
    }
}

/// 底层 GPUI 尚不暴露 aria-disabled；通过真实点击的剪贴板结果证明复制动作是否可用。
fn assert_copy_state(visual: &mut VisualTestContext, enabled: bool) {
    const SENTINEL: &str = "MCP test clipboard unchanged";
    visual.update(|_, app| {
        app.write_to_clipboard(ClipboardItem::new_string(SENTINEL.to_owned()));
    });
    click(visual, "mcp-settings-copy-json");
    let copied = visual.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()));
    if enabled {
        let expected: serde_json::Value =
            serde_json::from_str(running().client_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(copied.as_deref().unwrap()).unwrap(),
            expected
        );
    } else {
        assert_eq!(copied.as_deref(), Some(SENTINEL));
    }
}

/// 真正点击两次停止再启动，确认超时不会把用户困在显示可点却无效果的按钮上。
#[gpui_kit::test]
fn retries_timed_out_stop_and_can_start_again(app: &mut TestAppContext) {
    let control = RuntimeFixture::new(running(), true);
    let (runtime, panel, _, visual) = fixture_with_mcp(app, Some(control.clone()));
    click(visual, "preferences-nav-mcp");
    click(visual, "mcp-connection-info-toggle");
    assert_copy_state(visual, true);
    click(visual, "mcp-settings-toggle");
    assert_eq!(
        observed_element(visual, "mcp-settings-toggle").label(),
        Some(crate::locale::text("停止中…", "Stopping…").as_ref())
    );
    assert_copy_state(visual, false);
    pump_runtime(&runtime, visual);
    let toggle = observed_element(visual, "mcp-settings-toggle");
    assert_eq!(
        toggle.label(),
        Some(crate::locale::text("重试停止", "Retry stop").as_ref())
    );
    assert_copy_state(visual, false);
    assert_eq!(control.counts(), (0, 1));
    assert_eq!(
        visual.update(|_, app| panel.read(app).connectivity.mcp_status.clone()),
        Some(
            crate::locale::text(
                "停止未完成，请点击“重试停止”继续清理。",
                "Stopping did not finish. Choose Retry stop to finish cleanup.",
            )
            .to_string()
        )
    );

    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (0, 2));
    assert_eq!(control.snapshot().state, McpRuntimeState::Stopped);
    assert_copy_state(visual, false);

    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (1, 2));
    assert_eq!(control.snapshot().state, McpRuntimeState::Running);
    assert_copy_state(visual, true);
}

/// 局域网启动后展示 0.0.0.0 监听地址，并把运行中 listener 的网卡地址填入复制列表。
#[gpui_kit::test]
fn lan_start_shows_listen_address_and_live_interfaces(app: &mut TestAppContext) {
    let control = RuntimeFixture::new(McpRuntimeSnapshot::stopped(), false);
    let (runtime, panel, repository, visual) = fixture_with_mcp(app, Some(control.clone()));
    *control.repository.lock().unwrap() = Some(repository.clone());
    click(visual, "preferences-nav-mcp");
    click(visual, "mcp-bind-lan");
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(control.snapshot().state, McpRuntimeState::Running);
    assert_eq!(
        visual.update(|_, app| panel.read(app).mcp_settings.addresses.clone()),
        vec![std::net::Ipv4Addr::new(192, 168, 1, 20)]
    );
    click(visual, "mcp-connection-info-toggle");
    assert_eq!(
        observed_element(visual, "mcp-listen-address").label(),
        Some("http://0.0.0.0:37657/mcp")
    );
    assert_eq!(
        observed_element(visual, "mcp-lan-address-192.168.1.20").label(),
        Some("192.168.1.20")
    );
    assert_eq!(
        observed_element(visual, "mcp-connect-address").label(),
        Some("http://192.168.1.20:37657/mcp")
    );
    assert_eq!(
        observed_element(visual, "mcp-settings-copy-json").label(),
        Some(crate::locale::text("复制局域网配置", "Copy LAN config").as_ref())
    );
    click(visual, "mcp-config-local");
    assert_eq!(
        observed_element(visual, "mcp-connect-address").label(),
        Some("http://127.0.0.1:37657/mcp")
    );
    assert_eq!(
        observed_element(visual, "mcp-settings-copy-json").label(),
        Some(crate::locale::text("复制 JSON", "Copy JSON").as_ref())
    );

    click(visual, "mcp-lan-address-192.168.1.20");
    visual.update(|_, app| {
        app.write_to_clipboard(ClipboardItem::new_string("MCP LAN sentinel".to_owned()));
    });
    click(visual, "mcp-settings-copy-json");
    pump_runtime(&runtime, visual);
    let copied = visual
        .update(|_, app| app.read_from_clipboard().and_then(|item| item.text()))
        .expect("LAN client configuration was not copied");
    let json: serde_json::Value = serde_json::from_str(&copied).expect("valid LAN JSON");
    assert_eq!(
        json["mcpServers"]["kerminal"]["url"],
        "http://192.168.1.20:37657/mcp"
    );
    assert_eq!(
        json["mcpServers"]["kerminal"]["headers"]["Authorization"],
        "Bearer synthetic-token"
    );
    assert_eq!(
        observed_element(visual, "mcp-settings-copy-json").label(),
        Some(crate::locale::text("已复制", "Copied").as_ref())
    );
}

/// 其它调用方正在启动时禁用动作，避免重复提交和“启动”按钮无反馈。
#[gpui_kit::test]
fn shared_start_in_progress_disables_toggle(app: &mut TestAppContext) {
    let mut snapshot = McpRuntimeSnapshot::stopped();
    snapshot.state = McpRuntimeState::Starting;
    let control = RuntimeFixture::new(snapshot, false);
    let (runtime, _panel, _, visual) = fixture_with_mcp(app, Some(control.clone()));
    click(visual, "preferences-nav-mcp");
    let toggle = observed_element(visual, "mcp-settings-toggle");
    assert_eq!(
        toggle.label(),
        Some(crate::locale::text("启动中…", "Starting…").as_ref())
    );
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (0, 0));
}

/// endpoint 同步失败必须显示可重试动作，且同步成功前不能复制可能失效的地址。
#[gpui_kit::test]
fn retries_endpoint_sync_before_enabling_copy(app: &mut TestAppContext) {
    let mut snapshot = running();
    snapshot.state = McpRuntimeState::Synchronizing;
    snapshot.client_json = None;
    snapshot.endpoint_sync_pending = true;
    let control = RuntimeFixture::new(snapshot, false);
    let (runtime, _panel, _, visual) = fixture_with_mcp(app, Some(control.clone()));
    click(visual, "preferences-nav-mcp");
    click(visual, "mcp-connection-info-toggle");
    assert_eq!(
        observed_element(visual, "mcp-settings-toggle").label(),
        Some(crate::locale::text("重试同步", "Retry sync").as_ref())
    );
    assert_copy_state(visual, false);
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (1, 0));
    assert!(!control.snapshot().endpoint_sync_pending);
    assert_copy_state(visual, true);
}

/// 重复选择与改回原值不应阻止启动；非法端口只保留草稿，不到达运行态端口。
#[gpui_kit::test]
fn listener_draft_tracks_real_changes_and_validates_before_start(app: &mut TestAppContext) {
    let control = RuntimeFixture::new(McpRuntimeSnapshot::stopped(), false);
    let (runtime, panel, _, visual) = fixture_with_mcp(app, Some(control.clone()));
    click(visual, "preferences-nav-mcp");
    click(visual, "mcp-bind-loopback");
    assert!(!visual.update(|_, app| panel.read(app).mcp_settings.edited));
    click(visual, "mcp-bind-lan");
    assert!(visual.update(|_, app| panel.read(app).mcp_settings.edited));
    click(visual, "mcp-bind-loopback");
    assert!(!visual.update(|_, app| panel.read(app).mcp_settings.edited));

    let original = visual.update(|_, app| {
        panel
            .read(app)
            .mcp_settings
            .port
            .read(app)
            .value()
            .to_string()
    });
    for invalid in ["0", "65536", "abc", "+37657"] {
        click(visual, "mcp-settings-port");
        visual.simulate_keystrokes("secondary-a backspace");
        visual.simulate_input(invalid);
        super::draw(visual);
        assert!(visual.update(|_, app| panel.read(app).mcp_settings.valid_port(app).is_none()));
        click(visual, "mcp-settings-toggle");
        pump_runtime(&runtime, visual);
        assert_eq!(control.counts(), (0, 0));
    }
    click(visual, "mcp-settings-port");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input(&original);
    super::draw(visual);
    assert!(!visual.update(|_, app| panel.read(app).mcp_settings.edited));
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (1, 0));
    click(visual, "mcp-settings-port");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("invalid");
    super::draw(visual);
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (1, 1));
}

/// 运行中修改不重启；停止后启动才保存新端口，保存失败保留输入且可直接重试。
#[gpui_kit::test]
fn toggle_saves_latest_draft_only_before_start_and_retries_failed_save(app: &mut TestAppContext) {
    let control = RuntimeFixture::new(running(), false);
    let (runtime, panel, repository, visual) = fixture_with_mcp(app, Some(control.clone()));
    *control.repository.lock().unwrap() = Some(repository.clone());
    click(visual, "preferences-nav-mcp");
    click(visual, "mcp-settings-port");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("37659");
    super::draw(visual);
    assert_eq!(repository.snapshot().0.mcp.port, 37657);
    let _ = observed_element(visual, "mcp-settings-restart-hint");
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (0, 1));
    assert_eq!(repository.snapshot().0.mcp.port, 37657);

    control.fail_next_apply.store(true, Ordering::SeqCst);
    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (0, 1));
    assert!(visual.update(|_, app| panel.read(app).mcp_settings.edited));
    assert_eq!(repository.snapshot().0.mcp.port, 37657);

    click(visual, "mcp-settings-toggle");
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(control.counts(), (1, 1));
    assert_eq!(repository.snapshot().0.mcp.port, 37659);
    assert_eq!(
        control.snapshot().endpoint.as_deref(),
        Some("http://127.0.0.1:37659/mcp")
    );
    assert!(!visual.update(|_, app| panel.read(app).mcp_settings.edited));
}

/// 未落盘的未知回执即便 revision 未变化，也能只读核对后保留输入，下一次点击才重试。
#[gpui_kit::test]
fn reconciles_unknown_unsaved_mcp_draft_without_replaying_start(app: &mut TestAppContext) {
    verify_unknown_readback(app, false);
}

/// 已落盘的未知回执确认新值后不自行启动，避免核对动作变成第二次网络副作用。
#[gpui_kit::test]
fn reconciles_unknown_saved_mcp_draft_without_replaying_start(app: &mut TestAppContext) {
    verify_unknown_readback(app, true);
}

/// 两种未知结果共用生产按钮和仓储回读，区别仅是故障发生在 CAS 前还是 CAS 后。
fn verify_unknown_readback(app: &mut TestAppContext, saved: bool) {
    let control = RuntimeFixture::new(McpRuntimeSnapshot::stopped(), false);
    let (runtime, panel, repository, visual) = fixture_with_mcp(app, Some(control.clone()));
    *control.repository.lock().unwrap() = Some(repository.clone());
    if saved {
        control.unknown_after_apply.store(true, Ordering::SeqCst);
    } else {
        control.unknown_before_apply.store(true, Ordering::SeqCst);
    }
    click(visual, "preferences-nav-mcp");
    click(visual, "mcp-settings-port");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("37661");
    click(visual, "mcp-settings-toggle");
    for _ in 0..3 {
        pump_runtime(&runtime, visual);
    }
    assert_eq!(control.counts(), (0, 0));
    assert_eq!(
        repository.snapshot().0.mcp.port,
        if saved { 37661 } else { 37657 }
    );
    assert!(!visual.update(|_, app| panel.read(app).mcp_settings.outcome_unknown));
    assert_eq!(
        visual.update(|_, app| panel.read(app).mcp_settings.edited),
        !saved
    );
    assert_eq!(
        visual.update(|_, app| panel.read(app).mcp_settings.valid_port(app)),
        Some(37661)
    );
    click(visual, "mcp-settings-toggle");
    for _ in 0..3 {
        pump_runtime(&runtime, visual);
    }
    assert_eq!(control.counts(), (1, 0));
    assert_eq!(repository.snapshot().0.mcp.port, 37661);
}
