//! Context inspector 的分区布局与只读导航。
//!
// @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::agents::AgentTargetLiveStatus;

use crate::theme::ThemeTokens;

use super::{
    super::projection::{agent_lifecycle, safe_context_text},
    super::{
        ContextAgentAssociation, ContextAgentIndexState, ContextArtifactIndexState,
        ContextNavigationTarget, ContextPanel, ContextPanelEvent, ContextSection,
    },
    agent_index_label, agent_lifecycle_label, agent_lifecycle_label_en, artifact_category_label,
    artifact_category_label_en, artifact_index_label, connection_kind_label,
    connection_kind_label_en, connection_status_label, detail_row, empty_note,
    location_confidence_label, location_source_label, section_container, section_content,
    section_header,
};

/// 让状态名称与计数留在同一 chip 内，外层仅在窄栏换行，并保留稳定元素 ID。
/// `id` 会把 GPUI `Div` 包装成 `Stateful<Div>`，所以对外返回消费端可直接嵌入的元素接口。
///
/// @author kongweiguang
fn render_runtime_counts(
    ready: usize,
    pending: usize,
    failed: usize,
    theme: ThemeTokens,
) -> impl IntoElement {
    let values = [
        (crate::locale::text("就绪", "Ready"), ready),
        (crate::locale::text("等待", "Pending"), pending),
        (crate::locale::text("失败", "Failed"), failed),
    ];
    let mut row = div()
        .id("context-runtime-status-counts")
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1();
    for (label, count) in values {
        row = row.child(
            div()
                .flex_1()
                .min_w(px(72.))
                .flex()
                .items_center()
                .justify_between()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(theme.secondary)
                .child(
                    div()
                        .min_w(px(0.))
                        .flex_shrink_1()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(theme.text_xs)
                        .text_color(theme.foreground)
                        .child(count.to_string()),
                ),
        );
    }
    row
}

impl ContextPanel {
    /// 标签导航只请求 Root 切换视图；本机名称可读且 tooltip 保留原 ID，分支统一擦除元素类型。
    ///
    /// @author kongweiguang
    pub(super) fn render_workspace_section(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let projection = self.projection.clone();
        let collapsed = self.section_collapsed(ContextSection::Workspace);
        let mut section = section_container("context-workspace", theme).child(section_header(
            ContextSection::Workspace,
            crate::locale::text("工作区", "Workspace"),
            collapsed,
            cx,
        ));
        if !collapsed {
            let machine_identity = projection.machine_id.clone();
            let machine_label = machine_identity
                .as_deref()
                .map(super::machine_display_label)
                .or_else(|| {
                    projection
                        .selected_host_id
                        .as_ref()
                        .map(|host_id| host_id.to_string().into())
                })
                .unwrap_or_else(|| crate::locale::text("未选择", "Not selected"));
            let machine_value = match machine_identity.as_deref() {
                Some(super::LOCAL_MACHINE_ID) => {
                    let machine_tooltip = machine_identity
                        .clone()
                        .unwrap_or_else(|| super::LOCAL_MACHINE_ID.to_owned());
                    div()
                        .id("context-machine-identity")
                        .tooltip(move |window, cx| {
                            Tooltip::new(machine_tooltip.clone()).build(window, cx)
                        })
                        .child(machine_label)
                        .into_any_element()
                }
                _ => div().child(machine_label).into_any_element(),
            };
            let mut content = section_content(
                "context-workspace-content",
                theme,
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(detail_row(
                        crate::locale::text("工作区", "Workspace"),
                        crate::locale::text(
                            projection.subject_title.clone(),
                            projection.subject_title.clone(),
                        ),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("机器", "Machine"),
                        machine_value,
                        theme,
                    )),
            );
            if projection.tabs.is_empty() {
                content = content.child(empty_note(
                    "当前没有可导航的标签页。",
                    "There are no navigable tabs.",
                    theme,
                ));
            } else {
                let mut tabs = div()
                    .id("context-tab-list")
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt_2();
                for tab in &projection.tabs {
                    let target = ContextNavigationTarget::Tab(tab.tab_id.clone());
                    let selected = tab.active;
                    let label = crate::locale::text(
                        format!(
                            "{} · {}{}",
                            tab.title,
                            connection_kind_label(tab.kind),
                            if selected { " · 当前" } else { "" }
                        ),
                        format!(
                            "{} · {}{}",
                            tab.title,
                            connection_kind_label_en(tab.kind),
                            if selected { " · active" } else { "" }
                        ),
                    );
                    tabs = tabs.child(
                        Button::new(format!("context-tab-{}", tab.tab_id.as_str()))
                            .w_full()
                            .compact()
                            .label(label)
                            .when(selected, Button::primary)
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(ContextPanelEvent::Navigate(target.clone()));
                            })),
                    );
                }
                content = content.child(tabs);
            }
            section = section.child(content);
        }
        section
    }

    /// 位置一律显示其来源和置信度，持久目录不会冒充 shell 的实时 cwd。
    pub(super) fn render_location_section(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let location = self.projection.location.clone();
        let collapsed = self.section_collapsed(ContextSection::Location);
        let mut section = section_container("context-location", theme).child(section_header(
            ContextSection::Location,
            crate::locale::text("位置", "Location"),
            collapsed,
            cx,
        ));
        if !collapsed {
            let path = location
                .path
                .clone()
                .unwrap_or_else(|| crate::locale::text("不可用", "Unavailable").to_string());
            section = section.child(section_content(
                "context-location-content",
                theme,
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(detail_row(crate::locale::text("路径", "Path"), path, theme))
                    .child(detail_row(
                        crate::locale::text("来源", "Source"),
                        location_source_label(location.source),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("可信度", "Confidence"),
                        location_confidence_label(location.confidence),
                        theme,
                    )),
            ));
        }
        section
    }

    /// Runtime 计数来自当前 Root map；各状态独立显示并按最小宽度换行，避免窄栏拆分标签。
    ///
    /// @author kongweiguang
    pub(super) fn render_resources_section(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let projection = self.projection.clone();
        let collapsed = self.section_collapsed(ContextSection::Resources);
        let mut section = section_container("context-resources", theme).child(section_header(
            ContextSection::Resources,
            crate::locale::text("资源与运行态", "Resources and runtime"),
            collapsed,
            cx,
        ));
        if !collapsed {
            let pane = projection.active_pane.as_ref();
            let mut content = section_content(
                "context-resources-content",
                theme,
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(detail_row(
                        crate::locale::text("标签页", "Tabs"),
                        projection.runtime.tab_count.to_string(),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("窗格", "Panes"),
                        projection.runtime.pane_count.to_string(),
                        theme,
                    ))
                    .child(render_runtime_counts(
                        projection.runtime.ready_pane_count,
                        projection.runtime.pending_pane_count,
                        projection.runtime.failed_pane_count,
                        theme,
                    )),
            );
            if let Some(pane) = pane {
                let target = ContextNavigationTarget::Pane(pane.pane_id.clone());
                content = content.child(
                    Button::new(format!("context-active-pane-{}", pane.pane_id.as_str()))
                        .w_full()
                        .compact()
                        .label(crate::locale::text(
                            format!("活动窗格：{}", pane.title),
                            format!("Active pane: {}", pane.title),
                        ))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.emit(ContextPanelEvent::Navigate(target.clone()));
                        })),
                );
                content = content
                    .child(detail_row(
                        crate::locale::text("连接", "Connection"),
                        connection_status_label(pane.status),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("网格", "Grid"),
                        format!("{} × {}", pane.columns, pane.rows),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("Shell", "Shell"),
                        pane.shell
                            .clone()
                            .unwrap_or_else(|| crate::locale::text("未知", "Unknown").to_string()),
                        theme,
                    ));
            } else {
                content = content.child(empty_note(
                    "当前活动标签页没有可用终端窗格。",
                    "The active tab has no available terminal pane.",
                    theme,
                ));
            }
            content = content.child(detail_row(
                crate::locale::text("往返延迟", "Round-trip latency"),
                projection.runtime.latency_ms.map_or_else(
                    || crate::locale::text("未采集", "Not collected").to_string(),
                    |latency| format!("{latency} ms"),
                ),
                theme,
            ));
            content = content.child(detail_row(
                crate::locale::text("tmux", "tmux"),
                projection.runtime.tmux_attached.map_or_else(
                    || crate::locale::text("未采集", "Not collected").to_string(),
                    |attached| {
                        if attached {
                            crate::locale::text("已连接", "Attached").to_string()
                        } else {
                            crate::locale::text("未连接", "Not attached").to_string()
                        }
                    },
                ),
                theme,
            ));
            section = section.child(content);
        }
        section
    }

    /// 先展示右栏明确选中的 Agent，再展示当前 pane 的精确关联项，不猜测替代目标。
    pub(super) fn render_agent_section(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.section_collapsed(ContextSection::Agent);
        let mut section = section_container("context-agents", theme).child(section_header(
            ContextSection::Agent,
            crate::locale::text("关联 Agent", "Associated agents"),
            collapsed,
            cx,
        ));
        if !collapsed {
            let mut content = section_content(
                "context-agents-content",
                theme,
                div().flex().flex_col().gap_1().child(detail_row(
                    crate::locale::text("列表状态", "List status"),
                    agent_index_label(self.agent_index_state()),
                    theme,
                )),
            );
            if let Some(agent_id) = self.selected_agent_id.as_ref() {
                content = content.child(self.render_selected_agent(agent_id.as_str(), theme));
            }
            match self.agent_index_state() {
                ContextAgentIndexState::Unavailable => {
                    content = content.child(empty_note(
                        "Agent 服务不可用。",
                        "Agent service is unavailable.",
                        theme,
                    ));
                }
                ContextAgentIndexState::Loading => {
                    content = content.child(empty_note(
                        "正在读取 Agent 会话。",
                        "Loading Agent sessions.",
                        theme,
                    ));
                }
                ContextAgentIndexState::Failed => {
                    content = content.child(empty_note(
                        "读取失败；刷新后重试。",
                        "Could not load sessions; refresh to retry.",
                        theme,
                    ));
                }
                ContextAgentIndexState::Fresh => {
                    let agents = self.associated_agents();
                    if agents.is_empty() {
                        content = content.child(empty_note(
                            "没有与当前目标明确关联的 Agent。",
                            "No Agent is explicitly associated with this target.",
                            theme,
                        ));
                    } else {
                        for agent in agents {
                            let target =
                                ContextNavigationTarget::Agent(agent.agent_session_id.clone());
                            content = content.child(
                                Button::new(format!(
                                    "context-agent-{}",
                                    agent.agent_session_id.as_str()
                                ))
                                .w_full()
                                .compact()
                                .label(crate::locale::text(
                                    format!(
                                        "{} · {} · {}",
                                        agent.title,
                                        agent.provider,
                                        agent_lifecycle_label(agent.lifecycle)
                                    ),
                                    format!(
                                        "{} · {} · {}",
                                        agent.title,
                                        agent.provider,
                                        agent_lifecycle_label_en(agent.lifecycle)
                                    ),
                                ))
                                .when(
                                    agent.association == ContextAgentAssociation::Bound,
                                    Button::primary,
                                )
                                .on_click(cx.listener(
                                    move |_, _: &ClickEvent, _, cx| {
                                        cx.emit(ContextPanelEvent::Navigate(target.clone()));
                                    },
                                )),
                            );
                        }
                    }
                }
            }
            section = section.child(content);
        }
        section
    }

    /// 显示 Root 显式选择的 Agent 状态；选择已失效时保留身份并告知用户，不回退到列表首项。
    ///
    /// @author kongweiguang
    fn render_selected_agent(
        &self,
        agent_session_id: &str,
        theme: ThemeTokens,
    ) -> impl IntoElement {
        let selected = self.selected_agent();
        let mut details = div().flex().flex_col().gap_1();
        match selected {
            Some(view) => {
                let binding_status = match view
                    .session
                    .target
                    .as_ref()
                    .map(|target| target.live_status)
                {
                    None | Some(AgentTargetLiveStatus::Unbound) => {
                        crate::locale::text("未绑定", "Unbound")
                    }
                    Some(AgentTargetLiveStatus::Ready) => {
                        crate::locale::text("原绑定仍有效", "Original binding is live")
                    }
                    Some(AgentTargetLiveStatus::Stale) => {
                        crate::locale::text("目标失效", "Target unavailable")
                    }
                    Some(AgentTargetLiveStatus::Closed) => {
                        crate::locale::text("目标已关闭", "Target closed")
                    }
                };
                details = details
                    .child(detail_row(
                        crate::locale::text("当前选择", "Selected session"),
                        crate::locale::text(
                            format!(
                                "{} · {}",
                                safe_context_text(&view.session.title),
                                view.session.agent_id.as_str()
                            ),
                            format!(
                                "{} · {}",
                                safe_context_text(&view.session.title),
                                view.session.agent_id.as_str()
                            ),
                        ),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("会话状态", "Session status"),
                        crate::locale::text(
                            agent_lifecycle_label(agent_lifecycle(view)),
                            agent_lifecycle_label_en(agent_lifecycle(view)),
                        ),
                        theme,
                    ))
                    .child(detail_row(
                        crate::locale::text("目标绑定", "Target binding"),
                        binding_status,
                        theme,
                    ));
            }
            None => {
                let unavailable = match self.agent_index_state() {
                    ContextAgentIndexState::Unavailable => {
                        crate::locale::text("Agent 服务不可用", "Agent service unavailable")
                    }
                    ContextAgentIndexState::Loading => {
                        crate::locale::text("正在读取会话", "Loading session")
                    }
                    ContextAgentIndexState::Failed => {
                        crate::locale::text("列表读取失败", "Session list failed")
                    }
                    ContextAgentIndexState::Fresh => crate::locale::text(
                        "列表中不可用；不会自动切换其他会话",
                        "Unavailable; another session will not be selected automatically",
                    ),
                };
                details = details.child(detail_row(
                    crate::locale::text("当前选择", "Selected session"),
                    crate::locale::text(
                        format!("{agent_session_id} · {unavailable}"),
                        format!("{agent_session_id} · {unavailable}"),
                    ),
                    theme,
                ));
            }
        }
        div()
            .id("context-selected-agent")
            .w_full()
            .p_2()
            .rounded_md()
            .bg(theme.panel)
            .child(details)
    }

    /// 显示真实安全发现项；未连接索引与成功空结果使用不同文案。
    pub(super) fn render_discoveries_section(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let links = self.projection.terminal_links.clone();
        let commands = self.command_artifacts.clone();
        let collapsed = self.section_collapsed(ContextSection::Discoveries);
        let mut section = section_container("context-discoveries", theme).child(section_header(
            ContextSection::Discoveries,
            crate::locale::text("安全发现", "Safe discoveries"),
            collapsed,
            cx,
        ));
        if !collapsed {
            let content = section_content(
                "context-discoveries-content",
                theme,
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(render_terminal_links(&links, theme, cx))
                    .child(render_command_summaries(&commands, theme)),
            );
            section = section.child(content);
        }
        section
    }
}

/// 终端链接投影只显示验证后的短标签；过期快照中的候选一律禁用。
///
/// @author kongweiguang
fn render_terminal_links(
    links: &super::super::ContextTerminalLinkIndex,
    theme: ThemeTokens,
    cx: &mut Context<ContextPanel>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let mut content = div()
        .id("context-terminal-links")
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(detail_row(
            crate::locale::text("终端链接", "Terminal links"),
            artifact_index_label(&links.state),
            theme,
        ));

    match links.state {
        ContextArtifactIndexState::NotConnected => {
            content = content.child(empty_note(
                "终端链接索引尚未接入。",
                "The terminal link index is not connected.",
                theme,
            ));
        }
        ContextArtifactIndexState::Unavailable => {
            content = content.child(empty_note(
                "当前终端链接不可用。",
                "Terminal links are unavailable for this pane.",
                theme,
            ));
        }
        ContextArtifactIndexState::Loading => {
            content = content.child(empty_note(
                "正在索引当前终端视口。",
                "Indexing the current terminal viewport.",
                theme,
            ));
        }
        ContextArtifactIndexState::Partial => {
            content = content.child(empty_note(
                "已达到扫描预算；列表可能不完整。",
                "The scan budget was reached; this list may be incomplete.",
                theme,
            ));
        }
        ContextArtifactIndexState::Stale => {
            content = content.child(empty_note(
                "视口已变化；缓存链接仅供查看，不能打开。",
                "The viewport changed; cached links are read-only.",
                theme,
            ));
        }
        ContextArtifactIndexState::Available if links.entries.is_empty() => {
            content = content.child(empty_note(
                "当前视口没有发现链接。",
                "No links were found in the current viewport.",
                theme,
            ));
        }
        ContextArtifactIndexState::Available => {}
    }

    let can_open = matches!(
        links.state,
        ContextArtifactIndexState::Available | ContextArtifactIndexState::Partial
    );
    for (index, entry) in links.entries.iter().enumerate() {
        let identity = links.identity.clone();
        let candidate_id = entry.id;
        let label = crate::locale::text(
            format!(
                "{} · {} · {}",
                index + 1,
                link_source_label(entry.source),
                entry.label
            ),
            format!(
                "{} · {} · {}",
                index + 1,
                link_source_label_en(entry.source),
                entry.label
            ),
        );
        content = content.child(
            Button::new(format!("context-terminal-link-{index}"))
                .w_full()
                .compact()
                .label(label)
                .disabled(!can_open || identity.is_none())
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    if let Some(identity) = identity.clone() {
                        cx.emit(ContextPanelEvent::OpenLink {
                            identity,
                            candidate_id,
                        });
                    }
                })),
        );
    }
    content
}

/// 命令块状态与终端 URL 索引分开呈现，避免一侧刷新覆盖另一侧的来源结果。
///
/// @author kongweiguang
fn render_command_summaries(
    commands: &super::super::ContextArtifactIndex,
    theme: ThemeTokens,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let mut content = div()
        .id("context-command-blocks")
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(detail_row(
            crate::locale::text("命令块", "Command blocks"),
            artifact_index_label(&commands.state),
            theme,
        ));
    match commands.state {
        ContextArtifactIndexState::NotConnected => {
            content = content.child(empty_note(
                "OSC 133 命令块来源尚未接入。",
                "The OSC 133 command-block source is not connected.",
                theme,
            ));
        }
        ContextArtifactIndexState::Unavailable => {
            content = content.child(empty_note(
                "无法读取当前终端的命令块状态。",
                "Command-block state is unavailable for the active terminal.",
                theme,
            ));
        }
        ContextArtifactIndexState::Loading => {
            content = content.child(empty_note(
                "正在读取当前终端的命令块状态。",
                "Reading command-block state for the active terminal.",
                theme,
            ));
        }
        ContextArtifactIndexState::Partial => {
            content = content.child(empty_note(
                "只显示可信状态摘要，命令正文已过滤。",
                "Only trusted status summaries are shown; command text is filtered.",
                theme,
            ));
        }
        ContextArtifactIndexState::Stale => {
            content = content.child(empty_note(
                "命令块缓存已过期。",
                "The command-block snapshot is stale.",
                theme,
            ));
        }
        ContextArtifactIndexState::Available => {}
    }
    if commands.entries.is_empty()
        && matches!(
            commands.state,
            ContextArtifactIndexState::Available | ContextArtifactIndexState::Partial
        )
    {
        content = content.child(empty_note(
            "当前没有命令块。",
            "There are no command blocks in this session.",
            theme,
        ));
    }
    for (index, entry) in commands.entries.iter().enumerate() {
        content = content.child(detail_row(
            crate::locale::text(
                format!(
                    "{} · {}",
                    artifact_category_label(entry.category),
                    index + 1
                ),
                format!(
                    "{} · {}",
                    artifact_category_label_en(entry.category),
                    index + 1
                ),
            ),
            entry.title.clone(),
            theme,
        ));
    }
    content
}

/// 链接来源只显示 OSC8 或普通文本索引类别，不将原始终端文本带入标签。
///
/// @author kongweiguang
fn link_source_label(source: crate::terminal_canvas::TerminalLinkSource) -> &'static str {
    match source {
        crate::terminal_canvas::TerminalLinkSource::Osc8 => "OSC8 链接",
        crate::terminal_canvas::TerminalLinkSource::Text => "文本链接",
    }
}

/// 英文来源标签与安全候选类型一一对应。
fn link_source_label_en(source: crate::terminal_canvas::TerminalLinkSource) -> &'static str {
    match source {
        crate::terminal_canvas::TerminalLinkSource::Osc8 => "OSC8 link",
        crate::terminal_canvas::TerminalLinkSource::Text => "Text link",
    }
}
