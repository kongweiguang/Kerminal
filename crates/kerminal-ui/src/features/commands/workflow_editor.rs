//! 工作流定义编辑与多行脚本往返。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Window,
    component::input::{InputState, TextareaState},
};
use kerminal_application::{AppError, commands::VersionedCommandDefinition};
use kerminal_domain::commands::{
    CommandWorkflow, CommandWorkflowStep, WorkflowRunState, WorkflowScope,
};

use super::{CommandsView, EditorTransition, WorkflowStepEditor};

impl CommandsView {
    /// 紧凑面板先进入同一编辑草稿的详情层；同一新稿重复点击不制造隐式切换。
    ///
    /// @author kongweiguang
    pub(super) fn new_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workflow_is_new && self.selected_workflow.is_none() {
            self.show_compact_detail(super::CommandsSection::Workflows, cx);
            return;
        }
        self.show_compact_detail(super::CommandsSection::Workflows, cx);
        self.request_workflow_transition(EditorTransition::New, window, cx);
    }

    /// 工作流草稿与运行共用一个面板，先通过统一锁和保存决策再切换编辑对象。
    /// 新建说明由编辑器内联呈现，不写跨 section 的共享状态，以免串页或清除真实结果。
    ///
    /// @author kongweiguang
    fn install_new_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workflow_editor_generation = self.workflow_editor_generation.wrapping_add(1).max(1);
        self.selected_workflow = None;
        self.configuration_conflict = None;
        self.workflow_is_new = true;
        self.workflow_run = None;
        self.workflow_progress = None;
        self.workflow_binding = None;
        self.workflow_session_info = None;
        self.workflow_attempts.clear();
        self.workflow_control = None;
        self.workflow_running = false;
        self.pending_workflow_transition = None;
        self.confirm_workflow_delete = false;
        self.workflow_title
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.workflow_steps = vec![new_step_editor(
            CommandWorkflowStep {
                id: self.commands.new_definition_id(),
                title: super::command_message("步骤 1", "Step 1"),
                command: String::new(),
                description: None,
                scope: None,
                requires_confirmation: false,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
            },
            window,
            cx,
        )];
        cx.notify();
    }

    /// 判断当前运行是否仍持有待确认或执行中的步骤，避免确认窗口被编辑动作覆盖。
    pub(super) fn workflow_run_blocks_editor(&self) -> bool {
        self.workflow_running
            || self.workflow_run.as_ref().is_some_and(|run| {
                matches!(
                    run.state(),
                    WorkflowRunState::Running | WorkflowRunState::AwaitingConfirmation
                )
            })
    }

    /// 只有启动时捕获的同一 owner 与 generation 仍是活动目标时才可继续待确认运行。
    pub(super) fn workflow_binding_is_current(&self) -> bool {
        self.workflow_binding.as_ref().is_some_and(|binding| {
            self.session_id.as_ref() == Some(&binding.session_id)
                && self.ui_session_binding.as_ref() == Some(binding)
        })
    }

    /// 统一计算表单锁；待确认、异步读写、删除和脏稿决策期间都不能变更执行快照。
    pub(super) fn workflow_editor_locked(&self) -> bool {
        self.workflow_run_blocks_editor()
            || self.workflow_saving
            || self.workflow_loading
            || self.workflow_deleting
            || self.pending_workflow_transition.is_some()
            || self.confirm_workflow_delete
    }

    /// 脏稿切换统一保存、丢弃或取消，避免新建、打开与删除走出不同保护路径。
    pub(super) fn request_workflow_transition(
        &mut self,
        transition: EditorTransition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workflow_run_blocks_editor()
            || self.workflow_saving
            || self.workflow_loading
            || self.workflow_deleting
            || self.pending_workflow_transition.is_some()
            || self.confirm_workflow_delete
        {
            self.status = Some(super::command_message(
                "请先完成或取消当前工作流运行与编辑操作",
                "Finish or cancel the current workflow run or editor operation first",
            ));
            cx.notify();
            return;
        }
        if matches!(transition, EditorTransition::Delete) && self.selected_workflow.is_none() {
            self.status = Some(super::command_message(
                "请选择一个已保存的工作流",
                "Select a saved workflow first",
            ));
            cx.notify();
            return;
        }
        if matches!(transition, EditorTransition::Delete)
            && self
                .selected_workflow
                .as_ref()
                .is_some_and(|saved| saved.revision.is_none())
        {
            self.status = Some(super::command_message(
                "工作流缺少 revision，已阻止不安全删除",
                "Workflow has no revision; unsafe deletion was blocked",
            ));
            cx.notify();
            return;
        }
        if self.workflow_draft_is_dirty(cx) {
            self.pending_workflow_transition = Some(transition);
            self.status = Some(super::command_message(
                "工作流草稿尚未保存，请选择保存、丢弃或取消切换",
                "The workflow draft has unsaved changes. Save, discard, or cancel the switch",
            ));
            cx.notify();
            return;
        }
        self.apply_workflow_transition(transition, window, cx);
    }

    /// 只在显式丢弃或保存完成后应用编辑意图；片段导航不转成工作流 ID，删除仍需二次确认。
    ///
    /// @author kongweiguang
    fn apply_workflow_transition(
        &mut self,
        transition: EditorTransition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match transition {
            EditorTransition::New => self.install_new_workflow(window, cx),
            EditorTransition::Open(id) => self.load_workflow(id, window, cx),
            EditorTransition::OpenKey(_) | EditorTransition::PreviewKey(_) => {
                // 片段的来源 key 不属于工作流导航，防止共享 intent 被错误转成工作流 ID 查找。
                self.status = Some(super::command_message(
                    "片段导航不能应用于工作流",
                    "A snippet navigation request cannot be applied to a workflow",
                ));
                cx.notify();
            }
            EditorTransition::Delete => {
                self.confirm_workflow_delete = true;
                self.status = Some(super::command_message(
                    "草稿已保存或丢弃；请再次确认删除工作流",
                    "The draft is saved or discarded. Confirm deletion of the workflow",
                ));
                cx.notify();
            }
        }
    }

    /// 丢弃只移除当前编辑草稿，随后继续已暂存的 typed 导航意图。
    pub(super) fn discard_workflow_transition(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(transition) = self.pending_workflow_transition.take() else {
            return;
        };
        if matches!(transition, EditorTransition::Delete)
            && let Some(saved) = self.selected_workflow.clone()
        {
            self.install_workflow(saved, window, cx);
        }
        self.apply_workflow_transition(transition, window, cx);
    }

    /// 取消导航保留当前字段和 revision，用户可以继续编辑而不丢失输入。
    pub(super) fn cancel_workflow_transition(&mut self, cx: &mut Context<Self>) {
        self.pending_workflow_transition = None;
        self.status = None;
        cx.notify();
    }

    /// 已保存项按唯一 ID 异步读取；请求代次保证较早响应不会覆盖后来编辑器。
    fn load_workflow(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.workflow_editor_generation = self.workflow_editor_generation.wrapping_add(1).max(1);
        let generation = self.workflow_editor_generation;
        let service = self.commands.clone();
        let executor = self.executor.clone();
        self.workflow_loading = true;
        self.status = Some(super::command_message(
            "正在读取工作流…",
            "Loading workflow…",
        ));
        cx.spawn_in(window, async move |this, cx| {
            let loaded = executor
                .run(move |cancel| async move {
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(super::command_error_context(
                            "命令工作流读取已取消",
                            "Workflow loading was cancelled",
                        )));
                    }
                    service.get_workflow_versioned(id).await?.ok_or_else(|| {
                        AppError::not_found(super::command_error_context(
                            "命令工作流定义不存在",
                            "Workflow definition no longer exists",
                        ))
                    })
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    if view.workflow_editor_generation != generation {
                        return;
                    }
                    view.workflow_loading = false;
                    match loaded {
                        Ok(saved) => view.install_workflow(saved, window, cx),
                        Err(error) => {
                            view.status = Some(error.public_message().to_owned());
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
    }

    /// 追加步骤时沿用领域 ID，并限制工作流聚合的最大步骤数。
    pub(super) fn add_workflow_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workflow_editor_locked() || self.workflow_steps.len() >= 40 {
            return;
        }
        let sort_order = self.workflow_steps.len() as i64;
        let definition = CommandWorkflowStep {
            id: self.commands.new_definition_id(),
            title: super::command_message(
                format!("步骤 {}", sort_order + 1),
                format!("Step {}", sort_order + 1),
            ),
            command: String::new(),
            description: None,
            scope: None,
            requires_confirmation: false,
            sort_order,
            created_at: String::new(),
            updated_at: String::new(),
        };
        self.workflow_steps
            .push(new_step_editor(definition, window, cx));
        cx.notify();
    }

    /// 删除步骤时保留至少一个空步骤，避免 UI 创建无法修复的空工作流。
    pub(super) fn remove_workflow_step(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.workflow_editor_locked()
            || self.workflow_steps.len() <= 1
            || index >= self.workflow_steps.len()
        {
            return;
        }
        self.workflow_steps.remove(index);
        refresh_step_order(&mut self.workflow_steps);
        cx.notify();
    }

    /// 调整步骤顺序只移动编辑实体，用户正在输入的多行脚本不会被重新解析。
    pub(super) fn move_workflow_step(
        &mut self,
        index: usize,
        offset: isize,
        cx: &mut Context<Self>,
    ) {
        if self.workflow_editor_locked() || index >= self.workflow_steps.len() {
            return;
        }
        let destination = index as isize + offset;
        if !(0..self.workflow_steps.len() as isize).contains(&destination) {
            return;
        }
        self.workflow_steps.swap(index, destination as usize);
        refresh_step_order(&mut self.workflow_steps);
        cx.notify();
    }

    /// 执行前确认作为步骤定义属性保存，风险操作的交互策略不依赖命令文本约定。
    pub(super) fn toggle_workflow_step_confirmation(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if self.workflow_editor_locked() {
            return;
        }
        let Some(step) = self.workflow_steps.get_mut(index) else {
            return;
        };
        step.definition.requires_confirmation = !step.definition.requires_confirmation;
        cx.notify();
    }

    /// 按 ID 请求打开工作流并切入详情层；异步读取仍由编辑器代次保护。
    ///
    /// @author kongweiguang
    pub(super) fn open_workflow(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_compact_detail(super::CommandsSection::Workflows, cx);
        if self
            .selected_workflow
            .as_ref()
            .is_some_and(|saved| saved.definition.id == id)
        {
            return;
        }
        self.request_workflow_transition(EditorTransition::Open(id), window, cx);
    }

    /// 收集独立标题和多行输入，保留脚本字节及步骤元数据并统一校验大小边界。
    pub(super) fn collect_workflow_steps(
        &self,
        cx: &mut Context<Self>,
    ) -> Result<Vec<CommandWorkflowStep>, String> {
        if self.workflow_steps.is_empty() {
            return Err(super::command_message(
                "工作流至少需要一个步骤",
                "A workflow must have at least one step",
            ));
        }
        if self.workflow_steps.len() > 40 {
            return Err(super::command_message(
                "工作流最多支持 40 个步骤",
                "A workflow can have at most 40 steps",
            ));
        }
        self.workflow_steps
            .iter()
            .enumerate()
            .map(|(index, editor)| {
                let title = editor
                    .title
                    .update(cx, |input, _| input.value().to_string());
                let command = editor
                    .command
                    .update(cx, |input, _| input.text().to_string());
                let title = title.trim();
                if title.is_empty() || title.chars().count() > 80 {
                    return Err(super::command_message(
                        "步骤标题不能为空，且最多 80 个字符",
                        "Step titles must contain 1 to 80 characters",
                    ));
                }
                if command.trim().is_empty() {
                    return Err(super::command_message(
                        "步骤命令不能为空",
                        "Step script cannot be empty",
                    ));
                }
                if command.chars().count() > 8_000 || command.len() > 32_000 {
                    return Err(super::command_message(
                        "单步脚本最多 8,000 个字符且最多 32,000 个 UTF-8 字节",
                        "Each script is limited to 8,000 characters and 32,000 UTF-8 bytes",
                    ));
                }
                if command
                    .chars()
                    .any(|character| matches!(character, '\r' | '\0' | '\u{1b}'))
                {
                    return Err(super::command_message(
                        "脚本不能包含 CR、NUL 或 ESC 控制字符",
                        "Scripts cannot contain CR, NUL, or ESC control characters",
                    ));
                }
                let mut definition = editor.definition.clone();
                definition.title = title.to_owned();
                definition.command = command;
                definition.sort_order = index as i64;
                Ok(definition)
            })
            .collect()
    }

    /// 运行按钮只允许执行已保存快照，防止编辑中的内容与用户确认的定义不一致。
    pub(super) fn workflow_draft_matches_saved(&self, cx: &mut Context<Self>) -> bool {
        let Some(saved) = self.selected_workflow.as_ref() else {
            return false;
        };
        let title = self
            .workflow_title
            .update(cx, |input, _| input.value().to_string());
        if title.trim() != saved.definition.title.as_str() {
            return false;
        }
        let Ok(steps) = self.collect_workflow_steps(cx) else {
            return false;
        };
        steps.len() == saved.definition.steps.len()
            && steps
                .iter()
                .zip(&saved.definition.steps)
                .all(|(draft, stored)| same_workflow_step_content(draft, stored))
    }

    /// 判断已保存稿或新建空白模板是否被改动，供打开、新建和删除共用确认语义。
    pub(super) fn workflow_draft_is_dirty(&self, cx: &mut Context<Self>) -> bool {
        if self.selected_workflow.is_some() {
            return !self.workflow_draft_matches_saved(cx);
        }
        if !self.workflow_is_new {
            return false;
        }
        let title_is_set = self
            .workflow_title
            .update(cx, |input, _| !input.value().trim().is_empty());
        if title_is_set || self.workflow_steps.len() != 1 {
            return true;
        }
        let step = &self.workflow_steps[0];
        let title = step.title.update(cx, |input, _| input.value().to_string());
        let command = step.command.update(cx, |input, _| input.text().to_string());
        !matches!(title.as_str(), "步骤 1" | "Step 1")
            || !command.is_empty()
            || step.definition.requires_confirmation
    }

    /// 删除先复用脏稿过渡，再要求第二次明确确认，避免误删或绕过版本检查。
    pub(super) fn request_workflow_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_workflow_transition(EditorTransition::Delete, window, cx);
    }

    /// 用户在第二次确认后按打开时的 revision 做 CAS 删除，异步回执只更新原编辑器代次。
    pub(super) fn confirm_delete_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirm_workflow_delete
            || self.workflow_run_blocks_editor()
            || self.workflow_saving
            || self.workflow_loading
            || self.workflow_deleting
            || self.pending_workflow_transition.is_some()
        {
            return;
        }
        let Some(saved) = self.selected_workflow.clone() else {
            self.confirm_workflow_delete = false;
            self.status = Some(super::command_message(
                "请选择一个已保存的工作流",
                "Select a saved workflow first",
            ));
            cx.notify();
            return;
        };
        let Some(revision) = saved.revision else {
            self.confirm_workflow_delete = false;
            self.status = Some(super::command_message(
                "工作流缺少 revision，已阻止不安全删除",
                "Workflow has no revision; unsafe deletion was blocked",
            ));
            cx.notify();
            return;
        };

        self.workflow_delete_generation = self.workflow_delete_generation.wrapping_add(1).max(1);
        let delete_generation = self.workflow_delete_generation;
        self.workflow_editor_generation = self.workflow_editor_generation.wrapping_add(1).max(1);
        let editor_generation = self.workflow_editor_generation;
        self.confirm_workflow_delete = false;
        self.workflow_deleting = true;
        self.status = Some(super::command_message(
            "正在按当前 revision 删除工作流…",
            "Deleting workflow with its current revision…",
        ));
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let id = saved.definition.id;
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(super::command_error_context(
                            "命令工作流删除已取消",
                            "Workflow deletion was cancelled",
                        )));
                    }
                    service.delete_workflow_versioned(id, revision).await
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    if view.workflow_editor_generation != editor_generation
                        || view.workflow_delete_generation != delete_generation
                    {
                        return;
                    }
                    view.workflow_deleting = false;
                    match result {
                        Ok(true) => {
                            view.install_new_workflow(window, cx);
                            view.status =
                                Some(super::command_message("工作流已删除", "Workflow deleted"));
                            view.reload_panel(cx);
                        }
                        Ok(false) => {
                            view.status = Some(super::command_message(
                                "工作流已不存在；草稿与 revision 已保留",
                                "Workflow no longer exists; the draft and revision were retained",
                            ));
                            cx.notify();
                        }
                        Err(error) => {
                            view.status = Some(error.public_message().to_owned());
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
    }

    /// 关闭删除确认条只取消该意图，不改动选择、草稿或 CAS revision。
    pub(super) fn cancel_workflow_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_workflow_delete = false;
        self.status = None;
        cx.notify();
    }

    /// 保存时通过 revision CAS 更新定义；命令正文原样写入，不拆行、不转义 heredoc。
    pub(super) fn save_workflow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workflow_run_blocks_editor()
            || self.workflow_saving
            || self.workflow_loading
            || self.workflow_deleting
            || self.confirm_workflow_delete
        {
            self.status = Some(super::command_message(
                "工作流执行期间不能保存定义",
                "Workflow definitions cannot be saved while a run is active",
            ));
            cx.notify();
            return;
        }
        let title = self
            .workflow_title
            .update(cx, |input, _| input.value().to_string());
        if title.trim().is_empty() || title.chars().count() > 80 {
            self.pending_workflow_transition = None;
            self.status = Some(super::command_message(
                "工作流名称不能为空，且最多 80 个字符",
                "Workflow names must contain 1 to 80 characters",
            ));
            cx.notify();
            return;
        }
        let steps = match self.collect_workflow_steps(cx) {
            Ok(steps) => steps,
            Err(message) => {
                self.pending_workflow_transition = None;
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        let workflow = self
            .selected_workflow
            .as_ref()
            .map(|saved| saved.definition.clone());
        let (definition, revision) = match workflow {
            Some(mut workflow) => {
                workflow.title = title.trim().to_owned();
                workflow.steps = steps;
                (
                    workflow,
                    self.selected_workflow
                        .as_ref()
                        .and_then(|saved| saved.revision.clone()),
                )
            }
            None => (
                CommandWorkflow {
                    id: self.commands.new_definition_id(),
                    title: title.trim().to_owned(),
                    description: None,
                    tags: Vec::new(),
                    scope: WorkflowScope::Any,
                    steps,
                    sort_order: 0,
                    created_at: String::new(),
                    updated_at: String::new(),
                },
                None,
            ),
        };
        let service = self.commands.clone();
        let executor = self.executor.clone();
        self.workflow_editor_generation = self.workflow_editor_generation.wrapping_add(1).max(1);
        let generation = self.workflow_editor_generation;
        self.workflow_saving = true;
        self.status = Some(super::command_message(
            "正在保存工作流…",
            "Saving workflow…",
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let saved = service
                        .save_workflow_versioned(definition, revision)
                        .await?;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(super::command_error_context(
                            "命令工作流保存已取消",
                            "Workflow save was cancelled",
                        )));
                    }
                    Ok(saved)
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_workflow_save(generation, result, window, cx)
                });
            });
        })
        .detach();
    }

    /// 保存回执只更新匹配代次的 CAS 基线；成功后再执行暂存导航，失败保留草稿。
    fn finish_workflow_save(
        &mut self,
        generation: u64,
        result: Result<VersionedCommandDefinition<CommandWorkflow>, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.workflow_editor_generation {
            return;
        }
        self.workflow_saving = false;
        match result {
            Ok(saved) => {
                let title = saved.definition.title.clone();
                let transition = self.pending_workflow_transition.take();
                self.install_workflow(saved, window, cx);
                self.workflow_is_new = false;
                self.reload_panel(cx);
                if let Some(transition) = transition {
                    self.apply_workflow_transition(transition, window, cx);
                } else {
                    self.status = Some(super::command_message(
                        format!("已保存工作流：{title}"),
                        format!("Saved workflow: {title}"),
                    ));
                }
            }
            Err(error) => {
                self.pending_workflow_transition = None;
                self.status = Some(error.public_message().to_owned());
            }
        }
        cx.notify();
    }

    /// 用已保存步骤重建独立编辑器，保证每条脚本在加载、保存间保持原文。
    pub(super) fn install_workflow(
        &mut self,
        saved: VersionedCommandDefinition<CommandWorkflow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workflow_run_blocks_editor() {
            return;
        }
        self.workflow_editor_generation = self.workflow_editor_generation.wrapping_add(1).max(1);
        self.workflow_saving = false;
        self.workflow_loading = false;
        self.workflow_deleting = false;
        if let Some(control) = self.workflow_control.take() {
            control.cancel();
        }
        self.workflow_generation = self.workflow_generation.wrapping_add(1);
        self.workflow_run = None;
        self.workflow_progress = None;
        self.workflow_binding = None;
        self.workflow_session_info = None;
        self.workflow_attempts.clear();
        self.workflow_running = false;
        self.workflow_title.update(cx, |input, cx| {
            input.set_value(saved.definition.title.clone(), window, cx)
        });
        self.workflow_steps = saved
            .definition
            .steps
            .iter()
            .cloned()
            .map(|step| new_step_editor(step, window, cx))
            .collect();
        self.selected_workflow = Some(saved);
        self.workflow_is_new = false;
        self.pending_workflow_transition = None;
        self.confirm_workflow_delete = false;
        self.configuration_conflict = None;
        self.status = None;
        cx.notify();
    }
}

/// 每个步骤使用 GPUI Kit 的多行状态实体，避免绘制时丢失编辑内容。
fn new_step_editor(
    definition: CommandWorkflowStep,
    window: &mut Window,
    cx: &mut Context<CommandsView>,
) -> WorkflowStepEditor {
    let title_value = definition.title.clone();
    let command_value = definition.command.clone();
    WorkflowStepEditor {
        definition,
        title: cx.new(|cx| InputState::new(window, cx).default_value(title_value)),
        command: cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "完整 shell 脚本；保留换行、引号和 heredoc",
                    "Full shell script; preserves newlines, quotes, and heredocs",
                ))
                .default_value(command_value)
                .auto_grow(4, 16)
        }),
    }
}

/// 删除或移动步骤后立即同步 sort_order，持久化顺序与界面顺序保持一致。
fn refresh_step_order(steps: &mut [WorkflowStepEditor]) {
    for (index, step) in steps.iter_mut().enumerate() {
        step.definition.sort_order = index as i64;
    }
}

/// 比较可编辑脚本与定义元数据，不把持久化排序号或更新时间误判成用户改稿。
fn same_workflow_step_content(left: &CommandWorkflowStep, right: &CommandWorkflowStep) -> bool {
    left.id == right.id
        && left.title == right.title
        && left.command == right.command
        && left.description == right.description
        && left.scope == right.scope
        && left.requires_confirmation == right.requires_confirmation
}
