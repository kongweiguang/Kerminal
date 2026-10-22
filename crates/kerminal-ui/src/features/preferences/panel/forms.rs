//! 原生文本/多行输入构造与配置快照回显。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Entity, SharedString, Subscription, Window,
    component::input::{InputEvent, InputState, TextareaState},
};

use super::{
    AgentLauncherInputs, AgentLauncherTextField, KeywordRuleInputs, KeywordTextField,
    PreferencesPanel, ProfileInputs, ProfileTextField, SettingsInputs, SettingsTextField,
    TerminalBehaviorInputs,
};

/// settings 文本字段建立原生 GPUI Kit 输入状态并订阅值变化。
fn create_settings_input(
    placeholder: SharedString,
    field: SettingsTextField,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) -> (Entity<InputState>, Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
    let subscription = cx.subscribe(&input, move |panel, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            let value = input.read(cx).value().to_string();
            panel.update_settings_text(field, value, cx);
        }
    });
    (input, subscription)
}

/// Profile 文本输入订阅只更新 typed 草稿，不执行文件或进程 I/O。
fn create_profile_input(
    placeholder: SharedString,
    field: ProfileTextField,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) -> (Entity<InputState>, Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
    let subscription = cx.subscribe(&input, move |panel, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            let value = input.read(cx).value().to_string();
            panel.update_profile_text(field, value, cx);
        }
    });
    (input, subscription)
}

/// 多行参数和环境变量复用 GPUI Kit Textarea，避免把行分隔符压成不可编辑的单行文本。
fn create_profile_textarea(
    placeholder: SharedString,
    field: ProfileTextField,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) -> (Entity<TextareaState>, Subscription) {
    let input = cx.new(|cx| {
        TextareaState::new(window, cx)
            .placeholder(placeholder)
            .auto_grow(2, 6)
    });
    let subscription = cx.subscribe(&input, move |panel, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            let value = input.read(cx).value().to_string();
            panel.update_profile_text(field, value, cx);
        }
    });
    (input, subscription)
}

/// Agent Launcher 新定义仅收集普通配置文本，具体 command 安全约束在保存前说明。
fn create_agent_launcher_input(
    placeholder: SharedString,
    field: AgentLauncherTextField,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) -> (Entity<InputState>, Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
    let subscription = cx.subscribe(&input, move |panel, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            panel.update_custom_agent_text(field, input.read(cx).value().to_string(), cx);
        }
    });
    (input, subscription)
}

/// 新增和已有关键词行共用原生文本输入，字段身份通过 rule ID 而非行号关联。
fn create_keyword_input(
    placeholder: SharedString,
    field: KeywordTextField,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) -> (Entity<InputState>, Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
    let subscription = cx.subscribe(&input, move |panel, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            panel.update_keyword_text(field.clone(), input.read(cx).value().to_string(), cx);
        }
    });
    (input, subscription)
}

/// 仅变更现有输入的提示文案，保持其值、选择区间和焦点身份不变。
///
/// @author kongweiguang
fn refresh_input_placeholder(
    input: &Entity<InputState>,
    placeholder: SharedString,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    input.update(cx, |input, cx| {
        input.set_placeholder(placeholder, window, cx)
    });
}

/// 多行字段与单行字段分开更新，避免换语言时重建 textarea 的滚动/选区状态。
///
/// @author kongweiguang
fn refresh_textarea_placeholder(
    input: &Entity<TextareaState>,
    placeholder: SharedString,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    input.update(cx, |input, cx| {
        input.set_placeholder(placeholder, window, cx)
    });
}

impl PreferencesPanel {
    /// 原地更新已缓存的设置、快捷键和表单输入占位符，不重建状态或改变用户输入/焦点。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_locale_placeholders(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings_inputs = [
            (
                &self.settings_inputs.font_family,
                crate::locale::text("终端字体", "Terminal font"),
            ),
            (
                &self.settings_inputs.font_size,
                crate::locale::text("字号 10–24", "Font size 10–24"),
            ),
            (
                &self.settings_inputs.line_height,
                crate::locale::text("行高 1.0–1.8", "Line height 1.0–1.8"),
            ),
            (
                &self.settings_inputs.scrollback,
                crate::locale::text("滚屏行数 1000–50000", "Scrollback lines 1,000–50,000"),
            ),
            (
                &self.settings_inputs.background_path,
                crate::locale::text("背景图片路径", "Background image path"),
            ),
            (
                &self.settings_inputs.background_opacity,
                crate::locale::text("背景透明度 0–100", "Image opacity 0–100"),
            ),
            (
                &self.settings_inputs.window_opacity,
                crate::locale::text("窗口透明度 35–100", "Window opacity 35–100"),
            ),
            (
                &self.settings_inputs.sftp_global_transfers,
                crate::locale::text("SFTP 全局并发", "SFTP global concurrency"),
            ),
            (
                &self.settings_inputs.sftp_host_transfers,
                crate::locale::text("SFTP 单主机并发", "SFTP per-host concurrency"),
            ),
            (
                &self.settings_inputs.sftp_pipeline_depth,
                crate::locale::text("SFTP pipeline 深度", "SFTP pipeline depth"),
            ),
            (
                &self.settings_inputs.sftp_packet_bytes,
                crate::locale::text("SFTP 包大小", "SFTP packet size"),
            ),
            (
                &self.settings_inputs.sftp_idle_timeout,
                crate::locale::text("SFTP 空闲超时", "SFTP idle timeout"),
            ),
            (
                &self.settings_inputs.notification_min_duration,
                crate::locale::text("通知最小时长", "Notification minimum duration"),
            ),
            (
                &self.settings_inputs.notification_throttle,
                crate::locale::text("通知节流间隔", "Notification throttle interval"),
            ),
        ];
        for (input, placeholder) in settings_inputs {
            refresh_input_placeholder(input, placeholder, window, cx);
        }

        for shortcut in &self.shortcut_inputs {
            refresh_input_placeholder(
                &shortcut.windows,
                crate::locale::text("主键组合", "Primary shortcut"),
                window,
                cx,
            );
            refresh_input_placeholder(
                &shortcut.windows_alternate,
                crate::locale::text("备用组合（可选）", "Alternate shortcut (optional)"),
                window,
                cx,
            );
            refresh_input_placeholder(
                &shortcut.linux,
                crate::locale::text("按键组合", "Key combination"),
                window,
                cx,
            );
            refresh_input_placeholder(
                &shortcut.mac,
                crate::locale::text("按键组合", "Key combination"),
                window,
                cx,
            );
        }

        let profile_inputs = [
            (
                &self.profile_inputs.name,
                crate::locale::text("Profile 名称", "Profile name"),
            ),
            (
                &self.profile_inputs.shell,
                crate::locale::text("Shell 程序路径", "Shell executable"),
            ),
            (
                &self.profile_inputs.working_directory,
                crate::locale::text("工作目录（可选）", "Working directory (optional)"),
            ),
        ];
        for (input, placeholder) in profile_inputs {
            refresh_input_placeholder(input, placeholder, window, cx);
        }
        refresh_textarea_placeholder(
            &self.profile_inputs.arguments,
            crate::locale::text("启动参数：每行一个参数", "Arguments: one per line"),
            window,
            cx,
        );
        refresh_textarea_placeholder(
            &self.profile_inputs.environment,
            crate::locale::text(
                "环境变量：每行 KEY=VALUE",
                "Environment: one KEY=VALUE per line",
            ),
            window,
            cx,
        );

        refresh_input_placeholder(
            &self.agent_launcher_inputs.name,
            crate::locale::text("自定义 Agent 名称", "Custom Agent name"),
            window,
            cx,
        );
        refresh_input_placeholder(
            &self.agent_launcher_inputs.command,
            crate::locale::text(
                "启动命令（不要填写密钥）",
                "Launch command (do not enter secrets)",
            ),
            window,
            cx,
        );

        let terminal_inputs = [
            (
                &self.terminal_behavior_inputs.audit_retention_days,
                crate::locale::text("审计保留天数", "Audit retention days"),
            ),
            (
                &self.terminal_behavior_inputs.feedback_retention_days,
                crate::locale::text("反馈保留天数", "Feedback retention days"),
            ),
            (
                &self.terminal_behavior_inputs.new_keyword_pattern,
                crate::locale::text("关键词或正则", "Keyword or regular expression"),
            ),
            (
                &self.terminal_behavior_inputs.new_keyword_note,
                crate::locale::text("备注（可选）", "Note (optional)"),
            ),
        ];
        for (input, placeholder) in terminal_inputs {
            refresh_input_placeholder(input, placeholder, window, cx);
        }
        for rule in &self.terminal_behavior_inputs.keyword_rules {
            refresh_input_placeholder(
                &rule.pattern,
                crate::locale::text("关键词或正则", "Keyword or regular expression"),
                window,
                cx,
            );
            refresh_input_placeholder(&rule.note, crate::locale::text("备注", "Note"), window, cx);
        }
    }
}

impl PreferencesPanel {
    /// 固定 settings 控件用数组映射和解构保持字段一一对应，避免 Vec 顺序变更造成启动期 panic。
    pub(super) fn create_settings_inputs(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (SettingsInputs, Vec<Subscription>) {
        let fields = [
            (
                crate::locale::text("终端字体", "Terminal font"),
                SettingsTextField::FontFamily,
            ),
            (
                crate::locale::text("字号 10–24", "Font size 10–24"),
                SettingsTextField::FontSize,
            ),
            (
                crate::locale::text("行高 1.0–1.8", "Line height 1.0–1.8"),
                SettingsTextField::LineHeight,
            ),
            (
                crate::locale::text("滚屏行数 1000–50000", "Scrollback lines 1,000–50,000"),
                SettingsTextField::Scrollback,
            ),
            (
                crate::locale::text("背景图片路径", "Background image path"),
                SettingsTextField::BackgroundPath,
            ),
            (
                crate::locale::text("背景透明度 0–100", "Image opacity 0–100"),
                SettingsTextField::BackgroundOpacity,
            ),
            (
                crate::locale::text("窗口透明度 35–100", "Window opacity 35–100"),
                SettingsTextField::WindowOpacity,
            ),
            (
                crate::locale::text("SFTP 全局并发", "SFTP global concurrency"),
                SettingsTextField::SftpGlobalTransfers,
            ),
            (
                crate::locale::text("SFTP 单主机并发", "SFTP per-host concurrency"),
                SettingsTextField::SftpHostTransfers,
            ),
            (
                crate::locale::text("SFTP pipeline 深度", "SFTP pipeline depth"),
                SettingsTextField::SftpPipelineDepth,
            ),
            (
                crate::locale::text("SFTP 包大小", "SFTP packet size"),
                SettingsTextField::SftpPacketBytes,
            ),
            (
                crate::locale::text("SFTP 空闲超时", "SFTP idle timeout"),
                SettingsTextField::SftpIdleTimeout,
            ),
            (
                crate::locale::text("通知最小时长", "Notification minimum duration"),
                SettingsTextField::NotificationMinDuration,
            ),
            (
                crate::locale::text("通知节流间隔", "Notification throttle interval"),
                SettingsTextField::NotificationThrottle,
            ),
        ];
        let [
            (font_family, font_family_subscription),
            (font_size, font_size_subscription),
            (line_height, line_height_subscription),
            (scrollback, scrollback_subscription),
            (background_path, background_path_subscription),
            (background_opacity, background_opacity_subscription),
            (window_opacity, window_opacity_subscription),
            (sftp_global_transfers, sftp_global_transfers_subscription),
            (sftp_host_transfers, sftp_host_transfers_subscription),
            (sftp_pipeline_depth, sftp_pipeline_depth_subscription),
            (sftp_packet_bytes, sftp_packet_bytes_subscription),
            (sftp_idle_timeout, sftp_idle_timeout_subscription),
            (notification_min_duration, notification_min_duration_subscription),
            (notification_throttle, notification_throttle_subscription),
        ] = fields
            .map(|(placeholder, field)| create_settings_input(placeholder, field, window, cx));
        let values = SettingsInputs {
            font_family,
            font_size,
            line_height,
            scrollback,
            background_path,
            background_opacity,
            window_opacity,
            sftp_global_transfers,
            sftp_host_transfers,
            sftp_pipeline_depth,
            sftp_packet_bytes,
            sftp_idle_timeout,
            notification_min_duration,
            notification_throttle,
        };
        let subscriptions = vec![
            font_family_subscription,
            font_size_subscription,
            line_height_subscription,
            scrollback_subscription,
            background_path_subscription,
            background_opacity_subscription,
            window_opacity_subscription,
            sftp_global_transfers_subscription,
            sftp_host_transfers_subscription,
            sftp_pipeline_depth_subscription,
            sftp_packet_bytes_subscription,
            sftp_idle_timeout_subscription,
            notification_min_duration_subscription,
            notification_throttle_subscription,
        ];
        (values, subscriptions)
    }

    /// Profile 字段通过数组解构绑定输入与订阅，避免新增字段后遗漏实体或触发运行时断言。
    pub(super) fn create_profile_inputs(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (ProfileInputs, Vec<Subscription>) {
        let fields = [
            (
                crate::locale::text("Profile 名称", "Profile name"),
                ProfileTextField::Name,
            ),
            (
                crate::locale::text("Shell 程序路径", "Shell executable"),
                ProfileTextField::Shell,
            ),
            (
                crate::locale::text("工作目录（可选）", "Working directory (optional)"),
                ProfileTextField::WorkingDirectory,
            ),
        ];
        let [
            (name, name_subscription),
            (shell, shell_subscription),
            (working_directory, working_directory_subscription),
        ] = fields.map(|(placeholder, field)| create_profile_input(placeholder, field, window, cx));
        let (arguments, arguments_subscription) = create_profile_textarea(
            crate::locale::text("启动参数：每行一个参数", "Arguments: one per line"),
            ProfileTextField::Arguments,
            window,
            cx,
        );
        let (environment, environment_subscription) = create_profile_textarea(
            crate::locale::text(
                "环境变量：每行 KEY=VALUE",
                "Environment: one KEY=VALUE per line",
            ),
            ProfileTextField::Environment,
            window,
            cx,
        );
        let values = ProfileInputs {
            name,
            shell,
            working_directory,
            arguments,
            environment,
        };
        let subscriptions = vec![
            name_subscription,
            shell_subscription,
            working_directory_subscription,
            arguments_subscription,
            environment_subscription,
        ];
        (values, subscriptions)
    }

    /// 临时新建框在面板生命周期内保持稳定，避免输入时每帧重建原生控件。
    pub(super) fn create_agent_launcher_inputs(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (AgentLauncherInputs, Vec<Subscription>) {
        let (name, name_subscription) = create_agent_launcher_input(
            crate::locale::text("自定义 Agent 名称", "Custom agent name"),
            AgentLauncherTextField::Name,
            window,
            cx,
        );
        let (command, command_subscription) = create_agent_launcher_input(
            crate::locale::text(
                "启动命令（不要填写密钥）",
                "Launch command (do not enter secrets)",
            ),
            AgentLauncherTextField::Command,
            window,
            cx,
        );
        (
            AgentLauncherInputs { name, command },
            vec![name_subscription, command_subscription],
        )
    }

    /// Terminal 行为固定输入在构造时建立，已有关键词行由 settings 加载结果单独重建。
    pub(super) fn create_terminal_behavior_inputs(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (TerminalBehaviorInputs, Vec<Subscription>) {
        let (audit_retention_days, audit_subscription) = create_settings_input(
            crate::locale::text("审计保留天数", "Audit retention days"),
            SettingsTextField::SuggestionAuditRetention,
            window,
            cx,
        );
        let (feedback_retention_days, feedback_subscription) = create_settings_input(
            crate::locale::text("反馈保留天数", "Feedback retention days"),
            SettingsTextField::SuggestionFeedbackRetention,
            window,
            cx,
        );
        let (new_keyword_pattern, pattern_subscription) = create_keyword_input(
            crate::locale::text("关键词或正则", "Keyword or regular expression"),
            KeywordTextField::NewPattern,
            window,
            cx,
        );
        let (new_keyword_note, note_subscription) = create_keyword_input(
            crate::locale::text("备注（可选）", "Note (optional)"),
            KeywordTextField::NewNote,
            window,
            cx,
        );
        (
            TerminalBehaviorInputs {
                audit_retention_days,
                feedback_retention_days,
                new_keyword_pattern,
                new_keyword_note,
                keyword_rules: Vec::<KeywordRuleInputs>::new(),
            },
            vec![
                audit_subscription,
                feedback_subscription,
                pattern_subscription,
                note_subscription,
            ],
        )
    }

    /// 为当前快照中的关键词规则建立稳定输入并持有 Change subscription。
    pub(super) fn create_keyword_rule_inputs(
        id: String,
        pattern: &str,
        note: &str,
        colors: Option<&kerminal_application::preferences::TerminalKeywordHighlightCustomColors>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (KeywordRuleInputs, Vec<Subscription>) {
        let (pattern_input, pattern_subscription) = create_keyword_input(
            crate::locale::text("关键词或正则", "Keyword or regular expression"),
            KeywordTextField::RulePattern(id.clone()),
            window,
            cx,
        );
        pattern_input.update(cx, |input, cx| {
            input.set_value(pattern.to_owned(), window, cx)
        });
        let (note_input, note_subscription) = create_keyword_input(
            crate::locale::text("备注", "Note"),
            KeywordTextField::RuleNote(id.clone()),
            window,
            cx,
        );
        note_input.update(cx, |input, cx| input.set_value(note.to_owned(), window, cx));
        let (light_foreground, light_foreground_subscription) = create_keyword_input(
            "#RRGGBB".into(),
            KeywordTextField::LightForeground(id.clone()),
            window,
            cx,
        );
        let (light_background, light_background_subscription) = create_keyword_input(
            "#RRGGBB".into(),
            KeywordTextField::LightBackground(id.clone()),
            window,
            cx,
        );
        let (dark_foreground, dark_foreground_subscription) = create_keyword_input(
            "#RRGGBB".into(),
            KeywordTextField::DarkForeground(id.clone()),
            window,
            cx,
        );
        let (dark_background, dark_background_subscription) = create_keyword_input(
            "#RRGGBB".into(),
            KeywordTextField::DarkBackground(id.clone()),
            window,
            cx,
        );
        if let Some(colors) = colors {
            light_foreground.update(cx, |input, cx| {
                input.set_value(
                    colors.light.foreground.clone().unwrap_or_default(),
                    window,
                    cx,
                )
            });
            light_background.update(cx, |input, cx| {
                input.set_value(
                    colors.light.background.clone().unwrap_or_default(),
                    window,
                    cx,
                )
            });
            dark_foreground.update(cx, |input, cx| {
                input.set_value(
                    colors.dark.foreground.clone().unwrap_or_default(),
                    window,
                    cx,
                )
            });
            dark_background.update(cx, |input, cx| {
                input.set_value(
                    colors.dark.background.clone().unwrap_or_default(),
                    window,
                    cx,
                )
            });
        }
        (
            KeywordRuleInputs {
                id,
                pattern: pattern_input,
                note: note_input,
                light_foreground,
                light_background,
                dark_foreground,
                dark_background,
            },
            vec![
                pattern_subscription,
                note_subscription,
                light_foreground_subscription,
                light_background_subscription,
                dark_foreground_subscription,
                dark_background_subscription,
            ],
        )
    }
}

/// 同步单个输入控件的文本时清空 undo 并避免产生伪造 Change 事件。
pub(super) fn set_input(
    input: &Entity<InputState>,
    value: String,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    input.update(cx, |state, cx| state.set_value(value, window, cx));
}

/// 同步多行输入的恢复值而不触发 Change，保证从文件加载不会被算成用户编辑。
pub(super) fn set_textarea(
    input: &Entity<TextareaState>,
    value: String,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    input.update(cx, |state, cx| state.set_value(value, window, cx));
}
