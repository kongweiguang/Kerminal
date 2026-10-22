//! 共享 SFTP 工作区复用通用主机列表，端点选择和信任流程仍由 Root 管理。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, Context, Focusable as _, IntoElement, MouseButton, StyleRefinement, Window,
    assets::IconName,
    component::Disableable,
    component::button::{Button, ButtonVariants as _},
    div,
    prelude::*,
    px,
};
use kerminal_domain::{files::FileEndpoint, identity::HostId};

use crate::{
    features::host_picker::{
        HostPickerGroup, HostPickerList, HostPickerListMessage, HostPickerOption,
        HostPickerPopoverConfig, host_picker_popover,
    },
    locale,
    theme::ThemeTokens,
};

use super::super::FilesPane;

/// 冻结 SFTP 目录和搜索状态，使共享列表展示与当前 FilesPane owner 保持一致。
///
/// @author kongweiguang
struct SftpHostPickerListProjection {
    pane: gpui_kit::Entity<FilesPane>,
    search_input: gpui_kit::Entity<gpui_kit::component::input::InputState>,
    options: Vec<HostPickerOption<HostId>>,
    groups: Vec<HostPickerGroup>,
    query: String,
    loading: bool,
    error: Option<String>,
    theme: ThemeTokens,
}

/// 右侧触发器保持 endpoint 标题语义；弹层开关交由固定锚点统一处理。
///
/// @author kongweiguang
pub(super) fn render_trigger(pane: &FilesPane, theme: ThemeTokens) -> Button {
    let current = match &pane.panels[1].endpoint {
        FileEndpoint::Remote { .. } => pane.target_names.display(&pane.panels[1].endpoint).compact,
        _ => locale::text("搜索主机…", "Search hosts…").to_string(),
    };
    Button::new("files-sftp-host-picker-toggle")
        .compact()
        .ghost()
        .flex_1()
        .min_w(px(0.))
        .text_size(theme.text_xs)
        .icon(if pane.host_picker.expanded {
            gpui_kit::component::IconName::ChevronUp
        } else {
            gpui_kit::component::IconName::Search
        })
        .label(current)
        .tooltip(locale::text(
            "搜索并选择 SFTP 主机",
            "Search and select an SFTP host",
        ))
        .accessibility_label(locale::text(
            "打开或收起右侧主机搜索",
            "Open or close right-side host search",
        ))
}

/// 主机名栏与列表共享单一 Popover；用户选择后仍只把 HostId 交回 FilesPane owner。
///
/// @author kongweiguang
pub(super) fn render_selector_header(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let Some(search_input) = pane.ensure_host_search_input(window, cx) else {
        return div()
            .id("files-sftp-host-picker-disabled")
            .into_any_element();
    };
    let focus_handle = search_input.read(cx).focus_handle(cx);
    let (options, query, loading, error) = {
        let picker = &pane.host_picker;
        let options = picker
            .hosts
            .iter()
            .map(|host| HostPickerOption {
                key: host.id.as_str().to_owned().into(),
                target: host.id.clone(),
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
            .collect::<Vec<_>>();
        (
            options,
            picker.query.clone(),
            picker.loading,
            picker.error.clone(),
        )
    };
    let groups = pane.host_picker.groups.clone();
    let owner = cx.entity().clone();
    let picker_content =
        move |_: &mut gpui_kit::component::popover::PopoverState,
              _: &mut Window,
              _: &mut Context<gpui_kit::component::popover::PopoverState>| {
            sftp_host_picker_list(SftpHostPickerListProjection {
                pane: owner.clone(),
                search_input: search_input.clone(),
                options: options.clone(),
                groups: groups.clone(),
                query: query.clone(),
                loading,
                error: error.clone(),
                theme,
            })
        };
    let header_owner = cx.entity().clone();
    div()
        .id("files-sftp-target-selector-header")
        .w_full()
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(div().text_color(theme.accent).child(IconName::Server))
        .child(host_picker_popover(HostPickerPopoverConfig {
            id: "files-sftp-host-picker-popover".into(),
            trigger: render_trigger(pane, theme),
            mouse_button: MouseButton::Left,
            open: pane.host_picker.expanded,
            trigger_style: StyleRefinement::default().flex_1().min_w(px(0.)).h(px(28.)),
            focus_handle: Some(focus_handle),
            on_open_change: move |open: &bool, window: &mut Window, app: &mut gpui_kit::App| {
                header_owner.update(app, |pane, cx| {
                    pane.set_host_picker_open(*open, window, cx);
                });
            },
            content: picker_content,
        }))
        .into_any_element()
}

/// 组装 HostId 列表与 SFTP owner 操作；重试、刷新和创建均保留既有 FilesPane/Root 路径。
///
/// @author kongweiguang
fn sftp_host_picker_list(projection: SftpHostPickerListProjection) -> HostPickerList<HostId> {
    let SftpHostPickerListProjection {
        pane: pane_entity,
        search_input,
        options,
        groups,
        query,
        loading,
        error,
        theme,
    } = projection;
    let has_options = !options.is_empty();
    let select_pane = pane_entity.clone();
    let mut list = HostPickerList::new(
        "files-sftp-host-picker-list",
        options,
        move |host_id, window, app| {
            select_pane.update(app, |pane, cx| pane.request_sftp_host(host_id, cx));
            window.prevent_default();
        },
    )
    .search_input(search_input)
    .query(query.clone())
    .groups(groups)
    .theme(theme);

    if loading {
        list = list.message(HostPickerListMessage::Loading(locale::text(
            "正在读取已保存的 SSH/SFTP 主机…",
            "Loading saved SSH/SFTP hosts…",
        )));
    } else if let Some(error) = error.clone() {
        list = list.message(HostPickerListMessage::Error(error.into()));
    } else if !has_options && query.trim().is_empty() {
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
            Button::new("files-sftp-host-picker-refresh")
                .compact()
                .ghost()
                .icon(gpui_kit::component::IconName::RefreshCw)
                .accessibility_label(locale::text("刷新主机列表", "Refresh host list"))
                .tooltip(locale::text("刷新主机列表", "Refresh host list"))
                .disabled(loading)
                .on_click(move |_, window, app| {
                    refresh_pane.update(app, |pane, cx| pane.reload_sftp_hosts(cx));
                    window.prevent_default();
                }),
        )
        .child(
            Button::new("files-sftp-host-picker-create")
                .flex_1()
                .compact()
                .ghost()
                .icon(gpui_kit::component::IconName::Plus)
                .label(locale::text("新建 SFTP 主机…", "New SFTP host…"))
                .accessibility_label(locale::text(
                    "新建 SFTP 主机，创建后加入右侧",
                    "Create an SFTP host and add it to the right after creation",
                ))
                .tooltip(locale::text(
                    "创建后加入右侧主机选择",
                    "Add the host to the right-side selector after creation",
                ))
                .on_click(move |_, window, app| {
                    create_pane.update(app, |pane, cx| pane.request_create_sftp_host(cx));
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
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    div()
        .id("files-unselected-remote-target")
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
        .child(render_selector_header(pane, theme, window, cx))
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
                        .child(locale::text(
                            "选择右侧主机开始浏览",
                            "Choose a host on the right to start browsing",
                        )),
                ),
        )
        .into_any_element()
}
