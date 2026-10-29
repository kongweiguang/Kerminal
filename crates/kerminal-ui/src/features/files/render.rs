//! 文件浏览器、预览和传输任务的 GPUI 绘制。
//!
//! @author kongweiguang

pub(super) mod browser;
mod host_picker;
mod permissions;
mod transfers;
mod tree;
mod workspace;

use gpui_kit::{
    AnyElement, ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement, Render,
    SharedString, StatefulInteractiveElement as _, Window,
    component::{
        Disableable as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        input::Input,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::{ContainerRuntime, FileEndpoint, TransferStatus};

use crate::{locale, theme::ThemeTokens};

use super::{FilesPane, NameAction, bookmark_endpoint, endpoint_name};

impl Render for FilesPane {
    /// 宽工作区保留 shell 材质以露出全局壁纸；紧凑右栏只投影 main 的工具面板色阶，避免改动共享 Kit 主题。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_name_input(window, cx);
        self.ensure_host_search_input(window, cx);
        self.permissions.ensure_input(window, cx);
        let visible_side = if self.compact_layout {
            1
        } else {
            self.active_side
        };
        let selected = self.panels[visible_side].selected.clone();
        self.permissions
            .sync_selection(selected.as_ref(), window, cx);
        self.initialize(cx);
        self.watch_transfer_queue(cx);
        let theme = ThemeTokens::current(window, cx);
        let theme = if self.compact_layout {
            theme.for_tool_panel()
        } else {
            theme
        };
        let empty_remote_target = self.host_picker.enabled
            && self.host_picker.right_target_unselected
            && !self.compact_layout;
        let browser_area = if self.compact_layout {
            if self.view_mode == super::view_mode::FilesViewMode::Workspace {
                workspace::render_workspace_overview(self, theme, cx)
            } else if !self.compact_target_is_available() {
                render_compact_no_terminal_target(theme)
            } else {
                div()
                    .id("files-browser-area-compact")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(browser::render_browser(self, 1, window, theme, cx))
                    .into_any_element()
            }
        } else if self.view_mode == super::view_mode::FilesViewMode::Workspace {
            workspace::render_workspace_overview(self, theme, cx)
        } else {
            let area = div()
                .id("files-browser-area-wide")
                .flex_1()
                .min_h(px(0.))
                .flex()
                .gap_1()
                .px_1()
                .py_1()
                .overflow_hidden()
                .child(browser::render_browser(self, 0, window, theme, cx));
            let right_side = if empty_remote_target {
                div()
                    .id("files-right-workbench")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(host_picker::render_empty_target(self, theme, window, cx))
                    .into_any_element()
            } else {
                div()
                    .id("files-right-workbench")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(browser::render_browser(self, 1, window, theme, cx))
                    .into_any_element()
            };
            area.child(right_side).into_any_element()
        };
        let mut pane = div()
            .id("files-pane")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(if self.compact_layout {
                theme.tool_panel_surface
            } else {
                theme.materials.shell
            })
            .text_color(theme.foreground);
        // 右栏在浏览状态下省去重复标题；工作区状态仍保留返回入口。
        let show_header = self.host_picker.enabled
            || self.view_mode == super::view_mode::FilesViewMode::Workspace;
        if show_header {
            pane = pane.child(render_header(self, theme, cx));
        }
        pane = pane
            .child(browser_area)
            .child(render_name_action(self, theme, cx))
            .child(render_selection_details(self, theme, cx));
        if !self.compact_layout {
            pane = pane.child(render_bookmarks(self, theme, cx));
        }
        let empty_compact_queue = self.compact_layout
            && self.queue.archive_loaded
            && self.transfer_rows.is_empty()
            && self.transfer_recovery_rows.is_empty()
            && self.queue.archive_records.is_empty()
            && !self.queue.archive_loading
            && self.queue.archive_error.is_none()
            && !self.queue.recovery_list_loading
            && self.queue.recovery_list_error.is_none()
            && !self.queue.history_expanded
            && !self.queue.is_clearing_completed_history();
        // 仅在已确认没有可浏览的历史或恢复材料时省去右栏空队列；加载与错误仍可见。
        if !empty_compact_queue {
            pane = pane.child(transfers::render_transfers(self, theme, cx));
        }
        if let Some(details) = render_status_details(self, theme) {
            pane = pane.child(details);
        }
        if let Some(status) = render_status(self, theme, cx) {
            pane = pane.child(status);
        }
        pane
    }
}

/// 普通浏览标题留给端点和路径栏；仅工作台操作或工作区返回入口需要独立总栏。
///
/// @author kongweiguang
fn render_header(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    let workbench = pane.host_picker.enabled;
    let workspace = pane.view_mode == super::view_mode::FilesViewMode::Workspace;
    let title = if workbench {
        locale::text("SFTP 传输", "SFTP Transfer")
    } else {
        locale::text("文件", "Files")
    };
    let mut header = div()
        .w_full()
        .h(px(34.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.materials.chrome);
    if !workspace || workbench {
        header = header
            .child(
                div()
                    .text_color(theme.accent)
                    .text_size(theme.text_sm)
                    .child(if workbench {
                        Icon::new(gpui_kit::assets::IconName::ArrowLeftRight)
                    } else {
                        Icon::new(gpui_kit::assets::IconName::FolderOpen)
                    }),
            )
            .child(
                div()
                    .text_size(theme.text_xs)
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(title),
            );
    }
    if workspace {
        header = header.child(
            Button::new("files-toggle-workspace-overview")
                .compact()
                .ghost()
                .text_size(theme.text_xs)
                .label(locale::text("返回文件", "Back to files"))
                .on_click(
                    cx.listener(|pane, _: &ClickEvent, _, cx| pane.toggle_workspace_overview(cx)),
                ),
        );
    }
    header = header.child(div().flex_1());
    if workbench {
        let clearable = transfers::clearable_history_count(&pane.transfer_rows);
        let clearing = pane.queue.is_clearing_completed_history();
        header = header
            .child(
                icon_button(
                    "files-workbench-refresh",
                    IconName::RefreshCw,
                    locale::text("刷新文件与传输", "Refresh files and transfers"),
                    false,
                )
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.refresh_workbench(cx))),
            )
            .child(
                asset_icon_button(
                    "files-workbench-clear-completed",
                    gpui_kit::assets::IconName::Trash,
                    locale::text("清理已完成传输", "Clear completed transfers"),
                    clearable == 0 || clearing,
                )
                .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                    pane.clear_completed_transfer_history(cx)
                })),
            );
    }
    if !pane.compact_layout && !workbench && !workspace {
        header = header.child(
            div()
                .min_w(px(0.))
                .flex_1()
                .overflow_hidden()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .text_right()
                .text_ellipsis()
                .child(endpoint_pair_label(pane)),
        );
    }
    header
}

/// 紧凑右栏没有可信的本机或 SSH pane 时显示引导，不回退成浏览宿主本机目录。
///
/// @author kongweiguang
fn render_compact_no_terminal_target(theme: ThemeTokens) -> AnyElement {
    div()
        .id("files-no-current-terminal-target")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .px_3()
        .text_color(theme.muted)
        .child(IconName::FolderOpen)
        .child(
            div()
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .child(locale::text(
                    "当前终端没有可用的文件目标",
                    "The current terminal has no available file target",
                )),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(locale::text(
                    "聚焦已就绪的本机或 SSH 终端即可浏览其文件。",
                    "Focus a ready local or SSH terminal to browse its files.",
                )),
        )
        .into_any_element()
}

/// 紧凑右栏的新建和重命名由 Root 弹框承载，避免相同编辑状态在浏览器底部重复显示。
///
/// @author kongweiguang
fn render_name_action(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    if pane.compact_layout {
        return div().id("files-name-action").w_full().h(px(0.));
    }

    let mut row = div()
        .id("files-name-action")
        .w_full()
        .h(px(34.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel);
    if pane.name_action.is_none() {
        return row.h(px(0.));
    }
    if let Some(input) = pane.name_input.as_ref() {
        row = row.child(Input::new(input).flex_1());
    }
    row.child(
        Button::new("files-apply-name")
            .compact()
            .primary()
            .label(match pane.name_action {
                Some(NameAction::CreateDirectory) => locale::text("创建", "Create"),
                Some(NameAction::Rename) => locale::text("重命名", "Rename"),
                None => locale::text("应用", "Apply"),
            })
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.commit_name_action(cx))),
    )
    .child(
        icon_button(
            "files-cancel-name",
            IconName::Close,
            locale::text("取消名称编辑", "Cancel name edit"),
            false,
        )
        .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
            pane.name_action = None;
            cx.notify();
        })),
    )
}

/// 属性和预览跟随当前焦点条目展示，批量勾选语义仍由目录工具栏独立处理。
///
/// @author kongweiguang
fn render_selection_details(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let side = if pane.compact_layout {
        1
    } else {
        pane.active_side
    };
    if pane.panels[side].selected.is_none() {
        return div().h(px(0.)).into_any_element();
    }
    let details = div()
        .id("files-selection-details-scroll")
        .w_full()
        .flex_shrink_0()
        .px_2()
        .pb_1()
        .gap_1();
    if pane.compact_layout {
        details
            .flex()
            .flex_col()
            .max_h(px(136.))
            .overflow_y_scroll()
            .child(permissions::render_permissions(pane, theme, cx))
            .child(render_preview(pane, theme))
            .into_any_element()
    } else {
        details
            .flex()
            .items_stretch()
            .child(permissions::render_permissions(pane, theme, cx))
            .child(render_preview(pane, theme))
            .into_any_element()
    }
}

/// 文件预览使用有界片段，未选中或尚未完成时不保留空白大面板。
///
/// @author kongweiguang
fn render_preview(pane: &FilesPane, theme: ThemeTokens) -> AnyElement {
    let Some(preview) = &pane.preview else {
        return div().h(px(0.)).into_any_element();
    };
    let summary = if preview.binary {
        locale::text("二进制文件 · 不显示正文", "Binary file · contents hidden").to_string()
    } else {
        let text = String::from_utf8_lossy(&preview.bytes);
        let suffix = if preview.truncated {
            locale::text("\n…预览已截断", "\n…preview truncated").to_string()
        } else {
            String::new()
        };
        format!("{text}{suffix}")
    };
    div()
        .id("files-preview")
        .flex_1()
        .min_w(px(0.))
        .h(px(48.))
        .w_full()
        .px_3()
        .py_2()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_size(theme.text_xs)
        .text_color(theme.foreground)
        .child(summary)
        .into_any_element()
}

/// 当前 endpoint 作用域的书签作为快速路径入口显示；12px 标签不被压缩，窄栏通过横向滚动保留入口。
/// 书签导航留在当前面板，避免恢复后的左侧远端目录被误送到右侧。
///
/// @author kongweiguang
fn render_bookmarks(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    if pane.bookmarks.is_empty() {
        return div().h(px(0.)).into_any_element();
    }
    let side = if pane.compact_layout {
        1
    } else {
        pane.active_side
    };
    let current_endpoint = pane.panels[side].endpoint.clone();
    let mut row = div()
        .id("files-bookmarks")
        .w_full()
        .h(px(28.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .overflow_x_scroll()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.secondary);
    row = row.child(
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .child(IconName::Star)
            .child(locale::text("书签", "Bookmarks")),
    );
    for (index, bookmark) in pane.bookmarks.iter().enumerate() {
        let Some(endpoint) = bookmark_endpoint(&current_endpoint, bookmark) else {
            continue;
        };
        let label = bookmark
            .label
            .clone()
            .unwrap_or_else(|| bookmark.path.clone());
        row = row.child(
            Button::new(format!("files-bookmark-path-{index}"))
                .compact()
                .flex_shrink_0()
                .ghost()
                .text_size(theme.text_xs)
                .label(label)
                .tooltip(bookmark.path.clone())
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    pane.open_endpoint_on_side(side, endpoint.clone(), cx)
                })),
        );
    }
    row.into_any_element()
}

/// 仅存在实际操作反馈时显示底栏；空闲状态不占用工作区高度，长错误仍可展开查看。
///
/// @author kongweiguang
fn render_status(
    pane: &FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> Option<AnyElement> {
    let status = pane.status.clone()?;
    let status_tooltip = status.clone();
    let show_details = pane.status.as_deref().is_some_and(|status| {
        pane.status_details_expanded
            || status.len() > 36
            || status.contains('\n')
            || status.contains("未入队")
            || status.contains("Not queued")
            || status.contains("失败项")
            || status.contains("Failed items")
    });
    let expanded = pane.status_details_expanded;
    let details_button = show_details.then(|| {
        Button::new("files-status-details")
            .compact()
            .ghost()
            .label(if expanded {
                locale::text("收起", "Hide")
            } else {
                locale::text("详情", "Details")
            })
            .accessibility_label(if expanded {
                locale::text("收起完整操作详情", "Hide full operation details")
            } else {
                locale::text("展开完整操作详情", "Show full operation details")
            })
            .tab_stop(true)
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                pane.status_details_expanded = !pane.status_details_expanded;
                cx.notify();
            }))
    });
    Some(
        div()
            .w_full()
            .min_h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_size(theme.text_xs)
            .text_color(theme.subtle)
            .child(IconName::Info)
            .child(
                div()
                    .id("files-operation-status-summary")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .tooltip(move |window, cx| {
                        Tooltip::new(status_tooltip.clone()).build(window, cx)
                    })
                    .child(status),
            )
            .children(details_button)
            .into_any_element(),
    )
}

/// 将批量失败和取消详情放入有界滚动区域，避免窄栏状态栏只留下省略后的错误摘要。
///
/// @author kongweiguang
fn render_status_details(pane: &FilesPane, theme: ThemeTokens) -> Option<AnyElement> {
    if !pane.status_details_expanded {
        return None;
    }
    let status = pane.status.as_ref()?;
    Some(
        div()
            .id("files-status-details-panel")
            .w_full()
            .max_h(px(144.))
            .px_3()
            .py_2()
            .overflow_y_scroll()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.materials.panel)
            .text_size(theme.text_sm)
            .text_color(theme.foreground)
            .role(gpui_kit::Role::Region)
            .aria_label(locale::text("完整操作结果", "Full operation result"))
            .child(status.clone())
            .into_any_element(),
    )
}

/// 文件工作区的图标按钮统一提供 tooltip、无障碍名称和紧凑点击尺寸。
///
/// @author kongweiguang
pub(super) fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: SharedString,
    disabled: bool,
) -> Button {
    icon_button_without_tooltip(id, icon, label.clone(), disabled).tooltip(label)
}

/// 复用通用按钮语义并保留文件区现有几何；立即卸载的触发器仍由调用方管理元素生命周期提示。
///
/// @author kongweiguang
pub(super) fn icon_button_without_tooltip(
    id: impl Into<ElementId>,
    icon: IconName,
    label: SharedString,
    disabled: bool,
) -> Button {
    crate::ui_button::icon_button(id, icon, label)
        .with_size(gpui_kit::component::Size::Medium)
        .w(px(28.))
        .h(px(26.))
        .disabled(disabled)
}

/// 完整 Kit 图标与兼容图标共用相同构造路径，保留文件区既有尺寸和提示生命周期。
///
/// @author kongweiguang
fn asset_icon_button(
    id: impl Into<ElementId>,
    icon: gpui_kit::assets::IconName,
    label: SharedString,
    disabled: bool,
) -> Button {
    crate::ui_button::icon_button(id, icon, label.clone())
        .with_size(gpui_kit::component::Size::Medium)
        .w(px(28.))
        .h(px(26.))
        .disabled(disabled)
        .tooltip(label)
}

/// 顶栏简要说明左右目标类型，容器运行时用产品名称而不是枚举调试格式。
///
/// @author kongweiguang
fn endpoint_pair_label(pane: &FilesPane) -> String {
    format!(
        "{}  ↔  {}",
        endpoint_kind_label(&pane.panels[0].endpoint),
        endpoint_kind_label(&pane.panels[1].endpoint)
    )
}

/// 双语显示 endpoint 类别，不把当前路径或一次性认证细节挤进主标题。
///
/// @author kongweiguang
fn endpoint_kind_label(endpoint: &FileEndpoint) -> String {
    match endpoint {
        FileEndpoint::Local { .. } => locale::text("本机", "Local").to_string(),
        FileEndpoint::Remote { host_id, .. } => format!("SSH · {host_id}"),
        FileEndpoint::Container {
            runtime,
            container_id,
            ..
        } => format!("{} · {}", container_runtime_label(*runtime), container_id),
    }
}

/// 显示稳定的产品运行时名称，不让 Rust Debug 表示渗入面向用户的标签。
///
/// @author kongweiguang
fn container_runtime_label(runtime: ContainerRuntime) -> gpui_kit::SharedString {
    match runtime {
        ContainerRuntime::Docker => locale::text("Docker", "Docker"),
        ContainerRuntime::Podman => locale::text("Podman", "Podman"),
    }
}

/// 统一队列状态的双语短标签，协议状态不直接泄露给界面。
///
/// @author kongweiguang
fn transfer_status(status: TransferStatus) -> gpui_kit::SharedString {
    match status {
        TransferStatus::Queued => locale::text("等待", "Queued"),
        TransferStatus::Connecting => locale::text("连接中", "Connecting"),
        TransferStatus::Transferring => locale::text("传输中", "Transferring"),
        TransferStatus::WaitingForRecovery => locale::text("等待恢复", "Waiting to recover"),
        TransferStatus::Committing => locale::text("提交中", "Committing"),
        TransferStatus::Cancelling => locale::text("取消中", "Cancelling"),
        TransferStatus::Succeeded => locale::text("已完成", "Completed"),
        TransferStatus::Unchanged => locale::text("无变更", "No changes"),
        TransferStatus::Skipped => locale::text("已跳过", "Skipped"),
        TransferStatus::Failed => locale::text("失败", "Failed"),
        TransferStatus::Cancelled => locale::text("已取消", "Cancelled"),
    }
}

/// 以 KiB 进位格式化已确认字节数，未知总量不显示伪进度百分比。
///
/// @author kongweiguang
fn format_size(size: Option<u64>) -> String {
    match size {
        Some(bytes) if bytes >= 1024 * 1024 => {
            format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
        }
        Some(bytes) if bytes >= 1024 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        Some(bytes) => format!("{bytes} B"),
        None => String::new(),
    }
}
