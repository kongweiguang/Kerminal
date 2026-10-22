//! pane 输入处理、焦点请求与失焦清理的生命周期边界。
//!
//! session owner、代次与真实焦点意图需同时匹配；失焦只撤销本地组合，保留健康 session 和已排队输入。
//! @author kongweiguang

use gpui_kit::{AppContext as _, Context, Entity, Window};
use kerminal_application::sessions::{
    TerminalCommandContext, UiInputProjection, UiSessionBinding, UiTerminalInputRequest,
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::input::{KeyPhase, TerminalInputEvent, TerminalKeyEvent};

use super::pane_input::{
    PaneTerminalInputEvent, PaneTerminalInputHandler, PaneTerminalViewportEvent, PaneTextOrigin,
};
use super::{
    CachedTerminalCommandContext, CenterPanel, PaneInputRegistration, PendingPaneFocusIntent,
    RootView, RuntimePanePhase, SessionCommand, logical_input_modal_blocked, pane_input,
    terminal_input_modal_blocked, terminal_wheel, tracked_projection_for_input,
};

/// Profile 解析沿用新建预留的聚焦意图，但只更新精确旧代次；不重置原焦点、导航代次或恢复已取消意图。
///
/// 已绑定 session 的重连、其它 Tab/pane 及迟到的旧解析均不能转交，防止恢复回执抢占用户新目标。
///
/// @author kongweiguang
pub(super) fn advance_new_workspace_pane_focus_restore_generation(
    intent: Option<&mut PendingPaneFocusIntent>,
    tab_id: &TabId,
    pane_id: &PaneId,
    previous_generation: u64,
    generation: u64,
) -> bool {
    let Some(intent) = intent else {
        return false;
    };
    if intent.tab_id != *tab_id
        || intent.pane_id != *pane_id
        || intent.session_id.is_some()
        || intent.restore_generation != Some(previous_generation)
    {
        return false;
    }
    intent.restore_generation = Some(generation);
    true
}

impl RootView {
    /// 只为精确匹配的 Ready pane 创建输入 handler；异步绑定回到窗口线程以注册有生命周期的 blur 订阅。
    /// 焦点变化只撤销组合；自然退出留下只读绑定，重绘不得为它重新打开输入路径。
    /// Closing 投影仍可能渲染；发起与回执均检查真实 owner，避免关闭后重新绑定或展示旧 lookup 错误。
    ///
    /// @author kongweiguang
    pub(super) fn ensure_pane_input_handler(
        &mut self,
        session_id: SessionId,
        owner: SessionOwner,
        cx: &mut Context<Self>,
    ) -> Option<Entity<PaneTerminalInputHandler>> {
        if self.session_command_owner(&session_id).as_ref() != Some(&owner) {
            return None;
        }
        if self
            .terminal_view_bindings
            .get(&session_id)
            .is_some_and(|binding| binding.owner == owner)
            && !self.pane_input_handlers.contains_key(&session_id)
        {
            return None;
        }
        if let Some(current) = self.pane_input_handlers.get(&session_id)
            && current.binding.owner == owner
        {
            return Some(current.entity.clone());
        }

        if self
            .pane_input_handlers
            .get(&session_id)
            .is_some_and(|current| current.binding.owner != owner)
        {
            self.invalidate_pane_input_handler(&session_id, cx);
        }
        if self.pane_input_binding_failures.contains(&session_id)
            || self
                .pane_input_binding_requests
                .get(&session_id)
                .is_some_and(|(_, requested_owner)| requested_owner == &owner)
        {
            return None;
        }
        self.pane_input_binding_requests.remove(&session_id);

        self.pane_input_binding_request_generation = self
            .pane_input_binding_request_generation
            .wrapping_add(1)
            .max(1);
        let request_generation = self.pane_input_binding_request_generation;
        self.pane_input_binding_requests
            .insert(session_id.clone(), (request_generation, owner.clone()));
        let executor = self.services.executor.clone();
        let sessions = self.services.sessions.clone();
        let requested_session = session_id.clone();
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let lookup_session = requested_session.clone();
            let result = executor
                .run(
                    move |_| async move { sessions.terminal_command_context(lookup_session).await },
                )
                .await;
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    let request_is_current = root
                        .pane_input_binding_requests
                        .get(&requested_session)
                        .is_some_and(|(generation, current_owner)| {
                            *generation == request_generation && current_owner == &owner
                        });
                    if !request_is_current {
                        return;
                    }
                    root.pane_input_binding_requests.remove(&requested_session);
                    if root.session_command_owner(&requested_session).as_ref() != Some(&owner) {
                        return;
                    }
                    match result {
                        Ok(context)
                            if context.session_id == requested_session
                                && context.owner == owner =>
                        {
                            root.pane_input_binding_failures.remove(&requested_session);
                            root.install_pane_input_handler(
                                UiSessionBinding {
                                    session_id: context.session_id.clone(),
                                    owner: context.owner.clone(),
                                    session_generation: context.session_generation,
                                },
                                context,
                                window,
                                cx,
                            );
                        }
                        Ok(_) => {
                            root.pane_input_binding_failures
                                .insert(requested_session.clone());
                            let message = "终端输入绑定与当前会话所有者不匹配，输入已停用。";
                            if let SessionOwner::Agent { agent_session_id } = &owner {
                                root.report_agent_terminal_failure(
                                    agent_session_id,
                                    message.into(),
                                    cx,
                                );
                            } else {
                                root.status = Some(message.into());
                            }
                            cx.notify();
                        }
                        Err(error) => {
                            root.pane_input_binding_failures
                                .insert(requested_session.clone());
                            if let SessionOwner::Agent { agent_session_id } = &owner {
                                root.report_agent_terminal_failure(
                                    agent_session_id,
                                    error.public_message().into(),
                                    cx,
                                );
                            } else {
                                root.status = Some(error.public_message().to_owned().into());
                            }
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
        None
    }

    /// 绑定 handler 与上下文并补读等待绑定的脏快照；blur 只清预编辑，不释放会话输入 owner。
    ///
    /// @author kongweiguang
    pub(super) fn install_pane_input_handler(
        &mut self,
        binding: UiSessionBinding,
        context: TerminalCommandContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if context.session_id != binding.session_id
            || context.owner != binding.owner
            || context.session_generation != binding.session_generation
        {
            return;
        }
        self.terminal_view_bindings
            .insert(binding.session_id.clone(), binding.clone());
        if self
            .pane_input_handlers
            .get(&binding.session_id)
            .is_some_and(|current| current.binding == binding)
        {
            self.command_line_projection
                .observe_context(&binding, &context);
            self.terminal_command_contexts.insert(
                binding.session_id.clone(),
                CachedTerminalCommandContext {
                    binding: binding.clone(),
                    context: context.clone(),
                },
            );
            self.refresh_active_suggestion_context(cx);
            self.refresh_commands_session(cx);
            self.restore_pending_pane_focus(&binding, cx);
            self.resume_bound_workspace_snapshot(&binding, cx);
            return;
        }
        self.invalidate_pane_input_handler(&binding.session_id, cx);
        self.terminal_view_bindings
            .insert(binding.session_id.clone(), binding.clone());
        let input = cx.new(|input_cx| {
            PaneTerminalInputHandler::new(binding.clone(), input_cx.focus_handle())
        });
        let subscription = cx.subscribe(
            &input,
            |root, _input, event: &PaneTerminalInputEvent, cx| {
                root.handle_pane_input_event(event, cx);
            },
        );
        let focus_handle = input.read(cx).focus_handle();
        let viewport_subscription = cx.subscribe(
            &input,
            |root, _input, event: &PaneTerminalViewportEvent, cx| {
                if root
                    .pane_input_handlers
                    .get(&event.binding.session_id)
                    .is_some_and(|registration| registration.binding == event.binding)
                    && root.logical_focused_pane_binding(cx).as_ref() == Some(&event.binding)
                {
                    root.handle_terminal_viewport_scroll(event.binding.clone(), 0, cx);
                }
            },
        );
        let focus_out_subscription = cx.on_blur(&focus_handle, window, |root, window, cx| {
            root.cancel_unfocused_pane_compositions(window, cx);
        });
        self.pane_input_handlers.insert(
            binding.session_id.clone(),
            PaneInputRegistration {
                binding: binding.clone(),
                entity: input,
                _subscription: subscription,
                _viewport_subscription: viewport_subscription,
                _focus_out_subscription: focus_out_subscription,
            },
        );
        self.command_line_projection
            .observe_context(&binding, &context);
        self.terminal_command_contexts.insert(
            binding.session_id.clone(),
            CachedTerminalCommandContext {
                binding: binding.clone(),
                context: context.clone(),
            },
        );
        self.refresh_active_suggestion_context(cx);
        self.refresh_commands_session(cx);
        self.restore_pending_pane_focus(&binding, cx);
        self.resume_bound_workspace_snapshot(&binding, cx);
        cx.notify();
    }

    /// 绑定安装是等待中输出的最后一个唤醒点；只为当前可见且 dirty 的同一 owner 经 coalescer 补读。
    ///
    /// @author kongweiguang
    fn resume_bound_workspace_snapshot(
        &mut self,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) {
        let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return;
        };
        if self.terminal_view_bindings.get(&binding.session_id) == Some(binding)
            && self.workspace_tab_is_visible(tab_id)
            && self.workspace_runtime.snapshot_needs_refresh(pane_id)
        {
            self.request_workspace_pane_snapshot(tab_id.clone(), pane_id.clone(), cx);
        }
    }

    /// 在异步 handler 绑定完成时，重新验证用户焦点意图再恢复同一 pane 的焦点。
    ///
    /// 其它输入控件或 Quick Open 获得焦点、Tab/pane 改变或资源代次变化都会使请求失效，避免迟到 ACK 抢焦点。
    ///
    /// @author kongweiguang
    fn restore_pending_pane_focus(&mut self, binding: &UiSessionBinding, cx: &mut Context<Self>) {
        let Some(intent) = self.pending_pane_focus_intent.clone() else {
            return;
        };
        if self.center_panel != CenterPanel::Terminal
            || intent
                .session_id
                .as_ref()
                .is_some_and(|session_id| session_id != &binding.session_id)
            || binding.owner
                != (SessionOwner::Ui {
                    tab_id: intent.tab_id.clone(),
                    pane_id: intent.pane_id.clone(),
                })
        {
            return;
        }
        let Some(pane) = self.workspace_runtime.pane(&intent.pane_id) else {
            self.pending_pane_focus_intent = None;
            return;
        };
        if pane.tab_id != intent.tab_id
            || pane.phase != RuntimePanePhase::Ready
            || pane.session_id.as_ref() != Some(&binding.session_id)
            || intent.restore_generation.is_some_and(|generation| {
                self.workspace_restore_generations.get(&intent.pane_id) != Some(&generation)
            })
        {
            self.pending_pane_focus_intent = None;
            return;
        }
        let active_matches = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                let snapshot = controller.snapshot();
                snapshot.active_tab_id.as_ref() == Some(&intent.tab_id)
                    && snapshot.focused_pane_id.as_ref() == Some(&intent.pane_id)
            });
        let Some(registration) = self.pane_input_handlers.get(&binding.session_id) else {
            return;
        };
        if !active_matches || registration.binding != *binding {
            self.pending_pane_focus_intent = None;
            return;
        }

        let handler = registration.entity.clone();
        let binding = binding.clone();
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    let intent_is_current = root
                        .pending_pane_focus_intent
                        .as_ref()
                        .is_some_and(|current| current.generation == intent.generation);
                    if !intent_is_current {
                        return;
                    }
                    // 实际交接时再次采样前台和模态；预留时合法不代表迟到 ACK 仍可改变用户当前焦点。
                    let focus_unchanged = window.is_window_active()
                        && !window.has_active_prompt()
                        && !root.host_shutdown_requested
                        && !root.connection_prompt_modal_open
                        && !root.quick_open_modal_open
                        && !root.split_target_picker.read(cx).is_open()
                        && !logical_input_modal_blocked(
                            root.preferences_modal_open,
                            root.connections_modal_open,
                            root.snippet_editor_modal_open,
                            root.root_confirmation_owner.is_some(),
                            root.pending_quit_intent.is_some(),
                        )
                        && window.focused(cx) == intent.expected_focus;
                    let pane_is_current =
                        root.workspace_controller
                            .as_ref()
                            .is_some_and(|controller| {
                                let snapshot = controller.snapshot();
                                snapshot.active_tab_id.as_ref() == Some(&intent.tab_id)
                                    && snapshot.focused_pane_id.as_ref() == Some(&intent.pane_id)
                            })
                            && root.center_panel == CenterPanel::Terminal
                            && root
                                .workspace_runtime
                                .pane(&intent.pane_id)
                                .is_some_and(|pane| {
                                    pane.phase == RuntimePanePhase::Ready
                                        && pane.tab_id == intent.tab_id
                                        && pane.session_id.as_ref() == Some(&binding.session_id)
                                })
                            && intent.restore_generation.is_none_or(|generation| {
                                root.workspace_restore_generations.get(&intent.pane_id)
                                    == Some(&generation)
                            })
                            && root
                                .pane_input_handlers
                                .get(&binding.session_id)
                                .is_some_and(|current| current.binding == binding);
                    if focus_unchanged && pane_is_current {
                        window.focus(&handler.read(cx).focus_handle(), cx);
                    }
                    root.pending_pane_focus_intent = None;
                    root.cancel_unfocused_pane_compositions(window, cx);
                });
            });
        })
        .detach();
    }

    /// 点击或显式切回 Terminal 时，历史 pane 聚焦 Root 只读路由；其它输入绑定失败仍按原意重试，迟到 ACK 不夺焦点。
    ///
    /// @author kongweiguang
    pub(super) fn request_workspace_pane_focus(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pane_focus_intent_generation =
            self.pane_focus_intent_generation.wrapping_add(1).max(1);
        let session_id = self.workspace_runtime.session_for_pane(&pane_id).cloned();
        let owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        let input_handler = session_id.as_ref().and_then(|session_id| {
            // 自动渲染在绑定错误后保持失败锁存；只有显式焦点操作才解除并重试一次，避免渲染循环反复查找。
            self.pane_input_binding_failures.remove(session_id);
            self.ensure_pane_input_handler(session_id.clone(), owner.clone(), cx)
        });
        if let Some(input_handler) = input_handler {
            window.focus(&input_handler.read(cx).focus_handle(), cx);
            self.pending_pane_focus_intent = None;
        } else if let Some(_binding) = session_id
            .as_ref()
            .and_then(|session_id| self.terminal_view_bindings.get(session_id))
            .filter(|binding| {
                binding.owner == owner
                    && is_retired_history_binding(
                        self.terminal_view_bindings.get(&binding.session_id),
                        binding,
                        self.pane_input_handlers.contains_key(&binding.session_id),
                    )
                    && self.logical_focused_terminal_view_binding(cx).as_ref() == Some(*binding)
                    && window.is_window_active()
                    && !window.has_active_prompt()
                    && !self.host_shutdown_requested
                    && !self.connection_prompt_modal_open
            })
        {
            // Exited pane 的显式点击是用户焦点意图；复用 Root 的只读快捷键路由，不创建已退休的写入 handler。
            window.focus(&self.focus_handle, cx);
            self.pending_pane_focus_intent = None;
        } else {
            // 保持当前真实焦点不动；无焦点时也可在 pane 就绪后恢复，但任何焦点变化都会取消。
            self.pending_pane_focus_intent = Some(PendingPaneFocusIntent {
                generation: self.pane_focus_intent_generation,
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
                session_id: session_id.clone(),
                restore_generation: self.workspace_restore_generations.get(&pane_id).copied(),
                expected_focus: window.focused(cx),
            });
        }
        self.set_reported_focus_session(session_id, cx);
        self.cancel_unfocused_pane_compositions(window, cx);
    }

    /// 新建终端明确包含聚焦意图；先固定当前真实焦点，只有该意图、恢复代次和活动 pane 都未变化时才在 ACK 后恢复。
    pub(super) fn remember_new_workspace_pane_focus_intent(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        restore_generation: u64,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.pane_focus_intent_generation =
            self.pane_focus_intent_generation.wrapping_add(1).max(1);
        self.pending_pane_focus_intent = Some(PendingPaneFocusIntent {
            generation: self.pane_focus_intent_generation,
            tab_id,
            pane_id,
            session_id: None,
            restore_generation: Some(restore_generation),
            expected_focus: window.focused(cx),
        });
    }

    /// pane 和 Agent 新建共用用户导航代次；显式切换输入目标时撤销所有迟到首帧的抢焦点意图。
    ///
    /// @author kongweiguang
    pub(super) fn clear_pending_pane_focus(&mut self) {
        self.pending_pane_focus_intent = None;
        self.pending_agent_focus_intent = None;
        self.pane_focus_intent_generation =
            self.pane_focus_intent_generation.wrapping_add(1).max(1);
    }

    /// 关闭/换代时先失效 handler 与缓存，再同步清理命令面板 binding，避免旧代次继续执行。
    /// 尺寸失败只抑制当前绑定的自动重绘重试，不能泄漏到重连后的新输入归属；搜索计时器随同失效。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate_pane_input_handler(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        self.cancel_terminal_drag_for_session(session_id, cx);
        self.suspend_terminal_search(session_id, cx);
        self.terminal_tools_runtime.cancel_session(session_id);
        self.session_commands.cancel_pending(session_id);
        self.terminal_view_bindings.remove(session_id);
        self.failed_session_resizes.remove(session_id);
        self.pane_input_binding_requests.remove(session_id);
        self.pane_input_binding_failures.remove(session_id);
        terminal_wheel::remove(&mut self.terminal_wheels, session_id);
        if let Some(cached) = self.terminal_command_contexts.remove(session_id) {
            self.command_line_projection
                .invalidate_binding(&cached.binding);
        }
        if let Some(registration) = self.pane_input_handlers.remove(session_id) {
            registration
                .entity
                .update(cx, |handler, cx| handler.invalidate(cx));
        }
        if self
            .suggestion_menu_binding
            .as_ref()
            .is_some_and(|binding| &binding.session_id == session_id)
        {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
        }
        self.refresh_commands_session(cx);
    }

    /// 自然退出先撤销未发送 work 并清理失效 resize 标记，再保留精确只读 binding 供历史交互。
    /// 仅当当前 pane 的实际 handler 仍聚焦且未被搜索、菜单或模态覆盖时，将物理焦点交给 Root。
    ///
    /// @author kongweiguang
    pub(super) fn retire_pane_input_handler_for_history(
        &mut self,
        session_id: &SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let binding = self.terminal_view_bindings.get(session_id).cloned();
        let should_focus_history = binding.as_ref().is_some_and(|binding| {
            let logical_binding_matches =
                self.logical_focused_terminal_view_binding(cx).as_ref() == Some(binding);
            let handler_is_focused =
                self.pane_input_handlers
                    .get(session_id)
                    .is_some_and(|registration| {
                        registration.binding == *binding
                            && registration
                                .entity
                                .read(cx)
                                .focus_handle()
                                .is_focused(window)
                    });
            let search_is_open = self
                .workspace_tools
                .get(session_id)
                .is_some_and(|tools| tools.read(cx).search_overlay().is_some());
            let app_overlay_is_open = self.host_shutdown_requested
                || self.connection_prompt_modal_open
                || self.quit_ready_intent.is_some();
            should_focus_terminal_history_after_exit(
                window.is_window_active(),
                window.has_active_prompt(),
                logical_binding_matches,
                handler_is_focused,
                search_is_open,
                app_overlay_is_open,
            )
        });
        let pane_id = self.workspace_runtime.pane_for_session(session_id).cloned();
        if let (Some(pane_id), Some(binding)) = (pane_id.as_ref(), binding.as_ref()) {
            let _ = self
                .workspace_runtime
                .retire_history_binding(pane_id, binding.clone());
        }
        let dropped = self.session_commands.cancel_pending(session_id);
        let in_flight_history_resize = self
            .session_commands
            .in_flight_history_resize_size(session_id);
        if let Some(pane_id) = pane_id {
            if dropped.history_resize_commands > 0 {
                let _ = self
                    .workspace_runtime
                    .reset_pending_history_view_resize_after_queue_cancel(
                        &pane_id,
                        session_id,
                        in_flight_history_resize,
                    );
            }
            let _ = self
                .workspace_runtime
                .clear_pending_transport_resize_after_exit(&pane_id, session_id);
        }
        self.invalidate_pane_input_handler(session_id, cx);
        if let Some(binding) = binding {
            self.terminal_view_bindings
                .insert(session_id.clone(), binding);
        }
        if should_focus_history {
            // 只在退出回执仍属于当前物理与逻辑焦点时恢复只读键盘路由；异步迟到回执不会夺走新目标或弹层焦点。
            window.focus(&self.focus_handle, cx);
        }
    }

    /// 只在当前可见 pane 与 Root 焦点吻合且没有 modal 输入所有者时返回 pane 输入目标。
    ///
    /// @author kongweiguang
    pub(super) fn logical_focused_pane_binding(
        &self,
        cx: &Context<RootView>,
    ) -> Option<UiSessionBinding> {
        self.logical_focused_terminal_view_binding(cx)
            .filter(|binding| {
                self.pane_input_handlers
                    .get(&binding.session_id)
                    .is_some_and(|registration| registration.binding == *binding)
            })
    }

    /// 只读操作仍遵守当前逻辑目标与模态归属，但不以可写 handler 的存活作为历史可读条件；
    /// Quick Open 镜像关闭终端逻辑 binding，避免输入框失焦时复用旧终端作为目标。
    ///
    /// @author kongweiguang
    pub(super) fn logical_focused_terminal_view_binding(
        &self,
        cx: &Context<RootView>,
    ) -> Option<UiSessionBinding> {
        if self.split_target_picker.read(cx).is_open() {
            return None;
        }
        if terminal_input_modal_blocked(
            logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open,
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some(),
            ),
            self.quick_open_modal_open,
        ) {
            return None;
        }

        if self.focused_agent_terminal {
            let session_id = self.agent_terminal_session_id.as_ref()?;
            let agent_session_id = self.selected_agent_session_id.as_ref()?;
            let binding = self.terminal_view_bindings.get(session_id)?;
            let expected_owner = SessionOwner::Agent {
                agent_session_id: agent_session_id.clone(),
            };
            return (binding.session_id == *session_id && binding.owner == expected_owner)
                .then(|| binding.clone());
        }

        if self.center_panel != CenterPanel::Terminal {
            return None;
        }
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        let tab_id = snapshot.active_tab_id.as_ref()?;
        let active_tab = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?;
        let layout = active_tab.terminal_layout()?;
        let pane_id = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|pane_id| layout.contains_pane(pane_id))
            .cloned()
            .or_else(|| layout.pane_ids().into_iter().next())?;
        let pane = self.workspace_runtime.pane(&pane_id)?;
        let history_binding = self.terminal_view_bindings.get(pane.session_id.as_ref()?)?;
        let close_failed_history = pane.phase == RuntimePanePhase::CloseFailed
            && self
                .workspace_runtime
                .close_failed_history_binding_matches(&pane_id, history_binding);
        if (pane.phase != RuntimePanePhase::Ready && !close_failed_history)
            || &pane.tab_id != tab_id
        {
            return None;
        }
        let session_id = pane.session_id.as_ref()?;
        let binding = self.terminal_view_bindings.get(session_id)?;
        let expected_owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id,
        };
        (binding.session_id == *session_id && binding.owner == expected_owner)
            .then(|| binding.clone())
    }

    /// 验证物理焦点、逻辑当前 pane 与注册时不可变的 binding 同时一致。
    ///
    /// @author kongweiguang
    pub(super) fn pane_input_binding_is_focused(
        &self,
        binding: &UiSessionBinding,
        window: &Window,
        cx: &Context<Self>,
    ) -> bool {
        let Some(registration) = self.pane_input_handlers.get(&binding.session_id) else {
            return false;
        };
        let focus_handle_is_focused = registration
            .entity
            .read(cx)
            .focus_handle()
            .is_focused(window);
        pane_input::input_binding_is_focused(
            binding,
            &registration.binding,
            self.logical_focused_pane_binding(cx).as_ref(),
            focus_handle_is_focused,
        )
    }

    /// 焦点离开 pane 时撤销本地组合与拖选任务，避免隐藏 pane 的边缘滚动继续改变阅读位置。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_unfocused_pane_compositions(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let logical_binding = self.logical_focused_pane_binding(cx);
        let logical_view_binding = self.logical_focused_terminal_view_binding(cx);
        let drag_sessions = self
            .terminal_view_bindings
            .values()
            .filter(|binding| {
                let physical_focus = self
                    .pane_input_handlers
                    .get(&binding.session_id)
                    .map(|registration| {
                        registration
                            .entity
                            .read(cx)
                            .focus_handle()
                            .is_focused(window)
                    })
                    .unwrap_or_else(|| self.focus_handle.is_focused(window));
                !window.is_window_active()
                    || !physical_focus
                    || logical_view_binding.as_ref() != Some(*binding)
            })
            .map(|binding| binding.session_id.clone())
            .collect::<Vec<_>>();
        for session_id in drag_sessions {
            self.cancel_terminal_drag_for_session(&session_id, cx);
        }
        let to_cancel = self
            .pane_input_handlers
            .values()
            .filter_map(|registration| {
                let has_physical_focus = registration
                    .entity
                    .read(cx)
                    .focus_handle()
                    .is_focused(window);
                let is_current = window.is_window_active()
                    && has_physical_focus
                    && logical_binding.as_ref() == Some(&registration.binding);
                (!is_current && registration.entity.read(cx).needs_focus_loss_cancellation())
                    .then(|| registration.entity.clone())
            })
            .collect::<Vec<_>>();
        for handler in to_cancel {
            handler.update(cx, |handler, cx| handler.cancel_for_focus_loss(cx));
        }
    }

    /// 同时覆盖工作区与 Agent 工具视图；代次失效后旧定时任务自行结束，不影响已完成的选区。
    pub(super) fn cancel_terminal_drag_for_session(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(tools) = self.workspace_tools.get(session_id) {
            tools.update(cx, |tools, cx| tools.cancel_canvas_selection_gesture(cx));
        }
        if self.agent_terminal_session_id.as_ref() == Some(session_id)
            && let Some(tools) = self.agent_terminal_tools.as_ref()
        {
            tools.update(cx, |tools, cx| tools.cancel_canvas_selection_gesture(cx));
        }
    }

    /// 文本与新输入只接受当前逻辑焦点；同 binding 的 key-up 仍可释放先前按下的终端按键。
    /// 请求在命令与 session FIFO 间传递同一 UiInputWork Box，保留绑定快照且不在逐键路径重复分配。
    /// 隔离诊断仅公开固定阶段和拒绝码；准入记录必须位于去重之后，不能把随后丢弃的提交记成已接受。
    ///
    /// @author kongweiguang
    fn handle_pane_input_event(&mut self, event: &PaneTerminalInputEvent, cx: &mut Context<Self>) {
        let text_probe = if pane_input::ime_diagnostics_enabled() {
            match &event.event {
                TerminalInputEvent::Text(text) => Some((
                    text.encode_utf16().count() as u64,
                    event.binding.session_generation,
                )),
                _ => None,
            }
        } else {
            None
        };
        if let Some((utf16_units, session_generation)) = text_probe {
            tracing::info!(
                event = "TERMINAL_IME_DIAGNOSTIC",
                phase = 20_u64,
                count = utf16_units,
                ime_session_generation = session_generation
            );
        }
        let Some(registration) = self.pane_input_handlers.get(&event.binding.session_id) else {
            if text_probe.is_some() {
                tracing::info!(
                    event = "TERMINAL_IME_DIAGNOSTIC",
                    phase = 21_u64,
                    ime_reason = 1_u64
                );
            }
            return;
        };
        let is_key_release = matches!(
            &event.event,
            TerminalInputEvent::Key(TerminalKeyEvent {
                phase: KeyPhase::Release,
                ..
            })
        );
        if registration.binding != event.binding
            || (!is_key_release
                && self.logical_focused_pane_binding(cx).as_ref() != Some(&event.binding))
        {
            if text_probe.is_some() {
                tracing::info!(
                    event = "TERMINAL_IME_DIAGNOSTIC",
                    phase = 21_u64,
                    ime_reason = if registration.binding != event.binding {
                        2_u64
                    } else {
                        3_u64
                    }
                );
            }
            return;
        }
        if matches!(
            (&event.event, event.text_origin),
            (
                TerminalInputEvent::Text(text),
                Some(PaneTextOrigin::Plain)
            ) if self.consume_duplicate_key_commit(&event.binding.session_id, text)
        ) {
            if text_probe.is_some() {
                tracing::info!(
                    event = "TERMINAL_IME_DIAGNOSTIC",
                    phase = 21_u64,
                    ime_reason = 4_u64
                );
            }
            return;
        }
        if text_probe.is_some() {
            tracing::info!(event = "TERMINAL_IME_DIAGNOSTIC", phase = 22_u64);
        }
        let broadcast_active = matches!(&event.binding.owner, SessionOwner::Ui { .. })
            && self.center_panel == CenterPanel::Terminal
            && self.focused_terminal_session_id().as_ref() == Some(&event.binding.session_id)
            && self.broadcast_session_ids.is_some();
        if broadcast_active {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
            self.enqueue_session_command(
                SessionCommand::Input {
                    session_id: event.binding.session_id.clone(),
                    event: event.event.clone(),
                },
                cx,
            );
            return;
        }
        let projection = if self
            .pane_input_handlers
            .get(&event.binding.session_id)
            .is_some_and(|registration| registration.entity.read(cx).is_composing())
        {
            UiInputProjection::Invalidates
        } else {
            tracked_projection_for_input(&event.binding.owner, event)
        };
        if projection == UiInputProjection::Invalidates
            && self.is_active_suggestion_binding(&event.binding)
        {
            self.terminal_command_suggestions.clear();
            self.suggestion_menu_binding = None;
            self.sync_suggestion_menu(cx);
        }
        self.enqueue_session_command(
            SessionCommand::UiInput(Box::new(super::session_queue::UiInputWork::new(
                UiTerminalInputRequest {
                    binding: event.binding.clone(),
                    expected_context: None,
                    projection,
                    event: event.event.clone(),
                },
                None,
            ))),
            cx,
        );
    }
}

/// 退出时仅在原 pane 仍是活动焦点、窗口可见且没有菜单/搜索/模态焦点所有者时转交 Root。
/// 参数化的门禁让迟到生命周期回执与叠层状态可用纯单元测试固定。
///
/// @author kongweiguang
fn should_focus_terminal_history_after_exit(
    window_is_active: bool,
    prompt_is_open: bool,
    logical_binding_matches: bool,
    handler_is_focused: bool,
    search_is_open: bool,
    app_overlay_is_open: bool,
) -> bool {
    window_is_active
        && !prompt_is_open
        && logical_binding_matches
        && handler_is_focused
        && !search_is_open
        && !app_overlay_is_open
}

/// 历史焦点目标只能由已保留的同一只读 binding 标识，不能把暂时缺 handler 的新会话当成历史页。
///
/// @author kongweiguang
fn is_retired_history_binding(
    current_binding: Option<&UiSessionBinding>,
    expected_binding: &UiSessionBinding,
    has_input_handler: bool,
) -> bool {
    !has_input_handler && current_binding == Some(expected_binding)
}

#[cfg(test)]
#[path = "../../tests/unit/root_pane_input_focus.rs"]
mod history_focus_tests;
