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

/// 分组勾选是整组原子操作：部分选中时补齐，整组已选时整组取消，主机行复用同一规则。
#[test]
fn toggling_host_keys_flips_whole_group_atomically() {
    let keys = |values: &[&str]| {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let group = vec!["g1a".to_owned(), "g1b".to_owned()];

    let partial = keys(&["g1a", "g2a"]);
    let filled = toggled_host_keys(&partial, &group);
    assert_eq!(filled, keys(&["g1a", "g1b", "g2a"]));

    let cleared = toggled_host_keys(&filled, &group);
    assert_eq!(cleared, keys(&["g2a"]));

    let single = toggled_host_keys(&cleared, &["g2a".to_owned()]);
    assert!(
        single.is_empty(),
        "single host toggles off when already selected"
    );
}

/// Shift 连选只覆盖可见行；锚点被搜索隐藏时退化为当前行并请求 adopt_anchor。
#[test]
fn range_selection_respects_visible_groups_and_missing_anchor() {
    let row = |key: &str, hosts: &[&str]| {
        (
            key.to_owned(),
            hosts.iter().map(|host| (*host).to_owned()).collect(),
        )
    };
    let rows = vec![
        row("group:g1", &["a", "b"]),
        row("a", &["a"]),
        row("b", &["b"]),
        row("group:g2", &["c", "d"]),
        row("c", &["c"]),
        row("d", &["d"]),
    ];

    let (keys, adopt) = host_picker_range_keys(&rows, Some("a"), "c");
    assert_eq!(keys, vec!["a", "b", "c", "d"]);
    assert!(!adopt, "a visible anchor stays in place");

    let (keys, adopt) = host_picker_range_keys(&rows, Some("group:g1"), "group:g2");
    assert_eq!(keys, vec!["a", "b", "c", "d"]);
    assert!(!adopt);

    let (keys, adopt) = host_picker_range_keys(&rows, Some("filtered-out"), "b");
    assert_eq!(keys, vec!["b"]);
    assert!(
        adopt,
        "a hidden anchor hands the current row back as the new anchor"
    );
}

/// 多选harness保留搜索输入（搜索行右侧的“打开所选”按钮依赖它）并记录列表回调。
struct HostPickerMultiHarness {
    search: gpui_kit::Entity<InputState>,
    options: Vec<HostPickerOption<String>>,
    selected: std::collections::BTreeSet<String>,
    toggles: Rc<RefCell<Vec<Vec<String>>>>,
    opens: Rc<RefCell<usize>>,
    clicks: Rc<RefCell<Vec<String>>>,
}

impl Render for HostPickerMultiHarness {
    /// 使用生产多选列表，勾选与打开回调只记录意图，不创建业务资源。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let toggles = self.toggles.clone();
        let opens = self.opens.clone();
        let clicks = self.clicks.clone();
        HostPickerList::new(
            "host-picker-multi-test",
            self.options.clone(),
            move |target, _, _| clicks.borrow_mut().push(target),
        )
        .search_input(self.search.clone())
        .groups(vec![HostPickerGroup {
            id: "group".into(),
            title: "Group".into(),
        }])
        .multi_select(HostPickerMultiSelect {
            selected_keys: self.selected.clone(),
            anchor: None,
            on_toggle: Rc::new(move |row, _, _| toggles.borrow_mut().push(row.host_keys)),
            on_range: Rc::new(|_, _, _| {}),
            on_open_selected: Rc::new(move |_, _| *opens.borrow_mut() += 1),
        })
    }
}

fn multi_options() -> Vec<HostPickerOption<String>> {
    ["a", "b"]
        .into_iter()
        .map(|key| HostPickerOption {
            key: key.into(),
            target: key.to_owned(),
            title: format!("Host {key}").into(),
            detail: None,
            group_id: Some("group".into()),
        })
        .collect()
}

type MultiHarness<'a> = (
    Rc<RefCell<Vec<Vec<String>>>>,
    Rc<RefCell<usize>>,
    Rc<RefCell<Vec<String>>>,
    &'a mut VisualTestContext,
);

fn mount_multi<'a>(cx: &'a mut TestAppContext, selected: &[&str]) -> MultiHarness<'a> {
    cx.update(|app| {
        gpui_kit::init(app);
        app.set_reduce_motion(true);
    });
    let toggles: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let opens: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
    let clicks: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let selected = selected
        .iter()
        .map(|key| (*key).to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let toggles_for_view = toggles.clone();
    let opens_for_view = opens.clone();
    let clicks_for_view = clicks.clone();
    let (_, visual) = cx.add_window_view(move |window, cx| {
        let search = cx.new(|cx| InputState::new(window, cx));
        let harness = cx.new(|_| HostPickerMultiHarness {
            search,
            options: multi_options(),
            selected,
            toggles: toggles_for_view,
            opens: opens_for_view,
            clicks: clicks_for_view,
        });
        gpui_kit::base::Root::new(harness, window, cx)
    });
    (toggles, opens, clicks, visual)
}

fn static_id(id: String) -> &'static str {
    Box::leak(id.into_boxed_str())
}

fn debug_center(visual: &mut VisualTestContext, id: &str) -> gpui_kit::Point<gpui_kit::Pixels> {
    draw_picker(visual);
    let selector = static_id(id.to_owned());
    visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("element {id:?} is missing"))
        .center()
}

/// 勾选槽位只切换选择，不触发“直接打开”；未勾选时单击主机仍默认打开。
///
/// @author kongweiguang
#[gpui_kit::test]
fn multi_select_checkbox_toggles_without_opening(cx: &mut TestAppContext) {
    let (toggles, opens, clicks, visual) = mount_multi(cx, &[]);
    let check = debug_center(visual, "host-picker-multi-test-check-a");
    visual.simulate_click(check, Default::default());
    draw_picker(visual);
    assert_eq!(&*toggles.borrow(), &[vec!["a".to_owned()]]);
    assert!(
        clicks.borrow().is_empty(),
        "checkbox must not open the host"
    );
    assert_eq!(*opens.borrow(), 0);

    let row = debug_center(visual, "host-picker-multi-test-option-a");
    visual.simulate_click(row, Default::default());
    draw_picker(visual);
    assert_eq!(&*clicks.borrow(), &["a".to_owned()]);
}

/// 已有勾选后单击行改为勾选切换，搜索行右侧按钮一次打开全部所选。
///
/// @author kongweiguang
#[gpui_kit::test]
fn multi_select_row_toggles_when_selection_exists_and_open_button_batches(cx: &mut TestAppContext) {
    let (toggles, opens, clicks, visual) = mount_multi(cx, &["b"]);
    let row = debug_center(visual, "host-picker-multi-test-option-a");
    visual.simulate_click(row, Default::default());
    draw_picker(visual);
    assert_eq!(&*toggles.borrow(), &[vec!["a".to_owned()]]);
    assert!(
        clicks.borrow().is_empty(),
        "with an existing selection, a row click toggles instead of opening"
    );

    let id: gpui_kit::ElementId = "host-picker-multi-test-open-selected".into();
    let open = visual.update(|window, _| {
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("open-selected button is rendered")
            .bounds()
            .center()
    });
    visual.simulate_click(open, Default::default());
    draw_picker(visual);
    assert_eq!(*opens.borrow(), 1);
    assert!(clicks.borrow().is_empty());
}
