//! Logs 面板与命令工作区共用的历史查询范围和分页规则。
//!
//! @author kongweiguang

use kerminal_application::commands::CommandHistoryClearRequest;
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget,
    },
    identity::{HostId, PaneId, SessionId},
};
use tokio_util::sync::CancellationToken;

/// 读取槽把忙碌、协作取消和代次放在一起，旧回执不能解除新搜索的锁定。
#[derive(Default)]
pub(super) struct HistoryReadState {
    generation: u64,
    cancellation: Option<CancellationToken>,
}

impl HistoryReadState {
    /// 整页加载捕获同一版本；后续独立搜索一旦开始，旧整页历史不得回写。
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// 槽位存在即表示尚未收敛，鼠标与键盘共用此门禁。
    pub(super) fn is_loading(&self) -> bool {
        self.cancellation.is_some()
    }

    /// 内部权威刷新可取代旧读；删除后的刷新不能被旧搜索的忙碌状态吞掉。
    pub(super) fn begin(&mut self) -> (u64, CancellationToken) {
        self.cancel();
        let cancellation = CancellationToken::new();
        self.cancellation = Some(cancellation.clone());
        (self.generation, cancellation)
    }

    /// 成功或失败都释放当前槽，过期任务既不能写结果也不能解锁下一次请求。
    pub(super) fn finish(&mut self, generation: u64) -> bool {
        if self.generation != generation || !self.is_loading() {
            return false;
        }
        self.cancellation = None;
        true
    }

    /// 切目标、整页重读和释放实体都作废旧票据，并通知原任务停止读取。
    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
    }
}

pub(super) const PAGE_SIZE: usize = 8;

/// 查询和清理都从同一不可变范围生成，避免列表展示范围与确认删除范围漂移。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct HistoryScope {
    pub(super) target: Option<CommandHistoryTarget>,
    pub(super) pane_id: Option<PaneId>,
    pub(super) remote_host_id: Option<HostId>,
    pub(super) session_id: Option<SessionId>,
}

impl HistoryScope {
    /// 命令工作区保留原有的可选会话查询；无会话时的工作区历史仍可显式列全局记录。
    ///
    /// @author kongweiguang
    pub(super) fn session(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            ..Self::default()
        }
    }

    /// Logs 以 pane、目标和可信 host 引用组成旧版兼容 scope，不绑定一次临时 session。
    ///
    /// @author kongweiguang
    pub(super) fn pane(
        target: CommandHistoryTarget,
        pane_id: PaneId,
        remote_host_id: Option<HostId>,
    ) -> Self {
        Self {
            target: Some(target),
            pane_id: Some(pane_id),
            remote_host_id,
            session_id: None,
        }
    }

    /// 列表和搜索只改变查询字段，不改变调用方冻结的目标范围。
    ///
    /// @author kongweiguang
    pub(super) fn list_request(
        &self,
        query: String,
        source: Option<CommandHistorySource>,
        limit: usize,
    ) -> CommandHistoryListRequest {
        CommandHistoryListRequest {
            query: (!query.trim().is_empty()).then_some(query),
            source,
            target: self.target,
            pane_id: self.pane_id.clone(),
            remote_host_id: self.remote_host_id.clone(),
            session_id: self.session_id.clone(),
            limit: Some(limit),
        }
    }

    /// 清理忽略搜索和来源筛选，但沿用点击时捕获的完整 pane/target/host 范围。
    ///
    /// @author kongweiguang
    pub(super) fn clear_request(&self) -> CommandHistoryClearRequest {
        CommandHistoryClearRequest {
            target: self.target,
            pane_id: self.pane_id.clone(),
            remote_host_id: self.remote_host_id.clone(),
            session_id: self.session_id.clone(),
        }
    }

    /// 删除前复核行仍属于当前投影范围，避免旧行按钮被重绘延迟误用于新目标。
    ///
    /// @author kongweiguang
    pub(super) fn contains_entry(&self, entry: &CommandHistoryEntry) -> bool {
        self.target.is_none_or(|target| entry.target == target)
            && self
                .pane_id
                .as_ref()
                .is_none_or(|pane_id| entry.pane_id.as_ref() == Some(pane_id))
            && self
                .remote_host_id
                .as_ref()
                .is_none_or(|host_id| entry.remote_host_id.as_ref() == Some(host_id))
            && self
                .session_id
                .as_ref()
                .is_none_or(|session_id| entry.session_id.as_ref() == Some(session_id))
    }
}

/// Logs 每页展示八项，原始查询最多读取一百项，页面索引始终保持在有效范围内。
///
/// @author kongweiguang
pub(super) fn page_window(page: usize, total: usize) -> (usize, usize, usize) {
    let page_count = total.div_ceil(PAGE_SIZE).max(1);
    let page = page.clamp(1, page_count);
    let start = (page - 1) * PAGE_SIZE;
    (page, start, (start + PAGE_SIZE).min(total))
}
