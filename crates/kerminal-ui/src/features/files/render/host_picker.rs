//! 共享 SFTP 工作区复用通用主机列表，端点选择和信任流程仍由 Root 管理。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    AnyElement, Context, Focusable as _, IntoElement, MouseButton, StyleRefinement, Window,
    assets::IconName,
    base::ElementExt as _,
    component::button::{Button, ButtonVariants as _},
    component::input::{Enter, Escape, Input, InputState},
    component::{Disableable, FocusableExt as _, Sizable as _, spinner::Spinner},
    div,
    prelude::*,
    px, relative,
};
use kerminal_domain::{files::FileEndpoint, identity::HostId};

use crate::{
    features::host_picker::{
        HostPickerExternalKey, HostPickerGroup, HostPickerList, HostPickerListKeyboardState,
        HostPickerListMessage, HostPickerOption, HostPickerPopoverConfig,
        HostPickerSearchProjection, grouped_host_picker_options, handle_external_host_picker_key,
        host_picker_popover,
    },
    locale,
    theme::ThemeTokens,
};

use super::super::{FilePaneSide, FilesPane};

/// 本机与保存主机共用列表，但只有远端身份需要交给 Root 预检。
#[derive(Clone)]
enum SftpHostPickerTarget {
    Local,
    Remote(HostId),
}

/// 冻结 SFTP 目录和搜索状态，使共享列表展示与当前 FilesPane owner 保持一致。
///
/// @author kongweiguang
struct SftpHostPickerListProjection {
    pane: gpui_kit::Entity<FilesPane>,
    side: FilePaneSide,
    search_input: gpui_kit::Entity<gpui_kit::component::input::InputState>,
    keyboard_state: gpui_kit::Entity<HostPickerListKeyboardState>,
    options: Vec<HostPickerOption<SftpHostPickerTarget>>,
    groups: Vec<HostPickerGroup>,
    query: String,
    loading: bool,
    show_loading_message: bool,
    error: Option<String>,
    has_remote_options: bool,
    theme: ThemeTokens,
}

/// 每侧标签带独立保存滚动和上次显露原因，普通重绘不会抢走用户手动滚动的位置。
#[derive(Default)]
struct HostTabStripState {
    scroll: gpui_kit::ScrollHandle,
    last_active: Option<(u64, usize)>,
    last_focused: Option<(u64, usize)>,
    viewport_width: Option<f32>,
    scale_factor: Option<f32>,
}

/// 宽栏保持紧凑搜索框，窄栏让输入随外层收缩；连接状态不额外占用横向空间。
///
/// @author kongweiguang
fn render_trigger(
    input: &gpui_kit::Entity<InputState>,
    side: FilePaneSide,
    pending: bool,
    theme: ThemeTokens,
) -> Input {
    let index = side.index();
    Input::new(input)
        .id(format!("files-sftp-host-picker-input-{index}"))
        .prefix(IconName::Search)
        .w_full()
        .min_w(px(0.))
        .h(px(28.))
        .text_size(theme.text_xs)
        .when(pending, |input| {
            input.suffix(Spinner::new().with_size(px(12.)).color(theme.muted))
        })
        .aria_label(match side {
            FilePaneSide::Left => locale::text("添加左侧服务器", "Add a left-side host"),
            FilePaneSide::Right => locale::text("添加右侧服务器", "Add a right-side host"),
        })
}

/// 主机标签由外框统一绘制焦点边界（包含关闭按钮），并按视口约束、选择和焦点显露。
/// 关闭按钮保留独立键盘焦点；刷新消息与键盘行号使用同一过滤结果，避免列表跳动和索引错位。
///
/// @author kongweiguang
pub(super) fn render_selector_header(
    pane: &mut FilesPane,
    side: usize,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let Some(side) = FilePaneSide::from_index(side) else {
        return div()
            .id("files-sftp-host-picker-invalid-side")
            .into_any_element();
    };
    let index = side.index();
    let Some(search_input) = pane.ensure_host_search_input(side, window, cx) else {
        return div()
            .id("files-sftp-host-picker-disabled")
            .into_any_element();
    };
    let focus_handle = search_input.read(cx).focus_handle(cx);
    let (options, query, loading, error, has_remote_options) = {
        let picker = &pane.host_picker;
        let mut options = vec![HostPickerOption {
            key: "local-device".into(),
            target: SftpHostPickerTarget::Local,
            title: locale::text("本机", "This device"),
            detail: None,
            group_id: Some("__kerminal_local_device__".to_owned()),
        }];
        options.extend(
            picker
                .hosts
                .iter()
                .map(|host| HostPickerOption {
                    key: host.id.as_str().to_owned().into(),
                    target: SftpHostPickerTarget::Remote(host.id.clone()),
                    title: host.name.clone().into(),
                    detail: Some(
                        format!(
                            "{} · {}@{}:{}",
                            host.protocol_label(),
                            host.username,
                            host.address,
                            host.port
                        )
                        .into(),
                    ),
                    group_id: host.group_id.clone(),
                })
                .collect::<Vec<_>>(),
        );
        (
            options,
            picker.query[index].clone(),
            picker.loading,
            picker.error.clone(),
            !picker.hosts.is_empty(),
        )
    };
    let mut groups = vec![HostPickerGroup {
        id: "__kerminal_local_device__".to_owned(),
        title: locale::text("本地", "Local"),
    }];
    groups.extend(pane.host_picker.groups.clone());
    let show_loading_message =
        loading && grouped_host_picker_options(&options, &groups, &query).is_empty();
    let keyboard_state = pane.ensure_host_keyboard_state(side, cx);
    let trigger_input = search_input.clone();
    let key_input = search_input.clone();
    let key_state = keyboard_state.clone();
    let key_options = options.clone();
    let key_groups = groups.clone();
    let key_query = query.clone();
    let key_message = if loading {
        show_loading_message
    } else {
        error.is_some() || (!has_remote_options && query.trim().is_empty())
    };
    let confirm_input = key_input.clone();
    let confirm_state = key_state.clone();
    let confirm_options = key_options.clone();
    let confirm_groups = key_groups.clone();
    let confirm_query = key_query.clone();
    let key_owner = cx.entity().clone();
    let confirm_owner = key_owner.clone();
    let open_owner = key_owner.clone();
    let escape_owner = key_owner.clone();
    let escape_input = trigger_input.clone();
    let expanded = pane.host_picker.expanded_side == Some(side);
    let owner = cx.entity().clone();
    let picker_content =
        move |_: &mut gpui_kit::component::popover::PopoverState,
              _: &mut Window,
              _: &mut Context<gpui_kit::component::popover::PopoverState>| {
            sftp_host_picker_list(SftpHostPickerListProjection {
                pane: owner.clone(),
                side,
                search_input: search_input.clone(),
                keyboard_state: keyboard_state.clone(),
                options: options.clone(),
                groups: groups.clone(),
                query: query.clone(),
                loading,
                show_loading_message,
                error: error.clone(),
                has_remote_options,
                theme,
            })
        };
    let header_owner = cx.entity().clone();
    let pane_id = cx.entity_id().as_u64();
    let strip_state = window
        .use_keyed_state(
            format!("files-host-strip-scroll-{pane_id}-{index}"),
            cx,
            |_, _| Rc::new(RefCell::new(HostTabStripState::default())),
        )
        .read(cx)
        .clone();
    let scroll = strip_state.borrow().scroll.clone();
    let max_tab_width = strip_state.borrow().viewport_width.map(|width| {
        // 百分比宽度与视口边缘分别按设备像素取整，两端各留一像素可避免标签略宽于视口。
        let pixel_margin = 2. / window.scale_factor().max(f32::EPSILON);
        px((width - pixel_margin).max(0.))
    });
    let active = pane.host_picker.host_tabs.active[index];
    let mut active_item = None;
    let mut focused_item = None;
    let tab_focus_handles = Rc::new(
        pane.host_picker.host_tabs.sides[index]
            .iter()
            .map(|tab| {
                let handle = window
                    .use_keyed_state(
                        format!("files-host-tab-focus-{pane_id}-{index}-{}", tab.key()),
                        cx,
                        |_, cx| cx.focus_handle(),
                    )
                    .read(cx)
                    .clone();
                (tab.id, handle)
            })
            .collect::<Vec<_>>(),
    );
    let mut tabs = div()
        .id(format!("files-host-tabs-{index}"))
        .debug_selector(move || format!("files-host-tabs-{index}"))
        .size_full()
        .flex()
        .items_center()
        .gap_1()
        .overflow_x_scroll()
        .track_scroll(&scroll);
    for (position, tab) in pane.host_picker.host_tabs.sides[index].iter().enumerate() {
        let target = tab.display_endpoint(&pane.panels[index]).clone();
        let tab_id = tab.id;
        let key = tab.key();
        let selected = active == Some(tab_id);
        if selected {
            active_item = Some((tab_id, position));
        }
        let title = match &target {
            FileEndpoint::Local { .. } => locale::text("本机", "This device").to_string(),
            FileEndpoint::Remote { host_id, .. } => pane
                .host_picker
                .hosts
                .iter()
                .find(|host| &host.id == host_id)
                .map(|host| host.name.clone())
                .unwrap_or_else(|| pane.target_names.display(&target).compact),
            _ => pane.target_names.display(&target).compact,
        };
        let mut detail = pane.target_names.endpoint_detail(&target);
        if !detail.contains(&title) {
            detail = format!("{title} · {detail}");
        }
        let activate_selector = format!("files-host-tab-{index}-{key}");
        let close_selector = format!("files-host-tab-close-{index}-{key}");
        let activate_debug_selector = activate_selector.clone();
        let close_debug_selector = close_selector.clone();
        let activate_focus = tab_focus_handles[position].1.clone();
        let close_focus = window
            .use_keyed_state(
                format!("files-host-tab-close-focus-{pane_id}-{index}-{key}"),
                cx,
                |_, cx| cx.focus_handle(),
            )
            .read(cx)
            .clone();
        if activate_focus.is_focused(window) || close_focus.is_focused(window) {
            focused_item = Some((tab_id, position));
        }
        let activate_on_click = activate_focus.clone();
        let mut activate = Button::new(activate_selector.clone())
            .compact()
            .ghost()
            .focus_ring(false)
            .max_w(px(180.))
            .min_w(px(0.))
            .flex_shrink_1()
            .text_size(theme.text_xs)
            .label(title.clone())
            .tooltip(detail)
            .accessibility_label(title.clone())
            .focus_handle(&activate_focus)
            .on_click(cx.listener(move |pane, _, window, cx| {
                pane.activate_host_tab(side, tab_id, cx);
                window.focus(&activate_on_click, cx);
            }));
        if matches!(target, FileEndpoint::Local { .. }) {
            activate = activate.icon(gpui_kit::component::Icon::new(IconName::HardDrive));
        }
        let chip_selector = format!("files-host-tab-frame-{index}-{key}");
        let chip_debug_selector = chip_selector.clone();
        let mut chip = div()
            .id(chip_selector)
            .debug_selector(move || chip_debug_selector)
            .when_some(max_tab_width, |chip, width| chip.max_w(width))
            .when(max_tab_width.is_none(), |chip| chip.max_w(relative(1.)))
            .flex_shrink_0()
            .flex()
            .items_center()
            .rounded(theme.control_radius)
            .border_1()
            .border_color(theme.border.opacity(0.))
            .when(activate_focus.is_focused(window), |chip| {
                chip.border_color(theme.accent)
            })
            .when(selected, |chip| chip.bg(theme.selected))
            .child(
                div()
                    .min_w(px(0.))
                    .flex_shrink_1()
                    .debug_selector(move || activate_debug_selector)
                    .child(activate),
            );
        if !(side == FilePaneSide::Left && tab_id == 0) {
            let close_label = locale::text(format!("关闭 {title}"), format!("Close {title}"));
            let close_on_click = close_focus.clone();
            let removed_tab_focus = activate_focus.clone();
            let remaining_focus_handles = tab_focus_handles.clone();
            chip = chip.child(
                div()
                    .flex_shrink_0()
                    .debug_selector(move || close_debug_selector)
                    .child(
                        Button::new(close_selector)
                            .compact()
                            .ghost()
                            .icon(gpui_kit::component::IconName::Close)
                            .accessibility_label(close_label.clone())
                            .tooltip(close_label)
                            .focus_handle(&close_focus)
                            .on_click(cx.listener(move |pane, _, window, cx| {
                                let restore_focus = close_on_click.is_focused(window)
                                    || removed_tab_focus.is_focused(window);
                                pane.close_host_tab(side, tab_id, cx);
                                if restore_focus {
                                    let active = pane.host_picker.host_tabs.active[index];
                                    if let Some((_, handle)) = remaining_focus_handles
                                        .iter()
                                        .find(|(id, _)| Some(*id) == active)
                                    {
                                        window.focus(handle, cx);
                                    } else if let Some(input) =
                                        pane.host_picker.search_inputs[index].as_ref()
                                    {
                                        window.focus(&input.read(cx).focus_handle(cx), cx);
                                    }
                                }
                            })),
                    ),
            );
        }
        tabs = tabs.child(chip);
    }
    {
        let mut state = strip_state.borrow_mut();
        let reveal = if state.last_focused != focused_item && focused_item.is_some() {
            focused_item
        } else if state.last_active != active_item {
            active_item
        } else {
            None
        };
        state.last_active = active_item;
        state.last_focused = focused_item;
        if let Some((_, position)) = reveal {
            state.scroll.scroll_to_item(position);
        }
    }
    let viewport_state = strip_state.clone();
    let viewport_owner = cx.entity().downgrade();
    let tabs_viewport = div()
        .relative()
        .flex_1()
        .min_w(px(80.))
        .h_full()
        .child(tabs)
        .on_prepaint(move |bounds, window, app| {
            let width = bounds.size.width.as_f32();
            let scale_factor = window.scale_factor();
            let changed = {
                let mut state = viewport_state.borrow_mut();
                let previous_width = state.viewport_width.replace(width);
                let previous_scale = state.scale_factor.replace(scale_factor);
                width > 0.
                    && (previous_width.is_none_or(|previous| (width - previous).abs() > 0.25)
                        || previous_scale.is_none_or(|previous| previous != scale_factor))
            };
            if changed {
                app.defer(move |app| {
                    let _ = viewport_owner.update(app, |pane, cx| {
                        if !pane.host_picker.enabled {
                            return;
                        }
                        let focused = viewport_state.borrow().last_focused.map(|(id, _)| id);
                        let tabs = &pane.host_picker.host_tabs.sides[index];
                        let position = focused
                            .and_then(|id| tabs.iter().position(|tab| tab.id == id))
                            .or_else(|| {
                                pane.host_picker.host_tabs.active[index]
                                    .and_then(|id| tabs.iter().position(|tab| tab.id == id))
                            });
                        if let Some(position) = position {
                            viewport_state.borrow().scroll.scroll_to_item(position);
                            cx.notify();
                        }
                    });
                });
            }
        });
    let trigger_selector = format!("files-sftp-host-picker-toggle-{index}");
    div()
        .id(format!("files-host-strip-{index}"))
        .debug_selector(move || format!("files-host-strip-{index}"))
        .w_full()
        .h(px(38.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(tabs_viewport)
        .child(
            div()
                .w(px(148.))
                .min_w(px(56.))
                .max_w(px(148.))
                .flex_shrink_1()
                .debug_selector(move || trigger_selector)
                .key_context(crate::features::input_composition::PICKER_INPUT_CONTEXT)
                .capture_any_mouse_down(move |event, window, app| {
                    if event.button == MouseButton::Left {
                        open_owner.update(app, |pane, cx| {
                            pane.set_host_picker_open(side, true, window, cx);
                        });
                    }
                })
                .capture_action(move |_: &Escape, window, app| {
                    if !expanded {
                        return;
                    }
                    if escape_input.read(app).is_composing() {
                        crate::features::input_composition::cancel_input_composition(
                            &escape_input,
                            window,
                            app,
                        );
                    } else {
                        escape_owner.update(app, |pane, cx| {
                            pane.set_host_picker_open(side, false, window, cx);
                        });
                    }
                    window.prevent_default();
                    app.stop_propagation();
                })
                .capture_action(move |_: &Enter, window, app| {
                    if !expanded {
                        return;
                    }
                    let result = handle_external_host_picker_key(
                        "enter",
                        &confirm_state,
                        &confirm_input,
                        HostPickerSearchProjection {
                            options: &confirm_options,
                            groups: &confirm_groups,
                            query: &confirm_query,
                        },
                        key_message,
                        app,
                    );
                    if let HostPickerExternalKey::Select(target) = result {
                        confirm_owner.update(app, |pane, cx| {
                            pane.clear_host_search(side, window, cx);
                            match target {
                                SftpHostPickerTarget::Local => pane.request_local_host(side, cx),
                                SftpHostPickerTarget::Remote(host_id) => {
                                    pane.request_sftp_host(side, host_id, cx)
                                }
                            }
                        });
                        window.prevent_default();
                        app.stop_propagation();
                    }
                })
                .capture_key_down(move |event, window, app| {
                    if !expanded
                        || !matches!(
                            event.keystroke.key.to_ascii_lowercase().as_str(),
                            "up" | "down" | "arrowup" | "arrowdown"
                        )
                    {
                        return;
                    }
                    let result = handle_external_host_picker_key(
                        &event.keystroke.key,
                        &key_state,
                        &key_input,
                        HostPickerSearchProjection {
                            options: &key_options,
                            groups: &key_groups,
                            query: &key_query,
                        },
                        key_message,
                        app,
                    );
                    if matches!(&result, HostPickerExternalKey::Handled) {
                        key_owner.update(app, |_, cx| cx.notify());
                    }
                    if !matches!(result, HostPickerExternalKey::Ignore) {
                        window.prevent_default();
                        app.stop_propagation();
                    }
                })
                .child(host_picker_popover(HostPickerPopoverConfig {
                    id: format!("files-sftp-host-picker-popover-{index}").into(),
                    trigger: render_trigger(
                        &trigger_input,
                        side,
                        pane.host_picker.target_pending[index],
                        theme,
                    ),
                    // 输入框自行根据焦点和输入展开，鼠标定位光标不应切换弹层。
                    mouse_button: None,
                    open: pane.host_picker.expanded_side == Some(side),
                    trigger_style: StyleRefinement::default().w_full().min_w(px(0.)).h(px(28.)),
                    focus_handle: Some(focus_handle),
                    on_open_change:
                        move |open: &bool, window: &mut Window, app: &mut gpui_kit::App| {
                            header_owner.update(app, |pane, cx| {
                                pane.set_host_picker_open(side, *open, window, cx);
                            });
                        },
                    content: picker_content,
                })),
        )
        .into_any_element()
}

/// 组装 HostId 列表与 SFTP owner 操作；刷新保留已有选项几何并在原按钮展示真实忙态。
///
/// @author kongweiguang
fn sftp_host_picker_list(
    projection: SftpHostPickerListProjection,
) -> HostPickerList<SftpHostPickerTarget> {
    let SftpHostPickerListProjection {
        pane: pane_entity,
        side,
        search_input,
        keyboard_state,
        options,
        groups,
        query,
        loading,
        show_loading_message,
        error,
        has_remote_options,
        theme,
    } = projection;
    let index = side.index();
    let select_pane = pane_entity.clone();
    let mut list = HostPickerList::new(
        format!("files-sftp-host-picker-list-{index}"),
        options,
        move |target, window, app| {
            select_pane.update(app, |pane, cx| {
                pane.clear_host_search(side, window, cx);
                match target {
                    SftpHostPickerTarget::Local => pane.request_local_host(side, cx),
                    SftpHostPickerTarget::Remote(host_id) => {
                        pane.request_sftp_host(side, host_id, cx)
                    }
                }
            });
            window.prevent_default();
        },
    )
    .external_search_input(search_input, keyboard_state)
    .query(query.clone())
    .groups(groups)
    .theme(theme);

    if show_loading_message {
        list = list.message(HostPickerListMessage::Loading(locale::text(
            "正在读取已保存的 SSH/SFTP 主机…",
            "Loading saved SSH/SFTP hosts…",
        )));
    } else if !loading && let Some(error) = error.clone() {
        list = list.message(HostPickerListMessage::Error(error.into()));
    } else if !loading && !has_remote_options && query.trim().is_empty() {
        list = list.message(HostPickerListMessage::Empty(locale::text(
            "没有已启用 SFTP 的主机。",
            "No hosts with SFTP enabled are available.",
        )));
    }

    let refresh_pane = pane_entity.clone();
    let create_pane = pane_entity;
    let footer = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            Button::new(format!("files-sftp-host-picker-refresh-{index}"))
                .compact()
                .ghost()
                .icon(gpui_kit::component::IconName::RefreshCw)
                .loading(loading)
                .loading_icon(gpui_kit::component::IconName::RefreshCw)
                .accessibility_label(if loading {
                    locale::text("正在刷新主机列表", "Refreshing host list")
                } else {
                    locale::text("刷新主机列表", "Refresh host list")
                })
                .tooltip(locale::text("刷新主机列表", "Refresh host list"))
                .disabled(loading)
                .on_click(move |_, window, app| {
                    refresh_pane.update(app, |pane, cx| pane.reload_sftp_hosts(cx));
                    window.prevent_default();
                }),
        )
        .child(
            Button::new(format!("files-sftp-host-picker-create-{index}"))
                .flex_1()
                .compact()
                .ghost()
                .icon(gpui_kit::component::IconName::Plus)
                .label(locale::text("新建 SFTP 主机…", "New SFTP host…"))
                .accessibility_label(match side {
                    FilePaneSide::Left => locale::text(
                        "新建 SFTP 主机，创建后加入左侧",
                        "Create an SFTP host and add it to the left",
                    ),
                    FilePaneSide::Right => locale::text(
                        "新建 SFTP 主机，创建后加入右侧",
                        "Create an SFTP host and add it to the right",
                    ),
                })
                .on_click(move |_, window, app| {
                    create_pane.update(app, |pane, cx| {
                        pane.clear_host_search(side, window, cx);
                        pane.request_create_sftp_host(side, cx);
                    });
                    window.prevent_default();
                }),
        );
    list.footer(footer)
}

/// 未选远端时仍显示浏览引导；选择弹层锚定在标题行，内容不占用文件列表高度。
///
/// @author kongweiguang
pub(super) fn render_empty_target(
    pane: &mut FilesPane,
    side: usize,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    div()
        .id(format!("files-unselected-remote-target-{side}"))
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(render_selector_header(pane, side, theme, window, cx))
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .text_color(theme.muted)
                .child(
                    div()
                        .text_size(theme.text_body)
                        .text_color(theme.foreground)
                        .child(if side == 0 {
                            locale::text(
                                "选择左侧主机开始浏览",
                                "Choose a host on the left to start browsing",
                            )
                        } else {
                            locale::text(
                                "选择右侧主机开始浏览",
                                "Choose a host on the right to start browsing",
                            )
                        }),
                ),
        )
        .into_any_element()
}
