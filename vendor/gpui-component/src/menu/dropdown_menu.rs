// @author kongweiguang

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, Context, DismissEvent, Element, ElementId, Entity, FocusHandle,
    Focusable, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    RenderOnce, SharedString, Styled, Window, prelude::FluentBuilder,
};

use crate::{Selectable, button::Button, menu::PopupMenu, popover::Popover};

/// A dropdown menu trait for buttons and other interactive elements
pub trait DropdownMenu: Styled + Selectable + InteractiveElement + IntoElement + 'static {
    /// Create a dropdown menu with the given items, anchored to the TopLeft corner
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        self.dropdown_menu_with_anchor(Anchor::TopLeft, f)
    }

    /// Create a dropdown menu with the given items, anchored to the given corner
    fn dropdown_menu_with_anchor(
        mut self,
        anchor: impl Into<Anchor>,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self> {
        let id = self.interactivity().element_id.clone();

        DropdownMenuPopover::new(id.unwrap_or(0.into()), anchor, self, f)
    }
}

impl DropdownMenu for Button {}

#[derive(IntoElement)]
pub struct DropdownMenuPopover<T: Selectable + IntoElement + 'static> {
    id: ElementId,
    anchor: Anchor,
    auto_flip: bool,
    trigger_width: Option<(gpui::Pixels, gpui::Pixels)>,
    available_height: Option<(gpui::Pixels, gpui::Pixels)>,
    offset: Option<gpui::Pixels>,
    trigger: T,
    builder: Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>,
    on_open_change: Option<Rc<dyn Fn(&bool, &mut Window, &mut App)>>,
}

impl<T> DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    /// 默认沿用 legacy corner 定位，仅将调用方显式选择的行为存入 facade。
    fn new(
        id: ElementId,
        anchor: impl Into<Anchor>,
        trigger: T,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        Self {
            id: SharedString::from(format!("dropdown-menu:{:?}", id)).into(),
            anchor: anchor.into(),
            auto_flip: false,
            trigger_width: None,
            available_height: None,
            offset: None,
            trigger,
            builder: Rc::new(builder),
            on_open_change: None,
        }
    }

    /// Set the anchor corner for the dropdown menu popover.
    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }

    /// 显式允许菜单在首选边空间不足时翻到触发器相反侧。
    pub fn auto_flip(mut self) -> Self {
        self.auto_flip = true;
        self
    }

    /// Constrain the menu width to its trigger within the provided pixel range.
    pub fn trigger_width(mut self, min: gpui::Pixels, max: gpui::Pixels) -> Self {
        self.trigger_width = Some((min, max));
        self
    }

    /// Enables vertical placement and caps content before its first layout.
    pub fn available_height(
        mut self,
        minimum_space: gpui::Pixels,
        max_height: gpui::Pixels,
    ) -> Self {
        self.auto_flip = true;
        self.available_height = Some((
            minimum_space.max(gpui::px(0.)),
            max_height.max(gpui::px(0.)),
        ));
        self
    }

    /// Set the trigger-to-menu gap; omitted values preserve Popover's existing default.
    pub fn offset(mut self, offset: impl Into<gpui::Pixels>) -> Self {
        self.offset = Some(offset.into());
        self
    }

    /// Add a callback to be called when the menu opens or closes.
    ///
    /// The `&bool` parameter is the **new open state**.
    pub fn on_open_change(
        mut self,
        callback: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(callback));
        self
    }
}

#[derive(Default)]
struct DropdownMenuState {
    menu: Option<Entity<PopupMenu>>,
}

impl<T> RenderOnce for DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        TriggerFocus::new(self.id.clone(), move |trigger_focus, window, cx| {
            self.render_popover(trigger_focus, window, cx)
                .into_any_element()
        })
    }
}

impl<T> DropdownMenuPopover<T>
where
    T: Selectable + IntoElement + 'static,
{
    /// 将菜单定位选项交给共享 Popover，焦点和关闭行为仍保持单一 owner。
    fn render_popover(
        self,
        trigger_focus: FocusHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Popover {
        // Copy the opt-in before moving the trigger into the shared Popover facade.
        let auto_flip = self.auto_flip;
        let trigger_width = self.trigger_width;
        let available_height = self.available_height;
        let offset = self.offset;
        let builder = self.builder.clone();
        let menu_state =
            window.use_keyed_state(self.id.clone(), cx, |_, _| DropdownMenuState::default());

        Popover::new(SharedString::from(format!("popover:{}", self.id)))
            .appearance(false)
            .overlay_closable(false)
            .trigger(self.trigger)
            .anchor(self.anchor)
            .when(auto_flip, |this| this.auto_flip())
            .when_some(trigger_width, |this, (min, max)| {
                this.trigger_width(min, max)
            })
            .when_some(available_height, |this, (minimum, maximum)| {
                this.available_height(minimum, maximum)
            })
            .when_some(offset, |this, offset| this.offset(offset))
            .when_some(self.on_open_change, |this, callback| {
                this.on_open_change(move |open, window, cx| callback(open, window, cx))
            })
            .content(move |_, window, cx| {
                // Here is special logic to only create the PopupMenu once and reuse it.
                // Because this `content` will called in every time render, so we need to store the menu
                // in state to avoid recreating at every render.
                //
                // And we also need to rebuild the menu when it is dismissed, to rebuild menu items
                // dynamically for support `dropdown_menu` method, so we listen for DismissEvent below.
                let menu = match menu_state.read(cx).menu.clone() {
                    Some(menu) => menu,
                    None => {
                        let builder = builder.clone();
                        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                            builder(menu, window, cx)
                        });
                        menu.update(cx, |menu, cx| {
                            menu.set_trigger_focus(Some(trigger_focus.clone()), cx)
                        });
                        menu_state.update(cx, |state, _| {
                            state.menu = Some(menu.clone());
                        });
                        menu.focus_handle(cx).focus(window, cx);

                        // Listen for dismiss events from the PopupMenu to close the popover.
                        //
                        // Hold Weak handles here, not strong clones: the listener
                        // lives as long as `menu`, which `menu_state` owns, so a
                        // strong capture would close the cycle
                        // `menu_state -> menu -> listener -> menu_state` and leak
                        // the `PopupMenu` and `DropdownMenuState` when the trigger
                        // stops being rendered while the menu is open.
                        let popover_state = cx.entity().downgrade();
                        window
                            .subscribe(&menu, cx, {
                                let menu_state = menu_state.downgrade();
                                move |_, _: &DismissEvent, window, cx| {
                                    if let Some(popover_state) = popover_state.upgrade() {
                                        popover_state.update(cx, |state, cx| {
                                            state.dismiss(window, cx);
                                        });
                                    }
                                    _ = menu_state.update(cx, |state, _| {
                                        state.menu = None;
                                    });
                                }
                            })
                            .detach();

                        menu.clone()
                    }
                };

                menu.clone()
            })
    }
}

type TriggerFocusBuild = Box<dyn FnOnce(FocusHandle, &mut Window, &mut App) -> AnyElement>;

/// Registers a focus handle on the trigger's dispatch node without ever
/// focusing it, so the menu opened from the trigger can resolve its shortcut
/// hints against the trigger's key contexts on the frame it opens. GPUI looks
/// a handle up in the previously rendered frame; the trigger was in it when
/// the menu was not yet.
struct TriggerFocus {
    id: ElementId,
    build: Option<TriggerFocusBuild>,
}

#[derive(Default)]
struct TriggerFocusState {
    focus_handle: Option<FocusHandle>,
}

struct TriggerFocusFrame {
    focus_handle: FocusHandle,
    child: AnyElement,
}

impl TriggerFocus {
    fn new(
        id: ElementId,
        build: impl FnOnce(FocusHandle, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id,
            build: Some(Box::new(build)),
        }
    }
}

impl IntoElement for TriggerFocus {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TriggerFocus {
    type RequestLayoutState = TriggerFocusFrame;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let focus_handle =
            window.with_optional_element_state::<TriggerFocusState, _>(id, |state, _| {
                let mut state = state.flatten().unwrap_or_default();
                let focus_handle = state
                    .focus_handle
                    .get_or_insert_with(|| cx.focus_handle())
                    .clone();
                (focus_handle, Some(state))
            });
        let build = self.build.take().expect("TriggerFocus is laid out once");
        let mut child = build(focus_handle.clone(), window, cx);
        let layout_id = child.request_layout(window, cx);

        (
            layout_id,
            TriggerFocusFrame {
                focus_handle,
                child,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: gpui::Bounds<gpui::Pixels>,
        frame: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.set_focus_handle(&frame.focus_handle, cx);
        frame.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: gpui::Bounds<gpui::Pixels>,
        frame: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        frame.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        KeyBinding, MouseButton, ParentElement as _, Render, TestAppContext, WeakEntity, actions,
        div, point, px,
    };
    use std::cell::{Cell, RefCell};

    actions!(dropdown_menu_test, [CopyText]);

    const CONTEXT: &str = "dropdown_menu_test";

    /// Explicit menu gaps remain optional so all existing dropdowns keep Popover's default spacing.
    #[test]
    fn dropdown_offset_is_opt_in() {
        let menu = Button::new("default-offset")
            .dropdown_menu(|menu, _, _| menu.menu("Copy", Box::new(CopyText)));
        assert_eq!(menu.offset, None);

        let menu = Button::new("custom-offset")
            .dropdown_menu(|menu, _, _| menu.menu("Copy", Box::new(CopyText)))
            .offset(px(6.));
        assert_eq!(menu.offset, Some(px(6.)));
    }

    /// A constrained dropdown opts into side placement while other menus keep their defaults.
    #[test]
    fn dropdown_available_height_is_opt_in() {
        let menu = Button::new("default-height").dropdown_menu(|menu, _, _| menu);
        assert!(!menu.auto_flip);
        assert_eq!(menu.available_height, None);

        let menu = Button::new("bounded-height")
            .dropdown_menu(|menu, _, _| menu)
            .available_height(px(152.), px(320.));
        assert!(menu.auto_flip);
        assert_eq!(menu.available_height, Some((px(152.), px(320.))));
    }

    /// The story shape: the key binding lives in the key context of the
    /// trigger's ancestor, the menu names no `action_context`, and other
    /// content outside that context paints after the trigger.
    struct TestRoot {
        frames: Rc<Cell<usize>>,
    }

    impl Render for TestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.frames.set(self.frames.get() + 1);
            div()
                .size_full()
                .child(
                    div()
                        .key_context(CONTEXT)
                        .on_action(|_: &CopyText, _, _| {})
                        .child(
                            Button::new("trigger")
                                .label("Edit")
                                .w(px(100.))
                                .h(px(30.))
                                .dropdown_menu(|menu, _, _| menu.menu("Copy", Box::new(CopyText))),
                        ),
                )
                .child(div().child("Status"))
        }
    }

    /// Records the `PopupMenu` the dropdown builds, so a test can observe
    /// its release through a weak handle.
    struct MenuProbeRoot {
        menu: Rc<RefCell<Option<WeakEntity<PopupMenu>>>>,
    }

    impl Render for MenuProbeRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let slot = self.menu.clone();
            div().size_full().child(
                Button::new("trigger")
                    .label("Edit")
                    .w(px(100.))
                    .h(px(30.))
                    .dropdown_menu(move |menu, _, cx| {
                        *slot.borrow_mut() = Some(cx.weak_entity());
                        menu.menu("Copy", Box::new(CopyText))
                    }),
            )
        }
    }

    /// Opening the menu and closing the window without dismissing it must
    /// release the `PopupMenu`: the dismiss subscription lives as long as the
    /// menu, which `DropdownMenuState` owns, so strong captures in it formed
    /// a cycle that no element-state collection could break. The state still
    /// holds the menu while it is open, so the menu's release also proves the
    /// `DropdownMenuState` was released.
    ///
    /// The popover must go too: it holds the deferred-popover registration
    /// while open, and a leaked one keeps every later right-click menu
    /// stepping aside as if a popup were still showing.
    #[gpui::test]
    fn open_without_dismiss_releases_the_menu(cx: &mut TestAppContext) {
        cx.update(|cx| crate::init(cx));
        let menu = Rc::new(RefCell::new(None::<WeakEntity<PopupMenu>>));

        {
            let (_, cx) = cx.add_window_view(|_, _| MenuProbeRoot { menu: menu.clone() });
            cx.update(|window, cx| window.draw(cx).clear(cx));

            // Click the trigger; the menu opens and is left open.
            cx.simulate_mouse_down(
                point(px(10.), px(10.)),
                MouseButton::Left,
                Default::default(),
            );
            cx.run_until_parked();
            assert!(
                menu.borrow()
                    .as_ref()
                    .and_then(|menu| menu.upgrade())
                    .is_some(),
                "the menu must be open before the window closes"
            );

            // Close the window without dismissing the menu.
            cx.update(|window, _| window.remove_window());
            cx.run_until_parked();
        }

        assert!(
            menu.borrow()
                .as_ref()
                .and_then(|menu| menu.upgrade())
                .is_none(),
            "the PopupMenu must be released with the window"
        );
        cx.update(|cx| {
            assert!(
                !gpui_base::GlobalState::is_in_deferred_context(cx),
                "the popover's deferred registration must be released with the window"
            )
        });
    }

    #[gpui::test]
    fn shortcut_hint_is_painted_on_the_frame_the_menu_opens(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx);
            cx.bind_keys([KeyBinding::new("ctrl-c", CopyText, Some(CONTEXT))]);
        });
        let frames = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let frames = frames.clone();
            move |_, _| TestRoot { frames }
        });
        // The popup host captures its trigger bounds on the first frame.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let frames_before_open = frames.get();

        cx.simulate_mouse_down(
            point(px(10.), px(10.)),
            MouseButton::Left,
            Default::default(),
        );

        assert_eq!(
            frames.get(),
            frames_before_open + 1,
            "the press must be followed by exactly one frame for this to test the first one"
        );
        assert!(
            cx.debug_bounds("kbd:ctrl-c").is_some(),
            "the shortcut hint must be painted on the same frame as its item"
        );
    }
}
