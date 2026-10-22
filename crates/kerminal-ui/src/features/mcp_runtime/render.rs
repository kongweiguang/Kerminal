//! MCP 状态卡片与生命周期按钮。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::mcp_runtime::{McpRuntimeSnapshot, McpRuntimeState};

use crate::theme::ThemeTokens;

use super::{McpRuntimePanel, RuntimeOperation};

impl McpRuntimePanel {
    /// 以状态、真实 endpoint、client JSON 和可重试动作组成独立 MCP 面板。
    pub(super) fn render_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let snapshot = self.snapshot.clone();
        let start_label =
            if snapshot.state == McpRuntimeState::Failed || snapshot.endpoint_sync_pending {
                crate::locale::text("重试启动", "Retry start")
            } else {
                crate::locale::text("启动服务", "Start server")
            };
        let stop_label =
            if self.operation == Some(RuntimeOperation::Stop) || snapshot.endpoint_sync_pending {
                crate::locale::text("重试停止 / 同步", "Retry stop / sync")
            } else {
                crate::locale::text("停止服务", "Stop server")
            };
        let client_json = snapshot.client_json.clone();
        let controls = render_controls(self, theme, start_label, stop_label, cx);

        div()
            .id("mcp-runtime-panel")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .bg(theme.background)
            .text_color(theme.foreground)
            .p_4()
            .child(render_header(theme, &snapshot))
            .child(controls)
            .child(render_endpoint(theme, &snapshot))
            .when_some(client_json, |panel, config| {
                panel.child(render_client_config(theme, config))
            })
            .when_some(self.status.clone(), |panel, status| {
                panel.child(
                    div()
                        .id("mcp-runtime-message")
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(theme.panel)
                        .text_size(px(12.))
                        .text_color(if snapshot.failure_reason.is_some() {
                            theme.error
                        } else {
                            theme.muted
                        })
                        .child(status),
                )
            })
    }
}

/// 顶部摘要保持状态语义明确，Synchronizing/Stopping 不被显示为 Running/Stopped。
fn render_header(theme: ThemeTokens, snapshot: &McpRuntimeSnapshot) -> impl IntoElement {
    div()
        .id("mcp-runtime-header")
        .w_full()
        .p_4()
        .rounded_lg()
        .bg(theme.panel)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_size(px(16.))
                .child(crate::locale::text("MCP 服务", "MCP server")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(state_color(theme, snapshot.state))
                .child(state_label(snapshot.state)),
        )
        .when_some(snapshot.unavailable_reason.clone(), |header, reason| {
            header.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(reason),
            )
        })
}

/// 根据状态显式禁用不可能的动作；停止超时会保留 owner 并把同一按钮变为重试。
fn render_controls(
    panel: &mut McpRuntimePanel,
    theme: ThemeTokens,
    start_label: gpui_kit::SharedString,
    stop_label: gpui_kit::SharedString,
    cx: &mut Context<McpRuntimePanel>,
) -> impl IntoElement {
    let busy = panel.operation.is_some();
    let starting = panel.operation == Some(RuntimeOperation::Start);
    let unavailable = panel.snapshot.state == McpRuntimeState::Unavailable;
    let start_unavailable = !matches!(
        panel.snapshot.state,
        McpRuntimeState::Stopped | McpRuntimeState::Synchronizing | McpRuntimeState::Failed
    );
    let has_owner_or_pending_sync =
        panel.snapshot.endpoint.is_some() || panel.snapshot.endpoint_sync_pending;
    let copy_available = panel.snapshot.client_json.is_some();

    div()
        .id("mcp-runtime-controls")
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            Button::new("mcp-runtime-start")
                .label(if busy && starting {
                    crate::locale::text("启动中…", "Starting…")
                } else {
                    start_label
                })
                .when(!unavailable && !start_unavailable, Button::primary)
                .disabled(busy || unavailable || start_unavailable)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.start(cx))),
        )
        .child(
            Button::new("mcp-runtime-stop")
                .compact()
                .label(if busy && !starting {
                    crate::locale::text("停止中…", "Stopping…")
                } else {
                    stop_label
                })
                .disabled(busy || unavailable || !has_owner_or_pending_sync)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.stop(cx))),
        )
        .when(starting, |controls| {
            controls.child(
                Button::new("mcp-runtime-cancel-start")
                    .compact()
                    .label(crate::locale::text("取消启动", "Cancel start"))
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_start(cx))),
            )
        })
        .child(
            Button::new("mcp-runtime-refresh")
                .compact()
                .label(crate::locale::text("刷新", "Refresh"))
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.refresh(cx))),
        )
        .child(
            Button::new("mcp-runtime-copy")
                .compact()
                .label(crate::locale::text("复制客户端配置", "Copy client config"))
                .disabled(!copy_available)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.copy_client_config(cx))),
        )
        .bg(theme.background)
}

/// 显示由实际 socket owner 返回的 endpoint，停止中仍可排障但不会提供复制配置。
fn render_endpoint(theme: ThemeTokens, snapshot: &McpRuntimeSnapshot) -> impl IntoElement {
    let endpoint = snapshot
        .endpoint
        .clone()
        .unwrap_or_else(|| crate::locale::text("尚未绑定地址", "No endpoint bound").to_string());
    div()
        .id("mcp-runtime-endpoint")
        .w_full()
        .p_3()
        .rounded_md()
        .bg(theme.panel)
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(crate::locale::text("HTTP endpoint", "HTTP endpoint")),
        )
        .child(
            div()
                .font_family("monospace")
                .text_size(px(12.))
                .child(endpoint),
        )
        .when(snapshot.endpoint_sync_pending, |card| {
            card.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "Agent 工作区同步待完成；可重试对应操作。",
                        "Agent workspace synchronization is pending; retry the current action.",
                    )),
            )
        })
}

/// 只呈现 controller 在完全 Running 时发布的 client JSON，endpoint 和复制按钮使用同一快照。
fn render_client_config(theme: ThemeTokens, config: String) -> impl IntoElement {
    div()
        .id("mcp-runtime-client-config")
        .w_full()
        .p_3()
        .rounded_md()
        .bg(theme.panel)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(crate::locale::text("客户端配置", "Client configuration")),
        )
        .child(
            div()
                .w_full()
                .p_2()
                .rounded_md()
                .bg(theme.background)
                .font_family("monospace")
                .text_size(px(11.))
                .child(config),
        )
}

/// 稳定状态名称按 UI locale 映射，内部 enum 名称不直接暴露给界面。
fn state_label(state: McpRuntimeState) -> gpui_kit::SharedString {
    match state {
        McpRuntimeState::Unavailable => crate::locale::text("不可用", "Unavailable"),
        McpRuntimeState::Stopped => crate::locale::text("已停止", "Stopped"),
        McpRuntimeState::Starting => crate::locale::text("启动中", "Starting"),
        McpRuntimeState::Synchronizing => {
            crate::locale::text("同步 Agent 工作区", "Syncing Agent workspaces")
        }
        McpRuntimeState::Running => crate::locale::text("运行中", "Running"),
        McpRuntimeState::Stopping => crate::locale::text("停止中", "Stopping"),
        McpRuntimeState::Failed => crate::locale::text("启动失败", "Start failed"),
    }
}

/// 按语义令牌突出错误和活跃状态，确保浅色与深色主题均有足够对比。
fn state_color(theme: ThemeTokens, state: McpRuntimeState) -> gpui_kit::Hsla {
    match state {
        McpRuntimeState::Unavailable | McpRuntimeState::Failed => theme.error,
        McpRuntimeState::Running => theme.accent,
        _ => theme.muted,
    }
}
