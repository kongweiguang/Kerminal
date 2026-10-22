//! 固定右栏的宿主管理与容器只读视图。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
        switch::Switch,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::operations::{
    ComposeAction, ContainerAction, ContainerRuntime, ContainerStatus,
};

use crate::theme::ThemeTokens;

use super::super::{
    ContainerPanelContext, ContainerPanelUnavailableReason, ContainerReadKind, OperationsView,
    state::{
        ContainerMutationRequest, ContainerRemoveIntent, ContainerStatusFilter,
        container_primary_action, filter_containers, group_container_projects_with_retained,
    },
};

/// 根据 Root 提供的类型化上下文绘制对应面板，绝不从主机选择状态推断容器目标。
///
/// @author kongweiguang
pub(super) fn render(
    view: &mut OperationsView,
    context: &ContainerPanelContext,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    match context {
        ContainerPanelContext::Unavailable { reason } => render_unavailable(*reason, theme),
        ContainerPanelContext::Host { .. } => render_host(view, theme, cx),
        ContainerPanelContext::ContainerInfo {
            target,
            terminal_running,
            ..
        } => render_container_info(view, target, *terminal_running, theme, cx),
    }
}

/// 不可用状态展示 Root 给出的具体原因，不尝试读取本机或切换到其它容器。
///
/// @author kongweiguang
fn render_unavailable(reason: ContainerPanelUnavailableReason, theme: ThemeTokens) -> AnyElement {
    let (zh, en) = reason.label();
    super::section_page(
        crate::locale::text("容器", "Containers"),
        div()
            .w_full()
            .min_h(px(90.))
            .flex()
            .items_center()
            .justify_center()
            .p_3()
            .text_size(theme.text_sm)
            .text_color(theme.muted)
            .child(crate::locale::text(zh, en)),
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 宿主模式只列当前 Root 绑定的 host/runtime；常用读取保持直达，写操作集中在菜单。
/// 删除确认展开行 ticket 冻结的完整目标，窄栏也能核对，且不从当前选择推导宿主。
///
/// @author kongweiguang
fn render_host(
    view: &mut OperationsView,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let ticket = view.container_action_ticket(None);
    let busy = view.busy;
    let runtime = view.runtime;
    let rows = view.containers.data().cloned().unwrap_or_default();
    let filtered = filter_containers(&rows, &view.container_search, view.container_status_filter);
    let retained = if view.container_search.trim().is_empty()
        && view.container_status_filter == ContainerStatusFilter::All
    {
        view.compose_projects.as_slice()
    } else {
        &[]
    };
    let groups = group_container_projects_with_retained(filtered.clone(), retained);
    let inputs = view
        .inputs
        .as_ref()
        .expect("inputs initialized before render");
    let mut docker = Button::new("container-panel-runtime-docker")
        .compact()
        .label("Docker")
        .on_click(cx.listener({
            let ticket = ticket.clone();
            move |view, _: &ClickEvent, _, cx| {
                view.request_container_panel_refresh(ContainerRuntime::Docker, &ticket, cx)
            }
        }));
    let mut podman = Button::new("container-panel-runtime-podman")
        .compact()
        .label("Podman")
        .on_click(cx.listener({
            let ticket = ticket.clone();
            move |view, _: &ClickEvent, _, cx| {
                view.request_container_panel_refresh(ContainerRuntime::Podman, &ticket, cx)
            }
        }));
    if runtime == ContainerRuntime::Docker {
        docker = docker.primary();
    } else {
        podman = podman.primary();
    }
    let mut body = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_1()
                .child(docker)
                .child(podman),
        )
        .child(Input::new(&inputs.container_search).w_full())
        .child(view.render_container_status_filters(cx))
        .child(super::section_feedback(
            &view.containers,
            view.containers.data().is_some() && filtered.is_empty() && retained.is_empty(),
            crate::locale::text("尚未读取容器列表。", "Container list has not been loaded."),
            theme,
        ));

    for (group_index, group) in groups.into_iter().enumerate() {
        if group.compose_group {
            let mut header = div().w_full().flex().flex_col().gap_1().pt_2().child(
                div()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(format!(
                        "{} · {}",
                        group.project.as_deref().unwrap_or("Compose"),
                        group.containers.len()
                    )),
            );
            if let Some(target) = group.target.as_ref() {
                header = header.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(format!(
                            "{} · {}",
                            target.working_dir,
                            target.config_files.join(", ")
                        )),
                );
                let mut actions = div().w_full().flex().flex_wrap().gap_1();
                for (action, label) in [
                    (ComposeAction::Up, crate::locale::text("启动", "Up")),
                    (ComposeAction::Stop, crate::locale::text("停止", "Stop")),
                    (
                        ComposeAction::Restart,
                        crate::locale::text("重启", "Restart"),
                    ),
                    (ComposeAction::Down, crate::locale::text("拆除…", "Down…")),
                ] {
                    let target = target.clone();
                    let ticket = ticket.clone();
                    actions = actions.child(
                        Button::new(format!("container-compose-{group_index}-{action:?}"))
                            .compact()
                            .label(label)
                            .disabled(busy)
                            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                view.mutate_compose(target.clone(), action, &ticket, cx)
                            })),
                    );
                }
                header = header.child(actions);
            }
            if group.containers.is_empty() {
                header = header.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "当前没有容器；仍可从 Compose 项目启动。",
                            "No containers remain; this Compose project can still be started.",
                        )),
                );
            }
            let mut config_actions = div().w_full().flex().flex_wrap().gap_1();
            let mut config_count = 0usize;
            for container in &group.containers {
                let Some(metadata) = container.compose.as_ref() else {
                    continue;
                };
                for path in &metadata.config_paths {
                    let host_id = container.target.host_id.clone();
                    let path = path.clone();
                    let working_directory = metadata.working_dir.clone();
                    let label = path.rsplit(['/', '\\']).next().unwrap_or(&path).to_owned();
                    let config_ticket = ticket.clone();
                    config_actions = config_actions.child(
                        Button::new(format!(
                            "container-compose-config-{group_index}-{config_count}"
                        ))
                        .compact()
                        .label(format!("YAML · {label}"))
                        .on_click(cx.listener(
                            move |view, _: &ClickEvent, _, cx| {
                                view.request_compose_config(
                                    host_id.clone(),
                                    path.clone(),
                                    working_directory.clone(),
                                    &config_ticket,
                                    cx,
                                )
                            },
                        )),
                    );
                    config_count += 1;
                }
            }
            if config_count > 0 {
                header = header.child(config_actions);
            }
            body = body.child(header);
        } else if !group.containers.is_empty() {
            body = body.child(
                div()
                    .w_full()
                    .pt_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text("独立容器", "Standalone containers")),
            );
        }

        for container in group.containers {
            let target = container.target.clone();
            let row_ticket = view.container_action_ticket(Some(target.clone()));
            let short_id = container.short_id.clone();
            let mut row = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(theme.sidebar)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_size(px(13.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .truncate()
                                .child(container.name.clone()),
                        )
                        .child(status_pill(
                            container.status_text.clone(),
                            container.status,
                            theme,
                        )),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .truncate()
                        .child(format!("{} · {}", container.image, container.short_id)),
                );
            let pending = view
                .container_pending_remove
                .as_ref()
                .filter(|intent| intent.target == target);
            let is_pending = pending.is_some();
            let force = pending.is_some_and(|intent| intent.force);
            let primary_action = container_primary_action(container.status);
            let mut actions = div().w_full().flex().flex_wrap().items_center().gap_1();
            if container.status == ContainerStatus::Running {
                let ticket = row_ticket.clone();
                let shell_target = target.clone();
                actions = actions.child(
                    Button::new(format!("container-enter-{short_id}"))
                        .compact()
                        .primary()
                        .label(crate::locale::text("进入终端", "Open terminal"))
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            view.request_container_exec(shell_target.clone(), &ticket, cx)
                        })),
                );
            }
            let ticket = row_ticket.clone();
            let log_target = target.clone();
            actions = actions.child(
                Button::new(format!("container-logs-{short_id}"))
                    .compact()
                    .label(crate::locale::text("日志", "Logs"))
                    .disabled(busy)
                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                        view.request_container_read(
                            log_target.clone(),
                            ContainerReadKind::Logs,
                            &ticket,
                            cx,
                        )
                    })),
            );
            let ticket = row_ticket.clone();
            let file_target = target.clone();
            actions = actions.child(
                Button::new(format!("container-files-{short_id}"))
                    .compact()
                    .label(crate::locale::text("文件", "Files"))
                    .disabled(busy)
                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                        view.request_container_files(file_target.clone(), &ticket, cx)
                    })),
            );
            let menu = Button::new(format!("container-more-{short_id}"))
                .compact()
                .ghost()
                .label(crate::locale::text("更多", "More"));
            let entity = cx.entity().clone();
            let menu_target = target.clone();
            let menu_ticket = row_ticket.clone();
            let status = container.status;
            let menu = menu.dropdown_menu(move |mut menu, _, _| {
                if let Some(action) = primary_action {
                    let target = menu_target.clone();
                    let ticket = menu_ticket.clone();
                    let entity = entity.clone();
                    let label = if action == ContainerAction::Stop {
                        crate::locale::text("停止", "Stop")
                    } else {
                        crate::locale::text("启动", "Start")
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, app| {
                        entity.update(app, |view, cx| {
                            view.request_container_mutation(
                                ContainerMutationRequest {
                                    target: target.clone(),
                                    action,
                                    force: false,
                                },
                                &ticket,
                                cx,
                            );
                        });
                    }));
                }
                if status == ContainerStatus::Running {
                    let target = menu_target.clone();
                    let ticket = menu_ticket.clone();
                    let entity = entity.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text("重启", "Restart")).on_click(
                            move |_, _, app| {
                                entity.update(app, |view, cx| {
                                    view.request_container_mutation(
                                        ContainerMutationRequest {
                                            target: target.clone(),
                                            action: ContainerAction::Restart,
                                            force: false,
                                        },
                                        &ticket,
                                        cx,
                                    );
                                });
                            },
                        ),
                    );
                }
                if !is_pending {
                    let target = menu_target.clone();
                    let ticket = menu_ticket.clone();
                    let entity = entity.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text("删除…", "Remove…")).on_click(
                            move |_, _, app| {
                                entity.update(app, |view, cx| {
                                    if view
                                        .container_action_ticket_is_current(&ticket, Some(&target))
                                        && view.container_mutation_allowed(&target)
                                    {
                                        view.container_pending_remove =
                                            Some(ContainerRemoveIntent::new(target.clone()));
                                        cx.notify();
                                    }
                                });
                            },
                        ),
                    );
                }
                menu
            });
            actions = actions.child(menu);
            row = row.child(actions);
            if is_pending {
                let confirmation_target =
                    row_ticket.target.clone().unwrap_or_else(|| target.clone());
                let host_label = match confirmation_target.host_id.as_ref() {
                    None => crate::locale::text("本机", "Local").to_string(),
                    Some(host_id) => {
                        let id = wrap_container_identifier(host_id.as_str());
                        view.host_display_names
                            .get(host_id)
                            .map(|name| name.to_string())
                            .filter(|name| !name.trim().is_empty())
                            .map_or_else(
                                || id.clone(),
                                |name| format!("{}\n{id}", wrap_container_identifier(&name)),
                            )
                    }
                };
                let runtime_label = match confirmation_target.runtime {
                    ContainerRuntime::Docker => "Docker",
                    ContainerRuntime::Podman => "Podman",
                };
                let id_display = wrap_container_identifier(&confirmation_target.container_id);
                row = row.child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .rounded_md()
                        .bg(theme.materials.chrome)
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(format!("主机：{host_label}"))
                        .child(format!("运行时：{runtime_label}"))
                        .child(div().w_full().child(format!("容器 ID：\n{id_display}"))),
                );
                let remove_toggle_target = confirmation_target.clone();
                let remove_confirm_target = confirmation_target.clone();
                let cancel_target = confirmation_target;
                let confirm_ticket = row_ticket.clone();
                let toggle_ticket = confirm_ticket.clone();
                let cancel_ticket = confirm_ticket.clone();
                row = row.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .rounded_md()
                        .bg(theme.materials.chrome)
                        .child(
                            Switch::new(format!("container-force-remove-{short_id}"))
                                .checked(force)
                                .label(crate::locale::text(
                                    "强制删除（会停止运行中的容器）",
                                    "Force remove (stops a running container)",
                                ))
                                .disabled(busy)
                                .on_change(cx.listener(move |view, enabled: &bool, _, cx| {
                                    if !view.container_action_ticket_is_current(
                                        &toggle_ticket,
                                        Some(&remove_toggle_target),
                                    ) {
                                        return;
                                    }
                                    if let Some(intent) = view
                                        .container_pending_remove
                                        .as_mut()
                                        .filter(|intent| intent.target == remove_toggle_target)
                                    {
                                        intent.force = *enabled;
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .gap_1()
                                .child(
                                    Button::new(format!("container-confirm-remove-{short_id}"))
                                        .compact()
                                        .danger()
                                        .label(crate::locale::text("确认删除", "Confirm remove"))
                                        .disabled(busy)
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                if !view.container_action_ticket_is_current(
                                                    &confirm_ticket,
                                                    Some(&remove_confirm_target),
                                                ) || !view
                                                    .container_pending_remove
                                                    .as_ref()
                                                    .is_some_and(|intent| {
                                                        intent.target == remove_confirm_target
                                                    })
                                                {
                                                    return;
                                                }
                                                if let Some(intent) =
                                                    view.container_pending_remove.take()
                                                {
                                                    view.request_container_mutation(
                                                        intent.confirm(),
                                                        &confirm_ticket,
                                                        cx,
                                                    );
                                                }
                                            },
                                        )),
                                )
                                .child(
                                    Button::new(format!("container-cancel-remove-{short_id}"))
                                        .compact()
                                        .label(crate::locale::text("取消", "Cancel"))
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                if view.container_action_ticket_is_current(
                                                    &cancel_ticket,
                                                    Some(&cancel_target),
                                                ) && view
                                                    .container_pending_remove
                                                    .as_ref()
                                                    .is_some_and(|intent| {
                                                        intent.target == cancel_target
                                                    })
                                                {
                                                    view.container_pending_remove = None;
                                                    cx.notify();
                                                }
                                            },
                                        )),
                                ),
                        ),
                );
            }
            body = body.child(row);
        }
    }

    super::section_page(
        crate::locale::text("宿主容器", "Host containers"),
        body,
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 容器终端只读页按当前分页读取唯一 target；正文可在窄栏收缩，终端退出仍保留原目标快照。
///
/// @author kongweiguang
fn render_container_info(
    view: &mut OperationsView,
    target: &kerminal_domain::operations::ContainerTarget,
    terminal_running: bool,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    let ticket = view.container_action_ticket(Some(target.clone()));
    let selected = view.container_info_tab;
    let mut tabs = div().w_full().min_w_0().flex().flex_wrap().gap_1();
    for (kind, zh, en, id) in [
        (ContainerReadKind::Inspect, "概览", "Overview", "overview"),
        (ContainerReadKind::Stats, "资源", "Resources", "resources"),
        (ContainerReadKind::Logs, "日志", "Logs", "logs"),
    ] {
        let target = target.clone();
        let ticket = ticket.clone();
        let button = Button::new(format!("container-info-tab-{id}"))
            .compact()
            .label(crate::locale::text(zh, en))
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.request_container_read(target.clone(), kind, &ticket, cx)
            }));
        tabs = tabs.child(if selected == kind {
            button.primary()
        } else {
            button.ghost()
        });
    }
    let full_container_id = target.container_id.clone();
    let target_id_tooltip = crate::locale::text(
        format!("容器 ID：{full_container_id}"),
        format!("Container ID: {full_container_id}"),
    );
    let target_id_accessible = target_id_tooltip.clone();
    let visible_container_prefix = target.container_id.chars().take(12).collect::<String>();
    let visible_container_id = if target.container_id.chars().count() > 12 {
        format!("{visible_container_prefix}…")
    } else {
        visible_container_prefix
    };
    let mut body = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(tabs)
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(theme.sidebar)
                .child(
                    div()
                        .id("container-info-target-id")
                        .w_full()
                        .min_w_0()
                        .text_size(px(13.))
                        .truncate()
                        .child(visible_container_id)
                        .tooltip(move |window, cx| {
                            Tooltip::new(target_id_tooltip.clone()).build(window, cx)
                        })
                        .aria_label(target_id_accessible),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(format!("{:?}", target.runtime)),
                ),
        );
    if !terminal_running {
        body = body.child(
            div()
                .w_full()
                .p_2()
                .rounded_md()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "容器终端已结束；此页仍绑定原容器，可刷新查看最后状态。",
                    "The container terminal has ended. This page remains bound to the same container and can still be refreshed.",
                )),
        );
    }

    match selected {
        ContainerReadKind::Inspect => {
            if view.container_inspect.phase() == super::super::DetailReadPhase::Loading {
                body = body.child(loading_label(theme));
            }
            if let Some(message) = view.container_inspect.feedback() {
                body = body.child(container_read_error_row(message, theme));
            }
            if view.container_inspect.data().is_some()
                && matches!(
                    view.container_inspect.phase(),
                    super::super::DetailReadPhase::Loading
                        | super::super::DetailReadPhase::Failed
                        | super::super::DetailReadPhase::Cancelled
                )
            {
                body = body.child(stale_snapshot_hint(theme));
            }
            if let Some(details) = view.container_inspect.data() {
                let details = details.clone();
                body = body.child(super::container_inspect(&details, theme));
                let copy = format!("{}\n{}\n{}", details.name, details.id, details.image);
                body = body.child(copy_button(copy, "container-info-copy-overview"));
            }
        }
        ContainerReadKind::Stats | ContainerReadKind::Logs => {
            if view.container_output.phase() == super::super::DetailReadPhase::Loading {
                body = body.child(loading_label(theme));
            }
            if let Some(message) = view.container_output.feedback() {
                body = body.child(container_read_error_row(message, theme));
            }
            if view.container_output.data().is_some()
                && matches!(
                    view.container_output.phase(),
                    super::super::DetailReadPhase::Loading
                        | super::super::DetailReadPhase::Failed
                        | super::super::DetailReadPhase::Cancelled
                )
            {
                body = body.child(stale_snapshot_hint(theme));
            }
            if let Some(output) = view.container_output.data() {
                let output = output.clone();
                let no_logs = selected == ContainerReadKind::Logs && output.trim().is_empty();
                body = body.child(
                    div()
                        .w_full()
                        .p_2()
                        .rounded_md()
                        .bg(theme.sidebar)
                        .text_size(theme.text_xs)
                        .text_color(if no_logs {
                            theme.muted
                        } else {
                            theme.foreground
                        })
                        .child(if no_logs {
                            crate::locale::text("暂无日志", "No logs available.")
                        } else {
                            output.clone().into()
                        }),
                );
                if !no_logs {
                    body = body.child(copy_button(output, "container-info-copy-output"));
                }
            }
        }
    }

    super::section_page(
        crate::locale::text("容器信息", "Container info"),
        body,
        theme,
        true,
        true,
    )
    .into_any_element()
}

/// 状态标签使用主题令牌并保留原始状态文字供用户识别外部变化。
///
/// @author kongweiguang
fn status_pill(text: String, status: ContainerStatus, theme: ThemeTokens) -> impl IntoElement {
    let color = match status {
        ContainerStatus::Running => theme.success,
        ContainerStatus::Exited | ContainerStatus::Dead => theme.error,
        _ => theme.muted,
    };
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .text_size(theme.text_xs)
        .text_color(color)
        .child(text)
}

/// 统一加载提示，避免不同分页使用不同语气或布局。
///
/// @author kongweiguang
fn loading_label(theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(crate::locale::text("正在读取…", "Loading…"))
}

/// 只读页错误使用主题错误色并在窄栏显式折行，避免完整原因被裁切后无法判断如何重试。
///
/// @author kongweiguang
fn container_read_error_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    let display_message = super::add_container_detail_soft_breaks(message);
    div()
        .w_full()
        .min_w_0()
        .p_2()
        .rounded_md()
        .bg(theme.sidebar)
        .text_size(theme.text_xs)
        .text_color(theme.error)
        .child(display_message)
}

/// 读取失败或取消时保留快照，但明确提示它不是本次读取的新结果。
///
/// @author kongweiguang
fn stale_snapshot_hint(theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(crate::locale::text(
            "下方保留最近一次成功读取的快照。",
            "Showing the last successful snapshot below.",
        ))
}

/// 长主机名和容器标识按字符分行，保证确认卡片与概览详情在窄栏可读且完整。
///
/// @author kongweiguang
pub(super) fn wrap_container_identifier(value: &str) -> String {
    value
        .chars()
        .collect::<Vec<_>>()
        .chunks(16)
        .map(|chunk| chunk.iter().copied().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 复制展示内容只操作当前快照，不要求额外读取或改变绑定目标。
///
/// @author kongweiguang
fn copy_button(value: String, id: &str) -> impl IntoElement {
    Button::new(id.to_owned())
        .compact()
        .ghost()
        .label(crate::locale::text("复制", "Copy"))
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(value.clone()));
        })
}
