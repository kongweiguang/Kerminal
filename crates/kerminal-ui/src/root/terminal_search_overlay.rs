//! Root 将搜索控件作为独立浮层挂到终端 Canvas 上，不参与 PTY 网格布局。
//!
//! @author kongweiguang

use gpui_kit::{AnyElement, Context, Entity, div, prelude::*, px};
use kerminal_domain::identity::SessionId;

use crate::{
    features::terminal_tools::{SearchOverlayPlacement, TerminalSearchToolbar},
    terminal_metrics::TerminalMetrics,
};

use super::{
    RootView,
    terminal_tools_runtime::{TerminalToolsRequestKind, resolve_owner},
};

const OVERLAY_EDGE_GAP: f32 = 8.0;
const OVERLAY_MAX_WIDTH: f32 = 440.0;

impl RootView {
    /// 目标失效先同步撤销搜索提交门，再挂起输入计时器；不影响同一工具里的选择或日志任务。
    ///
    /// @author kongweiguang
    pub(super) fn suspend_terminal_search(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        let sources = self
            .workspace_tools
            .get(session_id)
            .cloned()
            .into_iter()
            .chain(
                self.agent_terminal_tools
                    .as_ref()
                    .filter(|tools| tools.read(cx).session_id() == Some(session_id))
                    .cloned(),
            )
            .collect::<Vec<_>>();
        for source in sources {
            if let Some(owner) = resolve_owner(self, &source, session_id, cx) {
                self.terminal_tools_runtime
                    .cancel(&owner, TerminalToolsRequestKind::Search);
            }
            source.update(cx, |tools, cx| {
                tools.set_search_suspended(true, cx);
            });
        }
    }

    /// 按整个活动 Tab 的可见性恢复查询，保留分屏中非聚焦 pane 的搜索且不转移物理焦点。
    ///
    /// Toolbar 的 suspended 位充当幂等门；重复投影不会重新计时，隐藏期间也不能重投迟到结果。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_workspace_search_visibility(&mut self, cx: &mut Context<Self>) {
        let sources = self
            .workspace_tools
            .iter()
            .map(|(session_id, source)| {
                let visible = self.terminal_view_bindings.contains_key(session_id)
                    && self
                        .workspace_runtime
                        .pane_for_session(session_id)
                        .and_then(|pane_id| self.workspace_runtime.pane(pane_id))
                        .is_some_and(|pane| self.workspace_tab_is_visible(&pane.tab_id));
                (session_id.clone(), source.clone(), visible)
            })
            .collect::<Vec<_>>();
        for (session_id, source, visible) in sources {
            if !visible && let Some(owner) = resolve_owner(self, &source, &session_id, cx) {
                self.terminal_tools_runtime
                    .cancel(&owner, TerminalToolsRequestKind::Search);
            }
            source.update(cx, |tools, cx| {
                tools.set_search_suspended(!visible, cx);
            });
        }
    }

    /// Agent Dock 按最终布局决定是否恢复，避免中央页切换误取消仍在右栏展示的搜索。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_agent_search_visibility(
        &mut self,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.agent_terminal_tools.clone() else {
            return;
        };
        let session_id = source.read(cx).session_id().cloned();
        let visible = visible
            && session_id
                .as_ref()
                .is_some_and(|id| self.terminal_view_bindings.contains_key(id));
        if !visible
            && let Some(session_id) = session_id
            && let Some(owner) = resolve_owner(self, &source, &session_id, cx)
        {
            self.terminal_tools_runtime
                .cancel(&owner, TerminalToolsRequestKind::Search);
        }
        source.update(cx, |tools, cx| {
            tools.set_search_suspended(!visible, cx);
        });
    }
}

/// 把查询卡片绝对定位在 Canvas 内并从同帧实测值推导 Reveal 安全行数。
///
/// 绝对定位保证搜索显隐、窄屏换行和错误文案不参与 Canvas 的 flex 尺寸；卡片仍由单个稳定
/// Entity 绘制，输入框的 GPUI/IME 状态不因 Root 重绘而重建。
///
/// @author kongweiguang
pub(crate) fn terminal_search_overlay_element(
    search: Entity<TerminalSearchToolbar>,
    placement: SearchOverlayPlacement,
    metrics: &TerminalMetrics,
) -> AnyElement {
    let line_height = metrics.line_height;
    let terminal_inset = metrics.inset;
    let measured_search = search.clone();

    div()
        .on_children_prepainted(move |children, _, app| {
            let Some(bounds) = children.first() else {
                return;
            };
            let safe_top_rows =
                measured_safe_top_rows(bounds.size.height.as_f32(), line_height, terminal_inset);
            measured_search.update(app, |search, cx| {
                search.set_overlay_metrics(bounds.size.width, safe_top_rows, cx);
            });
        })
        .id("terminal-search-overlay-positioner")
        .absolute()
        .left(px(OVERLAY_EDGE_GAP))
        .right(px(OVERLAY_EDGE_GAP))
        .when(placement == SearchOverlayPlacement::TopRight, |element| {
            element.top(px(OVERLAY_EDGE_GAP))
        })
        .when(
            placement == SearchOverlayPlacement::BottomRight,
            |element| element.bottom(px(OVERLAY_EDGE_GAP)),
        )
        .flex()
        .justify_end()
        .items_start()
        .child(
            div()
                .id("terminal-search-overlay-card-bounds")
                .debug_selector(|| "terminal-search-overlay-card-bounds".to_owned())
                .w_full()
                .max_w(px(OVERLAY_MAX_WIDTH))
                .min_w(px(0.))
                .child(search),
        )
        .into_any_element()
}

/// 将卡片高度换算成假设靠上时的遮挡；置底后仍保留此值，让下一次导航重新尝试回到顶部。
/// 实测高度同时覆盖不同 DPI、窄窗换行和错误展开，避免底部卡片永远无法恢复且遮住新命中。
///
/// @author kongweiguang
fn measured_safe_top_rows(overlay_height: f32, line_height: f32, terminal_inset: f32) -> u16 {
    if !overlay_height.is_finite()
        || !line_height.is_finite()
        || !terminal_inset.is_finite()
        || overlay_height < 0.0
        || line_height <= 0.0
        || terminal_inset < 0.0
    {
        return 0;
    }

    let covered_height = (OVERLAY_EDGE_GAP + overlay_height - terminal_inset).max(0.0);
    (covered_height / line_height)
        .ceil()
        .clamp(0.0, f32::from(u16::MAX)) as u16
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_search_overlay.rs"]
mod tests;
