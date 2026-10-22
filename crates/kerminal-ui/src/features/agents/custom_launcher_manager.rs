//! Agent 自定义启动器的本地草稿 UI；Root 持有弹层并负责唯一 Preferences CAS。
//!
//! @author kongweiguang

use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    IntoElement, Render, Subscription, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
    },
    div,
    prelude::*,
    px,
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
    Failed(String),
    Conflict(String),
    OutcomeUnknown(String),
    ReadBackFailed(String),
    ReadBack {
        settings: AgentLauncherSettings,
        retry_allowed: bool,
    },
}

/// 保存或读回回执对应的管理器交互状态；写入失败必须先经过真实读回才能恢复。
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

/// 同步门禁拒绝、异步失败和冲突都必须先从 Preferences owner 取得真实读回结果。
///
/// @author kongweiguang
fn recovery_flags_for_outcome(outcome: &CustomAgentManagerWriteOutcome) -> ManagerRecoveryFlags {
    match outcome {
        CustomAgentManagerWriteOutcome::Failed(_)
        | CustomAgentManagerWriteOutcome::Conflict(_)
        | CustomAgentManagerWriteOutcome::OutcomeUnknown(_)
        | CustomAgentManagerWriteOutcome::ReadBackFailed(_) => RECOVERY_READBACK,
        CustomAgentManagerWriteOutcome::ReadBack {
            retry_allowed: true,
            ..
        } => RECOVERY_RETRY,
        CustomAgentManagerWriteOutcome::ReadBack {
            retry_allowed: false,
            ..
        } => RECOVERY_PREFERENCES_REVIEW,
        CustomAgentManagerWriteOutcome::Confirmed(_) => RECOVERY_IDLE,
    }
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

/// 管理器不持有 Preferences service；所有编辑先留在本地，Root 只接收完整可校验快照。
pub(crate) struct CustomAgentLauncherManager {
    initial: AgentLauncherSettings,
    draft: AgentLauncherSettings,
    selected_id: Option<String>,
    editing_id: Option<String>,
    form_active: bool,
    confirm_delete_id: Option<String>,
    confirm_discard: bool,
    name: Entity<InputState>,
    command: Entity<TextareaState>,
    modal_focus: FocusHandle,
    _input_subscriptions: [Subscription; 2],
    pending_submission: Option<AgentLauncherSettings>,
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
        let name_subscription = cx.subscribe(&name, |manager, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                manager.validation_error = None;
                cx.notify();
            }
        });
        let command_subscription = cx.subscribe(&command, |manager, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                manager.validation_error = None;
                cx.notify();
            }
        });
        Self {
            initial: settings.clone(),
            draft: settings,
            selected_id: None,
            editing_id: None,
            form_active: true,
            confirm_delete_id: None,
            confirm_discard: false,
            name,
            command,
            modal_focus,
            _input_subscriptions: [name_subscription, command_subscription],
            pending_submission: None,
            busy: false,
            readback_required: false,
            retry_ready: false,
            preferences_review_required: false,
            status: None,
            validation_error: None,
        }
    }

    /// 只有 Root 确认保存回执，或读回值与原提交逐字段一致时，才允许 Root 关闭弹层。
    ///
    /// @author kongweiguang
    pub(crate) fn apply_write_outcome(
        &mut self,
        outcome: CustomAgentManagerWriteOutcome,
        cx: &mut Context<Self>,
    ) -> bool {
        self.busy = false;
        let recovery_flags = recovery_flags_for_outcome(&outcome);
        match outcome {
            CustomAgentManagerWriteOutcome::Confirmed(settings) => {
                if self.pending_submission.as_ref() == Some(&settings) {
                    self.pending_submission = None;
                    self.initial = settings.clone();
                    self.draft = settings;
                    self.apply_recovery_flags(RECOVERY_IDLE);
                    self.status = None;
                    cx.notify();
                    return true;
                }
                self.status = Some(crate::locale::text(
                    "保存回执与本次草稿不一致，请先读回核对。",
                    "The save receipt does not match this draft. Read settings back before continuing.",
                ).to_string());
                self.apply_recovery_flags(RECOVERY_READBACK);
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
                if self.pending_submission.as_ref() == Some(&settings) {
                    self.pending_submission = None;
                    self.initial = settings.clone();
                    self.draft = settings;
                    self.apply_recovery_flags(RECOVERY_IDLE);
                    self.status = None;
                    cx.notify();
                    return true;
                }
                self.pending_submission = None;
                self.apply_recovery_flags(recovery_flags);
                self.status = Some(if retry_allowed {
                    crate::locale::text(
                        "读回内容与本地草稿不同。草稿仍保留；确认后可用当前配置版本重新提交。",
                        "Read-back settings differ from this draft. The draft is preserved and can be resubmitted against the current settings revision.",
                    ).to_string()
                } else {
                    crate::locale::text(
                        "读回内容与本地草稿不同。草稿仍保留，请到设置中完成核对后再提交。",
                        "Read-back settings differ from this draft. The draft is preserved; review Settings before submitting again.",
                    ).to_string()
                });
            }
        }
        cx.notify();
        false
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

    /// Root 的 Escape、遮罩或系统关闭意图必须先经过草稿确认，不能直接销毁管理器 Entity。
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

    /// 本地格式、长度、唯一性与数量限制在 Root CAS 前校验，应用层仍会再次校验最终设置。
    ///
    /// @author kongweiguang
    fn validate_editor(&self, cx: &Context<Self>) -> Result<(String, String), String> {
        let name = self.name.read(cx).value().to_string();
        let command = self.command.read(cx).value().to_string();
        validate_custom_agent_fields(&self.draft, self.editing_id.as_deref(), &name, &command)?;
        Ok((name.trim().to_owned(), command.trim().to_owned()))
    }

    /// 将输入暂存到本地配置草稿；此操作不写磁盘，也不改变其他会话或目标绑定。
    ///
    /// @author kongweiguang
    fn stage_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.form_active || self.busy || self.readback_required {
            return false;
        }
        let (name, command) = match self.validate_editor(cx) {
            Ok(values) => values,
            Err(error) => {
                self.validation_error = Some(error);
                cx.notify();
                return false;
            }
        };
        let id =
            match upsert_custom_agent(&mut self.draft, self.editing_id.as_deref(), name, command) {
                Ok(id) => id,
                Err(error) => {
                    self.validation_error = Some(error);
                    cx.notify();
                    return false;
                }
            };
        self.selected_id = Some(id);
        self.editing_id = None;
        self.form_active = false;
        self.confirm_delete_id = None;
        self.validation_error = None;
        self.status = Some(
            crate::locale::text(
                "已加入本地草稿；点击“保存并选择”后才会写入设置。",
                "Added to the local draft. Settings are written only after Save and select.",
            )
            .to_string(),
        );
        set_manager_inputs(
            &self.name,
            &self.command,
            String::new(),
            String::new(),
            window,
            cx,
        );
        cx.notify();
        true
    }

    /// 新增入口只清空编辑区，不改动已暂存的定义或正在运行的 Agent 会话。
    ///
    /// @author kongweiguang
    fn begin_new_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.readback_required {
            return;
        }
        if !self.stage_editor_before_navigation(window, cx) {
            return;
        }
        if self.draft.custom_agents.len() >= 32 {
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
        self.form_active = true;
        self.confirm_delete_id = None;
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
        cx.notify();
    }

    /// 编辑列表项时按稳定 UUID 回填两个字段，不从当前 launcher 选择推断或重建定义。
    ///
    /// @author kongweiguang
    fn edit_agent(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.readback_required {
            return;
        }
        if !self.stage_editor_before_navigation(window, cx) {
            return;
        }
        let Some(definition) = self
            .draft
            .custom_agents
            .iter()
            .find(|definition| definition.id == id)
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
        self.form_active = true;
        self.confirm_delete_id = None;
        self.validation_error = None;
        self.status = None;
        set_manager_inputs(&self.name, &self.command, name, command, window, cx);
        cx.notify();
    }

    /// 删除前在列表内确认，确定后仅删除本地草稿项，当前 launcher 被删除时明确回退 Codex。
    ///
    /// @author kongweiguang
    fn delete_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.busy || self.readback_required {
            return;
        }
        if !remove_custom_agent(&mut self.draft, id) {
            self.confirm_delete_id = None;
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
        if self.editing_id.as_deref() == Some(id) {
            self.editing_id = None;
            self.form_active = false;
            self.validation_error = None;
        }
        if self.selected_id.as_deref() == Some(id) {
            self.selected_id = None;
        }
        self.confirm_delete_id = None;
        self.status = Some(
            crate::locale::text(
                "已从本地草稿移除；点击“保存并选择”后才会写入设置。",
                "Removed from the local draft. Settings are written only after Save and select.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 一次请求同时校验编辑器并提交完整快照；保留原表单，确保冲突、失败和未知回执可恢复。
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
        let mut settings = self.draft.clone();
        if self.form_active {
            let (name, command) = match self.validate_editor(cx) {
                Ok(values) => values,
                Err(error) => {
                    self.validation_error = Some(error);
                    cx.notify();
                    return;
                }
            };
            let id =
                match upsert_custom_agent(&mut settings, self.editing_id.as_deref(), name, command)
                {
                    Ok(id) => id,
                    Err(error) => {
                        self.validation_error = Some(error);
                        cx.notify();
                        return;
                    }
                };
            // Root owns CAS; retain the exact local submission and leave the editor mounted until
            // confirmation so a failed or unknown result cannot erase the user's input.
            self.draft = settings.clone();
            self.selected_id = Some(id.clone());
            self.editing_id = Some(id);
            self.confirm_delete_id = None;
            self.validation_error = None;
        }
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
        self.pending_submission = Some(settings.clone());
        self.busy = true;
        self.readback_required = false;
        self.retry_ready = false;
        self.preferences_review_required = false;
        self.confirm_discard = false;
        self.status = Some(
            crate::locale::text(
                "正在提交设置，等待保存回执…",
                "Saving settings; waiting for the confirmed result…",
            )
            .to_string(),
        );
        cx.emit(CustomAgentManagerEvent::Save {
            settings,
            select_key,
        });
        cx.notify();
    }

    /// 未确认的写入或偏好状态要求 Root 重新读取设置；读取期间保留弹层和原草稿。
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

    /// 从 Preferences 返回时用同一忙态和事件路径发起真实读回，不能用本地草稿冒充回执。
    ///
    /// @author kongweiguang
    pub(crate) fn request_readback_from_root(&mut self, cx: &mut Context<Self>) {
        self.request_readback(cx);
    }

    /// 失败草稿需要用户明确决定是否放弃，关闭和取消都不会自动写入或撤销外部会话。
    ///
    /// @author kongweiguang
    fn request_close(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.is_dirty(cx) {
            self.confirm_discard = true;
            cx.notify();
        } else {
            cx.emit(CustomAgentManagerEvent::Close);
        }
    }

    /// 比较初始设置和本地表单，关闭确认只针对管理器草稿，不检查或关闭运行中的 Agent。
    ///
    /// @author kongweiguang
    fn is_dirty(&self, cx: &Context<Self>) -> bool {
        self.draft != self.initial || self.editor_is_dirty(cx)
    }

    /// 新建表单按非空内容判断变化，编辑表单与原定义比较，避免未改动时弹出误导性的丢弃确认。
    ///
    /// @author kongweiguang
    fn editor_is_dirty(&self, cx: &Context<Self>) -> bool {
        if !self.form_active {
            return false;
        }
        let name = self.name.read(cx).value().trim().to_owned();
        let command = self.command.read(cx).value().trim().to_owned();
        if let Some(id) = self.editing_id.as_deref() {
            return self
                .draft
                .custom_agents
                .iter()
                .find(|definition| definition.id == id)
                .is_none_or(|definition| definition.name != name || definition.command != command);
        }
        !name.is_empty() || !command.is_empty()
    }

    /// 在列表导航前将有效编辑暂存为本地草稿；无改动表单直接收起，非法内容保留并阻止切换。
    ///
    /// @author kongweiguang
    fn stage_editor_before_navigation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.form_active {
            return true;
        }
        if self.editor_is_dirty(cx) {
            return self.stage_editor(window, cx);
        }
        self.form_active = false;
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
        true
    }

    /// 绘制管理器标题与关闭动作；异步写入和读回期间关闭入口锁定，避免丢弃未核对草稿。
    ///
    /// @author kongweiguang
    fn render_header(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .px_3()
            .py_3()
            .flex()
            .items_start()
            .justify_between()
            .gap_3()
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
                            .text_size(theme.text_title)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(crate::locale::text(
                                "管理自定义 Agent",
                                "Manage custom Agents",
                            )),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "保存常用的 Agent CLI 启动命令。",
                                "Save launch commands for your Agent CLIs.",
                            )),
                    ),
            )
            .child(
                Button::new("custom-agent-manager-close")
                    .compact()
                    .ghost()
                    .h(px(32.))
                    .label(crate::locale::text("关闭", "Close"))
                    .disabled(self.busy)
                    .on_click(
                        cx.listener(|manager, _: &ClickEvent, _, cx| manager.request_close(cx)),
                    ),
            )
    }

    /// 列表在宽窗口占左栏，窄窗口改为有限高度的顶部区域，避免挤压命令编辑器。
    ///
    /// @author kongweiguang
    fn render_saved_agents(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
        narrow: bool,
    ) -> impl IntoElement {
        let add_disabled =
            self.busy || self.readback_required || self.draft.custom_agents.len() >= 32;
        let mut list = div()
            .id("custom-agent-manager-list")
            .w_full()
            .flex()
            .flex_col()
            .gap_1();
        for definition in &self.draft.custom_agents {
            let id = definition.id.clone();
            let is_selected = self.selected_id.as_deref() == Some(id.as_str());
            let is_deleting = self.confirm_delete_id.as_deref() == Some(id.as_str());
            let name = definition.name.clone();
            let edit_id = id.clone();
            let mut row = div()
                .id(format!("custom-agent-row-{id}"))
                .w_full()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(if is_selected {
                    theme.accent
                } else {
                    theme.border
                })
                .bg(if is_selected {
                    theme.selected
                } else {
                    theme.secondary
                })
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new(format!("custom-agent-edit-{id}"))
                                .compact()
                                .ghost()
                                .flex_1()
                                .min_w(px(0.))
                                .h(px(32.))
                                .label(name)
                                .disabled(self.busy || self.readback_required)
                                .on_click(cx.listener(
                                    move |manager, _: &ClickEvent, window, cx| {
                                        manager.edit_agent(edit_id.clone(), window, cx)
                                    },
                                )),
                        )
                        .child(
                            Button::new(format!("custom-agent-delete-{id}"))
                                .compact()
                                .ghost()
                                .h(px(32.))
                                .label(crate::locale::text("删除", "Delete"))
                                .disabled(self.busy || self.readback_required)
                                .on_click(cx.listener(move |manager, _: &ClickEvent, _, cx| {
                                    manager.confirm_delete_id = Some(id.clone());
                                    cx.notify();
                                })),
                        ),
                );
            if is_deleting {
                let confirm_id = definition.id.clone();
                row = row.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .w_full()
                                .text_size(theme.text_xs)
                                .text_color(theme.warning)
                                .child(crate::locale::text(
                                    "仅从待保存草稿删除。",
                                    "Remove from the pending draft only.",
                                )),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .justify_end()
                                .gap_1()
                                .child(
                                    Button::new(format!("custom-agent-delete-cancel-{confirm_id}"))
                                        .compact()
                                        .ghost()
                                        .h(px(32.))
                                        .label(crate::locale::text("返回", "Keep"))
                                        .on_click(cx.listener(|manager, _: &ClickEvent, _, cx| {
                                            manager.confirm_delete_id = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(format!(
                                        "custom-agent-delete-confirm-{confirm_id}"
                                    ))
                                    .compact()
                                    .secondary()
                                    .h(px(32.))
                                    .label(crate::locale::text("确认", "Remove"))
                                    .on_click(cx.listener(
                                        move |manager, _: &ClickEvent, _, cx| {
                                            manager.delete_agent(&confirm_id, cx)
                                        },
                                    )),
                                ),
                        ),
                );
            }
            list = list.child(row);
        }
        if self.draft.custom_agents.is_empty() {
            list = list.child(
                div()
                    .w_full()
                    .min_h(px(96.))
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(theme.text_sm)
                    .text_color(theme.muted)
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
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_size(theme.text_body)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(crate::locale::text("自定义 Agent", "Custom Agents")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(format!("{} / 32", self.draft.custom_agents.len())),
                            )
                            .child(
                                Button::new("custom-agent-manager-add")
                                    .compact()
                                    .secondary()
                                    .h(px(32.))
                                    .label(crate::locale::text("新增", "Add"))
                                    .disabled(add_disabled)
                                    .on_click(cx.listener(
                                        move |manager, _: &ClickEvent, window, cx| {
                                            manager.begin_new_agent(window, cx)
                                        },
                                    )),
                            ),
                    ),
            )
            .child(
                div()
                    .id("custom-agent-manager-scroll")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(list),
            );
        if narrow {
            sidebar = sidebar
                .h(px(156.))
                .flex_shrink_0()
                .border_b_1()
                .border_color(theme.border);
        } else {
            sidebar = sidebar
                .w(px(236.))
                .flex_shrink_0()
                .border_r_1()
                .border_color(theme.border);
        }
        sidebar
    }

    /// 将实时校验反馈放在表单顶部；输入约束即时可见，启动命令仍交由独立会话解释。
    ///
    /// @author kongweiguang
    fn render_editor(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let name_count = self.name.read(cx).value().chars().count();
        let command_count = self.command.read(cx).value().chars().count();
        let live_validation_error = if self.form_active {
            self.validate_editor(cx).err()
        } else {
            None
        };
        let editor_feedback = self.validation_error.clone().or(live_validation_error);
        let editor_feedback_color = if self.validation_error.is_some() {
            theme.error
        } else {
            theme.warning
        };
        let mut editor = div()
            .id("custom-agent-manager-editor")
            .w_full()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll();
        if self.form_active {
            editor = editor
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
                                .child(crate::locale::text(
                                    if self.editing_id.is_some() { "编辑 Agent" } else { "添加 Agent" },
                                    if self.editing_id.is_some() { "Edit Agent" } else { "Add Agent" },
                                )),
                        )
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "名称用于启动器列表，命令在独立会话目录中运行。",
                                    "The name appears in the launcher; the command runs in its own session directory.",
                                )),
                        )
                        .when_some(editor_feedback, |element, message| {
                            element.child(
                                div()
                                    .id("custom-agent-manager-validation-feedback")
                                    .text_size(theme.text_xs)
                                    .text_color(editor_feedback_color)
                                    .child(message),
                            )
                        }),
                )
                .child(manager_field_label("名称", "Name", theme))
                .child(
                    Input::new(&self.name)
                        .w_full()
                        .h(px(32.))
                        .text_size(theme.text_sm)
                        .disabled(self.busy || self.readback_required),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .text_size(theme.text_xs)
                        .text_color(if name_count > 64 { theme.error } else { theme.muted })
                        .child(format!("{name_count} / 64")),
                )
                .child(manager_field_label("启动命令", "Launch command", theme))
                .child(
                    Textarea::new(&self.command)
                        .w_full()
                        .h(px(112.))
                        .text_size(theme.text_sm)
                        .disabled(self.busy || self.readback_required),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .text_size(theme.text_xs)
                        .text_color(if command_count > 4096 { theme.error } else { theme.muted })
                        .child(format!("{command_count} / 4096")),
                )
                .child(
                    div()
                        .w_full()
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
                                .child(crate::locale::text(
                                    "启动命令会以明文保存在 settings.toml。请勿填写 API Key、密码或 token。",
                                    "Launch commands are stored in plain text in settings.toml. Do not include API keys, passwords, or tokens.",
                                )),
                        ),
                );
        } else {
            let guidance = if self.retry_ready {
                crate::locale::text(
                    "设置已读回；检查本地定义后可明确重新提交草稿。",
                    "Settings were read back. Review the local definitions before explicitly resubmitting.",
                )
            } else {
                crate::locale::text(
                    "选择一个 Agent 进行编辑，或新增一条启动命令。",
                    "Select an Agent to edit, or add a launch command.",
                )
            };
            editor = editor.child(
                div()
                    .id("custom-agent-manager-empty-editor")
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .p_3()
                    .text_size(theme.text_sm)
                    .text_color(theme.muted)
                    .when_some(self.validation_error.clone(), |element, message| {
                        element.child(
                            div()
                                .w_full()
                                .text_size(theme.text_xs)
                                .text_color(theme.error)
                                .child(message),
                        )
                    })
                    .child(guidance),
            );
        }
        editor
    }

    /// 保留 Root 对写入与关闭的唯一仲裁；按钮实时复用提交校验，错误贴近表单顶部以免滚动裁掉。
    ///
    /// @author kongweiguang
    fn render_content(
        &mut self,
        theme: ThemeTokens,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let editor_valid = self.validate_editor(cx).is_ok();
        let save_disabled = manager_save_disabled(
            self.form_active,
            editor_valid,
            self.busy,
            self.readback_required,
            self.preferences_review_required,
        );
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
        if self.confirm_discard {
            footer = footer.child(
                div()
                    .flex_1()
                    .text_size(theme.text_sm)
                    .text_color(theme.warning)
                    .child(crate::locale::text(
                        "丢弃尚未写入的管理器草稿？",
                        "Discard the manager draft that has not been written?",
                    )),
            );
            footer = footer
                .child(
                    Button::new("custom-agent-discard-keep")
                        .compact()
                        .secondary()
                        .h(px(32.))
                        .label(crate::locale::text("继续编辑", "Keep editing"))
                        .on_click(cx.listener(|manager, _: &ClickEvent, _, cx| {
                            manager.confirm_discard = false;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("custom-agent-discard-confirm")
                        .compact()
                        .secondary()
                        .h(px(32.))
                        .label(crate::locale::text("丢弃并关闭", "Discard and close"))
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                            cx.emit(CustomAgentManagerEvent::Close);
                        })),
                );
        } else {
            let save_label = if self.retry_ready {
                crate::locale::text("按本地草稿重新提交", "Resubmit local draft")
            } else {
                crate::locale::text("保存并选择", "Save and select")
            };
            let readback_button = self.readback_required.then(|| {
                Button::new("custom-agent-readback")
                    .compact()
                    .secondary()
                    .h(px(32.))
                    .label(crate::locale::text("读回核对", "Read back"))
                    .disabled(self.busy)
                    .on_click(
                        cx.listener(|manager, _: &ClickEvent, _, cx| manager.request_readback(cx)),
                    )
            });
            let open_preferences_button =
                (!self.busy && self.preferences_review_required).then(|| {
                    Button::new("custom-agent-open-settings")
                        .compact()
                        .ghost()
                        .h(px(32.))
                        .label(crate::locale::text("打开设置", "Open Settings"))
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                            cx.emit(CustomAgentManagerEvent::OpenPreferences);
                        }))
                });
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
            if let Some(button) = open_preferences_button {
                footer = footer.child(button);
            }
            if let Some(button) = readback_button {
                footer = footer.child(button);
            }
            footer = footer
                .child(
                    Button::new("custom-agent-manager-cancel")
                        .compact()
                        .secondary()
                        .h(px(32.))
                        .label(crate::locale::text("取消", "Cancel"))
                        .disabled(self.busy)
                        .on_click(
                            cx.listener(|manager, _: &ClickEvent, _, cx| manager.request_close(cx)),
                        ),
                )
                .child(
                    Button::new("custom-agent-manager-save")
                        .compact()
                        .primary()
                        .h(px(32.))
                        .label(save_label)
                        .disabled(save_disabled)
                        .on_click(
                            cx.listener(|manager, _: &ClickEvent, _, cx| manager.request_save(cx)),
                        ),
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
        div()
            .id("custom-agent-manager")
            .w_full()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(self.render_header(theme, cx))
            .child(body)
            .child(footer)
    }
}

impl Render for CustomAgentLauncherManager {
    /// 主题令牌实时跟随浅色、深色和系统主题；窄视口将双栏折叠，保留可滚动的编辑区。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let narrow = window.viewport_size().width.as_f32() < 600.;
        self.render_content(ThemeTokens::current(window, cx), narrow, cx)
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

/// 保存按钮仅在活动表单有效且没有进行中的写入/核对时启用；已加入草稿时不要求空表单有效。
///
/// @author kongweiguang
fn manager_save_disabled(
    form_active: bool,
    editor_valid: bool,
    busy: bool,
    readback_required: bool,
    preferences_review_required: bool,
) -> bool {
    busy || readback_required || preferences_review_required || (form_active && !editor_valid)
}

/// 从本地草稿移除定义，只有被删除项正是选择值时才回退 Codex，供 UI 和回归测试共用。
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

/// 在本地草稿中按稳定 ID 新增或替换定义，同时把新定义设为保存时的选择。
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

/// 同步两个输入实体而不把回填误解释成一次本地草稿编辑。
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

/// 统一渲染字段名，保证双栏内输入提示与当前主题字号一致。
///
/// @author kongweiguang
fn manager_field_label(zh: &'static str, en: &'static str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_sm)
        .text_color(theme.muted)
        .child(crate::locale::text(zh, en))
}

#[cfg(test)]
#[path = "../../../tests/unit/custom_agent_launcher_manager.rs"]
mod tests;
