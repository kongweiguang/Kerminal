//! Agent 自定义启动器逐项编辑；Root 持有弹层并负责唯一 Preferences CAS。
//!
//! @author kongweiguang

use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    IntoElement, Render, Subscription, Window,
    assets::IconName,
    component::{
        Disableable, Icon, WindowExt as _,
        button::{Button, ButtonVariant, ButtonVariants},
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_application::preferences::{AgentLauncherSettings, CustomAgentDefinition};
use kerminal_domain::agents::{AgentLauncherKey, AgentProvider};

use crate::theme::ThemeTokens;

/// 管理器只向 Root 发出写入、读回和关闭请求，不把请求受理当作持久化成功。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CustomAgentManagerEvent {
    Save {
        settings: AgentLauncherSettings,
        select_key: AgentLauncherKey,
    },
    SaveKeepOpen {
        settings: AgentLauncherSettings,
        select_key: AgentLauncherKey,
    },
    ReadBack,
    OpenPreferences,
    Close,
}

/// Root 将唯一 Preferences CAS 或配置读回的最终结果交还给管理器。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CustomAgentManagerWriteOutcome {
    Confirmed(AgentLauncherSettings),
    ConfirmedKeepOpen(AgentLauncherSettings),
    Failed(String),
    Conflict(String),
    OutcomeUnknown(String),
    ReadBackFailed(String),
    ReadBack {
        settings: AgentLauncherSettings,
        retry_allowed: bool,
    },
}

/// 保存结果与一次自动读回共同控制重试；读回本身失败时转到 Preferences 人工核对。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManagerRecoveryFlags {
    readback_required: bool,
    retry_ready: bool,
    preferences_review_required: bool,
}

const RECOVERY_IDLE: ManagerRecoveryFlags = ManagerRecoveryFlags {
    readback_required: false,
    retry_ready: false,
    preferences_review_required: false,
};
const RECOVERY_READBACK: ManagerRecoveryFlags = ManagerRecoveryFlags {
    readback_required: true,
    retry_ready: false,
    preferences_review_required: false,
};
const RECOVERY_RETRY: ManagerRecoveryFlags = ManagerRecoveryFlags {
    readback_required: false,
    retry_ready: true,
    preferences_review_required: false,
};
const RECOVERY_PREFERENCES_REVIEW: ManagerRecoveryFlags = ManagerRecoveryFlags {
    readback_required: false,
    retry_ready: false,
    preferences_review_required: true,
};

/// 同步门禁拒绝、异步失败和冲突触发自动读回，读回失败则要求从 Preferences 再核对。
///
/// @author kongweiguang
fn recovery_flags_for_outcome(outcome: &CustomAgentManagerWriteOutcome) -> ManagerRecoveryFlags {
    match outcome {
        CustomAgentManagerWriteOutcome::Failed(_)
        | CustomAgentManagerWriteOutcome::Conflict(_)
        | CustomAgentManagerWriteOutcome::OutcomeUnknown(_) => RECOVERY_READBACK,
        CustomAgentManagerWriteOutcome::ReadBackFailed(_) => RECOVERY_PREFERENCES_REVIEW,
        CustomAgentManagerWriteOutcome::ReadBack {
            retry_allowed: true,
            ..
        } => RECOVERY_RETRY,
        CustomAgentManagerWriteOutcome::ReadBack {
            retry_allowed: false,
            ..
        } => RECOVERY_PREFERENCES_REVIEW,
        CustomAgentManagerWriteOutcome::Confirmed(_)
        | CustomAgentManagerWriteOutcome::ConfirmedKeepOpen(_) => RECOVERY_IDLE,
    }
}

/// 保存回执保留状态由 Root 显式选择；删除事务确认后必须仍留在管理器。
///
/// @author kongweiguang
const fn manager_closes_after_confirmation(keep_open: bool) -> bool {
    !keep_open
}

/// 普通失败与同步拒绝共用事件通道，因此明确说明要读回且核对前不能重提。
///
/// @author kongweiguang
fn requires_readback_status(message: String) -> String {
    crate::locale::text(
        format!("未确认本次设置变更：{message}。请先读回当前设置；核对前不会重新提交。"),
        format!("The settings change is not confirmed: {message}. Read back the current settings before resubmitting."),
    )
    .to_string()
}

/// 管理器只保留已确认基线和当前表单，Root 是写入所有者并接收逐项构造的完整设置快照。
pub(crate) struct CustomAgentLauncherManager {
    initial: AgentLauncherSettings,
    selected_id: Option<String>,
    editing_id: Option<String>,
    name: Entity<InputState>,
    command: Entity<TextareaState>,
    modal_focus: FocusHandle,
    _input_subscriptions: [Subscription; 2],
    pending_submission: Option<AgentLauncherSettings>,
    pending_keep_open: bool,
    pending_deleted_id: Option<String>,
    reset_editor_after_confirmation: bool,
    busy: bool,
    readback_required: bool,
    retry_ready: bool,
    preferences_review_required: bool,
    status: Option<String>,
    validation_error: Option<String>,
}

impl EventEmitter<CustomAgentManagerEvent> for CustomAgentLauncherManager {}

impl CustomAgentLauncherManager {
    /// 由 Root 注入已确认的 launcher 子快照，避免管理器自行读取或写入配置。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        settings: AgentLauncherSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let modal_focus = cx.focus_handle().tab_stop(false);
        let name = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "例如：Pi Agent",
                "For example: Pi Agent",
            ))
        });
        let command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "例如：pi --model provider/model",
                    "For example: pi --model provider/model",
                ))
                .auto_grow(3, 6)
        });
        let name_subscription =
            cx.subscribe(&name, |manager, _, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    manager.validation_error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => manager.request_save(cx),
                InputEvent::Focus | InputEvent::Blur => {}
            });
        let command_subscription = cx.subscribe(&command, |manager, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                manager.validation_error = None;
                cx.notify();
            }
        });
        Self {
            initial: settings,
            selected_id: None,
            editing_id: None,
            name,
            command,
            modal_focus,
            _input_subscriptions: [name_subscription, command_subscription],
            pending_submission: None,
            pending_keep_open: false,
            pending_deleted_id: None,
            reset_editor_after_confirmation: false,
            busy: false,
            readback_required: false,
            retry_ready: false,
            preferences_review_required: false,
            status: None,
            validation_error: None,
        }
    }

    /// 只有 Root 确认保存回执，或读回值与当前逐项提交一致时才完成事务；删除回执保持弹框打开。
    ///
    /// @author kongweiguang
    pub(crate) fn apply_write_outcome(
        &mut self,
        outcome: CustomAgentManagerWriteOutcome,
        cx: &mut Context<Self>,
    ) -> bool {
        self.busy = false;
        let receipt_keeps_open = matches!(
            &outcome,
            CustomAgentManagerWriteOutcome::ConfirmedKeepOpen(_)
        );
        self.pending_keep_open |= receipt_keeps_open;
        let recovery_flags = recovery_flags_for_outcome(&outcome);
        let mut start_readback = matches!(
            &outcome,
            CustomAgentManagerWriteOutcome::Failed(_)
                | CustomAgentManagerWriteOutcome::Conflict(_)
                | CustomAgentManagerWriteOutcome::OutcomeUnknown(_)
        );
        match outcome {
            CustomAgentManagerWriteOutcome::Confirmed(settings)
            | CustomAgentManagerWriteOutcome::ConfirmedKeepOpen(settings) => {
                let keep_open = self.pending_keep_open || receipt_keeps_open;
                if self.pending_submission.as_ref() == Some(&settings)
                    || (self.pending_deleted_id.as_ref().is_some_and(|id| {
                        !settings
                            .custom_agents
                            .iter()
                            .any(|definition| definition.id.as_str() == id.as_str())
                    }) && keep_open)
                {
                    return self.apply_confirmed_settings(settings, keep_open, cx);
                }
                self.status = Some(crate::locale::text(
                    "保存回执与本次修改不一致，正在读回核对。",
                    "The save receipt differs from this change. Reading settings back to reconcile it.",
                ).to_string());
                self.apply_recovery_flags(RECOVERY_READBACK);
                start_readback = true;
            }
            CustomAgentManagerWriteOutcome::Failed(message) => {
                // Failed 同时承接同步门禁拒绝与异步失败；保留提交值，直到真实读回判定是否已生效。
                self.apply_recovery_flags(recovery_flags);
                self.status = Some(requires_readback_status(message));
            }
            CustomAgentManagerWriteOutcome::Conflict(message) => {
                self.apply_recovery_flags(recovery_flags);
                self.status = Some(message);
            }
            CustomAgentManagerWriteOutcome::OutcomeUnknown(message) => {
                self.apply_recovery_flags(recovery_flags);
                self.status = Some(message);
            }
            CustomAgentManagerWriteOutcome::ReadBackFailed(message) => {
                self.apply_recovery_flags(recovery_flags);
                self.status = Some(message);
            }
            CustomAgentManagerWriteOutcome::ReadBack {
                settings,
                retry_allowed,
            } => {
                let keep_open = self.pending_keep_open;
                let was_delete = self.pending_deleted_id.is_some();
                let deletion_is_applied = self.pending_deleted_id.as_ref().is_some_and(|id| {
                    !settings
                        .custom_agents
                        .iter()
                        .any(|definition| definition.id.as_str() == id.as_str())
                });
                if self.pending_submission.as_ref() == Some(&settings)
                    || (keep_open && deletion_is_applied)
                {
                    return self.apply_confirmed_settings(settings, keep_open, cx);
                }
                self.pending_submission = None;
                self.pending_keep_open = false;
                self.pending_deleted_id = None;
                self.initial = settings.clone();
                if self.editing_id.as_ref().is_some_and(|id| {
                    !settings
                        .custom_agents
                        .iter()
                        .any(|definition| definition.id.as_str() == id.as_str())
                }) {
                    self.editing_id = None;
                    self.selected_id = None;
                }
                self.apply_recovery_flags(if was_delete && retry_allowed {
                    RECOVERY_IDLE
                } else {
                    recovery_flags
                });
                self.status = Some(if was_delete && retry_allowed {
                    crate::locale::text(
                        "删除尚未完成；当前设置已读回，可重新确认删除。",
                        "Deletion was not completed. Settings were read back; confirm deletion again to retry.",
                    ).to_string()
                } else if retry_allowed {
                    crate::locale::text(
                        "设置已更新，当前输入仍保留；确认后可按最新设置重新提交。",
                        "Settings changed. The current form is preserved and can be resubmitted against the latest settings revision.",
                    ).to_string()
                } else {
                    crate::locale::text(
                        "设置已更新，当前输入仍保留；请到设置中完成核对后再提交。",
                        "Settings changed. The current form is preserved; review Settings before submitting again.",
                    ).to_string()
                });
            }
        }
        if start_readback {
            self.request_readback(cx);
            return false;
        }
        cx.notify();
        false
    }

    /// Root 回执或逐字段读回一致后才更新基线；删除当前编辑项时将空表单留在管理器中。
    ///
    /// @author kongweiguang
    fn apply_confirmed_settings(
        &mut self,
        settings: AgentLauncherSettings,
        keep_open: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let reset_current_editor = self
            .pending_deleted_id
            .take()
            .is_some_and(|id| self.editing_id.as_deref() == Some(id.as_str()));
        self.pending_submission = None;
        self.pending_keep_open = false;
        self.initial = settings;
        if reset_current_editor {
            self.reset_editor_after_confirmation = true;
        }
        if self.selected_id.as_ref().is_some_and(|id| {
            !self
                .initial
                .custom_agents
                .iter()
                .any(|definition| definition.id.as_str() == id.as_str())
        }) {
            self.selected_id = None;
        }
        self.apply_recovery_flags(RECOVERY_IDLE);
        self.status = None;
        cx.notify();
        manager_closes_after_confirmation(keep_open)
    }

    /// 集中更新重试门禁，确保 UI 文案、读回入口和实际提交按钮使用同一状态。
    ///
    /// @author kongweiguang
    fn apply_recovery_flags(&mut self, flags: ManagerRecoveryFlags) {
        self.readback_required = flags.readback_required;
        self.retry_ready = flags.retry_ready;
        self.preferences_review_required = flags.preferences_review_required;
    }

    /// Root 只用名字输入框作为初始焦点；弹层边界必须使用独立容器句柄，不能缩成该字段的后代范围。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.read(cx).focus_handle(cx)
    }

    /// 给 Root 的标准 GPUI focus trap 提供独立且不可通过 Tab 进入的容器祖先句柄。
    ///
    /// @author kongweiguang
    pub(crate) fn modal_focus_handle(&self) -> FocusHandle {
        self.modal_focus.clone()
    }

    /// Root 的 Escape、遮罩或系统关闭意图复用同一忙态门禁，未提交的本地表单随关闭丢弃。
    ///
    /// @author kongweiguang
    pub(crate) fn request_close_from_root(&mut self, cx: &mut Context<Self>) {
        self.request_close(cx);
    }

    /// Root 在处理 Escape 或导航动作前读取忙态；写入和读回期间不能丢弃待核对快照。
    ///
    /// @author kongweiguang
    pub(crate) fn is_busy(&self) -> bool {
        self.busy
    }

    /// Root maps the confirmed Preferences receipt to keep-open or close completion semantics.
    ///
    /// @author kongweiguang
    pub(crate) fn pending_write_keeps_open(&self) -> bool {
        self.pending_keep_open
    }

    /// 本地格式、长度、唯一性与数量限制在 Root CAS 前校验，应用层仍会再次校验最终设置。
    ///
    /// @author kongweiguang
    fn validate_editor(&self, cx: &Context<Self>) -> Result<(String, String), String> {
        let name = self.name.read(cx).value().to_string();
        let command = self.command.read(cx).value().to_string();
        validate_custom_agent_fields(&self.initial, self.editing_id.as_deref(), &name, &command)?;
        Ok((name.trim().to_owned(), command.trim().to_owned()))
    }

    /// 新增入口立即清空当前表单；定义只有在保存回执确认后才进入已保存列表。
    ///
    /// @author kongweiguang
    fn begin_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy
            || self.readback_required
            || self.preferences_review_required
            || (self.pending_submission.is_some() && !self.retry_ready)
        {
            return;
        }
        if self.initial.custom_agents.len() >= 32 && self.editing_id.is_none() {
            self.status = Some(
                crate::locale::text(
                    "自定义 Agent 最多保存 32 个。",
                    "You can save up to 32 custom Agents.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.selected_id = None;
        self.editing_id = None;
        self.validation_error = None;
        self.status = None;
        set_manager_inputs(
            &self.name,
            &self.command,
            String::new(),
            String::new(),
            window,
            cx,
        );
        let focus_handle = self.name.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// 取消编辑清空本次未提交输入并切换为新增状态，已确认的定义不受影响。
    ///
    /// @author kongweiguang
    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.readback_required {
            return;
        }
        self.selected_id = None;
        self.editing_id = None;
        self.validation_error = None;
        self.status = None;
        set_manager_inputs(
            &self.name,
            &self.command,
            String::new(),
            String::new(),
            window,
            cx,
        );
        let focus_handle = self.name.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// 编辑列表项时按稳定 UUID 回填两个字段，不从当前 launcher 选择推断或重建定义。
    ///
    /// @author kongweiguang
    fn edit_agent(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.readback_required || self.preferences_review_required {
            return;
        }
        let Some(definition) = self
            .initial
            .custom_agents
            .iter()
            .find(|definition| definition.id.as_str() == id.as_str())
        else {
            self.status = Some(
                crate::locale::text(
                    "此自定义 Agent 已变化，请刷新设置后重试。",
                    "This custom Agent changed. Reload Settings before trying again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let name = definition.name.clone();
        let command = definition.command.clone();
        self.selected_id = Some(id.clone());
        self.editing_id = Some(id);
        self.validation_error = None;
        self.status = None;
        set_manager_inputs(&self.name, &self.command, name, command, window, cx);
        let focus_handle = self.name.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// 删除事务只从已确认基线构造快照，避免把编辑器中未提交的字段混入删除。
    ///
    /// @author kongweiguang
    fn delete_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.busy || self.readback_required || self.preferences_review_required {
            return;
        }
        let mut settings = self.initial.clone();
        if !remove_custom_agent(&mut settings, id) {
            self.status = Some(
                crate::locale::text(
                    "此自定义 Agent 已变化，请刷新设置后重试。",
                    "This custom Agent changed. Reload Settings before trying again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(select_key) = launcher_key(&settings.selected_agent_key) else {
            self.status = Some(
                crate::locale::text(
                    "当前选择无效，请重新选择自定义 Agent。",
                    "The selected launcher is invalid. Choose a custom Agent again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.pending_keep_open = true;
        self.pending_deleted_id = Some(id.to_owned());
        self.submit_settings(settings, select_key, cx);
    }

    /// 一次只提交当前编辑项；成功关闭，失败则保留输入并由后台读回判断是否可重试。
    ///
    /// @author kongweiguang
    fn request_save(&mut self, cx: &mut Context<Self>) {
        if self.busy
            || self.readback_required
            || self.preferences_review_required
            || (self.pending_submission.is_some() && !self.retry_ready)
        {
            return;
        }
        let (name, command) = match self.validate_editor(cx) {
            Ok(values) => values,
            Err(error) => {
                self.validation_error = Some(error);
                cx.notify();
                return;
            }
        };
        let mut settings = self.initial.clone();
        if let Err(error) =
            upsert_custom_agent(&mut settings, self.editing_id.as_deref(), name, command)
        {
            self.validation_error = Some(error);
            cx.notify();
            return;
        }
        self.validation_error = None;
        let Some(select_key) = launcher_key(&settings.selected_agent_key) else {
            self.validation_error = Some(
                crate::locale::text(
                    "当前选择无效，请重新选择自定义 Agent。",
                    "The selected launcher is invalid. Choose a custom Agent again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        if settings == self.initial {
            self.request_close(cx);
            return;
        }
        self.pending_keep_open = false;
        self.pending_deleted_id = None;
        self.submit_settings(settings, select_key, cx);
    }

    /// 提交已验证的单项设置快照；输入保持挂载，供失败后的自动读回或安全重试使用。
    ///
    /// @author kongweiguang
    fn submit_settings(
        &mut self,
        settings: AgentLauncherSettings,
        select_key: AgentLauncherKey,
        cx: &mut Context<Self>,
    ) {
        self.pending_submission = Some(settings.clone());
        self.busy = true;
        self.readback_required = false;
        self.retry_ready = false;
        self.preferences_review_required = false;
        self.status = None;
        let event = if self.pending_keep_open {
            CustomAgentManagerEvent::SaveKeepOpen {
                settings,
                select_key,
            }
        } else {
            CustomAgentManagerEvent::Save {
                settings,
                select_key,
            }
        };
        cx.emit(event);
        cx.notify();
    }

    /// 未确认的写入或偏好状态由 Root 读回；自动读回失败后只允许经 Preferences 再核对。
    ///
    /// @author kongweiguang
    fn request_readback(&mut self, cx: &mut Context<Self>) {
        if self.busy || !(self.readback_required || self.preferences_review_required) {
            return;
        }
        self.busy = true;
        self.status = Some(
            crate::locale::text(
                "正在读回设置并核对本次提交…",
                "Reading settings back to reconcile this save…",
            )
            .to_string(),
        );
        cx.emit(CustomAgentManagerEvent::ReadBack);
        cx.notify();
    }

    /// 从 Preferences 返回时沿用同一事件路径读取设置，避免以表单内容冒充持久化回执。
    ///
    /// @author kongweiguang
    pub(crate) fn request_readback_from_root(&mut self, cx: &mut Context<Self>) {
        self.request_readback(cx);
    }

    /// 关闭不触发隐藏写入或额外确认；忙态期间保留弹框以免丢失正在核对的事务。
    ///
    /// @author kongweiguang
    fn request_close(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        cx.emit(CustomAgentManagerEvent::Close);
    }

    /// 头部按 main 的 20/16px 留白与 17/24、13/20 文字度量排版；异步写入时仍锁定唯一关闭入口。
    ///
    /// @author kongweiguang
    fn render_header(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .px_5()
            .py_4()
            .flex()
            .items_start()
            .justify_between()
            .gap_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(17.))
                            .line_height(px(24.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(crate::locale::text(
                                "管理自定义 Agent",
                                "Manage custom Agents",
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(20.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "保存常用的 Agent CLI 启动命令；下次打开 Kerminal 可继续选择。",
                                "Save Agent CLI launch commands for the next time you open Kerminal.",
                            )),
                    ),
            )
            .child(
                Button::new("custom-agent-manager-close")
                    .compact()
                    .ghost()
                    .w(px(36.))
                    .h(px(36.))
                    .icon(IconName::X)
                    .accessibility_label(crate::locale::text("关闭", "Close"))
                    .tooltip(crate::locale::text("关闭", "Close"))
                    .disabled(self.busy)
                    .on_click(
                        cx.listener(|manager, _: &ClickEvent, _, cx| manager.request_close(cx)),
                    ),
            )
    }

    /// 侧栏复用 main 的 36% 列宽、16/12px 标题内边距和 8px 列表内缩；名称与命令分级截断，删除仍由确认框保护。
    ///
    /// @author kongweiguang
    fn render_saved_agents(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
        narrow: bool,
    ) -> impl IntoElement {
        let add_disabled = self.busy
            || self.readback_required
            || self.preferences_review_required
            || (self.initial.custom_agents.len() >= 32 && self.editing_id.is_none());
        let manager_entity = cx.entity().clone();
        let mut list = div()
            .id("custom-agent-manager-list")
            .w_full()
            .flex()
            .flex_col()
            .gap_1();
        for definition in &self.initial.custom_agents {
            let id = definition.id.clone();
            let is_selected = self.selected_id.as_deref() == Some(id.as_str());
            let name = definition.name.clone();
            let command_name = custom_agent_executable_name(&definition.command);
            let action_id = id.clone();
            let action_name = name.clone();
            let delete_id = id.clone();
            let delete_name = name.clone();
            let delete_entity = manager_entity.clone();
            let row = div()
                .id(format!("custom-agent-row-{id}"))
                .w_full()
                .px_2()
                .py(px(6.))
                .rounded_md()
                .border_1()
                .border_color(if is_selected {
                    theme.accent.opacity(0.38)
                } else {
                    theme.border.opacity(0.)
                })
                .bg(if is_selected {
                    theme.selected
                } else {
                    theme.panel.opacity(0.)
                })
                .flex()
                .items_center()
                .gap_2()
                .when(!is_selected, |row| row.hover(|style| style.bg(theme.hover)))
                .child(
                    Button::new(format!("custom-agent-edit-action-{id}"))
                        .text()
                        .flex_1()
                        .min_w(px(0.))
                        .h(px(40.))
                        .accessibility_label(crate::locale::text(
                            format!("编辑 {action_name}"),
                            format!("Edit {action_name}"),
                        ))
                        .disabled(
                            self.busy || self.readback_required || self.preferences_review_required,
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(28.))
                                        .h(px(28.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .flex_shrink_0()
                                        .rounded_lg()
                                        .bg(theme.panel)
                                        .text_color(theme.muted)
                                        .child(Icon::new(IconName::Terminal).size(px(14.))),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .items_start()
                                        .child(
                                            div()
                                                .w_full()
                                                .text_size(theme.text_xs)
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .text_color(theme.foreground)
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(name),
                                        )
                                        .child(
                                            div()
                                                .w_full()
                                                .font_family("monospace")
                                                .text_size(px(10.))
                                                .text_color(theme.muted)
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(command_name),
                                        ),
                                )
                                .child(
                                    Icon::new(IconName::Pencil)
                                        .size(px(14.))
                                        .text_color(theme.muted.opacity(0.7)),
                                ),
                        )
                        .on_click(cx.listener(move |manager, _: &ClickEvent, window, cx| {
                            manager.edit_agent(action_id.clone(), window, cx)
                        })),
                )
                .child(
                    Button::new(format!("custom-agent-delete-{id}"))
                        .compact()
                        .ghost()
                        .w(px(28.))
                        .h(px(28.))
                        .icon(Icon::new(IconName::Trash).size(px(14.)))
                        .accessibility_label(crate::locale::text(
                            format!("删除 {delete_name}"),
                            format!("Delete {delete_name}"),
                        ))
                        .tooltip(crate::locale::text("删除", "Delete"))
                        .disabled(
                            self.busy || self.readback_required || self.preferences_review_required,
                        )
                        .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                            let confirm_entity = delete_entity.clone();
                            let confirm_id = delete_id.clone();
                            let confirm_name = delete_name.clone();
                                            let theme = ThemeTokens::current(window, cx).for_tool_panel();
                            window.open_alert_dialog(cx, move |alert, _, _| {
                                let action_entity = confirm_entity.clone();
                                let action_id = confirm_id.clone();
                                alert
                                    .backdrop_blur(theme.modal_blur_radius)
                                    .surface_backdrop_filter(theme.floating_blur_radius, theme.floating_saturation)
                                    .bg(theme.floating_surface)
                                    .rounded(theme.radius_dialog)
                                    .title(crate::locale::text(
                                        "删除自定义 Agent？",
                                        "Delete custom Agent?",
                                    ))
                                    .description(crate::locale::text(
                                        "只会移除下拉框中的定义，已经创建的历史会话仍会保留。",
                                        "Only the definition in the launcher menu will be removed. Existing session history will remain.",
                                    ))
                                    .child(
                                        div()
                                            .w_full()
                                            .p_3()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.secondary)
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .w(px(32.))
                                                    .h(px(32.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .flex_shrink_0()
                                                    .rounded_md()
                                                    .bg(theme.panel)
                                                    .text_color(theme.muted)
                                                    .child(IconName::Wrench),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .text_size(theme.text_sm)
                                                    .text_color(theme.foreground)
                                                    .text_ellipsis()
                                                    .child(confirm_name.clone()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(theme.text_xs)
                                            .text_color(theme.muted)
                                            .child(crate::locale::text(
                                                "删除后无法从下拉框继续选择该定义；历史会话不会被删除。",
                                                "After deletion, this definition cannot be selected from the launcher menu; existing sessions will remain.",
                                            )),
                                    )
                                    .confirm()
                                    .cancel_text(crate::locale::text("取消", "Cancel"))
                                    .ok_text(crate::locale::text("删除定义", "Delete definition"))
                                    .ok_variant(ButtonVariant::Danger)
                                    .on_ok(move |_, _, app| {
                                        action_entity.update(app, |manager, cx| {
                                            manager.delete_agent(&action_id, cx);
                                        });
                                        true
                                    })
                            });
                        })),
                );
            list = list.child(row);
        }
        if self.initial.custom_agents.is_empty() {
            list = list.child(
                div()
                    .min_h(px(96.))
                    .mx_2()
                    .px_4()
                    .rounded_lg()
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme.muted)
                    .child(
                        Icon::new(IconName::Wrench)
                            .size(px(20.))
                            .text_color(theme.muted),
                    )
                    .child(crate::locale::text(
                        "还没有自定义 Agent",
                        "No custom Agents yet",
                    )),
            );
        }
        let mut sidebar = div()
            .id("custom-agent-manager-sidebar")
            .w_full()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .bg(theme.secondary)
            .child(
                div()
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(crate::locale::text("已保存", "Saved")),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.muted)
                                    .child(format!("{} / 32", self.initial.custom_agents.len())),
                            ),
                    )
                    .child(
                        Button::new("custom-agent-manager-add")
                            .compact()
                            .ghost()
                            .h(px(32.))
                            .px_3()
                            .icon(Icon::new(IconName::Plus).size(px(14.)))
                            .label(crate::locale::text("新增", "Add"))
                            .disabled(add_disabled)
                            .on_click(cx.listener(move |manager, _: &ClickEvent, window, cx| {
                                manager.begin_new_agent(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("custom-agent-manager-scroll")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .px_2()
                    .pb_3()
                    .overflow_y_scroll()
                    .child(list),
            );
        if narrow {
            sidebar = sidebar
                .h(relative(0.4))
                .flex_shrink_0()
                .border_b_1()
                .border_color(theme.border);
        } else {
            sidebar = sidebar
                .w(relative(0.36))
                .min_w(px(180.))
                .flex_shrink_0()
                .border_r_1()
                .border_color(theme.border);
        }
        sidebar
    }

    /// 表单按 main 的 16px 外边距、14px 字段节奏和 96px 命令框排版；自适应宽度仍由右侧列负责。
    /// 单行名称使用 Styled 高度，避免 Input 的多行专用 h 忽略 main 的 36px 规格。
    ///
    /// @author kongweiguang
    fn render_editor(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let name_count = self.name.read(cx).value().chars().count();
        let command_count = self.command.read(cx).value().chars().count();
        let editing_name = self
            .editing_id
            .as_deref()
            .and_then(|id| {
                self.initial
                    .custom_agents
                    .iter()
                    .find(|agent| agent.id == id)
            })
            .map(|agent| agent.name.clone());
        let save_label = if self.busy {
            crate::locale::text("正在保存", "Saving")
        } else if self.retry_ready {
            crate::locale::text("重新保存", "Save again")
        } else if self.editing_id.is_some() {
            crate::locale::text("保存修改", "Save changes")
        } else {
            crate::locale::text("保存并选择", "Save and select")
        };
        let editor_feedback = self.validation_error.clone();
        let save_disabled = manager_save_disabled(
            self.validate_editor(cx).is_ok(),
            self.busy,
            self.readback_required,
            self.preferences_review_required,
        );
        div()
            .id("custom-agent-manager-editor")
            .w_full()
            .flex_1()
            .min_w(px(260.))
            .min_h(px(0.))
            .p_4()
            .flex()
            .flex_col()
            .gap(px(14.))
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.text_body)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(if let Some(name) = editing_name {
                                crate::locale::text(format!("编辑 {name}"), format!("Edit {name}"))
                            } else {
                                crate::locale::text("添加 Agent", "Add Agent")
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "名称用于下拉展示，命令会在独立 Agent 会话目录中运行。",
                                "The name appears in the launcher menu; the command runs in its own Agent session directory.",
                            )),
                    )
                    .when_some(editor_feedback, |element, message| {
                        element.child(
                            div()
                                .id("custom-agent-manager-validation-feedback")
                                .text_size(theme.text_xs)
                                .text_color(theme.error)
                                .child(message),
                        )
                    }),
            )
            .child(
                div()
                    .w_full()
                    .mt(px(2.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(manager_field_label("名称", "Name", theme))
                    .child(
                        gpui_kit::Styled::h(Input::new(&self.name).w_full(), px(36.))
                            .text_size(px(16.))
                            .line_height(px(24.))
                            .bg(theme.panel)
                            .border_color(theme.border)
                            .rounded_md()
                            .disabled(self.busy || self.readback_required),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_end()
                            .text_size(px(10.))
                            .text_color(if name_count > 64 { theme.error } else { theme.muted })
                            .child(format!("{name_count} / 64")),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(manager_field_label("启动命令", "Launch command", theme))
                    .child(
                        Textarea::new(&self.command)
                            .w_full()
                            .h(px(96.))
                            .font_family("monospace")
                            .text_size(theme.text_xs)
                            .line_height(px(20.))
                            .bg(theme.panel)
                            .border_color(theme.border)
                            .rounded_md()
                            .disabled(self.busy || self.readback_required),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_end()
                            .text_size(px(10.))
                            .text_color(if command_count > 4096 { theme.error } else { theme.muted })
                            .child(format!("{command_count} / 4096")),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py(px(10.))
                    .rounded_md()
                    .border_1()
                    .border_color(theme.warning.opacity(0.35))
                    .bg(theme.warning.opacity(0.08))
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .mt(px(2.))
                            .flex_shrink_0()
                            .text_color(theme.warning)
                            .child(IconName::TriangleAlert),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(theme.warning)
                            .child(crate::locale::text(
                                "启动命令会以明文保存在 settings.toml。请勿填写 API Key、密码或 token。",
                                "Launch commands are stored in plain text in settings.toml. Do not include API keys, passwords, or tokens.",
                            )),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .mt(px(2.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .when(self.editing_id.is_some(), |row| {
                        row.child(
                            Button::new("custom-agent-cancel-edit")
                                .compact()
                                .ghost()
                                .h(px(32.))
                                .px_3()
                                .label(crate::locale::text("取消编辑", "Cancel edit"))
                                .disabled(self.busy || self.readback_required)
                                .on_click(cx.listener(
                                    |manager, _: &ClickEvent, window, cx| {
                                        manager.cancel_editor(window, cx)
                                    },
                                )),
                        )
                    })
                    .child(
                        Button::new("custom-agent-manager-save-form")
                            .compact()
                            .primary()
                            .h(px(32.))
                            .px_3()
                            .icon(Icon::new(IconName::Save).size(px(14.)))
                            .label(save_label)
                            .disabled(save_disabled)
                            .on_click(cx.listener(|manager, _: &ClickEvent, _, cx| {
                                manager.request_save(cx)
                            })),
                    ),
            )
    }

    /// Root 弹层统一拥有面板背景、边框和圆角；管理器只布局内容并裁剪滚动区，避免重复表面露角。
    ///
    /// @author kongweiguang
    fn render_content(
        &mut self,
        theme: ThemeTokens,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let show_footer =
            self.status.is_some() || self.readback_required || self.preferences_review_required;
        let mut footer = div()
            .id("custom-agent-manager-footer")
            .w_full()
            .px_3()
            .py_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .border_t_1()
            .border_color(theme.border);
        if narrow {
            footer = footer.gap_1();
        }
        footer = footer.child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .when_some(self.status.clone(), |element, message| {
                    element.child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(if self.readback_required {
                                theme.warning
                            } else {
                                theme.muted
                            })
                            .child(message),
                    )
                }),
        );
        if !self.busy && self.preferences_review_required {
            footer = footer.child(
                Button::new("custom-agent-open-settings")
                    .compact()
                    .ghost()
                    .h(px(32.))
                    .label(crate::locale::text("打开设置", "Open Settings"))
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        cx.emit(CustomAgentManagerEvent::OpenPreferences);
                    })),
            );
        }
        let mut body = div()
            .id("custom-agent-manager-body")
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(self.render_saved_agents(theme, cx, narrow))
            .child(self.render_editor(theme, cx));
        if narrow {
            body = body.flex_col();
        }
        let mut content = div()
            .id("custom-agent-manager")
            .w_full()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_color(theme.foreground)
            .child(self.render_header(theme, cx))
            .child(body);
        if show_footer {
            content = content.child(footer);
        }
        content
    }
}

impl Render for CustomAgentLauncherManager {
    /// 主题令牌跟随系统并投影右栏局部色阶；窄视口将列表放上方，编辑器只在确认后重置。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.reset_editor_after_confirmation {
            self.reset_editor_after_confirmation = false;
            self.selected_id = None;
            self.editing_id = None;
            self.validation_error = None;
            set_manager_inputs(
                &self.name,
                &self.command,
                String::new(),
                String::new(),
                window,
                cx,
            );
            let focus_handle = self.name.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
        }
        let narrow = window.viewport_size().width.as_f32() < 768.;
        self.render_content(
            ThemeTokens::current(window, cx).for_tool_panel(),
            narrow,
            cx,
        )
    }
}

/// 名称与命令在领域验证前 trim，并按现有 application 约束检查字符数和唯一性。
///
/// @author kongweiguang
fn validate_custom_agent_fields(
    settings: &AgentLauncherSettings,
    editing_id: Option<&str>,
    name: &str,
    command: &str,
) -> Result<(), String> {
    let name = name.trim();
    let command = command.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(crate::locale::text(
            "Agent 名称不能为空或超过 64 个字符。",
            "Agent names must be non-empty and no longer than 64 characters.",
        )
        .to_string());
    }
    if command.is_empty() || command.chars().count() > 4096 {
        return Err(crate::locale::text(
            "启动命令不能为空或超过 4096 个字符。",
            "Launch commands must be non-empty and no longer than 4096 characters.",
        )
        .to_string());
    }
    let normalized_name = name.to_lowercase();
    if settings.custom_agents.iter().any(|definition| {
        Some(definition.id.as_str()) != editing_id
            && definition.name.to_lowercase() == normalized_name
    }) {
        return Err(crate::locale::text(
            "Agent 名称已存在，请使用另一个名称。",
            "This Agent name is already in use. Choose another name.",
        )
        .to_string());
    }
    if editing_id.is_none() && settings.custom_agents.len() >= 32 {
        return Err(crate::locale::text(
            "自定义 Agent 最多保存 32 个。",
            "You can save up to 32 custom Agents.",
        )
        .to_string());
    }
    Ok(())
}

/// 单项保存只在当前表单有效且没有进行中的写入或配置核对时启用。
///
/// @author kongweiguang
fn manager_save_disabled(
    editor_valid: bool,
    busy: bool,
    readback_required: bool,
    preferences_review_required: bool,
) -> bool {
    busy || readback_required || preferences_review_required || !editor_valid
}

/// 从本次待提交快照移除定义；只有被删项正是选择值时才回退 Codex。
///
/// @author kongweiguang
fn remove_custom_agent(settings: &mut AgentLauncherSettings, id: &str) -> bool {
    let before = settings.custom_agents.len();
    settings
        .custom_agents
        .retain(|definition| definition.id != id);
    if settings.custom_agents.len() == before {
        return false;
    }
    if settings.selected_agent_key == format!("custom:{id}") {
        settings.selected_agent_key = "builtin:codex".to_owned();
    }
    true
}

/// 在独立提交快照中按稳定 ID 新增或替换定义，同时把新定义设为保存时的选择。
///
/// @author kongweiguang
fn upsert_custom_agent(
    settings: &mut AgentLauncherSettings,
    editing_id: Option<&str>,
    name: String,
    command: String,
) -> Result<String, String> {
    validate_custom_agent_fields(settings, editing_id, &name, &command)?;
    let name = name.trim().to_owned();
    let command = command.trim().to_owned();
    let id = if let Some(id) = editing_id {
        let Some(definition) = settings
            .custom_agents
            .iter_mut()
            .find(|definition| definition.id == id)
        else {
            return Err(crate::locale::text(
                "此自定义 Agent 已变化，请刷新设置后重试。",
                "This custom Agent changed. Reload Settings before trying again.",
            )
            .to_string());
        };
        definition.name = name;
        definition.command = command;
        id.to_owned()
    } else {
        let definition = CustomAgentDefinition::new(name, command);
        let id = definition.id.clone();
        settings.custom_agents.push(definition);
        id
    };
    settings.selected_agent_key = format!("custom:{id}");
    Ok(id)
}

/// 仅接受领域定义已有的内置 key 或自定义 canonical UUID key。
///
/// @author kongweiguang
fn launcher_key(value: &str) -> Option<AgentLauncherKey> {
    let provider = match value {
        "builtin:codex" => AgentProvider::Codex,
        "builtin:claude" => AgentProvider::Claude,
        "builtin:pi" => AgentProvider::Pi,
        _ => AgentProvider::Custom,
    };
    AgentLauncherKey::new(value.to_owned(), provider).ok()
}

/// 列表只显示 shell 可执行文件名而隐藏参数，避免普通摘要暴露可能包含的敏感值。
///
/// @author kongweiguang
fn custom_agent_executable_name(command: &str) -> String {
    let mut executable = String::new();
    let mut quote = None;
    let mut characters = command.trim().chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let escaped = characters.peek().is_some_and(|next| {
                *next == '"' || *next == '\'' || *next == '\\' || next.is_whitespace()
            });
            if escaped {
                if let Some(next) = characters.next() {
                    executable.push(next);
                }
            } else {
                executable.push(character);
            }
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                executable.push(character);
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            character if character.is_whitespace() => break,
            character => executable.push(character),
        }
    }
    let name = executable.rsplit(['/', '\\']).next();
    name.filter(|name| !name.is_empty())
        .unwrap_or("自定义命令")
        .to_owned()
}

/// 同步两个输入实体而不把回填误解释成一次用户输入。
///
/// @author kongweiguang
fn set_manager_inputs(
    name: &Entity<InputState>,
    command: &Entity<TextareaState>,
    name_value: String,
    command_value: String,
    window: &mut Window,
    cx: &mut Context<CustomAgentLauncherManager>,
) {
    name.update(cx, |input, cx| input.set_value(name_value, window, cx));
    command.update(cx, |input, cx| input.set_value(command_value, window, cx));
}

/// 字段标题固定为 main 的 12px semibold，并以 2px 补偿配合 4px flex gap 得到 6px 控件间距。
///
/// @author kongweiguang
fn manager_field_label(zh: &'static str, en: &'static str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_xs)
        .line_height(px(16.))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .mb(px(2.))
        .text_color(theme.muted)
        .child(crate::locale::text(zh, en))
}

#[cfg(test)]
#[path = "../../../tests/unit/custom_agent_launcher_manager.rs"]
mod tests;
