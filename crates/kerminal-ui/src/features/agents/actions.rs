//! Agent 会话生命周期、显式目标绑定与受限上下文预览用例协调。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    agents::{
        AgentHistoryMode, AgentHistoryRequest, AgentResumeRecoveryStatus, AgentSessionView,
        CreateAgentSessionFromHistoryRequest, CreateAgentSessionRequest,
        RenameAgentSessionTitleRequest,
    },
};
use kerminal_domain::{
    agents::{AgentLauncherKey, AgentSessionScope, AgentTargetBinding},
    identity::{AgentSessionId, SessionId},
};
use tokio_util::sync::CancellationToken;

use super::{AgentLifecycleOperation, AgentPanelEvent, AgentRenameTarget, AgentsPanel};

/// 新建模式只撤销右栏的选中投影；Root 据此隐藏原画布，不关闭 Agent 的终端 owner。
///
/// @author kongweiguang
pub(super) fn new_chat_selection_event() -> AgentPanelEvent {
    AgentPanelEvent::Selected {
        agent_session_id: None,
        terminal_session_id: None,
    }
}

impl AgentsPanel {
    /// 标题不是新建前置项，只在用户主动展开命名设置时展示输入框。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_title_editor(&mut self, cx: &mut Context<Self>) {
        self.title_editor_visible = !self.title_editor_visible;
        cx.notify();
    }

    /// 刷新会话列表；只在同一 Agent 被真实投影为 Exited 时收敛可解释的操作拒绝。
    ///
    /// @author kongweiguang
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        let request = AgentHistoryRequest {
            scope: self.scope.clone(),
            target_candidate: self
                .candidate
                .as_ref()
                .map(|candidate| candidate.binding.clone()),
            mode: self.history_mode,
        };
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move { agents.history(request, cancel).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.generation != generation {
                    return;
                }
                panel.loading = false;
                match result {
                    Ok(projection) => {
                        panel.sessions_verified = true;
                        if panel.selected_agent.as_ref().is_some_and(|id| {
                            !projection
                                .sessions
                                .iter()
                                .any(|view| &view.session.agent_session_id == id)
                        }) {
                            panel.invalidate_context_preview();
                            panel.selected_agent = None;
                        }
                        panel.sessions = projection.sessions;
                        panel.visible_session_ids = projection.visible_session_ids;
                        panel.current_target_count = projection.current_target_count;
                        panel.all_count = projection.all_count;
                        panel.archived_count = projection.archived_count;
                        let confirmed_exit_feedback_id = panel
                            .terminal_feedback
                            .iter()
                            .find(|feedback| {
                                should_clear_terminal_feedback_after_reload(
                                    feedback,
                                    panel.selected_agent.as_ref(),
                                    &panel.sessions,
                                )
                            })
                            .map(|feedback| feedback.agent_session_id.clone());
                        if let Some(agent_id) = confirmed_exit_feedback_id {
                            panel.terminal_feedback.retain(|feedback| {
                                feedback.agent_session_id != agent_id
                                    || !feedback.source.clears_after_confirmed_exit()
                            });
                        }
                        panel.publish_selection(cx);
                    }
                    Err(error) => {
                        panel.sessions_verified = false;
                        panel.status = Some(localized_error(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Launcher 目录独立刷新，列表失败或配置读取失败不会互相伪装成空结果。
    ///
    /// @author kongweiguang
    pub(super) fn reload_launcher_catalog(&mut self, cx: &mut Context<Self>) {
        if self.launcher_selection_busy {
            return;
        }
        self.launcher_generation = self.launcher_generation.wrapping_add(1).max(1);
        let generation = self.launcher_generation;
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.launcher_catalog_loading = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move { agents.launcher_catalog(cancel).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.launcher_generation != generation {
                    return;
                }
                panel.launcher_catalog_loading = false;
                match result {
                    Ok(catalog) => {
                        panel.launcher_load_error = None;
                        if panel.launcher_selection_error.is_none() {
                            panel.launcher_key = Some(catalog.selected_key.clone());
                        }
                        panel.launcher_catalog = Some(catalog);
                    }
                    Err(error) => {
                        panel.launcher_load_error = Some(localized_error(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 保存用户明确选择的 key；失败保留本地意图，让同一项可显式重试。
    ///
    /// @author kongweiguang
    pub(super) fn select_launcher(&mut self, key: AgentLauncherKey, cx: &mut Context<Self>) {
        if self.busy || self.launcher_selection_busy || self.launcher_catalog_loading {
            return;
        }
        self.launcher_key = Some(key.clone());
        self.launcher_selection_error = None;
        self.launcher_setup_prompt = false;
        self.launcher_selection_busy = true;
        self.launcher_generation = self.launcher_generation.wrapping_add(1).max(1);
        let generation = self.launcher_generation;
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.status = Some(
            crate::locale::text("正在保存 Agent 选择…", "Saving Agent selection…").to_string(),
        );
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move { agents.select_launcher(key, cancel).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.launcher_generation != generation {
                    return;
                }
                panel.launcher_selection_busy = false;
                match result {
                    Ok(catalog) => {
                        panel.launcher_key = Some(catalog.selected_key.clone());
                        panel.launcher_catalog = Some(catalog);
                        panel.launcher_selection_error = None;
                        panel.status = Some(
                            crate::locale::text("Agent 选择已保存。", "Agent selection saved.")
                                .to_string(),
                        );
                    }
                    Err(error) => {
                        let message = localized_error(&error);
                        panel.launcher_selection_error = Some(message.clone());
                        panel.status = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 重试仍使用用户最后选择的 key，不拿当前保存值覆盖未提交的界面意图。
    ///
    /// @author kongweiguang
    pub(super) fn retry_launcher_selection(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.launcher_key.clone() else {
            return;
        };
        self.select_launcher(key, cx);
    }

    /// 顶栏刷新同时重读会话和 launcher 设置，但保留两个独立错误/加载状态。
    ///
    /// @author kongweiguang
    pub(super) fn reload_all(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        self.reload_launcher_catalog(cx);
    }

    /// 用户切换历史视图时保留已选运行会话，只改变列表投影，不重建 CLI。
    ///
    /// @author kongweiguang
    pub(super) fn set_history_mode(&mut self, mode: AgentHistoryMode, cx: &mut Context<Self>) {
        if self.history_mode == mode || self.busy {
            return;
        }
        self.history_mode = mode;
        self.rename_target = None;
        self.reload(cx);
    }

    /// 处理异步历史行操作前，核验 captured 身份、过滤上下文和用户目标候选。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_history_action(
        &mut self,
        expected_id: &AgentSessionId,
        expected_title: &str,
        expected_mode: AgentHistoryMode,
        expected_scope: &AgentSessionScope,
        expected_candidate: Option<&AgentTargetBinding>,
        cx: &mut Context<Self>,
    ) -> bool {
        let candidate_is_current =
            self.candidate.as_ref().map(|candidate| &candidate.binding) == expected_candidate;
        let failure = if self.busy {
            Some((
                "另一个 Agent 操作正在进行。",
                "Another Agent operation is in progress.",
            ))
        } else if self.loading || !self.sessions_verified {
            Some((
                "Agent 历史尚未完成核验，请刷新后重试。",
                "Agent history is not verified. Refresh and try again.",
            ))
        } else if self.history_mode != expected_mode
            || &self.scope != expected_scope
            || !candidate_is_current
        {
            Some((
                "历史筛选或目标已变化，请重新打开该行菜单。",
                "The history filter or target changed. Reopen this row menu.",
            ))
        } else if !self.sessions.iter().any(|view| {
            &view.session.agent_session_id == expected_id && view.session.title == expected_title
        }) {
            Some((
                "该历史记录已变化或不存在，请刷新后重试。",
                "This history entry changed or no longer exists. Refresh and try again.",
            ))
        } else {
            None
        };
        if let Some((zh, en)) = failure {
            self.show_menu_notice(zh, en, cx);
            false
        } else {
            true
        }
    }

    /// 用同一条目与标题基线打开改名草稿；保留原有历史资格校验，失败时不替换草稿。
    ///
    /// @author kongweiguang
    pub(super) fn begin_rename_agent(
        &mut self,
        target: AgentRenameTarget,
        mode: AgentHistoryMode,
        scope: AgentSessionScope,
        candidate: Option<AgentTargetBinding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let AgentRenameTarget { id, expected_title } = target;
        if !self.ensure_history_action(&id, &expected_title, mode, &scope, candidate.as_ref(), cx) {
            return;
        }
        self.rename_title.update(cx, |input, cx| {
            input.set_value(expected_title.clone(), window, cx)
        });
        self.rename_target = Some((id, expected_title));
        cx.notify();
    }

    /// 提交标题 CAS；冲突保留精确草稿，不把服务端新标题伪装成本地成功。
    ///
    /// @author kongweiguang
    pub(super) fn save_agent_title(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((id, expected_title)) = self.rename_target.clone() else {
            return;
        };
        let title = self
            .rename_title
            .update(cx, |input, _| input.value().to_string());
        let request = RenameAgentSessionTitleRequest {
            agent_session_id: id.clone(),
            expected_title: expected_title.clone(),
            title,
        };
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.busy = true;
        self.status = Some(crate::locale::text("正在保存标题…", "Saving title…").to_string());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move { agents.rename_title(request, cancel).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.busy = false;
                match result {
                    Ok(view) => {
                        panel.replace_session_view(view);
                        if panel.rename_target.as_ref()
                            == Some(&(id.clone(), expected_title.clone()))
                        {
                            panel.rename_target = None;
                        }
                        panel.status =
                            Some(crate::locale::text("标题已保存。", "Title saved.").to_string());
                    }
                    Err(error) => {
                        panel.show_operation_error("保存标题失败", "Could not save title", error)
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消改名只丢弃 UI 草稿，不改写历史文件或标题 revision。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_agent_rename(&mut self, cx: &mut Context<Self>) {
        self.rename_target = None;
        cx.notify();
    }

    /// 使用历史启动定义创建全新身份；选择切换时作废旧上下文预览，目标仍取操作时捕获的候选。
    ///
    /// @author kongweiguang
    pub(super) fn create_from_history(
        &mut self,
        id: AgentSessionId,
        expected_title: String,
        mode: AgentHistoryMode,
        scope: AgentSessionScope,
        candidate: Option<AgentTargetBinding>,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_history_action(&id, &expected_title, mode, &scope, candidate.as_ref(), cx) {
            return;
        }
        let request = CreateAgentSessionFromHistoryRequest {
            source_agent_session_id: id,
            title: None,
            scope,
            target: candidate.unwrap_or_else(|| AgentTargetBinding::unbound(0)),
            initial_palette: self.terminal_palette,
        };
        let selection_guard = (
            self.selected_agent.clone(),
            self.scope.clone(),
            self.candidate.as_ref().map(|value| value.binding.clone()),
        );
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.start_creation_operation(
            ("正在使用启动配置新建…", "Creating from launch settings…"),
            (
                "已按该启动配置创建独立会话。",
                "A separate chat was created from this launch configuration.",
            ),
            Some(selection_guard),
            cx,
            move |cancel| async move { agents.create_from_history(request, cancel).await },
            executor,
        );
    }

    /// 保留历史、Launcher 与标题草稿，只将活动面板切回现有新建入口，不触碰 CLI 生命周期。
    ///
    /// @author kongweiguang
    pub(super) fn show_new_chat(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.launcher_selection_busy || self.selected_agent.is_none() {
            return;
        }
        self.invalidate_context_preview();
        self.selected_agent = None;
        self.rename_target = None;
        self.status = None;
        self.launcher_setup_prompt = false;
        cx.emit(new_chat_selection_event());
        cx.notify();
    }

    /// 选择一个投影中的 Agent 时作废旧预览并清除仅属于新建失败的设置 CTA，不重建或重启 terminal。
    ///
    /// @author kongweiguang
    pub(super) fn select_agent(&mut self, id: AgentSessionId, cx: &mut Context<Self>) {
        if !self
            .sessions
            .iter()
            .any(|view| view.session.agent_session_id == id)
        {
            self.status = Some(
                crate::locale::text(
                    "Agent 会话已不在当前列表，请刷新后重试。",
                    "This Agent session is no longer in the list. Refresh and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.invalidate_context_preview();
        self.launcher_setup_prompt = false;
        self.selected_agent = Some(id);
        self.publish_selection(cx);
        cx.notify();
    }

    /// 发布所选聚合与运行期 session id，Root 只需呈现共享终端 runtime 的当前快照。
    fn publish_selection(&self, cx: &mut Context<Self>) {
        let selected = self.selected_view();
        cx.emit(AgentPanelEvent::Selected {
            agent_session_id: selected.map(|view| view.session.agent_session_id.clone()),
            terminal_session_id: selected.and_then(|view| {
                view.terminal
                    .as_ref()
                    .map(|terminal| terminal.session_id.clone())
            }),
        });
    }

    /// 新建请求携带当前显式 launcher key；只有用例确认取消后才提示已取消，迟到成功仍展示真实会话。
    ///
    /// @author kongweiguang
    pub(super) fn create_agent(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.launcher_selection_busy || self.launcher_catalog_loading {
            return;
        }
        let Some(launcher_key) = self.launcher_key.clone() else {
            self.status = Some(self.launcher_load_error.clone().unwrap_or_else(|| {
                crate::locale::text(
                    "Agent 选项尚未读取成功，请刷新后重试。",
                    "Agent options are unavailable. Refresh and try again.",
                )
                .to_string()
            }));
            cx.notify();
            return;
        };
        if !self.launcher_catalog.as_ref().is_some_and(|catalog| {
            catalog
                .options
                .iter()
                .any(|option| option.key == launcher_key)
        }) {
            self.status = Some(
                crate::locale::text(
                    "所选 Agent 已不在当前配置中；请选择有效选项或刷新。",
                    "The selected Agent is no longer configured. Choose a valid option or refresh.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let entered_title = self
            .title
            .update(cx, |input, _| input.value().trim().to_owned());
        let target = self.candidate.as_ref().map_or_else(
            || AgentTargetBinding::unbound(0),
            |candidate| candidate.binding.clone(),
        );
        let request = CreateAgentSessionRequest {
            title: (!entered_title.is_empty()).then_some(entered_title),
            scope: self.scope.clone(),
            target,
            launcher_key: Some(launcher_key),
            initial_palette: self.terminal_palette,
        };
        let selection_guard = (
            self.selected_agent.clone(),
            self.scope.clone(),
            self.candidate
                .as_ref()
                .map(|candidate| candidate.binding.clone()),
        );
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.start_creation_operation(
            ("正在启动 Agent CLI…", "Starting Agent CLI…"),
            ("Agent CLI 已启动。", "Agent CLI started."),
            Some(selection_guard),
            cx,
            move |cancel| async move { agents.create(request, cancel).await },
            executor,
        );
    }

    /// 只向当前可取消用例发出请求；保留忙状态直到业务回执确认实际结果。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_agent_operation(&mut self, cx: &mut Context<Self>) {
        let requested = self
            .operation_cancellation
            .as_ref()
            .is_some_and(|cancellation| {
                if cancellation.is_cancelled() {
                    false
                } else {
                    cancellation.cancel();
                    true
                }
            });
        if requested {
            self.cancel_active_creation_focus_intent(cx);
            self.status = Some(
                crate::locale::text(
                    "已请求取消，正在等待实际结果…",
                    "Cancellation requested; waiting for the actual result…",
                )
                .to_string(),
            );
        }
        cx.notify();
    }

    /// 按历史 launch snapshot 继续一个已选会话，不在 UI 端重新拼接旧命令或权限 flags。
    pub(super) fn resume_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_agent.clone() else {
            self.status = Some(
                crate::locale::text(
                    "请先选择一个 Agent 会话。",
                    "Select an Agent session first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.resume_agent(id, cx);
    }

    /// 继续指定历史身份；菜单回调不会重新读取后来变更的 selected Agent。
    ///
    /// @author kongweiguang
    pub(super) fn resume_agent(&mut self, id: AgentSessionId, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let can_resume = self
            .sessions
            .iter()
            .find(|view| view.session.agent_session_id == id)
            .is_some_and(super::render::agent_can_resume);
        if !can_resume {
            self.status = Some(
                crate::locale::text(
                    "启动器尚未确认支持继续，或 Agent 仍在运行/恢复结果待核对。",
                    "Resume is not confirmed by the launcher, or the Agent is active or recovery is unresolved.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let previous_terminal_session_id = self
            .sessions
            .iter()
            .find(|view| view.session.agent_session_id == id)
            .and_then(|view| {
                view.terminal
                    .as_ref()
                    .map(|terminal| terminal.session_id.clone())
            });
        let selected_before = self.selected_agent.clone();
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        let initial_palette = self.terminal_palette;
        self.start_lifecycle_operation(
            AgentLifecycleOperation {
                target_id: id.clone(),
                selected_before,
                terminal_feedback_reset: super::AgentTerminalFeedbackReset::OnNewRunningTerminal {
                    previous_terminal_session_id,
                },
                pending: ("正在继续 Agent…", "Resuming Agent…"),
                success: ("Agent 已继续运行。", "Agent resumed."),
                cancellable: true,
            },
            cx,
            move |cancel| async move {
                agents
                    .resume_with_palette(id, initial_palette, cancel)
                    .await
            },
            executor,
        );
    }

    /// 只重读原恢复事务并更新投影；可用性由 view 决定，绝不转入继续启动路径。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_agent(&mut self, id: AgentSessionId, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let has_recovery = self.sessions.iter().any(|view| {
            view.session.agent_session_id == id && super::render::agent_can_reconcile(view)
        });
        if !has_recovery {
            self.show_menu_notice(
                "该 Agent 没有待核对的恢复事务，请刷新状态后重试。",
                "This Agent has no pending recovery to reconcile. Refresh and try again.",
                cx,
            );
            return;
        }
        let selected_before = self.selected_agent.clone();
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.start_lifecycle_operation(
            AgentLifecycleOperation {
                target_id: id.clone(),
                selected_before,
                terminal_feedback_reset: super::AgentTerminalFeedbackReset::Never,
                pending: ("正在核对原恢复事务…", "Reconciling the original recovery…"),
                success: (
                    "恢复状态已核对，可查看结果后再决定是否继续。",
                    "Recovery was checked. Review the result before deciding whether to resume.",
                ),
                cancellable: true,
            },
            cx,
            move |cancel| async move { agents.reconcile_resume(id, cancel).await },
            executor,
        );
    }

    /// 归档按钮调用聚合用例统一关闭 Agent CLI 并保留历史与上下文文件。
    pub(super) fn archive_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_agent.clone() else {
            self.status = Some(
                crate::locale::text(
                    "请先选择一个 Agent 会话。",
                    "Select an Agent session first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        self.archive_agent(id, cx);
    }

    /// 归档指定历史身份；保留目录和上下文，且不转向异步刷新后新的选中项。
    ///
    /// @author kongweiguang
    pub(super) fn archive_agent(&mut self, id: AgentSessionId, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(view) = self
            .sessions
            .iter()
            .find(|view| view.session.agent_session_id == id)
        else {
            self.show_menu_notice(
                "该 Agent 会话已不存在，请刷新后重试。",
                "This Agent session no longer exists. Refresh and try again.",
                cx,
            );
            return;
        };
        if view.session.status == kerminal_domain::agents::AgentSessionStatus::Archived {
            self.show_menu_notice("该会话已经归档。", "This chat is already archived.", cx);
            return;
        }
        let selected_before = self.selected_agent.clone();
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.start_lifecycle_operation(
            AgentLifecycleOperation {
                target_id: id.clone(),
                selected_before,
                terminal_feedback_reset: super::AgentTerminalFeedbackReset::Never,
                pending: ("正在归档 Agent…", "Archiving Agent…"),
                success: (
                    "Agent 已归档，历史与上下文已保留。",
                    "Agent archived; its history and context were preserved.",
                ),
                cancellable: false,
            },
            cx,
            move |cancel| async move { agents.archive(id, cancel).await },
            executor,
        );
    }

    /// 只有显式点击绑定时才写入新 generation；普通活动 Tab 更新不经过该调用。
    ///
    /// @author kongweiguang
    pub(super) fn bind_candidate(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(id) = self.selected_agent.clone() else {
            self.status = Some(
                crate::locale::text(
                    "请先选择一个 Agent 会话。",
                    "Select an Agent session first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(candidate) = self.candidate.clone() else {
            self.status = Some(
                crate::locale::text(
                    "当前没有可绑定的终端目标。",
                    "There is no terminal target to bind.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let generation = self
            .selected_view()
            .and_then(|view| view.session.target.as_ref())
            .map_or(0, |binding| binding.generation)
            .saturating_add(1);
        let mut binding = candidate.binding;
        binding.generation = generation;
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.busy = true;
        self.status = Some(
            crate::locale::text("正在显式绑定首选终端…", "Binding the preferred terminal…")
                .to_string(),
        );
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    agents.bind_preferred_target(id, binding, cancel).await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.busy = false;
                match result {
                    Ok(view) => {
                        panel.replace_session_view(view);
                        panel.status = Some(
                            crate::locale::text(
                                "Agent 首选终端已显式更新。",
                                "Agent preferred terminal updated.",
                            )
                            .to_string(),
                        );
                        cx.emit(AgentPanelEvent::TargetBindingChanged);
                    }
                    Err(error) => {
                        panel.show_operation_error("目标绑定失败", "Could not bind target", error)
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 读取当前绑定目标的受限纯文本预览；回执按 Agent 身份和请求代次校验，失效绑定不回退到新活动 Tab。
    pub(super) fn preview_context(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_agent.clone() else {
            self.status = Some(
                crate::locale::text(
                    "请先选择一个 Agent 会话。",
                    "Select an Agent session first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let requested_agent = id.clone();
        self.context_preview_generation = self.context_preview_generation.wrapping_add(1).max(1);
        let generation = self.context_preview_generation;
        self.context_preview_loading = true;
        let agents = self.agents.clone();
        let executor = self.executor.clone();
        self.busy = true;
        self.status =
            Some(crate::locale::text("正在读取目标上下文…", "Loading target context…").to_string());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(
                    move |cancel| async move { agents.target_context(id, 16 * 1024, cancel).await },
                )
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.context_preview_generation != generation
                    || panel.selected_agent.as_ref() != Some(&requested_agent)
                {
                    return;
                }
                panel.context_preview_loading = false;
                panel.busy = false;
                match result {
                    Ok(context) => {
                        panel.context_preview = context.snapshot.map(|snapshot| snapshot.text);
                        panel.status = Some(if context.redacted {
                            crate::locale::text(
                                "上下文已加载；敏感内容已脱敏。",
                                "Context loaded; sensitive content was redacted.",
                            )
                            .to_string()
                        } else {
                            crate::locale::text("上下文已加载。", "Context loaded.").to_string()
                        });
                    }
                    Err(error) => panel.show_operation_error(
                        "目标上下文不可用",
                        "Target context unavailable",
                        error,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 普通新建与历史新建共用取消与回执；焦点意图先捕获，且只有所选新会话带 Running PTY 才交给 Root。
    ///
    /// @author kongweiguang
    fn start_creation_operation<Build, Fut>(
        &mut self,
        pending: (&'static str, &'static str),
        success: (&'static str, &'static str),
        selection_guard: Option<(
            Option<AgentSessionId>,
            AgentSessionScope,
            Option<AgentTargetBinding>,
        )>,
        cx: &mut Context<Self>,
        build: Build,
        executor: ApplicationTaskExecutor,
    ) where
        Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<AgentSessionView, AppError>> + Send + 'static,
    {
        let request_generation = self.next_creation_focus_generation();
        self.active_creation_focus_generation = Some(request_generation);
        cx.emit(AgentPanelEvent::NewTerminalFocusStarted { request_generation });
        let cancellation = CancellationToken::new();
        self.operation_cancellation = Some(cancellation.clone());
        self.busy = true;
        self.launcher_setup_prompt = false;
        self.status = Some(crate::locale::text(pending.0, pending.1).to_string());
        cx.spawn(async move |this, cx| {
            let result = executor.run_cancellable(cancellation.clone(), build).await;
            let cancel_requested = cancellation.is_cancelled();
            let _ = this.update(cx, |panel, cx| {
                panel.operation_cancellation = None;
                panel.busy = false;
                match result {
                    Ok(view) => {
                        panel.launcher_setup_prompt = false;
                        let new_id = view.session.agent_session_id.clone();
                        let focus_target = super::new_terminal_focus_target(&view);
                        panel.replace_session_view(view);
                        let may_select = selection_guard.as_ref().is_none_or(
                            |(selected, scope, candidate)| {
                                panel.selected_agent.as_ref() == selected.as_ref()
                                    && &panel.scope == scope
                                    && panel
                                        .candidate
                                        .as_ref()
                                        .map(|value| &value.binding)
                                        == candidate.as_ref()
                            },
                        );
                        if may_select {
                            panel.invalidate_context_preview();
                            panel.selected_agent = Some(new_id);
                            if !cancel_requested
                                && let Some((agent_session_id, terminal_session_id)) = focus_target
                            {
                                panel.publish_creation_focus_ready(
                                    request_generation,
                                    agent_session_id,
                                    terminal_session_id,
                                    cx,
                                );
                            } else {
                                panel.cancel_creation_focus_intent(request_generation, cx);
                            }
                            panel.publish_selection(cx);
                        } else {
                            panel.cancel_creation_focus_intent(request_generation, cx);
                        }
                        panel.status = Some(if cancel_requested {
                            if may_select {
                                crate::locale::text(
                                    "Agent 已完成启动；取消请求到达前，新会话已经打开。",
                                    "The Agent finished starting before cancellation completed; its new chat is open.",
                                )
                                .to_string()
                            } else {
                                crate::locale::text(
                                    "Agent 已完成启动；取消请求到达前，新会话已创建，当前选择已保留。",
                                    "The Agent finished starting before cancellation completed; the new chat was created and the current selection was kept.",
                                )
                                .to_string()
                            }
                        } else {
                            crate::locale::text(success.0, success.1).to_string()
                        });
                        panel.reload(cx);
                    }
                    Err(error)
                        if cancel_requested
                            && error.public_code()
                                == kerminal_application::PublicErrorCode::Cancelled =>
                    {
                        panel.cancel_creation_focus_intent(request_generation, cx);
                        panel.launcher_setup_prompt = false;
                        panel.status = Some(
                            crate::locale::text(
                                "已确认取消，本次没有创建 Agent 会话。",
                                "Cancellation was confirmed; no Agent chat was created.",
                            )
                            .to_string(),
                        );
                    }
                    Err(error) => {
                        panel.cancel_creation_focus_intent(request_generation, cx);
                        let needs_readback = error.public_code()
                            == kerminal_application::PublicErrorCode::OutcomeUnknown;
                        panel.launcher_setup_prompt =
                            should_offer_launcher_setup_help(error.public_code());
                        panel.show_operation_error(
                            "Agent 创建失败",
                            "Could not create Agent",
                            error,
                        );
                        if needs_readback {
                            panel.reload(cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 为这次真实新建分配单调请求代次，防止旧异步回执与后续新建焦点意图混淆。
    ///
    /// @author kongweiguang
    fn next_creation_focus_generation(&mut self) -> u64 {
        self.creation_focus_generation = self.creation_focus_generation.wrapping_add(1).max(1);
        self.creation_focus_generation
    }

    /// 只向 Root 交付与当前新建请求配对的精确 Running PTY，历史选择和刷新不会调用此出口。
    ///
    /// @author kongweiguang
    fn publish_creation_focus_ready(
        &mut self,
        request_generation: u64,
        agent_session_id: AgentSessionId,
        terminal_session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        if self.active_creation_focus_generation != Some(request_generation) {
            return;
        }
        self.active_creation_focus_generation = None;
        cx.emit(AgentPanelEvent::NewTerminalFocusReady {
            request_generation,
            agent_session_id,
            terminal_session_id,
        });
    }

    /// 取消只清除此代新建请求，迟到结果不能撤销或覆盖后来操作的焦点意图。
    ///
    /// @author kongweiguang
    fn cancel_creation_focus_intent(&mut self, request_generation: u64, cx: &mut Context<Self>) {
        if self.active_creation_focus_generation != Some(request_generation) {
            return;
        }
        self.active_creation_focus_generation = None;
        cx.emit(AgentPanelEvent::NewTerminalFocusCancelled { request_generation });
    }

    /// 用户主动取消创建时立即撤销自动聚焦资格，即使后端稍后确认 CLI 已创建也不抢焦点。
    ///
    /// @author kongweiguang
    fn cancel_active_creation_focus_intent(&mut self, cx: &mut Context<Self>) {
        if let Some(request_generation) = self.active_creation_focus_generation {
            self.cancel_creation_focus_intent(request_generation, cx);
        }
    }

    /// 生命周期操作仅在明确 Resume 返回同 Agent 新 Running PTY 后清理旧反馈；核对、取消和失败不清。
    ///
    /// @author kongweiguang
    fn start_lifecycle_operation<Build, Fut>(
        &mut self,
        operation: AgentLifecycleOperation,
        cx: &mut Context<Self>,
        build: Build,
        executor: ApplicationTaskExecutor,
    ) where
        Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<AgentSessionView, AppError>> + Send + 'static,
    {
        let AgentLifecycleOperation {
            target_id,
            selected_before,
            terminal_feedback_reset,
            pending,
            success,
            cancellable,
        } = operation;
        let cancellation = cancellable.then(CancellationToken::new);
        self.operation_cancellation = cancellation.clone();
        self.busy = true;
        self.status = Some(crate::locale::text(pending.0, pending.1).to_string());
        cx.spawn(async move |this, cx| {
            let result = match cancellation.clone() {
                Some(cancellation) => executor.run_cancellable(cancellation, build).await,
                None => executor.run(build).await,
            };
            let cancel_requested = cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled);
            let _ = this.update(cx, |panel, cx| {
                panel.operation_cancellation = None;
                panel.busy = false;
                match result {
                    Ok(view) => {
                        if should_clear_terminal_feedback_after_lifecycle_success(
                            &terminal_feedback_reset,
                            &target_id,
                            &view,
                        ) {
                            panel
                                .terminal_feedback
                                .retain(|feedback| feedback.agent_session_id != target_id);
                        }
                        let archived = view.session.status
                            == kerminal_domain::agents::AgentSessionStatus::Archived;
                        let recovery = view.resume_recovery.clone();
                        let id = view.session.agent_session_id.clone();
                        panel.replace_session_view(view);
                        if panel.selected_agent == selected_before {
                            if archived && id == target_id {
                                panel.invalidate_context_preview();
                                panel.selected_agent = None;
                            } else if !archived {
                                if panel.selected_agent.as_ref() != Some(&id) {
                                    panel.invalidate_context_preview();
                                }
                                panel.selected_agent = Some(id);
                            }
                        }
                        panel.status = Some(if archived {
                            crate::locale::text(
                                "Agent 已归档，历史与上下文已保留。",
                                "Agent archived; its history and context were preserved.",
                            )
                            .to_string()
                        } else if let Some(recovery) = recovery {
                            let (status_zh, status_en) = match recovery.status {
                                AgentResumeRecoveryStatus::Pending => ("待核对", "needs readback"),
                                AgentResumeRecoveryStatus::Unknown => {
                                    ("结果未知", "has an unknown result")
                                }
                            };
                            crate::locale::text(
                                format!(
                                    "恢复事务{status_zh}，原终端所有权已保留。请使用“核对恢复”，不要重复启动。"
                                ),
                                format!(
                                    "Recovery {status_en}; the original terminal owner is retained. Use Reconcile recovery instead of starting another CLI."
                                ),
                            )
                            .to_string()
                        } else if cancel_requested {
                            crate::locale::text(
                                "操作已完成；取消请求到达时，结果已经提交。",
                                "The operation completed before the cancellation request took effect.",
                            )
                            .to_string()
                        } else {
                            crate::locale::text(success.0, success.1).to_string()
                        });
                        panel.publish_selection(cx);
                        panel.reload(cx);
                    }
                    Err(error)
                        if cancel_requested
                            && error.public_code()
                                == kerminal_application::PublicErrorCode::Cancelled =>
                    {
                        panel.status = Some(
                            crate::locale::text(
                                "本次操作已停止，请查看当前会话状态；已有待核对记录仍需处理。",
                                "The operation stopped. Review the current chat state; any existing recovery still needs reconciliation.",
                            )
                            .to_string(),
                        );
                        panel.reload(cx);
                    }
                    Err(error) => {
                        let needs_readback = error.public_code()
                            == kerminal_application::PublicErrorCode::OutcomeUnknown;
                        panel.show_operation_error(
                            "Agent 操作失败",
                            "Agent operation failed",
                            error,
                        );
                        if needs_readback {
                            panel.reload(cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 根据聚合身份替换唯一列表投影，防止并发刷新留下同一个 Agent 的重复行。
    fn replace_session_view(&mut self, view: AgentSessionView) {
        let id = view.session.agent_session_id.clone();
        self.sessions
            .retain(|item| item.session.agent_session_id != id);
        self.sessions.push(view);
    }

    /// 公开错误只显示安全消息；未知提交状态提示先核对实际列表再决定重试。
    fn show_operation_error(&mut self, action_zh: &str, action_en: &str, error: AppError) {
        let message =
            if error.public_code() == kerminal_application::PublicErrorCode::OutcomeUnknown {
                crate::locale::text(
                format!("{action_zh}：操作结果暂时无法确认，请刷新列表核对后再决定是否重试。"),
                format!(
                    "{action_en}: The result is not confirmed. Refresh the list before retrying."
                ),
            )
            .to_string()
            } else {
                let detail = localized_error(&error);
                crate::locale::text(
                    format!("{action_zh}：{detail}"),
                    format!("{action_en}: {detail}"),
                )
                .to_string()
            };
        self.status = Some(message);
    }
}

/// 成功读回必须同时匹配反馈 Agent 与当前选择，且看到 typed Exited；未知/运行错误绝不清除。
///
/// @author kongweiguang
pub(super) fn should_clear_terminal_feedback_after_reload(
    feedback: &super::AgentTerminalFeedback,
    selected_agent: Option<&AgentSessionId>,
    sessions: &[AgentSessionView],
) -> bool {
    if !feedback.source.clears_after_confirmed_exit()
        || selected_agent != Some(&feedback.agent_session_id)
    {
        return false;
    }

    sessions
        .iter()
        .find(|view| view.session.agent_session_id == feedback.agent_session_id)
        .and_then(|view| view.terminal.as_ref())
        .is_some_and(|terminal| {
            terminal.lifecycle == kerminal_domain::sessions::SessionLifecycle::Exited
        })
}

/// 仅凭显式 Resume 成功回执清理旧反馈，并验证 Agent owner、Running 状态及 PTY 身份确已更换。
/// 普通刷新、核对、归档、取消/失败和迟到的旧 PTY 回执都不能收敛真实 runtime 故障。
///
/// @author kongweiguang
pub(super) fn should_clear_terminal_feedback_after_lifecycle_success(
    reset: &super::AgentTerminalFeedbackReset,
    target_id: &AgentSessionId,
    view: &AgentSessionView,
) -> bool {
    let super::AgentTerminalFeedbackReset::OnNewRunningTerminal {
        previous_terminal_session_id,
    } = reset
    else {
        return false;
    };
    if &view.session.agent_session_id != target_id
        || view.session.status != kerminal_domain::agents::AgentSessionStatus::Active
        || view.resume_recovery.is_some()
    {
        return false;
    }
    let Some(terminal) = view.terminal.as_ref() else {
        return false;
    };
    if terminal.lifecycle != kerminal_domain::sessions::SessionLifecycle::Running
        || !matches!(
            &terminal.owner,
            kerminal_domain::sessions::SessionOwner::Agent { agent_session_id }
                if agent_session_id == target_id
        )
    {
        return false;
    }
    previous_terminal_session_id
        .as_ref()
        .is_none_or(|previous| previous != &terminal.session_id)
}

/// 按稳定错误码选择英文安全文案，同时保留应用定义的中文详细消息。
fn localized_error(error: &AppError) -> String {
    use kerminal_application::PublicErrorCode;

    let english = match error.public_code() {
        PublicErrorCode::Invalid => "Some input is invalid.",
        PublicErrorCode::NotFound => "The requested Agent resource was not found.",
        PublicErrorCode::Unsupported => "This operation is unavailable.",
        PublicErrorCode::CapabilityNotSupported => {
            "This connection does not support the requested capability."
        }
        PublicErrorCode::Conflict => "The Agent state changed. Refresh and try again.",
        PublicErrorCode::Cancelled => "The operation was cancelled.",
        PublicErrorCode::Infrastructure => {
            "An internal service failed. Check the session state and try again."
        }
        PublicErrorCode::OutcomeUnknown => {
            "The result is unknown. Refresh to verify before retrying."
        }
        PublicErrorCode::AuthenticationFailed => "Authentication failed.",
        PublicErrorCode::HostKeyUnknown => "The host key has not been trusted.",
        PublicErrorCode::HostKeyChanged => "The host key changed; the connection was blocked.",
        PublicErrorCode::HostKeyTrustMismatch => {
            "Host key verification did not match; trust was not saved."
        }
        PublicErrorCode::Timeout => "The operation timed out.",
        PublicErrorCode::RemoteExit => "The remote command exited.",
        PublicErrorCode::ConfigurationCorrupt => "Configuration is damaged and requires recovery.",
    };
    crate::locale::text(error.public_message(), english).to_string()
}

/// 仅为可能涉及启动定义的确定失败提供设置检查；认证、取消和未知回执使用各自的恢复流程。
///
/// @author kongweiguang
pub(super) fn should_offer_launcher_setup_help(
    code: kerminal_application::PublicErrorCode,
) -> bool {
    matches!(
        code,
        kerminal_application::PublicErrorCode::Invalid
            | kerminal_application::PublicErrorCode::NotFound
            | kerminal_application::PublicErrorCode::Unsupported
            | kerminal_application::PublicErrorCode::Infrastructure
            | kerminal_application::PublicErrorCode::ConfigurationCorrupt
    )
}
