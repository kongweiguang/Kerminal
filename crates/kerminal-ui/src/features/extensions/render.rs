//! 设置页内的紧凑插件工作区：统一列表、主从分栏与窄窗单栏。
//!
//! 页面只把目录与安装状态投影为一种阅读关系；已安装置顶、内置与待重启各自成组，
//! 目录刷新、错误和确认都在卡片内部表达，不再用列表顶部大错误条改变整页高度。
//!
//! @author kongweiguang

use super::{
    BuiltinRow, Confirmation, DropInState, EmptyStateKind, ExtensionsEvent, ExtensionsPage,
    Operation, SelectedEntry, StoredActionsPolicy, builtin_display_name, builtin_enabled,
    builtin_pending, builtin_rows, cancelable_failed_change, drop_in_confirmation_operation,
    drop_in_state, empty_state_kind, is_reserved_identity, label, pending_only_failure, selection,
    stored_actions_policy, unconfirmed_drop_in,
};
use crate::theme::ThemeTokens;
use gpui_kit::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Window,
    assets::IconName,
    component::{
        Disableable as _, Icon, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
        popover::Popover,
        resizable::{h_resizable, resizable_panel},
        scroll::ScrollableElement as _,
        spinner::Spinner,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::extensions::{
    CatalogPlugin, CatalogSelection, ExtensionChange, ExtensionIcon, ExternalPluginStartupMode,
    InstallProgress,
};
use kerminal_domain::plugins::{
    PendingPluginChange, PluginId, PluginInstallStatus, PluginSource, PluginStoreState,
};
use std::collections::BTreeSet;

use crate::plugins::tools::ToolIcon;

/// 将滚动条叠层固定在视口边缘，避免 GPUI 把它和内容一起平移。
/// 保留原滚动容器及句柄，让键盘定位仍使用直接子元素的索引。
///
/// @author kongweiguang
fn extension_scroll_viewport(content: impl IntoElement, scroll: &ScrollHandle) -> gpui_kit::Div {
    div()
        .relative()
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .child(content)
        .vertical_scrollbar(scroll)
}

impl Render for ExtensionsPage {
    /// 页头与单张主从卡片共同承担设置页的既有留白，卡片内部各自滚动。
    /// 确认交给窗口级 Kit Dialog，页面不再自行绘制遮罩或手写焦点陷阱。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 页面正在绘制即视为可见；退出在门禁之后被中止时下一次重绘自动恢复可操作状态。
        self.active = true;
        let theme = ThemeTokens::current(window, cx).for_settings();
        // 图标只从应用层结果或内置映射投影；这里启动补取，但结果到达前统一用
        // 固定尺寸占位，避免图标迟到改变列表行高或详情高度。
        self.ensure_icons(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let plugins = self
            .catalog
            .as_ref()
            .map_or(&[][..], |snapshot| snapshot.catalog.plugins.as_slice());
        let mut categories = BTreeSet::new();
        for plugin in plugins {
            categories.extend(plugin.categories.iter().cloned());
        }
        // 批量范围只取真实可更新的已安装记录，不受搜索、分类或内置组折叠影响；
        // 行内更新按钮与“全部更新”共用同一判定。
        let update_count = self.update_candidates().len();
        let entries = self.list_entries(plugins, &query);
        let narrow = self.is_narrow(window);
        let detail_visible = !narrow || (self.selected.is_some() && !self.back_to_list);
        // 行内进度只覆盖真正显示在列表里的安装行；本地预检、目录外文件安装以及
        // 窄窗详情遮住列表时都没有可见行可挂，必须在页头固定槽位显示活动与取消。
        let list_visible = !narrow || !detail_visible;
        // 宽窄转换或重新进入可能让此前被遮住的高亮变成可见详情；这里按需补读
        // 一次 README，并由 readme_attempted 去重，避免每帧重发。
        self.ensure_readme_for_visible_detail(detail_visible, cx);
        // 列表/详情挂载面切换时做一次焦点交接，避免焦点留在已卸载的一侧。
        self.hand_over_focus_on_layout_change(list_visible, window, cx);
        let show_activity = self.inspecting
            || (self.progress.is_some()
                && (!list_visible
                    || !entries
                        .iter()
                        .any(|entry| matches!(entry, ListEntry::Row(row) if row.installing))));
        let header = self.render_header(show_activity, theme, cx);
        let toolbar = self.render_toolbar(&categories, update_count, theme, cx);
        let list = self.render_list(&entries, theme, cx);
        let detail = self.render_detail(narrow, theme, cx);
        let inner: AnyElement = if narrow {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .child(if detail_visible { detail } else { list })
                .into_any_element()
        } else {
            let rem = window.rem_size();
            h_resizable("extensions-split")
                // 列表保持用户拖出的首选宽度，窗口变窄时由详情吸收剩余空间。
                .preserve_panel_size_on_container_resize(0)
                .child(
                    resizable_panel()
                        .size(rem * 22.0)
                        .size_range(rem * 18.0..rem * 28.0)
                        .child(list),
                )
                .child(resizable_panel().child(detail))
                .into_any_element()
        };
        // 以真实正文宽度决定分栏或单栏；整窗宽度包含导航与留白，不能直接拿来猜。
        let measured = self.content_width.clone();
        // prepaint 回调拿到的是 &mut App，只能按实体 ID 通知；宽度未变化时不通知，
        // 避免测量与重绘互相触发形成循环。
        let entity_id = cx.entity_id();
        // 测量回调定义在普通 Div 上；加 id 会变成 Stateful<Div> 而丢失该 API，
        // 这里不需要交互身份，保持普通容器即可。
        let body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(inner)
            .on_children_prepainted(move |bounds, _, cx| {
                let Some(bounds) = bounds.first() else {
                    return;
                };
                let width = bounds.size.width;
                if measured.get() != Some(width) {
                    measured.set(Some(width));
                    cx.notify(entity_id);
                }
            });

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
            .gap_3()
            .child(header)
            .child(card)
    }
}

impl ExtensionsPage {
    /// 紧凑页头只保留对象、来源状态和低频动作；分组计数下放到列表标题，
    /// 不再在页头重复一遍总计，避免同一事实出现两次并挤占横向空间。
    ///
    /// 商店来源与文件安装始终收在同一个更多菜单里：两个入口都低频，且宽窗直接
    /// 平铺文字按钮会随语言和字号膨胀、把刷新挤到下一行；窄窗复用同一菜单，
    /// 入口形态不随窗口宽度改变，也避免宽窄切换改变可发现性。
    ///
    /// @author kongweiguang
    fn render_header(
        &self,
        show_activity: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending_count = self.state.as_ref().map_or(0, |state| state.pending.len());
        let mode = self.service.startup_mode();
        let can_restart_normally = mode == ExternalPluginStartupMode::AutomaticSafeMode;
        // 目录失败不再新增错误条或独立感叹号：同一刷新按钮保持固定尺寸，
        // 失败原因进入 tooltip 与无障碍名，点击即重新请求目录。这样失败与重试
        // 共用唯一入口，页头高度和下方列表位置都不随错误出现而跳动。
        let refresh_label = if let Some(error) = &self.catalog_error {
            crate::locale::text(
                format!("刷新目录失败：{error}。点击重试"),
                format!("Catalog refresh failed: {error}. Click to retry."),
            )
        } else {
            crate::locale::text("刷新目录", "Refresh catalog")
        };
        let refresh_button = if self.refreshing {
            let button = Button::new("extensions-refresh")
                .compact()
                .ghost()
                .w(theme.metrics.control_height)
                .h(theme.metrics.control_height)
                .child(Spinner::new().small().color(theme.muted));
            if self.can_cancel_catalog_refresh() {
                button
                    .accessibility_label(crate::locale::text(
                        "取消刷新目录",
                        "Cancel catalog refresh",
                    ))
                    .tooltip(crate::locale::text(
                        "取消刷新目录",
                        "Cancel catalog refresh",
                    ))
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.cancel_refresh();
                        cx.notify();
                    }))
            } else {
                button.disabled(true)
            }
        } else {
            Button::new("extensions-refresh")
                .compact()
                .ghost()
                .w(theme.metrics.control_height)
                .h(theme.metrics.control_height)
                .icon(IconName::RefreshCw)
                .text_color(if self.catalog_error.is_some() {
                    theme.error
                } else {
                    theme.foreground
                })
                .disabled(self.busy)
                .debug_selector(|| "extensions-refresh".into())
                .accessibility_label(refresh_label.clone())
                .tooltip(refresh_label)
                .on_click(cx.listener(|page, _, _, cx| page.refresh(cx)))
        };
        // 菜单项复用同一禁用门禁，不因入口收进菜单绕过状态未知或忙碌时的写入限制。
        let file_disabled = self.busy || self.state.is_none() || self.state_error.is_some();
        // 来源切换只依赖目录与写入互斥，不要求已读到权威安装状态：安装状态未知时
        // 用户仍应能修正来源，否则错误来源会把页面永久卡在不可用状态。
        let source_disabled = self.busy
            || self.writing
            || self.source_saving
            || self.inspecting
            || self.batch.is_some();
        let page_entity = cx.entity();
        let file_action = div()
            .debug_selector(|| "extensions-more".into())
            .child(
                Button::new("extensions-more")
                    .compact()
                    .ghost()
                    .w(theme.metrics.control_height)
                    .h(theme.metrics.control_height)
                    .icon(IconName::Ellipsis)
                    // 安装与来源都不可用时才整体禁用；任一项可用就保留菜单入口。
                    .disabled(file_disabled && source_disabled)
                    .accessibility_label(crate::locale::text("更多操作", "More actions"))
                    .tooltip(crate::locale::text("更多操作", "More actions"))
                    .dropdown_menu(move |menu, _, _| {
                        let menu = crate::ui_popup::dropdown_menu(menu, theme);
                        let source_page = page_entity.clone();
                        let file_page = page_entity.clone();
                        menu.item(
                            PopupMenuItem::new(crate::locale::text("商店来源…", "Store source…"))
                                .disabled(source_disabled)
                                .on_click(move |_, window, app| {
                                    source_page.update(app, |page, cx| {
                                        page.open_source_editor(window, cx);
                                    });
                                }),
                        )
                        .item(
                            PopupMenuItem::new(crate::locale::text(
                                "从文件安装…",
                                "Install from file…",
                            ))
                            .disabled(file_disabled)
                            .on_click(move |_, _, app| {
                                file_page.update(app, |page, cx| page.choose_file(cx));
                            }),
                        )
                    }),
            )
            .into_any_element();
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            // 页标题使用主题的 heading 语义字号，随系统缩放
                            // 一起变化；固定物理像素会在 200% 下失去层级。
                            .text_size(theme.text_heading)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(crate::locale::text("扩展", "Extensions")),
                    )
                    .when_some(self.render_startup_mode(mode, theme), |title, status| {
                        title.child(status)
                    })
                    .when_some(self.render_header_status(theme, cx), |title, status| {
                        title.child(status)
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .when(pending_count > 0 || can_restart_normally, |actions| {
                        let label = if can_restart_normally {
                            crate::locale::text("正常重启", "Restart normally")
                        } else {
                            crate::locale::text("立即重启", "Restart now")
                        };
                        actions.child(
                            Button::new("extensions-restart")
                                .h(theme.metrics.control_height)
                                .label(label)
                                .disabled(source_disabled)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(ExtensionsEvent::RestartRequested)
                                })),
                        )
                    })
                    .when(show_activity, |actions| {
                        // 固定高度槽位：预检/目录外安装出现或消失都不改变页头高度，
                        // 也不挤动下方列表。取消按钮复用页面唯一的安装/批量取消令牌。
                        actions.child(
                            div()
                                .h(theme.metrics.control_height)
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(Spinner::new().small().color(theme.muted))
                                .child(
                                    Button::new("extensions-activity-cancel")
                                        .compact()
                                        .ghost()
                                        .w(theme.metrics.control_height)
                                        .h(theme.metrics.control_height)
                                        .icon(IconName::X)
                                        .disabled(self.cancel.is_none())
                                        .accessibility_label(crate::locale::text(
                                            "取消当前操作",
                                            "Cancel the current operation",
                                        ))
                                        .tooltip(crate::locale::text(
                                            "取消当前操作",
                                            "Cancel the current operation",
                                        ))
                                        .on_click(cx.listener(|page, _, _, cx| {
                                            page.cancel();
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .child(refresh_button)
                    .child(file_action),
            )
            .into_any_element()
    }

    /// 页头只说明来源状态，不重复列表组计数；状态未知和读取中占用固定副标题，
    /// 目录失败只由刷新按钮原地反馈。副标题不随刷新失败或 stale 回退出现/消失，
    /// 避免状态变化时页头高度和下方列表位置跳动。
    ///
    /// @author kongweiguang
    pub(super) fn header_status(&self) -> Option<SharedString> {
        if self.state_error.is_some() {
            Some(crate::locale::text(
                "已安装状态未确认",
                "Installed state unconfirmed",
            ))
        } else if self.state.is_none() {
            Some(crate::locale::text(
                "正在读取已安装状态…",
                "Reading the installed state…",
            ))
        } else {
            None
        }
    }

    /// 工具栏只放全局搜索、目录分类和批量更新；错误原因与恢复入口移到页头，
    /// 避免独立感叹号占住工具栏并和批量入口争夺宽度。
    ///
    /// 统一列表没有第二排模式筛选：分类只收窄“可安装”目录组，批量更新按钮
    /// 始终可见，窄窗换行时搜索与批量入口仍可达。
    ///
    /// @author kongweiguang
    fn render_toolbar(
        &self,
        categories: &BTreeSet<String>,
        update_count: usize,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 搜索框外包一层固定身份容器：窄窗验收需要证明搜索入口仍可达，
        // 同时保持输入组件自身的样式与焦点行为不变。
        let search_input = div()
            .debug_selector(|| "extensions-search".into())
            .flex_1()
            .min_w(px(180.))
            .child(gpui_kit::Styled::h(
                Input::new(&self.search)
                    .prefix(IconName::Search)
                    .cleanable(true)
                    .bg(theme.field_surface())
                    .border_color(theme.strong_border())
                    .rounded(theme.control_radius)
                    .font_weight(FontWeight::NORMAL)
                    .w_full(),
                theme.metrics.control_height,
            ));
        let mut toolbar = div()
            .px_4()
            .py_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(search_input);
        let all_categories = crate::locale::text("全部分类", "All categories");
        let category_button_label = self.filter.category.as_ref().map_or_else(
            || all_categories.clone(),
            |category| category_label(category),
        );
        let categories: Vec<String> = categories.iter().cloned().collect();
        let page_entity = cx.entity();
        let menu_categories = categories.clone();
        let current = self.filter.category.clone();
        toolbar = toolbar.child(
            Button::new("extensions-category")
                .compact()
                .ghost()
                .icon(IconName::ListFilter)
                .label(category_button_label)
                .dropdown_caret(true)
                .when(self.filter.category.is_some(), |button| {
                    button
                        .bg(theme.surface_selected())
                        .text_color(theme.foreground)
                })
                .dropdown_menu(move |menu, _, _| {
                    let menu = crate::ui_popup::dropdown_menu(menu, theme);
                    let all_page = page_entity.clone();
                    let mut menu = menu.item(
                        PopupMenuItem::new(all_categories.clone())
                            .checked(current.is_none())
                            .on_click(move |_, _, app| {
                                all_page.update(app, |page, cx| {
                                    page.filter.category = None;
                                    cx.notify();
                                });
                            }),
                    );
                    for category in &menu_categories {
                        let page = page_entity.clone();
                        let target = category.clone();
                        let checked = current.as_deref() == Some(category.as_str());
                        menu = menu.item(
                            PopupMenuItem::new(category_label(category))
                                .checked(checked)
                                .on_click(move |_, _, app| {
                                    let target = target.clone();
                                    page.update(app, |page, cx| {
                                        page.filter.category = Some(target.clone());
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    menu
                }),
        );
        // 批量更新按钮同时承担“可更新”筛选原本的发现职责：数量来自全体已安装
        // 记录，不受搜索与折叠影响；状态或目录未知时禁用并说明可恢复路径。
        let batch_disabled = update_count == 0
            || self.busy
            || self.offline
            || self.state_error.is_some()
            || self.state.is_none()
            || self.catalog.is_none();
        let batch_tooltip = if self.state.is_none() || self.state_error.is_some() {
            crate::locale::text(
                "已安装状态未确认，刷新状态后才能更新",
                "The installed state is unconfirmed; refresh it before updating",
            )
        } else if self.catalog.is_none() {
            crate::locale::text(
                "目录尚未读取，刷新目录后才能判断更新",
                "The catalog has not been read; refresh it to determine updates",
            )
        } else if self.offline {
            crate::locale::text(
                "离线时不能更新；刷新目录后重试",
                "Can’t update while offline; refresh the catalog and retry",
            )
        } else if update_count == 0 {
            crate::locale::text("没有可更新的扩展", "No updates available")
        } else {
            crate::locale::text(
                format!("更新全部 {update_count} 个可更新扩展"),
                format!("Update all {update_count} available extensions"),
            )
        };
        toolbar = toolbar.child(
            div()
                .debug_selector(|| "extensions-update-all".into())
                .child(
                    Button::new("extensions-update-all")
                        .compact()
                        .ghost()
                        // 批量更新保持纯文字动作；数量放进 tooltip 与确认弹层，
                        // 不让按钮宽度随候选数变化，也避免再引入图标或感叹号
                        // 与目录刷新争夺同一行的注意力。
                        .label(crate::locale::text("全部更新", "Update all"))
                        .disabled(batch_disabled)
                        .tooltip(batch_tooltip)
                        .on_click(cx.listener(|page, _, _, cx| page.open_batch_update(cx))),
                ),
        );
        toolbar.into_any_element()
    }

    /// 停用原因只在例外会话出现，并独立于目录错误和持久启用状态，避免刷新时提示消失。
    fn render_startup_mode(
        &self,
        mode: ExternalPluginStartupMode,
        theme: ThemeTokens,
    ) -> Option<AnyElement> {
        let message = match mode {
            ExternalPluginStartupMode::Normal => return None,
            ExternalPluginStartupMode::AutomaticSafeMode => crate::locale::text(
                "安全模式下未加载外部插件。",
                "External plugins are paused in safe mode.",
            ),
            ExternalPluginStartupMode::ExplicitSafeMode => crate::locale::text(
                "安全模式下未加载外部插件。请退出后正常打开 Kerminal。",
                "External plugins are paused in safe mode. Quit and open Kerminal normally to load them.",
            ),
        };
        Some(
            div()
                .text_size(theme.text_sm)
                .text_color(theme.warning)
                .child(message)
                .into_any_element(),
        )
    }

    /// 页头副标题只承载当前不可忽略的状态：普通状态用文字说明，带恢复动作的状态
    /// 改成文字按钮并打开同一个“原因 + 恢复”弹层。目录错误不在这里重复，它由
    /// 固定尺寸的刷新按钮直接承载；其余安装、状态和 drop-in 错误没有别的入口，
    /// 不能因为移除工具栏感叹号而失去原因与恢复路径。
    ///
    /// @author kongweiguang
    fn render_header_status(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.has_recoverable_status() {
            return Some(self.status_popover(self.status_trigger_label(), theme, cx));
        }
        self.header_status().map(|status| {
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(status)
                .into_any_element()
        })
    }

    /// 目录失败已有刷新按钮这一独立重试入口；状态、drop-in 和操作失败没有
    /// 其他可见载体，必须在页头保留可点击的恢复入口，避免“删感叹号”变成吞错。
    ///
    /// @author kongweiguang
    fn has_recoverable_status(&self) -> bool {
        self.state_error.is_some() || self.drop_in_error.is_some() || self.status.is_some()
    }

    /// 触发文字直接说明当前最重要的问题；弹层再展开全部原因，避免副标题随错误
    /// 数量增长而换行。操作警告与错误使用不同语气，不能把警告说成失败。
    ///
    /// @author kongweiguang
    fn status_trigger_label(&self) -> SharedString {
        if self.state_error.is_some() {
            self.header_status().unwrap_or_else(|| {
                crate::locale::text("已安装状态未确认", "Installed state unconfirmed")
            })
        } else if self.drop_in_error.is_some() {
            crate::locale::text("手动放入检查失败", "Drop-in inspection failed")
        } else if self.status_is_warning {
            crate::locale::text("操作结果需要注意", "Operation result needs attention")
        } else {
            crate::locale::text("操作未完成", "Operation incomplete")
        }
    }

    /// 状态弹层只承载状态、drop-in 与操作失败：目录失败已有固定尺寸的刷新按钮
    /// 作为唯一重试入口，再在该弹层重复一份会让两个恢复入口读写门禁不同步。
    ///
    /// @author kongweiguang
    pub(super) fn status_messages(&self) -> Vec<(SharedString, String)> {
        let mut messages: Vec<(SharedString, String)> = Vec::new();
        if let Some(error) = &self.state_error {
            messages.push((
                crate::locale::text("已安装状态", "Installed state"),
                error.clone(),
            ));
        }
        if let Some(error) = &self.drop_in_error {
            messages.push((
                crate::locale::text("手动放入检查", "Drop-in inspection"),
                error.clone(),
            ));
        }
        if let Some(error) = &self.status {
            // 操作失败即使当前选中了别的对象也必须保留在状态弹层里；
            // 详情只在反馈来源与当前对象一致时重复展示，不能让一次点击把
            // 失败原因和恢复入口一起藏掉。
            let scope = self.feedback_entry.as_ref().map_or_else(
                || crate::locale::text("操作", "Operation"),
                |entry| {
                    crate::locale::text(
                        format!("操作 · {}", entry.id().as_str()),
                        format!("Operation · {}", entry.id().as_str()),
                    )
                },
            );
            messages.push((scope, error.clone()));
        }
        messages
    }

    /// 目录、状态、drop-in 与操作失败共用一个气泡：入口放在页头副标题，点击才
    /// 展开原因与恢复按钮。入口是带文字的普通 Button，键盘可聚焦，不依赖颜色
    /// 或图标表达错误；这样页头不再有只有一个感叹号的独立控件。
    ///
    /// @author kongweiguang
    fn status_popover(
        &self,
        label: SharedString,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let messages = self.status_messages();
        let warning =
            self.status_is_warning && self.state_error.is_none() && self.drop_in_error.is_none();
        let tone = if warning { theme.warning } else { theme.error };
        let updated = self.catalog.as_ref().and_then(|snapshot| {
            chrono::DateTime::from_timestamp(snapshot.fetched_at as i64, 0).map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
        });
        // 权威状态未知时旧操作重试会被中央门禁拒绝；此时只提供刷新状态，
        // 让用户走“回读成功后再操作”的恢复路径。
        let retry_operation = self
            .state_error
            .is_none()
            .then(|| self.retry_operation.clone())
            .flatten();
        // 批量失败/取消后的重试必须重新计算剩余计划并重新确认，不能重放旧身份；
        // 状态未知或离线时与工具栏按钮保持同一门禁，只允许刷新。
        let retry_batch = self.retry_batch && self.state_error.is_none() && !self.offline;
        // 忙态下重试/刷新都会被页面门禁拒绝，按钮直接禁用，避免无反馈的空点击。
        let busy = self.busy;
        let retry_label = if retry_operation.is_some() {
            crate::locale::text("重试操作", "Retry operation")
        } else if retry_batch {
            crate::locale::text("重试更新", "Retry update")
        } else {
            crate::locale::text("刷新状态", "Refresh state")
        };
        let page = cx.entity();
        Popover::new("extensions-status-popover")
            .auto_flip()
            .trigger(
                // Popover 触发器必须是可聚焦控件；把调试身份放在按钮上，
                // 回归才能确认恢复入口位于页头而不是工具栏。
                Button::new("extensions-status")
                    .debug_selector(|| "extensions-status".into())
                    .compact()
                    .ghost()
                    .label(label)
                    .text_size(theme.text_xs)
                    .text_color(tone)
                    .accessibility_label(crate::locale::text(
                        "查看扩展状态与恢复操作",
                        "View extension status and recovery actions",
                    ))
                    .tooltip(crate::locale::text(
                        "查看错误原因与恢复操作",
                        "View the cause and recovery actions",
                    )),
            )
            .content(move |_, _, _| {
                let mut content = div()
                    .w(px(320.))
                    .max_h(px(280.))
                    .overflow_y_scrollbar()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_size(theme.text_sm)
                    .text_color(theme.foreground);
                for (scope, message) in &messages {
                    content = content.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(scope.clone()),
                            )
                            .child(div().text_color(tone).child(message.clone())),
                    );
                }
                if let Some(updated) = &updated {
                    content = content.child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                format!("上次更新 · {updated}"),
                                format!("Last updated · {updated}"),
                            )),
                    );
                }
                let page = page.clone();
                let retry_operation = retry_operation.clone();
                let retry_label = retry_label.clone();
                content.child(
                    div().flex().justify_end().gap_2().child(
                        Button::new("extensions-status-retry")
                            .compact()
                            .ghost()
                            .disabled(busy)
                            .debug_selector(|| "extensions-status-retry".into())
                            .accessibility_label(retry_label.clone())
                            .label(retry_label)
                            .on_click(move |_, _, app| {
                                page.update(app, |page, cx| {
                                    if let Some(retry) = retry_operation.clone() {
                                        page.run_operation_with_feedback(
                                            retry.operation,
                                            retry.feedback,
                                            cx,
                                        );
                                    } else if retry_batch {
                                        page.open_batch_update(cx);
                                    } else {
                                        page.activate(cx);
                                    }
                                });
                            }),
                    ),
                )
            })
            .into_any_element()
    }

    /// 列表与详情各自滚动；键盘焦点留在列表容器上，行本身不再抢焦点。
    ///
    /// 渲染直接消费与键盘导航同源的条目序列：组标题和行共用一套序号，
    /// 折叠后的内置组仍保留标题，不会出现“可见行”与“滚动子元素”两套索引。
    ///
    /// @author kongweiguang
    fn render_list(
        &self,
        entries: &[ListEntry],
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = div()
            .id("extensions-list")
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .track_focus(&self.list_focus)
            .on_key_down(cx.listener(Self::handle_list_key))
            .p_2()
            .flex()
            .flex_col()
            .gap_1();
        // 搜索命中会临时展开内置组；箭头必须反映实际可见状态，而不是用户的
        // 持久展开选择，否则标题说“收起”但行已经显示。
        let builtin_expanded =
            self.builtin_expanded || !self.search.read(cx).value().trim().is_empty();
        // 首个实际出现的组贴列表顶部，后续组用更大的上间距呼吸；只看渲染顺序，
        // 不假设某个组一定存在。
        let mut first_group = true;
        for entry in entries {
            match entry {
                ListEntry::Header { group, count } => {
                    list = list.child(group_header(
                        *group,
                        *count,
                        builtin_expanded,
                        first_group,
                        theme,
                        cx,
                    ));
                    first_group = false;
                }
                ListEntry::Row(row) => {
                    list = list.child(render_extension_row(
                        (**row).clone(),
                        &self.list_focus,
                        self.batch.is_some(),
                        theme,
                        cx,
                    ));
                }
            }
        }
        if entries.is_empty() {
            list = list.child(self.empty_state(theme, cx));
        }
        extension_scroll_viewport(list, &self.list_scroll).into_any_element()
    }

    /// 空态按搜索、分类与两条来源的加载/失败状态区分；失败不铺错误条，
    /// 也不伪造可安装内容。已安装或内置组仍可见时不会走到这里。
    fn empty_state(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).value().trim().to_owned();
        let kind = empty_state_kind(
            &query,
            self.filter.category.as_deref(),
            self.catalog.is_some(),
            self.catalog_error.is_some(),
            self.state.is_some(),
            self.state_error.is_some(),
        );
        let mut empty = div()
            .w_full()
            .p_6()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .text_align(gpui_kit::TextAlign::Center)
            .text_size(theme.text_sm)
            .text_color(theme.muted);
        match kind {
            EmptyStateKind::SearchNoMatch => {
                empty = empty
                    .child(crate::locale::text(
                        "没有匹配的扩展。",
                        "No matching extensions.",
                    ))
                    .child(
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
            EmptyStateKind::CatalogUnavailable => {
                // 目录重试只保留页头固定尺寸的刷新按钮；空态再放一个同名重试会
                // 产生第二个入口，并在失败/恢复时让卡片高度和焦点顺序来回变化。
                empty = empty.child(crate::locale::text(
                    "市场目录暂不可用",
                    "Marketplace catalog unavailable",
                ));
            }
            EmptyStateKind::CatalogReading => {
                empty = empty.child(crate::locale::text(
                    "正在读取市场目录…",
                    "Reading the marketplace catalog…",
                ));
            }
            EmptyStateKind::CategoryEmpty(category) => {
                empty = empty.child(crate::locale::text(
                    format!("分类「{category}」暂无扩展"),
                    format!("No extensions in “{category}”"),
                ));
            }
            EmptyStateKind::StateUnknown => {
                empty = empty
                    .child(crate::locale::text(
                        "已安装状态未确认，暂时无法列出扩展。",
                        "The installed state is unconfirmed; extensions cannot be listed yet.",
                    ))
                    .child(
                        Button::new("extensions-state-retry")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("刷新状态", "Refresh state"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|page, _, _, cx| page.activate(cx))),
                    );
            }
            EmptyStateKind::StateReading => {
                empty = empty.child(crate::locale::text(
                    "正在读取已安装状态…",
                    "Reading the installed state…",
                ));
            }
            EmptyStateKind::Empty => {
                empty = empty.child(crate::locale::text(
                    "尚未安装或发现扩展。",
                    "No extensions installed or available yet.",
                ));
            }
        }
        empty.into_any_element()
    }

    /// 详情标题与主操作区固定在上方，权限与 README 在下方独立滚动。
    fn render_detail(
        &self,
        narrow: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut detail = div()
            .id("extension-detail")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            // 窄窗详情显示时列表已卸载，Escape 必须由详情焦点链处理并交还列表焦点；
            // 否则事件冒泡到 Root 会关闭整个设置页。
            .track_focus(&self.detail_focus)
            .on_key_down(cx.listener(Self::handle_detail_key));
        if narrow {
            detail = detail.child(
                div()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    // 返回入口是窄窗唯一的列表导航，必须贴左而不是被行容器居中；
                    // 与工具栏搜索和详情正文保持同一阅读起点。
                    .flex()
                    .items_center()
                    .justify_start()
                    .child(
                        // Button 自身不记录 debug selector；等尺寸包裹层让窄窗
                        // 回归能确认返回入口保持内容宽度而不是被行容器拉满。
                        div()
                            .debug_selector(|| "extension-back-to-list".into())
                            .child(
                                Button::new("extension-back-to-list")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::ArrowLeft)
                                    .label(crate::locale::text("返回列表", "Back to list"))
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        page.back_to_list = true;
                                        window.focus(&page.list_focus, cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        let mut fixed = div()
            .w_full()
            .flex_shrink_0()
            // 头部区固定在上方，用舒展内边距与底部细分隔线划分层级；
            // 下方正文独立滚动，长 README 不会把身份与主操作顶走。
            .px_6()
            .py_5()
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap_4();
        let mut scroll = div()
            .id("extension-detail-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll)
            .px_6()
            .py_5()
            .flex()
            .flex_col()
            .gap_5();

        let selected_reserved = self
            .selected_reserved_record()
            .map(|(id, _installed)| id.clone());
        let selected_builtin = self.selected_builtin_unit().cloned();

        if let Some(id) = selected_reserved {
            let (status, tone) = drop_in_status_label(DropInState::ReservedIdentity);
            fixed = fixed
                .child(detail_identity(
                    ToolIcon::builtin(IconName::Puzzle),
                    id.as_str().to_owned(),
                    Some(
                        crate::locale::text("身份冲突 · 不可加载", "Identity conflict · not loadable")
                            .to_string(),
                    ),
                    Some(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(tone.color(theme))
                            .child(status)
                            .into_any_element(),
                    ),
                    theme,
                ))
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.foreground)
                        .child(crate::locale::text(
                            "该记录占用了内置单元身份，不能作为扩展加载。",
                            "This record occupies a built-in identity and cannot be loaded as an extension.",
                        )),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "磁盘上的冲突材料仍保留；移除对应目录后才能恢复该身份。",
                            "The conflicting material remains on disk; remove its directory to free the identity.",
                        )),
                );
            if let Some(feedback) =
                self.feedback_block(&SelectedEntry::Plugin(id.clone()), true, theme, cx)
            {
                fixed = fixed.child(feedback);
            }
        } else if let Some(unit) = selected_builtin {
            let enabled = builtin_enabled(self.state.as_ref(), &unit.id);
            let pending = builtin_pending(self.state.as_ref(), &unit.id);
            let builtin_note = if unit.can_disable {
                crate::locale::text(
                    "内置插件，可在此启用或停用。",
                    "Built-in extension; enable or disable it here.",
                )
            } else {
                crate::locale::text(
                    "内置插件 · 核心，始终启用。",
                    "Built-in extension · Core, always enabled.",
                )
            };
            fixed = fixed.child(detail_identity(
                self.entry_icon(&SelectedEntry::Builtin(unit.id.clone())),
                builtin_display_name(&unit),
                Some(unit.id.as_str().to_owned()),
                Some(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(builtin_note)
                        .into_any_element(),
                ),
                theme,
            ));
            let mut actions = div().w_full().flex().flex_col().gap_2();
            if let Some(error) = &self.state_error {
                actions = actions.child(state_error_note(error, theme));
            }
            if pending {
                actions = actions.child(pending_restart_text(theme));
            } else if !unit.can_disable {
                actions = actions.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "核心插件不能停用。",
                            "The core extension cannot be disabled.",
                        )),
                );
            } else {
                let id = unit.id.clone();
                let feedback = SelectedEntry::Builtin(id.clone());
                actions = actions.child(
                    Button::new(format!("extension-builtin-toggle-{}", id.as_str()))
                        .compact()
                        .ghost()
                        .label(if enabled {
                            crate::locale::text("禁用", "Disable")
                        } else {
                            crate::locale::text("启用", "Enable")
                        })
                        .disabled(self.busy || pending || self.state_error.is_some())
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation_with_feedback(
                                Operation::Change(if enabled {
                                    ExtensionChange::Disable(id.clone())
                                } else {
                                    ExtensionChange::Enable(id.clone())
                                }),
                                Some(feedback.clone()),
                                cx,
                            )
                        })),
                );
            }
            if let Some(feedback) =
                self.feedback_block(&SelectedEntry::Builtin(unit.id.clone()), false, theme, cx)
            {
                actions = actions.child(feedback);
            }
            fixed = fixed.child(actions);
        } else if let Some(plugin) = self.selected_plugin().cloned() {
            let current = self
                .state
                .as_ref()
                .and_then(|state| state.plugins.get(&plugin.id))
                .cloned();
            let pending_kind = self.state.as_ref().and_then(|state| {
                state
                    .pending
                    .iter()
                    .find(|change| change.plugin_id() == &plugin.id)
            });
            let pending_change = pending_kind.is_some();
            // 只有启动应用当前 pending 失败留下的绑定才算失败；遗留无绑定 failure
            // 不能借用给后来排队的新变更，否则详情会把正常提交误报成失败。
            let pending_failed = self
                .state
                .as_ref()
                .is_some_and(|state| cancelable_failed_change(state, &plugin.id).is_some());
            let available = selection(
                &plugin,
                &self.host_api,
                self.catalog.as_ref().map(|snapshot| &snapshot.identity),
            );
            // 已安装记录只复用统一资格判定；未安装目录项仍按平台可用版本提供安装。
            let install_target = match current.as_ref() {
                Some(_) => self.update_selection(&plugin.id),
                None => available.clone(),
            };
            let installing = self.active_install.as_ref() == Some(&plugin.id);
            let display_title = label(&plugin.name, plugin.id.as_str()).to_owned();
            // 标题已等于身份时不再重复同一 ID；只有人类可读名称与 ID 不同时
            // 才把 publisher · id 作为消歧信息保留。
            let subtitle = if display_title == plugin.id.as_str() {
                plugin.publisher.clone()
            } else {
                format!("{} · {}", plugin.publisher, plugin.id.as_str())
            };
            fixed = fixed
                .child(detail_identity(
                    self.entry_icon(&SelectedEntry::Plugin(plugin.id.clone())),
                    display_title,
                    (!subtitle.is_empty()).then_some(subtitle),
                    Some(
                        div()
                            .debug_selector(move || {
                                if pending_failed {
                                    "extension-detail-version-failed".into()
                                } else {
                                    "extension-detail-version".into()
                                }
                            })
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(detail_version_line(
                                available.as_ref(),
                                current.as_ref(),
                                pending_kind,
                                pending_failed,
                            ))
                            .into_any_element(),
                    ),
                    theme,
                ))
                .child(
                    div()
                        .w_full()
                        .text_size(theme.text_sm)
                        .text_color(theme.foreground)
                        .child(label(&plugin.description, "").to_owned()),
                );

            if let Some(error) = &self.state_error {
                fixed = fixed.child(state_error_note(error, theme));
            }
            // 左侧是管理动作，右侧是外部链接；两组分开排列，避免“删除扩展数据…”
            // 与仓库链接挤在一起。窄窗时两组各自换行，主按钮仍保持内容宽度。
            let mut action_bar = div().flex().flex_wrap().items_center().gap_2();
            if installing {
                action_bar = action_bar
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
                            // 批量执行共用页面级取消令牌；文案必须说明取消的是整批，
                            // 不能让用户以为只跳过当前项。
                            .label(if self.batch.is_some() {
                                crate::locale::text("取消批量更新", "Cancel batch update")
                            } else {
                                crate::locale::text("取消安装", "Cancel install")
                            })
                            .accessibility_label(if self.batch.is_some() {
                                crate::locale::text("取消批量更新", "Cancel batch update")
                            } else {
                                crate::locale::text("取消安装", "Cancel install")
                            })
                            .disabled(self.cancel.is_none())
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.cancel();
                                cx.notify();
                            })),
                    );
            } else if pending_change {
                // 主状态已经由版本行统一表达；这里不再重复禁用的安装按钮和
                // “变更已提交”成功句，避免同一事实出现三次。
            } else if let Some(target) = install_target.clone() {
                // 版本已在版本行展示，按钮只保留动作词，避免重复版本并让
                // 主按钮保持紧凑宽度。
                let caption = if current.is_some() {
                    crate::locale::text("更新", "Update")
                } else {
                    crate::locale::text("安装", "Install")
                };
                let install_selection = target;
                action_bar = action_bar.child(
                    div().debug_selector(|| "extension-install".into()).child(
                        Button::new("extension-install")
                            .primary()
                            .label(caption)
                            .disabled(self.busy || self.offline || self.state_error.is_some())
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_confirmation(
                                    Confirmation::Install(install_selection.clone()),
                                    cx,
                                );
                            })),
                    ),
                );
            } else if let Some(installed) = current.as_ref() {
                // 目录有更高版本但资格不满足：说明原因，避免用户误以为按钮丢失；
                // 状态未知时已有独立错误说明，不再重复。
                if available.as_ref().is_some_and(|available| {
                    available.version.as_semver() > installed.active.as_semver()
                }) {
                    let note = if matches!(
                        installed.status,
                        PluginInstallStatus::Unsupported { .. }
                    ) {
                        Some(crate::locale::text(
                            "当前版本不受支持；卸载后重新安装市场版本。",
                            "The current version is unsupported; uninstall it, then install the marketplace version.",
                        ))
                    } else if installed.source != PluginSource::Official
                        && self.state_error.is_none()
                    {
                        Some(crate::locale::text(
                            "该扩展不是从市场安装的；如需市场版本，请先卸载后重新安装。",
                            "This extension was not installed from the marketplace; uninstall it before installing the marketplace version.",
                        ))
                    } else {
                        None
                    };
                    if let Some(note) = note {
                        action_bar = action_bar.child(
                            div()
                                .debug_selector(|| "extension-update-blocked".into())
                                .text_size(theme.text_sm)
                                .text_color(theme.muted)
                                .child(note),
                        );
                    }
                }
            } else if current.is_none() {
                action_bar = action_bar.child(
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
                let toggle_id = plugin.id.clone();
                let enabled = installed.enabled;
                let uninstall_id = plugin.id.clone();
                let delete_id = plugin.id.clone();
                action_bar = action_bar
                    .child(
                        Button::new("extension-toggle")
                            .compact()
                            .outline()
                            .label(if enabled {
                                crate::locale::text("禁用", "Disable")
                            } else {
                                crate::locale::text("启用", "Enable")
                            })
                            .disabled(self.busy || pending_change || self.state_error.is_some())
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.run_operation(
                                    Operation::Change(if enabled {
                                        ExtensionChange::Disable(toggle_id.clone())
                                    } else {
                                        ExtensionChange::Enable(toggle_id.clone())
                                    }),
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("extension-uninstall")
                            .compact()
                            .outline()
                            .label(crate::locale::text("卸载", "Uninstall"))
                            .disabled(self.busy || pending_change || self.state_error.is_some())
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.run_operation(
                                    Operation::Change(ExtensionChange::Uninstall(
                                        uninstall_id.clone(),
                                    )),
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("extension-delete-data")
                            .compact()
                            .ghost()
                            .flex_shrink_0()
                            .text_color(theme.error)
                            .label(crate::locale::text(
                                "删除扩展数据…",
                                "Delete extension data…",
                            ))
                            .disabled(self.busy || self.state_error.is_some())
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_confirmation(
                                    Confirmation::DeleteData(delete_id.clone()),
                                    cx,
                                );
                            })),
                    );
            }
            let mut links = div().flex().flex_wrap().items_center().gap_3();
            if let Some(available) = &available
                && let Some(url) = plugin
                    .versions
                    .iter()
                    .find(|version| version.version == available.version)
                    .and_then(|version| version.changelog_url.clone())
            {
                links = links.child(div().debug_selector(|| "extension-changelog".into()).child(
                    external_link(
                        "extension-changelog",
                        crate::locale::text("更新日志", "Changelog"),
                        url,
                        cx,
                    ),
                ));
            }
            links = links.child(
                div()
                    .debug_selector(|| "extension-repository".into())
                    .child(external_link(
                        "extension-repository",
                        crate::locale::text("仓库", "Repository"),
                        plugin.repository.clone(),
                        cx,
                    )),
            );
            let action_bar = div()
                .debug_selector(|| "extension-detail-actions".into())
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_3()
                .pt_1()
                .child(action_bar)
                .child(links);
            fixed = fixed.child(action_bar);
            if self.offline && !pending_change && available.is_some() {
                fixed = fixed.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "离线时不能安装或更新；刷新目录后重试",
                            "Can’t install or update while offline; refresh the catalog and retry.",
                        )),
                );
            }
            if let Some(feedback) =
                self.feedback_block(&SelectedEntry::Plugin(plugin.id.clone()), true, theme, cx)
            {
                fixed = fixed.child(feedback);
            }

            let full_access = available
                .as_ref()
                .and_then(|selected| {
                    plugin
                        .versions
                        .iter()
                        .find(|version| version.version == selected.version)
                })
                .is_some_and(|version| version.requires_full_access());
            scroll = scroll
                .child(permissions_block(&plugin.permissions, full_access, theme))
                .child(self.readme_section(&plugin, theme, cx));
        } else if let Some((id, installed)) = self.selected.as_ref().and_then(|entry| {
            let SelectedEntry::Plugin(id) = entry else {
                return None;
            };
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
            let inspection = self.drop_ins.iter().find(|inspection| inspection.id == id);
            let is_drop_in = installed.source == PluginSource::DropIn;
            let drop_state = is_drop_in.then(|| drop_in_state(&installed, inspection));
            let policy = stored_actions_policy(&installed, inspection);
            let display_name = inspection
                .filter(|inspection| !inspection.name.is_empty())
                .map_or_else(
                    || id.as_str().to_owned(),
                    |inspection| label(&inspection.name, id.as_str()).to_owned(),
                );
            let subtitle = if is_drop_in {
                crate::locale::text(
                    format!("手动放入 · v{}", installed.active),
                    format!("Manually added · v{}", installed.active),
                )
                .to_string()
            } else {
                crate::locale::text(
                    format!("目录不可用 · v{}", installed.active),
                    format!("Catalog unavailable · v{}", installed.active),
                )
                .to_string()
            };
            // 本地来源同样使用统一身份头：版本状态进副标题/版本行，磁盘指纹与
            // 可用性说明留在正文，避免把多条短事实挤成一列小字。
            let version_line: Option<AnyElement>;
            let mut description_line: Option<AnyElement> = None;
            if is_drop_in {
                let (status, tone) = drop_state.map_or_else(
                    || {
                        (
                            crate::locale::text("正在检查磁盘状态", "Checking disk state"),
                            RowTone::Muted,
                        )
                    },
                    drop_in_status_label,
                );
                let fingerprint = inspection
                    .and_then(|inspection| inspection.fingerprint.as_deref())
                    .map_or_else(
                        || crate::locale::text("未读取", "Not read"),
                        |_| crate::locale::text("可用", "Available"),
                    );
                version_line = Some(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(tone.color(theme))
                        .child(status)
                        .into_any_element(),
                );
                description_line = Some(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            format!("指纹 · {fingerprint}"),
                            format!("Fingerprint · {fingerprint}"),
                        ))
                        .into_any_element(),
                );
            } else {
                version_line = Some(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "本地安装或目录中不可用的扩展仍可管理。",
                            "Extensions installed locally or removed from the catalog remain manageable.",
                        ))
                        .into_any_element(),
                );
            }
            fixed = fixed.child(detail_identity(
                self.entry_icon(&SelectedEntry::Plugin(id.clone())),
                display_name,
                Some(subtitle),
                version_line,
                theme,
            ));
            if let Some(description) = description_line {
                fixed = fixed.child(description);
            }
            if is_drop_in && pending_change {
                fixed = fixed.child(pending_restart_text(theme));
            }
            if let Some(error) = &self.state_error {
                fixed = fixed.child(state_error_note(error, theme));
            }
            if policy != StoredActionsPolicy::None {
                let mut buttons = div().w_full().flex().flex_wrap().items_center().gap_2();
                if policy == StoredActionsPolicy::Manage {
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
                            .disabled(self.busy || pending_change || self.state_error.is_some())
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
                } else if policy == StoredActionsPolicy::ConfirmDropIn
                    && let Some(inspection) = inspection.cloned()
                {
                    buttons = buttons.child(
                        Button::new("extension-confirm-drop-in")
                            .primary()
                            .label(crate::locale::text("确认并启用", "Confirm and enable"))
                            .disabled(
                                self.busy
                                    || pending_change
                                    || self.state_error.is_some()
                                    || drop_in_confirmation_operation(&inspection).is_none(),
                            )
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.open_confirmation(
                                    Confirmation::DropIn(inspection.clone()),
                                    cx,
                                );
                            })),
                    );
                }
                let target = id.clone();
                buttons = buttons.child(
                    Button::new("extension-uninstall-local")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("卸载", "Uninstall"))
                        .disabled(self.busy || pending_change || self.state_error.is_some())
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation(
                                Operation::Change(ExtensionChange::Uninstall(target.clone())),
                                cx,
                            )
                        })),
                );
                let target = id.clone();
                buttons = buttons.child(
                    Button::new("extension-delete-local")
                        .compact()
                        .ghost()
                        .flex_shrink_0()
                        .text_color(theme.error)
                        .label(crate::locale::text(
                            "删除扩展数据…",
                            "Delete extension data…",
                        ))
                        .disabled(self.busy || self.state_error.is_some())
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.open_confirmation(Confirmation::DeleteData(target.clone()), cx);
                        })),
                );
                fixed = fixed.child(buttons);
                if let Some(feedback) =
                    self.feedback_block(&SelectedEntry::Plugin(id.clone()), true, theme, cx)
                {
                    fixed = fixed.child(feedback);
                }
            }
        } else if let Some(id) = self.selected.as_ref().and_then(|entry| {
            let SelectedEntry::Plugin(id) = entry else {
                return None;
            };
            self.state
                .as_ref()
                .is_some_and(|state| {
                    !state.plugins.contains_key(id)
                        && state.pending.iter().any(|change| change.plugin_id() == id)
                })
                .then_some(id.clone())
        }) {
            let failed = self
                .state
                .as_ref()
                .is_some_and(|state| pending_only_failure(state, &id).is_some());
            let status_text = if failed {
                crate::locale::text(
                    "安装失败，可在下方撤销失败的变更。",
                    "The installation failed. Cancel the failed change below.",
                )
            } else {
                crate::locale::text(
                    "安装已提交 · 重启后生效",
                    "Installation submitted · Restart required",
                )
            };
            fixed = fixed
                .child(detail_identity(
                    // 尚未安装完成时没有可验证图标，使用统一占位而不是猜一个身份。
                    ToolIcon::builtin(IconName::Puzzle),
                    id.as_str().to_owned(),
                    Some(
                        crate::locale::text(
                            "待重启 · 尚无安装记录",
                            "Pending restart · no install record yet",
                        )
                        .to_string(),
                    ),
                    Some(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(if failed { theme.error } else { theme.muted })
                            .child(status_text)
                            .into_any_element(),
                    ),
                    theme,
                ))
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "重启应用后，该扩展才会出现在已安装列表。",
                            "The extension appears in the installed list after restarting.",
                        )),
                );
            if let Some(error) = &self.state_error {
                fixed = fixed.child(state_error_note(error, theme));
            }
            if let Some(feedback) =
                self.feedback_block(&SelectedEntry::Plugin(id.clone()), true, theme, cx)
            {
                fixed = fixed.child(feedback);
            }
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
                );
            return detail.into_any_element();
        }
        detail
            .child(fixed)
            .child(extension_scroll_viewport(scroll, &self.detail_scroll))
            .into_any_element()
    }

    /// 对象级反馈只在反馈来源与当前详情完全一致时出现；内置与同 ID 外部记录
    /// 共用稳定身份，因此比较必须带 `SelectedEntry` 来源。没有对象归属的状态
    /// 始终由页头状态弹层承载，切换选择不会让失败原因消失。
    ///
    /// @author kongweiguang
    fn feedback_block(
        &self,
        entry: &SelectedEntry,
        include_plugin_state: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = entry.id();
        let state = self.state.as_ref();
        let failure = include_plugin_state
            .then(|| state.and_then(|state| state.failures.get(id)).cloned())
            .flatten();
        let pending = include_plugin_state
            .then(|| {
                state
                    .and_then(|state| cancelable_failed_change(state, id))
                    .cloned()
            })
            .flatten();
        let revision = state.map_or(0, |state| state.revision);
        let action_status = self
            .feedback_entry
            .as_ref()
            .filter(|feedback| *feedback == entry)
            .and_then(|_| self.status.clone());
        if failure.is_none() && action_status.is_none() && pending.is_none() {
            return None;
        }
        let mut block = div().w_full().flex().flex_col().gap_2();
        if let Some(failure) = failure {
            block = block.child(
                div()
                    .text_size(theme.text_sm)
                    .text_color(theme.error)
                    .child(failure),
            );
        }
        if let Some(status) = action_status {
            let tone = if self.status_is_warning {
                theme.warning
            } else {
                theme.error
            };
            block = block.child(
                div()
                    .text_size(theme.text_sm)
                    .text_color(tone)
                    .child(status),
            );
            // 状态未知时重试会被中央门禁拒绝；恢复入口统一收敛到页头的
            // “刷新状态”，这里不再展示一条必然失败的按钮。
            if self.state_error.is_none()
                && let Some(retry) = self.retry_operation.clone()
            {
                block = block.child(
                    Button::new("extension-retry-operation")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("重试操作", "Retry operation"))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.run_operation_with_feedback(
                                retry.operation.clone(),
                                retry.feedback.clone(),
                                cx,
                            );
                        })),
                );
            }
        }
        if let Some(pending) = pending {
            block = block.child(
                Button::new("extension-cancel-failed")
                    .compact()
                    .ghost()
                    .label(crate::locale::text(
                        "撤销失败的变更",
                        "Cancel failed change",
                    ))
                    .disabled(self.busy || self.state_error.is_some())
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.run_operation(
                            Operation::Change(ExtensionChange::CancelFailedChange {
                                expected_revision: revision,
                                pending: pending.clone(),
                            }),
                            cx,
                        )
                    })),
            );
        }
        Some(block.into_any_element())
    }

    /// README 失败只影响正文，保留基本信息与安装动作；重试重新绑定当前选择。
    fn readme_section(
        &self,
        plugin: &CatalogPlugin,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(error) = &self.readme_error {
            return div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.error)
                        .child(crate::locale::text(
                            format!("说明加载失败：{error}"),
                            format!("Failed to load the description: {error}"),
                        )),
                )
                .child(
                    Button::new("extension-readme-retry")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("重试", "Retry"))
                        .on_click(cx.listener(|page, _, _, cx| {
                            // 显式重试允许覆盖“同项不重复请求”的去重。
                            page.retry_readme(cx);
                        })),
                )
                .into_any_element();
        }
        match &self.readme {
            Some(readme) => div()
                .w_full()
                .flex()
                .flex_col()
                .gap_4()
                .child(div().w_full().h(px(1.)).bg(theme.border))
                .child(super::markdown::render_readme(
                    format!("extension-readme-{}", plugin.id.as_str()),
                    readme.clone(),
                ))
                .into_any_element(),
            // 加载中不插入占位块，避免 README 到达时把详情整体挤动。
            None => div().into_any_element(),
        }
    }

    /// 使用窗口级 Kit Dialog 呈现确认：焦点、Escape 和关闭后的焦点恢复由 Kit 统一负责。
    /// 正文使用 Kit 的滚动视口，使长确认清单的轨道与底部操作区各自固定。
    pub(super) fn present_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active {
            self.confirmation = None;
            self.dialog_open = false;
            self.dialog_generation = self.dialog_generation.wrapping_add(1);
            return;
        }
        let Some(confirmation) = self.confirmation.clone() else {
            self.dialog_open = false;
            return;
        };
        let Some(model) = self.confirmation_dialog(&confirmation) else {
            self.clear_confirmation(cx);
            return;
        };
        let owner = cx.entity().downgrade();
        let ok_owner = owner.clone();
        let cancel_owner = owner.clone();
        let dismiss_owner = owner.clone();
        let confirm_owner = owner.clone();
        let ok_operation = model.action.clone();
        let confirm_operation = model.action.clone();
        let danger = model.danger;
        window.open_dialog(cx, move |dialog, window, app| {
            // 弹层可能跨主题切换存活；每次重建都按当次窗口与 App 取语义色，
            // 避免跟随系统切换后仍沿用打开瞬间的旧 panel 色。
            let theme = ThemeTokens::current(window, app).for_settings();
            let ok_owner = ok_owner.clone();
            let cancel_owner = cancel_owner.clone();
            let dismiss_owner = dismiss_owner.clone();
            let confirm_owner = confirm_owner.clone();
            let ok_operation = ok_operation.clone();
            let confirm_operation = confirm_operation.clone();
            dialog
                .centered()
                // 批量确认需要逐项展示范围，稍宽一点减少换行；单项确认保持紧凑。
                .width(px(if model.items.is_empty() { 440. } else { 520. }))
                // Kit Dialog 默认用 Kit tokens.background，深色下比应用面板更暗；
                // 本页显式使用应用 panel 语义色，与卡片和 popover 保持同一层级，
                // 不引入 raw hex，也不修改共享 Kit 主题。
                .bg(theme.panel)
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
                        .id("extensions-confirmation-body")
                        .max_h(window.viewport_size().height * 0.55)
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
                        .when(!model.items.is_empty(), |body| {
                            // 批量范围逐项列出，用户不必靠总数猜会更新哪些插件；
                            // 完全访问在对应项后标出，避免被聚合权限描述掩盖。
                            let mut list = body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(
                                        "将更新以下扩展",
                                        "The following extensions will be updated",
                                    )),
                            );
                            for item in &model.items {
                                list = list.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .py_1()
                                        .child(
                                            div()
                                                .text_size(theme.text_sm)
                                                .text_color(theme.foreground)
                                                .child(item.title.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(theme.text_xs)
                                                .text_color(theme.muted)
                                                .child(item.subtitle.clone()),
                                        )
                                        .when(item.full_access, |item| {
                                            item.child(
                                                div()
                                                    .text_size(theme.text_xs)
                                                    .text_color(theme.warning)
                                                    .child(crate::locale::text(
                                                        "完全访问：以当前用户权限运行",
                                                        "Full access: runs with the current user's permissions",
                                                    )),
                                            )
                                        }),
                                );
                            }
                            list
                        })
                        .when(!model.danger && model.items.is_empty(), |body| {
                            body.child(permissions_block(
                                &model.permissions,
                                model.full_access,
                                theme,
                            ))
                        })
                        .overflow_y_scrollbar(),
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
                    items: Vec::new(),
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
                    items: Vec::new(),
                    action: Operation::File(path.clone(), inspection.clone()),
                    action_label: crate::locale::text("安装", "Install"),
                    danger: false,
                })
            }
            Confirmation::BatchUpdate(plan) => {
                let count = plan.items.len();
                let items = plan
                    .items
                    .iter()
                    .map(|item| ConfirmationItem {
                        title: SharedString::from(item.name.clone()),
                        subtitle: SharedString::from(format!(
                            "{} · v{} → v{}",
                            item.id.as_str(),
                            item.from,
                            item.selection.version
                        )),
                        full_access: item.full_access,
                    })
                    .collect();
                Some(ConfirmationDialogModel {
                    title: crate::locale::text(
                        format!("更新 {count} 个扩展？"),
                        format!("Update {count} extensions?"),
                    ),
                    subtitle: crate::locale::text(
                        "将按当前已验签目录逐项安装更高版本；每项完成后需重启生效。",
                        "Higher versions from the current verified catalog will be installed one by one; restart is required afterwards.",
                    ),
                    // 完全信任必须说清楚：清单权限只是声明，不是沙箱；范围只含
                    // 已安装且目录中版本更高的记录，不因搜索或折叠扩大/缩小。
                    warning: Some(crate::locale::text(
                        "扩展以当前用户权限运行，清单中的权限声明不是沙箱。仅更新已安装且目录中版本更高的扩展。",
                        "Extensions run with the current user's permissions; manifest permissions are not a sandbox. Only installed extensions with a higher catalog version are updated.",
                    )),
                    source: Some(crate::locale::text(
                        "当前市场目录",
                        "Current marketplace catalog",
                    )),
                    full_access: false,
                    permissions: Vec::new(),
                    items,
                    action: Operation::BatchUpdate(plan.clone()),
                    action_label: crate::locale::text("更新全部", "Update all"),
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
                items: Vec::new(),
                action: Operation::Delete(id.clone()),
                action_label: crate::locale::text("删除数据", "Delete data"),
                danger: true,
            }),
            Confirmation::DropIn(inspection) => {
                let action = drop_in_confirmation_operation(inspection)?;
                let name = label(&inspection.name, inspection.id.as_str()).to_owned();
                Some(ConfirmationDialogModel {
                    title: crate::locale::text(
                        format!("启用手动放入的扩展「{name}」？"),
                        format!("Enable manually added extension “{name}”?"),
                    ),
                    subtitle: SharedString::from(format!(
                        "{} · v{}",
                        inspection.id.as_str(),
                        inspection.version
                    )),
                    warning: Some(crate::locale::text(
                        "此扩展由用户手动放入，未经市场审核。",
                        "This extension was added manually and has not been reviewed by the marketplace.",
                    )),
                    source: Some(crate::locale::text("手动放入", "Added manually")),
                    full_access: inspection
                        .permissions
                        .iter()
                        .any(|permission| permission == "full-access"),
                    permissions: inspection.permissions.clone(),
                    items: Vec::new(),
                    action,
                    action_label: crate::locale::text("确认并启用", "Confirm and enable"),
                    danger: false,
                })
            }
        }
    }

    /// 统一列表的条目序列：已安装外部记录置顶，内置组独立可折叠，待重启只列
    /// 没有安装记录的身份，最后是未安装目录项。组标题与行同源于这里，渲染和
    /// 键盘导航不再各算一套索引。
    ///
    /// 去重按稳定身份：已安装记录不再重复出现在目录组；pending-only 与内置身份
    /// 也不会同时出现在目录组。分类只收窄目录组，搜索作用于所有组。
    ///
    /// @author kongweiguang
    pub(super) fn list_entries(&self, plugins: &[CatalogPlugin], query: &str) -> Vec<ListEntry> {
        let state = self.state.as_ref();
        let mut entries = Vec::new();
        let mut installed = Vec::new();
        let mut pending = Vec::new();
        let mut market = Vec::new();
        if let Some(state) = state {
            for (id, record) in &state.plugins {
                if let Some(row) = self.installed_row(id, record, state, query) {
                    installed.push(row);
                }
            }
            // 本地包首次安装尚未重启时没有 installed 条目，仍需展示排队事实；
            // 这类身份不计入已安装组，只在待重启组出现。
            for change in &state.pending {
                let id = change.plugin_id();
                if state.plugins.contains_key(id)
                    || self.builtin_units.iter().any(|unit| &unit.id == id)
                    || !query_matches(query, &[id.as_str()])
                {
                    continue;
                }
                pending.push(pending_only_row(
                    state,
                    id,
                    self.row_selected(&SelectedEntry::Plugin(id.clone())),
                ));
            }
        }
        push_group(&mut entries, RowGroup::External, installed);
        // 内置组默认收起；搜索命中时临时展开，不改变用户的展开选择。收起时
        // 仍保留组标题，键盘滚动序号与渲染条目保持一致。
        let builtin_rows = builtin_rows(&self.builtin_units, state, query);
        if !builtin_rows.is_empty() {
            entries.push(ListEntry::Header {
                group: RowGroup::Builtin,
                count: builtin_rows.len(),
            });
            if self.builtin_expanded || !query.trim().is_empty() {
                for row in builtin_rows {
                    let selected = self.row_selected(&SelectedEntry::Builtin(row.id.clone()));
                    entries.push(ListEntry::Row(Box::new(builtin_row(row, selected))));
                }
            }
        }
        push_group(&mut entries, RowGroup::Pending, pending);
        for plugin in plugins {
            if let Some(row) = self.market_row(plugin, state, query) {
                market.push(row);
            }
        }
        push_group(&mut entries, RowGroup::Market, market);
        entries
    }

    /// 已安装记录行：优先使用可信目录元数据补充名称与描述，更新动作只在
    /// `update_selection` 判定可更新时出现，与批量范围共用同一条件。
    ///
    /// @author kongweiguang
    fn installed_row(
        &self,
        id: &PluginId,
        record: &kerminal_domain::plugins::InstalledPlugin,
        state: &kerminal_domain::plugins::PluginStoreState,
        query: &str,
    ) -> Option<ExtensionRow> {
        let pending_change = state.pending.iter().any(|change| change.plugin_id() == id);
        let inspection = self.drop_ins.iter().find(|inspection| &inspection.id == id);
        // 已安装行优先使用可信目录元数据：市场插件即使没有 drop-in 观察，
        // 也不应退化成纯 ID 或错误显示“目录不可用”。
        let catalog_plugin = self.catalog.as_ref().and_then(|snapshot| {
            snapshot
                .catalog
                .plugins
                .iter()
                .find(|plugin| &plugin.id == id)
        });
        let drop_state = (record.source == PluginSource::DropIn || is_reserved_identity(record))
            .then(|| drop_in_state(record, inspection));
        let name = inspection
            .filter(|inspection| !inspection.name.is_empty())
            .map(|inspection| label(&inspection.name, id.as_str()).to_owned())
            .or_else(|| catalog_plugin.map(|plugin| label(&plugin.name, id.as_str()).to_owned()))
            .unwrap_or_else(|| id.as_str().to_owned());
        let description = installed_description(
            drop_state,
            record,
            catalog_plugin.map(|plugin| label(&plugin.description, "")),
        );
        if !query_matches(query, &[name.as_str(), id.as_str(), description.as_str()]) {
            return None;
        }
        let installing = self.active_install.as_ref() == Some(id);
        let broken = state.failures.contains_key(id);
        let (status, tone) = installed_row_status(drop_state, pending_change, broken, record);
        let mut row = ExtensionRow {
            element_key: id.as_str().to_owned(),
            selection: SelectedEntry::Plugin(id.clone()),
            icon: self.entry_icon(&SelectedEntry::Plugin(id.clone())),
            name,
            description,
            progress: installing.then(|| self.row_progress(id)),
            status: Some((status, tone)),
            action: None,
            installing,
            cancel_enabled: self.cancel.is_some(),
            selected: self.row_selected(&SelectedEntry::Plugin(id.clone())),
        };
        if let Some(available) = self.update_selection(id) {
            row.action = Some(ExtensionRowAction {
                label: crate::locale::text("更新", "Update"),
                selection: available,
                disabled: self.busy || self.offline || self.state_error.is_some(),
            });
        }
        Some(row)
    }

    /// 未安装目录行：已安装、pending-only 与内置身份都不重复出现，保证一张
    /// 列表里同一身份只有一个入口。分类筛选只作用于这一组。
    ///
    /// @author kongweiguang
    fn market_row(
        &self,
        plugin: &CatalogPlugin,
        state: Option<&kerminal_domain::plugins::PluginStoreState>,
        query: &str,
    ) -> Option<ExtensionRow> {
        let id = &plugin.id;
        if state.is_some_and(|state| state.plugins.contains_key(id)) {
            return None;
        }
        if state.is_some_and(|state| state.pending.iter().any(|change| change.plugin_id() == id)) {
            return None;
        }
        if self.builtin_units.iter().any(|unit| &unit.id == id) {
            return None;
        }
        // 未确认投放的目录行会绕开确认门禁；列表只保留带确认入口的记录行。
        if unconfirmed_drop_in(state, id).is_some() {
            return None;
        }
        let name = label(&plugin.name, id.as_str());
        let description = label(&plugin.description, "");
        if !query_matches(
            query,
            &[name, id.as_str(), plugin.publisher.as_str(), description],
        ) {
            return None;
        }
        if let Some(category) = &self.filter.category
            && !plugin.categories.contains(category)
        {
            return None;
        }
        let available = selection(
            plugin,
            &self.host_api,
            self.catalog.as_ref().map(|snapshot| &snapshot.identity),
        );
        let broken = state.is_some_and(|state| state.failures.contains_key(id));
        let installing = self.active_install.as_ref() == Some(id);
        let mut row = ExtensionRow {
            element_key: id.as_str().to_owned(),
            selection: SelectedEntry::Plugin(id.clone()),
            icon: self.entry_icon(&SelectedEntry::Plugin(id.clone())),
            name: name.to_owned(),
            description: description.to_owned(),
            progress: installing.then(|| self.row_progress(id)),
            status: None,
            action: None,
            installing,
            cancel_enabled: self.cancel.is_some(),
            selected: self.row_selected(&SelectedEntry::Plugin(id.clone())),
        };
        if installing {
            // 行内进度已覆盖第二行，不再叠加状态文字。
        } else if broken {
            row.status = Some((crate::locale::text("损坏", "Broken"), RowTone::Error));
        } else if let Some(available) = available {
            row.action = Some(ExtensionRowAction {
                label: crate::locale::text("安装", "Install"),
                selection: available,
                disabled: self.busy || self.offline || self.state_error.is_some(),
            });
        } else {
            row.status = Some((
                crate::locale::text("不支持当前平台", "Not supported on this platform"),
                RowTone::Muted,
            ));
        }
        Some(row)
    }

    /// 进度文案只属于当前安装行；批量执行时前置“序号/总数”，单项安装保持原样。
    fn row_progress(&self, id: &PluginId) -> String {
        let label = self
            .progress
            .as_ref()
            .map(progress_label)
            .unwrap_or_else(|| crate::locale::text("准备中", "Preparing").to_string());
        match self.batch.as_ref().filter(|batch| &batch.id == id) {
            Some(batch) => format!("{}/{} · {label}", batch.index + 1, batch.total),
            None => label,
        }
    }
}

impl ExtensionsPage {
    /// 把条目身份投影成行首与详情头共用的图标。
    ///
    /// 内置单元直接查静态身份表，不发任何 I/O；外部条目只读页面已缓存的
    /// 应用层结果。未加载、没有图标或任何读取失败都落到同一个 Puzzle 占位，
    /// 因此图标槽尺寸与行高在加载前后完全一致。
    ///
    /// @author kongweiguang
    fn entry_icon(&self, selection: &SelectedEntry) -> ToolIcon {
        match selection {
            SelectedEntry::Builtin(id) => ToolIcon::builtin(builtin_unit_icon(id.as_str())),
            SelectedEntry::Plugin(id) => {
                match self.installed_icons.get(id).and_then(Option::as_ref) {
                    // 清单声明的内置名必须能在当前 Kit 图标表中解析；未知名称
                    // 仍回退 Puzzle，不能把插件字符串交给任意资源加载器。
                    Some(ExtensionIcon::Builtin(name)) => {
                        crate::plugins::icon_names::builtin_icon_name(name)
                            .map(ToolIcon::builtin)
                            .unwrap_or_else(|| ToolIcon::builtin(IconName::Puzzle))
                    }
                    Some(ExtensionIcon::Svg(bytes)) => ToolIcon::Svg(bytes.clone()),
                    // 包内没有图标时允许退回市场条目图标；两者都没有才是占位。
                    None => self
                        .catalog_icons
                        .get(id)
                        .and_then(Option::as_ref)
                        .map(|bytes| ToolIcon::Svg(bytes.clone()))
                        .unwrap_or_else(|| ToolIcon::builtin(IconName::Puzzle)),
                }
            }
        }
    }
}

/// 内置单元身份到 Kit 图标的唯一映射。
///
/// 身份是稳定 ID，显示名会随语言变化，所以不能按名称匹配；未列出的身份
/// 统一回退 Blocks 而不是随便借用相邻插件图标，避免制造错误的能力暗示。
fn builtin_unit_icon(id: &str) -> IconName {
    match id {
        "kerminal.core" => IconName::SquareTerminal,
        "kerminal.ssh" => IconName::Server,
        "kerminal.sftp" => IconName::FolderOpen,
        "kerminal.telnet" => IconName::Globe,
        "kerminal.serial" => IconName::Cable,
        "kerminal.rdp" => IconName::Monitor,
        "kerminal.agents" => IconName::Bot,
        "kerminal.commands" => IconName::FileText,
        "kerminal.tmux" => IconName::PanelsTopLeft,
        "kerminal.containers" => IconName::Container,
        "kerminal.ports" => IconName::Network,
        "kerminal.system" => IconName::Cpu,
        "kerminal.logs" => IconName::RotateCcwClock,
        _ => IconName::Blocks,
    }
}

/// 行数据只描述当前条目，按钮动作在渲染时绑定，保证内容、筛选与安装状态共用一个来源。
#[derive(Clone)]
pub(super) struct ExtensionRow {
    /// 元素键与插件身份分离：内置行与同名冲突记录必须能同时出现在列表里。
    pub(super) element_key: String,
    /// 选择来源与高亮共用：同 id 的内置行和记录行只能选中各自那一行。
    pub(super) selection: SelectedEntry,
    /// 行首固定图标槽的内容；未加载或读取失败时由投影统一回退 Puzzle。
    pub(super) icon: ToolIcon,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) progress: Option<String>,
    pub(super) status: Option<(SharedString, RowTone)>,
    pub(super) action: Option<ExtensionRowAction>,
    pub(super) installing: bool,
    pub(super) cancel_enabled: bool,
    pub(super) selected: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowGroup {
    Market,
    External,
    Pending,
    Builtin,
}

impl RowGroup {
    /// 原生验收与快照使用稳定字符串，不把本地化组标题当选择器。
    pub(super) fn acceptance_kind(self) -> &'static str {
        match self {
            RowGroup::External => "installed",
            RowGroup::Builtin => "builtin",
            RowGroup::Pending => "pending",
            RowGroup::Market => "market",
        }
    }
}

/// 列表条目把组标题和行合成同一序列：渲染、键盘移动与滚动定位消费同一份
/// 索引，折叠内置组时标题仍在序列里，不会出现两套序号错位。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) enum ListEntry {
    Header {
        group: RowGroup,
        count: usize,
    },
    /// 行结构远大于标题，装箱让条目向量按指针移动；渲染与键盘导航仍按值
    /// 克隆行，行为不变，避免为每个条目承担整块结构体宽度。
    Row(Box<ExtensionRow>),
}

/// 非空组才写标题，空组完全消失；组顺序由调用方决定。
fn push_group(entries: &mut Vec<ListEntry>, group: RowGroup, rows: Vec<ExtensionRow>) {
    if rows.is_empty() {
        return;
    }
    entries.push(ListEntry::Header {
        group,
        count: rows.len(),
    });
    entries.extend(rows.into_iter().map(|row| ListEntry::Row(Box::new(row))));
}

/// 待重启首次安装没有 installed 记录，单独成行并保留失败可撤销提示。
fn pending_only_row(state: &PluginStoreState, id: &PluginId, selected: bool) -> ExtensionRow {
    let failed = pending_only_failure(state, id).is_some();
    ExtensionRow {
        element_key: id.as_str().to_owned(),
        selection: SelectedEntry::Plugin(id.clone()),
        // 尚未安装成功的身份没有可验证图标，统一显示扩展占位而不是猜一个来源。
        icon: ToolIcon::builtin(IconName::Puzzle),
        name: id.as_str().to_owned(),
        description: crate::locale::text(
            if failed {
                "安装失败，可在详情中撤销失败的变更"
            } else {
                "安装已提交 · 重启后生效"
            },
            if failed {
                "Installation failed; cancel the failed change in the details"
            } else {
                "Installation submitted; restart required"
            },
        )
        .to_string(),
        progress: None,
        status: Some((
            if failed {
                crate::locale::text("损坏", "Broken")
            } else {
                crate::locale::text("重启生效", "Restart required")
            },
            if failed {
                RowTone::Error
            } else {
                RowTone::Warning
            },
        )),
        action: None,
        installing: false,
        cancel_enabled: false,
        selected,
    }
}

/// 内置单元行与外部记录共用行结构，但身份保持 `SelectedEntry::Builtin`，
/// 同名冲突时仍能区分并各自选中。
fn builtin_row(row: BuiltinRow, selected: bool) -> ExtensionRow {
    let icon = ToolIcon::builtin(builtin_unit_icon(row.id.as_str()));
    let (status, tone) = if row.pending {
        (
            crate::locale::text("重启生效", "Restart required"),
            RowTone::Warning,
        )
    } else if row.enabled {
        (crate::locale::text("已启用", "Enabled"), RowTone::Muted)
    } else {
        (crate::locale::text("已禁用", "Disabled"), RowTone::Muted)
    };
    ExtensionRow {
        element_key: format!("builtin-{}", row.id.as_str()),
        selection: SelectedEntry::Builtin(row.id.clone()),
        icon,
        name: row.name,
        description: crate::locale::text(
            if row.can_disable {
                "内置插件"
            } else {
                "内置插件 · 核心"
            },
            if row.can_disable {
                "Built-in extension"
            } else {
                "Built-in extension · Core"
            },
        )
        .to_string(),
        progress: None,
        status: Some((status, tone)),
        action: None,
        installing: false,
        cancel_enabled: false,
        selected,
    }
}

/// 安装动作绑定已选中的受信目录版本，界面不能在渲染后拼出下载地址。
#[derive(Clone)]
pub(super) struct ExtensionRowAction {
    pub(super) label: SharedString,
    pub(super) selection: CatalogSelection,
    pub(super) disabled: bool,
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
    /// 批量确认逐项列出“名称 · 身份 · 旧版本→目标版本”，不只用总数概括范围。
    items: Vec<ConfirmationItem>,
    action: Operation,
    action_label: SharedString,
    danger: bool,
}

/// 批量确认中的单项摘要；完全访问逐项标出，避免用聚合权限掩盖真实范围。
#[derive(Clone)]
struct ConfirmationItem {
    title: SharedString,
    subtitle: SharedString,
    full_access: bool,
}

/// 行状态只使用语义角色，不用颜色单独承载含义。
#[derive(Clone, Copy)]
pub(super) enum RowTone {
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

/// 组标题统一贴同一条左侧脊线：外层只用 `px_2`，与行容器的内边距一致，
/// 因此四类标题文字和行首图标槽从同一 x 开始。非折叠组不再插入假箭头空槽，
/// 否则 `已安装` 会被凭空右移；只有内置组在标题行右端保留披露箭头。
///
/// @author kongweiguang
fn group_header(
    group: RowGroup,
    count: usize,
    expanded: bool,
    first: bool,
    theme: ThemeTokens,
    cx: &mut Context<ExtensionsPage>,
) -> AnyElement {
    let label = match group {
        RowGroup::Market => {
            crate::locale::text(format!("可安装 {count}"), format!("Available {count}"))
        }
        RowGroup::External => {
            crate::locale::text(format!("已安装 {count}"), format!("Installed {count}"))
        }
        RowGroup::Pending => crate::locale::text(
            format!("待重启 {count}"),
            format!("Pending restart {count}"),
        ),
        RowGroup::Builtin => {
            crate::locale::text(format!("内置 {count}"), format!("Built-in {count}"))
        }
    };
    let kind = group.acceptance_kind();
    let arrow_selector = format!("extensions-group-arrow-{kind}");
    let title = div()
        .min_w(px(0.))
        .truncate()
        .text_size(theme.text_xs)
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.muted)
        .debug_selector({
            let selector = format!("extensions-group-title-{kind}");
            move || selector.clone()
        })
        .child(label.clone())
        .into_any_element();
    // 分组呼吸间距只由“是否首组”决定，不由组类型决定：任何组被过滤掉后，
    // 下一组都会自然接替首组位置，不会留下顶部空洞。
    let container = div()
        .w_full()
        .px_2()
        .when(first, |header| header.pt_1().pb_1())
        .when(!first, |header| header.pt_3().pb_1());
    if group == RowGroup::Builtin {
        // 组标题必须是真实 Button：只有可聚焦控件才保留 Tab 停靠与 Enter/Space
        // 激活。按钮铺满整行，点击标题任意位置都能折叠；文字在左、箭头在右，
        // 文字左缘与其它三组严格同一条脊线，箭头不再占据标题前面的位置。
        let arrow = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(theme.metrics.icon_size)
            .debug_selector(move || arrow_selector.clone())
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(theme.metrics.icon_size)
                .text_color(theme.muted),
            );
        return container
            .flex()
            .items_center()
            .child(
                Button::new("extensions-group-builtin")
                    .ghost()
                    // XSmall 让按钮内容沿用标题的 text_xs；自定义 Pixels 尺寸会把
                    // 内容字号提升到 text_base，破坏组标题与行内容的视觉层级。
                    .xsmall()
                    .p_0()
                    .w_full()
                    .text_color(theme.muted)
                    // 选择器只挂在真实可点控件上，避免外层容器与按钮重复登记
                    // 同名 selector 导致验收取到不可点击的更大区域。
                    .debug_selector(|| "extensions-group-builtin".into())
                    .accessibility_label(crate::locale::text(
                        format!("{label}，展开或收起内置扩展"),
                        format!("{label}, expand or collapse built-in extensions"),
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(title)
                            .child(arrow),
                    )
                    .on_click(cx.listener(|page, _, _, cx| {
                        page.builtin_expanded = !page.builtin_expanded;
                        cx.notify();
                    })),
            )
            .into_any_element();
    }
    container
        .flex()
        .items_center()
        .child(title)
        .into_any_element()
}

/// 行本身是普通容器，点击时把焦点交还列表；名称不再使用会抢焦点的 Button。
fn render_extension_row(
    row: ExtensionRow,
    list_focus: &gpui_kit::FocusHandle,
    batch_active: bool,
    theme: ThemeTokens,
    cx: &mut Context<ExtensionsPage>,
) -> AnyElement {
    let ExtensionRow {
        element_key,
        selection,
        icon,
        name,
        description,
        progress,
        status,
        action,
        installing,
        cancel_enabled,
        selected,
    } = row;
    let id_text = element_key;
    let select_entry = selection.clone();
    let focus = list_focus.clone();
    let action_name = name.clone();
    let action_button = if installing {
        // 批量执行共用页面级取消令牌；行内文案必须说明取消的是整批，
        // 不能让用户以为只跳过当前项。
        let cancel_label = if batch_active {
            crate::locale::text("取消批量更新", "Cancel batch update")
        } else {
            crate::locale::text("取消", "Cancel")
        };
        let cancel_accessibility = if batch_active {
            crate::locale::text(
                format!("取消批量更新 {action_name}"),
                format!("Cancel batch update {action_name}"),
            )
        } else {
            crate::locale::text(
                format!("取消安装 {action_name}"),
                format!("Cancel install {action_name}"),
            )
        };
        Some(
            Button::new(format!("extension-action-{id_text}"))
                .compact()
                .outline()
                .label(cancel_label)
                .accessibility_label(cancel_accessibility)
                .disabled(!cancel_enabled)
                .on_click(cx.listener(|page, _, _, cx| {
                    // 行内取消是独立动作，不能冒泡成整行选择；否则窄窗点取消会
                    // 顺带打开详情，把用户正要取消的安装行遮住。
                    cx.stop_propagation();
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
                .outline()
                .label(action.label)
                .accessibility_label(crate::locale::text(
                    format!("{action_label_accessibility} {action_name}"),
                    format!("{action_label_accessibility} {action_name}"),
                ))
                .disabled(action.disabled)
                .on_click(cx.listener(move |page, _, _, cx| {
                    // 安装/更新按钮的点击不能同时触发整行选择：窄窗里那会立刻
                    // 切到详情并卸载列表，用户看不到刚打开的确认来源行。
                    cx.stop_propagation();
                    page.open_confirmation(Confirmation::Install(selection.clone()), cx);
                }))
                .into_any_element(),
        )
    } else {
        status.map(|(text, tone)| {
            div()
                .id(format!("extension-action-{id_text}"))
                .flex_shrink_0()
                .text_size(theme.text_sm)
                .text_color(tone.color(theme))
                .child(text)
                .into_any_element()
        })
    };
    let second_line = progress.unwrap_or(description);

    div()
        .id(format!("extension-row-{id_text}"))
        // 行是普通容器而不是 Button，原生验收按 debug selector 定位它；
        // 只在 debug + test-support 下记录，不影响发布构建。
        .debug_selector({
            let selector = format!("extension-row-{id_text}");
            move || selector
        })
        .w_full()
        .min_h(px(56.))
        .flex_shrink_0()
        .px_2()
        .py_2()
        .rounded(theme.control_radius)
        .flex()
        .items_center()
        .gap_3()
        .when(selected, |row| row.bg(theme.surface_selected()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(theme.surface_hover()))
        })
        .on_click(cx.listener(move |page, _, window, cx| {
            page.select(select_entry.clone(), cx);
            // 窄窗打开详情会卸载列表，焦点必须落在实际挂载的详情上；宽窗列表
            // 仍然可见，继续保留列表焦点以便方向键连续移动。
            if page.is_narrow(window) {
                window.focus(&page.detail_focus, cx);
            } else {
                window.focus(&focus, cx);
            }
        }))
        // 固定 32px 图标槽：无论图标是否已加载，槽宽都不变，因此整列文字左缘
        // 始终对齐；图标本身只占 16px，留出柔和底块作为视觉锚点。
        .child(
            div()
                .size(px(32.))
                .flex_shrink_0()
                .rounded(theme.control_radius)
                .bg(theme.muted_surface())
                .border_1()
                .border_color(theme.border)
                .flex()
                .items_center()
                .justify_center()
                .debug_selector({
                    let selector = format!("extension-row-icon-{id_text}");
                    move || selector
                })
                .child(icon.component(theme.muted).size(px(16.))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_0p5()
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
                                .truncate()
                                .text_size(theme.text_sm)
                                .font_weight(theme.settings_medium_font_weight())
                                .text_color(theme.foreground)
                                // 固定图标槽之后的文字起点；记录解析后的 bounds，
                                // 供对齐测试核对整列文字严格同缘。
                                .debug_selector({
                                    let selector = format!("extension-row-content-{id_text}");
                                    move || selector
                                })
                                .child(name),
                        )
                        .when_some(action_button, |line, action| {
                            // 行内动作与纯状态共用右侧列，名称宽度变化时仍保持右对齐；
                            // outline 让动作按钮与只读状态文字有明确边界。
                            line.child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .child(action),
                            )
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(second_line),
                ),
        )
        .into_any_element()
}

/// 搜索同时匹配名称、身份、作者与描述；界面看到什么就能搜到什么。
fn query_matches(query: &str, values: &[&str]) -> bool {
    query.is_empty()
        || values
            .iter()
            .any(|value| value.to_lowercase().contains(query))
}

/// 已安装记录的行状态；磁盘冲突、缺失与损坏优先于陈旧的 pending 标记。
fn installed_row_status(
    drop_state: Option<DropInState>,
    pending_change: bool,
    broken: bool,
    installed: &kerminal_domain::plugins::InstalledPlugin,
) -> (SharedString, RowTone) {
    match drop_state {
        Some(
            state @ (DropInState::ReservedIdentity
            | DropInState::Missing
            | DropInState::Corrupt
            | DropInState::Unsupported),
        ) => drop_in_status_label(state),
        Some(DropInState::PendingConfirmation) => {
            if pending_change {
                (
                    crate::locale::text("重启生效", "Restart required"),
                    RowTone::Warning,
                )
            } else {
                drop_in_status_label(DropInState::PendingConfirmation)
            }
        }
        Some(DropInState::Ready) if pending_change => (
            crate::locale::text("重启生效", "Restart required"),
            RowTone::Warning,
        ),
        Some(DropInState::Ready) => (
            if installed.enabled {
                crate::locale::text("已启用", "Enabled")
            } else {
                crate::locale::text("已禁用", "Disabled")
            },
            RowTone::Muted,
        ),
        None if broken => (crate::locale::text("损坏", "Broken"), RowTone::Error),
        None if pending_change => (
            crate::locale::text("重启生效", "Restart required"),
            RowTone::Warning,
        ),
        None => (
            if installed.enabled {
                crate::locale::text("已启用", "Enabled")
            } else {
                crate::locale::text("已禁用", "Disabled")
            },
            RowTone::Muted,
        ),
    }
}

/// 已安装记录的第二行说明来源与磁盘事实，避免只显示一个 ID。
///
/// 目录仍包含该身份时优先使用目录描述；只有真正不在目录中的本地安装才提示
/// “目录不可用”，不能把市场插件误标成来源不明。
pub(super) fn installed_description(
    drop_state: Option<DropInState>,
    installed: &kerminal_domain::plugins::InstalledPlugin,
    catalog_description: Option<&str>,
) -> String {
    match drop_state {
        Some(DropInState::ReservedIdentity) => crate::locale::text(
            "占用内置单元身份，不可用",
            "Conflicts with a built-in identity; unavailable",
        )
        .to_string(),
        Some(DropInState::PendingConfirmation) => crate::locale::text(
            "手动放入，等待确认",
            "Manually added; awaiting confirmation",
        )
        .to_string(),
        Some(_) if installed.source == PluginSource::DropIn => {
            crate::locale::text("手动放入的扩展", "Manually added extension").to_string()
        }
        _ => catalog_description
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                crate::locale::text(
                    "本地安装或目录中不可用",
                    "Installed locally or unavailable in the catalog",
                )
                .to_string()
            }),
    }
}

/// 详情身份头：固定 48px 图标容器、标题、副标题与版本状态行。
///
/// 内置、市场、本地/drop-in、待重启与冲突记录共用同一结构，避免不同来源的
/// 详情在图标尺寸、字号和留白上漂移。图标无论是否已加载都占据同一容器，
/// 因此图标迟到只替换内部图形，不改变头部高度或正文起点。
///
/// @author kongweiguang
fn detail_identity(
    icon: ToolIcon,
    title: String,
    subtitle: Option<String>,
    version: Option<AnyElement>,
    theme: ThemeTokens,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_4()
        .child(
            div()
                .size(px(48.))
                .flex_shrink_0()
                // 固定身份槽尺寸供原生验收与回归确认图标迟到不会改变头部高度。
                .debug_selector(|| "extension-detail-icon".into())
                .rounded(theme.radius_panel)
                .bg(theme.muted_surface())
                .border_1()
                .border_color(theme.border)
                .flex()
                .items_center()
                .justify_center()
                .child(icon.component(theme.muted).size(px(24.))),
        )
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
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(title),
                )
                .when_some(
                    subtitle.filter(|subtitle| !subtitle.is_empty()),
                    |column, subtitle| {
                        column.child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(subtitle),
                        )
                    },
                )
                .when_some(version, |column, version| column.child(version)),
        )
        .into_any_element()
}

/// 权威状态未知时所有详情共用的停写说明；恢复路径统一指向页头的刷新状态。
fn state_error_note(error: &str, theme: ThemeTokens) -> AnyElement {
    div()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(crate::locale::text(
            format!("状态未确认，写入已禁用：{error}。请刷新状态后重试。"),
            format!(
                "State is unconfirmed; writes are disabled: {error}. Refresh the state and retry."
            ),
        ))
        .into_any_element()
}

/// pending 变更统一提示重启，不把排队状态伪装成已生效。
fn pending_restart_text(theme: ThemeTokens) -> AnyElement {
    div()
        .text_size(theme.text_xs)
        .text_color(theme.warning)
        .child(crate::locale::text(
            "变更已提交，重启后生效。",
            "The change is committed and takes effect after restart.",
        ))
        .into_any_element()
}

/// drop-in 的磁盘证据与冲突身份在列表和详情使用同一组文案与色调，避免两处判断漂移。
fn drop_in_status_label(state: DropInState) -> (SharedString, RowTone) {
    match state {
        DropInState::PendingConfirmation => (
            crate::locale::text("待确认", "Needs confirmation"),
            RowTone::Warning,
        ),
        DropInState::Ready => (crate::locale::text("就绪", "Ready"), RowTone::Muted),
        DropInState::Missing => (
            crate::locale::text("文件缺失", "Files missing"),
            RowTone::Error,
        ),
        DropInState::Corrupt => (crate::locale::text("损坏", "Broken"), RowTone::Error),
        DropInState::Unsupported => (
            crate::locale::text("不受支持", "Unsupported"),
            RowTone::Muted,
        ),
        DropInState::ReservedIdentity => (
            crate::locale::text("身份冲突", "Identity conflict"),
            RowTone::Error,
        ),
    }
}

/// 分类筛选值保持目录原文。
fn category_label(category: &str) -> SharedString {
    SharedString::from(category.to_owned())
}

/// 详情版本行先说明当前与可用版本，再补充重启状态，避免用户把待重启误当成已生效。
/// 待重启文案必须跟随真实变更类型：只有 Install/Update 才能声称安装或更新，
/// 启停、卸载与回退统一用“变更已提交”，否则同版本操作会被误报成升级。
/// 启动应用 pending 失败时优先陈述失败与当前版本，不能再声称已提交待生效。
pub(super) fn detail_version_line(
    available: Option<&CatalogSelection>,
    current: Option<&kerminal_domain::plugins::InstalledPlugin>,
    pending: Option<&PendingPluginChange>,
    failed: bool,
) -> String {
    if failed {
        return match current {
            Some(current) => crate::locale::text(
                format!("当前 v{} · 变更失败，未生效", current.active),
                format!("Current v{} · Change failed, not applied", current.active),
            )
            .to_string(),
            None => {
                crate::locale::text("变更失败，未生效", "Change failed, not applied").to_string()
            }
        };
    }
    if let Some(change) = pending {
        return match change {
            PendingPluginChange::Install { .. } => crate::locale::text(
                "安装已提交 · 重启后生效",
                "Installation submitted · Restart required",
            )
            .to_string(),
            PendingPluginChange::Update { to, .. } => match current {
                Some(current) => crate::locale::text(
                    format!(
                        "当前 v{} · 更新至 v{} 已提交 · 重启后生效",
                        current.active, to
                    ),
                    format!(
                        "Current v{} · Update to v{} submitted · Restart required",
                        current.active, to
                    ),
                )
                .to_string(),
                // 没有已安装记录时无法证明这是升级，退回通用变更事实。
                None => crate::locale::text(
                    "变更已提交 · 重启后生效",
                    "Change submitted · Restart required",
                )
                .to_string(),
            },
            PendingPluginChange::Enable { .. }
            | PendingPluginChange::Disable { .. }
            | PendingPluginChange::Uninstall { .. }
            | PendingPluginChange::Rollback { .. } => match current {
                Some(current) => crate::locale::text(
                    format!("当前 v{} · 变更已提交 · 重启后生效", current.active),
                    format!(
                        "Current v{} · Change submitted · Restart required",
                        current.active
                    ),
                )
                .to_string(),
                None => crate::locale::text(
                    "变更已提交 · 重启后生效",
                    "Change submitted · Restart required",
                )
                .to_string(),
            },
        };
    }
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
}

/// 权限区块：分区小标题 + 柔和卡片，完全访问摘要始终位于第一行。
///
/// 逐项权限用“原名 + 说明”两行表达，长权限名可以自然换行，不会被单行
/// 拼接挤到卡片边缘；卡片内边距与正文留白共用主题语义值，不引入散落色值。
///
/// @author kongweiguang
fn permissions_block(permissions: &[String], full_access: bool, theme: ThemeTokens) -> AnyElement {
    let mut card = div()
        .w_full()
        .p_4()
        .rounded(theme.control_radius)
        .bg(theme.muted_surface())
        .border_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .gap_2()
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
        card = card.child(
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
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.foreground)
                            .child(permission.clone()),
                    )
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(permission_description(permission)),
                    ),
            );
        }
    }
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted)
                .child(crate::locale::text("权限", "Permissions")),
        )
        .child(card)
        .into_any_element()
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
    let url = crate::plugins::external::view::allowed_external_url(&url);
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
