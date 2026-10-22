//! 工作区 Context inspector 的只读实体与刷新生命周期。
//!
//! @author kongweiguang

use std::{future::Future, sync::Arc, task::Poll, time::Duration};

use gpui_kit::{Context, EventEmitter, IntoElement, Render, Window};
use kerminal_application::{
    ApplicationTaskExecutor,
    agents::{AgentService, AgentSessionView},
    sessions::{CommandBlockSnapshot, SessionService},
};
use kerminal_domain::identity::{AgentSessionId, SessionId};
use tokio_util::sync::CancellationToken;

mod projection;
mod render;

pub use projection::{
    ContextAgentAssociation, ContextAgentLifecycle, ContextAgentSummary, ContextArtifactCategory,
    ContextArtifactIndex, ContextArtifactIndexState, ContextArtifactSummary, ContextConnectionKind,
    ContextConnectionStatus, ContextDiagnosticSeverity, ContextDiagnosticSummary,
    ContextLocationConfidence, ContextLocationSource, ContextLocationSummary,
    ContextNavigationTarget, ContextPaneSummary, ContextProjectionStatus, ContextRuntimeSummary,
    ContextSourceStatus, ContextSourceSummary, ContextTabSummary, ContextTargetSummary,
};

pub(crate) use projection::{
    ContextTerminalLinkIndex, ContextWorkspaceProjection, associated_agent_summaries,
};

pub(crate) use projection::terminal_link_identity_matches;

use projection::command_block_artifact_index;

const AGENT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Agent 列表的真实读取状态；失败不会被压成“没有关联会话”。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextAgentIndexState {
    Unavailable,
    Loading,
    Fresh,
    Failed,
}

/// 只读面板导航意图交还 Root；切换目标不会写入或重绑 Agent 首选目标。
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ContextPanelEvent {
    Navigate(ContextNavigationTarget),
    OpenLink {
        identity: crate::terminal_canvas::TerminalLinkSnapshotIdentity,
        candidate_id: crate::terminal_canvas::TerminalLinkCandidateId,
    },
}

/// Context 面板消费真实业务列表和不可变 workspace 投影，不持有连接或终端资源。
pub(crate) struct ContextPanel {
    agents: Option<Arc<dyn AgentService>>,
    sessions: Option<Arc<dyn SessionService>>,
    executor: ApplicationTaskExecutor,
    projection: ContextWorkspaceProjection,
    command_artifacts: ContextArtifactIndex,
    agent_views: Vec<AgentSessionView>,
    agent_state: ContextAgentIndexState,
    selected_agent_id: Option<AgentSessionId>,
    active: bool,
    refresh_generation: u64,
    refresh_cancellation: Option<CancellationToken>,
    artifact_session_id: Option<SessionId>,
    artifact_generation: u64,
    artifact_cancellation: Option<CancellationToken>,
    collapsed_sections: std::collections::BTreeSet<ContextSection>,
}

/// 面板只支持固定 inspector 分区，不为每个字段建立通用表单状态。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum ContextSection {
    Target,
    Workspace,
    Location,
    Resources,
    Agent,
    Discoveries,
}

impl EventEmitter<ContextPanelEvent> for ContextPanel {}

impl Drop for ContextPanel {
    /// Entity 销毁时取消 watcher；订阅等待会醒来，受监督的应用查询仍等待协作清理。
    fn drop(&mut self) {
        self.cancel_refresh();
        self.cancel_artifact_watch();
    }
}

impl ContextPanel {
    /// 注入应用 Agent 查询与共享执行器，初始状态明确为未加载且不触发后台轮询。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        agents: Option<Arc<dyn AgentService>>,
        executor: ApplicationTaskExecutor,
    ) -> Self {
        Self {
            agent_state: if agents.is_some() {
                ContextAgentIndexState::Loading
            } else {
                ContextAgentIndexState::Unavailable
            },
            agents,
            sessions: None,
            executor,
            projection: ContextWorkspaceProjection::unavailable(),
            command_artifacts: ContextArtifactIndex::not_connected(),
            agent_views: Vec::new(),
            active: false,
            selected_agent_id: None,
            refresh_generation: 0,
            refresh_cancellation: None,
            artifact_session_id: None,
            artifact_generation: 0,
            artifact_cancellation: None,
            collapsed_sections: Default::default(),
        }
    }

    /// 以同一个 SessionService 读取显式活动 session 的受限 OSC 133 摘要，不接触终端正文。
    ///
    /// @author kongweiguang
    pub(crate) fn with_sessions(mut self, sessions: Arc<dyn SessionService>) -> Self {
        self.sessions = Some(sessions);
        self.reset_command_artifacts();
        self
    }

    /// Root 传入同一工作区状态快照；普通 Tab 切换只更新 inspector 投影，不改 Agent 绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn set_projection(
        &mut self,
        projection: ContextWorkspaceProjection,
        cx: &mut Context<Self>,
    ) {
        let previous_session_id = self.artifact_session_id.clone();
        let next_session_id = projection
            .active_pane
            .as_ref()
            .and_then(|pane| pane.session_id.clone());
        self.projection = projection;
        if previous_session_id != next_session_id {
            self.cancel_artifact_watch();
            self.artifact_session_id = next_session_id;
            self.reset_command_artifacts();
            if self.active {
                self.start_artifact_watch(cx);
            }
        }
        cx.notify();
    }

    /// Root 传入右栏当前明确选择的 Agent 身份；普通 Tab 变化不会重写此选择或其目标绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn set_selected_agent(
        &mut self,
        agent_session_id: Option<AgentSessionId>,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent_id == agent_session_id {
            return;
        }
        self.selected_agent_id = agent_session_id;
        if self.active {
            self.start_refresh_loop(cx);
        } else {
            cx.notify();
        }
    }

    /// 仅在 Context 可见时刷新 Agent 与终端摘要；隐藏后取消两类任务并丢弃迟到结果。
    ///
    /// @author kongweiguang
    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.start_refresh_loop(cx);
            self.start_artifact_watch(cx);
        } else {
            self.cancel_refresh();
            self.cancel_artifact_watch();
            if self.artifact_session_id.is_some()
                && matches!(
                    &self.command_artifacts.state,
                    ContextArtifactIndexState::Available | ContextArtifactIndexState::Partial
                )
            {
                self.command_artifacts.state = ContextArtifactIndexState::Stale;
            }
            cx.notify();
        }
    }

    /// 仅面板活动时刷新 Agent 与当前 session 的 OSC 摘要，隐藏面板不启动后台扫描。
    ///
    /// @author kongweiguang
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        self.start_refresh_loop(cx);
        self.start_artifact_watch(cx);
    }

    /// 折叠或展开单个固定分区，不改变后台业务状态或刷新周期。
    pub(super) fn toggle_section(&mut self, section: ContextSection, cx: &mut Context<Self>) {
        if !self.collapsed_sections.remove(&section) {
            self.collapsed_sections.insert(section);
        }
        cx.notify();
    }

    /// 只有真实 AgentService 列表能标记为 Fresh；未装配或失败不会显示成功空列表。
    pub(super) fn agent_index_state(&self) -> &ContextAgentIndexState {
        &self.agent_state
    }

    /// 按当前目标身份计算关联会话，不从不相关 Tab 或失效 session 回退到其它 Agent。
    pub(super) fn associated_agents(&self) -> Vec<ContextAgentSummary> {
        associated_agent_summaries(&self.projection, &self.agent_views)
    }

    /// 按 Root 提供的稳定 id 读取当前服务快照，不以列表首项代替失效选择。
    ///
    /// @author kongweiguang
    pub(super) fn selected_agent(&self) -> Option<&AgentSessionView> {
        if !matches!(&self.agent_state, ContextAgentIndexState::Fresh) {
            return None;
        }
        let id = self.selected_agent_id.as_ref()?;
        self.agent_views
            .iter()
            .find(|view| &view.session.agent_session_id == id)
    }

    /// 分区状态只由用户显式折叠；默认展示完整上下文以保留旧 inspector 的信息密度。
    pub(super) fn section_collapsed(&self, section: ContextSection) -> bool {
        self.collapsed_sections.contains(&section)
    }

    /// 新一轮查询取消旧请求，并以代次阻止旧结果覆盖刚切换的工作区目标。
    fn start_refresh_loop(&mut self, cx: &mut Context<Self>) {
        self.cancel_refresh();
        let Some(agents) = self.agents.clone() else {
            self.agent_state = ContextAgentIndexState::Unavailable;
            cx.notify();
            return;
        };

        self.refresh_generation = self.refresh_generation.wrapping_add(1).max(1);
        let generation = self.refresh_generation;
        let panel_cancellation = CancellationToken::new();
        self.refresh_cancellation = Some(panel_cancellation.clone());
        let executor = self.executor.clone();
        let repeat = self.active;
        self.agent_state = ContextAgentIndexState::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            loop {
                let request_cancellation = CancellationToken::new();
                let service_cancellation = request_cancellation.clone();
                let panel_cancellation = panel_cancellation.clone();
                let request_agents = Arc::clone(&agents);
                let result = executor
                    .run(move |executor_cancellation| async move {
                        await_agent_list_with_cancellation(
                            request_agents.list(service_cancellation.clone()),
                            request_cancellation,
                            panel_cancellation,
                            executor_cancellation,
                        )
                        .await
                    })
                    .await;
                let should_continue = this
                    .update(cx, |panel, cx| {
                        if panel.refresh_generation != generation || !panel.active && repeat {
                            return false;
                        }
                        panel.apply_agent_result(result, cx);
                        panel.active && repeat
                    })
                    .unwrap_or(false);
                if !should_continue {
                    break;
                }

                cx.background_executor().timer(AGENT_REFRESH_INTERVAL).await;
                let still_current = this
                    .update(cx, |panel, _| {
                        panel.refresh_generation == generation && panel.active
                    })
                    .unwrap_or(false);
                if !still_current {
                    break;
                }
            }
        })
        .detach();
    }

    /// 取消只发送协作 token 并失效当前代次；应用用例 future 仍由共享 executor 管理。
    fn cancel_refresh(&mut self) {
        self.refresh_generation = self.refresh_generation.wrapping_add(1).max(1);
        if let Some(cancellation) = self.refresh_cancellation.take() {
            cancellation.cancel();
        }
    }

    /// 重置为与实际接线状态一致的 loading、unavailable 或 not-connected 状态。
    ///
    /// @author kongweiguang
    fn reset_command_artifacts(&mut self) {
        self.command_artifacts = if self.sessions.is_none() {
            projection::ContextArtifactIndex::not_connected()
        } else if self.artifact_session_id.is_some() {
            projection::ContextArtifactIndex {
                state: ContextArtifactIndexState::Loading,
                entries: Vec::new(),
            }
        } else {
            projection::ContextArtifactIndex {
                state: ContextArtifactIndexState::Unavailable,
                entries: Vec::new(),
            }
        };
    }

    /// 取消旧 session 的 OSC 订阅；取消代次与 Agent 列表分开，避免两类来源互相覆盖。
    ///
    /// @author kongweiguang
    fn cancel_artifact_watch(&mut self) {
        self.artifact_generation = self.artifact_generation.wrapping_add(1).max(1);
        if let Some(cancellation) = self.artifact_cancellation.take() {
            cancellation.cancel();
        }
    }

    /// 对捕获的 session 先订阅再读快照，事件仅触发重读；不会切 Tab 或回退到其它 session。
    ///
    /// @author kongweiguang
    fn start_artifact_watch(&mut self, cx: &mut Context<Self>) {
        self.cancel_artifact_watch();
        if !self.active {
            return;
        }
        let (Some(sessions), Some(session_id)) = (
            self.sessions.clone(),
            self.projection
                .active_pane
                .as_ref()
                .and_then(|pane| pane.session_id.clone()),
        ) else {
            self.reset_command_artifacts();
            cx.notify();
            return;
        };

        self.artifact_session_id = Some(session_id.clone());
        self.reset_command_artifacts();
        self.artifact_generation = self.artifact_generation.wrapping_add(1).max(1);
        let generation = self.artifact_generation;
        let panel_cancellation = CancellationToken::new();
        self.artifact_cancellation = Some(panel_cancellation.clone());
        let executor = self.executor.clone();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let subscription_sessions = Arc::clone(&sessions);
            let subscription_session_id = session_id.clone();
            let mut subscription = match executor
                .run(move |_| async move {
                    subscription_sessions
                        .subscribe(subscription_session_id)
                        .await
                })
                .await
            {
                Ok(subscription) => subscription,
                Err(_) => {
                    this.update(cx, |panel, cx| {
                        panel.apply_artifact_error(generation, &session_id, cx);
                    })
                    .ok();
                    return;
                }
            };
            if panel_cancellation.is_cancelled() {
                return;
            }

            loop {
                let query_sessions = Arc::clone(&sessions);
                let query_session_id = session_id.clone();
                let query_panel_cancellation = panel_cancellation.clone();
                let result = executor
                    .run(move |executor_cancellation| async move {
                        let service_cancellation = CancellationToken::new();
                        let operation_cancellation = service_cancellation.clone();
                        let operation = query_sessions
                            .command_block_snapshot(query_session_id, &operation_cancellation);
                        await_with_cooperative_cancellation(
                            operation,
                            service_cancellation,
                            query_panel_cancellation,
                            executor_cancellation,
                        )
                        .await
                    })
                    .await;

                let should_continue = this
                    .update(cx, |panel, cx| {
                        if panel.artifact_generation != generation
                            || panel.artifact_session_id.as_ref() != Some(&session_id)
                            || !panel.active
                        {
                            return false;
                        }
                        match result {
                            Ok(snapshot) if snapshot.session_id.as_str() == session_id.as_str() => {
                                panel.apply_command_block_snapshot(snapshot, cx)
                            }
                            Ok(_) => panel.apply_artifact_error(generation, &session_id, cx),
                            Err(_) => panel.apply_artifact_error(generation, &session_id, cx),
                        }
                        panel.active && panel.artifact_generation == generation
                    })
                    .unwrap_or(false);
                if !should_continue {
                    break;
                }

                // Resize/title 不改变命令块，跳过无关通知以免高频布局操作重复读取快照。
                let should_read_again = loop {
                    match subscription.recv(&panel_cancellation).await {
                        Ok(Some(change))
                            if matches!(
                                change.kind,
                                kerminal_application::sessions::SessionChangeKind::Output
                                    | kerminal_application::sessions::SessionChangeKind::Lifecycle
                                    | kerminal_application::sessions::SessionChangeKind::Closed
                            ) =>
                        {
                            break true;
                        }
                        Ok(Some(_)) => continue,
                        Ok(None) | Err(_) => break false,
                    }
                };
                if !should_read_again {
                    break;
                }
            }
        })
        .detach();
    }

    /// 将权威快照替换为仅含安全状态的发现项，不保留命令正文或错误细节。
    ///
    /// @author kongweiguang
    fn apply_command_block_snapshot(
        &mut self,
        snapshot: CommandBlockSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.command_artifacts = command_block_artifact_index(&snapshot);
        cx.notify();
    }

    /// 查询失败只显示不可用，避免将缺失权限、断开或实现缺项误报成空结果。
    ///
    /// @author kongweiguang
    fn apply_artifact_error(
        &mut self,
        generation: u64,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        if self.artifact_generation != generation
            || self.artifact_session_id.as_ref() != Some(session_id)
        {
            return;
        }
        self.command_artifacts = projection::ContextArtifactIndex {
            state: ContextArtifactIndexState::Unavailable,
            entries: Vec::new(),
        };
        cx.notify();
    }

    /// 只保存真实列表或安全公开错误；不会把调用失败伪装成无关联 Agent。
    fn apply_agent_result(
        &mut self,
        result: kerminal_application::AppResult<Vec<AgentSessionView>>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(views) => {
                self.agent_views = views;
                self.agent_state = ContextAgentIndexState::Fresh;
            }
            Err(_) => self.agent_state = ContextAgentIndexState::Failed,
        }
        cx.notify();
    }
}

impl Render for ContextPanel {
    /// 主题令牌覆盖浅色、深色和跟随系统，所有内容仅展示安全元数据。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(window, cx)
    }
}

/// 结合面板隐藏与应用退出取消请求，同时继续等待只读查询响应完成。
///
/// @author kongweiguang
async fn await_agent_list_with_cancellation<F>(
    operation: F,
    service_cancellation: CancellationToken,
    panel_cancellation: CancellationToken,
    executor_cancellation: CancellationToken,
) -> kerminal_application::AppResult<Vec<AgentSessionView>>
where
    F: Future<Output = kerminal_application::AppResult<Vec<AgentSessionView>>> + Send,
{
    await_with_cooperative_cancellation(
        operation,
        service_cancellation,
        panel_cancellation,
        executor_cancellation,
    )
    .await
}

/// 通知用例取消后仍轮询原 future，直到它完成清理并返回真实结果。
///
/// @author kongweiguang
async fn await_with_cooperative_cancellation<T, F>(
    operation: F,
    service_cancellation: CancellationToken,
    panel_cancellation: CancellationToken,
    executor_cancellation: CancellationToken,
) -> T
where
    F: Future<Output = T> + Send,
{
    let mut operation = Box::pin(operation);
    let mut panel_cancelled = Box::pin(panel_cancellation.cancelled_owned());
    let mut executor_cancelled = Box::pin(executor_cancellation.cancelled_owned());
    let mut cancellation_sent = false;

    std::future::poll_fn(|context| {
        if let Poll::Ready(result) = operation.as_mut().poll(context) {
            return Poll::Ready(result);
        }
        if !cancellation_sent
            && (panel_cancelled.as_mut().poll(context).is_ready()
                || executor_cancelled.as_mut().poll(context).is_ready())
        {
            cancellation_sent = true;
            service_cancellation.cancel();
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
#[path = "../../../tests/unit/context_projection.rs"]
mod tests;
