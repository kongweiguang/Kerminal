//! 候选分屏标题栏固定与取消入口的 UI 回归。
//!
//! 背景：分屏测量或会话创建迟迟不收敛时，旧实现用整块透明盾和隐藏标题按钮来“锁”交互，
//! 结果取消按钮与关闭入口一起被吞掉，用户只能等待。这里用真实 Kit 控件验证：候选期按钮仍在原位，
//! 且候选 pane 的关闭会沿普通 Close 意图返回 Root。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    AnyElement, Bounds, Context, ElementId, Entity, Pixels, Subscription, TestAppContext,
    VisualTestContext, Window, div, prelude::*, px, size,
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use super::{
    PaneBroadcastProjection, PaneHeaderProjection, SplitCandidatePreview,
    SplitCandidateProbeContext, WorkspacePaneEvent, WorkspacePanes,
};

/// 测试用终端画布不创建会话，只占住叶子内容区。
fn render_terminal(
    _: &PaneId,
    _: Option<SplitCandidateProbeContext>,
    _: &mut Window,
    _: &mut Context<'_, WorkspacePanes>,
) -> AnyElement {
    div().child("terminal").into_any_element()
}

/// 标题投影保持分屏可用，贴近真实 Ready pane 的工具栏形态。
fn pane_header(pane: &PaneId) -> PaneHeaderProjection {
    PaneHeaderProjection {
        title: pane.as_str().into(),
        split_enabled: true,
        session_action: None,
        host_picker: None,
        broadcast: PaneBroadcastProjection::default(),
        session_disconnected: false,
        candidate_failed: false,
    }
}

fn split_layout(source: &PaneId, candidate: &PaneId) -> TerminalLayoutNode {
    TerminalLayoutNode::Split {
        id: "split-candidate-test".to_owned(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::Pane {
                pane_id: source.clone(),
            },
            TerminalLayoutNode::Pane {
                pane_id: candidate.clone(),
            },
        ],
        sizes: None,
    }
}

/// 从真实 Kit Button 的访问性快照取命中区域；缺失或不可见直接失败。
fn button_bounds(visual: &mut VisualTestContext, id: &str) -> Bounds<Pixels> {
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    let id: ElementId = id.to_owned().into();
    visual.update(|window, _| {
        let snapshot = gpui_kit::base::test_support::find(window, &[], &id).unwrap_or_else(|| {
            panic!(
                "button {id:?} missing: {}",
                gpui_kit::base::test_support::registered_paths(window)
            )
        });
        assert!(snapshot.visible(), "button {id:?} must be visible");
        snapshot.bounds()
    })
}

/// 候选期内标题栏按钮不得消失或移位到无法到达的位置；关闭候选 pane 必须发出 Close。
#[gpui_kit::test]
fn pending_split_keeps_header_controls_and_closes_candidate(app: &mut TestAppContext) {
    let source = PaneId::new();
    let candidate = PaneId::new();
    app.update(crate::init);

    let mounted: Rc<RefCell<Option<Entity<WorkspacePanes>>>> = Rc::new(RefCell::new(None));
    let events: Rc<RefCell<Vec<WorkspacePaneEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let subscriptions: Rc<RefCell<Option<Subscription>>> = Rc::new(RefCell::new(None));

    let mounted_for_view = Rc::clone(&mounted);
    let events_for_view = Rc::clone(&events);
    let subscriptions_for_view = Rc::clone(&subscriptions);
    let source_for_view = source.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panes = cx.new(|_| {
            WorkspacePanes::new(
                TerminalLayoutNode::Pane {
                    pane_id: source_for_view.clone(),
                },
                source_for_view,
                render_terminal,
                pane_header,
            )
        });
        *mounted_for_view.borrow_mut() = Some(panes.clone());
        *subscriptions_for_view.borrow_mut() = Some(cx.subscribe(
            &panes,
            move |_, _: Entity<WorkspacePanes>, event: &WorkspacePaneEvent, _| {
                events_for_view.borrow_mut().push(event.clone());
            },
        ));
        gpui_kit::base::Root::new(panes, window, cx)
    });
    visual.simulate_resize(size(px(960.), px(600.)));
    visual.run_until_parked();

    let panes = mounted
        .borrow()
        .clone()
        .expect("WorkspacePanes mounted into the test window");
    let preview = SplitCandidatePreview {
        operation_id: "candidate-header-operation".to_owned(),
        tab_id: TabId::new(),
        source_pane_id: source.clone(),
        pane_id: candidate.clone(),
    };
    let layout = split_layout(&source, &candidate);
    let preview_for_view = preview.clone();
    let source_for_view = source.clone();
    visual.update(move |_, app| {
        panes.update(app, |panes, cx| {
            // 锁定参数保持与 Root 在测量期相同，确认锁不再影响标题栏可达性。
            panes.set_projection(
                layout,
                source_for_view,
                Some(preview_for_view),
                None,
                true,
                cx,
            );
        });
    });
    visual.run_until_parked();

    let close = button_bounds(visual, &format!("pane-close-{}", candidate.as_str()));
    button_bounds(
        visual,
        &format!("pane-split-horizontal-after-{}-trigger", source.as_str()),
    );
    button_bounds(
        visual,
        &format!("pane-split-vertical-after-{}-trigger", candidate.as_str()),
    );
    assert!(
        visual.debug_bounds("rr-workspace-pane-prepare").is_some(),
        "candidate status badge must occupy its own slot instead of replacing controls"
    );

    visual.simulate_click(close.center(), Default::default());
    visual.run_until_parked();
    assert!(
        events.borrow().iter().any(|event| matches!(
            event,
            WorkspacePaneEvent::Close(pane_id) if *pane_id == candidate
        )),
        "candidate close must reach Root as the normal close intent"
    );
}
