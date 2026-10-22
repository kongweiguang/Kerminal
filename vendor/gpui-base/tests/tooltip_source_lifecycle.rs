//! 覆盖 gpui-component managed tooltip 与 Base Overlay 之间的来源生命周期契约。
//!
//! @author kongweiguang

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    AnyElement, AnyWindowHandle, AppContext as _, Context, Entity, InputEvent as _,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, TestAppContext, Window, WindowHandle,
    div, point, px,
};
use gpui_base::{ElementExt as _, TooltipDefaults, TooltipOverlay, TooltipRequest};

struct ManagedTooltipSource {
    overlay: Entity<TooltipOverlay>,
    tooltip_clicks: Rc<Cell<usize>>,
    bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
    tooltip_bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
}

impl Render for ManagedTooltipSource {
    /// 让预绘制与 hover 回调共享来源边界，同时分别持有 Rc 句柄，避免注册闭包消费另一监听器状态。
    ///
    /// @author kongweiguang
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let hover_bounds = bounds.clone();
        let overlay = self.overlay.clone();
        let tooltip_clicks = self.tooltip_clicks.clone();
        let tooltip_bounds = self.tooltip_bounds.clone();

        div()
            .id("managed-tooltip-source")
            .absolute()
            .left(px(100.))
            .top(px(100.))
            .w(px(60.))
            .h(px(40.))
            .debug_selector(|| "managed-tooltip-trigger".into())
            .on_prepaint(move |current_bounds, _, _| bounds.set(current_bounds))
            .on_hover(move |hovered, window, cx| {
                if *hovered {
                    let Some(_source_id) = window.managed_tooltip_trigger() else {
                        panic!("managed hover callback must expose its stable source ID");
                    };
                    let trigger_bounds = hover_bounds.get();
                    let clicks = tooltip_clicks.clone();
                    let content_bounds = tooltip_bounds.clone();
                    overlay.update(cx, |overlay, cx| {
                        overlay.request_show(
                            TooltipRequest::new(trigger_bounds, move |_, cx| {
                                let clicks = clicks.clone();
                                let bounds = content_bounds.clone();
                                cx.new(move |_| TooltipContent { clicks, bounds }).into()
                            })
                            .with_show_delay(Duration::ZERO),
                            window,
                            cx,
                        );
                    });
                } else {
                    overlay.update(cx, |overlay, cx| overlay.request_hide(window, cx));
                }
            })
    }
}

struct TooltipContent {
    clicks: Rc<Cell<usize>>,
    bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
}

impl Render for TooltipContent {
    /// 用可点击正文和其预绘制实测边界验证 overlay 是否仍参加绘制与命中。
    ///
    /// @author kongweiguang
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let clicks = self.clicks.clone();
        let tooltip_bounds = self.bounds.clone();
        div()
            .id("managed-tooltip-content")
            .w(px(100.))
            .h(px(32.))
            .debug_selector(|| "managed-tooltip-content".into())
            .on_prepaint(move |bounds, _, _| tooltip_bounds.set(bounds))
            .child("managed tooltip")
            .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
    }
}

struct TooltipLifecycleRoot {
    source: Entity<ManagedTooltipSource>,
    overlay: Entity<TooltipOverlay>,
    source_mounted: Rc<Cell<bool>>,
    occluded: Rc<Cell<bool>>,
    background_revision: Rc<Cell<usize>>,
    manual_source_render: bool,
}

impl Render for TooltipLifecycleRoot {
    /// 让 Root 持有来源 Entity，再按状态挂载 keyed View 或手工 AnyElement。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("managed-tooltip-root")
            .relative()
            .size_full()
            .child(
                div()
                    .id("managed-tooltip-background")
                    .size_full()
                    .child(format!("background {}", self.background_revision.get())),
            );

        if self.source_mounted.get() {
            let source: AnyElement = if self.manual_source_render {
                self.source.update(cx, |source, source_cx| {
                    source.render(window, source_cx).into_any_element()
                })
            } else {
                self.source.clone().into_any_element()
            };
            root = root.child(source);
        }

        if self.occluded.get() {
            root = root.child(
                div()
                    .id("managed-tooltip-modal-occlusion")
                    .absolute()
                    .inset_0()
                    .occlude(),
            );
        }

        root.child(self.overlay.clone())
    }
}

struct TooltipLifecycleOwner {
    root: WindowHandle<TooltipLifecycleRoot>,
    window: AnyWindowHandle,
    tooltip_clicks: Rc<Cell<usize>>,
    tooltip_bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
}

/// 创建 persistent source entity，覆盖 Root 条件挂载面板而不销毁其状态的情形。
///
/// @author kongweiguang
fn create_owner(cx: &mut TestAppContext, manual_source_render: bool) -> TooltipLifecycleOwner {
    let tooltip_clicks = Rc::new(Cell::new(0));
    let tooltip_bounds = Rc::new(Cell::new(gpui::Bounds::default()));
    let source_mounted = Rc::new(Cell::new(true));
    let occluded = Rc::new(Cell::new(false));
    let background_revision = Rc::new(Cell::new(0));
    let root = cx.add_window({
        let tooltip_clicks = tooltip_clicks.clone();
        let tooltip_bounds = tooltip_bounds.clone();
        let source_mounted = source_mounted.clone();
        let occluded = occluded.clone();
        let background_revision = background_revision.clone();
        move |_, cx| {
            let overlay = cx.new(|_| TooltipOverlay::new());
            let source = cx.new(|_| ManagedTooltipSource {
                overlay: overlay.clone(),
                tooltip_clicks,
                bounds: Rc::new(Cell::new(gpui::Bounds::default())),
                tooltip_bounds,
            });
            TooltipLifecycleRoot {
                source,
                overlay,
                source_mounted,
                occluded,
                background_revision,
                manual_source_render,
            }
        }
    });
    let window = AnyWindowHandle::from(root);
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
        .expect("initial tooltip lifecycle window draw must succeed");

    TooltipLifecycleOwner {
        root,
        window,
        tooltip_clicks,
        tooltip_bounds,
    }
}

/// 移动到真实来源并绘制，触发 Base Overlay 的 managed hover 请求。
///
/// @author kongweiguang
fn hover_source(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| {
        window.simulate_mouse_move(point(px(110.), px(110.)), cx);
        window.draw(cx).clear(cx);
    })
    .expect("hovering the managed tooltip source must succeed");
}

/// 从内容自身的预绘制回调读取真实布局，避免依赖测试专用 debug-bounds API 或固定放置方向。
///
/// @author kongweiguang
fn tooltip_center(owner: &TooltipLifecycleOwner) -> gpui::Point<gpui::Pixels> {
    let bounds = owner.tooltip_bounds.get();
    assert!(
        bounds.size.width > px(0.) && bounds.size.height > px(0.),
        "visible tooltip content must publish its prepaint bounds"
    );
    bounds.center()
}

/// 经 GPUI 的公开鼠标分发路径点击位置，验证 tooltip 的当前命中状态。
///
/// @author kongweiguang
fn click_at(cx: &mut TestAppContext, window: AnyWindowHandle, position: gpui::Point<gpui::Pixels>) {
    cx.update_window(window, |_, window, cx| {
        window.simulate_mouse_move(position, cx);
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
    .expect("tooltip hit-test input must succeed");
}

/// 真实 managed tooltip 在 cached View 重绘中保持可见，且鼠标进入提示区时 grace 生效。
///
/// @author kongweiguang
#[gpui::test]
fn managed_tooltip_survives_cached_frames_and_pointer_grace(cx: &mut TestAppContext) {
    cx.update(|app| {
        TooltipDefaults::new()
            .with_show_delay(Duration::ZERO)
            .with_grace_period(Duration::from_secs(2))
            .install(app);
    });
    let owner = create_owner(cx, false);
    hover_source(cx, owner.window);
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    let tip = tooltip_center(&owner);
    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        1,
        "source hover must show a clickable tooltip"
    );

    for _ in 0..2 {
        cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }
    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        2,
        "cached source prepaint must remain live"
    );
}

/// 用同一断言覆盖 keyed Entity 和 Root 手工物化 AnyElement 的卸载与重挂路径。
///
/// @author kongweiguang
fn assert_tooltip_dies_after_persistent_source_unmount(
    cx: &mut TestAppContext,
    manual_source_render: bool,
) {
    cx.update(|app| {
        TooltipDefaults::new()
            .with_show_delay(Duration::ZERO)
            .install(app)
    });

    let owner = create_owner(cx, manual_source_render);
    hover_source(cx, owner.window);
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    let tip = tooltip_center(&owner);
    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        1,
        "tooltip must be hittable before unmount"
    );

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
    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        1,
        "unmounted source tooltip must not receive input"
    );

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
    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        1,
        "same-ID remount cannot revive old token"
    );

    hover_source(cx, owner.window);
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    let fresh_tip = tooltip_center(&owner);
    click_at(cx, owner.window, fresh_tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        2,
        "a fresh hover must create a new live request"
    );
}

/// keyed `Entity` 从 Root 子树卸载后，旧 tooltip 不应因 Root 仍持有 Entity 而存活。
///
/// @author kongweiguang
#[gpui::test]
fn managed_tooltip_dies_after_persistent_keyed_source_unmount(cx: &mut TestAppContext) {
    assert_tooltip_dies_after_persistent_source_unmount(cx, false);
}

/// 手工 `Entity::update` 生成的 AnyElement 卸载后也必须结束旧 tooltip 生命周期。
///
/// @author kongweiguang
#[gpui::test]
fn managed_tooltip_dies_after_persistent_manual_source_unmount(cx: &mut TestAppContext) {
    assert_tooltip_dies_after_persistent_source_unmount(cx, true);
}

/// 来源仍挂载但被 modal 遮挡时，deferred prepaint 使当前请求永久失效。
///
/// 鼠标停在触发器上完成遮挡校验；之后再点击旧 popup 坐标，避免 pointer grace 掩盖遮挡结果。
/// @author kongweiguang
#[gpui::test]
fn managed_tooltip_dies_when_source_is_occluded(cx: &mut TestAppContext) {
    cx.update(|app| {
        TooltipDefaults::new()
            .with_show_delay(Duration::ZERO)
            .install(app)
    });
    let owner = create_owner(cx, false);
    hover_source(cx, owner.window);
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    let tip = tooltip_center(&owner);
    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        1,
        "tooltip must be clickable before occlusion"
    );

    // 先将指针放回来源，随后新增 modal；遮挡帧在指针仍处于来源 bounds 时做 hit-stack 判断。
    cx.update_window(owner.window, |_, window, cx| {
        window.simulate_mouse_move(point(px(110.), px(110.)), cx);
        window.draw(cx).clear(cx);
    })
    .unwrap();
    owner
        .root
        .update(cx, |root, _, cx| {
            root.occluded.set(true);
            cx.notify();
        })
        .unwrap();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
    cx.update_window(owner.window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();

    click_at(cx, owner.window, tip);
    assert_eq!(
        owner.tooltip_clicks.get(),
        1,
        "occluded tooltip must stop receiving clicks"
    );
}
