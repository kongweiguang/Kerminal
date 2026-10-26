//! 历史搜索等待、刷新取代与目标切换的生命周期回归。
//!
//! @author kongweiguang

use super::history_query::HistoryReadState;

/// 真实任务尚未回执时保持等待；成功和失败共用完成路径，随后都允许重试。
#[test]
fn history_read_completion_releases_busy_and_allows_retry() {
    let mut state = HistoryReadState::default();
    assert!(!state.is_loading());
    let (first, _) = state.begin();
    assert!(state.is_loading());
    assert!(state.finish(first));
    assert!(!state.is_loading());
    assert!(!state.finish(first));
    let (retry, _) = state.begin();
    assert_ne!(first, retry);
    assert!(state.finish(retry));
}

/// 删除后的刷新取代旧搜索，旧回执不能解除新请求的等待或写入旧列表。
#[test]
fn history_read_stale_completion_does_not_unlock_replacement() {
    let mut state = HistoryReadState::default();
    let (old, old_cancellation) = state.begin();
    let (new, new_cancellation) = state.begin();
    assert!(old_cancellation.is_cancelled());
    assert!(!state.finish(old));
    assert!(state.is_loading());
    assert!(!new_cancellation.is_cancelled());
    assert!(state.finish(new));
}

/// 切目标和销毁通知旧任务取消，旧目标回执不能恢复已清空的忙态。
#[test]
fn history_read_target_change_invalidates_and_cancels_old_request() {
    let mut state = HistoryReadState::default();
    let (old, cancellation) = state.begin();
    state.cancel();
    assert!(cancellation.is_cancelled());
    assert!(!state.is_loading());
    assert!(!state.finish(old));
    let (new, _) = state.begin();
    assert!(!state.finish(old));
    assert!(state.finish(new));
}

/// 整页加载捕获的历史版本必须因后续搜索失效，避免旧面板把新结果覆盖回去。
#[test]
fn history_read_new_search_invalidates_full_panel_history_snapshot() {
    let mut state = HistoryReadState::default();
    state.cancel();
    let panel_generation = state.generation();
    let (search, _) = state.begin();
    assert_ne!(panel_generation, state.generation());
    assert!(state.finish(search));
    assert_ne!(panel_generation, state.generation());
}
