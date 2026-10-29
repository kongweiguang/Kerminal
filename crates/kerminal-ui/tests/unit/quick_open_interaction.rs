//! Quick Open 与 GPUI Kit Command/InputState 调度边界的真实交互回归。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, IntoElement, MouseButton, MouseMoveEvent,
    Render, ScrollDelta, ScrollWheelEvent, Subscription, TestAppContext, VisualTestContext, Window,
    WindowControlArea, div, point, px,
};
use gpui_kit::{
    component::command::{Command, CommandItem, CommandState},
    prelude::*,
};

use crate::{
    features::quick_open::{QuickOpenEvent, QuickOpenSelection},
    keybindings::KeybindingAction,
};

struct QuickOpenOwnerHarness {
    command_state: Entity<CommandState>,
    is_open: bool,
    query: String,
    loading: bool,
    query_callbacks: Vec<String>,
}

impl EventEmitter<QuickOpenEvent> for QuickOpenOwnerHarness {}

impl QuickOpenOwnerHarness {
    /// 复用同一个 owner 与 CommandState，验证面板重开时输入焦点仍指向真实查询控件。
    ///
    /// @author kongweiguang
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.is_open = true;
        self.query.clear();
        self.loading = false;
        self.command_state
            .update(cx, |state, cx| state.set_query("", window, cx));
        self.command_state
            .update(cx, |state, cx| state.set_loading(false, window, cx));
        self.command_state
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// 先完成关闭和输入清理再发事件，使 Root 订阅者只观察稳定状态。
    ///
    /// @author kongweiguang
    fn close(
        &mut self,
        selection: Option<QuickOpenSelection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_open {
            return;
        }
        self.is_open = false;
        self.query.clear();
        self.loading = false;
        self.command_state
            .update(cx, |state, cx| state.set_query("", window, cx));
        self.command_state
            .update(cx, |state, cx| state.set_loading(false, window, cx));
        match selection {
            Some(selection) => cx.emit(QuickOpenEvent::Selected {
                selection,
                origin: None,
                terminal_binding: None,
            }),
            None => cx.emit(QuickOpenEvent::Dismissed {
                origin_binding: None,
            }),
        }
        cx.notify();
    }
}

struct QuickOpenInteractionHarness {
    owner: Entity<QuickOpenOwnerHarness>,
    _event_subscription: Subscription,
    delivered_events: Vec<QuickOpenEvent>,
    owner_open_at_delivery: Vec<bool>,
    canvas_mouse_downs: usize,
    canvas_mouse_moves: usize,
    canvas_scrolls: usize,
    nc_control_hits: usize,
}

impl QuickOpenInteractionHarness {
    /// 让独立 Root-like 实体订阅 owner，验证 GPUI 事件排队后才跨实体投递。
    ///
    /// @author kongweiguang
    fn new(owner: Entity<QuickOpenOwnerHarness>, window: &Window, cx: &mut Context<Self>) -> Self {
        let event_subscription = cx.subscribe_in(&owner, window, |root, owner, event, _, cx| {
            root.delivered_events.push(event.clone());
            root.owner_open_at_delivery.push(owner.read(cx).is_open);
        });
        Self {
            owner,
            _event_subscription: event_subscription,
            delivered_events: Vec::new(),
            owner_open_at_delivery: Vec::new(),
            canvas_mouse_downs: 0,
            canvas_mouse_moves: 0,
            canvas_scrolls: 0,
            nc_control_hits: 0,
        }
    }
}

impl Render for QuickOpenInteractionHarness {
    /// 使用真实 Kit Command/InputState 与当前主题，复刻 owner 更新边界而不固定遮罩配色。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let canvas_mouse_down = cx.listener(|root, _, _, _| {
            root.canvas_mouse_downs += 1;
        });
        let canvas_mouse_move = cx.listener(|root, _: &MouseMoveEvent, _, _| {
            root.canvas_mouse_moves += 1;
        });
        let canvas_scroll = cx.listener(|root, _: &ScrollWheelEvent, _, _| {
            root.canvas_scrolls += 1;
        });
        let nc_control_mouse_down = cx.listener(|root, _, _, _| {
            root.nc_control_hits += 1;
        });
        let mut surface = div()
            .id("quick-open-interaction-root")
            .debug_selector(|| "quick-open-interaction-root".to_owned())
            .size_full()
            .relative()
            .child(
                div()
                    .id("quick-open-canvas-probe")
                    .size_full()
                    .on_mouse_down(MouseButton::Left, canvas_mouse_down)
                    .on_mouse_move(canvas_mouse_move)
                    .on_scroll_wheel(canvas_scroll)
                    .child(
                        div()
                            .id("quick-open-nc-close-probe")
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(34.))
                            .h(px(36.))
                            .window_control_area(WindowControlArea::Close)
                            .on_mouse_down(MouseButton::Left, nc_control_mouse_down),
                    ),
            );
        let (is_open, command_state) = self
            .owner
            .read_with(cx, |owner, _| (owner.is_open, owner.command_state.clone()));
        if !is_open {
            return surface;
        }

        let query_owner = self.owner.clone();
        let confirm_owner = self.owner.clone();
        let cancel_owner = self.owner.clone();
        let command = Command::new(&command_state)
            .item(
                CommandItem::new()
                    .label("MCP runtime")
                    .keywords(["MCP", "runtime"]),
            )
            .item(CommandItem::new().label("Terminal search"))
            .filterable(false)
            .on_query(move |query, window, app| {
                let query = query.to_string();
                query_owner.update(app, |owner, cx| {
                    if !owner.is_open {
                        return;
                    }
                    owner.query = query.clone();
                    owner.query_callbacks.push(query);
                    owner.loading = true;
                    owner.command_state.update(cx, |state, cx| {
                        state.set_loading(true, window, cx);
                    });
                });
            })
            .on_confirm(move |index, window, app| {
                confirm_owner.update(app, |owner, cx| {
                    let selection = (index.row == 0)
                        .then_some(QuickOpenSelection::Action(KeybindingAction::TerminalSearch));
                    owner.close(selection, window, cx);
                });
            })
            .on_cancel(move |window, app| {
                let cancel_owner = cancel_owner.clone();
                window.defer(app, move |window, app| {
                    cancel_owner.update(app, |owner, cx| owner.close(None, window, cx));
                });
            });
        surface = surface.child(
            super::view::quick_open_overlay_surface(crate::theme::ThemeTokens::current(window, cx))
                .child(
                    div()
                        .id("quick-open-modal")
                        .debug_selector(|| "quick-open-command-frame".to_owned())
                        .mt(px(54.))
                        .w(px(720.))
                        .max_w_full()
                        .max_h(px(560.))
                        .child(command),
                ),
        );
        surface
    }
}

/// 创建 headless GPUI 窗口，并在 Command 首帧渲染前调用真实 CommandState 焦点路径。
///
/// @author kongweiguang
fn fixture(
    app: &mut TestAppContext,
) -> (
    Entity<QuickOpenInteractionHarness>,
    Entity<QuickOpenOwnerHarness>,
    &mut VisualTestContext,
) {
    app.update(|app| {
        gpui_kit::init(app);
        app.set_reduce_motion(true);
    });
    let root_result = Rc::new(RefCell::new(None));
    let owner_result = Rc::new(RefCell::new(None));
    let root_out = root_result.clone();
    let owner_out = owner_result.clone();
    let (_, visual) = app.add_window_view(move |window, app| {
        let command_state = app.new(|cx| CommandState::new(window, cx));
        let owner = app.new(|_| QuickOpenOwnerHarness {
            command_state: command_state.clone(),
            is_open: true,
            query: String::new(),
            loading: false,
            query_callbacks: Vec::new(),
        });

        // CommandState defaults to searchable=true, so opening before its first render must focus InputState.
        command_state.update(app, |state, cx| state.focus(window, cx));

        let root_owner = owner.clone();
        let root = app.new(|cx| QuickOpenInteractionHarness::new(root_owner, window, cx));
        *root_out.borrow_mut() = Some(root.clone());
        *owner_out.borrow_mut() = Some(owner);
        gpui_kit::base::Root::new(root, window, app)
    });
    let root = root_result
        .borrow_mut()
        .take()
        .expect("Root-like interaction host is mounted");
    let owner = owner_result
        .borrow_mut()
        .take()
        .expect("Quick Open owner is mounted");
    (root, owner, visual)
}

/// 清空当前交互产生的 GPUI effect 队列，再绘制以读取稳定状态。
///
/// @author kongweiguang
fn settle(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.run_until_parked();
}

/// 刷新 hitbox 后再继续派发输入，复用测试平台按真实帧边界处理指针事件的顺序。
///
/// @author kongweiguang
fn render_visual_frame(visual: &mut VisualTestContext) {
    visual.update(|window, app| {
        window.refresh();
        window.draw(app).clear(app);
    });
}

/// 鼠标事件按移动、按下、抬起逐帧分发，避免不同事件共用陈旧 hitbox。
///
/// @author kongweiguang
fn simulate_click_at(visual: &mut VisualTestContext, position: gpui_kit::Point<gpui_kit::Pixels>) {
    visual.simulate_mouse_move(position, None, Default::default());
    render_visual_frame(visual);
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    render_visual_frame(visual);
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    render_visual_frame(visual);
}

/// 点击真实查询文本区；左侧搜索图标属于装饰区域，不能用它代替输入控件的聚焦验收。
///
/// @author kongweiguang
fn click_query_area(visual: &mut VisualTestContext) {
    let command_bounds = visual
        .debug_bounds("quick-open-command-frame")
        .expect("Quick Open command frame was drawn");
    let position = command_bounds.origin + point(px(180.), px(18.));
    simulate_click_at(visual, position);
}

/// 在遮罩空白角落实际分发指针和滚轮，覆盖底层 Canvas 常用的交互通道。
///
/// @author kongweiguang
fn scroll_empty_overlay(visual: &mut VisualTestContext) {
    let root_bounds = visual
        .debug_bounds("quick-open-interaction-root")
        .expect("Quick Open root was drawn");
    let position = root_bounds.origin + point(px(8.), px(8.));
    visual.simulate_mouse_move(position, None, Default::default());
    render_visual_frame(visual);
    visual.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0., 1.)),
        ..Default::default()
    });
    render_visual_frame(visual);
}

/// 在窗口控制区的底层 hitbox 发送点击，确保模态遮罩也覆盖 Root 标题栏区域。
///
/// @author kongweiguang
fn click_nc_close_area(visual: &mut VisualTestContext) {
    let bounds = visual
        .debug_bounds("quick-open-interaction-root")
        .expect("Quick Open root was drawn");
    let position = point(
        bounds.origin.x + bounds.size.width - px(8.),
        bounds.origin.y + px(8.),
    );
    simulate_click_at(visual, position);
}

/// 经真实文本命中验证查询、遮挡及延后确认；关闭后复点底层，证明零命中断言使用有效探针。
///
/// @author kongweiguang
#[gpui_kit::test]
fn focused_input_query_and_enter_confirmation_follow_kit_effect_order(app: &mut TestAppContext) {
    let (root, owner, visual) = fixture(app);
    settle(visual);

    click_query_area(visual);
    visual.simulate_input("MCP");
    settle(visual);

    let (query_after_query_click, input_after_query_click) = visual.update(|_, app| {
        owner.read_with(app, |owner, app| {
            (
                owner.query.clone(),
                owner.command_state.read(app).query(app).to_string(),
            )
        })
    });
    assert_eq!(input_after_query_click, "MCP");
    assert_eq!(query_after_query_click, "MCP");

    let root_bounds = visual
        .debug_bounds("quick-open-interaction-root")
        .expect("Quick Open root was drawn");
    let blank_position = root_bounds.origin + point(px(8.), px(8.));
    simulate_click_at(visual, blank_position);
    scroll_empty_overlay(visual);
    click_nc_close_area(visual);
    let (canvas_mouse_downs, canvas_mouse_moves, canvas_scrolls, nc_control_hits, is_open) = visual
        .update(|_, app| {
            (
                root.read_with(app, |root, _| root.canvas_mouse_downs),
                root.read_with(app, |root, _| root.canvas_mouse_moves),
                root.read_with(app, |root, _| root.canvas_scrolls),
                root.read_with(app, |root, _| root.nc_control_hits),
                owner.read_with(app, |owner, _| owner.is_open),
            )
        });
    assert_eq!(canvas_mouse_downs, 0);
    assert_eq!(canvas_mouse_moves, 0);
    assert_eq!(canvas_scrolls, 0);
    assert_eq!(nc_control_hits, 0);
    assert!(is_open);

    visual.simulate_input("!");
    settle(visual);

    let (query, loading, query_callbacks, input_value) = visual.update(|_, app| {
        owner.read_with(app, |owner, app| {
            (
                owner.query.clone(),
                owner.loading,
                owner.query_callbacks.clone(),
                owner.command_state.read(app).query(app).to_string(),
            )
        })
    });
    assert_eq!(input_value, "MCP!");
    assert_eq!(query, "MCP!");
    assert!(loading);
    assert_eq!(query_callbacks.last().map(String::as_str), Some("MCP!"));

    visual.simulate_keystrokes("enter");
    settle(visual);

    let (is_open, input_value) = visual.update(|_, app| {
        owner.read_with(app, |owner, app| {
            (
                owner.is_open,
                owner.command_state.read(app).query(app).to_string(),
            )
        })
    });
    assert!(!is_open);
    assert!(input_value.is_empty());

    let (events, open_at_delivery) = visual.update(|_, app| {
        root.read_with(app, |root, _| {
            (
                root.delivered_events.clone(),
                root.owner_open_at_delivery.clone(),
            )
        })
    });
    assert!(matches!(
        events.as_slice(),
        [QuickOpenEvent::Selected { .. }]
    ));
    assert_eq!(open_at_delivery, [false]);

    simulate_click_at(visual, blank_position);
    assert_eq!(
        visual.update(|_, app| root.read(app).canvas_mouse_downs),
        1,
        "卸载遮罩后底层 Canvas 应重新接收同一位置的鼠标事件"
    );
}

/// 验证 Escape 先清查询再延后关闭，并在同一 owner 重开后继续向真实输入框键入。
///
/// @author kongweiguang
#[gpui_kit::test]
fn escape_clears_then_defers_close_and_reopen_has_no_old_event(app: &mut TestAppContext) {
    let (root, owner, visual) = fixture(app);
    settle(visual);

    visual.simulate_input("MCP");
    visual.simulate_keystrokes("escape");
    settle(visual);

    let (is_open, query) = visual.update(|_, app| {
        owner.read_with(app, |owner, app| {
            (
                owner.is_open,
                owner.command_state.read(app).query(app).to_string(),
            )
        })
    });
    assert!(is_open);
    assert!(query.is_empty());

    visual.simulate_keystrokes("escape");
    settle(visual);

    let (is_open, events, open_at_delivery) = visual.update(|_, app| {
        (
            owner.read_with(app, |owner, _| owner.is_open),
            root.read_with(app, |root, _| root.delivered_events.clone()),
            root.read_with(app, |root, _| root.owner_open_at_delivery.clone()),
        )
    });
    assert!(!is_open);
    assert!(matches!(
        events.as_slice(),
        [QuickOpenEvent::Dismissed { .. }]
    ));
    assert_eq!(open_at_delivery, [false]);

    visual.update(|window, app| {
        owner.update(app, |owner, cx| owner.open(window, cx));
    });
    settle(visual);
    visual.simulate_input("MCP");
    settle(visual);

    let (state, event_count) = visual.update(|_, app| {
        (
            owner.read_with(app, |owner, app| {
                (
                    owner.is_open,
                    owner.command_state.read(app).query(app).to_string(),
                )
            }),
            root.read_with(app, |root, _| root.delivered_events.len()),
        )
    });
    assert!(state.0);
    assert_eq!(state.1, "MCP");
    assert_eq!(event_count, 1);
}
