// @author kongweiguang

use gpui::{
    AnyElement, AnyView, AnyWindowHandle, App, AppContext as _, Context, Entity, InputEvent as _,
    InteractiveElement, MouseButton, MouseDownEvent, MouseUpEvent, Render,
    StatefulInteractiveElement, TestAppContext, Window, WindowHandle, div, point, prelude::*, px,
};
use std::{cell::Cell, rc::Rc, time::Duration};

const TEST_TOOLTIP_DELAY: Duration = Duration::from_millis(100);

struct TooltipMouseDownView {
    tooltip_builds: Rc<Cell<usize>>,
    tooltip_content_renders: Rc<Cell<usize>>,
    tooltip_clicks: Rc<Cell<usize>>,
    target_visible: Rc<Cell<bool>>,
    operation_started: Rc<Cell<bool>>,
    target_left: Rc<Cell<f32>>,
    occluded: Rc<Cell<bool>>,
    hoverable: bool,
}

impl Render for TooltipMouseDownView {
    /// 该集成夹具保留来源显隐与遮挡开关，以分开验证卸载清理和 bounds-only 遮挡行为。
    ///
    /// @author kongweiguang
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let operation_started = self.operation_started.clone();
        let target = div()
            .id("tooltip-source")
            .absolute()
            .left(px(self.target_left.get()))
            .w(px(60.))
            .h(px(40.))
            .tooltip_show_delay(TEST_TOOLTIP_DELAY);
        let build_tooltip = tooltip_builder(
            self.tooltip_builds.clone(),
            self.tooltip_content_renders.clone(),
            self.tooltip_clicks.clone(),
        );
        let target = if self.hoverable {
            target.hoverable_tooltip(build_tooltip)
        } else {
            target.tooltip(build_tooltip)
        };
        let target = target.on_click(move |_, _, _| operation_started.set(true));

        let content = if self.target_visible.get() {
            target
        } else {
            div().id("replacement").size_full()
        };
        let mut root = div().relative().size_full().child(content);
        if self.occluded.get() {
            root = root.child(div().id("modal-occlusion").absolute().inset_0().occlude());
        }
        root
    }
}

struct PersistentTooltipOverlayRoot {
    /// Root 持有面板实体；切换时只卸载视图节点，模拟弹窗关闭但实体仍存活。
    source: Entity<TooltipMouseDownView>,
    source_mounted: Rc<Cell<bool>>,
    background_revision: Rc<Cell<usize>>,
    manual_source_render: bool,
}

impl Render for PersistentTooltipOverlayRoot {
    /// 保持背景与弹层分开，使测试能在不移动鼠标时触发 Root 重绘和实体视图重挂。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = div()
            .id("persistent-root-background")
            .size_full()
            .child(format!("背景版本 {}", self.background_revision.get()));
        let root = div()
            .id("persistent-tooltip-root")
            .relative()
            .size_full()
            .child(background);
        if self.source_mounted.get() {
            let source: AnyElement = if self.manual_source_render {
                self.source.update(cx, |view, view_cx| {
                    view.render(window, view_cx).into_any_element()
                })
            } else {
                self.source.clone().into_any_element()
            };
            root.child(
                div()
                    .id("persistent-tooltip-overlay-host")
                    .absolute()
                    .inset_0()
                    .child(source),
            )
        } else {
            root
        }
    }
}

struct PersistentTooltipOwner {
    root: WindowHandle<PersistentTooltipOverlayRoot>,
    window: AnyWindowHandle,
    tooltip_builds: Rc<Cell<usize>>,
    tooltip_clicks: Rc<Cell<usize>>,
    operation_started: Rc<Cell<bool>>,
}

struct TooltipMouseDownContent {
    renders: Rc<Cell<usize>>,
    clicks: Rc<Cell<usize>>,
}

impl Render for TooltipMouseDownContent {
    /// Tooltip 使用稳定元素 ID 启用状态化点击监听，并记下真实渲染以避免依赖调试 bounds API。
    ///
    /// @author kongweiguang
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        let clicks = self.clicks.clone();
        div()
            .id("tooltip-content")
            .w(px(30.))
            .h(px(20.))
            .child("关闭设置")
            .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
    }
}

/// 显式声明回调的窗口生命周期，确保测试覆盖 GPUI 公开 tooltip API 的通用闭包签名。
///
/// @author kongweiguang
fn tooltip_builder(
    tooltip_builds: Rc<Cell<usize>>,
    tooltip_content_renders: Rc<Cell<usize>>,
    tooltip_clicks: Rc<Cell<usize>>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window: &mut Window, cx: &mut App| -> AnyView {
        tooltip_builds.set(tooltip_builds.get() + 1);
        let renders = tooltip_content_renders.clone();
        let clicks = tooltip_clicks.clone();
        cx.new(
            move |_: &mut Context<TooltipMouseDownContent>| TooltipMouseDownContent {
                renders,
                clicks,
            },
        )
        .into()
    }
}

/// 创建注册真实 tooltip 事件处理器的窗口，并返回用于卸载目标的句柄。
///
/// @author kongweiguang
fn create_tooltip_owner(
    cx: &mut TestAppContext,
    hoverable: bool,
) -> (
    WindowHandle<TooltipMouseDownView>,
    AnyWindowHandle,
    Rc<Cell<usize>>,
    Rc<Cell<usize>>,
    Rc<Cell<bool>>,
    Rc<Cell<f32>>,
    Rc<Cell<usize>>,
    Rc<Cell<bool>>,
) {
    let tooltip_builds = Rc::new(Cell::new(0));
    let tooltip_content_renders = Rc::new(Cell::new(0));
    let tooltip_clicks = Rc::new(Cell::new(0));
    let target_visible = Rc::new(Cell::new(true));
    let operation_started = Rc::new(Cell::new(false));
    let target_left = Rc::new(Cell::new(0.));
    let occluded = Rc::new(Cell::new(false));
    let window = cx.add_window({
        let tooltip_builds = tooltip_builds.clone();
        let tooltip_content_renders = tooltip_content_renders.clone();
        let tooltip_clicks = tooltip_clicks.clone();
        let target_visible = target_visible.clone();
        let operation_started = operation_started.clone();
        let target_left = target_left.clone();
        let occluded = occluded.clone();
        move |_, _| TooltipMouseDownView {
            tooltip_builds,
            tooltip_content_renders,
            tooltip_clicks,
            target_visible,
            operation_started,
            target_left,
            occluded,
            hoverable,
        }
    });
    let any_window = AnyWindowHandle::from(window);
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    (
        window,
        any_window,
        tooltip_builds,
        tooltip_content_renders,
        operation_started,
        target_left,
        tooltip_clicks,
        occluded,
    )
}

/// 创建 Root 长期持有来源 Entity、仅按状态挂载其视图的窗口夹具。
///
/// 该结构覆盖设置面板关闭时 Entity 仍由 Root 保留、tooltip 的元素视图从树中移除的真实生命周期。
/// @author kongweiguang
fn create_persistent_tooltip_owner(
    cx: &mut TestAppContext,
    manual_source_render: bool,
) -> PersistentTooltipOwner {
    let tooltip_builds = Rc::new(Cell::new(0));
    let tooltip_content_renders = Rc::new(Cell::new(0));
    let tooltip_clicks = Rc::new(Cell::new(0));
    let target_visible = Rc::new(Cell::new(true));
    let operation_started = Rc::new(Cell::new(false));
    let target_left = Rc::new(Cell::new(0.));
    let occluded = Rc::new(Cell::new(false));
    let source_mounted = Rc::new(Cell::new(true));
    let background_revision = Rc::new(Cell::new(0));
    let root = cx.add_window({
        let tooltip_builds = tooltip_builds.clone();
        let tooltip_content_renders = tooltip_content_renders.clone();
        let tooltip_clicks = tooltip_clicks.clone();
        let target_visible = target_visible.clone();
        let operation_started = operation_started.clone();
        let target_left = target_left.clone();
        let occluded = occluded.clone();
        let source_mounted = source_mounted.clone();
        let background_revision = background_revision.clone();
        move |_, cx| {
            let source = cx.new(|_| TooltipMouseDownView {
                tooltip_builds,
                tooltip_content_renders,
                tooltip_clicks,
                target_visible,
                operation_started,
                target_left,
                occluded,
                hoverable: false,
            });
            PersistentTooltipOverlayRoot {
                source,
                source_mounted,
                background_revision,
                manual_source_render,
            }
        }
    });
    let window = AnyWindowHandle::from(root);
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    PersistentTooltipOwner {
        root,
        window,
        tooltip_builds,
        tooltip_clicks,
        operation_started,
    }
}

/// 将鼠标移入来源元素并完成绘制，使正常的 tooltip 延时开始计时。
///
/// @author kongweiguang
fn hover_tooltip_source(cx: &mut TestAppContext, window: AnyWindowHandle) {
    hover_tooltip_source_at(cx, window, point(px(10.), px(10.)));
}

/// 使用显式坐标触发 hover，便于把 tooltip 命中区域与来源控件分离验证。
///
/// @author kongweiguang
fn hover_tooltip_source_at(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    position: gpui::Point<gpui::Pixels>,
) {
    cx.update_window(window, |_, window, cx| {
        window.simulate_mouse_move(position, cx);
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

/// 通过公开平台输入分发按下和抬起事件，覆盖 tooltip 与 click handler 的真实共用路径。
///
/// @author kongweiguang
fn click_tooltip_source(cx: &mut TestAppContext, window: AnyWindowHandle) {
    click_at(cx, window, point(px(10.), px(10.)));
}

/// 在指定位置经过 GPUI 的真实鼠标分发，确认当前帧中的提示视图是否可命中。
///
/// @author kongweiguang
fn click_at(cx: &mut TestAppContext, window: AnyWindowHandle, position: gpui::Point<gpui::Pixels>) {
    cx.update_window(window, |_, window, cx| {
        for event in [
            MouseDownEvent {
                position,
                button: MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }
            .to_platform_input(),
            MouseUpEvent {
                position,
                button: MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
            }
            .to_platform_input(),
        ] {
            window.dispatch_event(event, cx);
        }
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

/// 点击会卸载其来源元素；延时到期后不得再显示旧按钮的 tooltip。
///
/// @author kongweiguang
#[gpui::test]
fn mouse_down_cancels_pending_tooltip_before_owner_unmount(cx: &mut TestAppContext) {
    let (owner, any_window, tooltip_builds, _, operation_started, _, _, _) =
        create_tooltip_owner(cx, false);

    hover_tooltip_source(cx, any_window);
    click_tooltip_source(cx, any_window);
    assert!(operation_started.get());

    owner
        .update(cx, |view, _, cx| {
            view.target_visible.set(false);
            cx.notify();
        })
        .unwrap();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    cx.dispatcher
        .advance_clock(TEST_TOOLTIP_DELAY + Duration::from_millis(1));
    cx.run_until_parked();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    assert_eq!(tooltip_builds.get(), 0);
}

/// 同一 ID 控件重排后不再使用 hover 开始时捕获的旧 bounds。
///
/// @author kongweiguang
#[gpui::test]
fn delayed_tooltip_rechecks_bounds_after_same_element_moves(cx: &mut TestAppContext) {
    let (owner, any_window, tooltip_builds, _, _, target_left, _, _) =
        create_tooltip_owner(cx, false);

    hover_tooltip_source(cx, any_window);
    owner
        .update(cx, |_, _, cx| {
            target_left.set(120.);
            cx.notify();
        })
        .unwrap();
    // 绘制新布局但不发送 MouseMove，模拟 CTA 变成同一 Tab 按钮的真实交互。
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    cx.dispatcher
        .advance_clock(TEST_TOOLTIP_DELAY + Duration::from_millis(1));
    cx.run_until_parked();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    assert_eq!(tooltip_builds.get(), 0);
}

/// 普通 hover 仍会创建 tooltip，点击也保留已显示的可交互 tooltip。
///
/// @author kongweiguang
#[gpui::test]
fn mouse_down_preserves_visible_hoverable_tooltip(cx: &mut TestAppContext) {
    let (_owner, any_window, tooltip_builds, content_renders, _operation_started, _, _, _) =
        create_tooltip_owner(cx, true);
    hover_tooltip_source(cx, any_window);

    cx.dispatcher.advance_clock(TEST_TOOLTIP_DELAY);
    cx.run_until_parked();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(tooltip_builds.get(), 1);

    let renders_before_click = content_renders.get();
    click_tooltip_source(cx, any_window);
    assert!(content_renders.get() > renders_before_click);
}

/// 已显示的普通提示应随来源卸载消失，且同一元素 ID 重挂不能复用旧提示状态。
///
/// @author kongweiguang
#[gpui::test]
fn visible_tooltip_disappears_after_owner_unmount_and_same_id_remount(cx: &mut TestAppContext) {
    let (
        owner,
        any_window,
        tooltip_builds,
        content_renders,
        operation_started,
        _,
        tooltip_clicks,
        _,
    ) = create_tooltip_owner(cx, false);
    let source_point = point(px(10.), px(10.));
    let old_tooltip_point = point(px(20.), px(20.));

    hover_tooltip_source_at(cx, any_window, source_point);
    cx.dispatcher.advance_clock(TEST_TOOLTIP_DELAY);
    cx.run_until_parked();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(tooltip_builds.get(), 1);
    assert!(content_renders.get() > 0);

    // 先在旧提示范围验证点击确实可命中，避免“卸载后未命中”因夹具从未可交互而假通过。
    click_at(cx, any_window, old_tooltip_point);
    assert_eq!(tooltip_clicks.get(), 1);

    let renders_before_unmount = content_renders.get();
    owner
        .update(cx, |view, _, cx| {
            view.target_visible.set(false);
            cx.notify();
        })
        .unwrap();
    // 不移动指针连续绘制两帧，先验证 tooltip 视图与旧 owner 的 ElementState 都退场。
    for _ in 0..2 {
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(content_renders.get(), renders_before_unmount);
    }

    owner
        .update(cx, |view, _, cx| {
            view.target_visible.set(true);
            view.target_left.set(120.);
            view.operation_started.set(false);
            cx.notify();
        })
        .unwrap();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(tooltip_builds.get(), 1, "同 ID 重挂不应复活旧 tooltip");
    assert_eq!(content_renders.get(), renders_before_unmount);

    // 点击旧 tooltip 曾占据、但已在来源边界之外的位置，确认它不再接收命中事件。
    click_at(cx, any_window, old_tooltip_point);
    assert_eq!(tooltip_clicks.get(), 1);
    assert!(!operation_started.get());
    assert_eq!(content_renders.get(), renders_before_unmount);
}

/// 固定 tooltip 来源被 modal 遮挡后应退出当前 hover 命中栈并停止接收点击。
///
/// 用同一坐标的真实鼠标分发验证，不依赖 tooltip 内容的 render 次数；点击点落在来源元素
/// bounds 内，避免点击本身清理 non-hoverable tooltip。
/// @author kongweiguang
#[gpui::test]
fn visible_tooltip_is_hidden_when_its_source_becomes_occluded(cx: &mut TestAppContext) {
    let (owner, any_window, tooltip_builds, _, _, _, tooltip_clicks, occluded) =
        create_tooltip_owner(cx, false);

    hover_tooltip_source(cx, any_window);
    cx.dispatcher.advance_clock(TEST_TOOLTIP_DELAY);
    cx.run_until_parked();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(tooltip_builds.get(), 1);

    // (20, 20) 位于来源的 60×40 bounds 内；non-hoverable tooltip 因而不会被这次点击提前清除。
    let tooltip_point = point(px(20.), px(20.));
    click_at(cx, any_window, tooltip_point);
    click_at(cx, any_window, tooltip_point);
    assert_eq!(tooltip_clicks.get(), 2, "显示中的 tooltip 应真实接收点击");

    owner
        .update(cx, |_, _, cx| {
            occluded.set(true);
            cx.notify();
        })
        .unwrap();
    cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    click_at(cx, any_window, tooltip_point);
    assert_eq!(
        tooltip_clicks.get(),
        2,
        "来源被 modal 遮挡后，旧 tooltip 不应继续接收点击"
    );
}

/// Root 保留来源 Entity 时，关闭弹层只卸载视图也必须清掉旧 tooltip 状态。
///
/// 测试在关闭后更新背景并重复绘制，再以同一 Entity、同一元素 ID 重挂；不发送 MouseMove，
/// 通过关闭前后同一 tooltip 坐标的真实点击判断旧提示是否残留。
/// @author kongweiguang
#[gpui::test]
fn visible_tooltip_disappears_when_persistent_entity_view_is_unmounted(cx: &mut TestAppContext) {
    let owner = create_persistent_tooltip_owner(cx, false);
    let source_point = point(px(10.), px(10.));
    let tooltip_point = point(px(20.), px(20.));

    hover_tooltip_source_at(cx, owner.window, source_point);
    cx.dispatcher.advance_clock(TEST_TOOLTIP_DELAY);
    cx.run_until_parked();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(owner.tooltip_builds.get(), 1);

    // 点击坐标仍在来源 bounds 内，确认 tooltip 可命中且点击不会提前清理它。
    click_at(cx, owner.window, tooltip_point);
    click_at(cx, owner.window, tooltip_point);
    assert_eq!(owner.tooltip_clicks.get(), 2);
    // 前两次点击也位于来源范围内；清除历史状态，避免把测试夹具副作用误判成卸载后的命中。
    owner.operation_started.set(false);

    owner
        .root
        .update(cx, |root, _, cx| {
            root.source_mounted.set(false);
            root.background_revision.set(1);
            cx.notify();
        })
        .unwrap();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    owner
        .root
        .update(cx, |root, _, cx| {
            root.background_revision.set(2);
            cx.notify();
        })
        .unwrap();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    click_at(cx, owner.window, tooltip_point);
    assert_eq!(
        owner.tooltip_clicks.get(),
        2,
        "Root 持有 Entity 但视图卸载后，旧 tooltip 不应仍可命中"
    );
    assert!(!owner.operation_started.get());

    owner
        .root
        .update(cx, |root, _, cx| {
            root.source_mounted.set(true);
            root.background_revision.set(3);
            cx.notify();
        })
        .unwrap();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    click_at(cx, owner.window, tooltip_point);
    assert_eq!(
        owner.tooltip_clicks.get(),
        2,
        "同一 Entity 和元素 ID 重挂不得复活卸载前的 tooltip"
    );
    assert_eq!(owner.tooltip_builds.get(), 1);
}

/// Root 持有 Entity 并通过 update 手工生成 AnyElement 时，关闭、重绘和同源重挂也必须清理旧提示。
///
/// @author kongweiguang
#[gpui::test]
fn visible_tooltip_disappears_when_manually_rendered_entity_view_is_unmounted(
    cx: &mut TestAppContext,
) {
    let owner = create_persistent_tooltip_owner(cx, true);
    let source_point = point(px(10.), px(10.));
    let tooltip_point = point(px(20.), px(20.));

    hover_tooltip_source_at(cx, owner.window, source_point);
    cx.dispatcher.advance_clock(TEST_TOOLTIP_DELAY);
    cx.run_until_parked();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    assert_eq!(owner.tooltip_builds.get(), 1);

    click_at(cx, owner.window, tooltip_point);
    click_at(cx, owner.window, tooltip_point);
    assert_eq!(owner.tooltip_clicks.get(), 2, "关闭前提示应确实可命中");
    owner.operation_started.set(false);

    owner
        .root
        .update(cx, |root, _, cx| {
            root.source_mounted.set(false);
            root.background_revision.set(1);
            cx.notify();
        })
        .unwrap();
    for revision in [2, 3] {
        cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        owner
            .root
            .update(cx, |root, _, cx| {
                root.background_revision.set(revision);
                cx.notify();
            })
            .unwrap();
    }
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    click_at(cx, owner.window, tooltip_point);
    assert_eq!(
        owner.tooltip_clicks.get(),
        2,
        "手工 AnyElement 卸载后，旧提示不能继续命中"
    );
    assert!(!owner.operation_started.get());

    owner
        .root
        .update(cx, |root, _, cx| {
            root.source_mounted.set(true);
            root.background_revision.set(4);
            cx.notify();
        })
        .unwrap();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    click_at(cx, owner.window, tooltip_point);
    assert_eq!(
        owner.tooltip_clicks.get(),
        2,
        "同一持久 Entity 的 AnyElement 重挂不能复活旧提示"
    );
    assert_eq!(owner.tooltip_builds.get(), 1);
}
