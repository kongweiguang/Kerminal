//! 容器删除保留用户看到的完整名称与目标票据，不从后来选中的行重新解析。
//!
//! @author kongweiguang

use gpui_kit::component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{
    AnyElement, Context, Entity, IntoElement, Subscription, Window, div, prelude::*, px,
};
use kerminal_domain::operations::{ContainerAction, ContainerTarget};

use super::{
    OperationsEvent, OperationsView, container_panel::ContainerPanelActionTicket,
    state::ContainerMutationRequest,
};
use crate::theme::ThemeTokens;

pub(super) struct ContainerDeleteConfirmation {
    target: ContainerTarget,
    name: String,
    action: ContainerAction,
    ticket: ContainerPanelActionTicket,
    input: Entity<InputState>,
    _subscription: Subscription,
}

impl OperationsView {
    /// 精确名称确认与 main 一致；冻结票据可阻止菜单打开后切宿主再误删新目标。
    /// @author kongweiguang
    pub(super) fn request_remove_container(
        &mut self,
        target: ContainerTarget,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_container_lifecycle_confirmation(
            target,
            name,
            ContainerAction::Remove,
            window,
            cx,
        );
    }

    /// main 的 ModalShell 允许忙碌时关闭弹框；只丢弃确认草稿，让已提交的变更继续运行。
    ///
    /// @author kongweiguang
    pub(super) fn dismiss_container_delete_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.container_delete_confirmation.take().is_none() {
            return;
        }
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// main 仅启动直接执行；停止、重启和删除共享固定对象的确认，避免菜单打开后解析新行。
    /// @author kongweiguang
    pub(super) fn request_container_lifecycle_confirmation(
        &mut self,
        target: ContainerTarget,
        name: String,
        action: ContainerAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy
            || self.root_confirmation_active()
            || name.is_empty()
            || !self.container_mutation_allowed(&target)
        {
            return;
        }
        let ticket = self.container_action_ticket(Some(target.clone()));
        if !self.container_action_ticket_is_current(&ticket, Some(&target)) {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(name.clone()));
        let subscription = cx.subscribe(&input, |view, _, event, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                view.confirm_container_delete(cx);
            }
            cx.notify();
        });
        self.container_delete_confirmation = Some(ContainerDeleteConfirmation {
            target,
            name,
            action,
            ticket,
            input,
            _subscription: subscription,
        });
        self.status = None;
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 输入必须逐字匹配显示名称，写入仍通过原 typed mutation 和 Root live 目标门禁。
    /// @author kongweiguang
    pub(super) fn confirm_container_delete(&mut self, cx: &mut Context<Self>) {
        let Some(confirmation) = self.container_delete_confirmation.as_ref() else {
            return;
        };
        if self.busy
            || (confirmation.action == ContainerAction::Remove
                && confirmation.input.read(cx).value().as_str() != confirmation.name)
        {
            return;
        }
        if !self
            .container_action_ticket_is_current(&confirmation.ticket, Some(&confirmation.target))
        {
            return;
        }
        let confirmation = self
            .container_delete_confirmation
            .take()
            .expect("checked confirmation");
        self.request_container_mutation(
            ContainerMutationRequest {
                target: confirmation.target,
                action: confirmation.action,
                force: false,
            },
            &confirmation.ticket,
            cx,
        );
        cx.emit(OperationsEvent::RootConfirmationChanged);
        cx.notify();
    }

    /// 输入实体随确认存在，焦点可在根遮罩挂载后交还，不在 render 中创建状态。
    /// @author kongweiguang
    pub(super) fn focus_container_delete(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(confirmation) = self.container_delete_confirmation.as_ref() else {
            return false;
        };
        if confirmation.action != ContainerAction::Remove {
            return false;
        }
        confirmation
            .input
            .update(cx, |input, cx| input.focus(window, cx));
        true
    }

    /// 外观只描述 main 的名称确认，业务资格和输入校验使用提交时的同一份快照。
    /// 单行确认框用 Styled 保持 36px 高度，绕开 Input 的多行专用 h。
    /// @author kongweiguang
    pub(super) fn render_container_delete(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let confirmation = self.container_delete_confirmation.as_ref()?;
        let removing = confirmation.action == ContainerAction::Remove;
        let matches_name = confirmation.input.read(cx).value().as_str() == confirmation.name;
        let can_delete = !self.busy
            && (!removing || matches_name)
            && self.container_action_ticket_is_current(
                &confirmation.ticket,
                Some(&confirmation.target),
            );
        let (title, description, helper) =
            container_lifecycle_copy(confirmation.action, &confirmation.name);
        let helper = if removing && !matches_name {
            crate::locale::text(
                "请输入完整容器名确认删除",
                "Enter the full container name to confirm deletion",
            )
        } else {
            helper
        };
        let confirm = Button::new("container-delete-submit")
            .icon(gpui_kit::assets::IconName::Check)
            .label(title.clone())
            .disabled(!can_delete)
            .on_click(cx.listener(|view, _, _, cx| view.confirm_container_delete(cx)));
        let confirm = match confirmation.action {
            ContainerAction::Stop | ContainerAction::Remove => confirm.danger(),
            ContainerAction::Restart | ContainerAction::Start => confirm.primary(),
        };
        Some(
            div()
                .id("container-delete-confirmation")
                .w_full()
                .h_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .text_color(theme.foreground)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_4()
                        .border_b_1()
                        .border_color(theme.border)
                        .px_5()
                        .py_4()
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(17.))
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme.muted)
                                        .child(description),
                                ),
                        )
                        .child(
                            Button::new("container-confirm-close")
                                .ghost()
                                .icon(gpui_kit::assets::IconName::Close)
                                .accessibility_label(crate::locale::text(
                                    "关闭弹窗",
                                    "Close dialog",
                                ))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.dismiss_container_delete_confirmation(cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .id("container-lifecycle-confirmation-body")
                        .w_full()
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .px_5()
                        .py_4()
                        .when(removing, |surface| {
                            surface.child(
                                div()
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(theme.text_xs)
                                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                                            .text_color(theme.muted)
                                            .child(crate::locale::text("容器名", "Container name")),
                                    )
                                    .child(gpui_kit::Styled::h(
                                        Input::new(&confirmation.input),
                                        px(36.),
                                    )),
                            )
                        })
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(if removing && !matches_name {
                                    theme.error
                                } else {
                                    theme.muted
                                })
                                .child(helper),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_shrink_0()
                        .justify_end()
                        .gap_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .px_5()
                        .py_4()
                        .child(
                            Button::new("container-delete-cancel")
                                .ghost()
                                .label(crate::locale::text("取消", "Cancel"))
                                .disabled(self.busy)
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.cancel_root_confirmation(cx)),
                                ),
                        )
                        .child(confirm),
                )
                .into_any_element(),
        )
    }
}

/// 文案按 main 的生命周期提示逐项映射，后台动作名称不泄露到产品界面。
/// @author kongweiguang
fn container_lifecycle_copy(
    action: ContainerAction,
    name: &str,
) -> (
    gpui_kit::SharedString,
    gpui_kit::SharedString,
    gpui_kit::SharedString,
) {
    use crate::locale::text;
    match action {
        ContainerAction::Stop => (
            text("停止容器", "Stop container"),
            text(
                format!("停止 {name} 会中断其中正在运行的进程。"),
                format!("Stopping {name} interrupts processes running inside it."),
            ),
            text(
                "停止后可重新启动容器。",
                "The container can be started again after it stops.",
            ),
        ),
        ContainerAction::Restart => (
            text("重启容器", "Restart container"),
            text(
                format!("重启 {name} 会先停止再启动容器。"),
                format!("Restarting {name} stops and then starts the container."),
            ),
            text(
                "适合服务卡住或需要重新加载配置时使用。",
                "Use when a service is stuck or needs to reload its configuration.",
            ),
        ),
        ContainerAction::Remove => (
            text("删除容器", "Delete container"),
            text(
                format!("删除 {name} 会移除该容器对象，容器内未持久化数据可能丢失。"),
                format!(
                    "Deleting {name} removes the container. Data that is not persisted may be lost."
                ),
            ),
            text(
                "请输入容器名以确认删除。",
                "Enter the container name to confirm deletion.",
            ),
        ),
        ContainerAction::Start => (
            text("启动容器", "Start container"),
            text(format!("启动 {name}。"), format!("Start {name}.")),
            text(
                "启动完成后可以进入容器终端。",
                "Enter the container terminal after it starts.",
            ),
        ),
    }
}
