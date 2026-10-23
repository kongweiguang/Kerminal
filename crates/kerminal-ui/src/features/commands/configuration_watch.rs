//! 监听外部文件化配置修改并保护命令面板中的未保存草稿。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::configuration_changes::{
    ConfigurationChangeMessage, ConfigurationChangeSubscription,
};
use kerminal_domain::commands::{
    SnippetCatalogOrigin, SnippetContextBinding, SnippetContextBindingKind, SnippetDefaultAction,
    SnippetDefinition, SnippetPreferenceKey, SnippetRisk, SnippetScope,
};

use super::{CommandsSection, CommandsView, SnippetDraftValues, command_message};

/// 保存解析前的片段表单值，用来识别新建入口的真实空白基线。
///
/// 空白标题和模板在保存时是非法输入，但它们仍是可编辑表单的正常初始状态；
/// dirty 检查因此需要先判断原始表单是否仍等于初始化值，不能依赖保存校验结果。
///
/// @author kongweiguang
struct SnippetDraftSnapshot {
    title: String,
    command: String,
    description: String,
    tags: String,
    category: String,
    sort_order: String,
    scope: SnippetScope,
    risk: SnippetRisk,
    default_action: SnippetDefaultAction,
    variable_count: usize,
    context_bindings: Vec<(SnippetContextBinding, String)>,
}

/// 外部文件变化先形成精确决策；重载不主动打开或关闭根级编辑器。
#[derive(Clone, Debug, Eq, PartialEq)]
enum SnippetConfigurationRefreshAction {
    Ignore,
    PreserveDirtyDraft,
    ReloadExact {
        key: SnippetPreferenceKey,
        reopen_editor: bool,
    },
}

/// 空 binding 与编辑器自动呈现的单个 Global binding 语义相同，不应单独触发脏稿确认。
///
/// @author kongweiguang
fn snippet_context_bindings_match_draft(
    draft: &[SnippetContextBinding],
    saved: &[SnippetContextBinding],
) -> bool {
    let implicit_global = [SnippetContextBinding {
        kind: SnippetContextBindingKind::Global,
        target_id: None,
    }];
    (draft == saved)
        || (draft.is_empty() && saved == implicit_global.as_slice())
        || (saved.is_empty() && draft == implicit_global.as_slice())
}

/// Fill 和外部变更检测共用定义字段比较；运行变量值不属于可保存定义。
///
/// @author kongweiguang
pub(super) fn snippet_definition_draft_matches_saved(
    saved: &SnippetDefinition,
    values: &SnippetDraftValues,
) -> bool {
    values.title == saved.title
        && values.command == saved.command
        && values.description == saved.description
        && values.tags == saved.tags
        && values.sort_order == saved.sort_order
        && values.scope == saved.scope
        && values.category == saved.category
        && values.risk == saved.risk
        && values.default_action == saved.default_action
        && values.variables == saved.variables
        && snippet_context_bindings_match_draft(&values.context_bindings, &saved.context_bindings)
}

/// 用户与内置目录允许同 ID；当前定义是否带文件 revision 决定唯一来源。
///
/// @author kongweiguang
fn selected_snippet_preference_key(
    snippet_id: &str,
    has_user_revision: bool,
) -> SnippetPreferenceKey {
    SnippetPreferenceKey {
        origin: if has_user_revision {
            SnippetCatalogOrigin::User
        } else {
            SnippetCatalogOrigin::Builtin
        },
        snippet_id: snippet_id.to_owned(),
    }
}

/// 只重载受影响的用户文件；脏稿作为冲突保留，内置同 ID 不得被用户文件事件替换。
///
/// @author kongweiguang
fn snippet_configuration_refresh_action(
    key: SnippetPreferenceKey,
    paths: &[String],
    full_rescan: bool,
    dirty: bool,
) -> SnippetConfigurationRefreshAction {
    if key.origin != SnippetCatalogOrigin::User {
        return SnippetConfigurationRefreshAction::Ignore;
    }
    let path = format!("snippets/{}.toml", key.snippet_id);
    if !full_rescan && !paths.iter().any(|changed| changed == &path) {
        return SnippetConfigurationRefreshAction::Ignore;
    }
    if dirty {
        SnippetConfigurationRefreshAction::PreserveDirtyDraft
    } else {
        SnippetConfigurationRefreshAction::ReloadExact {
            key,
            // 保留编辑器原有可见状态；外部变化不能重新弹出用户已关闭的 modal。
            reopen_editor: false,
        }
    }
}

impl SnippetDraftSnapshot {
    /// 区分构造时的空表单与新建时写入的默认排序、Global 绑定，不掩盖任何用户输入。
    ///
    /// @author kongweiguang
    fn matches_empty_baseline(&self, is_new: bool) -> bool {
        let context_bindings_match = match (is_new, self.context_bindings.as_slice()) {
            (false, []) => true,
            (true, [(binding, target)]) => {
                binding.kind == SnippetContextBindingKind::Global
                    && binding.target_id.is_none()
                    && target.is_empty()
            }
            _ => false,
        };

        self.title.is_empty()
            && self.command.is_empty()
            && self.description.is_empty()
            && self.tags.is_empty()
            && self.category.is_empty()
            && self.sort_order == if is_new { "0" } else { "" }
            && self.scope == SnippetScope::Any
            && self.risk == SnippetRisk::Unknown
            && self.default_action == SnippetDefaultAction::Insert
            && self.variable_count == 0
            && context_bindings_match
    }
}

impl CommandsView {
    /// 使用 tracked executor 等待共享 watcher，页面或应用关闭时协作结束订阅。
    pub(super) fn watch_configuration_changes(
        &mut self,
        subscription: ConfigurationChangeSubscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let executor = self.executor.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut subscription = subscription;
            loop {
                let result = executor
                    .run(move |cancellation| async move {
                        let message = subscription.recv(cancellation).await;
                        Ok((subscription, message))
                    })
                    .await;
                let (next_subscription, message) = match result {
                    Ok(result) => result,
                    Err(_) => break,
                };
                subscription = next_subscription;
                if matches!(
                    &message,
                    ConfigurationChangeMessage::Cancelled | ConfigurationChangeMessage::Closed
                ) {
                    break;
                }
                let _ = cx.update(|window, app| {
                    let _ = this.update(app, |view, cx| {
                        view.apply_configuration_change(message, window, cx);
                    });
                });
            }
        })
        .detach();
    }

    /// 只响应片段和工作流配置变化；脏稿保持原文并继续受旧 revision 的 CAS 保护。
    ///
    /// @author kongweiguang
    fn apply_configuration_change(
        &mut self,
        message: ConfigurationChangeMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (paths, full_rescan) = match message {
            ConfigurationChangeMessage::Changed(event) => {
                (event.relative_paths, event.full_rescan_required)
            }
            ConfigurationChangeMessage::RescanRequired => (Vec::new(), true),
            ConfigurationChangeMessage::Cancelled | ConfigurationChangeMessage::Closed => return,
        };
        let paths = paths
            .into_iter()
            .map(|path| path.replace('\\', "/"))
            .collect::<Vec<_>>();
        let snippets_changed =
            full_rescan || paths.iter().any(|path| path.starts_with("snippets/"));
        let workflows_changed =
            full_rescan || paths.iter().any(|path| path.starts_with("workflows/"));
        if !snippets_changed && !workflows_changed {
            return;
        }

        self.reload_panel(cx);
        match self.section {
            CommandsSection::Snippets if snippets_changed => {
                let Some(key) = self.selected_snippet.as_ref().map(|selected| {
                    selected_snippet_preference_key(
                        &selected.definition.id,
                        selected.revision.is_some(),
                    )
                }) else {
                    return;
                };
                let is_dirty = !self.snippet_draft_matches_saved(cx);
                match snippet_configuration_refresh_action(key, &paths, full_rescan, is_dirty) {
                    SnippetConfigurationRefreshAction::Ignore => {}
                    SnippetConfigurationRefreshAction::PreserveDirtyDraft => {
                        let conflict = command_message(
                            "片段文件已在外部修改；本地草稿已保留，保存时会检查原 revision。",
                            "The snippet changed outside Kerminal. Your draft is preserved and save will check its original revision.",
                        );
                        self.configuration_conflict = Some(conflict.clone());
                        self.status = Some(conflict);
                        cx.notify();
                    }
                    SnippetConfigurationRefreshAction::ReloadExact { key, reopen_editor } => {
                        self.configuration_conflict = None;
                        // 同来源精确重读；false 保留关闭态，也不改变已打开 modal 的可见状态。
                        self.load_snippet_key(key, reopen_editor, false, window, cx);
                    }
                }
            }
            CommandsSection::Workflows if workflows_changed => {
                let Some(id) = self
                    .selected_workflow
                    .as_ref()
                    .map(|selected| selected.definition.id.clone())
                else {
                    return;
                };
                let changed = full_rescan
                    || paths
                        .iter()
                        .any(|path| path == &format!("workflows/{id}.toml"));
                if !changed {
                    return;
                }
                let is_dirty = !self.workflow_draft_matches_saved(cx);
                if is_dirty {
                    let conflict = command_message(
                        "工作流文件已在外部修改；本地草稿已保留，保存时会检查原 revision。",
                        "The workflow changed outside Kerminal. Your draft is preserved and save will check its original revision.",
                    );
                    self.configuration_conflict = Some(conflict.clone());
                    self.status = Some(conflict);
                    cx.notify();
                    return;
                }
                self.configuration_conflict = None;
                self.open_workflow(id, window, cx);
            }
            _ => {}
        }
    }

    /// 比较完整片段草稿和打开时快照，检测标题、脚本及变量定义的本地修改。
    pub(super) fn snippet_draft_matches_saved(&self, cx: &mut Context<Self>) -> bool {
        let Some(saved) = self.selected_snippet.as_ref() else {
            return false;
        };
        let Ok(values) = self.collect_snippet_draft_values(cx) else {
            return false;
        };
        let runtime_values_unchanged = self.snippet_variables.iter().all(|field| {
            let value = field.input.update(cx, |input, _| input.value().to_string());
            value == field.variable.default_value.as_deref().unwrap_or_default()
        });
        snippet_definition_draft_matches_saved(&saved.definition, &values)
            && runtime_values_unchanged
    }

    /// 运行时变量值也属于会被切换丢弃的草稿，即使定义本身尚未发生修改。
    ///
    /// @author kongweiguang
    pub(super) fn snippet_draft_is_dirty(&self, cx: &mut Context<Self>) -> bool {
        if self.selected_snippet.is_some() {
            return !self.snippet_draft_matches_saved(cx);
        }
        let snapshot = self.snippet_draft_snapshot(cx);
        if snapshot.matches_empty_baseline(self.snippet_is_new) {
            return false;
        }
        let Ok(values) = self.collect_snippet_draft_values(cx) else {
            return true;
        };
        let default_binding = SnippetContextBinding {
            kind: SnippetContextBindingKind::Global,
            target_id: None,
        };
        let values_changed = !values.title.is_empty()
            || !values.command.is_empty()
            || values.description.is_some()
            || !values.tags.is_empty()
            || values.sort_order != 0
            || values.scope != SnippetScope::Any
            || values.category.is_some()
            || values.risk != SnippetRisk::Unknown
            || values.default_action != SnippetDefaultAction::Insert
            || !values.variables.is_empty()
            || (!values.context_bindings.is_empty()
                && values.context_bindings.as_slice() != std::slice::from_ref(&default_binding));
        let runtime_value_changed = self.snippet_variables.iter().any(|field| {
            !field
                .input
                .update(cx, |input, _| input.value().to_string())
                .is_empty()
        });
        values_changed || runtime_value_changed
    }

    /// 读取未规范化字段，因为空白新草稿在标题/命令校验阶段本来就无法生成保存 DTO。
    ///
    /// @author kongweiguang
    fn snippet_draft_snapshot(&self, cx: &mut Context<Self>) -> SnippetDraftSnapshot {
        SnippetDraftSnapshot {
            title: self
                .snippet_title
                .update(cx, |input, _| input.value().to_string()),
            command: self
                .snippet_command
                .update(cx, |input, _| input.text().to_string()),
            description: self
                .snippet_description
                .update(cx, |input, _| input.value().to_string()),
            tags: self
                .snippet_tags
                .update(cx, |input, _| input.value().to_string()),
            category: self
                .snippet_category
                .update(cx, |input, _| input.value().to_string()),
            sort_order: self
                .snippet_sort_order
                .update(cx, |input, _| input.value().to_string()),
            scope: self.snippet_scope,
            risk: self.snippet_risk,
            default_action: self.snippet_default_action,
            variable_count: self.snippet_variables.len(),
            context_bindings: self
                .snippet_context_bindings
                .iter()
                .map(|binding| {
                    (
                        binding.binding.clone(),
                        binding
                            .target_id
                            .update(cx, |input, _| input.value().to_string()),
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/commands_snippet_configuration_watch.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/commands_snippet_draft.rs"]
mod draft_tests;
