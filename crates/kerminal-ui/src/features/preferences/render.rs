//! 偏好设置页面的主布局、导航、恢复提示与离开确认。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Render, Window,
    assets::IconName,
    base::Selectable,
    component::{
        Disableable, Icon,
        button::{Button, ButtonVariants},
        input::Input,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::preferences::{ConfigurationMode, ConfigurationRecoveryState};

use crate::theme::ThemeTokens;

use super::{
    panel::{
        PreferencesNavigationDestination, PreferencesPanel, PreferencesSection,
        recovery_issue_label,
    },
    render_settings::keybinding_label,
};

/// 搜索结果保存实际页面卡片索引，导航时可定位到对应设置而非只切换大类。
///
/// @author kongweiguang
struct NavigationTarget {
    id: String,
    section: PreferencesSection,
    label: gpui_kit::SharedString,
    keywords: String,
    icon: IconName,
    scroll_index: Option<usize>,
    category: bool,
    destination: Option<PreferencesNavigationDestination>,
}

/// 搜索栏过滤后的正文选择；无匹配时保留原 section 并呈现空态，外部目标独立投影。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NavigationSearchSelection {
    pub(super) section: PreferencesSection,
    pub(super) selected_target: Option<String>,
    pub(super) has_matches: bool,
    pub(super) scroll_index: Option<usize>,
    pub(super) destination: Option<PreferencesNavigationDestination>,
}

/// 正文只展示命中字段、受控外部目的地或空态，避免搜索结果落到同类但无关页面。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationBodyProjection {
    NoResults,
    Destination(PreferencesNavigationDestination),
    Section(PreferencesSection),
}

/// 将搜索选择映射到唯一正文分支；renderer 与测试共同消费此纯投影。
///
/// @author kongweiguang
fn project_navigation_body(
    query: &str,
    selection: &NavigationSearchSelection,
) -> NavigationBodyProjection {
    if !query.trim().is_empty() && !selection.has_matches {
        NavigationBodyProjection::NoResults
    } else if let Some(destination) = selection.destination {
        NavigationBodyProjection::Destination(destination)
    } else {
        NavigationBodyProjection::Section(selection.section)
    }
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
        .filter(|target| matches_navigation_query(&target.keywords, &query))
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
            destination: None,
        };
    }

    let matches = matching_navigation_targets(settings, mcp_runtime_available, query);
    let Some(first) = matches.first() else {
        return NavigationSearchSelection {
            section: current_section,
            selected_target: None,
            has_matches: false,
            scroll_index: None,
            destination: None,
        };
    };

    if let Some(selected) = current_target.and_then(|id| matches.iter().find(|t| t.id == id)) {
        return NavigationSearchSelection {
            section: selected.section,
            selected_target: Some(selected.id.clone()),
            has_matches: true,
            scroll_index: selected.scroll_index,
            destination: selected.destination,
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
            destination: current_page_match.destination,
        };
    }

    NavigationSearchSelection {
        section: first.section,
        selected_target: Some(first.id.clone()),
        has_matches: true,
        scroll_index: first.scroll_index,
        destination: first.destination,
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
    settings: Option<&kerminal_application::preferences::SettingsSnapshot>,
    mcp_runtime_available: bool,
) -> Vec<NavigationTarget> {
    let mut targets = vec![
        navigation_target(NavigationTargetSpec {
            id: "category-appearance",
            section: PreferencesSection::Appearance,
            zh_label: "外观",
            en_label: "Appearance",
            keywords: "界面 外观 主题 theme language 语言 density 密度 background 背景 image 图片 opacity 透明度",
            icon: IconName::Palette,
            scroll_index: Some(0),
            category: true,
        }),
        navigation_target(NavigationTargetSpec {
            id: "appearance-theme",
            section: PreferencesSection::Appearance,
            zh_label: "主题",
            en_label: "Theme",
            keywords: "主题 theme light 浅色 dark 深色 system 系统 appearance 外观",
            icon: IconName::Palette,
            scroll_index: Some(0),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "appearance-language",
            section: PreferencesSection::Appearance,
            zh_label: "界面语言",
            en_label: "Interface language",
            keywords: "语言 language zh 中文 english 英文",
            icon: IconName::Palette,
            scroll_index: Some(0),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "appearance-density",
            section: PreferencesSection::Appearance,
            zh_label: "界面密度",
            en_label: "Interface density",
            keywords: "密度 density compact 紧凑 comfortable 舒适 spacious 宽松",
            icon: IconName::Palette,
            scroll_index: Some(0),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "appearance-background",
            section: PreferencesSection::Appearance,
            zh_label: "背景图片与透明度",
            en_label: "Background image and opacity",
            keywords: "背景 background image 图片 fit 适配 cover contain tile 平铺 opacity 透明度 window 窗口",
            icon: IconName::Palette,
            scroll_index: Some(1),
            category: false,
        }),
        {
            let mut target = navigation_target(NavigationTargetSpec {
                id: "about-updates",
                section: PreferencesSection::Appearance,
                zh_label: "关于与更新",
                en_label: "About & updates",
                keywords: "about 关于 updates update version 版本 更新 release 发布",
                icon: IconName::Info,
                scroll_index: None,
                category: false,
            });
            target.destination = Some(PreferencesNavigationDestination::Updates);
            target
        },
        navigation_target(NavigationTargetSpec {
            id: "category-terminal",
            section: PreferencesSection::Terminal,
            zh_label: "终端外观",
            en_label: "Terminal",
            keywords: "终端 terminal font 字体字号 size 行高 line-height scrollback 滚屏 cursor 光标 color 颜色 palette shell",
            icon: IconName::SquareTerminal,
            scroll_index: Some(0),
            category: true,
        }),
        navigation_target(NavigationTargetSpec {
            id: "terminal-font",
            section: PreferencesSection::Terminal,
            zh_label: "字体与滚屏缓冲",
            en_label: "Font and scrollback",
            keywords: "font family 字体字号 size 字体与滚动 line-height 行高 scrollback 滚动 滚屏 缓冲 滚屏缓冲行数",
            icon: IconName::SquareTerminal,
            scroll_index: Some(1),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "terminal-cursor-color",
            section: PreferencesSection::Terminal,
            zh_label: "光标与配色",
            en_label: "Cursor and color scheme",
            keywords: "cursor 光标 blink 闪烁 style 样式 color scheme 配色 palette 颜色",
            icon: IconName::SquareTerminal,
            scroll_index: Some(2),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "terminal-behavior",
            section: PreferencesSection::Terminal,
            zh_label: "输入行为、建议与关键词",
            en_label: "Input behavior, suggestions, and keywords",
            keywords: "terminal behavior rightclick 右键 option meta paste 粘贴 suggestion 建议 inline suggestion command block 命令块 highlight 高亮 keyword 关键词 selection copy",
            icon: IconName::SquareTerminal,
            scroll_index: Some(3),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "category-shortcuts",
            section: PreferencesSection::Shortcuts,
            zh_label: "快捷键",
            en_label: "Keyboard shortcuts",
            keywords: "快捷键 shortcut shortcuts keyboard 键盘 binding 绑定 conflict 冲突 invalid 无效",
            icon: IconName::Keyboard,
            scroll_index: Some(0),
            category: true,
        }),
        navigation_target(NavigationTargetSpec {
            id: "shortcuts-diagnostics",
            section: PreferencesSection::Shortcuts,
            zh_label: "快捷键冲突与校验",
            en_label: "Shortcut conflicts and validation",
            keywords: "快捷键 shortcut conflict 冲突 invalid 无效 binding 绑定 parser chord 组合键",
            icon: IconName::Keyboard,
            scroll_index: Some(0),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "category-integrations",
            section: PreferencesSection::Integrations,
            zh_label: "通知与集成",
            en_label: "Notifications",
            keywords: "通知 notification integrations 集成 sftp launcher 启动器 suggestions 建议 keywords 关键词 toolrail 工具栏 agent external launch 外部启动",
            icon: IconName::Bell,
            scroll_index: Some(0),
            category: true,
        }),
        navigation_target(NavigationTargetSpec {
            id: "integrations-notifications",
            section: PreferencesSection::Integrations,
            zh_label: "桌面通知",
            en_label: "Desktop notifications",
            keywords: "notification notifications 桌面通知 duration throttle important 后台重要事件",
            icon: IconName::Bell,
            scroll_index: Some(1),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "integrations-external-launch",
            section: PreferencesSection::Integrations,
            zh_label: "外部启动",
            en_label: "External launch",
            keywords: "external launch 外部启动 ssh sources 来源 vendor args third party 第三方 sftp",
            icon: IconName::Bell,
            scroll_index: Some(2),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "integrations-sftp",
            section: PreferencesSection::Integrations,
            zh_label: "SFTP 传输",
            en_label: "SFTP transfers",
            keywords: "sftp transfer transfers 传输 concurrency 并发 pipeline packet bytes idle timeout 空闲超时",
            icon: IconName::Bell,
            scroll_index: Some(3),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "integrations-agent-launcher",
            section: PreferencesSection::Integrations,
            zh_label: "Agent 启动器",
            en_label: "Agent Launcher",
            keywords: "agent launcher 启动器 codex claude custom 自定义 command 命令",
            icon: IconName::Bell,
            scroll_index: Some(4),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "integrations-tool-rail",
            section: PreferencesSection::Integrations,
            zh_label: "工具栏布局",
            en_label: "Tool rail layout",
            keywords: "tool rail toolbar 工具栏 order 顺序 hidden 隐藏 bottom 底部 pin placement 位置",
            icon: IconName::Bell,
            scroll_index: Some(5),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "integrations-protocol",
            section: PreferencesSection::Integrations,
            zh_label: "kerminal:// 协议状态",
            en_label: "kerminal:// protocol status",
            keywords: "external protocol URI handler URL 链接 注册状态",
            icon: IconName::ExternalLink,
            scroll_index: Some(6),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "category-sync",
            section: PreferencesSection::ConfigurationSync,
            zh_label: "配置同步",
            en_label: "Configuration sync",
            keywords: "配置同步 configuration sync git fetch pull commit push 冲突 conflict remote 远端",
            icon: IconName::CloudSync,
            scroll_index: Some(0),
            category: true,
        }),
        navigation_target(NavigationTargetSpec {
            id: "sync-actions",
            section: PreferencesSection::ConfigurationSync,
            zh_label: "同步操作与仓库状态",
            en_label: "Sync actions and repository status",
            keywords: "fetch pull commit push remote 远端 branch 分支 upstream ahead behind conflict 冲突 rebase 变基",
            icon: IconName::CloudSync,
            scroll_index: Some(0),
            category: false,
        }),
        navigation_target(NavigationTargetSpec {
            id: "category-vault",
            section: PreferencesSection::Vault,
            zh_label: "凭据密钥",
            en_label: "Vault keys",
            keywords: "凭据 vault key 密钥 credential recovery 恢复 import export rotate 轮换 backup 备份",
            icon: IconName::KeyRound,
            scroll_index: Some(0),
            category: true,
        }),
        navigation_target(NavigationTargetSpec {
            id: "vault-actions",
            section: PreferencesSection::Vault,
            zh_label: "密钥检查、导入与备份",
            en_label: "Key checks, import, and backup",
            keywords: "vault key readiness 状态 import 导入 export 导出 rotate 轮换 restore 恢复 backup 备份",
            icon: IconName::KeyRound,
            scroll_index: Some(0),
            category: false,
        }),
    ];

    if let Some(settings) = settings {
        for (index, binding) in settings.keybindings.iter().enumerate() {
            let display_name = keybinding_label(&binding.action, &binding.label).to_string();
            let zh_label = format!("快捷键 · {display_name}");
            let en_label = format!("Shortcut · {display_name}");
            let label = crate::locale::text(zh_label, en_label);
            let keywords = format!(
                "{} {} {} {} {} {} {} {} 快捷键 keyboard shortcut binding 组合键",
                binding.action,
                binding.label,
                binding.description,
                binding.binding,
                binding.windows_binding,
                binding
                    .windows_alternate_binding
                    .as_deref()
                    .unwrap_or_default(),
                binding.linux_binding.as_deref().unwrap_or_default(),
                binding.mac_binding
            );
            targets.push(NavigationTarget {
                id: format!("keybinding-{index}"),
                section: PreferencesSection::Shortcuts,
                label,
                keywords,
                icon: IconName::Keyboard,
                scroll_index: Some(index + 2),
                category: false,
                destination: None,
            });
        }
    }

    if mcp_runtime_available {
        let mut target = navigation_target(NavigationTargetSpec {
            id: "mcp-runtime",
            section: PreferencesSection::Integrations,
            zh_label: "MCP 运行时",
            en_label: "MCP runtime",
            keywords: "model context protocol server listener tools endpoint",
            icon: IconName::Network,
            scroll_index: None,
            category: false,
        });
        target.destination = Some(PreferencesNavigationDestination::McpRuntime);
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
        keywords: format!("{zh_label} {en_label} {keywords}"),
        icon,
        scroll_index,
        category,
        destination: None,
    }
}

impl Render for PreferencesPanel {
    /// 根据当前系统外观与已保存主题绘制面板，GPUI 的实际窗口外观变化仍由根视图订阅。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(window, cx);
        let search_selection = self.reconcile_navigation_search_selection();
        if let Some(index) = self.pending_navigation_search_scroll.take() {
            let scroll = self.content_scroll.clone();
            window.on_next_frame(move |_, _| scroll.scroll_to_top_of_item(index));
        }
        let theme = ThemeTokens::current(window, cx);
        let query = self.navigation_search_query.trim();
        let content = match project_navigation_body(query, &search_selection) {
            NavigationBodyProjection::NoResults => {
                self.render_no_search_results(theme).into_any_element()
            }
            NavigationBodyProjection::Destination(destination) => self
                .render_search_destination(destination, theme, cx)
                .into_any_element(),
            NavigationBodyProjection::Section(section) => match section {
                PreferencesSection::Appearance => {
                    self.render_appearance(theme, window, cx).into_any_element()
                }
                PreferencesSection::Terminal => self.render_terminal(theme, cx).into_any_element(),
                PreferencesSection::Shortcuts => {
                    self.render_shortcuts(theme, cx).into_any_element()
                }
                PreferencesSection::Profiles => {
                    self.render_profiles(theme, window, cx).into_any_element()
                }
                PreferencesSection::Integrations => {
                    self.render_integrations(theme, cx).into_any_element()
                }
                PreferencesSection::ConfigurationSync => {
                    self.render_configuration_sync(theme, cx).into_any_element()
                }
                PreferencesSection::Vault => self.render_vault(theme, cx).into_any_element(),
            },
        };

        div()
            .id("preferences-panel")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_header(theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .overflow_hidden()
                    .child(self.render_sidebar(theme, window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(self.render_recovery(theme, cx))
                            .child(content)
                            .child(self.render_footer(theme, cx)),
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
            .when_some(
                self.render_settings_reload_confirmation(theme, cx),
                |root, overlay| root.child(overlay),
            )
            .when_some(self.render_quit_confirmation(theme, cx), |root, overlay| {
                root.child(overlay)
            })
    }
}

impl PreferencesPanel {
    /// 外部运行时与更新结果不伪装成本地配置；按钮沿用现有离开草稿门禁。
    ///
    /// @author kongweiguang
    fn render_search_destination(
        &mut self,
        destination: PreferencesNavigationDestination,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match destination {
            PreferencesNavigationDestination::McpRuntime => div()
                .id("preferences-mcp-runtime-search-result")
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(
                    div()
                        .w_full()
                        .max_w(px(560.))
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.panel)
                        .p_6()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .text_size(theme.text_title)
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(crate::locale::text("MCP 运行时", "MCP runtime")),
                        )
                        .child(
                            div()
                                .text_size(theme.text_body)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "MCP 控制位于运行态独立工作区，不属于本地设置项。",
                                    "MCP controls live in a separate runtime workspace, not in local settings.",
                                )),
                        )
                        .child(
                            Button::new("preferences-open-mcp-runtime")
                                .primary()
                                .label(crate::locale::text(
                                    "打开 MCP 运行时",
                                    "Open MCP runtime",
                                ))
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.request_mcp_runtime_navigation(cx);
                                })),
                        ),
                ),
            PreferencesNavigationDestination::Updates => div()
                .id("preferences-updates-search-result")
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(
                    div()
                        .w_full()
                        .max_w(px(560.))
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.panel)
                        .p_6()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .text_size(theme.text_title)
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(crate::locale::text("关于与更新", "About & updates")),
                        )
                        .child(
                            div()
                                .text_size(theme.text_body)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "版本信息与更新操作位于工作台的更新面板。",
                                    "Version information and update actions are available in the Updates workspace.",
                                )),
                        )
                        .child(
                            Button::new("preferences-open-updates")
                                .primary()
                                .label(crate::locale::text("打开更新面板", "Open Updates"))
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.request_updates_navigation(cx);
                                })),
                        ),
                ),
        }
    }

    /// 搜索无命中时清楚说明结果为空，避免继续展示与查询无关的旧设置页。
    ///
    /// @author kongweiguang
    fn render_no_search_results(&self, theme: ThemeTokens) -> impl IntoElement {
        div()
            .id("preferences-no-search-results")
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.background)
            .child(
                div()
                    .max_w(px(420.))
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .px_5()
                    .py_4()
                    .text_size(theme.text_body)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "没有匹配的设置，请尝试其他关键词。",
                        "No settings match this search. Try another keyword.",
                    )),
            )
    }

    /// 标题栏用模态关闭图标；Root 恢复打开前页面，离开仍执行同一草稿关闭门禁。
    ///
    /// @author kongweiguang
    fn render_header(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h(px(68.))
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme.panel)
            .child(
                div()
                    .flex_1()
                    .text_size(px(18.))
                    .font_weight(gpui_kit::FontWeight::BOLD)
                    .child(crate::locale::text("设置", "Settings")),
            )
            .child(
                Button::new("preferences-close")
                    .compact()
                    .ghost()
                    .icon(IconName::Close)
                    .accessibility_label(crate::locale::text("关闭设置", "Close settings"))
                    .tooltip(crate::locale::text("关闭设置", "Close settings"))
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.close_button(window, cx);
                    })),
            )
    }

    /// 默认菜单只列类别，搜索按实际字段过滤；固定窄栏的底部使用短提示，避免标点单独换行。
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
            .w(px(204.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme.sidebar)
            .child(Input::new(&self.navigation_search).w_full().h(px(36.)));
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
                        "没有匹配的设置",
                        "No matching settings",
                    ))
                    .into_any_element(),
            );
        }
        sidebar = sidebar.child(
            div()
                .id("preferences-navigation-results")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .children(nav_rows),
        );
        sidebar.child(
            div()
                .mt_3()
                .px_2()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "配置保存在本机",
                    "Settings stored locally",
                )),
        )
    }

    /// 导航项保持左对齐的扁平 ghost 外观；外部工作区目标经共享离开门禁处理。
    ///
    /// @author kongweiguang
    fn nav_button(
        &mut self,
        target: NavigationTarget,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = if self.navigation_search_query.trim().is_empty() {
            target.category && self.section == target.section
        } else {
            self.selected_navigation_target.as_deref() == Some(target.id.as_str())
        };
        let target_id = target.id.clone();
        let target_section = target.section;
        let scroll_index = target.scroll_index;
        let destination = target.destination;
        let label = target.label.clone();
        let text_color = if selected {
            theme.selected_foreground
        } else {
            theme.foreground
        };
        let icon_color = if selected { theme.accent } else { theme.muted };
        let button = Button::new(format!("preferences-nav-{}", target.id))
            .w_full()
            .h(px(44.))
            .compact()
            .ghost()
            .selected(selected)
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
                            .size(px(28.))
                            .flex_shrink_0()
                            .rounded(theme.radius_sm)
                            .bg(if selected {
                                theme.accent.opacity(0.12)
                            } else {
                                theme.panel
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(target.icon).size(px(18.)).text_color(icon_color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_left()
                            .text_size(theme.text_body)
                            .font_weight(if selected {
                                gpui_kit::FontWeight::MEDIUM
                            } else {
                                gpui_kit::FontWeight::NORMAL
                            })
                            .text_color(text_color)
                            .child(label),
                    ),
            )
            .on_click(
                cx.listener(move |panel, _: &ClickEvent, window, cx| match destination {
                    Some(PreferencesNavigationDestination::McpRuntime) => {
                        panel.request_mcp_runtime_navigation(cx);
                    }
                    Some(PreferencesNavigationDestination::Updates) => {
                        panel.request_updates_navigation(cx);
                    }
                    None => {
                        panel.navigate_to_target(
                            target_id.clone(),
                            target_section,
                            scroll_index,
                            window,
                            cx,
                        );
                    }
                }),
            );
        div().w_full().rounded(theme.radius_sm).child(button)
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
        self.section = section;
        self.selected_navigation_target = Some(id);
        if changed {
            cx.notify();
        }
        if section != PreferencesSection::Profiles
            && let Some(index) = scroll_index
        {
            let scroll = self.content_scroll.clone();
            window.on_next_frame(move |_, _| scroll.scroll_to_top_of_item(index));
        }
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

    /// 底栏在窄内容区允许状态和操作换行，并让提示准确表达显式保存语义。
    ///
    /// @author kongweiguang
    fn render_footer(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = match self.section {
            PreferencesSection::Profiles => self
                .profile_draft
                .as_ref()
                .is_some_and(|draft| draft.is_dirty()),
            PreferencesSection::ConfigurationSync | PreferencesSection::Vault => false,
            _ => self.settings.as_ref().is_some_and(|draft| draft.is_dirty()),
        };
        let saving = match self.section {
            PreferencesSection::Profiles => {
                self.profile_draft
                    .as_ref()
                    .is_some_and(|draft| draft.status() == super::DraftStatus::Saving)
                    || self.profile_operation_running
            }
            PreferencesSection::ConfigurationSync | PreferencesSection::Vault => {
                self.sync_busy.is_some()
                    || self.vault_busy
                    || self.vault_loading
                    || self.sync_loading
            }
            _ => self
                .settings
                .as_ref()
                .is_some_and(|draft| draft.status() == super::DraftStatus::Saving),
        };
        let can_save = dirty
            && !saving
            && self.recovery.mode != ConfigurationMode::ReadOnlyRecovery
            && (self.section == PreferencesSection::Profiles
                || !self.settings.as_ref().is_some_and(|draft| {
                    matches!(
                        draft.status(),
                        super::DraftStatus::ExternalConflict | super::DraftStatus::OutcomeUnknown
                    )
                }))
            && match self.section {
                PreferencesSection::Profiles => self.profile_input_errors.is_empty(),
                PreferencesSection::ConfigurationSync | PreferencesSection::Vault => false,
                _ => self.settings_input_errors.is_empty(),
            };
        let save_label = if saving {
            crate::locale::text("保存中…", "Saving…")
        } else {
            crate::locale::text("保存更改", "Save changes")
        };
        let settings_footer_error = (!matches!(
            self.section,
            PreferencesSection::Profiles
                | PreferencesSection::ConfigurationSync
                | PreferencesSection::Vault
        ))
        .then_some(self.settings_error.as_deref())
        .flatten();
        let footer_status = self.status.as_deref().or_else(|| {
            self.settings_success_status
                .map(|status| status.message(crate::locale::current_language()))
        });
        let (footer_message, footer_message_is_error) =
            settings_footer_message(settings_footer_error, footer_status)
                .map_or((None, false), |(message, is_error)| {
                    (Some(message.to_owned()), is_error)
                });
        let mut footer = div()
            .w_full()
            .min_h(px(52.))
            .px_4()
            .py_2()
            .flex()
            .flex_wrap()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .bg(theme.panel)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(12.))
                    .text_color(if footer_message_is_error {
                        theme.error
                    } else {
                        theme.muted
                    })
                    .child(footer_message.unwrap_or_else(|| {
                        if dirty {
                            crate::locale::text("有未保存修改", "Unsaved changes").to_string()
                        } else {
                            crate::locale::text(
                                "更改会在保存后生效。",
                                "Changes take effect after saving.",
                            )
                            .to_string()
                        }
                    })),
            );
        let blocking_settings_issues = self
            .settings_issues
            .iter()
            .filter(|issue| issue.blocks_save())
            .count();
        if blocking_settings_issues > 0 {
            footer = footer.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(theme.error)
                    .child(crate::locale::text(
                        format!("{} 项设置需要修正", blocking_settings_issues),
                        format!("{} settings need correction", blocking_settings_issues),
                    )),
            );
        }
        let settings_reload_needed = !matches!(
            self.section,
            PreferencesSection::Profiles
                | PreferencesSection::ConfigurationSync
                | PreferencesSection::Vault
        ) && (self.settings_error.is_some()
            || self.settings.as_ref().is_some_and(|draft| {
                matches!(
                    draft.status(),
                    super::DraftStatus::ExternalConflict | super::DraftStatus::OutcomeUnknown
                )
            }));
        if settings_reload_needed {
            footer = footer.child(
                Button::new("preferences-reload")
                    .compact()
                    .flex_shrink_0()
                    .label(if self.loading_settings {
                        crate::locale::text("重新读取中…", "Reloading…")
                    } else {
                        crate::locale::text("重新读取", "Reload")
                    })
                    .disabled(self.loading_settings || saving)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.request_reload_settings(cx)
                    })),
            );
        }
        // Profile 页面在所选记录表单内保存，footer 仅显示状态以免同一操作出现两个入口。
        if matches!(
            self.section,
            PreferencesSection::Profiles
                | PreferencesSection::ConfigurationSync
                | PreferencesSection::Vault
        ) {
            return footer;
        }
        footer.child(
            Button::new("preferences-save")
                .compact()
                .flex_shrink_0()
                .primary()
                .label(save_label)
                .disabled(!can_save)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                    panel.save_settings(cx);
                })),
        )
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

    /// settings 外部冲突会保留本地草稿，只有用户确认后才接受新的配置 revision。
    fn render_settings_reload_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        self.settings_reload_confirmation.then(|| {
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
                                "重新读取会放弃本地设置草稿",
                                "Reloading will discard local settings changes",
                            ),
                            "Reloading will discard local settings changes",
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("preferences-reload-cancel")
                                        .compact()
                                        .label(crate::locale::text("继续编辑", "Keep editing"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.cancel_reload_settings(cx)
                                        })),
                                )
                                .child(
                                    Button::new("preferences-reload-confirm")
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
                                            panel.confirm_reload_settings(cx)
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

/// 固定页脚优先显示 settings 读回错误，避免旧的成功或重载状态掩盖当前冲突。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn settings_footer_message<'a>(
    settings_error: Option<&'a str>,
    status: Option<&'a str>,
) -> Option<(&'a str, bool)> {
    settings_error
        .map(|message| (message, true))
        .or_else(|| status.map(|message| (message, false)))
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
