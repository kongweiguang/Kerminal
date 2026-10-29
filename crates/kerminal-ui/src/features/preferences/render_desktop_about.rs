//! GPUI 设置中的桌面通知、平台快捷键和关于页面。
//!
//! @author kongweiguang

use std::rc::Rc;

use gpui_kit::{
    ClickEvent, Context, Entity, IntoElement, SharedString,
    assets::IconName,
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

use super::{KeyboardPlatform, panel::PreferencesPanel};

/// main v0.3.48 的设置页动作目录；GPUI 专属运行态动作不改变这份可见目录。
#[derive(Clone, Copy)]
pub(super) struct MainShortcutPresentation {
    pub(super) action: &'static str,
    pub(super) label_zh: &'static str,
    pub(super) label_en: &'static str,
    pub(super) description_zh: &'static str,
    pub(super) description_en: &'static str,
    pub(super) scope: KeybindingScope,
}

/// 保留 main 的动作顺序与文案；显示层投影不覆盖配置中的用户绑定。
pub(super) const MAIN_SHORTCUT_PRESENTATIONS: [MainShortcutPresentation; 16] = [
    MainShortcutPresentation {
        action: "settings.open",
        label_zh: "打开设置",
        label_en: "Open settings",
        description_zh: "参考 IntelliJ IDEA 的设置入口打开设置。",
        description_en: "Open Settings using the IntelliJ IDEA shortcut.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "settings.keybindings",
        label_zh: "查看快捷键与动作",
        label_en: "View shortcuts and actions",
        description_zh: "打开快捷键与动作列表。",
        description_en: "Open the shortcut and action list directly.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "terminal.focus",
        label_zh: "回到终端",
        label_en: "Return to terminal",
        description_zh: "显示终端工作区并收起右栏。",
        description_en: "Show the terminal workspace and collapse the right pane.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "terminal.newTab",
        label_zh: "新建本地终端",
        label_en: "New local terminal",
        description_zh: "新建本地终端 tab。",
        description_en: "Open a new local terminal workspace tab.",
        scope: KeybindingScope::Workspace,
    },
    MainShortcutPresentation {
        action: "terminal.closeTab",
        label_zh: "关闭当前终端 tab",
        label_en: "Close current terminal tab",
        description_zh: "关闭当前终端 tab。",
        description_en: "Close the current terminal workspace tab.",
        scope: KeybindingScope::Workspace,
    },
    MainShortcutPresentation {
        action: "terminal.closePane",
        label_zh: "关闭当前分屏",
        label_en: "Close current pane",
        description_zh: "关闭当前分屏，多分屏时生效。",
        description_en: "Close the current split when multiple panes are open.",
        scope: KeybindingScope::Terminal,
    },
    MainShortcutPresentation {
        action: "terminal.splitHorizontal",
        label_zh: "左右分屏",
        label_en: "Split right",
        description_zh: "在当前终端右侧创建分屏。",
        description_en: "Create a pane to the right of the current terminal.",
        scope: KeybindingScope::Workspace,
    },
    MainShortcutPresentation {
        action: "terminal.splitVertical",
        label_zh: "上下分屏",
        label_en: "Split below",
        description_zh: "在当前终端下方创建分屏。",
        description_en: "Create a pane below the current terminal.",
        scope: KeybindingScope::Workspace,
    },
    MainShortcutPresentation {
        action: "terminal.previousTab",
        label_zh: "上一个终端 tab",
        label_en: "Previous terminal tab",
        description_zh: "切到左侧终端 tab。",
        description_en: "Switch to the terminal tab immediately to the left.",
        scope: KeybindingScope::Workspace,
    },
    MainShortcutPresentation {
        action: "terminal.nextTab",
        label_zh: "下一个终端 tab",
        label_en: "Next terminal tab",
        description_zh: "切到右侧终端 tab。",
        description_en: "Switch to the terminal tab immediately to the right.",
        scope: KeybindingScope::Workspace,
    },
    MainShortcutPresentation {
        action: "tool.agentLauncher",
        label_zh: "打开 Agent Launcher",
        label_en: "Open Agent Launcher",
        description_zh: "打开 Agent Launcher。",
        description_en: "Open the Agent Launcher tool window.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "tool.system",
        label_zh: "打开系统信息",
        label_en: "Open system information",
        description_zh: "打开系统信息。",
        description_en: "Open the system information tool window.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "tool.sftp",
        label_zh: "打开 SFTP",
        label_en: "Open SFTP",
        description_zh: "打开 SFTP。",
        description_en: "Open the SFTP tool window.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "tool.ports",
        label_zh: "打开端口转发",
        label_en: "Open port forwarding",
        description_zh: "打开端口转发。",
        description_en: "Open the port forwarding tool window.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "tool.snippets",
        label_zh: "打开脚本片段",
        label_en: "Open snippets",
        description_zh: "打开脚本片段。",
        description_en: "Open the snippets tool window.",
        scope: KeybindingScope::Global,
    },
    MainShortcutPresentation {
        action: "tool.logs",
        label_zh: "打开命令历史",
        label_en: "Open command history",
        description_zh: "打开当前终端的命令历史。",
        description_en: "Open the current terminal's command history.",
        scope: KeybindingScope::Global,
    },
];

/// GPUI 新增动作仍留在设置快照和运行时，只从 main 基线设置目录中排除。
pub(super) const GPUI_ONLY_SHORTCUT_ACTIONS: [&str; 5] = [
    "terminal.search",
    "workspace.quickOpen",
    "workspace.commandPalette",
    "application.quit",
    "terminal.paste",
];

/// 返回 main 默认列表中的动作位置；未知自定义动作由稳定排序放在其后。
pub(super) fn main_shortcut_order(action: &str) -> Option<usize> {
    MAIN_SHORTCUT_PRESENTATIONS
        .iter()
        .position(|presentation| presentation.action == action)
}

/// GPUI-only 动作仅从设置目录隐藏，避免删除仍由运行时消费的配置项。
pub(super) fn main_settings_shortcut_visible(action: &str) -> bool {
    !GPUI_ONLY_SHORTCUT_ACTIONS.contains(&action)
}

/// 返回 main 对应动作的静态文案投影，用户自定义 action 继续使用其配置字段。
pub(super) fn main_shortcut_presentation(
    action: &str,
) -> Option<&'static MainShortcutPresentation> {
    MAIN_SHORTCUT_PRESENTATIONS
        .iter()
        .find(|presentation| presentation.action == action)
}

/// 已知动作遵从 main 的固定作用域，自定义动作沿用配置作用域避免丢失用户分组。
/// @author kongweiguang
pub(super) fn shortcut_display_scope(
    action: &str,
    configured_scope: KeybindingScope,
) -> KeybindingScope {
    main_shortcut_presentation(action).map_or(configured_scope, |presentation| presentation.scope)
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

/// 行与 Switch 共用受控回调；Switch 停止冒泡以免一次点击重复提交，禁用时不注册行点击。
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
        .gap_3()
        .px_3()
        .py_2()
        .min_h(px(44.))
        .rounded(theme.control_radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(Icon::new(icon).text_color(theme.accent))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_sm)
                .child(label),
        )
        .child(
            Switch::new(id)
                .track_size(px(40.), px(24.))
                .checked(checked)
                .disabled(disabled)
                .accessibility_label(switch_accessible_label)
                .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                    cx.stop_propagation();
                    switch_change(panel, *enabled, cx);
                })),
        )
        .when(checked, |row| {
            row.border_color(theme.accent.opacity(0.3))
                .bg(theme.surface_selected())
        });
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
                    label: crate::locale::text(
                        "优先通知后台和耗时事件",
                        "Prefer background and long-running events",
                    ),
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

    /// 关于页呈现真实版本、许可、仓库和更新事务状态；GitHub 外链保留语义按钮并将图标放在文字之后对齐 main。
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
            .flex_shrink_0();
        if let Some(state) = &update {
            let check_label = if state.checking {
                crate::locale::text("检查中", "Checking")
            } else {
                crate::locale::text("检查", "Check")
            };
            actions = actions.child(
                Button::new("settings-about-check-updates")
                    .compact()
                    .outline()
                    .disabled(!state.can_check)
                    .label(check_label)
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
            .p_4()
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
                    .overflow_hidden()
                    .rounded(theme.radius_sm)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
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

    /// Windows 与 macOS 分段选择只改变当前投影，保存时仅修改所选平台字段。
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
                let input = self
                    .shortcut_inputs
                    .iter()
                    .find(|inputs| inputs.action == binding.action)
                    .map(|inputs| shortcut_input(inputs, platform));
                let current = binding_for_platform(binding, platform);
                let default = defaults
                    .iter()
                    .find(|candidate| candidate.action == binding.action)
                    .map(|candidate| binding_for_platform(candidate, platform))
                    .unwrap_or_else(|| current.clone());
                let customized = current != default;
                let issues = self
                    .settings_issues
                    .iter()
                    .filter(|issue| issue.actions.iter().any(|action| action == &binding.action));
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
                        input,
                        platform,
                        customized,
                        disabled,
                        last_in_group: row_index + 1 == bindings.len(),
                        theme,
                        issues: issues.map(|issue| issue.message.clone()).collect(),
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
                    "使用 Ctrl+Shift+T 格式；留空表示禁用。",
                    "Use Ctrl+Shift+T syntax; leave empty to disable.",
                )),
        )
        .child(self.render_save_notice(theme))
    }

    /// 只恢复选中平台的默认值，并同步输入实体，避免其它平台自定义绑定被覆盖。
    fn reset_shortcut(
        &mut self,
        action: &str,
        platform: KeyboardPlatform,
        input: Option<Entity<InputState>>,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(default) = SettingsSnapshot::default()
            .keybindings
            .into_iter()
            .find(|binding| binding.action == action)
        else {
            return;
        };
        let value = binding_for_platform(&default, platform);
        self.edit_settings(
            |settings| {
                if let Some(binding) = settings
                    .keybindings
                    .iter_mut()
                    .find(|binding| binding.action == action)
                {
                    match platform {
                        KeyboardPlatform::Windows => {
                            binding.binding = default.binding;
                            binding.windows_binding = default.windows_binding;
                        }
                        KeyboardPlatform::Mac => binding.mac_binding = default.mac_binding,
                        KeyboardPlatform::Linux => {}
                    }
                }
            },
            cx,
        );
        if let Some(input) = input {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
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

/// 用 main 的图标、标签和值层级显示版本及许可元数据。
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
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .px_3()
        .py(px(10.))
        .child(Icon::new(icon).mt(px(2.)).text_color(theme.muted))
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

/// 外链操作与 URL 文字同一行，保持错误无关的 About 更新状态。
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
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .px_3()
        .py(px(10.))
        .child(Icon::new(icon).mt(px(2.)).text_color(theme.muted))
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

/// 错误和正常更新状态使用主题令牌区分，不将未知结果伪装成最新版。
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
        .gap_3()
        .px_3()
        .py(px(10.))
        .child(Icon::new(icon).mt(px(2.)).text_color(theme.muted))
        .child(
            div()
                .flex_1()
                .min_w(px(160.))
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
    .disabled(disabled);
    if platform == selected {
        button = button.custom(
            ButtonCustomVariant::new(cx)
                .color(theme.surface_selected())
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
        panel.shortcut_platform = platform;
        cx.notify();
    }))
}

/// 一条快捷键保留 main 文案投影、作用域和行分隔，并继续驱动真实绑定输入。
///
/// @author kongweiguang
struct ShortcutRowConfig<'a> {
    binding: &'a KeybindingPreference,
    label: SharedString,
    description: SharedString,
    scope: KeybindingScope,
    input: Option<Entity<InputState>>,
    platform: KeyboardPlatform,
    customized: bool,
    disabled: bool,
    last_in_group: bool,
    theme: ThemeTokens,
    issues: Vec<String>,
}

/// 把同一行的显示与编辑上下文组合传入，保留输入实体和草稿状态的单一 owner。
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
        input,
        platform,
        customized,
        disabled,
        last_in_group,
        theme,
        issues,
    } = config;
    let action = binding.action.clone();
    let reset_input = input.clone();
    let editable = binding.editable && !disabled;
    let reset = Button::new(format!("shortcut-reset-{}", binding.action))
        .compact()
        .ghost()
        .custom(super::render_settings::settings_ghost_variant(theme, cx))
        .icon(IconName::RotateCcw)
        .disabled(!editable || !customized)
        .tooltip(crate::locale::text("恢复默认", "Restore default"))
        .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
            panel.reset_shortcut(&action, platform, reset_input.clone(), window, cx);
        }));
    let binding_control = if binding.editable {
        match input {
            Some(input) => div()
                .id(format!(
                    "shortcut-input-{}-{}",
                    binding.action,
                    platform_key(platform)
                ))
                .flex_1()
                .min_w(px(0.))
                .child(
                    super::render_settings::settings_input(&input, theme)
                        .disabled(disabled)
                        .w_full()
                        .min_w(px(0.)),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text("加载中…", "Loading…"))
                .into_any_element(),
        }
    } else {
        div()
            .flex_1()
            .min_w(px(0.))
            .px_2()
            .py_1()
            .rounded(theme.radius_sm)
            .border_1()
            .border_color(theme.accent.opacity(0.3))
            .bg(theme.selected)
            .text_size(theme.text_xs)
            .text_color(theme.accent)
            .font_family("monospace")
            .child(binding_for_platform(binding, platform))
            .into_any_element()
    };
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

/// 平台输入状态由 panel 的稳定 action 集合维护，视图只选择对应的既有实体。
fn shortcut_input(
    inputs: &super::panel::ShortcutInputs,
    platform: KeyboardPlatform,
) -> Entity<InputState> {
    match platform {
        KeyboardPlatform::Windows | KeyboardPlatform::Linux => inputs.windows.clone(),
        KeyboardPlatform::Mac => inputs.mac.clone(),
    }
}

/// 配置模型的历史回退语义与快捷键验证器一致，兼容只填写通用字段的旧配置。
fn binding_for_platform(binding: &KeybindingPreference, platform: KeyboardPlatform) -> String {
    match platform {
        KeyboardPlatform::Windows => effective_binding(&binding.windows_binding, &binding.binding),
        KeyboardPlatform::Mac => effective_binding(&binding.mac_binding, &binding.binding),
        KeyboardPlatform::Linux => binding
            .linux_binding
            .clone()
            .unwrap_or_else(|| effective_binding(&binding.windows_binding, &binding.binding)),
    }
}

/// 只把 platform-specific 字段非空值作为覆盖，保留历史通用快捷键回退规则。
fn effective_binding(platform: &str, fallback: &str) -> String {
    if platform.is_empty() {
        fallback.to_owned()
    } else {
        platform.to_owned()
    }
}

/// Git clone suffix 只属于打开地址，About 文本沿用 main 展示的仓库路径形式。
///
/// @author kongweiguang
pub(super) fn display_repository_url(url: &str) -> String {
    let visible_url = url.strip_prefix("https://").unwrap_or(url);
    visible_url
        .strip_suffix(".git")
        .unwrap_or(visible_url)
        .to_owned()
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

/// Only Windows and macOS are offered because these are the two platforms in main's settings UI.
fn platform_key(platform: KeyboardPlatform) -> &'static str {
    match platform {
        KeyboardPlatform::Windows => "windows",
        KeyboardPlatform::Mac => "macos",
        KeyboardPlatform::Linux => "linux",
    }
}

/// Segment labels follow the application's platform naming used in main.
fn platform_label(platform: KeyboardPlatform) -> SharedString {
    match platform {
        KeyboardPlatform::Windows => crate::locale::text("Windows", "Windows"),
        KeyboardPlatform::Mac => crate::locale::text("macOS", "macOS"),
        KeyboardPlatform::Linux => crate::locale::text("Linux", "Linux"),
    }
}
