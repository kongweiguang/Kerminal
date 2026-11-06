//! 主机选择列表搜索匹配规则的纯单元测试。
//!
//! @author kongweiguang

use super::*;
use std::{cell::RefCell, rc::Rc};

use gpui_kit::{Render, TestAppContext, VisualTestContext};

struct HostPickerRefreshHarness {
    search: gpui_kit::Entity<InputState>,
    options: Vec<HostPickerOption<String>>,
    loading: bool,
    selected: Rc<RefCell<Vec<String>>>,
}

impl Render for HostPickerRefreshHarness {
    /// 以生产列表与同一输入实体验证刷新期间的行位置和键盘选择。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected.clone();
        let list = HostPickerList::new(
            "host-picker-refresh-test",
            self.options.clone(),
            move |target, _, _| {
                selected.borrow_mut().push(target);
            },
        )
        .search_input(self.search.clone())
        .groups(vec![HostPickerGroup {
            id: "group".into(),
            title: "Hosts".into(),
        }]);
        if self.loading {
            list.message(HostPickerListMessage::Loading("Loading hosts…".into()))
        } else {
            list
        }
    }
}

/// 两次绘制完成命中测试和文本测量，避免用未布局的列表断言行位置。
///
/// @author kongweiguang
fn draw_picker(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, app| window.draw(app).clear(app));
    cx.update(|window, app| window.draw(app).clear(app));
}

/// 有旧结果的刷新不能插入顶置状态行；方向键滚动与 Enter 仍对应同一可见目标。
///
/// @author kongweiguang
#[gpui_kit::test]
fn refreshing_keeps_rows_fixed_and_keyboard_targets_aligned(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_kit::init(app);
        app.set_reduce_motion(true);
    });
    let slot = Rc::new(RefCell::new(None));
    let output = slot.clone();
    let selected = Rc::new(RefCell::new(Vec::new()));
    let selected_for_window = selected.clone();
    let (_, visual) = cx.add_window_view(move |window, cx| {
        let search = cx.new(|cx| InputState::new(window, cx));
        let harness = cx.new(|_| HostPickerRefreshHarness {
            search,
            options: ["alpha", "beta"]
                .into_iter()
                .map(|name| HostPickerOption {
                    key: name.into(),
                    target: name.to_owned(),
                    title: name.into(),
                    detail: Some("SSH".into()),
                    group_id: Some("group".into()),
                })
                .collect(),
            loading: false,
            selected: selected_for_window,
        });
        *output.borrow_mut() = Some(harness.clone());
        gpui_kit::base::Root::new(harness, window, cx)
    });
    let harness = slot.borrow_mut().take().expect("picker harness mounted");
    draw_picker(visual);
    let alpha = visual
        .debug_bounds("host-picker-refresh-test-option-alpha")
        .expect("first row");
    let beta = visual
        .debug_bounds("host-picker-refresh-test-option-beta")
        .expect("second row");

    visual.update(|window, app| {
        harness.update(app, |harness, cx| {
            harness.loading = true;
            harness
                .search
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        });
    });
    draw_picker(visual);
    assert_eq!(
        visual.debug_bounds("host-picker-refresh-test-option-alpha"),
        Some(alpha)
    );
    assert_eq!(
        visual.debug_bounds("host-picker-refresh-test-option-beta"),
        Some(beta)
    );
    visual.simulate_keystrokes("down");
    draw_picker(visual);
    visual.simulate_keystrokes("enter");
    assert_eq!(selected.borrow().as_slice(), &["beta"]);
}

/// 搜索统一覆盖稳定 ID、主机信息和真实分组标题，避免三个入口各自维护过滤规则。
///
/// @author kongweiguang
#[test]
fn host_picker_search_matches_identity_details_and_group_title() {
    let option = HostPickerOption {
        key: "build-host-id".into(),
        target: (),
        title: "Build Server".into(),
        detail: Some("SSH · deploy@192.0.2.40:2222".into()),
        group_id: Some("group-1".to_owned()),
    };
    let group_title = SharedString::from("Production Database Hosts");

    assert!(option_matches_query(&option, "build", Some(&group_title)));
    assert!(option_matches_query(&option, "HOST-ID", Some(&group_title)));
    assert!(option_matches_query(
        &option,
        "ssh deploy",
        Some(&group_title)
    ));
    assert!(option_matches_query(
        &option,
        "192.0.2.40",
        Some(&group_title)
    ));
    assert!(option_matches_query(&option, "2222", Some(&group_title)));
    assert!(option_matches_query(
        &option,
        "production database",
        Some(&group_title)
    ));
    assert!(!option_matches_query(
        &option,
        "staging",
        Some(&group_title)
    ));
}

/// IME 组合期间不改变高亮或提交；普通方向键限制在可见结果边界内。
///
/// @author kongweiguang
#[test]
fn host_picker_keyboard_navigation_respects_ime_and_result_bounds() {
    assert_eq!(
        host_picker_key_action("ArrowDown", 0, 3, false),
        HostPickerKeyAction::MoveTo(1)
    );
    assert_eq!(
        host_picker_key_action("ArrowUp", 0, 3, false),
        HostPickerKeyAction::MoveTo(0)
    );
    assert_eq!(
        host_picker_key_action("ArrowDown", 99, 3, false),
        HostPickerKeyAction::MoveTo(2)
    );
    assert_eq!(
        host_picker_key_action("Enter", 1, 3, false),
        HostPickerKeyAction::Select
    );
    assert_eq!(
        host_picker_key_action("Enter", 1, 3, true),
        HostPickerKeyAction::Ignore
    );
    assert_eq!(
        host_picker_key_action("ArrowDown", 1, 3, true),
        HostPickerKeyAction::Ignore
    );
    assert_eq!(
        host_picker_key_action("Enter", 0, 0, false),
        HostPickerKeyAction::Ignore
    );
}
