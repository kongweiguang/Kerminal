//! 网页插件验收借用正式导航、原生 IPC 与页面生命周期；仅隔离验收构建公开。
//! @author kongweiguang

use super::*;
use crate::plugins::{
    actions::OpenContentTab, content::ContentScope, external_webview::renderer::WebViewRenderer,
};
use gpui_kit::component::WindowExt as _;
use kerminal_application::{AppError, AppResult};
use serde_json::{Value, json};

const SEARCH_CONTENT: &str = "kerminal.file-search/search-tab";

impl RootView {
    /// 打开真实示例标签，资源和运行服务均来自启动扫描，没有验收工厂替身。
    pub fn plugin_webview_acceptance_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<TabId> {
        self.open_plugin_tab(
            &OpenContentTab {
                content_id: SEARCH_CONTENT.into(),
                title: "File Search".into(),
                state_version: 1,
                state: json!({}),
            },
            window,
            cx,
        )
    }

    /// 每个标签都读取自身 renderer 的原生身份及页面代次，隐藏标签不能冒充当前标签。
    pub fn plugin_webview_acceptance_snapshot(&self, window: &Window, cx: &Context<Self>) -> Value {
        let mut snapshot = self.plugin_acceptance_snapshot(cx);
        if let Some(tabs) = snapshot["tabs"].as_array_mut() {
            for tab in tabs {
                tab["renderer"] = tab["tabId"]
                    .as_str()
                    .and_then(|id| TabId::parse(id).ok())
                    .and_then(|id| self.content_host.view(&ContentScope::Tab(id)))
                    .and_then(|view| view.downcast::<WebViewRenderer>().ok())
                    .map(|view| view.read(cx).acceptance_snapshot(cx))
                    .unwrap_or(Value::Null);
            }
        }
        snapshot["preferencesOpen"] = json!(self.preferences_modal_open);
        snapshot["settingsReady"] = json!(self.settings_snapshot.is_some());
        snapshot["registeredContent"] =
            json!(self.services.plugins.content_type(SEARCH_CONTENT).is_some());
        snapshot["windowActive"] = json!(window.is_window_active());
        snapshot
    }

    /// 探针只定位当前标签，目标消失即失败，绝不回退到另一个网页。
    fn plugin_webview_acceptance_renderer(&self) -> AppResult<Entity<WebViewRenderer>> {
        self.active_content_scope
            .as_ref()
            .and_then(|scope| self.content_host.view(scope))
            .and_then(|view| view.downcast::<WebViewRenderer>().ok())
            .ok_or_else(|| AppError::not_found("活动网页插件实例"))
    }

    /// 脚本可调用局部 report(value) 读回真实 DOM；控件点击仍触发示例页面的生产监听器。
    pub fn plugin_webview_acceptance_evaluate(
        &mut self,
        script: &str,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.plugin_webview_acceptance_renderer()?
            .read(cx)
            .acceptance_evaluate(script, cx)
    }

    /// 重载调用真实页面替换路径，旧 IPC 和运行回执须由生产代次门禁拒绝。
    pub fn plugin_webview_acceptance_reload(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        self.plugin_webview_acceptance_renderer()?
            .update(cx, |renderer, cx| renderer.acceptance_reload(window, cx));
        Ok(())
    }

    /// 偏好模态使用与标题栏相同入口，让 Root 正式遮挡投影接管网页。
    pub fn plugin_webview_acceptance_open_preferences(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_preferences_modal(window, cx);
    }

    /// 验收关闭也走已有离开确认路径，不手工清除偏好模态镜像。
    pub fn plugin_webview_acceptance_close_preferences(&mut self, cx: &mut Context<Self>) {
        self.request_close_preferences_modal(cx);
    }

    /// 固定使用应用内正式通知，避免测试受系统通知策略或自动消失计时影响。
    pub fn plugin_webview_acceptance_show_notification(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::component::notification::{Notification, NotificationDelivery};
        window.push_notification(
            Notification::new()
                .message("WebView notification acceptance")
                .delivery(NotificationDelivery::InApp)
                .autohide(false),
            cx,
        );
    }

    /// 清理保留 Kit 退出动画，让原生可见状态证明遮挡一直持续到通知真正卸载。
    pub fn plugin_webview_acceptance_clear_notification(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.clear_notifications(cx);
    }
}
