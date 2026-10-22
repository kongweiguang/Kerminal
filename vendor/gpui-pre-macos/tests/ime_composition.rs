// @author kongweiguang

#[path = "../src/ime_composition.rs"]
mod ime_composition;

use ime_composition::{ImeInputHandlerState, InputHandlerOperation};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Handler(u8);

/// 验证 marked、提交与 unmark 在同一代次内始终使用原 handler。
#[test]
fn composition_keeps_its_handler_until_unmark_finishes() {
    let mut state = ImeInputHandlerState::new();
    state.set_current(Some(1), Handler(10));

    assert_eq!(
        state
            .select(InputHandlerOperation::SetMarkedText)
            .unwrap()
            .handler,
        Handler(10)
    );
    assert_eq!(
        state
            .select(InputHandlerOperation::InsertText)
            .unwrap()
            .handler,
        Handler(10)
    );
    let unmark = state.select(InputHandlerOperation::UnmarkText).unwrap();
    assert_eq!(unmark.handler, Handler(10));
    state.finish_unmark(unmark.finish_unmark.unwrap());

    assert_eq!(
        state
            .select(InputHandlerOperation::DirectEdit)
            .unwrap()
            .handler,
        Handler(10)
    );
}

/// 验证原生同步取消重入只命中旧目标，取消返回后恢复空闲路由。
#[test]
fn cancellation_reentry_cannot_commit_into_the_new_handler() {
    let mut state = ImeInputHandlerState::new();
    state.set_current(Some(1), Handler(10));
    state.select(InputHandlerOperation::SetMarkedText).unwrap();

    let cancellation = state.set_current(Some(2), Handler(20)).unwrap();
    assert_eq!(
        state.select(InputHandlerOperation::Query).unwrap().handler,
        Handler(10)
    );
    let reentrant_unmark = state.select(InputHandlerOperation::UnmarkText).unwrap();
    assert_eq!(reentrant_unmark.handler, Handler(10));
    assert!(reentrant_unmark.finish_unmark.is_none());
    assert!(state.select(InputHandlerOperation::InsertText).is_none());
    assert!(state.select(InputHandlerOperation::SetMarkedText).is_none());

    state.finish_cancellation(cancellation);
    assert_eq!(
        state
            .select(InputHandlerOperation::InsertText)
            .unwrap()
            .handler,
        Handler(20)
    );
}

/// 验证 FocusLost 释放组合 pin，窗口销毁同时清理当前 handler 与 pin。
#[test]
fn focus_loss_and_teardown_release_handler_owners() {
    let mut state = ImeInputHandlerState::new();
    state.set_current(Some(1), Handler(10));
    state.select(InputHandlerOperation::SetMarkedText).unwrap();

    let cancellation = state.begin_cancellation().unwrap();
    assert_eq!(
        state.select(InputHandlerOperation::Query).unwrap().handler,
        Handler(10)
    );
    state.finish_cancellation(cancellation);
    assert_eq!(
        state
            .select(InputHandlerOperation::DirectEdit)
            .unwrap()
            .handler,
        Handler(10)
    );

    state.clear();
    assert!(state.select(InputHandlerOperation::Query).is_none());
}

/// 无预编辑文本仍可正常粘贴、听写和辅助输入，且焦点变化后不遗留取消墓碑。
#[test]
fn no_preedit_insert_uses_the_current_handler() {
    let mut state = ImeInputHandlerState::new();
    state.set_current(Some(1), Handler(10));

    assert_eq!(
        state
            .select(InputHandlerOperation::InsertText)
            .unwrap()
            .handler,
        Handler(10)
    );
    assert_eq!(
        state
            .select(InputHandlerOperation::UnmarkText)
            .unwrap()
            .handler,
        Handler(10)
    );

    assert_eq!(state.set_current(Some(2), Handler(20)), None);
    assert_eq!(
        state
            .select(InputHandlerOperation::InsertText)
            .unwrap()
            .handler,
        Handler(20)
    );
}

/// 同焦点重绘只刷新当前槽；组合保持旧 clone，待原生 unmark 后再切换。
#[test]
fn same_focus_redraw_does_not_cancel_composition() {
    let mut state = ImeInputHandlerState::new();
    state.set_current(Some(1), Handler(10));
    state.select(InputHandlerOperation::SetMarkedText).unwrap();

    assert_eq!(state.take_current(), Some(Handler(10)));
    assert_eq!(
        state.select(InputHandlerOperation::Query).unwrap().handler,
        Handler(10)
    );
    assert_eq!(state.set_current(Some(1), Handler(11)), None);
    assert_eq!(
        state.select(InputHandlerOperation::Query).unwrap().handler,
        Handler(10)
    );
    let unmark = state.select(InputHandlerOperation::UnmarkText).unwrap();
    assert_eq!(unmark.handler, Handler(10));
    state.finish_unmark(unmark.finish_unmark.unwrap());

    assert_eq!(
        state
            .select(InputHandlerOperation::DirectEdit)
            .unwrap()
            .handler,
        Handler(11)
    );
}
