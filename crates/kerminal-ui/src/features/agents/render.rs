//! Agent 右栏的紧凑会话栏、历史列表与状态提示。
//!
//! @author kongweiguang

use chrono::{DateTime, Local};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement, Pixels,
    Render, StatefulInteractiveElement as _, Subscription, Window,
    assets::IconName,
    component::WindowExt as _,
    component::{
        Disableable, Theme as KitTheme,
        button::{Button, ButtonCustomVariant, ButtonRounded, ButtonVariants},
        dialog::{DialogDescription, DialogEntrance, DialogFooter, DialogTitle},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
        scroll::ScrollableElement as _,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_application::{
    PublicErrorCode,
    agents::{
        AgentHistoryMode, AgentResumeCapability, AgentResumeRecoveryStatus, AgentSessionView,
    },
};
use kerminal_domain::{
    agents::{AgentProvider, AgentSessionScope, AgentSessionStatus},
    identity::AgentSessionId,
    sessions::SessionLifecycle,
};

use crate::locale::UiLanguage;
use crate::theme::ThemeTokens;

use super::{
    AgentArchiveConfirmationOwner, AgentPanelEvent, AgentPendingSendPreview, AgentRenameTarget,
    AgentSendHistoryMetadata, AgentSendOutcome, AgentTerminalFeedback, AgentTerminalFeedbackSource,
    AgentsPanel,
};

/// 一项历史筛选按钮的稳定身份、双语文案和实时计数；theme 与 GPUI 上下文仍显式传入。
///
/// @author kongweiguang
struct HistoryFilterOption {
    id: &'static str,
    label_zh: &'static str,
    label_en: &'static str,
    description_zh: &'static str,
    description_en: &'static str,
    count: usize,
    mode: AgentHistoryMode,
}

/// 历史区默认展示三条完整行；视口高度固定，记录数量变化不能移动上方入口。
const HISTORY_VISIBLE_ROWS: usize = 3;

/// 行式列表右侧始终为滚动条预留的 gutter；用固定物理像素与 Kit 滚动条宽度对齐，
/// 不随 rem 放大，避免 3→4 条时正文横向回流。
const HISTORY_SCROLLBAR_GUTTER: Pixels = px(16.);

/// 两行紧凑行高：第一行标题与常驻操作共用真实 Button 高度，第二行身份与状态单行。
/// 普通态和 rename 态都按同一预算取整，输入框高度也计入，避免逐条内容撑高或裁掉按钮。
///
/// @author kongweiguang
fn history_row_height(theme: ThemeTokens, rem: Pixels) -> Pixels {
    // 上下 p_2 合计 1rem；两行之间保留 gap_1，让标题行和身份行不互相挤压。
    let padding = rem * 1.0;
    let row_gap = rem * 0.25;
    // 管理按钮为 Kit 默认 2rem 图标按钮；rename 输入使用产品控件高度，两者取较大者。
    let first_row = (rem * 2.)
        .max(theme.text_xs * 1.618_034)
        .max(theme.metrics.control_height);
    // 第二行文字固定 1.25 行高；14px 下限保证小字号下仍有可读的点击行。
    let second_row = (theme.text_xs * 1.25).max(px(14.));
    (padding + first_row + row_gap + second_row).ceil()
}

/// 视口高度恒为三条行高；行式列表没有外框，不再补偿不存在的上下边框。
///
/// @author kongweiguang
fn history_viewport_height(theme: ThemeTokens, rem: Pixels) -> Pixels {
    history_row_height(theme, rem) * HISTORY_VISIBLE_ROWS as f32
}

/// 历史行表面只在真实选中和中性悬停之间切换；选中优先于悬停，
/// 让“已附着”和“指针经过”保持不同语义。
///
/// @author kongweiguang
fn history_row_surface(theme: ThemeTokens, active: bool, hovered: bool) -> gpui_kit::Hsla {
    if active {
        theme.surface_selected()
    } else if hovered {
        theme.surface_hover()
    } else {
        theme.panel.alpha(0.)
    }
}

/// 行分隔线绝对定位，不参与行高预算；左右内缩并避开滚动条 gutter，
/// 避免像外框一样包住整行，也避免滚动条出现时压住分隔线。
///
/// @author kongweiguang
fn history_row_separator(theme: ThemeTokens, rem: Pixels) -> impl IntoElement {
    div()
        .absolute()
        .left(rem * 0.5)
        .right(HISTORY_SCROLLBAR_GUTTER)
        .bottom(px(0.))
        .h(px(1.))
        .bg(theme.border.opacity(0.5))
}

/// 第二行身份文案：当前目标筛选下省略重复的“当前目标”，全部筛选才补归属；
/// 完整身份始终保留 scope，供 tooltip 和可访问名称使用。
///
/// @author kongweiguang
fn history_metadata_text(
    provider: &str,
    scope: &str,
    time: &str,
    mode: AgentHistoryMode,
    is_current_target: bool,
) -> (String, String) {
    let full = format!("{provider} · {scope} · {time}");
    // 当前目标筛选里所有可见行都属于当前目标，重复文案没有信息量；
    // 但异常情况下若行不属于当前目标，仍保留 scope，避免身份丢失。
    let display = if mode == AgentHistoryMode::CurrentTarget && is_current_target {
        format!("{provider} · {time}")
    } else {
        full.clone()
    };
    (display, full)
}

impl AgentsPanel {
    /// 冻结历史行上下文并复用 Kit 的焦点与关闭语义；短淡入不改变已确认的操作目标。
    ///
    /// @author kongweiguang
    pub(super) fn open_archive_confirmation(
        &mut self,
        mut owner: AgentArchiveConfirmationOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.archive_confirmation_owner.is_some() {
            return;
        }
        self.archive_confirmation_generation =
            self.archive_confirmation_generation.wrapping_add(1).max(1);
        owner.generation = self.archive_confirmation_generation;
        self.archive_confirmation_owner = Some(owner.clone());
        self.archive_confirmation_error = None;
        let entity = cx.entity().clone();
        let runtime_active = self.sessions.iter().any(|view| {
            view.session.agent_session_id == owner.id
                && view.session.title == owner.expected_title
                && has_running_agent_terminal(view)
        });
        let runtime_description = if runtime_active {
            crate::locale::text(
                "该会话当前仍在 Kerminal 中运行，删除记录后会同时关闭这里的 Agent 终端。",
                "This chat is still running in Kerminal. Deleting the record also closes its Agent terminal here.",
            )
        } else {
            crate::locale::text(
                "删除后，这条会话将不再出现在 Kerminal 的历史列表中。",
                "This chat will no longer appear in Kerminal's history list.",
            )
        };
        let footer_entity =
            cx.new(|cx| AgentArchiveConfirmationFooter::new(entity.clone(), owner.clone(), cx));
        let content_entity = cx.new(|cx| {
            AgentArchiveConfirmationContent::new(
                entity.clone(),
                owner.clone(),
                runtime_description.to_string(),
                footer_entity.clone(),
                cx,
            )
        });
        window.open_dialog(cx, move |dialog, window, app| {
            let theme = entity.update(app, |_, cx| {
                ThemeTokens::current(window, cx).for_tool_panel()
            });
            let confirm_entity = entity.clone();
            let confirm_owner = owner.clone();
            let cancel_entity = entity.clone();
            let cancel_owner = owner.clone();
            let dialog_content = content_entity.clone();
            dialog
                .centered()
                .entrance(DialogEntrance::Fade)
                .p_0()
                .gap_0()
                .min_h(px(0.))
                .close_button(false)
                .backdrop_blur(theme.modal_blur_radius)
                .surface_backdrop_filter(theme.floating_blur_radius, theme.floating_saturation)
                .bg(theme.floating_surface)
                .border_color(theme.tool_panel_border)
                .rounded(theme.radius_dialog)
                .child(dialog_content)
                .on_ok(move |_, window, app| {
                    let owner = confirm_owner.clone();
                    let window_handle = window.window_handle();
                    confirm_entity.update(app, |panel, cx| {
                        panel.confirm_archive_confirmation(&owner, window_handle, cx);
                    });
                    false
                })
                .on_cancel(move |_, _, app| {
                    cancel_entity.update(app, |panel, cx| {
                        panel.cancel_archive_confirmation(&cancel_owner, cx)
                    })
                })
        });
        cx.notify();
    }
}

/// 确认弹框观察 panel 的 owner、busy 与错误状态，避免异步失败被 modal 遮挡。
///
/// @author kongweiguang
struct AgentArchiveConfirmationContent {
    panel: Entity<AgentsPanel>,
    owner: AgentArchiveConfirmationOwner,
    runtime_description: String,
    footer: Entity<AgentArchiveConfirmationFooter>,
    _panel_subscription: Subscription,
}

impl AgentArchiveConfirmationContent {
    /// 订阅唯一 panel 状态源，使失败可重试、提交中按钮禁用并显示加载态。
    ///
    /// @author kongweiguang
    fn new(
        panel: Entity<AgentsPanel>,
        owner: AgentArchiveConfirmationOwner,
        runtime_description: String,
        footer: Entity<AgentArchiveConfirmationFooter>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _panel_subscription = cx.observe(&panel, |_, _, cx| cx.notify());
        Self {
            panel,
            owner,
            runtime_description,
            footer,
            _panel_subscription,
        }
    }
}

impl Render for AgentArchiveConfirmationContent {
    /// 保留固定头尾与有界正文滚动；状态说明只出现一次，已打开弹框随活动主题同步刷新。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (busy, _, error) = self
            .panel
            .read(cx)
            .archive_confirmation_feedback(&self.owner);
        let theme = ThemeTokens::current(window, cx).for_tool_panel();
        let foreground = theme.foreground;
        let title = self.owner.expected_title.clone();
        let runtime_description = self.runtime_description.clone();
        let cancel_parent = self.panel.clone();
        let cancel_owner = self.owner.clone();
        let max_height = (theme.metrics.control_height * 9.)
            .min((window.viewport_size().height - theme.metrics.dialog_padding * 2.).max(px(0.)));

        let mut body = div()
            .id("agent-archive-confirmation-content")
            .w_full()
            .min_h(px(0.))
            .flex_1()
            .flex()
            .flex_col()
            .gap(theme.metrics.panel_padding)
            .px(theme.metrics.dialog_padding)
            .py(theme.metrics.section_gap)
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(theme.metrics.panel_padding)
                    .p(theme.metrics.panel_padding)
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.tool_panel_border)
                    .bg(theme.muted_surface())
                    .child(
                        div()
                            .size(theme.metrics.control_height)
                            .flex_none()
                            .rounded(theme.control_radius)
                            .bg(theme.tool_panel_surface)
                            .text_color(theme.muted)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(IconName::MessageSquare),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(theme.text_sm)
                            .text_color(foreground)
                            .line_height(relative(1.5))
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(title),
                    ),
            )
            .child(
                div()
                    .text_size(theme.text_xs)
                    .line_height(relative(1.5))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "Codex、Claude 或其他 Agent 服务商自身保存的历史不会被删除。",
                        "History saved by Codex, Claude, or another Agent provider is not deleted.",
                    )),
            );
        if let Some(error) = error {
            body = body.child(
                div()
                    .id("agent-archive-confirmation-error")
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(error),
            );
        }
        div()
            .id("agent-archive-confirmation-surface")
            .w_full()
            .max_h(max_height)
            .min_h(px(0.))
            .text_color(foreground)
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(theme.metrics.section_gap)
                    .px(theme.metrics.dialog_padding)
                    .py(theme.metrics.section_gap)
                    .border_b_1()
                    .border_color(theme.tool_panel_border)
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                DialogTitle::new()
                                    .text_size(theme.text_heading)
                                    .line_height(relative(1.25))
                                    .child(crate::locale::text(
                                        "删除会话记录？",
                                        "Delete chat record?",
                                    )),
                            )
                            .child(
                                DialogDescription::new()
                                    .text_size(theme.text_xs)
                                    .line_height(relative(1.5))
                                    .text_color(theme.muted)
                                    .child(runtime_description),
                            ),
                    )
                    .child(
                        Button::new("agent-archive-close")
                            .ghost()
                            .icon(IconName::Close)
                            .size(theme.metrics.control_height)
                            .accessibility_label(crate::locale::text("关闭弹窗", "Close dialog"))
                            .disabled(busy)
                            .on_click(move |_, window, app| {
                                cancel_parent.update(app, |panel, cx| {
                                    if panel.cancel_archive_confirmation(&cancel_owner, cx) {
                                        window.close_dialog(cx);
                                    }
                                });
                            }),
                    ),
            )
            .child(body)
            .child(self.footer.clone())
    }
}

/// 确认按钮订阅与正文相同的 panel owner，避免弹框忙碌状态下重复归档或取消。
///
/// @author kongweiguang
struct AgentArchiveConfirmationFooter {
    panel: Entity<AgentsPanel>,
    owner: AgentArchiveConfirmationOwner,
    _panel_subscription: Subscription,
}

impl AgentArchiveConfirmationFooter {
    /// footer 独立投影禁用和忙碌文案，与正文共享 panel；外框和焦点仍由 Kit Dialog 持有。
    ///
    /// @author kongweiguang
    fn new(
        panel: Entity<AgentsPanel>,
        owner: AgentArchiveConfirmationOwner,
        cx: &mut Context<Self>,
    ) -> Self {
        let _panel_subscription = cx.observe(&panel, |_, _, cx| cx.notify());
        Self {
            panel,
            owner,
            _panel_subscription,
        }
    }
}

impl Render for AgentArchiveConfirmationFooter {
    /// 失败保持原 owner 以供重试；危险提交与取消共用主题字号，并明确区分不可撤销结果。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (busy, owner_is_current, _) = self
            .panel
            .read(cx)
            .archive_confirmation_feedback(&self.owner);
        let parent = self.panel.clone();
        let owner = self.owner.clone();
        let cancel_parent = parent.clone();
        let cancel_owner = owner.clone();
        let theme = ThemeTokens::current(window, cx).for_tool_panel();
        DialogFooter::new()
            .w_full()
            .flex_shrink_0()
            .justify_end()
            .flex_wrap()
            .gap(theme.metrics.gap)
            .px(theme.metrics.dialog_padding)
            .py(theme.metrics.section_gap)
            .border_t_1()
            .border_color(theme.tool_panel_border)
            .child(
                Button::new("agent-archive-cancel")
                    .ghost()
                    .h(theme.metrics.control_height)
                    .px_3()
                    .text_size(theme.text_sm)
                    .line_height(relative(1.25))
                    .font_weight(gpui_kit::FontWeight::NORMAL)
                    .label(crate::locale::text("取消", "Cancel"))
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        cancel_parent.update(cx, |panel, cx| {
                            if panel.cancel_archive_confirmation(&cancel_owner, cx) {
                                window.close_dialog(cx);
                            }
                        });
                    }),
            )
            .child(
                Button::new("agent-archive-confirm")
                    .danger()
                    .h(theme.metrics.control_height)
                    .px_3()
                    .text_size(theme.text_sm)
                    .line_height(relative(1.25))
                    .font_weight(gpui_kit::FontWeight::NORMAL)
                    .icon(IconName::Trash)
                    .label(if busy {
                        crate::locale::text("正在删除", "Deleting")
                    } else {
                        crate::locale::text("删除记录", "Delete record")
                    })
                    .disabled(busy || !owner_is_current)
                    .on_click(move |_, window, cx| {
                        let window_handle = window.window_handle();
                        parent.update(cx, |panel, cx| {
                            panel.confirm_archive_confirmation(&owner, window_handle, cx);
                        });
                    }),
            )
    }
}

/// 历史 terminal 的退出分类只从 typed 生命周期与回执导出，不解析错误文案。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalExitProjection {
    Normal,
    Failure,
    Unknown,
    Closed,
    Failed,
}

/// 顶栏摘要保持短小，详细错误留在右栏反馈条和 tooltip 中。
///
/// @author kongweiguang
struct TerminalHeaderStatus {
    message: String,
    tone: TerminalStatusTone,
}

/// 终端成功退出、待核对与真实故障分别使用中性、警示和错误语义色。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalStatusTone {
    Neutral,
    Warning,
    Error,
}

impl Render for AgentsPanel {
    /// 根容器透出唯一工具宿主的材质，Agent 终端及卡片继续使用各自的内容表面。
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_tool_panel();
        let terminal_attached = has_selected_agent_terminal_surface(self.selected_view());

        let mut panel = div()
            .id("agents-panel")
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.tool_panel_surface)
            .text_color(theme.foreground);
        if terminal_attached {
            panel = panel.child(self.render_header(theme, terminal_attached, cx));
        }

        if let Some(view) = self.selected_view()
            && let Some(recovery) = &view.resume_recovery
        {
            panel = panel.child(self.render_recovery_banner(
                view.session.agent_session_id.clone(),
                recovery.status,
                theme,
                cx,
            ));
        }

        if !terminal_attached {
            panel = panel
                .h_full()
                .min_h(px(0.))
                .child(self.render_landing_content(theme, window.rem_size(), cx));
        }

        if self.pending_send.is_some() || !self.send_history.is_empty() {
            panel = panel.child(self.render_pending_send(theme, cx));
        }
        if let Some(feedback) = self.visible_terminal_feedback() {
            panel = panel.child(render_terminal_feedback(theme, feedback.message.clone()));
        }
        if let Some(status) = self
            .status
            .clone()
            .filter(|status| !terminal_attached || !is_terminal_success_notice(status))
        {
            let cancel_button =
                self.operation_cancellation.as_ref().map(|cancellation| {
                    let cancel_requested = cancellation.is_cancelled();
                    let (label_zh, label_en, tooltip_zh, tooltip_en) = if cancel_requested {
                        (
                            "正在取消…",
                            "Cancelling…",
                            "正在等待当前操作回执。",
                            "Waiting for the current operation result.",
                        )
                    } else {
                        (
                            "取消操作",
                            "Cancel operation",
                            "请求取消；界面会等待实际结果。",
                            "Request cancellation; the panel will wait for the actual result.",
                        )
                    };
                    Button::new("agents-cancel-operation")
                        .compact()
                        .secondary()
                        .label(crate::locale::text(label_zh, label_en))
                        .tooltip(crate::locale::text(tooltip_zh, tooltip_en))
                        .accessibility_label(crate::locale::text(label_zh, label_en))
                        .disabled(cancel_requested)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_agent_operation(cx)
                        }))
                });
            let launcher_settings_button = self.launcher_setup_prompt.then(|| {
                self.render_launcher_settings_button(
                    "agents-failure-launcher-settings",
                    "检查设置",
                    "Review settings",
                    cx,
                )
            });
            panel = panel.child(render_status(
                theme,
                status,
                cancel_button,
                launcher_settings_button,
            ));
        }
        panel
    }
}

impl AgentsPanel {
    /// 将 Pending/Unknown 的下一步直接呈现；未核对前只保留原事务，不误导用户归档。
    ///
    /// @author kongweiguang
    fn render_recovery_banner(
        &self,
        id: AgentSessionId,
        status: AgentResumeRecoveryStatus,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (title_zh, title_en) = match status {
            AgentResumeRecoveryStatus::Pending => ("恢复待核对", "Recovery needs readback"),
            AgentResumeRecoveryStatus::Unknown => ("恢复结果未知", "Recovery result unknown"),
        };
        div()
            .id("agents-recovery-banner")
            .flex_shrink_0()
            .mx_2()
            .mt_1()
            .p_2()
            .rounded_md()
            .bg(theme.secondary)
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(crate::locale::text(title_zh, title_en)),
            )
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "核对仅处理原恢复事务，不启动新的 CLI。待核对期间不能改名或归档；可保留记录并先执行“核对恢复”。",
                        "Check only the original recovery without starting another CLI. Rename and archiving stay locked until verified; keep the record and reconcile first.",
                    )),
            )
            .child(
                Button::new("agents-reconcile-recovery")
                    .compact()
                    .secondary()
                    .label(crate::locale::text("核对恢复", "Reconcile recovery"))
                    .disabled(self.busy || self.loading || !self.sessions_verified)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        if panel.ensure_recovery_selection(&id, cx) && panel.can_reconcile_selected() {
                            panel.reconcile_agent(id.clone(), cx);
                        }
                    })),
            )
    }

    /// 只将仍选中的 Agent 运行态错误写入该面板，丢弃已切换或已归档会话的迟到结果。
    ///
    /// @author kongweiguang
    pub fn report_terminal_failure(
        &mut self,
        agent_session_id: &AgentSessionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent.as_ref() != Some(agent_session_id) {
            return;
        }
        self.record_terminal_feedback(AgentTerminalFeedback {
            agent_session_id: agent_session_id.clone(),
            message,
            source: AgentTerminalFeedbackSource::RuntimeFailure,
        });
        cx.notify();
    }

    /// Root 将终端操作的 typed 回执与业务状态分离，且不以迟到操作拒绝覆盖真实 runtime 故障。
    ///
    /// @author kongweiguang
    pub fn report_terminal_operation_failure(
        &mut self,
        agent_session_id: &AgentSessionId,
        code: PublicErrorCode,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent.as_ref() != Some(agent_session_id) {
            return;
        }
        let requires_exit_readback =
            AgentTerminalFeedbackSource::OperationFailure(code).clears_after_confirmed_exit();
        self.record_terminal_feedback(AgentTerminalFeedback {
            agent_session_id: agent_session_id.clone(),
            message,
            source: AgentTerminalFeedbackSource::OperationFailure(code),
        });
        if requires_exit_readback {
            self.reload(cx);
        }
        cx.notify();
    }

    /// 入口和历史共用面板内容留白；列只限制最大宽度，避免停靠方向和密度变化时重复缩进。
    /// 入口与历史作为一组在可用高度内垂直居中；历史视口恒为三条行高，记录数量、筛选、
    /// 刷新和读取错误只改变视口内部，不移动上方入口。内容高于视口时由外层滚动兜底，
    /// 居中容器只保证最小高度，不会裁掉超出部分。
    /// 内容层必须禁止 flex 收缩：panel_content 是 flex_col，默认 shrink 会把高内容压回
    /// 视口高，justify_center 随即在负空间里把列顶裁出滚动范围，外层也拿不到真实内容高度。
    /// @author kongweiguang
    fn render_landing_content(
        &self,
        theme: ThemeTokens,
        rem: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        crate::ui_surface::panel_content(theme)
            .id("agents-landing-scroll")
            .debug_selector(|| "rr-agent-landing-scroll".into())
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(
                div()
                    .id("agents-landing-scroll-content")
                    .debug_selector(|| "rr-agent-landing-scroll-content".into())
                    .w_full()
                    .min_h(relative(1.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("agents-landing-column")
                            .debug_selector(|| "rr-agent-landing-column".into())
                            .w_full()
                            .max_w(px(320.))
                            .flex_shrink_0()
                            .py(theme.metrics.gap)
                            .flex()
                            .flex_col()
                            .gap(theme.metrics.panel_padding)
                            .child(self.render_launcher(theme, cx))
                            .child(self.render_session_history(theme, rem, cx)),
                    ),
            )
    }

    /// 已进入的 Agent 只展示返回、身份和真实状态；页头随共享密度缩放，返回不终止原 CLI。
    ///
    /// @author kongweiguang
    fn render_header(
        &mut self,
        theme: ThemeTokens,
        terminal_attached: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected_view();
        let title = selected.map_or_else(String::new, |view| session_picker_label(view, true));
        let icon = selected.map_or(IconName::Bot, |view| launcher_icon(view.session.agent_id));
        let command = selected.map_or_else(String::new, |view| {
            format!(
                "{} · {}",
                view.session.launch.command_label, view.session.launch.cwd
            )
        });
        let status = terminal_header_status_row(
            selected,
            terminal_attached,
            self.visible_terminal_feedback(),
            should_render_continue(selected),
        );
        let mut header = div()
            .id("agents-header")
            .w_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("agents-header-toolbar")
                    .w_full()
                    .min_h(theme.metrics.toolbar_height)
                    .flex_shrink_0()
                    .px(theme.metrics.panel_padding)
                    .py(theme.metrics.gap / 2.)
                    .flex()
                    .items_center()
                    .gap(theme.metrics.gap)
                    .child(
                        Button::new("agents-back")
                            .compact()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .accessibility_label(crate::locale::text(
                                "返回对话列表",
                                "Back to chats",
                            ))
                            .tooltip(crate::locale::text("返回对话列表", "Back to chats"))
                            .disabled(self.busy || self.launcher_selection_busy)
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.show_new_chat(cx)),
                            ),
                    )
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(command),
                            ),
                    ),
            );

        if let Some(status) = status {
            let tooltip_status = status.message.clone();
            let (icon, color) = match status.tone {
                TerminalStatusTone::Neutral => (IconName::CircleCheck, theme.muted),
                TerminalStatusTone::Warning => (IconName::CircleAlert, theme.warning),
                TerminalStatusTone::Error => (IconName::CircleAlert, theme.error),
            };
            header = header.child(
                div()
                    .id("agents-terminal-status-row")
                    .debug_selector(|| "rr-agent-terminal-status-row".into())
                    .w_full()
                    .min_h(px(28.))
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_size(theme.text_xs)
                    .text_color(color)
                    .child(icon)
                    .child(div().flex_1().min_w(px(0.)).child(status.message))
                    .tooltip(move |window, cx| {
                        Tooltip::new(tooltip_status.clone()).build(window, cx)
                    }),
            );
        }
        header
    }

    /// 恢复提示可能在异步刷新期间保持可见；执行前复核原会话身份和当前列表/操作状态。
    /// @author kongweiguang
    fn ensure_recovery_selection(
        &mut self,
        expected_id: &kerminal_domain::identity::AgentSessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        let failure = if self.selected_agent.as_ref() != Some(expected_id) {
            Some((
                "所选 Agent 已变化，请重新选择该会话后核对恢复。",
                "The selected Agent changed. Select the session again to reconcile.",
            ))
        } else if self.busy {
            Some((
                "另一个 Agent 操作正在进行。",
                "Another Agent action is in progress.",
            ))
        } else if self.loading {
            Some((
                "Agent 列表正在刷新，请稍后重试。",
                "The Agent list is refreshing.",
            ))
        } else if !self.sessions_verified {
            Some((
                "Agent 列表不可用，请刷新后重试。",
                "The Agent list is unavailable. Refresh and try again.",
            ))
        } else {
            match self.selected_view() {
                Some(view) if view.session.status != AgentSessionStatus::Archived => None,
                Some(_) => Some((
                    "归档会话不能执行此操作。",
                    "Archived chats cannot use this action.",
                )),
                None => Some((
                    "所选 Agent 已不存在，请刷新列表。",
                    "The selected Agent no longer exists.",
                )),
            }
        };
        if let Some((zh, en)) = failure {
            self.show_action_notice(zh, en, cx);
            false
        } else {
            true
        }
    }

    /// 待核对状态只开放原事务核对动作，避免把“继续”按钮变成第二次 CLI 启动。
    ///
    /// @author kongweiguang
    fn can_reconcile_selected(&self) -> bool {
        self.selected_view().is_some_and(agent_can_reconcile)
    }

    /// 过期操作不静默失效也不自动选新目标，把具体原因留在面板状态区。
    /// @author kongweiguang
    pub(super) fn show_action_notice(&mut self, zh: &str, en: &str, cx: &mut Context<Self>) {
        self.status = Some(crate::locale::text(zh, en).to_string());
        cx.notify();
    }

    /// 选择器与启动动作从同一密度投影计算双行高度，双语和无障碍名称保留真实启动范围。
    /// 空目录仍通过选择菜单进入自定义定义管理。
    ///
    /// @author kongweiguang
    fn render_launcher(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("agents-launcher")
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.render_new_chat_target(theme))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .px(theme.metrics.gap / 4.)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.render_launcher_selector(theme, cx)),
                    )
                    .child({
                        // 顶部“进入”始终创建新会话；运行中会话只能从下方历史行显式继续，
                        // 因此这里保持唯一主按钮，不再为临时选择切换主次样式。
                        Button::new("agents-launcher-start")
                            .compact()
                            .primary()
                            .flex_none()
                            .min_w(theme.metrics.control_height * 1.75)
                            .h(theme.metrics.control_height + theme.metrics.gap * 1.5)
                            .accessibility_label(crate::locale::text("启动 Agent", "Start Agent"))
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .line_height(relative(1.25))
                                    .font_weight(gpui_kit::FontWeight::NORMAL)
                                    .child(crate::locale::text("进入", "Start")),
                            )
                            .disabled(!self.can_create_with_selected_launcher())
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.create_agent(cx)),
                            )
                    }),
            )
            .when_some(self.launcher_load_error.as_ref(), |element, error| {
                let retry_enabled =
                    !self.busy && !self.launcher_selection_busy && !self.launcher_catalog_loading;
                element.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.error)
                                .child(crate::locale::text(
                                    format!("无法读取 Launcher 配置：{error}"),
                                    format!("Could not read launcher settings: {error}"),
                                )),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_1()
                                .child(
                                    Button::new("agents-launcher-catalog-retry")
                                        .compact()
                                        .ghost()
                                        .label(crate::locale::text("重试", "Retry"))
                                        .disabled(!retry_enabled)
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.reload_launcher_catalog(cx)
                                        })),
                                )
                                .child(self.render_launcher_settings_button(
                                    "agents-launcher-load-error-settings",
                                    "设置",
                                    "Settings",
                                    cx,
                                )),
                        ),
                )
            })
            .when_some(self.launcher_selection_error.as_ref(), |element, error| {
                let retry_enabled = !self.busy && !self.launcher_selection_busy;
                element.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(theme.text_xs)
                                .text_color(theme.warning)
                                .child(crate::locale::text(
                                    format!("选择未保存：{error}"),
                                    format!("Selection was not saved: {error}"),
                                )),
                        )
                        .child(
                            Button::new("agents-launcher-retry")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("重试", "Retry"))
                                .disabled(!retry_enabled)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.retry_launcher_selection(cx)
                                })),
                        ),
                )
            })
    }

    /// 将设置入口交给 Root 处理；面板不直接修改偏好，也不猜测具体启动失败原因。
    ///
    /// @author kongweiguang
    fn render_launcher_settings_button(
        &self,
        id: &'static str,
        label_zh: &'static str,
        label_en: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .compact()
            .ghost()
            .label(crate::locale::text(label_zh, label_en))
            .accessibility_label(crate::locale::text(label_zh, label_en))
            .tooltip(crate::locale::text(
                "若错误涉及启动命令或参数，可搜索“Agent 启动器”检查配置；不会自动切换或重试。",
                "If the error involves the launch command or arguments, review Agent Launcher settings. No automatic switch or retry is performed.",
            ))
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                cx.emit(AgentPanelEvent::OpenAgentLauncherSettings);
            }))
    }

    /// 沿用目录顺序与安装快照；选择器和菜单共用主题字号，避免双语及放大时出现独立的小字层级。
    /// 外层裁切补足 Kit Button 不公开的单侧圆角，同时保留其焦点与键盘行为。
    /// 菜单继续由 Kit 管理状态；所有图标由内容绘制，避免尾项的 Plus 让 Kit 为整列额外预留图标槽。
    /// @author kongweiguang
    fn render_launcher_selector(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected_key = self.launcher_key.clone();
        let options = self
            .launcher_catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .options
                    .iter()
                    .map(|option| {
                        (
                            option.key.clone(),
                            option.label.clone(),
                            option.provider,
                            catalog.installation_for(option.provider),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let selected_provider = selected_key.as_ref().and_then(|key| {
            options
                .iter()
                .find(|(option_key, _, _, _)| option_key == key)
                .map(|(_, _, provider, _)| *provider)
        });
        let selected_installation = selected_key.as_ref().and_then(|key| {
            options
                .iter()
                .find(|(option_key, _, _, _)| option_key == key)
                .and_then(|(_, _, _, installation)| *installation)
        });
        let selected_icon = selected_provider.map_or(IconName::Bot, launcher_icon);
        let selected_descriptor = selected_provider
            .map(|provider| launcher_status_descriptor(provider, selected_installation));
        let label = selected_key
            .as_ref()
            .and_then(|key| {
                options
                    .iter()
                    .find(|(option_key, _, _, _)| option_key == key)
                    .map(|(_, label, _, _)| label.clone())
            })
            .unwrap_or_else(|| {
                if self.launcher_catalog_loading {
                    crate::locale::text("正在读取 Agent 选项…", "Loading Agent options…")
                        .to_string()
                } else if self.launcher_load_error.is_some() {
                    crate::locale::text("Agent 选项不可用", "Agent options unavailable").to_string()
                } else if self
                    .launcher_catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.options.is_empty())
                {
                    crate::locale::text("未配置 Agent", "No Agent configured").to_string()
                } else {
                    crate::locale::text("选择 Agent", "Select an Agent").to_string()
                }
            });
        let accessibility_detail = selected_descriptor
            .as_deref()
            .map_or_else(String::new, |detail| format!(" · {detail}"));
        let accessibility_label = crate::locale::text(
            format!("选择 Agent · {label}{accessibility_detail}"),
            format!("Select an Agent · {label}{accessibility_detail}"),
        );
        let entity = cx.entity().clone();
        let disabled = self.busy || self.launcher_selection_busy || self.launcher_catalog_loading;
        let accent = cx.read_global::<KitTheme, _>(|theme, _| theme.colors.primary);

        let picker = Button::new("agents-launcher-selector")
            .compact()
            .secondary()
            .outline()
            .rounded(ButtonRounded::None)
            .px_3()
            .w_full()
            .min_w(px(0.))
            .h(theme.metrics.control_height + theme.metrics.gap * 1.5)
            .dropdown_caret(true)
            .debug_selector(|| "rr-agent-picker".into())
            .accessibility_label(accessibility_label.clone())
            .tooltip(accessibility_label)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(theme.metrics.gap)
                    .child(
                        div()
                            .debug_selector(|| "rr-agent-launcher-icon".into())
                            .size(theme.metrics.control_height - theme.metrics.gap / 2.)
                            .rounded(theme.control_radius)
                            .bg(theme.panel)
                            .text_color(accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(selected_icon),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap_0()
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .line_height(relative(1.25))
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(label),
                            )
                            .when_some(selected_descriptor, |element, detail| {
                                element.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .text_size(theme.text_xs)
                                        .line_height(relative(1.25))
                                        .text_color(theme.subtle)
                                        .child(launcher_installation_dot(
                                            theme,
                                            selected_installation,
                                        ))
                                        .child(detail),
                                )
                            }),
                    ),
            )
            .disabled(disabled)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = crate::ui_popup::agent_launcher_menu(menu, theme);
                for (key, label, provider, installation) in &options {
                    let selected = selected_key.as_ref() == Some(key);
                    let key = key.clone();
                    let label = label.clone();
                    let provider = *provider;
                    let installation = *installation;
                    let entity = entity.clone();
                    let detail = launcher_menu_descriptor(provider, installation);
                    let icon = launcher_icon(provider);
                    let option_id = format!("agents-launcher-option-{}", key.as_str());
                    let option_accessibility_label = format!("{} · {}", label, detail);
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .id(option_id.clone())
                                .debug_selector(|| "rr-agent-launcher-option".into())
                                .aria_label(option_accessibility_label.clone())
                                .flex_1()
                                .min_w(px(0.))
                                .min_h(theme.metrics.control_height)
                                .py(theme.metrics.gap)
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(
                                    div()
                                        .size(theme.metrics.control_height)
                                        .flex_none()
                                        .rounded(theme.control_radius)
                                        .bg(theme.panel)
                                        .text_color(accent)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(icon),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap_0()
                                        .child(
                                            div()
                                                .w_full()
                                                .text_size(theme.text_sm)
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .text_color(theme.foreground)
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(label.clone()),
                                        )
                                        .child(
                                            div()
                                                .w_full()
                                                .flex()
                                                .items_center()
                                                .gap_1()
                                                .min_w(px(0.))
                                                .overflow_hidden()
                                                .text_size(theme.text_xs)
                                                .line_height(relative(1.25))
                                                .text_color(theme.subtle)
                                                .child(launcher_installation_dot(
                                                    theme,
                                                    installation,
                                                ))
                                                .child(detail.clone()),
                                        ),
                                )
                        })
                        .checked(selected)
                        .on_click(move |_, _, app| {
                            let key = key.clone();
                            entity.update(app, |panel, cx| panel.select_launcher(key, cx));
                        }),
                    );
                }
                let manage_entity = entity.clone();
                menu = menu.item(PopupMenuItem::separator());
                menu.item(
                    PopupMenuItem::element(|_, _| {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(IconName::Plus)
                            .child(crate::locale::text("添加自定义 Agent", "Add custom Agent"))
                    })
                    .on_click(move |_, _, app| {
                        manage_entity.update(app, |_panel, cx| {
                            cx.emit(AgentPanelEvent::OpenCustomAgentManager);
                        });
                    }),
                )
            })
            .available_height(px(152.), px(320.))
            .offset(px(6.))
            .trigger_width(px(240.), px(360.));

        div()
            .w_full()
            .rounded_tl(theme.control_radius)
            .rounded_bl(theme.control_radius)
            .overflow_hidden()
            .child(picker)
    }

    /// 目标与 Info 保持辅助层级；字号继承主题，长目标保留省略和信息入口。
    ///
    /// @author kongweiguang
    fn render_new_chat_target(&self, theme: ThemeTokens) -> impl IntoElement {
        let summary = self.target_summary.as_deref().or_else(|| {
            self.candidate
                .as_ref()
                .map(|candidate| candidate.label.as_str())
        });
        let target_label = summary
            .map(str::to_owned)
            .unwrap_or_else(|| crate::locale::text("未绑定终端", "Unbound").to_string());
        let target_tooltip = target_label.clone();
        div()
            .w_full()
            .px_1()
            .mb_1()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(theme.metrics.icon_size)
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .overflow_hidden()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text("当前目标 ·", "Current target · "))
                    .child(
                        div()
                            .id("agents-new-chat-target")
                            .flex_1()
                            .min_w(px(0.))
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(gpui_kit::FontWeight::NORMAL)
                            .text_color(theme.muted)
                            .tooltip(move |window, cx| {
                                Tooltip::new(target_tooltip.clone()).build(window, cx)
                            })
                            .child(target_label),
                    ),
            )
            .child(
                Button::new("agents-launcher-details")
                    .compact()
                    .ghost()
                    .text_color(theme.muted)
                    .flex_none()
                    .size(theme.metrics.icon_size + theme.metrics.gap)
                    .icon(IconName::Info)
                    .accessibility_label(crate::locale::text(
                        "启动器与目标详情",
                        "Launcher and target details",
                    ))
                    .tooltip(self.launcher_technical_detail()),
            )
    }

    /// Info 只显示原始自定义命令或内置启动参数，避免把目标身份混入启动命令提示。
    /// @author kongweiguang
    fn launcher_technical_detail(&self) -> String {
        self.launcher_catalog
            .as_ref()
            .and_then(|catalog| {
                self.launcher_key.as_ref().and_then(|key| {
                    catalog
                        .custom_command_for_display(key)
                        .map(str::to_owned)
                        .or_else(|| {
                            catalog
                                .options
                                .iter()
                                .find(|option| &option.key == key)
                                .map(|option| launcher_command_detail(option.provider))
                        })
                })
            })
            .map(|command| {
                crate::locale::text(
                    format!("启动命令：{command}"),
                    format!("Command: {command}"),
                )
                .to_string()
            })
            .unwrap_or_else(|| {
                crate::locale::text("未选择启动器", "No launcher selected").to_string()
            })
    }

    /// 只有目录中的当前 key 才能启动，避免保存失败或配置外部变更后回退到另一 Agent。
    /// @author kongweiguang
    fn can_create_with_selected_launcher(&self) -> bool {
        !self.busy
            && !self.launcher_selection_busy
            && !self.launcher_catalog_loading
            && !self.loading
            && self.sessions_verified
            && self.launcher_catalog.as_ref().is_some_and(|catalog| {
                self.launcher_key
                    .as_ref()
                    .is_some_and(|key| catalog.options.iter().any(|option| &option.key == key))
            })
    }

    /// 两段筛选共享紧凑文字和等宽槽位；长文案可截断并提供完整提示，窄栏不挤压相邻标题。
    /// @author kongweiguang
    fn render_history_filter(
        &self,
        option: HistoryFilterOption,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let HistoryFilterOption {
            id,
            label_zh,
            label_en,
            description_zh,
            description_en,
            count,
            mode,
        } = option;
        let selected = self.history_mode == mode;
        let foreground = if selected {
            theme.foreground
        } else {
            theme.muted
        };
        let label =
            history_filter_display_text(crate::locale::text(label_zh, label_en).as_ref(), count);
        let description = crate::locale::text(description_zh, description_en).to_string();
        let accessibility_label = format!("{description} · {count}");

        Button::new(id)
            .compact()
            .ghost()
            .flex_1()
            .min_w(px(0.))
            .h(theme.metrics.control_height - theme.metrics.gap / 2.)
            .p_0()
            .accessibility_label(accessibility_label)
            .tooltip(description)
            .child(
                div()
                    .w_full()
                    .h_full()
                    .min_w(px(0.))
                    .rounded(theme.control_radius)
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(theme.metrics.gap)
                    .when(selected, |element| element.bg(theme.panel).shadow_sm())
                    .child(
                        div()
                            .min_w(px(0.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(theme.text_xs)
                            .line_height(relative(1.25))
                            .font_weight(gpui_kit::FontWeight::NORMAL)
                            .text_color(foreground)
                            .child(label),
                    ),
            )
            .on_click(
                cx.listener(move |panel, _: &ClickEvent, _, cx| panel.set_history_mode(mode, cx)),
            )
    }

    /// 历史区将标题、刷新和筛选保持在同一对齐行；窄栏允许筛选收缩并保留完整可访问名称。
    /// 刷新进度留在标题按钮中，保留原列表几何；错误栏以稳定 ID 持有滚动状态，成功重试后收起。
    /// @author kongweiguang
    fn render_session_history(
        &self,
        theme: ThemeTokens,
        rem: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut history = div()
            .id("agents-session-history")
            .w_full()
            .pt_1()
            .pb_2()
            .flex()
            .flex_col()
            .gap_1();
        let refresh_disabled = self.loading
            || self.busy
            || self.launcher_catalog_loading
            || self.launcher_selection_busy;
        history = history.child(
            div()
                .id("agents-history-heading")
                .debug_selector(|| "rr-agent-history-heading".into())
                .w_full()
                .mb_1()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .min_h(theme.metrics.icon_size)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .flex()
                        .items_center()
                        .text_size(theme.text_xs)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(crate::locale::text("对话", "Chats")),
                )
                .child(
                    Button::new("agents-history-refresh")
                        .compact()
                        .ghost()
                        .w(theme.metrics.control_height)
                        .h(theme.metrics.control_height)
                        .icon(IconName::RefreshCw)
                        .loading(self.loading)
                        .accessibility_label(crate::locale::text("刷新会话", "Refresh chats"))
                        .tooltip(crate::locale::text("刷新会话", "Refresh chats"))
                        .disabled(refresh_disabled)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.reload_all(cx))),
                )
                .child(
                    div()
                        .id("agents-history-filter")
                        .flex()
                        .items_center()
                        .w(theme.metrics.control_height * 6.)
                        .min_w(px(0.))
                        .flex_shrink_1()
                        .p(theme.metrics.gap / 4.)
                        .rounded(theme.radius)
                        .bg(theme.secondary)
                        .child(self.render_history_filter(
                            HistoryFilterOption {
                                id: "agents-history-current",
                                label_zh: "当前目标",
                                label_en: "Current target",
                                description_zh: "当前目标会话",
                                description_en: "Chats for the current target",
                                count: self.current_target_count,
                                mode: AgentHistoryMode::CurrentTarget,
                            },
                            theme,
                            cx,
                        ))
                        .child(self.render_history_filter(
                            HistoryFilterOption {
                                id: "agents-history-all",
                                label_zh: "全部",
                                label_en: "All",
                                description_zh: "全部未归档会话",
                                description_en: "All non-archived chats",
                                count: self.all_count,
                                mode: AgentHistoryMode::All,
                            },
                            theme,
                            cx,
                        )),
                ),
        );
        let mut body = div()
            .id("agents-history-body")
            .debug_selector(|| "rr-agent-history-body".into())
            .w_full()
            .h(history_viewport_height(theme, rem))
            .min_h(px(0.))
            .flex()
            .flex_col();
        if !self.loading && !self.sessions_verified {
            let error = self.history_load_error.clone().unwrap_or_else(|| {
                crate::locale::text("暂时无法读取会话。", "Chats are temporarily unavailable.")
                    .to_string()
            });
            body = body.child(
                div()
                    .id("agent-history-error")
                    .debug_selector(|| "rr-agent-history-error".into())
                    .flex_shrink_0()
                    .max_h(px(96.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(theme.content_surface())
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .when(!self.visible_session_ids.is_empty(), |element| {
                                element.child(crate::locale::text(
                                    "刷新失败，保留上次结果。",
                                    "Refresh failed; previous results are preserved.",
                                ))
                            })
                            .child(error),
                    )
                    .child(
                        Button::new("agents-refresh")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("重试", "Retry"))
                            .disabled(refresh_disabled)
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.reload_all(cx)),
                            ),
                    ),
            );
        }
        if self.visible_session_ids.is_empty() {
            if !self.loading && !self.sessions_verified {
                return history.child(body);
            }
            return history.child(body.child(self.render_empty_state(theme)));
        }
        // 行式列表去掉卡片外框和底色，只保留行内分隔线；外层视口持有稳定 debug 身份，
        // 内层滚动容器使用 Kit 滚动条。行内容右侧始终留出 gutter，滚动条出现时不再挤压正文。
        // 先一次性收集可见行，避免重复遍历和两套过滤条件产生不一致的末行分隔线判断。
        let visible: Vec<&AgentSessionView> = self
            .sessions
            .iter()
            .filter(|view| {
                self.visible_session_ids
                    .contains(&view.session.agent_session_id)
            })
            .collect();
        let visible_count = visible.len();
        let mut rows = div()
            .id("agents-session-list")
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_y_scrollbar();
        for (visible_index, view) in visible.into_iter().enumerate() {
            rows = rows.child(self.render_session_row(
                view,
                visible_index + 1 < visible_count,
                theme,
                rem,
                cx,
            ));
        }
        let list = div()
            .id("agents-session-list-viewport")
            .debug_selector(|| "rr-agent-history-list".into())
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(rows);
        history.child(body.child(list))
    }

    /// 主按钮继续原会话；改名和删除均捕获稳定身份，避免动作落到后来选中的记录。
    /// 两行行式布局：第一行标题与固定操作槽，第二行身份与状态；主按钮覆盖整行但使用
    /// 透明自定义变体，悬停只由外行中性表面表达，避免底层 ghost 把整行刷成强调色。
    /// 管理按钮阻挡底层命中；单行改名通过 Styled 指定高度，Input 自带的 h 仅控制多行编辑器。
    /// 选中态和可选择性由 self 与 view 在同一渲染帧内唯一决定，行内计算即可，避免调用方
    /// 重复传入可推导的布尔值；分隔线仍由可见列表位置决定，保留为参数。
    ///
    /// @author kongweiguang
    fn render_session_row(
        &self,
        view: &AgentSessionView,
        show_separator: bool,
        theme: ThemeTokens,
        rem: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.selected_agent.as_ref() == Some(&view.session.agent_session_id);
        let selectable = self.sessions_verified && !self.loading;
        let id = view.session.agent_session_id.clone();
        let title = view.session.title.clone();
        let can_rename = view.resume_recovery.is_none();
        let can_archive = agent_can_archive(view);
        let running = has_running_agent_terminal(view);
        let mode = self.history_mode;
        let scope = self.scope.clone();
        let candidate = self.candidate.as_ref().map(|value| value.binding.clone());
        let is_current_target = session_matches_current_target(view, &scope);
        let provider = session_provider_label(view.session.agent_id);
        let scope_label = session_scope_label(view, is_current_target);
        let time = format_session_time(view);
        let (display_metadata, full_identity) =
            history_metadata_text(&provider, &scope_label, &time, mode, is_current_target);
        let status_label = lifecycle_label(view);
        let status_color = if view.resume_recovery.is_some() {
            theme.warning
        } else if running {
            theme.accent
        } else if view
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Failed)
        {
            theme.error
        } else {
            theme.muted
        };
        // 两行布局的右侧固定槽：普通态放改名/删除，rename 态在同一位置换成保存/取消。
        // 槽宽按两个 2rem 图标按钮加 gap_1 预算，标题和输入只占剩余宽度，行高不随状态变化。
        let action_slot = rem * 4.25;

        if self
            .rename_target
            .as_ref()
            .is_some_and(|(rename_id, _)| rename_id == &id)
        {
            let rename_title_valid =
                super::can_save_agent_session_title(self.rename_title.read(cx).value().as_ref());
            let rename_row_debug = format!("rr-agent-history-row-{}", id.as_str());
            return div()
                .id(format!("agent-session-rename-{}", id.as_str()))
                .debug_selector(move || rename_row_debug.clone())
                .relative()
                .w_full()
                .h(history_row_height(theme, rem))
                .flex_shrink_0()
                .p_2()
                .pr(HISTORY_SCROLLBAR_GUTTER)
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            // 输入框与保存/取消共用第一行；右侧槽位与普通态同宽，
                            // 切换改名不会让标题列或行高跳动。
                            div()
                                .debug_selector(|| "rr-agent-rename-input".into())
                                .flex_1()
                                .min_w(px(0.))
                                .child(
                                    gpui_kit::Styled::h(
                                        Input::new(&self.rename_title).w_full(),
                                        theme.metrics.control_height,
                                    )
                                    .disabled(self.busy || !selectable || !can_rename),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "rr-agent-history-rename-actions".into())
                                // 与普通态操作槽保持一致：保存/取消按钮之间的间隙也不能
                                // 穿透到行级命中区，滚轮仍可滚动列表。
                                .block_mouse_except_scroll()
                                .w(action_slot)
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_1()
                                .child(
                                    Button::new(format!("agent-session-save-{}", id.as_str()))
                                        .compact()
                                        .ghost()
                                        .icon(IconName::Check)
                                        .accessibility_label(crate::locale::text(
                                            "保存标题",
                                            "Save title",
                                        ))
                                        .tooltip(crate::locale::text("保存标题", "Save title"))
                                        .disabled(
                                            self.busy
                                                || !selectable
                                                || !can_rename
                                                || !rename_title_valid,
                                        )
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.save_agent_title(cx)
                                        })),
                                )
                                .child(
                                    Button::new(format!("agent-session-cancel-{}", id.as_str()))
                                        .compact()
                                        .ghost()
                                        .icon(IconName::X)
                                        .accessibility_label(crate::locale::text(
                                            "取消改名",
                                            "Cancel rename",
                                        ))
                                        .tooltip(crate::locale::text("取消改名", "Cancel rename"))
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.cancel_agent_rename(cx)
                                        })),
                                ),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(session_metadata(
                            &display_metadata,
                            &full_identity,
                            theme,
                            &id,
                        ))
                        .child(session_status_text(&status_label, status_color, theme, &id)),
                )
                .when(show_separator, |element| {
                    element.child(history_row_separator(theme, rem))
                });
        }

        let continue_id = id.clone();
        let continue_title = title.clone();
        let continue_scope = scope.clone();
        let continue_candidate = candidate.clone();
        // 完整身份与状态写入可访问名称和 tooltip；当前目标筛选下第二行可省略重复 scope，
        // 但辅助技术仍能读到 provider、归属、时间和真实生命周期。
        let continue_label = crate::locale::text(
            format!("继续对话 {title} · {full_identity} · {status_label}"),
            format!("Continue chat {title} · {full_identity} · {status_label}"),
        );
        // 整行主按钮用透明自定义变体：ghost() 的 hover 会画强调色，覆盖外行中性表面；
        // 这里把 normal/hover/active 都设为透明，焦点线仍用正文色保持键盘可见。
        let transparent = theme.panel.alpha(0.);
        let primary_variant = ButtonCustomVariant::new(cx)
            .color(transparent)
            .foreground(theme.foreground)
            .hover_foreground(theme.foreground)
            .hover(transparent)
            .active(transparent);
        let primary = Button::new(format!("agent-session-continue-{}", id.as_str()))
            .compact()
            .custom(primary_variant)
            .accessibility_label(continue_label.clone())
            .tooltip(continue_label)
            .disabled(!selectable || self.busy)
            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                panel.continue_agent_from_history(
                    continue_id.clone(),
                    continue_title.clone(),
                    mode,
                    continue_scope.clone(),
                    continue_candidate.clone(),
                    cx,
                );
            }));

        let rename_id = id.clone();
        let rename_title = title.clone();
        let rename_scope = scope.clone();
        let rename_candidate = candidate.clone();
        let rename = Button::new(format!("agent-session-rename-{}", id.as_str()))
            .block_mouse_except_scroll()
            .compact()
            .ghost()
            .icon(IconName::Pencil)
            .accessibility_label(crate::locale::text(
                format!("重命名 {title}"),
                format!("Rename {title}"),
            ))
            .tooltip(crate::locale::text("重命名", "Rename"))
            .disabled(self.busy || !selectable || !can_rename)
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.begin_rename_agent(
                    AgentRenameTarget {
                        id: rename_id.clone(),
                        expected_title: rename_title.clone(),
                    },
                    mode,
                    rename_scope.clone(),
                    rename_candidate.clone(),
                    window,
                    cx,
                );
            }));

        let confirmation_owner = AgentArchiveConfirmationOwner {
            id: id.clone(),
            expected_title: title.clone(),
            mode,
            scope: scope.clone(),
            candidate: candidate.clone(),
            generation: 0,
        };
        let delete = Button::new(format!("agent-session-delete-{}", id.as_str()))
            .block_mouse_except_scroll()
            .compact()
            .ghost()
            .icon(IconName::Trash)
            .accessibility_label(crate::locale::text(
                format!("删除 {title}"),
                format!("Delete {title}"),
            ))
            .tooltip(crate::locale::text("删除记录", "Delete chat record"))
            .disabled(self.busy || !selectable || !can_archive)
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.open_archive_confirmation(confirmation_owner.clone(), window, cx);
            }));

        let row_debug = format!("rr-agent-history-row-{}", id.as_str());
        div()
            .id(format!("agent-history-row-{}", id.as_str()))
            .debug_selector(move || row_debug.clone())
            .relative()
            .w_full()
            .h(history_row_height(theme, rem))
            .flex_shrink_0()
            .p_2()
            // 右侧 gutter 固定为 Kit 滚动条宽度；4 条以上出现 thumb 时不再横向回流。
            .pr(HISTORY_SCROLLBAR_GUTTER)
            .flex()
            .flex_col()
            .gap_1()
            .when(active, |element| {
                element.bg(history_row_surface(theme, true, false))
            })
            .when(!active, |element| {
                element.hover(move |style| style.bg(history_row_surface(theme, false, true)))
            })
            .child(primary.absolute().inset_0().size_full())
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        // 标题与操作同处第一行：操作槽固定宽度，标题只占剩余空间并截断；
                        // 完整标题仍在主按钮的可访问名称与 tooltip 中，不靠缩小字体硬塞。
                        div()
                            .debug_selector(|| "rr-agent-history-title".into())
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(theme.text_xs)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .debug_selector(|| "rr-agent-history-actions".into())
                            // 两个图标按钮之间的 gap 仍属于整行主按钮的命中区；容器必须
                            // 挡住非滚轮鼠标事件，否则点间隙会误触继续会话。滚轮保持透传，
                            // 列表仍可在操作槽上滚动。
                            .block_mouse_except_scroll()
                            .w(action_slot)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_1()
                            .child(rename)
                            .child(delete),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(session_metadata(
                        &display_metadata,
                        &full_identity,
                        theme,
                        &id,
                    ))
                    .child(session_status_text(&status_label, status_color, theme, &id)),
            )
            .when(show_separator, |element| {
                element.child(history_row_separator(theme, rem))
            })
    }

    /// 空列表保留 main 的描边卡片；正常加载使用中性文字，读取故障由列表的唯一重试提示表达。
    /// @author kongweiguang
    fn render_empty_state(&self, theme: ThemeTokens) -> impl IntoElement {
        let show_loading_detail = self.loading;
        let (title, body) = if self.loading {
            (
                crate::locale::text("正在读取会话", "Loading chats"),
                crate::locale::text(
                    "读取完成后会显示最近会话。",
                    "Recent chats appear after loading.",
                ),
            )
        } else if self.history_mode == AgentHistoryMode::CurrentTarget {
            (
                crate::locale::text("当前目标暂无对话", "No chats for this target"),
                crate::locale::text(
                    "可切换到全部历史查看其他会话。",
                    "Switch to All to view other chats.",
                ),
            )
        } else {
            (
                crate::locale::text("暂无 Agent 对话", "No chat history"),
                crate::locale::text("新建后的会话会显示在这里。", "New chats will appear here."),
            )
        };
        let mut empty_state = div()
            .id("agents-empty-state")
            .debug_selector(|| "rr-agent-empty-state".into())
            .w_full();
        if show_loading_detail {
            empty_state = empty_state
                .min_h(px(96.))
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.content_surface())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1();
        } else {
            empty_state = empty_state
                .px_3()
                .py(px(28.))
                .text_center()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.content_surface());
        }
        empty_state = empty_state.child(
            div()
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(title),
        );
        if show_loading_detail {
            empty_state = empty_state.child(
                div()
                    .max_w(px(260.))
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .text_center()
                    .child(body),
            );
        }
        empty_state
    }

    /// 显示 Root 提供的短期发送预览与无正文历史；确认事件只携带 opaque IDs。
    ///
    /// @author kongweiguang
    fn render_pending_send(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pending = self.pending_send.clone();
        let preview = self.pending_send_preview.clone();
        let history = self.send_history.clone();
        let selected_agent = self.selected_agent.clone();
        let mut content = div()
            .id("agents-pending-send")
            .w_full()
            .max_h(px(300.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.materials.chrome);

        if let Some(pending) = pending {
            let preview = preview.filter(|preview| preview.request_id == pending.request_id);
            content = content.child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(IconName::Send)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(crate::locale::text("发送到 Agent", "Send to Agent")),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(pending.source_label.clone()),
                            ),
                    ),
            );
            if let Some(preview) = preview {
                let expired = pending_send_preview_expired(&preview);
                let target_matches = selected_agent.as_ref() == Some(&preview.agent_session_id);
                let confirm_request_id = preview.request_id.clone();
                let confirm_preview_id = preview.preview_id.clone();
                let confirm_agent_id = preview.agent_session_id.clone();
                let cancel_request_id = pending.request_id;
                let mut preview_body = div()
                    .id("agents-pending-send-body")
                    .min_h(px(0.))
                    .max_h(px(136.))
                    .mx_3()
                    .mt_2()
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.materials.shell)
                    .p_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.foreground)
                    .child(gpui_kit::text!(
                        id = "agents-pending-send-body-text",
                        preview.text.clone()
                    ));
                if preview.redacted || preview.truncated || expired || !target_matches {
                    let warning = if expired {
                        crate::locale::text(
                            "发送预览已过期，请重新选择内容。",
                            "The send preview expired. Select the content again.",
                        )
                    } else if !target_matches {
                        crate::locale::text(
                            "预览关联的 Agent 已变化，请选择对应会话。",
                            "The preview belongs to another Agent session. Select that session to continue.",
                        )
                    } else if preview.truncated {
                        crate::locale::text("预览内容已截断。", "The preview is truncated.")
                    } else {
                        crate::locale::text(
                            "预览内容包含脱敏数据。",
                            "The preview contains redacted data.",
                        )
                    };
                    preview_body = preview_body.text_color(theme.warning);
                    content = content.child(
                        div()
                            .mx_3()
                            .mt_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.warning)
                            .child(warning),
                    );
                }
                let preview_details = crate::locale::text(
                    format!(
                        "{} · {} B · 截止 {}",
                        pending_source_short_label(&pending.source_label),
                        preview.byte_length,
                        send_preview_expiry_label(&preview.expires_at)
                    ),
                    format!(
                        "{} · {} B · Expires {}",
                        pending_source_short_label(&pending.source_label),
                        preview.byte_length,
                        send_preview_expiry_label(&preview.expires_at)
                    ),
                );
                content = content.child(preview_body).child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("agents-pending-send-cancel")
                                .compact()
                                .ghost()
                                .icon(IconName::X)
                                .label(crate::locale::text("取消", "Cancel"))
                                .disabled(self.busy)
                                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                    panel.pending_send = None;
                                    panel.pending_send_preview = None;
                                    cx.emit(AgentPanelEvent::PendingSendCancelled {
                                        request_id: cancel_request_id.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("agents-pending-send-confirm")
                                .compact()
                                .primary()
                                .icon(IconName::ArrowRight)
                                .label(crate::locale::text("确认发送", "Send"))
                                .disabled(self.busy || expired || !target_matches)
                                .tooltip(preview_details)
                                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                    if panel.pending_send_preview.as_ref().is_none_or(|current| {
                                        current.request_id != confirm_request_id
                                            || current.preview_id != confirm_preview_id
                                            || pending_send_preview_expired(current)
                                    }) || panel.selected_agent.as_ref()
                                        != Some(&confirm_agent_id)
                                    {
                                        return;
                                    }
                                    panel.pending_send_preview = None;
                                    cx.emit(AgentPanelEvent::PendingSendConfirmed {
                                        agent_session_id: confirm_agent_id.clone(),
                                        request_id: confirm_request_id.clone(),
                                        preview_id: confirm_preview_id.clone(),
                                    });
                                })),
                        ),
                );
            } else {
                content = content.child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "选择一个 Agent 会话后生成安全预览。",
                            "Select an Agent session to create a safe preview.",
                        )),
                );
            }
        }

        if !history.is_empty() {
            content = content.child(render_send_history(&history, theme));
        }
        content
    }
}

/// 过期预览按失效处理；Root service 仍是发送前最终校验的权威来源。
///
/// @author kongweiguang
fn pending_send_preview_expired(preview: &AgentPendingSendPreview) -> bool {
    DateTime::parse_from_rfc3339(&preview.expires_at).map_or(true, |expires| {
        expires.timestamp_millis() <= chrono::Utc::now().timestamp_millis()
    })
}

/// Root supplies a sanitized source label; keep the displayed label bounded in the narrow rail.
///
/// @author kongweiguang
fn pending_source_short_label(label: &str) -> String {
    const MAX_CHARS: usize = 42;
    let mut chars = label.chars();
    let value = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{value}…")
    } else {
        value
    }
}

/// 将历史 metadata 渲染为不含正文的可扫读列表。
///
/// @author kongweiguang
fn render_send_history(
    history: &[AgentSendHistoryMetadata],
    theme: ThemeTokens,
) -> impl IntoElement {
    let mut list = div()
        .id("agents-send-history")
        .w_full()
        .max_h(px(104.))
        .overflow_y_scroll()
        .border_t_1()
        .border_color(theme.border);
    for item in history.iter().take(8) {
        let (outcome, color) = match item.outcome {
            AgentSendOutcome::Sent => (crate::locale::text("已发送", "Sent"), theme.accent),
            AgentSendOutcome::Queued => (crate::locale::text("已排队", "Queued"), theme.accent),
            AgentSendOutcome::Cancelled => {
                (crate::locale::text("已取消", "Cancelled"), theme.muted)
            }
            AgentSendOutcome::Expired => (crate::locale::text("已过期", "Expired"), theme.warning),
            AgentSendOutcome::Failed => (crate::locale::text("失败", "Failed"), theme.error),
        };
        list = list.child(
            div()
                .id(format!("agents-send-history-{}", item.request_id))
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.foreground)
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(pending_source_short_label(&item.source_label)),
                        )
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(format!("{} · {} B", item.created_at, item.byte_length)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(theme.text_xs)
                        .text_color(color)
                        .child(outcome),
                ),
        );
    }
    list
}

/// 把 ISO 时间显示为本地时钟时间；格式无效时保留来源字符串以便故障核对。
///
/// @author kongweiguang
fn send_preview_expiry_label(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|expires| {
            expires
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// 身份行固定单行省略；显示文案可省略重复 scope，但 tooltip 始终保留完整身份，
/// 避免窄栏或 rem2 时换行把状态文字挤出固定行高。
///
/// @author kongweiguang
fn session_metadata(
    display: &str,
    full_identity: &str,
    theme: ThemeTokens,
    row_id: &AgentSessionId,
) -> impl IntoElement {
    let tooltip = full_identity.to_owned();
    div()
        .id(format!("agent-history-metadata-{}", row_id.as_str()))
        .debug_selector(|| "rr-agent-history-metadata".into())
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(theme.text_xs)
        .line_height(relative(1.25))
        .text_color(theme.subtle)
        .child(display.to_owned())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

/// 状态只用文字和语义色表达，不再画描边 badge；文字本身承载运行/失败/待核对语义，
/// 颜色只做辅助，避免仅靠颜色区分状态。
/// 窄行下状态保留最小可读宽度并可收缩省略，完整状态仍由 tooltip 和行主按钮的
/// 无障碍标签承载；否则长英文状态会横向溢出整行并压住滚动条留白。
///
/// @author kongweiguang
fn session_status_text(
    label: &str,
    status_color: gpui_kit::Hsla,
    theme: ThemeTokens,
    row_id: &AgentSessionId,
) -> impl IntoElement {
    let tooltip = label.to_owned();
    div()
        .id(format!("agent-session-status-{}", row_id.as_str()))
        .debug_selector(|| "rr-agent-history-status".into())
        .flex_shrink_1()
        .min_w(theme.text_xs * 3.)
        // 状态最多占第二行六成，给 provider · 相对时间留出非零预算；超出时
        // 单行省略，完整状态仍由 tooltip 和行主按钮的无障碍标签承载。
        .max_w(relative(0.6))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(theme.text_xs)
        .line_height(relative(1.25))
        .text_color(status_color)
        .child(label.to_owned())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

/// 卡片归属标签与 application 的历史筛选保持一致；终端重连不改变会话所属 Tab。
///
/// @author kongweiguang
fn session_matches_current_target(
    view: &AgentSessionView,
    current_scope: &AgentSessionScope,
) -> bool {
    view.session.scope.as_ref() == Some(current_scope)
}

/// 当前目标记录简称为“当前目标”，其它历史按 main 的 global/tab 权限范围显示。
///
/// @author kongweiguang
fn session_scope_label(view: &AgentSessionView, is_current_target: bool) -> String {
    if is_current_target {
        return crate::locale::text("当前目标", "Current target").to_string();
    }
    let scope = view.session.scope.as_ref();
    match scope {
        Some(AgentSessionScope::Tab { .. }) => {
            crate::locale::text("当前 Tab", "Current tab").to_string()
        }
        Some(AgentSessionScope::Global) => {
            crate::locale::text("整个 Kerminal", "Entire Kerminal").to_string()
        }
        None if view
            .session
            .target
            .as_ref()
            .is_some_and(|target| target.tab_id.is_some()) =>
        {
            crate::locale::text("当前 Tab", "Current tab").to_string()
        }
        None => crate::locale::text("整个 Kerminal", "Entire Kerminal").to_string(),
    }
}

/// Provider 标签与 main 历史卡片的词汇保持一致，不把自定义启动命令内容展开到列表。
///
/// @author kongweiguang
fn session_provider_label(provider: AgentProvider) -> String {
    let (zh, en) = match provider {
        AgentProvider::Codex => ("Codex", "Codex"),
        AgentProvider::Claude => ("Claude", "Claude"),
        AgentProvider::Pi => ("PI Agent", "PI Agent"),
        AgentProvider::Custom => ("Custom", "Custom"),
    };
    crate::locale::text(zh, en).to_string()
}

/// 历史行固定展示会话创建时间：created_at 是 Unix epoch 秒字符串，转成本地时区后
/// 只保留 M/D HH:MM。updated_at 记录业务状态变化，不能替代创建时间；空、非数字和
/// 超出 chrono 范围的记录显示未知，不把缺失伪造成当前时间。
///
/// @author kongweiguang
fn format_session_time(view: &AgentSessionView) -> String {
    format_session_time_for_language(view, crate::locale::current_language())
}

/// 显式语言版本让测试验证中英文兜底而不改进程级 locale；生产 wrapper 仍只读取
/// current_language，避免测试并发污染其它模块的全局语言。
///
/// @author kongweiguang
fn format_session_time_for_language(view: &AgentSessionView, language: UiLanguage) -> String {
    view.session
        .created_at
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%-m/%-d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| match language {
            UiLanguage::ZhCn => "时间未知".to_owned(),
            UiLanguage::EnUs => "Time unknown".to_owned(),
        })
}

/// 将图标身份与安全的短命令提示统一映射，菜单和当前选择不会出现相互矛盾的说明。
/// @author kongweiguang
fn launcher_icon(provider: AgentProvider) -> IconName {
    match provider {
        AgentProvider::Codex => IconName::Terminal,
        AgentProvider::Claude => IconName::Sparkles,
        AgentProvider::Pi => IconName::Pi,
        AgentProvider::Custom => IconName::Wrench,
    }
}

/// 优先呈现本次目录探测结果；未探测或 Custom 时继续显示来源状态，不推断命令可运行。
/// @author kongweiguang
fn launcher_status_descriptor(provider: AgentProvider, installation: Option<bool>) -> String {
    if let Some(descriptor) = launcher_installation_descriptor(provider, installation) {
        return descriptor;
    }
    let descriptor = match provider {
        AgentProvider::Codex => {
            crate::locale::text("内置启动器 · codex", "Built-in launcher · codex")
        }
        AgentProvider::Claude => {
            crate::locale::text("内置启动器 · claude", "Built-in launcher · claude")
        }
        AgentProvider::Pi => crate::locale::text("内置启动器 · pi", "Built-in launcher · pi"),
        AgentProvider::Custom => {
            crate::locale::text("已保存 · 自定义命令", "Saved · Custom command")
        }
    };
    descriptor.to_string()
}

/// 菜单使用同一份安装快照；未知和 Custom 保持中性来源文案，避免目录成员被误读成可运行。
///
/// @author kongweiguang
fn launcher_menu_descriptor(provider: AgentProvider, installation: Option<bool>) -> String {
    if let Some(descriptor) = launcher_installation_descriptor(provider, installation) {
        return descriptor;
    }
    match provider {
        AgentProvider::Codex => crate::locale::text("内置 · codex", "Built-in · codex"),
        AgentProvider::Claude => crate::locale::text("内置 · claude", "Built-in · claude"),
        AgentProvider::Pi => crate::locale::text("内置 · pi", "Built-in · pi"),
        AgentProvider::Custom => {
            crate::locale::text("已保存 · 自定义命令", "Saved · custom command")
        }
    }
    .to_string()
}

/// 只有 builtin 的明确探测结果才生成安装状态，Custom 与未接入探测的调用返回 None。
///
/// @author kongweiguang
fn launcher_installation_descriptor(
    provider: AgentProvider,
    installation: Option<bool>,
) -> Option<String> {
    let command = match provider {
        AgentProvider::Codex => "codex",
        AgentProvider::Claude => "claude",
        AgentProvider::Pi => "pi",
        AgentProvider::Custom => return None,
    };
    let installed = installation?;
    let (zh, en) = if installed {
        ("可用", "Available")
    } else {
        ("需安装", "Needs installation")
    };
    Some(crate::locale::text(format!("{zh} · {command}"), format!("{en} · {command}")).to_string())
}

/// 技术提示里的默认命令与应用层首次启动 argv 保持一致；可运行状态仍不由静态目录推断。
/// @author kongweiguang
fn launcher_command_detail(provider: AgentProvider) -> String {
    kerminal_application::agents::builtin_launch_command_line(provider).unwrap_or_else(|| {
        crate::locale::text(
            "自定义命令（在管理器中查看）",
            "Custom command (see manager)",
        )
        .to_string()
    })
}

/// 仅对明确探测结果使用语义色；无探测时以中性点保留原有外观。
///
/// @author kongweiguang
fn launcher_installation_dot(theme: ThemeTokens, installation: Option<bool>) -> impl IntoElement {
    let color = match installation {
        Some(true) => theme.success,
        Some(false) => theme.warning,
        None => theme.muted,
    };
    div().size(px(6.)).rounded_full().bg(color)
}

/// 计数与标签留在同一不换行文本中，避免窄 flex 段压掉零值。
///
/// @author kongweiguang
fn history_filter_display_text(label: &str, count: usize) -> String {
    format!("{label} {count}")
}

/// 只在有业务回执时展开有限高度；长错误允许换行并提供完整提示，不能藏掉核对后再试的指引。
/// @author kongweiguang
fn render_status(
    theme: ThemeTokens,
    status: String,
    cancel_button: Option<Button>,
    launcher_settings_button: Option<Button>,
) -> impl IntoElement {
    let tooltip_status = status.clone();
    let mut row = div()
        .id("agents-status")
        .w_full()
        .max_h(px(96.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .overflow_y_scroll()
        .bg(theme.materials.chrome)
        .border_t_1()
        .border_color(theme.border);
    if let Some(button) = cancel_button {
        row = row.child(button);
    }
    if let Some(button) = launcher_settings_button {
        row = row.child(button);
    }
    row.child(
        div()
            .id("agents-status-message")
            .flex_1()
            .min_w_0()
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .child(status)
            .tooltip(move |window, cx| Tooltip::new(tooltip_status.clone()).build(window, cx)),
    )
}

/// 故障与业务回执分别保留；有限换行和完整提示兼顾终端空间与失败恢复指引的可读性。
///
/// @author kongweiguang
fn render_terminal_feedback(theme: ThemeTokens, message: String) -> impl IntoElement {
    let tooltip_message = message.clone();
    div()
        .id("agents-terminal-feedback")
        .w_full()
        .max_h(px(96.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .overflow_y_scroll()
        .bg(theme.materials.chrome)
        .border_t_1()
        .border_color(theme.border)
        .child(IconName::CircleAlert)
        .child(
            div()
                .id("agents-terminal-feedback-message")
                .flex_1()
                .min_w_0()
                .text_size(theme.text_xs)
                .text_color(theme.error)
                .child(message)
                .tooltip(move |window, cx| Tooltip::new(tooltip_message.clone()).build(window, cx)),
        )
}

/// Agent 启动确认已由终端画布证明，避免在右栏顶栏重复显示并挤压会话名称。
/// @author kongweiguang
fn is_terminal_success_notice(status: &str) -> bool {
    matches!(
        status,
        "Agent CLI 已启动。"
            | "Agent CLI started."
            | "Agent 已完成启动，已打开新会话。"
            | "Agent finished starting and opened a new chat."
            | "Agent 已继续运行。"
            | "Agent resumed."
            | "已切回运行中的 Agent 会话。"
            | "Returned to the running Agent chat."
    )
}

/// 保留活跃 Agent 的终端投影，包括 Exited 快照；归档与待核对恢复状态回到入口。
/// @author kongweiguang
fn has_agent_terminal_surface(view: &AgentSessionView) -> bool {
    view.session.status != AgentSessionStatus::Archived
        && view.resume_recovery.is_none()
        && view.terminal.is_some()
}

/// 没有选中的会话时绘制 Launcher；切换新建模式只隐藏原终端投影，不改变它的生命周期。
///
/// @author kongweiguang
fn has_selected_agent_terminal_surface(selected: Option<&AgentSessionView>) -> bool {
    selected.is_some_and(has_agent_terminal_surface)
}

/// 状态文案同时出现在会话标题和历史行，统一判定避免两处界面给出不同生命周期。
/// @author kongweiguang
fn lifecycle_label(view: &AgentSessionView) -> String {
    if let Some(recovery) = &view.resume_recovery {
        let (zh, en) = match recovery.status {
            AgentResumeRecoveryStatus::Pending => ("待核对", "Needs readback"),
            AgentResumeRecoveryStatus::Unknown => ("结果未知", "Unknown result"),
        };
        return crate::locale::text(zh, en).to_string();
    }
    if view.session.status == AgentSessionStatus::Archived {
        return crate::locale::text("已归档", "Archived").to_string();
    }
    if has_running_agent_terminal(view) {
        return crate::locale::text("运行中", "Running").to_string();
    }
    if let Some(terminal) = view.terminal.as_ref() {
        let label = match terminal_exit_projection(view) {
            Some(TerminalExitProjection::Normal) => Some(("已正常退出", "Exited normally")),
            Some(TerminalExitProjection::Failure) => Some(("异常退出", "Exited with an error")),
            Some(TerminalExitProjection::Unknown) => {
                Some(("退出结果待确认", "Exit result unconfirmed"))
            }
            Some(TerminalExitProjection::Closed) => Some(("已关闭", "Closed")),
            Some(TerminalExitProjection::Failed) => Some(("运行失败", "Failed")),
            None => match terminal.lifecycle {
                SessionLifecycle::Starting => Some(("启动中", "Starting")),
                SessionLifecycle::Closing => Some(("正在关闭", "Closing")),
                SessionLifecycle::Closed => Some(("已关闭", "Closed")),
                SessionLifecycle::Failed => Some(("运行失败", "Failed")),
                SessionLifecycle::Running | SessionLifecycle::Exited => None,
            },
        };
        if let Some((zh, en)) = label {
            return crate::locale::text(zh, en).to_string();
        }
    }
    let (zh, en) = match view.resume_capability {
        AgentResumeCapability::Supported => ("可继续", "Ready to resume"),
        AgentResumeCapability::Unsupported => ("已结束", "Ended"),
        AgentResumeCapability::MetadataUnavailable => ("恢复信息待检查", "Check recovery metadata"),
    };
    crate::locale::text(zh, en).to_string()
}

/// ExitStatus 成功位不能替代明确零码；缺码/矛盾回执保持待确认，真实非零或 signal 显示失败。
///
/// @author kongweiguang
fn terminal_exit_projection(view: &AgentSessionView) -> Option<TerminalExitProjection> {
    let terminal = view.terminal.as_ref()?;
    match terminal.lifecycle {
        SessionLifecycle::Exited => match terminal.exit_status.as_ref() {
            Some(status) if status.success && status.code == Some(0) && status.signal.is_none() => {
                Some(TerminalExitProjection::Normal)
            }
            Some(status)
                if !status.success
                    || status.code.is_some_and(|code| code != 0)
                    || status.signal.is_some() =>
            {
                Some(TerminalExitProjection::Failure)
            }
            _ => Some(TerminalExitProjection::Unknown),
        },
        SessionLifecycle::Closed => Some(TerminalExitProjection::Closed),
        SessionLifecycle::Failed => Some(TerminalExitProjection::Failed),
        SessionLifecycle::Starting | SessionLifecycle::Running | SessionLifecycle::Closing => None,
    }
}

/// 将 typed 退出分类映射为短状态；非零细节保留退出码或 signal，不隐藏真实失败。
///
/// @author kongweiguang
fn terminal_exit_message(view: &AgentSessionView) -> Option<(String, TerminalStatusTone)> {
    let projection = terminal_exit_projection(view)?;
    let terminal = view.terminal.as_ref()?;
    let message = match projection {
        TerminalExitProjection::Normal => {
            crate::locale::text("已正常退出", "Exited normally").to_string()
        }
        TerminalExitProjection::Failure | TerminalExitProjection::Failed => {
            let detail = terminal.exit_status.as_ref().and_then(|status| {
                status
                    .code
                    .map(|code| {
                        crate::locale::text(format!("退出码 {code}"), format!("exit code {code}"))
                            .to_string()
                    })
                    .or_else(|| {
                        status.signal.as_ref().map(|signal| {
                            crate::locale::text(
                                format!("信号 {signal}"),
                                format!("signal {signal}"),
                            )
                            .to_string()
                        })
                    })
            });
            detail.map_or_else(
                || crate::locale::text("终端运行失败", "Terminal failed").to_string(),
                |detail| {
                    crate::locale::text(
                        format!("终端异常退出（{detail}）"),
                        format!("Terminal exited unsuccessfully ({detail})"),
                    )
                    .to_string()
                },
            )
        }
        TerminalExitProjection::Unknown => {
            crate::locale::text("已结束，退出结果待确认", "Exited; result unconfirmed").to_string()
        }
        TerminalExitProjection::Closed => {
            crate::locale::text("终端已关闭", "Terminal closed").to_string()
        }
    };
    let tone = match projection {
        TerminalExitProjection::Normal | TerminalExitProjection::Closed => {
            TerminalStatusTone::Neutral
        }
        TerminalExitProjection::Unknown => TerminalStatusTone::Warning,
        TerminalExitProjection::Failure | TerminalExitProjection::Failed => {
            TerminalStatusTone::Error
        }
    };
    Some((message, tone))
}

/// 真实 runtime 错误优先于派生退出提示；只有无 runtime 反馈时才呈现生命周期摘要。
///
/// @author kongweiguang
fn terminal_header_status(
    selected_view: Option<&AgentSessionView>,
    feedback: Option<&AgentTerminalFeedback>,
) -> Option<TerminalHeaderStatus> {
    if let Some(feedback) = feedback {
        return Some(TerminalHeaderStatus {
            message: feedback.message.clone(),
            tone: TerminalStatusTone::Error,
        });
    }
    let (message, tone) = terminal_exit_message(selected_view?)?;
    Some(TerminalHeaderStatus { message, tone })
}

/// 正常运行由终端画布表达，不再占据额外状态行；退出、故障及待恢复状态仍保留明确提示。
///
/// @author kongweiguang
fn terminal_header_status_row(
    selected_view: Option<&AgentSessionView>,
    terminal_attached: bool,
    feedback: Option<&AgentTerminalFeedback>,
    continue_visible: bool,
) -> Option<TerminalHeaderStatus> {
    let status = terminal_header_status(
        if terminal_attached {
            selected_view
        } else {
            None
        },
        feedback,
    );
    status.or_else(|| {
        if terminal_attached || continue_visible {
            selected_view
                .filter(|view| !has_running_agent_terminal(view))
                .map(|view| TerminalHeaderStatus {
                    message: lifecycle_label(view),
                    tone: TerminalStatusTone::Neutral,
                })
        } else {
            None
        }
    })
}

/// Continue 只在现有恢复门禁开放时出现；Pending/Unknown 仍只能核对原事务。
///
/// @author kongweiguang
fn should_render_continue(view: Option<&AgentSessionView>) -> bool {
    view.is_some_and(agent_can_resume)
}

/// 只把实际处于 Running 的 CLI 当作活动终端，Exited 的历史句柄不能占据运行布局。
/// @author kongweiguang
pub(super) fn has_running_agent_terminal(view: &AgentSessionView) -> bool {
    view.session.status == AgentSessionStatus::Active
        && view.resume_recovery.is_none()
        && view
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
}

/// 允许复用已退出的终端记录继续会话，但阻止 Starting、Running 或 Closing 时重复启动 CLI。
/// @author kongweiguang
fn terminal_can_be_resumed(terminal: Option<&kerminal_domain::sessions::SessionInfo>) -> bool {
    terminal.is_none_or(|terminal| {
        matches!(
            terminal.lifecycle,
            SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed
        )
    })
}

/// 只有显式支持恢复且旧 PTY 已退出、无未决核对时才开放继续；不猜 Custom provider 能力。
///
/// @author kongweiguang
pub(super) fn agent_can_resume(view: &AgentSessionView) -> bool {
    view.resume_capability == AgentResumeCapability::Supported
        && view.session.status != AgentSessionStatus::Archived
        && view.resume_recovery.is_none()
        && terminal_can_be_resumed(view.terminal.as_ref())
}

/// 核对只针对可见的原恢复事务，与 terminal 是否仍运行无关，便于处理停止后的 Unknown。
///
/// @author kongweiguang
pub(super) fn agent_can_reconcile(view: &AgentSessionView) -> bool {
    view.session.status != AgentSessionStatus::Archived && view.resume_recovery.is_some()
}
/// 归档会改变 Agent 生命周期并与未完成恢复事务冲突，因此只允许归档已核对记录。
///
/// @author kongweiguang
pub(super) fn agent_can_archive(view: &AgentSessionView) -> bool {
    view.session.status != AgentSessionStatus::Archived && view.resume_recovery.is_none()
}

/// 下拉框只缩短视觉标签、不修改持久 title，避免历史记录占满右栏宽度。
/// @author kongweiguang
fn compact_title(title: &str) -> String {
    const MAX_CHARS: usize = 18;
    let mut chars = title.chars();
    let compact = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{compact}…")
    } else if compact.is_empty() {
        crate::locale::text("未命名会话", "Untitled chat").to_string()
    } else {
        compact
    }
}

/// Custom 未命名会话用保存的启动器标签替代通用 provider 标题，同时保留用户填写的会话标题。
///
/// @author kongweiguang
fn session_picker_title(view: &AgentSessionView) -> &str {
    let title = view.session.title.as_str();
    let launcher_label = view.session.launch.command_label.trim();
    if view.session.agent_id == AgentProvider::Custom
        && title == "Custom Agent"
        && !launcher_label.is_empty()
    {
        launcher_label
    } else {
        title
    }
}

/// 状态已单列时保留完整可读的会话名，避免将标题和退出回执竞争同一窄行。
///
/// @author kongweiguang
fn session_picker_label(view: &AgentSessionView, status_is_separate: bool) -> String {
    let title = compact_title(session_picker_title(view));
    if status_is_separate {
        title
    } else {
        format!("{title} · {}", lifecycle_label(view))
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/agent_recovery_projection.rs"]
mod recovery_projection_tests;

#[cfg(test)]
#[path = "../../../tests/unit/agent_pending_send.rs"]
mod pending_send_tests;

#[cfg(test)]
#[path = "../../../tests/unit/agent_landing_visual.rs"]
mod agent_landing_visual_tests;
