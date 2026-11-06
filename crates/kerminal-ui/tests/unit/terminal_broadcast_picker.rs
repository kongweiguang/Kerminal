//! 广播目标选择器的真实控件、状态和取消交互回归。
//!
//! @author kongweiguang

use super::*;
use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    Entity, KeyDownEvent, KeyUpEvent, Keystroke, Subscription, TestAppContext, VisualTestContext,
    base::Root,
};

struct BroadcastPickerHarness {
    picker: Entity<BroadcastTargetPicker>,
    events: Rc<RefCell<Vec<BroadcastEvent>>>,
    _subscription: Subscription,
    width: gpui_kit::Pixels,
}

impl Render for BroadcastPickerHarness {
    /// 保留生产选择器实体和键盘焦点，只替换真实终端服务的事件接收方。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(self.width).tab_group().child(self.picker.clone())
    }
}

/// 创建两项可用会话并记录操作意图，所有交互都经过真实 GPUI 控件。
///
/// @author kongweiguang
fn fixture(
    cx: &mut TestAppContext,
) -> (
    Entity<BroadcastPickerHarness>,
    SessionId,
    SessionId,
    &mut VisualTestContext,
) {
    cx.update(|app| {
        gpui_kit::init(app);
        app.set_reduce_motion(true);
    });
    let first = SessionId::new();
    let second = SessionId::new();
    let targets = vec![
        BroadcastTargetProjection {
            session_id: first.clone(),
            title: "First terminal".into(),
            available: true,
        },
        BroadcastTargetProjection {
            session_id: second.clone(),
            title: "Second terminal".into(),
            available: true,
        },
    ];
    let slot = Rc::new(RefCell::new(None));
    let output = slot.clone();
    let (_, visual) = cx.add_window_view(move |window, cx| {
        let picker = cx.new(|_| BroadcastTargetPicker::new(targets));
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded = events.clone();
        let subscription = cx.subscribe(&picker, move |_, _, event, _| {
            recorded.borrow_mut().push(event.clone());
        });
        let harness = cx.new(|_| BroadcastPickerHarness {
            picker,
            events,
            _subscription: subscription,
            width: px(400.),
        });
        *output.borrow_mut() = Some(harness.clone());
        Root::new(harness, window, cx)
    });
    let harness = slot.borrow_mut().take().expect("broadcast picker mounted");
    draw(visual);
    (harness, first, second, visual)
}

/// 刷新布局和事件队列，确保下次点击命中当前帧的控件。
///
/// @author kongweiguang
fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|window, app| window.draw(app).clear(app));
}

/// 按真实控件边界发起鼠标点击，避免通过直接调用模拟选择结果。
///
/// @author kongweiguang
fn click(visual: &mut VisualTestContext, selector: &str) {
    let bounds = visual
        .update(|window, _| window.debug_selector_bounds(selector))
        .unwrap_or_else(|| panic!("missing broadcast control {selector}"));
    visual.simulate_click(bounds.center(), Default::default());
    draw(visual);
}

/// 返回稳定的选择器实体，供断言真实交互之后的状态使用。
///
/// @author kongweiguang
fn picker(
    harness: &Entity<BroadcastPickerHarness>,
    visual: &mut VisualTestContext,
) -> Entity<BroadcastTargetPicker> {
    visual.update(|_, app| harness.read_with(app, |harness, _| harness.picker.clone()))
}

/// 键盘可勾选目标，提交只发一次；失败保留选择，取消才清除草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn checkbox_keyboard_submit_failure_and_cancel_preserve_contract(cx: &mut TestAppContext) {
    let (harness, first, _, visual) = fixture(cx);
    let picker = picker(&harness, visual);
    visual.update(|window, app| {
        window.focus_next(app);
        assert!(window.focused(app).is_some());
    });
    draw(visual);
    let keystroke = Keystroke::parse("space").expect("space is a supported key");
    visual.simulate_event(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(KeyUpEvent { keystroke });
    draw(visual);
    visual.update(|_, app| assert_eq!(picker.read(app).selected_targets(), vec![first.clone()]));

    click(visual, "terminal-broadcast-start");
    click(visual, "terminal-broadcast-start");
    visual.update(|_, app| {
        harness.read_with(app, |harness, _| {
            assert_eq!(
                harness.events.borrow().as_slice(),
                &[BroadcastEvent::Start {
                    session_ids: vec![first.clone()],
                }]
            );
        });
        picker.update(app, |picker, cx| picker.set_error("Cannot start", cx));
        assert_eq!(picker.read(app).selected_targets(), vec![first.clone()]);
    });
    draw(visual);
    click(visual, "terminal-broadcast-cancel");
    visual.update(|_, app| {
        assert!(picker.read(app).selected_targets().is_empty());
        harness.read_with(app, |harness, _| {
            assert_eq!(
                harness.events.borrow().last(),
                Some(&BroadcastEvent::Cancel)
            );
        });
    });
}

/// 已选择的失效目标仍可被取消，不能因为刷新目录而改选另一会话。
///
/// @author kongweiguang
#[gpui_kit::test]
fn removed_target_remains_deselectable_after_refresh(cx: &mut TestAppContext) {
    let (harness, first, _, visual) = fixture(cx);
    let picker = picker(&harness, visual);
    let selector = format!("terminal-broadcast-target-{}", first.as_str());
    click(visual, &selector);
    visual.update(|_, app| {
        assert_eq!(picker.read(app).selected_targets(), vec![first]);
        picker.update(app, |picker, cx| picker.set_targets(Vec::new(), cx));
    });
    draw(visual);
    click(visual, &selector);
    visual.update(|_, app| assert!(picker.read(app).selected_targets().is_empty()));
}

/// 窄 pane 中提交和取消仍位于面板内；两项动作换行时不能相互遮挡。
///
/// @author kongweiguang
#[gpui_kit::test]
fn narrow_picker_keeps_both_actions_inside_its_bounds(cx: &mut TestAppContext) {
    let (harness, _, _, visual) = fixture(cx);
    visual.update(|_, app| {
        harness.update(app, |harness, cx| {
            harness.width = px(160.);
            cx.notify();
        });
    });
    draw(visual);
    let panel = visual
        .debug_bounds("terminal-broadcast-picker")
        .expect("picker visible");
    let cancel = visual
        .debug_bounds("terminal-broadcast-cancel")
        .expect("cancel visible");
    let start = visual
        .debug_bounds("terminal-broadcast-start")
        .expect("start visible");
    for action in [cancel, start] {
        assert!(action.left() >= panel.left());
        assert!(action.right() <= panel.right());
        assert!(action.bottom() <= panel.bottom());
    }
    assert!(cancel.right() <= start.left() || cancel.bottom() <= start.top());
}
