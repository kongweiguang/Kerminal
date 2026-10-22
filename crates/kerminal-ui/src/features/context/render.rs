//! Context inspector 的主题化展示与只读导航。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Window,
    component::{Disableable, button::Button},
    div,
    prelude::*,
    px,
};

use crate::theme::ThemeTokens;

mod sections;

/// 只有这个稳定的 typed machine ID 可安全翻译为用户熟悉的本机名称。
pub(super) const LOCAL_MACHINE_ID: &str = "machine-local";

/// 仅翻译已知本机身份，未知和远程机器标识保持原文，避免猜测主机名称。
///
/// @author kongweiguang
pub(super) fn machine_display_label(machine_id: &str) -> gpui_kit::SharedString {
    if machine_id == LOCAL_MACHINE_ID {
        crate::locale::text("本机", "Local machine")
    } else {
        machine_id.to_owned().into()
    }
}

use super::{
    ContextAgentIndexState, ContextAgentLifecycle, ContextArtifactCategory,
    ContextArtifactIndexState, ContextConnectionKind, ContextConnectionStatus,
    ContextDiagnosticSeverity, ContextLocationConfidence, ContextLocationSource,
    ContextNavigationTarget, ContextPanel, ContextPanelEvent, ContextProjectionStatus,
    ContextSection, ContextSourceStatus,
};

impl ContextPanel {
    /// 根层使用 shell 材质显示右栏壁纸，内部来源与状态内容仍由不透明卡片承载。
    ///
    /// @author kongweiguang
    pub(super) fn render_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let projection = self.projection.clone();
        let mut body = div()
            .id("context-inspector-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_3();

        body = body.child(self.render_target_section(theme, cx));
        body = body.child(self.render_workspace_section(theme, cx));
        body = body.child(self.render_location_section(theme, cx));
        body = body.child(self.render_resources_section(theme, cx));
        body = body.child(self.render_agent_section(theme, cx));
        body = body.child(self.render_discoveries_section(theme, cx));
        body = body.child(render_sources(theme, &projection.sources));
        body = body.child(render_diagnostics(theme, &projection.diagnostics));

        div()
            .id("context-inspector")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.shell)
            .text_color(theme.foreground)
            .child(self.render_header(theme, cx))
            .child(body)
    }

    /// 显示 Context 状态并提供不改变目标绑定的手动刷新。
    ///
    /// @author kongweiguang
    fn render_header(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = matches!(&self.agent_state, ContextAgentIndexState::Loading);
        div()
            .id("context-inspector-header")
            .w_full()
            .h(px(48.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.panel)
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(crate::locale::text("上下文", "Context")),
            )
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(projection_status_label(self.projection.status)),
            )
            .child(
                Button::new("context-inspector-refresh")
                    .compact()
                    .label(if busy {
                        crate::locale::text("同步中…", "Refreshing…")
                    } else {
                        crate::locale::text("刷新", "Refresh")
                    })
                    .disabled(self.agents.is_none())
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.refresh(cx))),
            )
    }

    /// 首屏展示当前真实 target；失败或缺少目标时明确区分状态。
    fn render_target_section(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.section_collapsed(ContextSection::Target);
        let mut section = section_container("context-target", theme).child(section_header(
            ContextSection::Target,
            crate::locale::text("当前目标", "Current target"),
            collapsed,
            cx,
        ));
        if !collapsed {
            section = section.child(section_content(
                "context-target-content",
                theme,
                render_target_card(self, theme, cx),
            ));
        }
        section
    }
}

/// 固定分区标题使用原生按钮，折叠只影响显示不重建任何应用服务。
fn section_header(
    section: ContextSection,
    title: gpui_kit::SharedString,
    collapsed: bool,
    cx: &mut Context<ContextPanel>,
) -> Button {
    Button::new(format!("context-section-{section:?}"))
        .w_full()
        .compact()
        .label(if collapsed {
            crate::locale::text(format!("▸ {title}"), format!("▸ {title}"))
        } else {
            crate::locale::text(format!("▾ {title}"), format!("▾ {title}"))
        })
        .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
            panel.toggle_section(section, cx);
        }))
}

/// 为各分区提供相同的背景、间距和圆角，保证浅深主题均由语义令牌绘制。
fn section_container(id: &'static str, theme: ThemeTokens) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(theme.panel)
}

/// 统一分区内部的留白，避免详情字段在高密度右栏中相互粘连。
fn section_content(
    id: &'static str,
    theme: ThemeTokens,
    content: impl IntoElement,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .py_1()
        .bg(theme.panel)
        .child(content)
}

/// 展示标签与值时限制布局宽度，并允许长路径换行。
///
/// @author kongweiguang
fn detail_row(
    label: gpui_kit::SharedString,
    value: impl IntoElement,
    theme: ThemeTokens,
) -> gpui_kit::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .w(px(92.))
                .flex_shrink_0()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_xs)
                .text_color(theme.foreground)
                .child(value),
        )
}

/// 明确的空态提示保留来源状态，不把加载失败或未接线写成无结果。
///
/// @author kongweiguang
fn empty_note(zh: &'static str, en: &'static str, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(crate::locale::text(zh, en))
}

/// Target 卡片的 host 导航只在有准确 HostId 时显示，不按名字匹配连接。
fn render_target_card(
    panel: &mut ContextPanel,
    theme: ThemeTokens,
    cx: &mut Context<ContextPanel>,
) -> gpui_kit::AnyElement {
    let target = panel.projection.active_target.clone();
    let Some(target) = target else {
        return empty_note("当前没有可用目标。", "No target is available.", theme)
            .into_any_element();
    };
    let mut card = div()
        .id("context-target-card")
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(detail_row(
            crate::locale::text("类型", "Kind"),
            crate::locale::text(
                connection_kind_label(target.kind),
                connection_kind_label_en(target.kind),
            ),
            theme,
        ))
        .child(detail_row(
            crate::locale::text("名称", "Name"),
            target.label.clone(),
            theme,
        ));
    if let Some(host_id) = target.host_id.clone() {
        let navigation = ContextNavigationTarget::Host(host_id.clone());
        card = card.child(
            Button::new(format!("context-host-{}", host_id.as_str()))
                .w_full()
                .compact()
                .label(crate::locale::text(
                    format!("主机：{}", host_id.as_str()),
                    format!("Host: {}", host_id.as_str()),
                ))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(ContextPanelEvent::Navigate(navigation.clone()));
                })),
        );
    }
    if let Some(container_id) = target.container_id.as_ref() {
        card = card.child(detail_row(
            crate::locale::text("容器", "Container"),
            container_id.clone(),
            theme,
        ));
    }
    if target.reference.is_some() {
        card = card.child(detail_row(
            crate::locale::text("引用", "Reference"),
            target.reference.clone().unwrap_or_default(),
            theme,
        ));
    }
    card.into_any_element()
}

/// 各状态显示固定中英文字串，不透传底层错误和未知 enum debug。
fn projection_status_label(status: ContextProjectionStatus) -> gpui_kit::SharedString {
    match status {
        ContextProjectionStatus::NoWorkspace => crate::locale::text("无工作区", "No workspace"),
        ContextProjectionStatus::Restoring => crate::locale::text("恢复中", "Restoring"),
        ContextProjectionStatus::Ready => crate::locale::text("就绪", "Ready"),
        ContextProjectionStatus::Partial => crate::locale::text("部分数据", "Partial data"),
        ContextProjectionStatus::Unavailable => crate::locale::text("不可用", "Unavailable"),
    }
}

/// Agent 查询状态明确区分未装配、加载、真实空结果和错误。
fn agent_index_label(state: &ContextAgentIndexState) -> gpui_kit::SharedString {
    match state {
        ContextAgentIndexState::Unavailable => crate::locale::text("不可用", "Unavailable"),
        ContextAgentIndexState::Loading => crate::locale::text("读取中", "Loading"),
        ContextAgentIndexState::Fresh => crate::locale::text("已刷新", "Up to date"),
        ContextAgentIndexState::Failed => crate::locale::text("读取失败", "Read failed"),
    }
}

/// 位置来源标签强调 snapshot 与实时 shell 状态的差异。
fn location_source_label(source: ContextLocationSource) -> gpui_kit::SharedString {
    match source {
        ContextLocationSource::LiveShellIntegration => {
            crate::locale::text("Shell 实时状态", "Live shell state")
        }
        ContextLocationSource::PersistedCurrentDirectory => {
            crate::locale::text("已保存的当前目录", "Persisted current directory")
        }
        ContextLocationSource::ConfiguredStartDirectory => {
            crate::locale::text("启动目录", "Configured start directory")
        }
        ContextLocationSource::WorkspaceFile => crate::locale::text("工作区文件", "Workspace file"),
        ContextLocationSource::Unavailable => crate::locale::text("未采集", "Not collected"),
    }
}

/// 可信度作为独立状态呈现，避免用户把旧 cwd 当作最新事实。
fn location_confidence_label(confidence: ContextLocationConfidence) -> gpui_kit::SharedString {
    match confidence {
        ContextLocationConfidence::High => crate::locale::text("高", "High"),
        ContextLocationConfidence::Medium => crate::locale::text("中", "Medium"),
        ContextLocationConfidence::Low => crate::locale::text("低", "Low"),
        ContextLocationConfidence::Unknown => crate::locale::text("未知", "Unknown"),
    }
}

/// 连接状态映射固定且中性，不展示异常正文或主机凭据信息。
fn connection_status_label(status: ContextConnectionStatus) -> gpui_kit::SharedString {
    match status {
        ContextConnectionStatus::Ready => crate::locale::text("就绪", "Ready"),
        ContextConnectionStatus::Starting => crate::locale::text("启动中", "Starting"),
        ContextConnectionStatus::Restoring => crate::locale::text("恢复中", "Restoring"),
        ContextConnectionStatus::Closing => crate::locale::text("关闭中", "Closing"),
        ContextConnectionStatus::Failed => crate::locale::text("失败", "Failed"),
        ContextConnectionStatus::Offline => crate::locale::text("离线", "Offline"),
    }
}

/// 列表来源的状态不会把 Stale 或 Error 与正常可用混在一起。
fn source_status_label(status: ContextSourceStatus) -> gpui_kit::SharedString {
    match status {
        ContextSourceStatus::Available => crate::locale::text("可用", "Available"),
        ContextSourceStatus::Loading => crate::locale::text("读取中", "Loading"),
        ContextSourceStatus::Stale => crate::locale::text("已过期", "Stale"),
        ContextSourceStatus::Unavailable => crate::locale::text("未接入", "Unavailable"),
        ContextSourceStatus::Error => crate::locale::text("错误", "Error"),
    }
}

/// Runtime 数据来源逐项展示，避免将未采集的 latency/tmux 状态补成默认值。
fn render_sources(
    theme: ThemeTokens,
    sources: &[super::projection::ContextSourceSummary],
) -> impl IntoElement {
    let mut content = div()
        .id("context-sources")
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text("数据新鲜度", "Data freshness")),
        );
    if sources.is_empty() {
        content = content.child(empty_note(
            "没有可报告的数据来源。",
            "No data sources are reported.",
            theme,
        ));
    } else {
        for source in sources {
            let updated = source.updated_at.as_deref().unwrap_or("—");
            content = content.child(detail_row(
                crate::locale::text(source.source.clone(), source.source.clone()),
                crate::locale::text(
                    format!("{} · {updated}", source_status_label(source.status)),
                    format!("{} · {updated}", source_status_label(source.status)),
                ),
                theme,
            ));
        }
    }
    content
}

/// 诊断按固定 severity 样式呈现，summary 仅来自 Root 的安全分类文案。
///
/// @author kongweiguang
fn render_diagnostics(
    theme: ThemeTokens,
    diagnostics: &[super::projection::ContextDiagnosticSummary],
) -> impl IntoElement {
    let mut content = div()
        .id("context-diagnostics")
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text("诊断", "Diagnostics")),
        );
    if diagnostics.is_empty() {
        content = content.child(empty_note("没有诊断项。", "No diagnostics.", theme));
    } else {
        for diagnostic in diagnostics {
            let severity = match diagnostic.severity {
                ContextDiagnosticSeverity::Info => crate::locale::text("信息", "Info"),
                ContextDiagnosticSeverity::Warning => crate::locale::text("警告", "Warning"),
                ContextDiagnosticSeverity::Error => crate::locale::text("错误", "Error"),
            };
            content = content.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .text_size(theme.text_xs)
                    .text_color(if diagnostic.severity == ContextDiagnosticSeverity::Error {
                        theme.error
                    } else {
                        theme.foreground
                    })
                    .child(format!("{} · {}", severity, diagnostic.code))
                    .child(
                        div()
                            .min_w(px(0.))
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(diagnostic.summary.clone()),
                    ),
            );
        }
    }
    content
}

/// 安全索引状态采用明确文案，让“未接入”不会误导为“扫描后为空”。
fn artifact_index_label(state: &ContextArtifactIndexState) -> gpui_kit::SharedString {
    match state {
        ContextArtifactIndexState::Available => crate::locale::text("可用", "Available"),
        ContextArtifactIndexState::Partial => crate::locale::text("部分结果", "Partial results"),
        ContextArtifactIndexState::Loading => crate::locale::text("读取中", "Loading"),
        ContextArtifactIndexState::Stale => crate::locale::text("已过期", "Stale"),
        ContextArtifactIndexState::NotConnected => crate::locale::text("未接入", "Not connected"),
        ContextArtifactIndexState::Unavailable => crate::locale::text("不可用", "Unavailable"),
    }
}

/// 安全发现只显示类别与脱敏标题，不把命令正文或终端输出放入 Context。
fn artifact_category_label(category: ContextArtifactCategory) -> &'static str {
    match category {
        ContextArtifactCategory::Link => "链接",
        ContextArtifactCategory::Command => "命令",
    }
}

/// 英文发现类型文案与中文标签一一对应。
fn artifact_category_label_en(category: ContextArtifactCategory) -> &'static str {
    match category {
        ContextArtifactCategory::Link => "Link",
        ContextArtifactCategory::Command => "Command",
    }
}

/// Agent 生命周期文案与目标关联状态分开，target stale 不会改写 CLI lifecycle。
fn agent_lifecycle_label(lifecycle: ContextAgentLifecycle) -> gpui_kit::SharedString {
    match lifecycle {
        ContextAgentLifecycle::Running => crate::locale::text("运行中", "Running"),
        ContextAgentLifecycle::ReadyToResume => crate::locale::text("可继续", "Ready to resume"),
        ContextAgentLifecycle::Exited => crate::locale::text("已退出", "Exited"),
        ContextAgentLifecycle::Stale => crate::locale::text("状态未知", "Stale"),
        ContextAgentLifecycle::RecoveryPending => crate::locale::text("待核对", "Recovery pending"),
        ContextAgentLifecycle::Archived => crate::locale::text("已归档", "Archived"),
    }
}

/// English Agent lifecycle labels are stable user-facing strings, not Debug output.
fn agent_lifecycle_label_en(lifecycle: ContextAgentLifecycle) -> &'static str {
    match lifecycle {
        ContextAgentLifecycle::Running => "Running",
        ContextAgentLifecycle::ReadyToResume => "Ready to resume",
        ContextAgentLifecycle::Exited => "Exited",
        ContextAgentLifecycle::Stale => "Stale",
        ContextAgentLifecycle::RecoveryPending => "Recovery pending",
        ContextAgentLifecycle::Archived => "Archived",
    }
}

/// Target type uses a fixed localized table and never derives labels from raw protocol values.
fn connection_kind_label(kind: ContextConnectionKind) -> &'static str {
    match kind {
        ContextConnectionKind::Terminal => "终端",
        ContextConnectionKind::Local => "本地",
        ContextConnectionKind::Ssh => "SSH",
        ContextConnectionKind::Telnet => "Telnet",
        ContextConnectionKind::Serial => "串口",
        ContextConnectionKind::Container => "容器",
        ContextConnectionKind::FileWorkspace => "文件工作区",
        ContextConnectionKind::Unsupported => "未支持",
    }
}

/// 标签显示文案与持久化中使用的标识分离。
fn connection_kind_label_en(kind: ContextConnectionKind) -> &'static str {
    match kind {
        ContextConnectionKind::Terminal => "Terminal",
        ContextConnectionKind::Local => "Local",
        ContextConnectionKind::Ssh => "SSH",
        ContextConnectionKind::Telnet => "Telnet",
        ContextConnectionKind::Serial => "Serial",
        ContextConnectionKind::Container => "Container",
        ContextConnectionKind::FileWorkspace => "File workspace",
        ContextConnectionKind::Unsupported => "Unsupported",
    }
}
