//! 内容恢复失败就地展示；重试复用原 Tab 和状态，不建立第二个实例。
//!
//! @author kongweiguang

use super::*;

impl RootView {
    /// 绘制仅使用实例和已记录错误，网络与工厂只由显式重试动作触发。
    /// 已创建但激活失败的实体继续保留，恢复回执决定挂载，不能让它遮住重试入口。
    /// 未安装的提供方及正在关闭的实例不提供无效重试按钮。
    ///
    /// @author kongweiguang
    pub(super) fn render_plugin_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let scope = self.active_content_scope.clone();
        if let Some(view) = scope
            .as_ref()
            .filter(|scope| self.plugin_content_is_ready(scope))
            .and_then(|scope| {
                self.content_host
                    .view_for_mount(scope, self.content_window_active)
            })
        {
            return view.into_any_element();
        }
        let message = scope
            .as_ref()
            .and_then(|scope| self.content_failures.get(scope))
            .cloned()
            .unwrap_or_else(|| {
                crate::locale::text(
                    "内容提供方不可用，已保留原始状态。",
                    "The content provider is unavailable. Its saved state is preserved.",
                )
            });
        let retry_scope = scope.filter(|scope| {
            use crate::plugins::content::{ContentPhase, ContentScope};
            if matches!(
                self.content_host.phase(scope),
                Some(ContentPhase::Closing | ContentPhase::Closed)
            ) {
                return false;
            }
            let content_id = match scope {
                ContentScope::Page(id) => Some(id.as_str()),
                ContentScope::Tab(id) => {
                    self.workspace_controller.as_ref().and_then(|controller| {
                        controller
                            .snapshot()
                            .terminal_tabs
                            .iter()
                            .find_map(|tab| match tab {
                                WorkspaceSessionTab::Extension {
                                    id: tab_id,
                                    content_id,
                                    ..
                                } if tab_id == id => Some(content_id.as_str()),
                                _ => None,
                            })
                    })
                }
            };
            content_id.is_some_and(|id| {
                self.services
                    .content_factories
                    .iter()
                    .any(|factory| factory.content_id() == id)
            })
        });
        div()
            .id("plugin-content-recovery")
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .child(div().max_w(px(560.)).child(message))
            .children(retry_scope.map(|scope| {
                gpui_kit::component::button::Button::new("retry-plugin-content")
                    .label(crate::locale::text("重试", "Retry"))
                    .on_click(cx.listener(move |root, _, window, cx| {
                        if !root.host_shutdown_requested
                            && !root.tool_panel_overlay_focus_is_blocked(window, cx)
                            && root.active_content_scope.as_ref() == Some(&scope)
                        {
                            match &scope {
                                crate::plugins::content::ContentScope::Tab(tab_id) => {
                                    root.activate_workspace_tab_surface(tab_id, true, window, cx);
                                }
                                crate::plugins::content::ContentScope::Page(content_id) => {
                                    if let Err(error) =
                                        root.open_plugin_page(content_id, window, cx)
                                    {
                                        root.status =
                                            Some(error.public_message().to_owned().into());
                                    }
                                }
                            }
                            cx.notify();
                        }
                    }))
            }))
            .into_any_element()
    }
}
