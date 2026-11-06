//! 文件浏览器和传输任务的 GPUI 绘制。
//!
//! @author kongweiguang

pub(super) mod browser;
mod host_picker;
mod transfers;
mod tree;

use gpui_kit::{
    AnyElement, ClickEvent, Context, ElementId, InteractiveElement as _, IntoElement, Render,
    SharedString, StatefulInteractiveElement as _, Window, canvas,
    component::{
        Disableable as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        input::Input,
        resizable::{h_resizable, resizable_panel, v_resizable},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::{ContainerRuntime, FileEndpoint, TransferStatus};

use crate::{locale, theme::ThemeTokens};

use super::{FilesPane, NameAction, bookmark_endpoint, endpoint_name};

const FILES_WORKBENCH_MIN_COLUMN_WIDTH: f32 = 200.;
const FILES_WORKBENCH_STACK_THRESHOLD: f32 = FILES_WORKBENCH_MIN_COLUMN_WIDTH * 2. + 20.;

impl Render for FilesPane {
    /// 工作区按自身测量宽度选择布局；紧凑右栏缺少可信目标时隐藏传输操作，避免暴露其他工作区记录。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_name_input(window, cx);
        self.ensure_host_search_input(super::FilePaneSide::Left, window, cx);
        self.ensure_host_search_input(super::FilePaneSide::Right, window, cx);
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
        let empty_target = [0, 1].map(|side| {
            self.host_picker.enabled
                && self.host_picker.target_unselected[side]
                && !self.compact_layout
        });
        let browser_area = if self.compact_layout {
            if !self.compact_target_is_available() {
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
        } else {
            let pane_id = cx.entity_id().as_u64();
            let orientation = window.use_keyed_state(
                format!("files-workbench-orientation-{pane_id}"),
                cx,
                |_, _| false,
            );
            let stacked = *orientation.read(cx);
            let measurement = canvas(
                move |bounds, _, app| {
                    let next = bounds.size.width < px(FILES_WORKBENCH_STACK_THRESHOLD);
                    if *orientation.read(app) != next {
                        app.defer(move |app| {
                            orientation.update(app, |stacked, cx| {
                                *stacked = next;
                                cx.notify();
                            });
                        });
                    }
                },
                |_, (), _, _| {},
            )
            .absolute()
            .inset_0();
            let split_id = format!(
                "files-workbench-split-{pane_id}-{}",
                if stacked { "vertical" } else { "horizontal" }
            );
            let minimum_extent = if stacked {
                px(160.)
            } else {
                px(FILES_WORKBENCH_MIN_COLUMN_WIDTH)
            };
            let left_side = if empty_target[0] {
                host_picker::render_empty_target(self, 0, theme, window, cx)
            } else {
                browser::render_browser(self, 0, window, theme, cx).into_any_element()
            };
            let left_pane = resizable_panel()
                .size_range(minimum_extent..px(4_000.))
                .child(left_side);
            let right_side = if empty_target[1] {
                div()
                    .id("files-right-workbench")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(host_picker::render_empty_target(self, 1, theme, window, cx))
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
            let right_pane = resizable_panel()
                .size_range(minimum_extent..px(4_000.))
                .child(right_side);
            let split = if stacked {
                v_resizable(split_id).child(left_pane).child(right_pane)
            } else {
                h_resizable(split_id).child(left_pane).child(right_pane)
            };
            div()
                .id("files-browser-area-wide")
                .debug_selector(|| "files-browser-area-wide".to_owned())
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .flex()
                .px_1()
                .py_1()
                .relative()
                .overflow_hidden()
                .child(split)
                .child(measurement)
                .into_any_element()
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
        // 右栏在浏览状态下省去重复标题；传输台仍保留刷新入口。
        let show_header = self.host_picker.enabled;
        if show_header {
            pane = pane.child(render_header(self, theme, cx));
        }
        pane = pane
            .child(browser_area)
            .child(render_name_action(self, theme, cx));
        if !self.compact_layout {
            pane = pane.child(render_bookmarks(self, theme, cx));
        }
        // 传输区只展示当前任务和必要恢复操作，不暴露冷档入口。
        if !self.compact_layout || self.compact_target_is_available() {
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

/// 普通浏览标题留给端点和路径栏；仅传输台操作需要独立总栏。
/// 工作台刷新复用原图标的忙态并阻止连点，避免短暂文案和重复后台请求。
///
/// @author kongweiguang
fn render_header(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> impl IntoElement {
    let workbench = pane.host_picker.enabled;
    let title = if workbench {
        locale::text("传输台", "Transfers")
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
        .bg(theme.materials.chrome)
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
    header = header.child(div().flex_1());
    if workbench {
        let refreshing =
            pane.panels.iter().any(|panel| panel.loading) || pane.queue.recovery_list_loading;
        header = header.child(
            icon_button(
                "files-workbench-refresh",
                IconName::RefreshCw,
                locale::text("刷新文件与传输", "Refresh files and transfers"),
                refreshing,
            )
            .loading(refreshing)
            .loading_icon(IconName::RefreshCw)
            .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| pane.refresh_workbench(cx))),
        );
    }
    if !pane.compact_layout && !workbench {
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

/// 必要反馈可展开或主动关闭；普通成功由对象状态呈现，不产生额外底栏。
/// 目录错误从当前侧派生并提供原位重试，列表、树与保留旧清单的刷新失败共用同一入口。
///
/// @author kongweiguang
fn render_status(
    pane: &FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> Option<AnyElement> {
    let status = pane.visible_operation_status()?.to_owned();
    let side = if pane.compact_layout {
        1
    } else {
        pane.active_side
    };
    // 底栏重试只跟随尚未内联展示的目录错误；内联错误卡自带重试时不会再出现第二个入口。
    let directory_retry = if pane.directory_error_is_inline() {
        None
    } else {
        pane.panels[side].directory_error.clone()
    };
    let status_tooltip = status.clone();
    let show_details = pane.visible_operation_status().is_some_and(|status| {
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
            .children(directory_retry.map(|error| {
                Button::new("files-retry-directory")
                    .compact()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .label(locale::text("重试", "Retry"))
                    .accessibility_label(locale::text(
                        "重新读取当前目录",
                        "Retry current directory",
                    ))
                    .tooltip(error)
                    .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                        pane.retry_panel_load(side, cx);
                    }))
            }))
            .children(pane.status.as_ref().map(|_| {
                Button::new("files-dismiss-status")
                    .compact()
                    .ghost()
                    .icon(IconName::Close)
                    .accessibility_label(locale::text("关闭操作提示", "Dismiss operation message"))
                    .on_click(cx.listener(|pane, _: &ClickEvent, _, cx| {
                        pane.status = None;
                        pane.status_details_expanded = false;
                        cx.notify();
                    }))
            }))
            .into_any_element(),
    )
}

/// 将批量失败和取消详情放入有界滚动区域，避免窄栏状态栏只留下省略后的错误摘要。
/// 详情与摘要使用同一派生来源，目录错误不重复写入业务反馈状态。
///
/// @author kongweiguang
fn render_status_details(pane: &FilesPane, theme: ThemeTokens) -> Option<AnyElement> {
    if !pane.status_details_expanded {
        return None;
    }
    let status = pane.visible_operation_status()?;
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
            .child(status.to_owned())
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
