//! MCP 设置页的生产按钮回归，覆盖停止超时后的恢复和共享状态变化。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gpui_kit::{ClipboardItem, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult,
    mcp_runtime::{Instant, McpRuntimeControl, McpRuntimeSnapshot, McpRuntimeState},
};
use tokio_util::sync::CancellationToken;

use super::{click, fixture_with_mcp, observed_element, pump_runtime};

struct RuntimeFixture {
    state: Mutex<RuntimeState>,
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

    /// 同步重试复用同一个端口并确认完整 Running 回执。
    async fn start(&self, _: CancellationToken) -> AppResult<McpRuntimeSnapshot> {
        let mut state = self.state.lock().unwrap();
        state.starts += 1;
        state.snapshot = running();
        Ok(state.snapshot.clone())
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

    /// 不将测试结束计入用户操作次数，便于检测错误的启动路由。
    async fn shutdown_until(&self, _: Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 合法配置只由 Running 返回，地址为测试数据，不启动真正的网络服务。
fn running() -> McpRuntimeSnapshot {
    McpRuntimeSnapshot {
        state: McpRuntimeState::Running,
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
