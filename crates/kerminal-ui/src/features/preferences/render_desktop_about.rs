//! GPUI 设置中的桌面通知、平台快捷键和关于页面。
//!
//! @author kongweiguang

use std::rc::Rc;

use gpui_kit::{
    ClickEvent, Context, Entity, FocusHandle, IntoElement, Keystroke, SharedString,
    assets::IconName,
    base::Selectable,
    component::{
        Disableable, Icon,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        input::InputState,
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{KeybindingPreference, KeybindingScope, SettingsSnapshot};

use crate::{
    features::updates::{AboutUpdateAction, about_identity, about_repository_url},
    theme::ThemeTokens,
};

use super::{
    KeyboardPlatform,
    panel::{PreferencesPanel, ShortcutBindingField, keyboard_platform},
    validate_settings,
    validation::{binding_for_platform, binding_is_customized},
};

/// main v0.3.48 的动作说明作为默认文案，实际范围始终来自配置。
#[derive(Clone, Copy)]
pub(super) struct MainShortcutPresentation {
    pub(super) action: &'static str,
    pub(super) label_zh: &'static str,
    pub(super) label_en: &'static str,
    pub(super) description_zh: &'static str,
    pub(super) description_en: &'static str,
}

/// 保留 main 的动作顺序与文案；显示层投影不覆盖配置中的用户绑定。
pub(super) const MAIN_SHORTCUT_PRESENTATIONS: [MainShortcutPresentation; 16] = [
    MainShortcutPresentation {
        action: "settings.open",
        label_zh: "打开设置",
        label_en: "Open settings",
        description_zh: "参考 IntelliJ IDEA 的设置入口打开设置。",
        description_en: "Open Settings using the IntelliJ IDEA shortcut.",
    },
    MainShortcutPresentation {
        action: "settings.keybindings",
        label_zh: "查看快捷键与动作",
        label_en: "View shortcuts and actions",
        description_zh: "打开快捷键与动作列表。",
        description_en: "Open the shortcut and action list directly.",
    },
    MainShortcutPresentation {
        action: "terminal.focus",
        label_zh: "回到终端",
        label_en: "Return to terminal",
        description_zh: "显示终端工作区并收起右栏。",
        description_en: "Show the terminal workspace and collapse the right pane.",
    },
    MainShortcutPresentation {
        action: "terminal.newTab",
        label_zh: "新建本地终端",
        label_en: "New local terminal",
        description_zh: "新建本地终端 tab。",
        description_en: "Open a new local terminal workspace tab.",
    },
    MainShortcutPresentation {
        action: "terminal.closeTab",
        label_zh: "关闭当前终端 tab",
        label_en: "Close current terminal tab",
        description_zh: "关闭当前终端 tab。",
        description_en: "Close the current terminal workspace tab.",
    },
    MainShortcutPresentation {
        action: "terminal.closePane",
        label_zh: "关闭当前分屏",
        label_en: "Close current pane",
        description_zh: "关闭当前分屏，多分屏时生效。",
        description_en: "Close the current split when multiple panes are open.",
    },
    MainShortcutPresentation {
        action: "terminal.splitHorizontal",
        label_zh: "左右分屏",
        label_en: "Split right",
        description_zh: "在当前终端右侧创建分屏。",
        description_en: "Create a pane to the right of the current terminal.",
    },
    MainShortcutPresentation {
        action: "terminal.splitVertical",
        label_zh: "上下分屏",
        label_en: "Split below",
        description_zh: "在当前终端下方创建分屏。",
        description_en: "Create a pane below the current terminal.",
    },
    MainShortcutPresentation {
        action: "terminal.previousTab",
        label_zh: "上一个终端 tab",
        label_en: "Previous terminal tab",
        description_zh: "切到左侧终端 tab。",
        description_en: "Switch to the terminal tab immediately to the left.",
    },
    MainShortcutPresentation {
        action: "terminal.nextTab",
        label_zh: "下一个终端 tab",
        label_en: "Next terminal tab",
        description_zh: "切到右侧终端 tab。",
        description_en: "Switch to the terminal tab immediately to the right.",
    },
    MainShortcutPresentation {
        action: "tool.agentLauncher",
        label_zh: "打开 Agent Launcher",
        label_en: "Open Agent Launcher",
        description_zh: "打开 Agent Launcher。",
        description_en: "Open the Agent Launcher tool window.",
    },
    MainShortcutPresentation {
        action: "tool.system",
        label_zh: "打开系统信息",
        label_en: "Open system information",
        description_zh: "打开系统信息。",
        description_en: "Open the system information tool window.",
    },
    MainShortcutPresentation {
        action: "tool.sftp",
        label_zh: "打开 SFTP",
        label_en: "Open SFTP",
        description_zh: "打开 SFTP。",
        description_en: "Open the SFTP tool window.",
    },
    MainShortcutPresentation {
        action: "tool.ports",
        label_zh: "打开端口转发",
        label_en: "Open port forwarding",
        description_zh: "打开端口转发。",
        description_en: "Open the port forwarding tool window.",
    },
    MainShortcutPresentation {
        action: "tool.snippets",
        label_zh: "打开脚本片段",
        label_en: "Open snippets",
        description_zh: "打开脚本片段。",
        description_en: "Open the snippets tool window.",
    },
    MainShortcutPresentation {
        action: "tool.logs",
        label_zh: "打开命令历史",
        label_en: "Open command history",
        description_zh: "打开当前终端的命令历史。",
        description_en: "Open the current terminal's command history.",
    },
];

/// GPUI 专属动作提供本地化文案，同时保持 main 动作在各组内的既有排序。
const GPUI_SHORTCUT_PRESENTATIONS: [MainShortcutPresentation; 5] = [
    MainShortcutPresentation {
        action: "terminal.search",
        label_zh: "搜索终端",
        label_en: "Search terminal",
        description_zh: "搜索当前终端的内容。",
        description_en: "Search the current terminal.",
    },
    MainShortcutPresentation {
        action: "terminal.paste",
        label_zh: "粘贴到终端",
        label_en: "Paste into terminal",
        description_zh: "将剪贴板内容粘贴到当前终端。",
        description_en: "Paste clipboard text into the current terminal.",
    },
    MainShortcutPresentation {
        action: "workspace.quickOpen",
        label_zh: "快速打开",
        label_en: "Quick open",
        description_zh: "打开快速选择。",
        description_en: "Open quick navigation.",
    },
    MainShortcutPresentation {
        action: "workspace.commandPalette",
        label_zh: "命令面板",
        label_en: "Command palette",
        description_zh: "打开命令面板。",
        description_en: "Open the command palette.",
    },
    MainShortcutPresentation {
        action: "application.quit",
        label_zh: "退出 Kerminal",
        label_en: "Quit Kerminal",
        description_zh: "退出应用。",
        description_en: "Quit the application.",
    },
];

/// 返回 main 默认列表中的动作位置；未知自定义动作由稳定排序放在其后。
pub(super) fn main_shortcut_order(action: &str) -> Option<usize> {
    MAIN_SHORTCUT_PRESENTATIONS
        .iter()
        .position(|presentation| presentation.action == action)
}

/// 保留未知配置并展示所有当前可执行动作，避免运行中的快捷键无法在设置中修改。
pub(super) fn main_settings_shortcut_visible(_action: &str) -> bool {
    true
}

/// 返回 main 对应动作的静态文案投影，用户自定义 action 继续使用其配置字段。
pub(super) fn main_shortcut_presentation(
    action: &str,
) -> Option<&'static MainShortcutPresentation> {
    MAIN_SHORTCUT_PRESENTATIONS
        .iter()
        .chain(GPUI_SHORTCUT_PRESENTATIONS.iter())
        .find(|presentation| presentation.action == action)
}

/// 展示配置中的真实作用域，避免界面分组与运行态编译出的作用域不一致。
/// @author kongweiguang
pub(super) fn shortcut_display_scope(
    _action: &str,
    configured_scope: KeybindingScope,
) -> KeybindingScope {
    configured_scope
}

/// 将通知开关的稳定身份与当前设置投影放在同一组，避免行构造器依赖多组位置参数。
///
/// @author kongweiguang
struct PolicyToggleConfig {
    id: SharedString,
    icon: IconName,
    label: SharedString,
    checked: bool,
    disabled: bool,
}

/// 行与 Switch 共用受控回调；只用开关表达选中，避免设置列表被大面积强调色干扰。
/// Switch 停止冒泡以免一次点击重复提交，禁用时不注册行点击。
///
/// @author kongweiguang
fn policy_toggle(
    config: PolicyToggleConfig,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
    on_change: impl Fn(&mut PreferencesPanel, bool, &mut Context<PreferencesPanel>) + 'static,
) -> impl IntoElement {
    let PolicyToggleConfig {
        id,
        icon,
        label,
        checked,
        disabled,
    } = config;
    let on_change = Rc::new(on_change);
    let switch_change = Rc::clone(&on_change);
    let row_change = Rc::clone(&on_change);
    let switch_accessible_label = label.clone();
    let row_id = format!("desktop-policy-row-{id}");
    let mut row = div()
        .id(row_id)
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap(theme.metrics.gap)
        .px(theme.metrics.gap)
        .py_2()
        .min_h(theme.metrics.toolbar_height)
        .rounded(theme.control_radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            Icon::new(icon)
                .size(theme.metrics.icon_size)
                .text_color(theme.muted),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_sm)
                .child(label),
        )
        .child(
            Switch::new(id)
                .track_size(theme.metrics.icon_size * 2.5, theme.metrics.icon_size * 1.5)
                .checked(checked)
                .disabled(disabled)
                .accessibility_label(switch_accessible_label)
                .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                    cx.stop_propagation();
                    switch_change(panel, *enabled, cx);
                })),
        );
    if !disabled {
        row = row.cursor_pointer().on_click(
            cx.listener(move |panel, _: &ClickEvent, _, cx| row_change(panel, !checked, cx)),
        );
    }
    row
}

impl PreferencesPanel {
    /// 搜索命中高级通知字段时展开对应折叠区，导航不会丢失其余草稿。
    pub(super) fn reveal_desktop_advanced(&mut self, cx: &mut Context<Self>) {
        if !self.desktop_about.advanced_expanded {
            self.desktop_about.advanced_expanded = true;
            cx.notify();
        }
    }

    /// 秒输入仍统一换算为毫秒，折叠摘要按 main 规则四舍五入显示整秒；滚动根保持透明，避免覆盖弹框底色。
    pub(super) fn render_desktop_settings(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme);
        };
        let notifications = settings.desktop_notifications;
        let advanced_expanded = self.desktop_about.advanced_expanded;
        let controls_locked = self.settings_controls_locked();
        let summary_seconds = notifications.min_duration_ms.saturating_add(500) / 1000;

        let notification_card = div()
            .id("settings-desktop-notifications")
            .w_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .rounded(theme.radius_panel)
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(card_heading(
                IconName::BellRing,
                crate::locale::text("桌面通知", "Desktop notifications"),
                theme,
            ))
            .child(policy_toggle(
                PolicyToggleConfig {
                    id: "desktop-notifications-enabled".into(),
                    icon: IconName::Bell,
                    label: crate::locale::text("启用桌面通知", "Enable desktop notifications"),
                    checked: notifications.enabled,
                    disabled: controls_locked,
                },
                theme,
                cx,
                |panel, value, cx| {
                    panel.edit_settings(
                        |settings| settings.desktop_notifications.enabled = value,
                        cx,
                    )
                },
            ))
            .child(policy_toggle(
                PolicyToggleConfig {
                    id: "desktop-notifications-background-only".into(),
                    icon: IconName::Clock,
                    label: crate::locale::text("仅在后台通知", "Notify only in the background"),
                    checked: notifications.background_only,
                    disabled: controls_locked,
                },
                theme,
                cx,
                |panel, value, cx| {
                    panel.edit_settings(
                        |settings| settings.desktop_notifications.background_only = value,
                        cx,
                    )
                },
            ))
            .child(policy_toggle(
                PolicyToggleConfig {
                    id: "desktop-notifications-important-only".into(),
                    icon: IconName::ShieldAlert,
                    label: crate::locale::text("只通知重要事件", "Notify important events only"),
                    checked: notifications.important_only,
                    disabled: controls_locked,
                },
                theme,
                cx,
                |panel, value, cx| {
                    panel.edit_settings(
                        |settings| settings.desktop_notifications.important_only = value,
                        cx,
                    )
                },
            ));

        // Keep the disclosure as one semantic button, but own its content layout so the title stays left and the summary stays right.
        let advanced_button = Button::new("desktop-notifications-advanced-toggle")
            .w_full()
            .min_h(px(44.))
            .px_0()
            .py_0()
            .rounded(theme.radius_sm)
            .accessibility_label(crate::locale::text(
                "高级通知设置",
                "Advanced notification settings",
            ))
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(theme.panel)
                    .foreground(theme.foreground)
                    .hover(theme.hover)
                    .active(theme.panel),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .child(
                        Icon::new(if advanced_expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .text_color(theme.muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(theme.text_sm)
                            .text_color(theme.foreground)
                            .child(crate::locale::text(
                                "高级通知设置",
                                "Advanced notification settings",
                            )),
                    )
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                format!("{summary_seconds} 秒"),
                                format!("{summary_seconds} sec"),
                            )),
                    ),
            )
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                panel.desktop_about.advanced_expanded = !panel.desktop_about.advanced_expanded;
                cx.notify();
            }));
        let mut advanced = div()
            .id("desktop-notifications-advanced")
            .w_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .rounded(theme.radius_sm)
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(advanced_button);
        if advanced_expanded {
            advanced = advanced.child(
                div()
                    .w_full()
                    .border_t_1()
                    .border_color(theme.border)
                    .p_3()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_3()
                            .child(notification_number_field(
                                "desktop-duration-threshold",
                                crate::locale::text("耗时阈值", "Duration threshold"),
                                crate::locale::text(
                                    "前台任务低于该时长不发通知。",
                                    "Foreground tasks below this duration do not notify.",
                                ),
                                &self.settings_inputs.notification_min_duration,
                                crate::locale::text("1–120", "1–120"),
                                controls_locked,
                                theme,
                            ))
                            .child(notification_number_field(
                                "desktop-event-throttle",
                                crate::locale::text("同类事件节流", "Same-event throttling"),
                                crate::locale::text(
                                    "同类事件在该时间内只发一次。",
                                    "Send each event type at most once during this interval.",
                                ),
                                &self.settings_inputs.notification_throttle,
                                crate::locale::text("0–600", "0–600"),
                                controls_locked,
                                theme,
                            )),
                    ),
            );
        }

        div()
            .id("settings-desktop-page")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(notification_card)
            .child(advanced)
            .child(self.render_save_notice(theme))
    }

    /// 关于页呈现真实版本、许可、仓库和更新事务状态；刷新使用原按钮忙态，避免内容位移。
    pub(super) fn render_about_settings(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (version, license) = about_identity();
        let update = self
            .inline_updates
            .as_ref()
            .map(|panel| panel.read(cx).settings_projection());
        let github_url = about_repository_url();
        let github_label = github_url.map(display_repository_url).unwrap_or_else(|| {
            crate::locale::text("仓库地址不可用", "Repository URL unavailable").to_string()
        });
        let github = Button::new("settings-about-github")
            .compact()
            .outline()
            .disabled(github_url.is_none())
            .label(crate::locale::text("打开", "Open"))
            .child(Icon::new(IconName::ExternalLink).size(px(14.)))
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                if let Some(url) = github_url {
                    cx.open_url(url);
                }
            }));

        let mut actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .max_w_full()
            .flex_shrink_0();
        if let Some(state) = &update {
            actions = actions.child(
                Button::new("settings-about-check-updates")
                    .compact()
                    .outline()
                    .disabled(!state.can_check)
                    .loading(state.checking)
                    .label(crate::locale::text("检查", "Check"))
                    .icon(IconName::RefreshCw)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.run_inline_update_action(AboutUpdateAction::Check, cx)
                    })),
            );
            if state.can_install {
                actions = actions.child(
                    Button::new("settings-about-install-update")
                        .compact()
                        .primary()
                        .label(crate::locale::text("安装", "Install"))
                        .icon(IconName::Download)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.run_inline_update_action(AboutUpdateAction::Install, cx)
                        })),
                );
            }
        }

        let update_status = update.as_ref().map_or_else(
            || crate::locale::text("更新服务不可用。", "Update service is unavailable."),
            |state| state.detail.clone().into(),
        );
        let update_label = update.as_ref().map_or_else(
            || crate::locale::text("不可用", "Unavailable"),
            |state| state.status.clone().into(),
        );
        let update_tone = if update.as_ref().is_some_and(|state| state.failed) {
            theme.error
        } else if update
            .as_ref()
            .is_some_and(|state| state.restarting || state.available)
        {
            theme.accent
        } else if update.as_ref().is_some_and(|state| state.up_to_date) {
            theme.success
        } else {
            theme.muted
        };
        let about_card = div()
            .id("settings-about-card")
            .w_full()
            .p(theme.metrics.panel_padding)
            .flex()
            .flex_col()
            .gap_4()
            .rounded(theme.radius_panel)
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(card_heading(
                IconName::Info,
                crate::locale::text("关于 Kerminal", "About Kerminal"),
                theme,
            ))
            .child(
                div()
                    .id("settings-about-list")
                    .w_full()
                    .child(about_row(
                        IconName::Hash,
                        crate::locale::text("版本", "Version"),
                        version,
                        theme,
                    ))
                    .child(about_row(
                        IconName::Scale,
                        crate::locale::text("协议", "License"),
                        license,
                        theme,
                    ))
                    .child(about_action_row(
                        IconName::GitBranch,
                        crate::locale::text("GitHub", "GitHub"),
                        github_label,
                        github,
                        theme,
                    ))
                    .child(about_status_row(
                        IconName::RotateCcw,
                        crate::locale::text("更新", "Updates"),
                        update_label,
                        update_status,
                        update_tone,
                        actions,
                        theme,
                    )),
            );

        div()
            .id("settings-about-page")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(about_card)
            .child(self.render_save_notice(theme))
    }

    /// 三平台分段选择只改变当前投影，保存时仅修改所选平台字段。
    /// @author kongweiguang
    pub(super) fn render_keybindings_settings(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return self.render_settings_loading(theme);
        };
        let platform = self.shortcut_platform;
        let disabled = self.settings_controls_locked();
        let platform_issues = if platform == keyboard_platform() {
            self.settings_issues.clone()
        } else {
            validate_settings(&settings, platform)
        };
        let defaults = SettingsSnapshot::default().keybindings;
        let groups = [
            KeybindingScope::Global,
            KeybindingScope::Terminal,
            KeybindingScope::Workspace,
        ];
        let visible_bindings = settings
            .keybindings
            .iter()
            .enumerate()
            .filter(|(_, binding)| main_settings_shortcut_visible(&binding.action))
            .collect::<Vec<_>>();
        let mut body = div()
            .id("settings-shortcuts-page")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .rounded(theme.radius_panel)
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel);

        let platform_picker = div()
            .id("settings-shortcut-header")
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w(px(160.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(theme.text_sm)
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(Icon::new(IconName::Keyboard).text_color(theme.accent))
                    .child(crate::locale::text("快捷键", "Keyboard shortcuts")),
            )
            .child(
                div()
                    .id("settings-shortcut-platform")
                    .flex()
                    .items_center()
                    .gap_1()
                    .p(px(2.))
                    .rounded(theme.control_radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .flex_shrink_0()
                    .child(platform_button(
                        KeyboardPlatform::Windows,
                        platform,
                        disabled,
                        theme,
                        cx,
                    ))
                    .child(platform_button(
                        KeyboardPlatform::Linux,
                        platform,
                        disabled,
                        theme,
                        cx,
                    ))
                    .child(platform_button(
                        KeyboardPlatform::Mac,
                        platform,
                        disabled,
                        theme,
                        cx,
                    )),
            );
        body = body.child(platform_picker);

        for scope in groups {
            let mut bindings = visible_bindings
                .iter()
                .filter(|(_, binding)| {
                    shortcut_display_scope(&binding.action, binding.scope) == scope
                })
                .copied()
                .collect::<Vec<_>>();
            bindings.sort_by_key(|(source_index, binding)| {
                (
                    main_shortcut_order(&binding.action).unwrap_or(usize::MAX),
                    *source_index,
                )
            });
            if bindings.is_empty() {
                continue;
            }
            let mut group = div()
                .id(format!("settings-shortcut-group-{}", scope_key(scope)))
                .w_full()
                .border_t_1()
                .border_color(theme.border)
                .pt_3()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .px_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(160.))
                                .text_size(theme.text_xs)
                                .font_weight(theme.settings_medium_font_weight())
                                .child(scope_heading(scope)),
                        )
                        .child(count_badge(bindings.len(), theme)),
                );

            let mut rows = div()
                .w_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded(theme.radius_sm)
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel);
            for (row_index, (_, binding)) in bindings.iter().enumerate() {
                let binding = *binding;
                let capture = self
                    .shortcut_capture
                    .as_ref()
                    .filter(|capture| capture.action == binding.action)
                    .map(|capture| ShortcutCaptureProjection {
                        field: capture.field,
                        focus: capture.focus.clone(),
                        strokes: capture.strokes.clone(),
                        recording: capture.recording,
                        error: capture.error.clone(),
                    });
                let field = ShortcutBindingField::from_editor_platform(platform);
                let default_binding = defaults
                    .iter()
                    .find(|candidate| candidate.action == binding.action);
                let customized = default_binding
                    .is_some_and(|default| binding_is_customized(binding, default, platform));
                let mut issues = platform_issues
                    .iter()
                    .filter(|issue| issue.actions.iter().any(|action| action == &binding.action))
                    .map(|issue| issue.message.clone())
                    .collect::<Vec<_>>();
                if let Some(message) =
                    self.shortcut_field_errors
                        .get(&format!("{}:{}", binding.action, field.key()))
                {
                    issues.push(message.clone());
                }
                let (label, description, display_scope) =
                    main_shortcut_presentation(&binding.action)
                        .map(|presentation| {
                            (
                                crate::locale::text(presentation.label_zh, presentation.label_en),
                                if binding.description.is_empty() {
                                    crate::locale::text(
                                        presentation.description_zh,
                                        presentation.description_en,
                                    )
                                } else {
                                    binding.description.clone().into()
                                },
                                shortcut_display_scope(&binding.action, binding.scope),
                            )
                        })
                        .unwrap_or_else(|| {
                            (
                                binding.label.clone().into(),
                                binding.description.clone().into(),
                                binding.scope,
                            )
                        });
                rows = rows.child(shortcut_row(
                    ShortcutRowConfig {
                        binding,
                        label,
                        description,
                        scope: display_scope,
                        capture,
                        platform,
                        customized,
                        disabled,
                        last_in_group: row_index + 1 == bindings.len(),
                        theme,
                        issues,
                    },
                    cx,
                ));
            }
            group = group.child(rows);
            body = body.child(group);
        }

        body.child(
            div()
                .w_full()
                .border_t_1()
                .border_color(theme.border)
                .px_1()
                .pt_3()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "选择平台后点击铅笔录制；恢复默认只影响当前平台。",
                    "Pick a platform, then use the pencil to record. Restore affects only that platform.",
                )),
        )
        .child(self.render_save_notice(theme))
    }
}

/// 卡片标题图标与文本在主题变化时共用语义颜色。
fn card_heading(
    icon: IconName,
    title: impl Into<SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(theme.text_sm)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(Icon::new(icon).text_color(theme.accent))
        .child(title.into())
}

/// 提供秒单位提示，但输入实体沿用设置表单统一的解析和错误保留语义。
/// @author kongweiguang
fn notification_number_field(
    id: &'static str,
    label: impl Into<SharedString>,
    help: impl Into<SharedString>,
    input: &Entity<InputState>,
    range: impl Into<SharedString>,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_1()
        .min_w(px(220.))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .font_weight(theme.settings_medium_font_weight())
                .child(label.into()),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(help.into()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    super::render_settings::settings_input(input, theme)
                        .disabled(disabled)
                        .w(px(120.)),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("秒", "sec")),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(range.into()),
                ),
        )
}

/// 用共享图标和间距展示元数据，行与标题对齐，不叠加第二层容器。
fn about_row(
    icon: IconName,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(theme.metrics.gap)
        .border_b_1()
        .border_color(theme.border)
        .py(theme.metrics.gap)
        .child(
            Icon::new(icon)
                .size(theme.metrics.icon_size)
                .mt(px(2.))
                .text_color(theme.muted),
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
                        .text_size(theme.text_sm)
                        .font_weight(theme.settings_medium_font_weight())
                        .child(label.into()),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(value.into()),
                ),
        )
}

/// 外链与其对象保持邻近，并允许长 URL 在窄窗口中收缩。
fn about_action_row(
    icon: IconName,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    action: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_start()
        .gap(theme.metrics.gap)
        .border_b_1()
        .border_color(theme.border)
        .py(theme.metrics.gap)
        .child(
            Icon::new(icon)
                .size(theme.metrics.icon_size)
                .mt(px(2.))
                .text_color(theme.muted),
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
                        .text_size(theme.text_sm)
                        .font_weight(theme.settings_medium_font_weight())
                        .child(label.into()),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(value.into()),
                ),
        )
        .child(action)
}

/// 更新状态与操作可换行，避免文字放大时固定最小宽度挤出视口。
fn about_status_row(
    icon: IconName,
    label: impl Into<SharedString>,
    status: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    tone: gpui_kit::Hsla,
    actions: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(theme.metrics.gap)
        .py(theme.metrics.gap)
        .child(
            Icon::new(icon)
                .size(theme.metrics.icon_size)
                .mt(px(2.))
                .text_color(theme.muted),
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
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(theme.text_sm)
                                .font_weight(theme.settings_medium_font_weight())
                                .child(label.into()),
                        )
                        .child(
                            div()
                                .px_2()
                                .py(px(2.))
                                .rounded_full()
                                .border_1()
                                .border_color(tone.opacity(0.25))
                                .bg(tone.opacity(0.1))
                                .text_size(theme.text_xs)
                                .text_color(tone)
                                .child(status.into()),
                        ),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(detail.into()),
                ),
        )
        .child(actions)
}

/// 平台按钮沿用选中令牌，点击只更新局部投影状态而不写配置。
/// @author kongweiguang
fn platform_button(
    platform: KeyboardPlatform,
    selected: KeyboardPlatform,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    let mut button = Button::new(format!(
        "settings-shortcut-platform-{}",
        platform_key(platform)
    ))
    .compact()
    .label(platform_label(platform))
    .selected(platform == selected)
    .disabled(disabled);
    if platform == selected {
        button = button.custom(
            ButtonCustomVariant::new(cx)
                // Custom 按钮会把基础色再混入 20% 透明；先补偿，静止态才与其它分段控件一致。
                .color(super::render_settings::settings_choice_variant_fill(
                    theme.surface_selected(),
                ))
                .foreground(theme.settings_choice_foreground(true))
                .hover(theme.surface_selected())
                .active(theme.surface_selected()),
        );
    } else {
        button = button
            .ghost()
            .custom(super::render_settings::settings_ghost_variant(theme, cx));
    }
    button.on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
        panel.cancel_shortcut_capture(cx);
        panel.shortcut_platform = platform;
        cx.notify();
    }))
}

/// 将录制实体投影为只读值，避免渲染期间重新读取正在更新的面板实体。
///
/// @author kongweiguang
struct ShortcutCaptureProjection {
    field: ShortcutBindingField,
    focus: FocusHandle,
    strokes: Vec<String>,
    recording: bool,
    error: Option<String>,
}

/// 三个平台共用同一条录制控件，不再为其他平台退回文本输入。
///
/// @author kongweiguang
struct ShortcutControlConfig<'a> {
    action: &'a str,
    field: ShortcutBindingField,
    value: String,
    editable: bool,
    capture: Option<&'a ShortcutCaptureProjection>,
    theme: ThemeTokens,
}

/// 录制值在确认前只存在于局部草稿；显示值始终来自所选平台的有效绑定。
///
/// @author kongweiguang
fn shortcut_control(
    config: ShortcutControlConfig<'_>,
    cx: &mut Context<PreferencesPanel>,
) -> gpui_kit::AnyElement {
    let ShortcutControlConfig {
        action,
        field,
        value,
        editable,
        capture,
        theme,
    } = config;
    let suffix = format!("{action}-{}", field.key());
    let platform = field.editor_platform();
    if let Some(capture) = capture {
        let recorded = capture
            .strokes
            .iter()
            .map(|stroke| display_shortcut(stroke, platform))
            .collect::<Vec<_>>()
            .join(" ");
        let preview = if capture.recording {
            if recorded.is_empty() {
                crate::locale::text("按下组合键", "Press a shortcut").to_string()
            } else {
                format!("{recorded} …")
            }
        } else {
            recorded
        };
        let mut control = div()
            .id(format!("shortcut-capture-{suffix}"))
            .track_focus(&capture.focus)
            .aria_label(crate::locale::text("快捷键录制", "Shortcut recording"))
            .w_full()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_1()
            .rounded(theme.radius_sm)
            .border_1()
            .border_color(theme.accent)
            .px_2()
            .py_1()
            .child(
                div()
                    .min_w(px(0.))
                    .text_size(theme.text_xs)
                    .font_family("monospace")
                    .child(preview),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new(format!("shortcut-add-stroke-{suffix}"))
                            .compact()
                            .ghost()
                            .icon(IconName::Plus)
                            .tooltip(crate::locale::text("添加下一段", "Add next stroke"))
                            .accessibility_label(crate::locale::text(
                                "添加下一段",
                                "Add next stroke",
                            ))
                            .disabled(capture.recording || capture.strokes.is_empty())
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.add_shortcut_stroke(window, cx)
                            })),
                    )
                    .child(
                        Button::new(format!("shortcut-confirm-{suffix}"))
                            .compact()
                            .ghost()
                            .icon(IconName::Check)
                            .tooltip(crate::locale::text("确认快捷键", "Confirm shortcut"))
                            .accessibility_label(crate::locale::text(
                                "确认快捷键",
                                "Confirm shortcut",
                            ))
                            .disabled(capture.recording || capture.strokes.is_empty())
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.confirm_shortcut_capture(window, cx)
                            })),
                    )
                    .child(
                        Button::new(format!("shortcut-cancel-{suffix}"))
                            .compact()
                            .ghost()
                            .icon(IconName::X)
                            .tooltip(crate::locale::text("取消录制", "Cancel recording"))
                            .accessibility_label(crate::locale::text(
                                "取消录制",
                                "Cancel recording",
                            ))
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.finish_shortcut_capture(window, cx)
                            })),
                    ),
            );
        if let Some(error) = &capture.error {
            control = control.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(error.clone()),
            );
        }
        return control.into_any_element();
    }

    let action_for_capture = action.to_owned();
    let unassigned = value.is_empty();
    let display_value = if unassigned {
        crate::locale::text("未设置", "Unassigned").to_string()
    } else {
        display_shortcut(&value, platform)
    };
    div()
        .id(format!("shortcut-value-{suffix}"))
        .w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .px_2()
                .py_1()
                .rounded(theme.radius_sm)
                .border_1()
                .border_color(theme.border)
                .text_size(theme.text_xs)
                .text_color(if unassigned {
                    theme.muted
                } else {
                    theme.foreground
                })
                .font_family("monospace")
                .child(display_value),
        )
        .child(
            Button::new(format!("shortcut-record-{suffix}"))
                .compact()
                .ghost()
                .icon(IconName::Pencil)
                .tooltip(crate::locale::text("录制快捷键", "Record shortcut"))
                .accessibility_label(crate::locale::text("录制快捷键", "Record shortcut"))
                .disabled(!editable)
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.start_shortcut_capture(action_for_capture.clone(), field, window, cx);
                })),
        )
        .into_any_element()
}

/// 展示层统一为 `Ctrl+Shift+F12`；存储仍保留 GPUI 规范串，避免显示格式反写配置。
///
/// @author kongweiguang
pub(super) fn display_shortcut(binding: &str, platform: KeyboardPlatform) -> String {
    binding
        .split_whitespace()
        .map(|stroke| display_shortcut_stroke(stroke, platform))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 单个按键按目标平台显示修饰键名称，解析失败时保留原始输入。
fn display_shortcut_stroke(stroke: &str, platform: KeyboardPlatform) -> String {
    let normalized = crate::keybindings::normalize_legacy_binding(stroke);
    let Ok(keystroke) = Keystroke::parse(&normalized) else {
        return stroke.to_owned();
    };
    let modifiers = keystroke.modifiers;
    let mut parts = Vec::with_capacity(5);
    if modifiers.function {
        parts.push("Fn".to_owned());
    }
    if modifiers.control {
        parts.push("Ctrl".to_owned());
    }
    if modifiers.alt {
        parts.push(
            if platform == KeyboardPlatform::Mac {
                "Option"
            } else {
                "Alt"
            }
            .to_owned(),
        );
    }
    if modifiers.platform {
        parts.push(
            match platform {
                KeyboardPlatform::Mac => "Cmd",
                KeyboardPlatform::Windows => "Win",
                KeyboardPlatform::Linux => "Super",
            }
            .to_owned(),
        );
    }
    if modifiers.shift {
        parts.push("Shift".to_owned());
    }
    parts.push(display_shortcut_key(&keystroke.key));
    parts.join("+")
}

/// 单字母和功能键统一大写，导航与编辑键使用可读名称。
fn display_shortcut_key(key: &str) -> String {
    let key = key.to_ascii_lowercase();
    let named = match key.as_str() {
        "left" => Some("Left"),
        "right" => Some("Right"),
        "up" => Some("Up"),
        "down" => Some("Down"),
        "pageup" => Some("PageUp"),
        "pagedown" => Some("PageDown"),
        "home" => Some("Home"),
        "end" => Some("End"),
        "tab" => Some("Tab"),
        "enter" => Some("Enter"),
        "escape" | "esc" => Some("Esc"),
        "space" => Some("Space"),
        "backspace" => Some("Backspace"),
        "delete" => Some("Delete"),
        "insert" => Some("Insert"),
        _ => None,
    };
    if let Some(name) = named {
        return name.to_owned();
    }
    if key.len() == 1 {
        return key.to_ascii_uppercase();
    }
    if key
        .strip_prefix('f')
        .is_some_and(|digits| digits.chars().all(|digit| digit.is_ascii_digit()))
        && key.len() > 1
    {
        return key.to_ascii_uppercase();
    }
    key
}

/// 一条快捷键保留 main 文案投影、作用域和行分隔，并继续驱动真实绑定。
///
/// @author kongweiguang
struct ShortcutRowConfig<'a> {
    binding: &'a KeybindingPreference,
    label: SharedString,
    description: SharedString,
    scope: KeybindingScope,
    capture: Option<ShortcutCaptureProjection>,
    platform: KeyboardPlatform,
    customized: bool,
    disabled: bool,
    last_in_group: bool,
    theme: ThemeTokens,
    issues: Vec<String>,
}

/// 行配置只组合显示上下文；绑定值的唯一来源仍是设置草稿。
///
/// @author kongweiguang
fn shortcut_row(
    config: ShortcutRowConfig<'_>,
    cx: &mut Context<PreferencesPanel>,
) -> impl IntoElement {
    let ShortcutRowConfig {
        binding,
        label,
        description,
        scope,
        capture,
        platform,
        customized,
        disabled,
        last_in_group,
        theme,
        issues,
    } = config;
    let action = binding.action.clone();
    let editable = binding.editable
        && !disabled
        && crate::keybindings::KeybindingAction::from_id(&binding.action).is_some();
    let reset = Button::new(format!("shortcut-reset-{}", binding.action))
        .compact()
        .ghost()
        .icon(IconName::RotateCcw)
        .disabled(!editable || !customized)
        .tooltip(crate::locale::text("恢复默认", "Restore default"))
        .accessibility_label(crate::locale::text("恢复默认", "Restore default"))
        .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
            panel.finish_shortcut_capture(window, cx);
            panel.reset_shortcut(&action, platform, cx);
        }));
    let field = ShortcutBindingField::from_editor_platform(platform);
    let binding_control = shortcut_control(
        ShortcutControlConfig {
            action: &binding.action,
            field,
            value: binding_for_platform(binding, platform),
            editable,
            capture: capture.as_ref().filter(|capture| capture.field == field),
            theme,
        },
        cx,
    );
    let mut row = div()
        .id(format!("shortcut-row-{}", binding.action))
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_wrap()
        .items_start()
        .gap_3()
        .px_3()
        .py_3()
        .when(!last_in_group, |row| {
            row.border_b_1().border_color(theme.border)
        })
        .child(
            div()
                .flex_1()
                .min_w(px(240.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .font_weight(theme.settings_medium_font_weight())
                        .child(label),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(description),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(shortcut_metadata_badge(scope_label(scope), false, theme))
                        .child(shortcut_metadata_badge(binding.action.clone(), true, theme)),
                )
                .children(issues.into_iter().map(|message| {
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.error)
                        .child(message)
                })),
        )
        .child(
            div()
                .w(px(205.))
                .max_w(px(320.))
                .min_w(px(160.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .font_weight(theme.settings_medium_font_weight())
                        .text_color(theme.muted)
                        .child(platform_label(platform)),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w(px(0.)).child(binding_control))
                        .child(reset),
                ),
        );
    if !binding.editable {
        row = row.text_color(theme.muted);
    }
    row
}

/// 页面保留完整 HTTPS 地址，仅移除供 Git clone 使用的后缀。
///
/// @author kongweiguang
pub(super) fn display_repository_url(url: &str) -> String {
    url.strip_suffix(".git").unwrap_or(url).to_owned()
}

/// 标签和 action ID 保持稳定键，不依赖本地化字符串作为持久化身份。
fn scope_label(scope: KeybindingScope) -> SharedString {
    crate::locale::text(
        match scope {
            KeybindingScope::Global => "全局",
            KeybindingScope::Terminal => "终端",
            KeybindingScope::Workspace => "工作区",
        },
        match scope {
            KeybindingScope::Global => "Global",
            KeybindingScope::Terminal => "Terminal",
            KeybindingScope::Workspace => "Workspace",
        },
    )
}

/// Section title remains separate from its count badge as in main's grouped list.
fn scope_heading(scope: KeybindingScope) -> SharedString {
    crate::locale::text(
        match scope {
            KeybindingScope::Global => "全局快捷键",
            KeybindingScope::Terminal => "终端快捷键",
            KeybindingScope::Workspace => "工作区快捷键",
        },
        match scope {
            KeybindingScope::Global => "Global shortcuts",
            KeybindingScope::Terminal => "Terminal shortcuts",
            KeybindingScope::Workspace => "Workspace shortcuts",
        },
    )
}

/// 计数徽标沿用 main 的圆角与中性描边，空分组仍按主页面规则隐藏。
fn count_badge(count: usize, theme: ThemeTokens) -> impl IntoElement {
    div()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(crate::locale::text(
            format!("{count} 项"),
            format!("{count} items"),
        ))
}

/// 快捷键作用域与动作标识沿用 main 的中性描边徽标以便快速扫描。
fn shortcut_metadata_badge(
    label: impl Into<SharedString>,
    monospace: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let mut badge = div()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .text_size(theme.text_xs)
        .text_color(theme.muted);
    if monospace {
        badge = badge.font_family("monospace");
    }
    badge.child(label.into())
}

/// Group IDs remain stable across localization for tests, disclosure search, and UI identities.
fn scope_key(scope: KeybindingScope) -> &'static str {
    match scope {
        KeybindingScope::Global => "global",
        KeybindingScope::Terminal => "terminal",
        KeybindingScope::Workspace => "workspace",
    }
}

/// 三个桌面平台使用稳定的配置字段名作为控件身份。
fn platform_key(platform: KeyboardPlatform) -> &'static str {
    match platform {
        KeyboardPlatform::Windows => "windows",
        KeyboardPlatform::Mac => "macos",
        KeyboardPlatform::Linux => "linux",
    }
}

/// 平台名称只在标签和辅助文案中显示，不参与持久化身份。
fn platform_label(platform: KeyboardPlatform) -> SharedString {
    match platform {
        KeyboardPlatform::Windows => crate::locale::text("Windows", "Windows"),
        KeyboardPlatform::Mac => crate::locale::text("macOS", "macOS"),
        KeyboardPlatform::Linux => crate::locale::text("Linux", "Linux"),
    }
}
