//! 运维危险操作仅向根模态提供内容，遮罩、视口限制和快捷键隔离由根层唯一协调。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
};
use kerminal_domain::operations::ComposeAction;

use super::forms::{ForwardFormTransition, next_forward_form_open};
use super::{ContainerPanelContext, OperationsEvent, OperationsView};
use crate::theme::ThemeTokens;

impl OperationsView {
    /// 打开时一次性冻结目标与 pane cwd，并按 main 的目录/目标名和时间戳生成可编辑名称。
    /// @author kongweiguang
    pub(crate) fn request_tmux_create_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.root_confirmation_active()
            || self.tmux.phase() != super::state::SectionPhase::Ready
            || !super::actions::tmux_session_creation_enabled(
                self.busy,
                self.tmux_target_available(),
                self.tmux.data().map(|snapshot| &snapshot.capability),
            )
        {
            return;
        }
        self.ensure_inputs(window, cx);
        let target = self.tmux_target();
        let frozen_working_directory = self
            .tmux_attach_binding
            .as_ref()
            .filter(|binding| binding.target.matches_tmux_target(&target))
            .and_then(|binding| binding.working_directory.clone());
        let target_label = target
            .host_id
            .as_ref()
            .map(|host_id| {
                self.host_display_names
                    .get(host_id)
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| host_id.as_str().to_owned())
            })
            .unwrap_or_else(|| crate::locale::text("本机", "Local").to_string());
        let existing_names = self
            .tmux
            .data()
            .map(|snapshot| {
                snapshot
                    .sessions
                    .iter()
                    .map(|session| session.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let name = super::render::tmux::default_tmux_session_name(
            frozen_working_directory.as_deref(),
            &target_label,
            &timestamp,
            &existing_names,
        );
        self.tmux_create_cwd_snapshot = Some(frozen_working_directory.clone());
        if let Some(inputs) = self.inputs.as_ref() {
            inputs
                .tmux_name
                .update(cx, |state, cx| state.set_value(name, window, cx));
            inputs.tmux_working_directory.update(cx, |state, cx| {
                state.set_value(
                    frozen_working_directory.clone().unwrap_or_default(),
                    window,
                    cx,
                );
            });
        }
        self.tmux_dialog_context = Some((target, self.tmux_view_generation));
        self.tmux_create_form_open = true;
        self.status = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 保存会话创建身份并沿用原输入初始化；刷新可更新活动状态，但不能把旧 ID 换成新会话。
    /// @author kongweiguang
    pub(super) fn request_tmux_rename_dialog(
        &mut self,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.root_confirmation_active() || !self.tmux_target_available() {
            return;
        }
        let Some(session) = self.tmux.data().and_then(|snapshot| {
            snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .cloned()
        }) else {
            return;
        };
        let name = session.name.clone();
        self.prepare_tmux_rename(session_id, name, window, cx);
        self.tmux_dialog_context = Some((self.tmux_target(), self.tmux_view_generation));
        self.tmux_dialog_session = Some(session);
        self.status = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 结束确认绑定真实 ID 与 server 作用域，取消不发送命令，提交后必须等待实际回执。
    /// @author kongweiguang
    pub(super) fn request_tmux_kill_dialog(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.busy || self.root_confirmation_active() || !self.tmux_target_available() {
            return;
        }
        let Some(session) = self.tmux.data().and_then(|snapshot| {
            snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .cloned()
        }) else {
            return;
        };
        self.tmux_dialog_context = Some((self.tmux_target(), self.tmux_view_generation));
        self.tmux_dialog_session = Some(session);
        self.tmux_pending_kill = Some(session_id);
        self.status = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 按钮与回车共用同一资格，写入后的回读未完成时不允许重复发出远端变更。
    /// @author kongweiguang
    pub(super) fn tmux_dialog_submission_enabled(&self) -> bool {
        !self.busy
            && self.active
            && self.tmux_target_available()
            && self.tmux.phase() == super::state::SectionPhase::Ready
            && self
                .tmux_dialog_context
                .as_ref()
                .is_some_and(|(target, generation)| {
                    target == &self.tmux_target() && *generation == self.tmux_view_generation
                })
            && self.tmux_dialog_session_is_current()
    }

    /// 新建没有既存会话；改名和结束必须仍指向弹窗所展示的同一对象。
    /// @author kongweiguang
    fn tmux_dialog_session_is_current(&self) -> bool {
        self.tmux_dialog_session.as_ref().is_none_or(|expected| {
            super::state::tmux_confirmation_session_matches(
                expected,
                self.tmux.data().and_then(|snapshot| {
                    snapshot
                        .sessions
                        .iter()
                        .find(|session| session.id == expected.id)
                }),
            )
        })
    }

    /// 仅在接纳新快照后失效旧确认；缺少创建时间时不跨刷新延续未知身份。
    /// 保留此前的失败/未知结果说明，在途写操作仍由真实回执收尾。
    /// @author kongweiguang
    pub(super) fn reconcile_tmux_dialog_session(&mut self, cx: &mut Context<Self>) {
        let Some(expected) = self.tmux_dialog_session.as_ref() else {
            return;
        };
        if self.busy || expected.created_at.is_some() && self.tmux_dialog_session_is_current() {
            return;
        }
        self.clear_tmux_dialog();
        let notice = crate::locale::text(
            "会话已变化或无法确认原身份，请重新选择会话后操作。",
            "The session changed or its original identity could not be verified. Select it again.",
        );
        self.status = Some(match self.status.take() {
            Some(previous) => format!("{previous}\n{notice}"),
            None => notice.to_string(),
        });
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 回调在提交瞬间再校验目标；目标失效关闭旧意图，读取失败则保留草稿供刷新或取消。
    /// @author kongweiguang
    pub(super) fn ensure_tmux_dialog_current(&mut self, cx: &mut Context<Self>) -> bool {
        if self.tmux_dialog_submission_enabled() {
            return true;
        }
        let same_context = self.active
            && self.tmux_target_available()
            && self
                .tmux_dialog_context
                .as_ref()
                .is_some_and(|(target, generation)| {
                    target == &self.tmux_target() && *generation == self.tmux_view_generation
                });
        if !same_context || !self.tmux_dialog_session_is_current() {
            self.clear_tmux_dialog();
            self.status = Some(
                crate::locale::text(
                    "tmux 目标或会话已变化，请核对后重新打开操作。",
                    "The tmux target or session changed. Verify it and reopen the operation.",
                )
                .to_string(),
            );
            cx.emit(OperationsEvent::RootConfirmationChanged);
        } else if self.status.is_none() {
            self.status = Some(
                crate::locale::text(
                    "请先刷新会话状态并等待核对完成。",
                    "Refresh sessions and wait for verification to finish.",
                )
                .to_string(),
            );
        }
        cx.notify();
        false
    }

    /// 三类 tmux 弹窗互斥地退出，连同冻结目标清空，避免残余意图重新占用根弹层。
    /// @author kongweiguang
    pub(crate) fn clear_tmux_dialog(&mut self) {
        self.tmux_create_form_open = false;
        self.tmux_rename_target = None;
        self.tmux_pending_kill = None;
        self.tmux_dialog_context = None;
        self.tmux_create_cwd_snapshot = None;
        self.tmux_dialog_session = None;
    }

    /// 复用 main 各弹框宽度；容器提示使用 max-w-xl，隧道端点保留并排阅读空间。
    /// @author kongweiguang
    pub(crate) fn root_confirmation_max_width(&self) -> f32 {
        if self
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.forward_form_open)
        {
            1024.
        } else if self.container_delete_confirmation.is_some() || self.tmux_dialog_context.is_some()
        {
            576.
        } else {
            680.
        }
    }

    /// 根层读取真实待确认状态，不根据可见项目或当前主机重新构造目标。
    /// @author kongweiguang
    pub(crate) fn root_confirmation_active(&self) -> bool {
        self.compose_pending_down.is_some()
            || self.container_delete_confirmation.is_some()
            || self.forward_pending_delete.is_some()
            || self.tmux_dialog_context.is_some()
            || self
                .inputs
                .as_ref()
                .is_some_and(|inputs| inputs.forward_form_open)
    }

    /// 表单直接聚焦首字段；结束 tmux 保留 Root trap 焦点，避免焦点落到未挂载的叶子句柄。
    /// @author kongweiguang
    pub(crate) fn focus_root_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_container_delete(window, cx) {
            return;
        }
        let input = self.inputs.as_ref().and_then(|inputs| {
            if self.tmux_create_form_open {
                Some(inputs.tmux_name.clone())
            } else if self.tmux_rename_target.is_some() {
                Some(inputs.tmux_rename.clone())
            } else if inputs.forward_form_open {
                Some(inputs.forward_name.clone())
            } else {
                None
            }
        });
        if let Some(input) = input {
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        if self.tmux_dialog_context.is_some() {
            return;
        }
        let focus = self
            .confirmation_focus
            .get_or_insert_with(|| cx.focus_handle());
        window.focus(focus, cx);
    }

    /// 只为仍打开的转发表单提供同一取消路径，避免验收入口误清其它根层确认状态。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_forward_form(&mut self, cx: &mut Context<Self>) {
        if !self
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.forward_form_open)
        {
            return;
        }
        self.cancel_root_confirmation(cx);
    }

    /// Esc 和取消按钮都只撤销尚未发出的请求，不假装能撤销已经提交的远端变更。
    /// 容器 ModalShell 的关闭规则与 X 一致，忙碌时只释放其草稿，不取消原对象的在途操作。
    /// @author kongweiguang
    pub(crate) fn cancel_root_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.container_delete_confirmation.is_some() {
            self.dismiss_container_delete_confirmation(cx);
            return;
        }
        if self.busy {
            return;
        }
        if self.forward_save_pending.is_some() {
            self.archive_forward_save_review(cx);
            return;
        }
        self.compose_pending_down = None;
        self.container_delete_confirmation = None;
        self.compose_pending_context = None;
        self.confirmed_compose_down = None;
        self.forward_pending_delete = None;
        self.clear_tmux_dialog();
        if let Some(inputs) = self.inputs.as_mut() {
            inputs.forward_form_open =
                next_forward_form_open(inputs.forward_form_open, ForwardFormTransition::Cancel);
        }
        self.forward_edit_expected = None;
        self.forward_edit_transient_id = None;
        self.forward_add_host_expected = None;
        self.status = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 确认使用持有的完整目标/基线；消费后交给原用例处理取消、未知回执和资源所有权。
    /// @author kongweiguang
    pub(crate) fn confirm_root_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.forward_save_pending.is_some() {
            return;
        }
        if self.container_delete_confirmation.is_some() {
            self.confirm_container_delete(cx);
            return;
        }
        if self.tmux_dialog_context.is_some() {
            if self.tmux_create_form_open {
                self.create_tmux_session(cx);
            } else if self.tmux_rename_target.is_some() {
                self.rename_tmux_session(cx);
            } else if let Some(session_id) = self.tmux_pending_kill.clone() {
                self.kill_tmux_session(session_id, cx);
            }
            return;
        }
        if let Some(target) = self.compose_pending_down.clone() {
            self.compose_pending_down = None;
            let context = self.compose_pending_context.take();
            cx.emit(OperationsEvent::RootConfirmationChanged);
            if let Some(context) = context {
                if self.active
                    && self.container_panel_context.as_ref() == Some(&context)
                    && matches!(
                        context,
                        ContainerPanelContext::Host { .. }
                            | ContainerPanelContext::WorkspaceHost { .. }
                    )
                    && self.host_target == target.host_id
                    && self.runtime == target.runtime
                {
                    self.confirmed_compose_down = Some((context.clone(), target.clone()));
                    cx.emit(OperationsEvent::ComposeMutationRequested {
                        context,
                        target,
                        action: ComposeAction::Down,
                    });
                }
            } else {
                self.execute_compose(target, ComposeAction::Down, cx);
            }
        } else if let Some(expected) = self.forward_pending_delete.clone() {
            self.delete_forward_definition(expected, cx);
        }
    }

    /// 表单使用宿主内框的宽高预算，避免继承窄右栏布局或把操作栏挤出可视区域。
    /// @author kongweiguang
    pub(crate) fn render_root_confirmation(
        &self,
        theme: ThemeTokens,
        available_width: f32,
        available_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = if self.compact_layout {
            theme.for_tool_panel()
        } else {
            theme
        };
        if self.container_delete_confirmation.is_some() {
            return self.render_container_delete(theme, cx);
        }
        if self.tmux_dialog_context.is_some() {
            return self.render_tmux_dialog(theme, available_width, cx);
        }
        if self
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.forward_form_open)
        {
            return Some(self.render_forward_form_modal(
                theme,
                available_width,
                available_height,
                cx,
            ));
        }
        let (title, message, confirm_label) = if let Some(target) =
            self.compose_pending_down.as_ref()
        {
            let host = target.host_id.as_ref().map_or_else(
                || crate::locale::text("本机", "Local").to_string(),
                |host_id| format!("SSH · {}", host_id.as_str()),
            );
            (
                crate::locale::text("拆除 Compose 项目", "Remove Compose project"),
                crate::locale::text(
                    format!(
                        "目标：{host} · {:?}\n项目：{}\n目录：{}\n配置（按序）：{}\n\n会移除容器及项目网络；不删除数据卷。完成后仍可从此处重新启动。",
                        target.runtime,
                        target.project,
                        target.working_dir,
                        target.config_files.join(" → "),
                    ),
                    format!(
                        "Target: {host} · {:?}\nProject: {}\nDirectory: {}\nConfig (ordered): {}\n\nContainers and project networks will be removed; volumes are retained. You can start it again here.",
                        target.runtime,
                        target.project,
                        target.working_dir,
                        target.config_files.join(" → "),
                    ),
                ),
                crate::locale::text("确认拆除", "Confirm down"),
            )
        } else {
            let expected = self.forward_pending_delete.as_ref()?;
            (
                crate::locale::text("删除转发定义", "Delete forward definition"),
                crate::locale::text(
                    format!(
                        "名称：{}\nID：{}\nSSH 主机：{}\n类型：{:?}\n监听：{}\n目标：{}\n\n仅删除此停止态定义，不影响运行中的隧道。",
                        expected.config.name,
                        expected.id,
                        expected.config.host_id,
                        expected.config.kind,
                        expected.config.bind,
                        expected
                            .config
                            .destination
                            .as_ref()
                            .map_or_else(|| "SOCKS5 动态目标".to_owned(), ToString::to_string,),
                    ),
                    format!(
                        "Name: {}\nID: {}\nSSH host: {}\nType: {:?}\nBind: {}\nDestination: {}\n\nOnly this stopped definition will be deleted; running tunnels are unchanged.",
                        expected.config.name,
                        expected.id,
                        expected.config.host_id,
                        expected.config.kind,
                        expected.config.bind,
                        expected.config.destination.as_ref().map_or_else(
                            || "SOCKS5 dynamic destination".to_owned(),
                            ToString::to_string,
                        ),
                    ),
                ),
                crate::locale::text("确认删除", "Confirm delete"),
            )
        };
        Some(
            div()
                .id("operations-root-confirmation")
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .bg(theme.materials.chrome)
                .text_color(theme.foreground)
                .rounded_lg()
                .when_some(self.confirmation_focus.as_ref(), |view, focus| {
                    view.track_focus(focus)
                })
                .child(
                    div()
                        .text_size(theme.text_heading)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(div().text_size(theme.text_sm).child(message))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("operations-root-confirm-cancel")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("取消", "Cancel"))
                                .disabled(self.busy)
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.cancel_root_confirmation(cx)
                                })),
                        )
                        .child(
                            Button::new("operations-root-confirm-submit")
                                .compact()
                                .primary()
                                .label(confirm_label)
                                .disabled(self.busy)
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.confirm_root_confirmation(cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}
