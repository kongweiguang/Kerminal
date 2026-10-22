//! 主机选择列表搜索匹配规则的纯单元测试。
//!
//! @author kongweiguang

use super::*;

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
