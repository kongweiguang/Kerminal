//! 偏好设置页面的主布局、导航、恢复提示与离开确认。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Focusable as _, IntoElement, Render, Window,
    assets::IconName,
    base::Selectable,
    component::{
        Disableable, Icon,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::preferences::{ConfigurationMode, ConfigurationRecoveryState};

use crate::theme::ThemeTokens;

use super::panel::{PreferencesPanel, PreferencesSection, recovery_issue_label};

/// 搜索结果保存实际页面卡片索引，导航时可定位到对应设置而非只切换大类。
///
/// @author kongweiguang
struct NavigationTarget {
    id: String,
    section: PreferencesSection,
    label: gpui_kit::SharedString,
    description: String,
    keywords: String,
    icon: IconName,
    scroll_index: Option<usize>,
    category: bool,
}

/// 搜索栏过滤后的正文选择；无匹配时保留原 section 并呈现空态，外部目标独立投影。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NavigationSearchSelection {
    pub(super) section: PreferencesSection,
    pub(super) selected_target: Option<String>,
    pub(super) has_matches: bool,
    pub(super) scroll_index: Option<usize>,
}

/// 将查询投影为实际导航结果；字段命中优先于宽泛分类，避免展示不相关的页面入口。
///
/// @author kongweiguang
fn matching_navigation_targets(
    settings: Option<&kerminal_application::preferences::SettingsSnapshot>,
    mcp_runtime_available: bool,
    query: &str,
) -> Vec<NavigationTarget> {
    let query = query.trim().to_lowercase();
    let matches = navigation_targets(settings, mcp_runtime_available)
        .into_iter()
        .filter(|target| {
            (query.is_empty() || !target.category)
                && matches_navigation_query(&target.keywords, &query)
        })
        .take(if query.is_empty() { usize::MAX } else { 8 })
        .collect::<Vec<_>>();
    if query.is_empty() || !matches.iter().any(|target| !target.category) {
        return matches;
    }
    matches
        .into_iter()
        .filter(|target| !target.category)
        .collect()
}

/// 搜索优先选择具体设置字段，避免分类关键词遮住更精确的字段命中；空查询保留当前页。
///
/// @author kongweiguang
pub(super) fn resolve_navigation_search_selection(
    query: &str,
    current_section: PreferencesSection,
    current_target: Option<&str>,
    settings: Option<&kerminal_application::preferences::SettingsSnapshot>,
    mcp_runtime_available: bool,
) -> NavigationSearchSelection {
    if query.trim().is_empty() {
        return NavigationSearchSelection {
            section: current_section,
            selected_target: None,
            has_matches: true,
            scroll_index: None,
        };
    }

    let matches = matching_navigation_targets(settings, mcp_runtime_available, query);
    let Some(first) = matches.first() else {
        return NavigationSearchSelection {
            section: current_section,
            selected_target: None,
            has_matches: false,
            scroll_index: None,
        };
    };

    if let Some(selected) = current_target.and_then(|id| matches.iter().find(|t| t.id == id)) {
        return NavigationSearchSelection {
            section: selected.section,
            selected_target: Some(selected.id.clone()),
            has_matches: true,
            scroll_index: selected.scroll_index,
        };
    }
    if let Some(current_page_match) = matches
        .iter()
        .find(|target| target.section == current_section)
    {
        return NavigationSearchSelection {
            section: current_section,
            selected_target: Some(current_page_match.id.clone()),
            has_matches: true,
            scroll_index: current_page_match.scroll_index,
        };
    }

    NavigationSearchSelection {
        section: first.section,
        selected_target: Some(first.id.clone()),
        has_matches: true,
        scroll_index: first.scroll_index,
    }
}

/// 搜索导航字段在构建时保持成组，避免类别、可见标题和定位索引错位。
///
/// @author kongweiguang
struct NavigationTargetSpec<'a> {
    id: &'a str,
    section: PreferencesSection,
    zh_label: &'a str,
    en_label: &'a str,
    keywords: &'a str,
    icon: IconName,
    scroll_index: Option<usize>,
    category: bool,
}

/// 将设置页的可见卡片映射到同一搜索目录，索引对应各页滚动容器的直接子项。
/// 固定宽度导航使用简短标题，完整业务名称保留在页内，避免窄栏裁切。
/// 本地主机及默认项已统一到连接管理，不再通过 Profile 分类暴露第二套编辑入口。
///
/// @author kongweiguang
fn navigation_targets(
    _settings: Option<&kerminal_application::preferences::SettingsSnapshot>,
    mcp_runtime_available: bool,
) -> Vec<NavigationTarget> {
    let _ = mcp_runtime_available;
    let pages = [
        (
            "appearance",
            PreferencesSection::Appearance,
            "界面外观",
            "Appearance",
            "语言 language 主题 theme 透明度 opacity 密度 density 背景 图片 background",
            IconName::MonitorCog,
        ),
        (
            "terminal",
            PreferencesSection::Terminal,
            "终端",
            "Terminal",
            "配色 color 字体 font 字号 行高 字重 渲染 renderer 光标 cursor 右键 滚屏 scrollback",
            IconName::Terminal,
        ),
        (
            "keywords",
            PreferencesSection::KeywordHighlights,
            "关键词高亮",
            "Keyword highlights",
            "关键词 高亮 keyword highlight 规则 正则 regex 色板 颜色 预览",
            IconName::Highlighter,
        ),
        (
            "suggestions",
            PreferencesSection::CommandSuggestions,
            "命令提示",
            "Command suggestions",
            "命令 提示 suggestion Provider 历史 远端 路径 Git CLI Spec 接受键 Tab",
            IconName::Clipboard,
        ),
        (
            "sftp",
            PreferencesSection::Sftp,
            "SFTP",
            "SFTP",
            "SFTP 传输 并发 concurrency 流水线 pipeline 包大小 packet 超时 timeout 高级",
            IconName::Network,
        ),
        (
            "external-launch",
            PreferencesSection::ExternalLaunch,
            "外部启动",
            "External launch",
            "外部 启动 external launch SSH 协议 注册 URI 参数 来源 模板",
            IconName::Route,
        ),
        (
            "mcp",
            PreferencesSection::Mcp,
            "MCP",
            "MCP",
            "MCP 服务 启动 停止 endpoint JSON 连接信息 复制",
            IconName::Puzzle,
        ),
        (
            "sync",
            PreferencesSection::ConfigurationSync,
            "同步",
            "Sync",
            "同步 sync Git 工作区 密钥 vault key 保存",
            IconName::KeyRound,
        ),
        (
            "desktop",
            PreferencesSection::Desktop,
            "桌面",
            "Desktop",
            "桌面 通知 notification 耗时 阈值 节流 高级",
            IconName::Bell,
        ),
        (
            "shortcuts",
            PreferencesSection::Shortcuts,
            "快捷键列表",
            "Keyboard shortcuts",
            "快捷键 shortcut keyboard Windows macOS 绑定 binding 默认 恢复",
            IconName::Keyboard,
        ),
        (
            "about",
            PreferencesSection::About,
            "关于",
            "About",
            "关于 about 版本 version 协议 license GitHub 更新 update 下载 安装 重启",
            IconName::Info,
        ),
    ];
    let mut targets: Vec<_> = pages
        .into_iter()
        .map(|(id, section, zh_label, en_label, keywords, icon)| {
            navigation_target(NavigationTargetSpec {
                id,
                section,
                zh_label,
                en_label,
                keywords,
                icon,
                scroll_index: Some(0),
                category: true,
            })
        })
        .collect();
    // 与 main 同源的十八个搜索入口；标题、说明、分类与关键词共同参与匹配。
    for (id, section, title, description, index) in [
        (
            "settings-interface-appearance-panel",
            PreferencesSection::Appearance,
            "应用外观",
            "切换深色、浅色或跟随系统。 dark light system theme 主题 深色 浅色",
            0,
        ),
        (
            "settings-interface-basics-panel",
            PreferencesSection::Appearance,
            "界面基础设置",
            "界面语言、密度和窗口透明度。 language density opacity 语言 密度 透明度",
            0,
        ),
        (
            "settings-background-panel",
            PreferencesSection::Appearance,
            "主页面背景",
            "选择工作台背景图、透明度和铺放方式。 background image 壁纸 背景 图片",
            1,
        ),
        (
            "settings-terminal-theme-panel",
            PreferencesSection::Terminal,
            "终端主题",
            "分别设置浅色和深色终端配色。 terminal color scheme 终端 主题 配色",
            0,
        ),
        (
            "settings-terminal-font-panel",
            PreferencesSection::Terminal,
            "字体配置",
            "终端字体、字号、行高和字重。 font size line height 字体 字号 行高 字重",
            1,
        ),
        (
            "settings-terminal-renderer-panel",
            PreferencesSection::Terminal,
            "终端渲染",
            "GPUI 原生终端渲染与真实诊断状态。 renderer webgl gpu cpu 渲染 显卡 加速",
            2,
        ),
        (
            "settings-terminal-interaction-panel",
            PreferencesSection::Terminal,
            "终端交互",
            "选中复制、右键行为、自动重连和滚屏缓冲。 copy right click reconnect scrollback 右键 复制 重连 缓冲",
            3,
        ),
        (
            "settings-terminal-cursor-panel",
            PreferencesSection::Terminal,
            "光标形态",
            "光标样式和闪烁行为。 cursor blink 光标 闪烁",
            4,
        ),
        (
            "settings-keyword-highlights-panel",
            PreferencesSection::KeywordHighlights,
            "关键词高亮规则",
            "全局终端文本、整词或安全正则高亮规则。 highlight keyword regex word 关键词 高亮 正则 整词 颜色",
            0,
        ),
        (
            "settings-command-suggestions-policy-panel",
            PreferencesSection::CommandSuggestions,
            "命令灰色提示",
            "灰色命令提示开关、接受按键和远端探测策略。 suggestion ghost inline 提示 灰色提示 接受按键",
            0,
        ),
        (
            "settings-command-suggestions-providers-panel",
            PreferencesSection::CommandSuggestions,
            "提示 Provider",
            "历史、远端路径、远端命令、Git 和 CLI Spec provider。 provider history git remote 历史 远端 spec",
            1,
        ),
        (
            "settings-sftp-panel",
            PreferencesSection::Sftp,
            "SFTP 传输",
            "SFTP 传输并发、pipeline 和超时。 sftp transfer pipeline timeout 传输 并发 超时",
            0,
        ),
        (
            "settings-external-launch-compatibility",
            PreferencesSection::ExternalLaunch,
            "外部 SSH 启动",
            "堡垒机、第三方 SSH 参数和 kerminal:// SSH 入口。 external bastion jump ssh putty mobaxterm xshell securecrt kerminal:// 跳板机 堡垒机 外部启动",
            0,
        ),
        (
            "settings-mcp-connection-info",
            PreferencesSection::Mcp,
            "MCP Server",
            "Kerminal MCP Server 状态、endpoint 和工具导航。 mcp server endpoint agent 工具",
            0,
        ),
        (
            "settings-sync-panel",
            PreferencesSection::ConfigurationSync,
            "同步",
            "Git、密钥和配置同步。 sync git key 同步 密钥",
            0,
        ),
        (
            "settings-desktop-panel",
            PreferencesSection::Desktop,
            "桌面通知",
            "系统通知、后台事件和节流。 notification desktop background 通知 后台 节流",
            0,
        ),
        (
            "settings-keybindings-panel",
            PreferencesSection::Shortcuts,
            "快捷键",
            "Windows 和 macOS 快捷键列表。 keyboard shortcut keybinding 快捷键 键盘",
            0,
        ),
        (
            "settings-about-panel",
            PreferencesSection::About,
            "关于",
            "版本、更新和项目链接。 about version update 关于 版本 更新",
            0,
        ),
    ] {
        let mut target = navigation_target(NavigationTargetSpec {
            id,
            section,
            zh_label: title,
            en_label: title,
            keywords: description,
            icon: IconName::Search,
            scroll_index: Some(index),
            category: false,
        });
        target.description = description.to_owned();
        targets.push(target);
    }

    targets
}

/// 双语标题和关键词在一个目标对象中共同过滤，图标与滚动索引只影响导航呈现。
///
/// @author kongweiguang
fn navigation_target(spec: NavigationTargetSpec<'_>) -> NavigationTarget {
    let NavigationTargetSpec {
        id,
        section,
        zh_label,
        en_label,
        keywords,
        icon,
        scroll_index,
        category,
    } = spec;
    NavigationTarget {
        id: id.to_owned(),
        section,
        label: crate::locale::text(zh_label, en_label),
        description: String::new(),
        keywords: format!("{zh_label} {en_label} {keywords}"),
        icon,
        scroll_index,
        category,
    }
}

impl Render for PreferencesPanel {
    /// 沿用 main 的十一页和即时反馈；输入搜索时正文保留当前页，避免滚动与焦点跳动。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(window, cx);
        let _search_selection = self.reconcile_navigation_search_selection();
        if let Some(index) = self.pending_navigation_search_scroll.take() {
            let scroll = self.content_scroll.clone();
            window.on_next_frame(move |_, _| scroll.scroll_to_top_of_item(index));
        }
        let theme = ThemeTokens::current(window, cx).for_settings();
        let content = match self.section {
            PreferencesSection::Appearance => {
                self.render_appearance(theme, window, cx).into_any_element()
            }
            PreferencesSection::Terminal => {
                self.render_terminal(theme, window, cx).into_any_element()
            }
            PreferencesSection::KeywordHighlights => self
                .render_keyword_highlights(theme, window, cx)
                .into_any_element(),
            PreferencesSection::CommandSuggestions => self
                .render_command_suggestions(theme, window, cx)
                .into_any_element(),
            PreferencesSection::Sftp => self.render_sftp(theme, window, cx).into_any_element(),
            PreferencesSection::ExternalLaunch => self
                .render_external_launch(theme, window, cx)
                .into_any_element(),
            PreferencesSection::Mcp => self.render_mcp_settings(theme, cx).into_any_element(),
            PreferencesSection::Desktop => {
                self.render_desktop_settings(theme, cx).into_any_element()
            }
            PreferencesSection::About => self.render_about_settings(theme, cx).into_any_element(),
            PreferencesSection::Shortcuts => self
                .render_keybindings_settings(theme, cx)
                .into_any_element(),
            PreferencesSection::Profiles => {
                self.render_profiles(theme, window, cx).into_any_element()
            }
            PreferencesSection::Integrations => {
                self.render_integrations(theme, cx).into_any_element()
            }
            PreferencesSection::ConfigurationSync => {
                self.render_configuration_sync(theme, cx).into_any_element()
            }
        };

        settings_text_font(
            div()
                .id("preferences-panel")
                .size_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .font_weight(gpui_kit::FontWeight::NORMAL)
                .text_color(theme.foreground)
                .child(self.render_header(theme, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .gap(px(16.))
                        .px(px(20.))
                        .py(px(16.))
                        .overflow_hidden()
                        .child(self.render_sidebar(theme, window, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .track_focus(&self.settings_content_focus)
                                .flex()
                                .flex_col()
                                .overflow_hidden()
                                .child(self.render_recovery(theme, cx))
                                .child(content),
                        ),
                )
                .when_some(
                    self.render_close_confirmation(theme, cx),
                    |root, overlay| root.child(overlay),
                )
                .when_some(
                    self.render_profile_switch_confirmation(theme, cx),
                    |root, overlay| root.child(overlay),
                )
                .when_some(
                    self.render_profile_reload_confirmation(theme, cx),
                    |root, overlay| root.child(overlay),
                )
                .when_some(
                    self.render_profile_delete_confirmation(theme, cx),
                    |root, overlay| root.child(overlay),
                )
                .when_some(self.render_quit_confirmation(theme, cx), |root, overlay| {
                    root.child(overlay)
                }),
        )
    }
}

/// Windows Segoe UI does not contain CJK glyphs; direct its fallback to the same YaHei UI family WebView resolves first.
/// The settings subtree owns this fallback so other GPUI surfaces keep their existing platform typography.
///
/// @author kongweiguang
fn settings_text_font<T: gpui_kit::Styled>(element: T) -> T {
    #[cfg(target_os = "windows")]
    {
        let font = gpui_pre::font("Segoe UI");
        element.font(gpui_pre::Font {
            fallbacks: Some(gpui_pre::FontFallbacks::from_fonts(vec![
                "Microsoft YaHei UI".to_owned(),
                "Microsoft YaHei".to_owned(),
            ])),
            ..font
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        element
    }
}

impl PreferencesPanel {
    /// 标题栏保持 main 的 36px 关闭控件和上下 16px 留白，不随正文的 flex 压缩。
    /// Root 恢复打开前页面，离开仍执行同一草稿关闭门禁。
    ///
    /// @author kongweiguang
    fn render_header(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h(px(69.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .px_5()
            .flex()
            .items_start()
            .py_4()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .text_size(px(17.))
                    .line_height(px(24.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(crate::locale::text("设置", "Settings")),
            )
            .child(
                Button::new("preferences-close")
                    .focus_handle(&self.settings_close_focus)
                    .size(px(36.))
                    .compact()
                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                    .icon(IconName::Close)
                    .accessibility_label(crate::locale::text("关闭设置", "Close settings"))
                    .tooltip(crate::locale::text("关闭设置", "Close settings"))
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.close_button(window, cx);
                    })),
            )
    }

    /// 分类与搜索结果共用 main 的窄侧栏；暴露裁切边界供验收先滚动再点击，避免误点栏外控件。
    ///
    /// @author kongweiguang
    fn render_sidebar(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let query = self.navigation_search_query.trim().to_owned();
        let matches = matching_navigation_targets(
            self.settings.as_ref().map(|draft| draft.value()),
            self.mcp_runtime_available,
            &query,
        );
        let matches: Vec<_> = if query.is_empty() {
            matches
                .into_iter()
                .filter(|target| target.category)
                .collect()
        } else {
            matches
        };
        let no_matches = !query.is_empty() && matches.is_empty();
        let mut sidebar = div()
            .w(px(190.))
            .flex_shrink_0()
            .self_start()
            .max_h_full()
            .flex()
            .flex_col()
            .gap_2()
            .pr(px(16.))
            .border_r_1()
            .border_color(theme.border)
            .child(
                div().id("preferences-search").child(
                    gpui_kit::Styled::h(
                        super::render_settings::settings_input(&self.navigation_search, theme),
                        px(36.),
                    )
                    .w_full()
                    .prefix(
                        Icon::new(IconName::Search)
                            .size(px(16.))
                            .text_color(theme.muted),
                    )
                    .suffix(div().when(
                        !self.navigation_search_query.is_empty(),
                        |row| {
                            row.child(
                                Button::new("preferences-search-clear")
                                    .custom(super::render_settings::settings_ghost_variant(
                                        theme, cx,
                                    ))
                                    .size(px(24.))
                                    .icon(IconName::Close)
                                    .accessibility_label(crate::locale::text(
                                        "清除设置搜索",
                                        "Clear settings search",
                                    ))
                                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                        panel.navigation_search_query.clear();
                                        panel.navigation_search.update(cx, |input, cx| {
                                            input.set_value(String::new(), window, cx)
                                        });
                                        cx.notify();
                                    })),
                            )
                        },
                    )),
                ),
            );
        let mut nav_rows = matches
            .into_iter()
            .map(|target| self.nav_button(target, theme, cx).into_any_element())
            .collect::<Vec<_>>();
        if no_matches {
            nav_rows.push(
                div()
                    .px_2()
                    .text_size(theme.text_sm)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "没有找到匹配设置。",
                        "No matching settings found.",
                    ))
                    .into_any_element(),
            );
        }
        sidebar = sidebar.child(
            div()
                .id("preferences-navigation-results")
                .debug_selector(|| "preferences-navigation-viewport".into())
                .flex_shrink_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .children(nav_rows),
        );
        sidebar
    }

    /// 导航项保持左对齐的扁平 ghost 外观；外部工作区目标经共享离开门禁处理。
    ///
    /// @author kongweiguang
    fn nav_button(
        &mut self,
        target: NavigationTarget,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let selected = if self.navigation_search_query.trim().is_empty() {
            target.category && self.section == target.section
        } else {
            self.selected_navigation_target.as_deref() == Some(target.id.as_str())
        };
        let target_id = target.id.clone();
        let target_section = target.section;
        let scroll_index = target.scroll_index;
        if !target.category {
            let description = target
                .description
                .split_once('。')
                .map_or(target.description.as_str(), |(text, _)| text)
                .to_owned();
            let category = navigation_targets(None, self.mcp_runtime_available)
                .into_iter()
                .find(|entry| entry.category && entry.section == target.section)
                .map(|entry| entry.label)
                .unwrap_or_default();
            let description = format!("{category} · {description}。");
            return Button::new(format!("preferences-nav-{}", target.id))
                .custom(super::render_settings::settings_ghost_variant(theme, cx))
                .w_full()
                .h_auto()
                .px(px(10.))
                .py(px(8.))
                .accessibility_label(format!("打开设置项：{}", target.label))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_left()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(theme.settings_medium_font_weight())
                                .child(target.label),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.muted)
                                .child(description),
                        ),
                )
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.navigate_to_target(
                        target_id.clone(),
                        target_section,
                        scroll_index,
                        window,
                        cx,
                    );
                }))
                .into_any_element();
        }
        let label = target.label.clone();
        let text_color = theme.settings_choice_foreground(selected);
        let icon_color = if selected { text_color } else { theme.muted };
        let button = Button::new(format!("preferences-nav-{}", target.id))
            .w_full()
            .min_h(px(40.))
            .h_auto()
            .compact()
            .px(px(8.))
            .py(px(6.))
            .custom(super::render_settings::settings_ghost_variant(theme, cx))
            .selected(selected)
            .when(selected, |button| button.bg(theme.surface_selected()))
            .accessibility_label(label.clone())
            .child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(32.))
                            .flex_shrink_0()
                            .rounded(theme.radius_sm)
                            .bg(if selected {
                                theme.surface_selected()
                            } else {
                                theme.muted_surface()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(target.icon).size(px(16.)).text_color(icon_color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_left()
                            .text_size(px(13.))
                            .font_weight(theme.settings_medium_font_weight())
                            .text_color(text_color)
                            .child(label),
                    ),
            )
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.navigate_to_target(
                    target_id.clone(),
                    target_section,
                    scroll_index,
                    window,
                    cx,
                );
            }));
        div()
            .w_full()
            .rounded(theme.control_radius)
            .child(button)
            .into_any_element()
    }

    /// 切换页面后等新滚动内容完成布局再定位，避免搜索结果指向上一页的组件索引。
    ///
    /// @author kongweiguang
    fn navigate_to_target(
        &mut self,
        id: String,
        section: PreferencesSection,
        scroll_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.section != section
            || self.selected_navigation_target.as_deref() != Some(id.as_str());
        if self.section != section {
            self.reset_page_disclosures();
            if self.section == PreferencesSection::KeywordHighlights {
                self.discard_pending_keyword_rule();
            }
        }
        self.section = section;
        match id.as_str() {
            "settings-external-launch-compatibility" => {
                self.reveal_connectivity_target("external-sources", cx)
            }
            "settings-mcp-connection-info" => self.reveal_connectivity_target("mcp-connection", cx),
            _ => {}
        }
        match section {
            PreferencesSection::Terminal if id == "settings-terminal-renderer-panel" => {
                self.reveal_terminal_renderer(cx)
            }
            PreferencesSection::Sftp if id == "settings-sftp-panel" => {
                self.reveal_sftp_advanced(cx)
            }
            PreferencesSection::Desktop if id == "settings-desktop-panel" => {
                self.reveal_desktop_advanced(cx)
            }
            _ => {}
        }
        let focus = match id.as_str() {
            "settings-terminal-font-panel" => {
                self.settings_inputs.font_family.read(cx).focus_handle(cx)
            }
            "settings-background-panel" => self
                .settings_inputs
                .background_path
                .read(cx)
                .focus_handle(cx),
            _ => self.settings_content_focus.clone(),
        };
        self.selected_navigation_target = Some(id);
        self.navigation_search_query.clear();
        self.navigation_search
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        if changed {
            cx.notify();
        }
        if section != PreferencesSection::Profiles
            && let Some(index) = scroll_index
        {
            let scroll = self.content_scroll.clone();
            window.on_next_frame(move |_, _| scroll.scroll_to_top_of_item(index));
        }
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
    }

    /// 只读恢复条目提供固定配置目录入口，状态只反映系统请求回执，不承诺窗口已显示。
    ///
    /// @author kongweiguang
    fn render_recovery(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let can_open_directory = self.configuration_directory.is_some();
        let request_pending = matches!(
            self.recovery_directory_status.as_ref(),
            Some(&super::panel::RecoveryDirectoryStatus::Requesting)
        );
        let status = self.recovery_directory_status.clone();

        div().when_some(recovery_banner(&self.recovery, theme), |root, banner| {
            root.child(banner).child(
                div()
                    .w_full()
                    .px_4()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("preferences-open-config-directory")
                            .compact()
                            .label(crate::locale::text("打开配置目录", "Open configuration folder"))
                            .disabled(!can_open_directory || request_pending)
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.open_recovery_configuration_directory(cx);
                            })),
                    )
                    .when_some(status, |row, status| {
                        let (message, is_error) = match status {
                            super::panel::RecoveryDirectoryStatus::Requesting => (
                                crate::locale::text(
                                    "正在请求打开配置目录…",
                                    "Requesting the configuration folder…",
                                )
                                .to_string(),
                                false,
                            ),
                            super::panel::RecoveryDirectoryStatus::Accepted => (
                                crate::locale::text(
                                    "系统已接受请求；文件管理器是否显示由系统决定。",
                                    "The OS accepted the request; folder visibility depends on the system.",
                                )
                                .to_string(),
                                false,
                            ),
                            super::panel::RecoveryDirectoryStatus::Failed(message) => {
                                (message, true)
                            }
                        };
                        row.child(
                            div()
                                .flex_1()
                                .text_size(theme.text_xs)
                                .text_color(if is_error { theme.error } else { theme.muted })
                                .child(message),
                        )
                    }),
            )
        })
    }

    /// 保存反馈放在正文末尾；只有确认成功才显示 main 的勾，失败和在途状态不冒充保存完成。
    /// @author kongweiguang
    pub(super) fn render_save_notice(&self, theme: ThemeTokens) -> impl IntoElement {
        let message = self
            .settings_error
            .as_deref()
            .or(self.status.as_deref())
            .map(str::to_owned)
            .or_else(|| {
                self.settings_success_status
                    .map(|state| state.message(crate::locale::current_language()).to_owned())
            });
        div().when_some(message, |row, message| {
            let failed = self.settings_error.is_some()
                || !self.settings_input_errors.is_empty()
                || self.settings_issues.iter().any(|issue| issue.blocks_save())
                || self.settings.as_ref().is_some_and(|draft| {
                    matches!(
                        draft.status(),
                        super::DraftStatus::Failed
                            | super::DraftStatus::ExternalConflict
                            | super::DraftStatus::OutcomeUnknown
                    )
                });
            let color = if failed { theme.error } else { theme.success };
            row.rounded(theme.control_radius)
                .border_1()
                .border_color(color.opacity(0.2))
                .bg(color.opacity(0.1))
                .px_3()
                .py_2()
                .text_size(px(13.))
                .text_color(if failed {
                    color
                } else {
                    theme.settings_saved_foreground()
                })
                .flex()
                .items_center()
                .gap_2()
                .when(
                    !failed
                        && self.status.is_none()
                        && self.settings_success_status
                            == Some(super::panel::SettingsSuccessStatus::Saved),
                    |notice| notice.child(Icon::new(IconName::Check).size(px(16.))),
                )
                .child(message)
        })
    }

    /// Dirty-close 提示同时覆盖两个草稿上下文，取消保留输入，丢弃后才发出根视图返回事件。
    fn render_close_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.close_confirmation.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .child(
                    div()
                        .w(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.muted)
                        .rounded_md()
                        .child(crate::locale::text(
                            crate::locale::text(
                                "设置尚有未保存修改",
                                "Settings contain unsaved changes",
                            ),
                            "Preferences contain unsaved changes",
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("preferences-close-cancel")
                                        .compact()
                                        .label(crate::locale::text("继续编辑", "Keep editing"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.cancel_close(cx)
                                        })),
                                )
                                .child(
                                    Button::new("preferences-close-discard")
                                        .compact()
                                        .primary()
                                        .label(crate::locale::text(
                                            crate::locale::text("丢弃并返回", "Discard and return"),
                                            "Discard and return",
                                        ))
                                        .on_click(cx.listener(
                                            |panel, _: &ClickEvent, window, cx| {
                                                panel.discard_and_close(window, cx)
                                            },
                                        )),
                                ),
                        ),
                )
        })
    }

    /// 切换 profile 前的丢弃提示局限在 Profile 表单，不会误丢其它设置草稿。
    fn render_profile_switch_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.profile_switch_confirmation.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .child(
                    div()
                        .w(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.muted)
                        .rounded_md()
                        .child(crate::locale::text(
                            crate::locale::text(
                                "当前 Profile 有未保存修改",
                                "This profile has unsaved changes",
                            ),
                            "This profile has unsaved changes",
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("profile-switch-cancel")
                                        .compact()
                                        .label(crate::locale::text("继续编辑", "Keep editing"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.profile_switch_confirmation = false;
                                            panel.pending_profile_selection = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("profile-switch-discard")
                                        .compact()
                                        .primary()
                                        .label(crate::locale::text(
                                            crate::locale::text("丢弃并切换", "Discard and switch"),
                                            "Discard and switch",
                                        ))
                                        .on_click(cx.listener(
                                            |panel, _: &ClickEvent, window, cx| {
                                                panel.confirm_profile_discard(window, cx)
                                            },
                                        )),
                                ),
                        ),
                )
        })
    }

    /// 外部 Profile 版本冲突后，重读前明确提示会丢弃当前本地草稿。
    fn render_profile_reload_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.profile_reload_confirmation.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .child(
                    div()
                        .w(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.muted)
                        .rounded_md()
                        .child(crate::locale::text(
                            crate::locale::text(
                                "重新读取会放弃当前 Profile 草稿",
                                "Reloading will discard this profile draft",
                            ),
                            "Reloading will discard this profile draft",
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("profile-reload-cancel")
                                        .compact()
                                        .label(crate::locale::text("继续编辑", "Keep editing"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.cancel_profile_reload(cx)
                                        })),
                                )
                                .child(
                                    Button::new("profile-reload-confirm")
                                        .compact()
                                        .primary()
                                        .label(crate::locale::text(
                                            crate::locale::text(
                                                "丢弃并重新读取",
                                                "Discard and reload",
                                            ),
                                            "Discard and reload",
                                        ))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.confirm_profile_reload(cx)
                                        })),
                                ),
                        ),
                )
        })
    }

    /// 删除使用单独的二次确认，避免列表按钮误触发持久化删除。
    fn render_profile_delete_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.profile_delete_confirmation.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .child(
                    div()
                        .w(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.muted)
                        .rounded_md()
                        .child(crate::locale::text(
                            "确认删除此 Profile？",
                            "Delete this profile?",
                        ))
                        .child(crate::locale::text(
                            "删除后无法从设置面板撤销。",
                            "This cannot be undone from Preferences.",
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("profile-delete-cancel")
                                        .compact()
                                        .label(crate::locale::text("取消", "Cancel"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.cancel_delete_profile(cx)
                                        })),
                                )
                                .child(
                                    Button::new("profile-delete-confirm")
                                        .compact()
                                        .primary()
                                        .label(crate::locale::text(
                                            "删除 Profile",
                                            "Delete profile",
                                        ))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.confirm_delete_profile(cx)
                                        })),
                                ),
                        ),
                )
        })
    }

    /// 偏好草稿退出确认由面板先完成，根窗口再依次检查编辑器并决定是否关闭。
    fn render_quit_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.quit_confirmation.then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0., 0., 0., 0.45))
                .child(
                    div()
                        .w(px(440.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.muted)
                        .rounded_md()
                        .child(crate::locale::text(
                            crate::locale::text("偏好设置有未保存修改", "Preferences contain unsaved changes"),
                            "Preferences contain unsaved changes",
                        ))
                        .child(crate::locale::text(
                            crate::locale::text("退出将丢弃设置、Profile 或尚未添加的临时输入。", "Quitting will discard settings, profile edits, and unsubmitted inputs."),
                            "Quitting will discard settings, profile edits, and pending inputs.",
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("preferences-quit-cancel")
                                        .compact()
                                        .label(crate::locale::text("继续编辑", "Keep editing"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.cancel_quit(cx)
                                        })),
                                )
                                .child(
                                    Button::new("preferences-quit-confirm")
                                        .compact()
                                        .primary()
                                        .label(crate::locale::text(
                                            crate::locale::text("丢弃并退出", "Discard and quit"),
                                            "Discard and quit",
                                        ))
                                        .on_click(cx.listener(
                                            |panel, _: &ClickEvent, window, cx| {
                                                panel.confirm_quit(window, cx)
                                            },
                                        )),
                                ),
                        ),
                )
        })
    }
}

/// 搜索查询按空格分词并要求全部命中，支持中文词与英文多词设置名。
///
/// @author kongweiguang
fn matches_navigation_query(keywords: &str, query: &str) -> bool {
    let keywords = keywords.to_lowercase();
    query.is_empty() || query.split_whitespace().all(|term| keywords.contains(term))
}

#[cfg(test)]
#[path = "../../../tests/unit/preferences_navigation.rs"]
mod tests;

/// 只读恢复提示独立于当前 section，原因由受控枚举映射而不是原始解析器错误组成。
pub(super) fn recovery_banner(
    state: &ConfigurationRecoveryState,
    theme: ThemeTokens,
) -> Option<impl IntoElement> {
    if state.mode != ConfigurationMode::ReadOnlyRecovery {
        return None;
    }
    let rows = state.issues.iter().map(|issue| {
        let position = match (issue.line, issue.column) {
            (Some(line), Some(column)) => crate::locale::text(
                format!("第 {line} 行，第 {column} 列"),
                format!("Line {line}, column {column}"),
            )
            .to_string(),
            (Some(line), None) => {
                crate::locale::text(format!("第 {line} 行"), format!("Line {line}")).to_string()
            }
            _ => crate::locale::text("位置未知", "Location unknown").to_string(),
        };
        div()
            .w_full()
            .flex()
            .gap_2()
            .text_size(px(12.))
            .text_color(theme.error)
            .child(format!(
                "{} · {} · {}",
                issue.relative_path,
                recovery_issue_label(issue.kind),
                position
            ))
    });
    Some(
        div()
            .w_full()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_1()
            .bg(theme.panel)
            .child(crate::locale::text("配置处于只读恢复模式。请先备份并修复列出的文件；应用不会用默认值覆盖原内容。", "Configuration is in read-only recovery mode. Back up and repair the listed files; defaults will not overwrite them."))
            .children(rows),
    )
}

/// 保存错误或重载中的错误通过安全文案显示；调用方从不格式化 AppError source。
pub(in crate::features::preferences) fn settings_error_banner(
    message: Option<&str>,
    theme: ThemeTokens,
) -> Option<impl IntoElement> {
    message.map(|message| {
        div()
            .w_full()
            .px_3()
            .py_2()
            .text_color(theme.error)
            .bg(theme.panel)
            .child(message.to_owned())
    })
}
