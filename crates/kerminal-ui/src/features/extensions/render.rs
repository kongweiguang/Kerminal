//! 设置页内的扩展主从视图：页头动作、卡片工具栏、稳定行高列表和独立详情。
//!
//! 页面只把目录与安装状态投影为设置页中的一种阅读关系；目录刷新、错误和确认都在
//! 卡片内部表达，不再用底部状态条改变整页高度。
//!
//! @author kongweiguang

use super::{
    Confirmation, ExtensionsEvent, ExtensionsPage, Filter, Operation, label, sample, selection,
};
use crate::theme::ThemeTokens;
use gpui_kit::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, Render,
    SharedString, StatefulInteractiveElement as _, Window,
    assets::IconName,
    base::Selectable as _,
    component::{
        Disableable as _, Icon, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
        scroll::ScrollableElement as _,
        spinner::Spinner,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::extensions::{
    CatalogPlugin, CatalogSelection, ExtensionChange, InstallProgress,
};
use kerminal_domain::plugins::PluginId;
use std::collections::BTreeSet;

impl Render for ExtensionsPage {
    /// 页头与单张主从卡片共同承担设置页的既有留白，卡片内部各自滚动。
    /// 确认交给窗口级 Kit Dialog，页面不再自行绘制遮罩或手写焦点陷阱。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_settings();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let state = self.state.as_ref();
        let installed_count = state.map_or(0, |state| state.plugins.len());
        let pending_count = state.map_or(0, |state| state.pending.len());
        let preview = self.catalog.is_none();
        let plugins: &[CatalogPlugin] = if preview {
            &sample::catalog().plugins
        } else {
            &self
                .catalog
                .as_ref()
                .expect("目录存在时预览分支不会启用")
                .catalog
                .plugins
        };
        let mut categories = BTreeSet::new();
        for plugin in plugins {
            categories.extend(plugin.categories.iter().cloned());
        }
        let update_count = plugins
            .iter()
            .filter(|plugin| {
                state
                    .and_then(|state| state.plugins.get(&plugin.id))
                    .zip(selection(plugin, &self.host_api))
                    .is_some_and(|(installed, available)| {
                        available.version.as_semver() > installed.active.as_semver()
                    })
            })
            .count();
        let summary = header_summary(
            preview,
            self.refreshing,
            plugins.len(),
            installed_count,
            update_count,
            pending_count,
            self.offline,
        );
        // 刷新中把同一位置变成可点击取消：Spinner 只表达真实等待，按钮本身仍可命中。
        let refresh_button = if self.refreshing {
            Button::new("extensions-refresh")
                .compact()
                .ghost()
                .child(Spinner::new().small().color(theme.muted))
                .accessibility_label(crate::locale::text(
                    "取消刷新目录",
                    "Cancel catalog refresh",
                ))
                .tooltip(crate::locale::text(
                    "取消刷新目录",
                    "Cancel catalog refresh",
                ))
                .on_click(cx.listener(|page, _, _, cx| {
                    page.cancel();
                    cx.notify();
                }))
        } else {
            Button::new("extensions-refresh")
                .compact()
                .ghost()
                .icon(IconName::RefreshCw)
                .disabled(self.busy)
                .accessibility_label(crate::locale::text("刷新目录", "Refresh catalog"))
                .tooltip(crate::locale::text("刷新目录", "Refresh catalog"))
                .on_click(cx.listener(|page, _, _, cx| page.refresh(cx)))
        };

        let header = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(36.))
                            .flex_shrink_0()
                            .rounded(theme.control_radius)
                            .bg(theme.surface_selected())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Puzzle)
                                    .size(px(16.))
                                    .text_color(theme.settings_choice_foreground(true)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(crate::locale::text("扩展", "Extensions")),
                            )
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(summary),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .when(pending_count > 0, |actions| {
                        actions.child(
                            Button::new("extensions-restart")
                                .label(crate::locale::text("立即重启", "Restart now"))
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(ExtensionsEvent::RestartRequested)
                                })),
                        )
                    })
                    .child(refresh_button)
                    .child(
                        Button::new("extensions-file")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("从文件安装…", "Install from file…"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|page, _, _, cx| page.choose_file(cx))),
                    ),
            );

        let search_input = gpui_kit::Styled::h(
            Input::new(&self.search)
                .prefix(IconName::Search)
                .cleanable(true)
                .bg(theme.field_surface())
                .border_color(theme.strong_border())
                .rounded(theme.control_radius)
                .font_weight(FontWeight::NORMAL)
                .flex_1()
                .min_w(px(180.)),
            theme.metrics.control_height,
        );
        let all_categories = crate::locale::text("全部分类", "All categories");
        let category_button_label = match &self.filter {
            Filter::Category(category) => category_label(category),
            _ => all_categories.clone(),
        };
        let categories: Vec<String> = categories.into_iter().collect();
        let category_active = !matches!(self.filter, Filter::Category(_));
        let page_entity = cx.entity();
        let menu_categories = categories.clone();
        let current_filter = self.filter.clone();
        let category_button = Button::new("extensions-category")
            .compact()
            .ghost()
            .icon(IconName::ListFilter)
            .label(category_button_label)
            .dropdown_caret(true)
            .when(!category_active, |button| {
                button
                    .bg(theme.surface_selected())
                    .text_color(theme.foreground)
            })
            .dropdown_menu(move |menu, _, _| {
                let menu = crate::ui_popup::dropdown_menu(menu, theme);
                let all_page = page_entity.clone();
                let mut menu = menu.item(
                    PopupMenuItem::new(all_categories.clone())
                        .checked(category_active)
                        .on_click(move |_, _, app| {
                            all_page.update(app, |page, cx| {
                                page.filter = Filter::All;
                                cx.notify();
                            });
                        }),
                );
                for category in &menu_categories {
                    let page = page_entity.clone();
                    let target = category.clone();
                    let label = category_label(category);
                    let checked =
                        matches!(&current_filter, Filter::Category(active) if active == category);
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, app| {
                            let target = target.clone();
                            page.update(app, |page, cx| {
                                page.filter = Filter::Category(target.clone());
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            });

        let toolbar = div()
            .px_4()
            .py_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(search_input)
            .child(filter_button(
                "extensions-all",
                crate::locale::text("全部", "All"),
                self.filter == Filter::All,
                Filter::All,
                theme,
                cx,
            ))
            .child(filter_button(
                "extensions-installed",
                crate::locale::text(
                    format!("已安装 ({installed_count})"),
                    format!("Installed ({installed_count})"),
                ),
                self.filter == Filter::Installed,
                Filter::Installed,
                theme,
                cx,
            ))
            .child(filter_button(
                "extensions-updates",
                crate::locale::text(
                    format!("可更新 ({update_count})"),
                    format!("Updates ({update_count})"),
                ),
                self.filter == Filter::Updates,
                Filter::Updates,
                theme,
                cx,
            ))
            .child(category_button);

        let rows = self.visible_rows(plugins, &query, preview);
        let row_count = rows.len();
        let active_row_visible = rows.iter().any(|row| row.installing);
        let mut list = div()
            .id("extensions-list")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .vertical_scrollbar(&self.list_scroll)
            .p_2()
            .flex()
            .flex_col()
            .gap_1();
        for row in rows {
            list = list.child(render_extension_row(row, theme, cx));
        }
        if row_count == 0 {
            let (message, clear_search) = if !query.is_empty() {
                (
                    crate::locale::text("没有匹配的扩展。", "No matching extensions."),
                    true,
                )
            } else {
                match &self.filter {
                    Filter::Installed => (
                        crate::locale::text("尚未安装扩展。", "No extensions installed yet."),
                        false,
                    ),
                    Filter::Updates => (
                        crate::locale::text("没有可更新的扩展。", "No updates available."),
                        false,
                    ),
                    Filter::Category(category) => {
                        let display = category_label(category);
                        (
                            crate::locale::text(
                                format!("分类「{display}」暂无扩展。"),
                                format!("No extensions in “{display}”."),
                            ),
                            false,
                        )
                    }
                    _ if plugins.is_empty() => (
                        crate::locale::text(
                            "市场目录暂无可用的扩展。",
                            "The catalog has no extensions yet.",
                        ),
                        false,
                    ),
                    _ => (
                        crate::locale::text("没有匹配的扩展。", "No matching extensions."),
                        false,
                    ),
                }
            };
            let mut empty = div()
                .w_full()
                .p_6()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .text_align(gpui_kit::TextAlign::Center)
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(message);
            if clear_search {
                empty = empty.child(
                    Button::new("extensions-clear-search")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("清除搜索", "Clear search"))
                        .on_click(cx.listener(|page, _, window, cx| {
                            page.search.update(cx, |input, cx| {
                                input.set_value(String::new(), window, cx);
                            });
                        })),
                );
            }
            list = list.child(empty);
        }

        let mut notices = div().mx_2().mt_2().flex().flex_col().gap_1();
        // 文件预检和没有可见行的安装需要页面级活动条，否则用户看不到进度或取消入口。
        if self.inspecting || (self.progress.is_some() && !active_row_visible) {
            let message = if self.inspecting {
                crate::locale::text(
                    "正在校验本地插件包…",
                    "Verifying the local extension package…",
                )
                .to_string()
            } else {
                crate::locale::text(
                    format!(
                        "安装中 · {}",
                        self.progress
                            .as_ref()
                            .map(progress_label)
                            .unwrap_or_default()
                    ),
                    format!(
                        "Installing · {}",
                        self.progress
                            .as_ref()
                            .map(progress_label)
                            .unwrap_or_default()
                    ),
                )
                .to_string()
            };
            notices = notices.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.control_radius)
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme.surface_selected())
                    .text_size(theme.text_sm)
                    .text_color(theme.foreground)
                    .child(Spinner::new().small().color(theme.accent))
                    .child(div().flex_1().min_w(px(0.)).child(message))
                    .child(
                        Button::new("extensions-activity-cancel")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("取消", "Cancel"))
                            .disabled(self.cancel.is_none())
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.cancel();
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(message) = self.status.clone() {
            let warning = self.status_is_warning;
            let tone = if warning { theme.warning } else { theme.error };
            let icon = if warning {
                IconName::TriangleAlert
            } else {
                IconName::CircleAlert
            };
            let mut notice = div()
                .px_3()
                .py_2()
                .rounded(theme.control_radius)
                .flex()
                .items_center()
                .gap_2()
                .bg(tone.opacity(0.10))
                .text_size(theme.text_sm)
                .text_color(tone)
                .child(Icon::new(icon).size(px(16.)).text_color(tone))
                .child(div().flex_1().min_w(px(0.)).child(message));
            if !warning {
                let retry_operation = self.retry_operation.is_some();
                notice = notice.child(
                    Button::new("extensions-retry")
                        .compact()
                        .ghost()
                        .label(if retry_operation {
                            crate::locale::text("重试操作", "Retry operation")
                        } else {
                            crate::locale::text("刷新状态", "Refresh state")
                        })
                        .disabled(self.busy)
                        .on_click(cx.listener(|page, _, _, cx| {
                            // 忙碌时按钮已禁用；这里再次检查，避免先取走待重试操作再静默丢弃。
                            if page.busy {
                                return;
                            }
                            if let Some(operation) = page.retry_operation.take() {
                                page.run_operation(operation, cx);
                            } else {
                                page.activate(cx);
                            }
                        })),
                );
            }
            notices = notices.child(notice);
        }
        if preview {
            let text = if self.refreshing {
                crate::locale::text(
                    "正在获取市场目录，以下为示例预览。",
                    "Fetching the marketplace catalog; showing sample previews.",
                )
            } else {
                crate::locale::text(
                    "市场目录暂不可用，以下为示例预览。示例条目不可安装。",
                    "Marketplace catalog unavailable; showing sample previews. Samples are not installable.",
                )
            };
            notices = notices.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.control_radius)
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme.surface_selected())
                    .text_size(theme.text_sm)
                    .text_color(theme.muted)
                    .child(
                        Icon::new(IconName::Info)
                            .size(px(16.))
                            .text_color(theme.muted),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(text)),
            );
        }

        let results = div()
            .id("extensions-results")
            .flex_1()
            .min_w(px(260.))
            .h_full()
            .flex()
            .flex_col()
            .child(notices)
            .child(list);

        let detail = self.render_detail(preview, theme, cx);

        let body = div()
            .id("extensions-body")
            .flex_1()
            .min_h(px(0.))
            .flex()
            // 设置弹框可能窄于列表与详情的最小宽度；只让这一层兜底横向滚动，不新增页面级滚动。
            .overflow_x_scroll()
            .child(results)
            .child(detail);

        let card = div()
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_panel)
            .overflow_hidden()
            .child(toolbar)
            .child(body);

        div()
            .id("extensions-page")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .gap_4()
            .child(header)
            .child(card)
    }
}

impl ExtensionsPage {
    /// 三类来源共用同一行结构：目录条目、目录缺失的本地安装、等待重启的首次安装。
    /// 过滤与搜索只决定可见性，不改变安装状态或行高。
    fn visible_rows(
        &self,
        plugins: &[CatalogPlugin],
        query: &str,
        preview: bool,
    ) -> Vec<ExtensionRow> {
        let mut rows = Vec::new();
        let state = self.state.as_ref();
        for plugin in plugins {
            let current = state.and_then(|state| state.plugins.get(&plugin.id));
            let available = selection(plugin, &self.host_api);
            let update = current
                .zip(available.clone())
                .is_some_and(|(installed, available)| {
                    available.version.as_semver() > installed.active.as_semver()
                });
            let name = label(&plugin.name, plugin.id.as_str());
            let description = label(&plugin.description, "");
            if !query.is_empty()
                && !format!(
                    "{name} {} {} {description}",
                    plugin.id.as_str(),
                    plugin.publisher
                )
                .to_lowercase()
                .contains(query)
            {
                continue;
            }
            if match &self.filter {
                Filter::All => false,
                Filter::Installed => current.is_none(),
                Filter::Updates => !update,
                Filter::Category(category) => !plugin.categories.contains(category),
            } {
                continue;
            }
            let pending_change = state.is_some_and(|state| {
                state
                    .pending
                    .iter()
                    .any(|change| change.plugin_id() == &plugin.id)
            });
            let broken = state.is_some_and(|state| state.failures.contains_key(&plugin.id));
            let installing = self.active_install.as_ref() == Some(&plugin.id);
            let mut row = ExtensionRow {
                id: plugin.id.clone(),
                name: name.to_owned(),
                description: description.to_owned(),
                progress: None,
                status: None,
                action: None,
                installing,
                cancel_enabled: self.cancel.is_some(),
                selected: self.selected.as_ref() == Some(&plugin.id),
            };
            if installing {
                row.progress = Some(
                    self.progress
                        .as_ref()
                        .map(progress_label)
                        .unwrap_or_else(|| crate::locale::text("准备中", "Preparing").to_string()),
                );
            } else if broken {
                row.status = Some((crate::locale::text("损坏", "Broken"), RowTone::Error));
            } else if pending_change {
                row.status = Some((
                    crate::locale::text("重启生效", "Restart required"),
                    RowTone::Warning,
                ));
            } else if preview {
                row.status = Some((crate::locale::text("示例", "Sample"), RowTone::Muted));
            } else if update || current.is_none() {
                if let Some(available) = available {
                    row.action = Some(ExtensionRowAction {
                        label: if update {
                            crate::locale::text("更新", "Update")
                        } else {
                            crate::locale::text("安装", "Install")
                        },
                        selection: available,
                        disabled: self.busy || self.offline,
                    });
                } else {
                    row.status = Some((
                        crate::locale::text("不支持当前平台", "Not supported on this platform"),
                        RowTone::Muted,
                    ));
                }
            } else if let Some(current) = current {
                row.status = Some((
                    if current.enabled {
                        crate::locale::text("已启用", "Enabled")
                    } else {
                        crate::locale::text("已禁用", "Disabled")
                    },
                    RowTone::Muted,
                ));
            } else {
                row.status = Some((
                    crate::locale::text("不支持当前平台", "Not supported on this platform"),
                    RowTone::Muted,
                ));
            }
            rows.push(row);
        }

        // 本地安装或目录下架的插件仍属于用户的已安装列表。
        if let Some(state) = state {
            for (id, installed) in &state.plugins {
                if plugins.iter().any(|plugin| &plugin.id == id)
                    || !matches!(&self.filter, Filter::All | Filter::Installed)
                    || (!query.is_empty() && !id.as_str().contains(query))
                {
                    continue;
                }
                // 文件更新已安装插件时必须保留该行，否则进度与取消会随筛选条件一起消失。
                let installing = self.active_install.as_ref() == Some(id);
                let pending_change = state.pending.iter().any(|change| change.plugin_id() == id);
                rows.push(ExtensionRow {
                    id: id.clone(),
                    name: id.as_str().to_owned(),
                    description: crate::locale::text(
                        "本地安装或目录中不可用",
                        "Installed locally or unavailable in the catalog",
                    )
                    .to_string(),
                    progress: installing.then(|| {
                        self.progress
                            .as_ref()
                            .map(progress_label)
                            .unwrap_or_else(|| {
                                crate::locale::text("准备中", "Preparing").to_string()
                            })
                    }),
                    status: Some((
                        if pending_change {
                            crate::locale::text("重启生效", "Restart required")
                        } else if installed.enabled {
                            crate::locale::text("已启用", "Enabled")
                        } else {
                            crate::locale::text("已禁用", "Disabled")
                        },
                        if pending_change {
                            RowTone::Warning
                        } else {
                            RowTone::Muted
                        },
                    )),
                    action: None,
                    installing,
                    cancel_enabled: self.cancel.is_some(),
                    selected: self.selected.as_ref() == Some(id),
                });
            }
        }
        // 本地包首次安装尚未重启时没有 installed 条目，仍需展示排队事实。
        if let Some(state) = state {
            for change in &state.pending {
                let id = change.plugin_id();
                if state.plugins.contains_key(id)
                    || plugins.iter().any(|plugin| &plugin.id == id)
                    || !matches!(&self.filter, Filter::All | Filter::Installed)
                    || (!query.is_empty() && !id.as_str().contains(query))
                {
                    continue;
                }
                rows.push(ExtensionRow {
                    id: id.clone(),
                    name: id.as_str().to_owned(),
                    description: crate::locale::text(
                        "安装已排队，重启后生效",
                        "Installation queued and applied after restart",
                    )
                    .to_string(),
                    progress: None,
                    status: Some((
                        crate::locale::text("重启生效", "Restart required"),
                        RowTone::Warning,
                    )),
                    action: None,
                    installing: false,
                    cancel_enabled: false,
                    selected: self.selected.as_ref() == Some(id),
                });
            }
        }
        rows
    }

    /// 详情独立滚动，保持名称、版本、描述、操作、链接、权限和 README 的固定阅读顺序。
    fn render_detail(
        &self,
        preview: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut detail = div()
            .id("extension-detail")
            .w(px(320.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll)
            .vertical_scrollbar(&self.detail_scroll);

        if let Some(plugin) = self.selected_plugin().cloned() {
            let current = self
                .state
                .as_ref()
                .and_then(|state| state.plugins.get(&plugin.id))
                .cloned();
            let pending_change = self.state.as_ref().is_some_and(|state| {
                state
                    .pending
                    .iter()
                    .any(|change| change.plugin_id() == &plugin.id)
            });
            let available = selection(&plugin, &self.host_api);
            let installing = self.active_install.as_ref() == Some(&plugin.id);
            let version_line = detail_version_line(
                preview,
                available.as_ref(),
                current.as_ref(),
                pending_change,
                &plugin,
            );
            detail = detail.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(label(&plugin.name, plugin.id.as_str()).to_owned()),
                    )
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(format!("{} · {}", plugin.publisher, plugin.id.as_str())),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(version_line),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(label(&plugin.description, "").to_owned()),
                    ),
            );

            let mut actions = div().w_full().flex().flex_col().gap_2();
            if preview {
                actions = actions.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "示例条目不可安装；正式市场目录发布后这里会出现可安装版本。",
                            "Samples are not installable; installable versions appear here after the official catalog is published.",
                        )),
                );
            } else if installing {
                actions = actions
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(self.progress.as_ref().map(progress_label).unwrap_or_else(
                                || crate::locale::text("准备中", "Preparing").to_string(),
                            )),
                    )
                    .child(
                        Button::new("extension-install-cancel")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("取消安装", "Cancel install"))
                            .disabled(self.cancel.is_none())
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.cancel();
                                cx.notify();
                            })),
                    );
            } else if let Some(available) = available.clone() {
                let installable = current.as_ref().is_none_or(|installed| {
                    available.version.as_semver() > installed.active.as_semver()
                });
                if installable {
                    let caption = if current.is_some() {
                        crate::locale::text(
                            format!("更新到 {}", available.version),
                            format!("Update to {}", available.version),
                        )
                    } else {
                        crate::locale::text(
                            format!("安装 v{}", available.version),
                            format!("Install v{}", available.version),
                        )
                    };
                    actions = actions.child(
                        Button::new("extension-install")
                            .primary()
                            .label(caption)
                            .disabled(self.busy || pending_change || self.offline)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_confirmation(
                                    Confirmation::Install(available.clone()),
                                    cx,
                                );
                            })),
                    );
                    if pending_change {
                        actions = actions.child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "变更已提交，重启后生效。",
                                    "The change is committed and takes effect after restart.",
                                )),
                        );
                    }
                }
            } else if current.is_none() {
                actions = actions.child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "不支持当前平台",
                            "Not supported on this platform",
                        )),
                );
            }
            if let Some(installed) = current.clone() {
                let id = plugin.id.clone();
                let enabled = installed.enabled;
                let mut buttons = div().w_full().flex().flex_wrap().items_center().gap_2();
                buttons = buttons.child(
                    Button::new("extension-toggle")
                        .compact()
                        .ghost()
                        .label(if enabled {
                            crate::locale::text("禁用", "Disable")
                        } else {
                            crate::locale::text("启用", "Enable")
                        })
                        .disabled(self.busy || pending_change)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(if enabled {
                                    ExtensionChange::Disable(id.clone())
                                } else {
                                    ExtensionChange::Enable(id.clone())
                                }),
                                cx,
                            )
                        })),
                );
                if let Some(previous) = installed.previous.clone() {
                    let id = plugin.id.clone();
                    buttons = buttons.child(
                        Button::new("extension-rollback")
                            .compact()
                            .ghost()
                            .label(crate::locale::text(
                                format!("回退到 {previous}"),
                                format!("Roll back to {previous}"),
                            ))
                            .disabled(self.busy || pending_change)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.run_operation(
                                    Operation::Change(ExtensionChange::Rollback(id.clone())),
                                    cx,
                                )
                            })),
                    );
                }
                let id = plugin.id.clone();
                buttons = buttons.child(
                    Button::new("extension-uninstall")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("卸载", "Uninstall"))
                        .disabled(self.busy || pending_change)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(ExtensionChange::Uninstall(id.clone())),
                                cx,
                            )
                        })),
                );
                let id = plugin.id.clone();
                actions = actions.child(buttons).child(
                    div()
                        .w_full()
                        .border_t_1()
                        .border_color(theme.border)
                        .pt_3()
                        .child(
                            Button::new("extension-delete-data")
                                .compact()
                                .ghost()
                                .text_color(theme.error)
                                .label(crate::locale::text(
                                    "删除扩展数据…",
                                    "Delete extension data…",
                                ))
                                .disabled(self.busy)
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    page.open_confirmation(
                                        Confirmation::DeleteData(id.clone()),
                                        cx,
                                    );
                                })),
                        ),
                );
            }
            if let Some(failure) = self.failure_block(&plugin.id, theme, cx) {
                actions = actions.child(failure);
            }
            detail = detail.child(actions);

            let mut meta = div().w_full().flex().flex_col().gap_1();
            if !preview
                && let Some(available) = &available
                && let Some(url) = plugin
                    .versions
                    .iter()
                    .find(|version| version.version == available.version)
                    .and_then(|version| version.changelog_url.clone())
            {
                meta = meta.child(external_link(
                    "extension-changelog",
                    crate::locale::text("更新日志", "Changelog"),
                    url,
                    cx,
                ));
            }
            meta = meta.child(external_link(
                "extension-repository",
                crate::locale::text("仓库", "Repository"),
                plugin.repository.clone(),
                cx,
            ));
            detail = detail.child(meta);

            let full_access = available
                .as_ref()
                .and_then(|selected| {
                    plugin
                        .versions
                        .iter()
                        .find(|version| version.version == selected.version)
                })
                .is_some_and(|version| version.requires_full_access());
            detail = detail
                .child(permissions_block(&plugin.permissions, full_access, theme))
                .child(failure_readme(&self.readme, preview, &plugin));
        } else if let Some((id, installed)) = self.selected.as_ref().and_then(|id| {
            self.state
                .as_ref()?
                .plugins
                .get(id)
                .map(|entry| (id.clone(), entry.clone()))
        }) {
            let pending_change = self
                .state
                .as_ref()
                .is_some_and(|state| state.pending.iter().any(|change| change.plugin_id() == &id));
            let enabled = installed.enabled;
            detail = detail
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(id.as_str().to_owned()),
                )
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.foreground)
                        .child(crate::locale::text(
                            format!(
                                "当前版本 {} · 目录不可用",
                                installed.active
                            ),
                            format!(
                                "Current version {} · Catalog unavailable",
                                installed.active
                            ),
                        )),
                )
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "本地安装或目录中不可用的扩展仍可管理。",
                            "Extensions installed locally or removed from the catalog remain manageable.",
                        )),
                );
            let mut buttons = div().w_full().flex().flex_wrap().items_center().gap_2();
            let target = id.clone();
            buttons = buttons.child(
                Button::new("extension-toggle-local")
                    .compact()
                    .ghost()
                    .label(if enabled {
                        crate::locale::text("禁用", "Disable")
                    } else {
                        crate::locale::text("启用", "Enable")
                    })
                    .disabled(self.busy || pending_change)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.run_operation(
                            Operation::Change(if enabled {
                                ExtensionChange::Disable(target.clone())
                            } else {
                                ExtensionChange::Enable(target.clone())
                            }),
                            cx,
                        )
                    })),
            );
            if let Some(previous) = installed.previous.clone() {
                let target = id.clone();
                buttons = buttons.child(
                    Button::new("extension-rollback-local")
                        .compact()
                        .ghost()
                        .label(crate::locale::text(
                            format!("回退到 {previous}"),
                            format!("Roll back to {previous}"),
                        ))
                        .disabled(self.busy || pending_change)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(ExtensionChange::Rollback(target.clone())),
                                cx,
                            )
                        })),
                );
            }
            let target = id.clone();
            buttons = buttons.child(
                Button::new("extension-uninstall-local")
                    .compact()
                    .ghost()
                    .label(crate::locale::text("卸载", "Uninstall"))
                    .disabled(self.busy || pending_change)
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.run_operation(
                            Operation::Change(ExtensionChange::Uninstall(target.clone())),
                            cx,
                        )
                    })),
            );
            let target = id.clone();
            detail = detail.child(buttons).child(
                div()
                    .w_full()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt_3()
                    .child(
                        Button::new("extension-delete-local")
                            .compact()
                            .ghost()
                            .text_color(theme.error)
                            .label(crate::locale::text(
                                "删除扩展数据…",
                                "Delete extension data…",
                            ))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_confirmation(
                                    Confirmation::DeleteData(target.clone()),
                                    cx,
                                );
                            })),
                    ),
            );
            if let Some(failure) = self.failure_block(&id, theme, cx) {
                detail = detail.child(failure);
            }
        } else if let Some(id) = self.selected.as_ref().filter(|id| {
            self.state.as_ref().is_some_and(|state| {
                !state.plugins.contains_key(*id)
                    && state.pending.iter().any(|change| change.plugin_id() == *id)
            })
        }) {
            detail = detail
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(id.as_str().to_owned()),
                )
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "安装已排队，重启后生效。",
                            "The installation is queued and takes effect after restart.",
                        )),
                );
        } else {
            detail = detail
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .size(px(48.))
                        .rounded_xl()
                        .bg(theme.muted_surface())
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(IconName::Puzzle)
                                .size(px(20.))
                                .text_color(theme.muted),
                        ),
                )
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(crate::locale::text(
                            "选择扩展查看详情",
                            "Select an extension to see its details",
                        )),
                )
                .child(
                    div()
                        .max_w(px(240.))
                        .text_align(gpui_kit::TextAlign::Center)
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "左侧列表提供安装、更新和已安装条目；示例仅在市场目录不可用时出现。",
                            "The list shows installable, updatable, and installed entries; samples appear only when the catalog is unavailable.",
                        )),
                );
        }
        detail.into_any_element()
    }

    /// 失败撤销绑定已展示的修订和完整意图，任何迟到点击都不能撤销另一笔新变更。
    fn failure_block(
        &self,
        id: &PluginId,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.state.as_ref()?;
        let failure = state.failures.get(id)?.clone();
        let pending = state
            .pending
            .iter()
            .find(|change| change.plugin_id() == id)?
            .clone();
        let revision = state.revision;
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.error)
                        .child(failure),
                )
                .child(
                    Button::new("extension-cancel-failed")
                        .compact()
                        .ghost()
                        .label(crate::locale::text(
                            "撤销失败的变更",
                            "Cancel failed change",
                        ))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(ExtensionChange::CancelFailedChange {
                                    expected_revision: revision,
                                    pending: pending.clone(),
                                }),
                                cx,
                            )
                        })),
                )
                .into_any_element(),
        )
    }

    /// 使用窗口级 Kit Dialog 呈现确认：焦点、Escape 和关闭后的焦点恢复由 Kit 统一负责。
    pub(super) fn present_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirmation) = self.confirmation.clone() else {
            self.dialog_open = false;
            return;
        };
        let Some(model) = self.confirmation_dialog(&confirmation) else {
            self.clear_confirmation(cx);
            return;
        };
        let theme = ThemeTokens::current(window, cx).for_settings();
        let owner = cx.entity().downgrade();
        let ok_owner = owner.clone();
        let cancel_owner = owner.clone();
        let dismiss_owner = owner.clone();
        let confirm_owner = owner.clone();
        let ok_operation = model.action.clone();
        let confirm_operation = model.action.clone();
        let danger = model.danger;
        window.open_dialog(cx, move |dialog, _, _| {
            let ok_owner = ok_owner.clone();
            let cancel_owner = cancel_owner.clone();
            let dismiss_owner = dismiss_owner.clone();
            let confirm_owner = confirm_owner.clone();
            let ok_operation = ok_operation.clone();
            let confirm_operation = confirm_operation.clone();
            dialog
                .centered()
                .width(px(440.))
                .close_button(false)
                .overlay_closable(false)
                .title(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(model.title.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .when(!model.subtitle.is_empty(), |body| {
                            body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(model.subtitle.clone()),
                            )
                        })
                        .when_some(model.warning.clone(), |body, warning| {
                            body.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_size(theme.text_sm)
                                    .text_color(theme.warning)
                                    .child(
                                        Icon::new(IconName::TriangleAlert)
                                            .size(px(16.))
                                            .text_color(theme.warning),
                                    )
                                    .child(warning),
                            )
                        })
                        .when_some(model.source.clone(), |body, source| {
                            body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(
                                        format!("来源 · {source}"),
                                        format!("Source · {source}"),
                                    )),
                            )
                        })
                        .when(!model.danger, |body| {
                            body.child(permissions_block(
                                &model.permissions,
                                model.full_access,
                                theme,
                            ))
                        }),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("extensions-dismiss")
                                .label(crate::locale::text("取消", "Cancel"))
                                .on_click(move |_, window, app| {
                                    window.close_dialog(app);
                                    if let Some(page) = dismiss_owner.upgrade() {
                                        page.update(app, |page, cx| page.clear_confirmation(cx));
                                    }
                                }),
                        )
                        .child(
                            Button::new("extensions-confirm")
                                .when(danger, |button| button.danger())
                                .when(!danger, |button| button.primary())
                                .label(model.action_label.clone())
                                .on_click(move |_, window, app| {
                                    window.close_dialog(app);
                                    if let Some(page) = confirm_owner.upgrade() {
                                        let operation = confirm_operation.clone();
                                        page.update(app, |page, cx| {
                                            page.finish_confirmation(Some(operation), cx)
                                        });
                                    }
                                }),
                        ),
                )
                // 非危险确认允许 Enter 执行；删除只能由显式按钮触发，Enter 等同取消。
                .on_ok(move |_, _, app| {
                    if let Some(page) = ok_owner.upgrade() {
                        let operation = (!danger).then(|| ok_operation.clone());
                        page.update(app, |page, cx| page.finish_confirmation(operation, cx));
                    }
                    true
                })
                .on_cancel(move |_, _, app| {
                    if let Some(page) = cancel_owner.upgrade() {
                        page.update(app, |page, cx| page.clear_confirmation(cx));
                    }
                    true
                })
        });
    }

    /// 确认模型在打开前冻结对象、版本、来源和权限，Kit 重建弹层时不重新查询页面状态。
    fn confirmation_dialog(&self, confirmation: &Confirmation) -> Option<ConfirmationDialogModel> {
        match confirmation {
            Confirmation::Install(selection) => {
                let plugin = self.catalog.as_ref().and_then(|catalog| {
                    catalog
                        .catalog
                        .plugins
                        .iter()
                        .find(|plugin| plugin.id == selection.id)
                });
                let name = plugin.map_or_else(
                    || selection.id.as_str().to_owned(),
                    |plugin| label(&plugin.name, selection.id.as_str()).to_owned(),
                );
                let current = self
                    .state
                    .as_ref()
                    .and_then(|state| state.plugins.get(&selection.id));
                let updating = current.is_some_and(|installed| {
                    selection.version.as_semver() > installed.active.as_semver()
                });
                let subtitle = current.filter(|_| updating).map_or_else(
                    || {
                        SharedString::from(format!(
                            "{} · v{}",
                            selection.id.as_str(),
                            selection.version
                        ))
                    },
                    |installed| {
                        SharedString::from(format!(
                            "{} · v{} → v{}",
                            selection.id.as_str(),
                            installed.active,
                            selection.version
                        ))
                    },
                );
                let full_access = plugin
                    .and_then(|plugin| {
                        plugin
                            .versions
                            .iter()
                            .find(|version| version.version == selection.version)
                    })
                    .is_some_and(|version| version.requires_full_access());
                Some(ConfirmationDialogModel {
                    title: if updating {
                        crate::locale::text(
                            format!("更新扩展「{name}」？"),
                            format!("Update “{name}”?"),
                        )
                    } else {
                        crate::locale::text(
                            format!("安装扩展「{name}」？"),
                            format!("Install “{name}”?"),
                        )
                    },
                    subtitle,
                    warning: None,
                    source: plugin.map(|plugin| SharedString::from(plugin.publisher.clone())),
                    full_access,
                    permissions: plugin.map_or_else(Vec::new, |plugin| plugin.permissions.clone()),
                    action: Operation::Install(selection.clone()),
                    action_label: if updating {
                        crate::locale::text("更新", "Update")
                    } else {
                        crate::locale::text("安装", "Install")
                    },
                    danger: false,
                })
            }
            Confirmation::File(path, inspection) => {
                let name = label(&inspection.name, inspection.id.as_str()).to_owned();
                let full_access = inspection
                    .permissions
                    .iter()
                    .any(|permission| permission == "full-access");
                Some(ConfirmationDialogModel {
                    title: crate::locale::text(
                        format!("安装本地扩展「{name}」？"),
                        format!("Install local extension “{name}”?"),
                    ),
                    subtitle: SharedString::from(format!(
                        "{} · v{}",
                        inspection.id.as_str(),
                        inspection.version
                    )),
                    warning: Some(crate::locale::text(
                        "此本地包未经市场审核。",
                        "This local package has not been reviewed by the marketplace.",
                    )),
                    source: Some(crate::locale::text("本地文件", "Local file")),
                    full_access,
                    permissions: inspection.permissions.clone(),
                    action: Operation::File(path.clone(), inspection.clone()),
                    action_label: crate::locale::text("安装", "Install"),
                    danger: false,
                })
            }
            Confirmation::DeleteData(id) => Some(ConfirmationDialogModel {
                title: crate::locale::text(
                    format!("删除扩展数据「{}」？", id.as_str()),
                    format!("Delete extension data for “{}”?", id.as_str()),
                ),
                subtitle: SharedString::from(""),
                warning: Some(crate::locale::text(
                    "此操作不可撤销。",
                    "This cannot be undone.",
                )),
                source: None,
                full_access: false,
                permissions: Vec::new(),
                action: Operation::Delete(id.clone()),
                action_label: crate::locale::text("删除数据", "Delete data"),
                danger: true,
            }),
        }
    }
}

/// 行数据只描述当前条目，按钮动作在渲染时绑定，保证内容、筛选与安装状态共用一个来源。
struct ExtensionRow {
    id: PluginId,
    name: String,
    description: String,
    progress: Option<String>,
    status: Option<(SharedString, RowTone)>,
    action: Option<ExtensionRowAction>,
    installing: bool,
    cancel_enabled: bool,
    selected: bool,
}

/// 安装动作绑定已选中的受信目录版本，界面不能在渲染后拼出下载地址。
struct ExtensionRowAction {
    label: SharedString,
    selection: CatalogSelection,
    disabled: bool,
}

/// 窗口级确认在打开前冻结决策所需文案与权限，Kit 重建弹层时不再读取页面状态。
#[derive(Clone)]
struct ConfirmationDialogModel {
    title: SharedString,
    subtitle: SharedString,
    warning: Option<SharedString>,
    source: Option<SharedString>,
    full_access: bool,
    permissions: Vec<String>,
    action: Operation,
    action_label: SharedString,
    danger: bool,
}

/// 行状态只使用语义角色，不用颜色单独承载含义。
#[derive(Clone, Copy)]
enum RowTone {
    Muted,
    Warning,
    Error,
}

impl RowTone {
    fn color(self, theme: ThemeTokens) -> Hsla {
        match self {
            RowTone::Muted => theme.muted,
            RowTone::Warning => theme.warning,
            RowTone::Error => theme.error,
        }
    }
}

/// 行高由移除分隔线后的两行内容自然撑开；右侧动作与状态共用固定尾部空间。
fn render_extension_row(
    row: ExtensionRow,
    theme: ThemeTokens,
    cx: &mut Context<ExtensionsPage>,
) -> AnyElement {
    let ExtensionRow {
        id,
        name,
        description,
        progress,
        status,
        action,
        installing,
        cancel_enabled,
        selected,
    } = row;
    let id_text = id.as_str().to_owned();
    let select_id = id.clone();
    let action_name = name.clone();
    let action_button = if installing {
        Some(
            Button::new(format!("extension-action-{id_text}"))
                .compact()
                .ghost()
                .label(crate::locale::text("取消", "Cancel"))
                .accessibility_label(crate::locale::text(
                    format!("取消安装 {action_name}"),
                    format!("Cancel install {action_name}"),
                ))
                .disabled(!cancel_enabled)
                .on_click(cx.listener(|page, _, _, cx| {
                    page.cancel();
                    cx.notify();
                }))
                .into_any_element(),
        )
    } else if let Some(action) = action {
        let selection = action.selection;
        let action_label_accessibility = action.label.clone();
        Some(
            Button::new(format!("extension-action-{id_text}"))
                .compact()
                .ghost()
                .label(action.label)
                .accessibility_label(crate::locale::text(
                    format!("{action_label_accessibility} {action_name}"),
                    format!("{action_label_accessibility} {action_name}"),
                ))
                .disabled(action.disabled)
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.open_confirmation(Confirmation::Install(selection.clone()), cx);
                }))
                .into_any_element(),
        )
    } else {
        status.map(|(text, tone)| {
            div()
                .flex_shrink_0()
                .text_size(theme.text_sm)
                .text_color(tone.color(theme))
                .child(text)
                .into_any_element()
        })
    };
    let second_line = progress.or(Some(description)).unwrap_or_default();

    div()
        .id(format!("extension-row-{id_text}"))
        .w_full()
        .min_h(px(56.))
        .flex_shrink_0()
        .px_2()
        .py_2()
        .rounded(theme.control_radius)
        .flex()
        .flex_col()
        .gap_1()
        .when(selected, |row| row.bg(theme.surface_selected()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(theme.surface_hover()))
        })
        .on_click(cx.listener(move |page, _, _, cx| page.select(select_id.clone(), cx)))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .child(
                            Button::new(format!("extension-select-{id_text}"))
                                .compact()
                                .ghost()
                                .w_full()
                                .px_0()
                                .py_0()
                                .max_w_full()
                                .child(div().w_full().min_w(px(0.)).truncate().child(name))
                                .on_click(
                                    cx.listener(move |page, _, _, cx| page.select(id.clone(), cx)),
                                ),
                        ),
                )
                .when_some(action_button, |line, action| line.child(action)),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .truncate()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .child(second_line),
        )
        .into_any_element()
}

/// 分类筛选值保持目录原文；仅示例分类在英文界面显示为 Samples，不影响筛选比较。
fn category_label(category: &str) -> SharedString {
    if category == "示例" {
        crate::locale::text("示例", "Samples")
    } else {
        SharedString::from(category.to_owned())
    }
}

/// 过滤按钮共享同一分段语义：选中项使用持久表面与正文色，未选中保持安静。
fn filter_button(
    id: &'static str,
    label: SharedString,
    active: bool,
    filter: Filter,
    theme: ThemeTokens,
    cx: &mut Context<ExtensionsPage>,
) -> Button {
    Button::new(id)
        .compact()
        .ghost()
        .selected(active)
        .label(label)
        .text_color(if active {
            theme.foreground
        } else {
            theme.muted
        })
        .font_weight(if active {
            theme.settings_medium_font_weight()
        } else {
            FontWeight::NORMAL
        })
        .when(active, |button| button.bg(theme.surface_selected()))
        .on_click(cx.listener(move |page, _, _, cx| {
            page.filter = filter.clone();
            cx.notify();
        }))
}

/// 页头摘要把目录、安装、更新、待重启和离线状态压缩为一行，不随刷新产生额外占位元素。
fn header_summary(
    preview: bool,
    refreshing: bool,
    extensions: usize,
    installed: usize,
    updates: usize,
    pending: usize,
    offline: bool,
) -> SharedString {
    if preview {
        return if refreshing {
            crate::locale::text(
                "示例预览 · 正在获取市场目录",
                "Sample preview · Fetching marketplace catalog",
            )
        } else {
            let mut zh = format!("示例预览 · {extensions} 个示例 · 已安装 {installed}");
            let mut en = format!(
                "Sample preview · {} · {installed} installed",
                english_count(extensions, "sample", "samples")
            );
            if pending > 0 {
                zh.push_str(&format!(" · {pending} 项变更待重启"));
                en.push_str(&format!(
                    " · {} pending restart",
                    english_count(pending, "change", "changes")
                ));
            }
            crate::locale::text(zh, en)
        };
    }
    let mut zh = format!("{extensions} 个扩展 · 已安装 {installed}");
    let mut en = format!(
        "{} · {installed} installed",
        english_count(extensions, "extension", "extensions")
    );
    if updates > 0 {
        zh.push_str(&format!(" · {updates} 个可更新"));
        en.push_str(&format!(
            " · {} available",
            english_count(updates, "update", "updates")
        ));
    }
    if pending > 0 {
        zh.push_str(&format!(" · {pending} 项变更待重启"));
        en.push_str(&format!(
            " · {} pending restart",
            english_count(pending, "change", "changes")
        ));
    }
    if offline {
        zh.push_str(" · 离线缓存");
        en.push_str(" · offline cache");
    }
    crate::locale::text(zh, en)
}

/// 英文摘要按数量选择单复数，避免“1 extensions”这类不自然文案。
fn english_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// 详情版本行先说明当前与可用版本，再补充重启状态，避免用户把待重启误当成已生效。
fn detail_version_line(
    preview: bool,
    available: Option<&CatalogSelection>,
    current: Option<&kerminal_domain::plugins::InstalledPlugin>,
    pending: bool,
    plugin: &CatalogPlugin,
) -> String {
    let mut line = if preview {
        let version = plugin
            .versions
            .first()
            .map_or_else(String::new, |version| version.version.to_string());
        crate::locale::text(format!("示例 · v{version}"), format!("Sample · v{version}"))
            .to_string()
    } else {
        match (current, available) {
            (Some(current), Some(available)) => crate::locale::text(
                format!("当前 v{} · 可用 v{}", current.active, available.version),
                format!(
                    "Current v{} · Available v{}",
                    current.active, available.version
                ),
            )
            .to_string(),
            (Some(current), None) => crate::locale::text(
                format!("当前 v{}", current.active),
                format!("Current v{}", current.active),
            )
            .to_string(),
            (None, Some(available)) => crate::locale::text(
                format!("未安装 · 可用 v{}", available.version),
                format!("Not installed · Available v{}", available.version),
            )
            .to_string(),
            (None, None) => {
                crate::locale::text("不支持当前平台", "Not supported on this platform").to_string()
            }
        }
    };
    if pending {
        line.push_str(&crate::locale::text(" · 重启后生效", " · Restart required"));
    }
    line
}

/// 权限块在详情与确认中复用；完全访问始终先于逐项权限出现，不能藏在列表末尾。
fn permissions_block(permissions: &[String], full_access: bool, theme: ThemeTokens) -> AnyElement {
    let mut block = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text("权限", "Permissions")),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .child(if full_access {
                    crate::locale::text(
                        "完全访问：此扩展以当前用户权限运行",
                        "Full access: this extension runs with the current user's permissions",
                    )
                } else {
                    crate::locale::text(
                        "受限访问：通过声明的宿主权限运行",
                        "Restricted access: uses declared host permissions",
                    )
                }),
        );
    if permissions.is_empty() {
        block = block.child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "无额外权限",
                    "No additional permissions",
                )),
        );
    } else {
        for permission in permissions {
            block = block.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(format!(
                        "{permission} — {}",
                        permission_description(permission)
                    )),
            );
        }
    }
    block.into_any_element()
}

/// README 只来自签名目录；示例和本地条目没有可验签正文，保持详情仅展示内嵌元数据。
fn failure_readme(readme: &Option<String>, preview: bool, plugin: &CatalogPlugin) -> AnyElement {
    if preview {
        return div().into_any_element();
    }
    match readme {
        Some(readme) => super::markdown::render_readme(
            format!("extension-readme-{}", plugin.id.as_str()),
            readme.clone(),
        ),
        None => div().into_any_element(),
    }
}

/// 进度属于当前安装行；下载显示百分比与 MB，避免裸字节数难以判断规模。
fn progress_label(progress: &InstallProgress) -> String {
    match progress {
        InstallProgress::Preparing => crate::locale::text("准备中", "Preparing").to_string(),
        InstallProgress::Downloading { received, total } => {
            let received_mb = *received as f64 / 1_048_576.0;
            if *total == 0 {
                crate::locale::text(
                    format!("下载中 · {received_mb:.1} MB"),
                    format!("Downloading · {received_mb:.1} MB"),
                )
                .to_string()
            } else {
                let percent = (*received as f64 / *total as f64 * 100.).round() as u32;
                let total_mb = *total as f64 / 1_048_576.0;
                crate::locale::text(
                    format!("下载中 · {percent}% · {received_mb:.1}/{total_mb:.1} MB"),
                    format!("Downloading · {percent}% · {received_mb:.1}/{total_mb:.1} MB"),
                )
                .to_string()
            }
        }
        InstallProgress::Verifying => crate::locale::text("校验中", "Verifying").to_string(),
        InstallProgress::Publishing => crate::locale::text("保存中", "Saving").to_string(),
    }
}

/// 权限原名始终可见，未知权限不被猜成安全能力。
fn permission_description(permission: &str) -> SharedString {
    match permission {
        "ui.notify" => crate::locale::text("显示通知", "Show notifications"),
        "ui.dialog" => crate::locale::text(
            "显示确认和输入对话框",
            "Show confirmation and input dialogs",
        ),
        "ui.file-picker" => crate::locale::text("请求用户选择文件", "Ask the user to select files"),
        "clipboard.read" => crate::locale::text("读取剪贴板", "Read the clipboard"),
        "clipboard.write" => crate::locale::text("写入剪贴板", "Write to the clipboard"),
        "open-external" => {
            crate::locale::text("在浏览器打开 HTTPS 链接", "Open HTTPS links in the browser")
        }
        "hosts.read" => {
            crate::locale::text("读取不含凭据的主机列表", "Read hosts without credentials")
        }
        "terminal.write" => crate::locale::text(
            "向所选终端粘贴文本",
            "Paste text into the selected terminal",
        ),
        "events.subscribe" => crate::locale::text("订阅宿主事件", "Subscribe to host events"),
        "secrets" => {
            crate::locale::text("访问此插件的密钥槽", "Access this extension's secret slots")
        }
        "full-access" => crate::locale::text(
            "以当前用户权限运行",
            "Run with the current user's permissions",
        ),
        value if value.starts_with("network:") => {
            crate::locale::text("访问指定网络主机", "Access the specified network host")
        }
        _ => crate::locale::text(
            "查看扩展文档了解此权限",
            "See the extension documentation for this permission",
        ),
    }
}

/// 外部仓库与更新日志链接只接受无凭据的 HTTPS 地址。
fn external_link(
    id: &'static str,
    label: SharedString,
    url: String,
    cx: &mut Context<ExtensionsPage>,
) -> Button {
    let permissions =
        std::collections::BTreeSet::from([kerminal_plugin_protocol::Permission::OpenExternal]);
    let url = crate::plugins::external::view::allowed_external_url(&permissions, &url);
    Button::new(id)
        .compact()
        .ghost()
        .icon(IconName::ExternalLink)
        .label(label)
        .disabled(url.is_none())
        .on_click(cx.listener(move |_, _, _, cx| {
            if let Some(url) = &url {
                cx.open_url(url.as_str());
            }
        }))
}
