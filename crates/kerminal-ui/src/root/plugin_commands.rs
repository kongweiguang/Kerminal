//! 外部命令、快捷键和标签事件的窗口侧协调；插件 RPC 仍由应用服务执行。
//!
//! @author kongweiguang

use std::{
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{Context, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    external_contributions::PreparedPlugin,
    workspace::{WorkspaceSessionTab, WorkspaceTargetRef},
};
use kerminal_domain::plugins::PluginId;
use serde_json::{Map, Value, json};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::RootView;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_EVENTS: usize = 128;

// 此纯状态模块不依赖窗口或 I/O，便于在不启动桌面的情况下验证身份和生命周期。
mod command_logic {
    use std::{
        collections::{BTreeMap, VecDeque},
        time::Instant,
    };

    /// 完整身份由宿主冻结；同名命令属于不同插件时不能互相覆盖。
    #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub(crate) struct PluginCommandId {
        pub(crate) plugin: String,
        pub(crate) command: String,
    }

    /// 命令入口只携带稳定身份，不把原始主机配置或凭据送进插件。
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub(crate) struct PluginCommandContext {
        pub(crate) tab: Option<String>,
        pub(crate) host: Option<String>,
    }

    /// 面板打开时连同目标一起固定，确认时不按当前焦点重新寻找替代目标。
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(crate) struct PluginCommandInvocation {
        pub(crate) id: PluginCommandId,
        pub(crate) context: PluginCommandContext,
    }

    /// 列表展示与快捷键使用同一份冻结声明，标题变化不参与身份判断。
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(crate) struct PluginCommand {
        pub(crate) id: PluginCommandId,
        pub(crate) title: String,
        pub(crate) keybinding: Option<String>,
    }

    /// 只展开完整翻译标记，并对空翻译回退英文；缺失时不能把占位标记当成命令标题。
    pub(super) fn localized_title(
        translations: &BTreeMap<String, BTreeMap<String, String>>,
        locale: &str,
        title: &str,
    ) -> Option<String> {
        let value = match title
            .strip_prefix('%')
            .and_then(|value| value.strip_suffix('%'))
        {
            Some(key) if !key.is_empty() => translations
                .get(locale)
                .and_then(|messages| messages.get(key))
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    translations
                        .get("en")
                        .and_then(|messages| messages.get(key))
                })
                .map(String::as_str),
            _ => Some(title),
        };
        value
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
    }

    /// 全局插件快捷键与已有完整绑定或组合键前缀重叠时必须让出，避免悄悄覆盖用户设置。
    pub(super) fn sequences_overlap(left: &str, right: &str) -> bool {
        left == right
            || left
                .strip_prefix(right)
                .is_some_and(|rest| rest.starts_with(' '))
            || right
                .strip_prefix(left)
                .is_some_and(|rest| rest.starts_with(' '))
    }

    /// 同一命令一次只保留一个请求；代次使取消后的迟到回执不能结束新请求。
    pub(super) struct InFlight<T> {
        next: u64,
        entries: BTreeMap<PluginCommandId, (u64, T)>,
    }

    impl<T> Default for InFlight<T> {
        /// 初始序号不复用已完成的请求，避免在窗口生命周期内混淆两个请求。
        fn default() -> Self {
            Self {
                next: 0,
                entries: BTreeMap::new(),
            }
        }
    }

    impl<T> InFlight<T> {
        /// 拒绝重复和序号耗尽；调用方持有的取消资源随拒绝自动释放。
        pub(super) fn begin(&mut self, id: PluginCommandId, resource: T) -> Option<u64> {
            if self.entries.contains_key(&id) {
                return None;
            }
            let next = self.next.checked_add(1)?;
            self.next = next;
            self.entries.insert(id, (next, resource));
            Some(next)
        }

        /// 只移除原始身份和序号同时匹配的请求，不让旧结果覆盖新状态。
        pub(super) fn finish(&mut self, id: &PluginCommandId, sequence: u64) -> bool {
            if self
                .entries
                .get(id)
                .is_some_and(|(current, _)| *current == sequence)
            {
                self.entries.remove(id);
                true
            } else {
                false
            }
        }

        /// 取消所有所有者资源，但不重置序号，以隔离仍在收尾的旧请求。
        pub(super) fn clear(&mut self) {
            self.entries.clear();
        }
    }

    /// 入队结果区分重复切换和容量耗尽，后者必须反馈给窗口。
    #[derive(Debug, Eq, PartialEq)]
    pub(super) enum EnqueueResult {
        Queued,
        Unchanged,
        Full,
    }

    /// 窗口内按顺序发送标签变化；取消后保留序号以隔离迟到事件。
    pub(super) struct TabEventQueue<T> {
        queued: VecDeque<(Option<String>, Instant)>,
        active: Option<(u64, T)>,
        next: u64,
        last_tab: Option<Option<String>>,
    }

    impl<T> Default for TabEventQueue<T> {
        /// 每个窗口从独立队列开始，不让其它窗口的选中状态影响去重。
        fn default() -> Self {
            Self {
                queued: VecDeque::new(),
                active: None,
                next: 0,
                last_tab: None,
            }
        }
    }

    impl<T> TabEventQueue<T> {
        /// 容量检查发生在修改最后目标之前，队满后仍可重试同一个标签。
        pub(super) fn enqueue(
            &mut self,
            tab: Option<String>,
            deadline: Instant,
            limit: usize,
        ) -> EnqueueResult {
            if self.last_tab.as_ref() == Some(&tab) {
                return EnqueueResult::Unchanged;
            }
            if self.queued.len() >= limit {
                return EnqueueResult::Full;
            }
            self.last_tab = Some(tab.clone());
            self.queued.push_back((tab, deadline));
            EnqueueResult::Queued
        }

        /// 只有上一通知已结束才能取下一项；序号耗尽时保留队首并显式报告失败。
        pub(super) fn begin(
            &mut self,
            resource: T,
        ) -> Result<Option<(u64, Option<String>, Instant)>, ()> {
            if self.active.is_some() || self.queued.is_empty() {
                return Ok(None);
            }
            let next = self.next.checked_add(1).ok_or(())?;
            let Some((tab, deadline)) = self.queued.pop_front() else {
                return Ok(None);
            };
            self.next = next;
            self.active = Some((next, resource));
            Ok(Some((next, tab, deadline)))
        }

        /// 旧通知结束不能撤销取消后新建立的活动通知。
        pub(super) fn finish(&mut self, sequence: u64) -> bool {
            if self
                .active
                .as_ref()
                .is_some_and(|(current, _)| *current == sequence)
            {
                self.active = None;
                true
            } else {
                false
            }
        }

        /// 释放当前通知的取消资源并丢弃未发送项；下一次入队重新判断标签。
        pub(super) fn clear(&mut self) {
            self.queued.clear();
            self.active = None;
            self.last_tab = None;
        }
    }
}
// END COMMAND LOGIC

pub(crate) use command_logic::{
    PluginCommand, PluginCommandContext, PluginCommandId, PluginCommandInvocation,
};

/// GPUI Action 只携带稳定命令身份；上下文必须在实际触发时由窗口捕获。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.plugin.command", no_json, no_register)]
pub(crate) struct PluginCommandAction {
    pub(crate) id: PluginCommandId,
}

/// 安装失败要能报告具体命令，避免无效或冲突绑定被误认为已经生效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PluginShortcutIssue {
    pub(crate) id: PluginCommandId,
    pub(crate) binding: String,
    pub(crate) reason: &'static str,
}

/// 窗口持有取消资源及有界事件队列；释放窗口会协作取消尚未完成的调用。
#[derive(Default)]
pub(super) struct PluginCommandState {
    commands: command_logic::InFlight<DropGuard>,
    events: command_logic::TabEventQueue<DropGuard>,
}

impl PluginCommandState {
    /// 丢弃请求令牌会传播取消；不重置代次以防迟到回执命中新调用。
    pub(super) fn cancel(&mut self) {
        self.commands.clear();
        self.events.clear();
    }
}

/// 目录只消费启动时准备好的资源，按完整身份排序并对当前语言解析标题。
pub(crate) fn plugin_commands(plugins: &[Arc<PreparedPlugin>], locale: &str) -> Vec<PluginCommand> {
    let mut commands = Vec::new();
    for prepared in plugins {
        let manifest = &prepared.package.manifest;
        if manifest.id != prepared.id.as_str() || manifest.version != prepared.version.to_string() {
            tracing::warn!(plugin = %prepared.id, "插件命令快照身份不一致");
            continue;
        }
        for declaration in &manifest.contributes.commands {
            let Some(title) =
                command_logic::localized_title(&prepared.localizations, locale, &declaration.title)
            else {
                tracing::warn!(plugin = %prepared.id, command = %declaration.id, "插件命令翻译不可用");
                continue;
            };
            commands.push(PluginCommand {
                id: PluginCommandId {
                    plugin: prepared.id.as_str().to_owned(),
                    command: declaration.id.clone(),
                },
                title,
                keybinding: declaration.keybinding.clone(),
            });
        }
    }
    commands.sort_by(|left, right| left.id.cmp(&right.id));
    commands.dedup_by(|left, right| left.id == right.id);
    commands
}

/// 使用真实 GPUI parser 编译插件 Action，冲突绑定保留诊断并让位给已有用户键位。
/// 返回的绑定须并入现有 Root keymap，不能安装到先于终端输入的全局 GPUI keymap。
pub(crate) fn plugin_shortcut_bindings(
    commands: &[PluginCommand],
    reserved: &[KeyBinding],
) -> (Vec<KeyBinding>, Vec<PluginShortcutIssue>) {
    let mut bindings = Vec::new();
    let mut issues = Vec::new();
    let mut occupied: Vec<String> = reserved
        .iter()
        .map(canonical_binding)
        .filter(|value| !value.is_empty())
        .collect();
    for command in commands {
        let Some(source) = command
            .keybinding
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        let issue = |reason| PluginShortcutIssue {
            id: command.id.clone(),
            binding: source.to_owned(),
            reason,
        };
        let predicate = match KeyBindingContextPredicate::parse("KerminalApp && !KerminalTextInput")
        {
            Ok(predicate) => Rc::new(predicate),
            Err(_) => {
                issues.push(issue("invalid_context"));
                continue;
            }
        };
        let normalized = crate::keybindings::normalize_legacy_binding(source);
        let action = PluginCommandAction {
            id: command.id.clone(),
        };
        let binding = match KeyBinding::load(
            &normalized,
            Box::new(action),
            Some(predicate),
            false,
            None,
            &DummyKeyboardMapper,
        ) {
            Ok(binding) if !binding.keystrokes().is_empty() => binding,
            _ => {
                issues.push(issue("invalid_binding"));
                continue;
            }
        };
        let canonical = canonical_binding(&binding);
        if occupied
            .iter()
            .any(|other| command_logic::sequences_overlap(other, &canonical))
        {
            issues.push(issue("conflict"));
            continue;
        }
        occupied.push(canonical);
        bindings.push(binding);
    }
    (bindings, issues)
}

/// 使用 GPUI 实际解析结果比较组合键，而不是把别名或大小写不同的键位当成不同绑定。
fn canonical_binding(binding: &KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|stroke| stroke.unparse())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 从当前语言中选择插件资源使用的 locale，英文仍允许回退到清单要求的 en。
pub(crate) fn plugin_command_locale() -> &'static str {
    match crate::locale::current_language() {
        crate::locale::UiLanguage::ZhCn => "zh-CN",
        crate::locale::UiLanguage::EnUs => "en-US",
    }
}

/// 仅从明确的目标类型取主机 ID，不把本地配置、路径和凭据复制到事件或 RPC。
fn target_host_id(target: &WorkspaceTargetRef) -> Option<String> {
    match target {
        WorkspaceTargetRef::Ssh { host_id }
        | WorkspaceTargetRef::Telnet { host_id }
        | WorkspaceTargetRef::Serial { host_id }
        | WorkspaceTargetRef::DockerContainer { host_id, .. } => Some(host_id.as_str().to_owned()),
        WorkspaceTargetRef::Local { .. } | WorkspaceTargetRef::Unavailable { .. } => None,
    }
}

impl RootView {
    /// 设置快照替换后重建插件绑定；冲突只禁用对应快捷键，命令仍可从面板执行。
    pub(super) fn install_plugin_command_bindings(&mut self) {
        let commands = plugin_commands(&self.services.external_plugins, plugin_command_locale());
        let issues = self.keybinding_runtime.install_plugin_commands(&commands);
        for issue in &issues {
            tracing::warn!(plugin = %issue.id.plugin, command = %issue.id.command, binding = %issue.binding, reason = issue.reason, "插件快捷键未安装");
        }
        if !issues.is_empty() {
            self.status = Some(crate::locale::text(
                "部分插件快捷键无效或与已有快捷键冲突，可从命令面板执行。",
                "Some plugin shortcuts are invalid or conflict with existing shortcuts. Use the command palette.",
            ));
        }
    }

    /// 快捷键在触发时固定目标，命令面板则使用打开时已经捕获的 invocation。
    pub(super) fn run_external_plugin_command(
        &mut self,
        id: PluginCommandId,
        cx: &mut Context<Self>,
    ) {
        let context = self.capture_plugin_command_context();
        self.run_plugin_command(PluginCommandInvocation { id, context }, cx);
    }

    /// 文件宿主取冻结 target，终端取明确焦点 pane；不借侧栏或其它 pane 补全插件上下文。
    /// @author kongweiguang
    pub(crate) fn capture_plugin_command_context(&self) -> PluginCommandContext {
        let tab_id = self.visible_workspace_tab_id();
        let host = tab_id.as_ref().and_then(|tab_id| {
            let snapshot = self.workspace_controller.as_ref()?.snapshot();
            let tab = snapshot
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == tab_id)?;
            match tab {
                WorkspaceSessionTab::Terminal { layout, .. } => {
                    let pane_id = snapshot
                        .focused_pane_id
                        .as_ref()
                        .filter(|id| layout.contains_pane(id))?;
                    let pane = snapshot
                        .terminal_panes
                        .iter()
                        .find(|pane| &pane.id == pane_id)?;
                    let host = target_host_id(pane.target.as_ref()?)?;
                    if pane
                        .remote_host_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() != host)
                    {
                        None
                    } else {
                        Some(host)
                    }
                }
                WorkspaceSessionTab::FileWorkspace { target, .. }
                | WorkspaceSessionTab::WorkspaceFile { target, .. } => target_host_id(target),
                WorkspaceSessionTab::SftpTransfer { .. }
                | WorkspaceSessionTab::Extension { .. }
                | WorkspaceSessionTab::Unsupported { .. } => None,
            }
        });
        PluginCommandContext {
            tab: tab_id.map(|id| id.as_str().to_owned()),
            host,
        }
    }

    /// 只执行仍属于冻结目录的命令；异步请求携带触发时已固定的插件、命令和上下文。
    pub(crate) fn run_plugin_command(
        &mut self,
        invocation: PluginCommandInvocation,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested {
            return;
        }
        let id = invocation.id.clone();
        let declared = self.services.external_plugins.iter().any(|plugin| {
            plugin.id.as_str() == id.plugin
                && plugin.package.manifest.id == id.plugin
                && plugin
                    .package
                    .manifest
                    .contributes
                    .commands
                    .iter()
                    .any(|command| command.id == id.command)
        });
        let Some(runtime) = self
            .services
            .external_plugin_runtime
            .clone()
            .filter(|_| declared)
        else {
            self.status = Some(crate::locale::text(
                "插件命令不可用，请检查插件状态。",
                "The plugin command is unavailable. Check the plugin status.",
            ));
            cx.notify();
            return;
        };
        let Ok(plugin_id) = PluginId::parse(&id.plugin) else {
            self.status = Some(crate::locale::text(
                "插件命令身份无效。",
                "The plugin command identity is invalid.",
            ));
            cx.notify();
            return;
        };
        let cancellation = CancellationToken::new();
        let Some(sequence) = self
            .plugin_commands
            .commands
            .begin(id.clone(), cancellation.clone().drop_guard())
        else {
            self.status = Some(crate::locale::text(
                "该插件命令正在执行。",
                "This plugin command is already running.",
            ));
            cx.notify();
            return;
        };
        let mut context = Map::new();
        if let Some(tab) = invocation.context.tab {
            context.insert("tab".into(), Value::String(tab));
        }
        if let Some(host) = invocation.context.host {
            context.insert("host".into(), Value::String(host));
        }
        let params = json!({"command": id.command, "context": context});
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let executor = self.services.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |token| async move {
                    if token.is_cancelled() {
                        return Err(AppError::cancelled("插件命令已取消"));
                    }
                    let response = runtime
                        .call_plugin(plugin_id, "command.run".to_owned(), params, deadline, token)
                        .await?;
                    if response.is_object() {
                        Ok(())
                    } else {
                        Err(AppError::invalid("插件命令返回值无效"))
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.plugin_commands.commands.finish(&id, sequence)
                    || root.host_shutdown_requested
                {
                    return;
                }
                if let Err(error) = result
                    && error.public_code() != PublicErrorCode::Cancelled
                {
                    root.status = Some(crate::locale::text(
                        format!("插件命令执行失败：{}", error.public_message()),
                        format!("The plugin command failed: {}", error.public_message()),
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 实际标签切换提交后入队；去重和有界队列防止重复通知及无界积压。
    pub(super) fn publish_plugin_tab_changed(&mut self, cx: &mut Context<Self>) {
        if self.host_shutdown_requested || self.services.plugin_events.is_none() {
            return;
        }
        let tab = self
            .visible_workspace_tab_id()
            .map(|id| id.as_str().to_owned());
        match self.plugin_commands.events.enqueue(
            tab,
            Instant::now() + EVENT_TIMEOUT,
            MAX_PENDING_EVENTS,
        ) {
            command_logic::EnqueueResult::Unchanged => return,
            command_logic::EnqueueResult::Full => {
                self.status = Some(crate::locale::text(
                    "插件标签事件队列已满。",
                    "The plugin tab event queue is full.",
                ));
                cx.notify();
                return;
            }
            command_logic::EnqueueResult::Queued => {}
        }
        self.start_next_plugin_tab_event(cx);
    }

    /// 同一窗口按提交顺序发送事件；每次调用都有独立截止时间和窗口拥有的取消令牌。
    fn start_next_plugin_tab_event(&mut self, cx: &mut Context<Self>) {
        if self.host_shutdown_requested {
            return;
        }
        let Some(events) = self.services.plugin_events.clone() else {
            return;
        };
        let cancellation = CancellationToken::new();
        let (sequence, tab, deadline) = match self
            .plugin_commands
            .events
            .begin(cancellation.clone().drop_guard())
        {
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(()) => {
                self.status = Some(crate::locale::text(
                    "插件标签事件序号已耗尽。",
                    "The plugin tab event sequence is exhausted.",
                ));
                cx.notify();
                return;
            }
        };
        let executor = self.services.executor.clone();
        cx.spawn(async move |this, cx| {
            let result: AppResult<()> = executor
                .run_cancellable(cancellation, move |token| async move {
                    if token.is_cancelled() {
                        return Err(AppError::cancelled("插件标签事件已取消"));
                    }
                    events
                        .publish_cancellable("tab.changed", json!({"tab": tab}), deadline, token)
                        .await
                        .map(|_| ())
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.plugin_commands.events.finish(sequence) {
                    return;
                }
                if root.host_shutdown_requested {
                    return;
                }
                if let Err(error) = result
                    && error.public_code() != PublicErrorCode::Cancelled
                {
                    root.status = Some(crate::locale::text(
                        "插件标签事件发送失败。",
                        "The plugin tab event could not be delivered.",
                    ));
                    cx.notify();
                }
                root.start_next_plugin_tab_event(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_plugin_commands.rs"]
mod tests;
