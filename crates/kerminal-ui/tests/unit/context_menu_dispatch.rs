//! 嵌套 Kit 菜单的真实鼠标派发回归；覆盖左栏对象菜单与背景菜单共享命中区域的结构。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    Context, FocusHandle, IntoElement, MouseButton, Render, TestAppContext, Window,
    component::menu::{ContextMenuExt as _, PopupMenuItem},
    div, point,
    prelude::*,
    px,
};

/// 菜单构建记录证明只有命中对象获得打开权；动作记录另行证明键盘和鼠标能实际执行。
struct NestedMenus {
    opened: Rc<RefCell<Vec<&'static str>>>,
    invoked: Rc<RefCell<Vec<&'static str>>>,
    focus: FocusHandle,
}

impl Render for NestedMenus {
    /// 使用生产 Kit 的嵌套组合，避免手动调用菜单闭包绕过真正出错的鼠标冒泡顺序。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let opened = self.opened.clone();
        let invoked = self.invoked.clone();
        div()
            .id("navigation")
            .size_full()
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .children(["group", "host", "profile", "container"].map(|target| {
                let opened = self.opened.clone();
                let invoked = self.invoked.clone();
                div()
                    .id(target)
                    .w_full()
                    .h(px(40.))
                    .flex_shrink_0()
                    .context_menu(move |menu, _, _| {
                        opened.borrow_mut().push(target);
                        let invoked = invoked.clone();
                        menu.item(PopupMenuItem::new(target).on_click(move |_, _, _| {
                            invoked.borrow_mut().push(target);
                        }))
                    })
            }))
            .context_menu(move |menu, _, _| {
                opened.borrow_mut().push("background");
                let invoked = invoked.clone();
                menu.item(PopupMenuItem::new("background").on_click(move |_, _, _| {
                    invoked.borrow_mut().push("background");
                }))
            })
    }
}

/// 同一次右键只能打开最内层菜单；关闭后换对象或背景仍能打开并执行，焦点回到原导航。
#[gpui_kit::test]
fn nested_context_menu_opens_only_the_clicked_target(app: &mut TestAppContext) {
    app.update(crate::init);
    let opened = Rc::new(RefCell::new(Vec::new()));
    let invoked = Rc::new(RefCell::new(Vec::new()));
    let (root, visual) = app.add_window_view({
        let opened = opened.clone();
        let invoked = invoked.clone();
        move |window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            NestedMenus {
                opened,
                invoked,
                focus,
            }
        }
    });
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));

    for (ix, target) in ["group", "host", "profile", "container", "background"]
        .into_iter()
        .enumerate()
    {
        opened.borrow_mut().clear();
        let position = point(px(10.), px(ix as f32 * 40. + 20.));
        visual.simulate_mouse_down(position, MouseButton::Right, Default::default());
        visual.simulate_mouse_up(position, MouseButton::Right, Default::default());
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            &*opened.borrow(),
            &[target],
            "one press must claim one menu"
        );

        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(root.read(cx).focus.is_focused(window));
        });
        assert_eq!(
            invoked.borrow().len(),
            ix,
            "Escape must not execute an item"
        );
        opened.borrow_mut().clear();
        visual.simulate_mouse_down(position, MouseButton::Right, Default::default());
        visual.simulate_mouse_up(position, MouseButton::Right, Default::default());
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(&*opened.borrow(), &[target]);
        if ix % 2 == 0 {
            visual.simulate_keystrokes("down enter");
        } else {
            let item = point(position.x + px(30.), position.y + px(17.));
            visual.simulate_mouse_move(item, None, Default::default());
            visual.simulate_click(item, Default::default());
        }
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(invoked.borrow().last(), Some(&target));
        assert_eq!(
            invoked.borrow().len(),
            ix + 1,
            "action must fire exactly once"
        );
    }

    // 不先按 Escape，连续在不同区域右键也只能保留一个菜单。
    for y in [20., 140., 180., 60.] {
        let position = point(px(300.), px(y));
        visual.simulate_mouse_down(position, MouseButton::Right, Default::default());
        visual.simulate_mouse_up(position, MouseButton::Right, Default::default());
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let menus = gpui_kit::base::test_support::snapshots(window);
            assert_eq!(
                menus
                    .iter()
                    .filter(|node| node.visible() && node.role() == Some(gpui_kit::Role::Menu))
                    .count(),
                1
            );
        });
    }
}
