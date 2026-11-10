//! Workspace layout invariants and v3 persistence compatibility.
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_domain::{
    identity::PaneId,
    workspace::{SplitDirection, SplitLayoutSizes, TerminalLayoutNode, WorkspaceLayoutError},
};

/// Creates stable pane IDs through the same validation path used for persisted values.
fn pane_id(value: &str) -> PaneId {
    PaneId::parse(value).expect("test pane ids are valid")
}

/// Keeps fixture trees compact while making child ordering explicit in each test.
fn split(
    id: &str,
    children: Vec<TerminalLayoutNode>,
    sizes: Option<SplitLayoutSizes>,
) -> TerminalLayoutNode {
    TerminalLayoutNode::Split {
        id: id.to_owned(),
        direction: SplitDirection::Horizontal,
        children,
        sizes,
    }
}

/// Checks that splitting a weighted direct child preserves its parent's slot weight.
#[test]
fn splitting_child_rekeys_parent_weight_to_new_split_id() {
    let p1 = pane_id("p1");
    let p2 = pane_id("p2");
    let mut sizes = BTreeMap::new();
    sizes.insert("p1".to_owned(), 0.25);
    sizes.insert("p2".to_owned(), 0.75);
    let layout = split(
        "outer",
        vec![
            TerminalLayoutNode::pane(p1.clone()),
            TerminalLayoutNode::pane(p2),
        ],
        Some(sizes),
    );

    let next = layout
        .split_pane(&p1, pane_id("p3"), SplitDirection::Vertical, "inner", false)
        .expect("split should remain a valid layout");

    let TerminalLayoutNode::Split {
        children,
        sizes: Some(parent_sizes),
        ..
    } = next
    else {
        panic!("root should remain a split");
    };
    assert_eq!(parent_sizes.get("inner"), Some(&0.25));
    assert_eq!(parent_sizes.get("p2"), Some(&0.75));
    assert!(!parent_sizes.contains_key("p1"));
    let TerminalLayoutNode::Split { id, sizes, .. } = &children[0] else {
        panic!("replaced child should be the new split");
    };
    assert_eq!(id, "inner");
    assert!(sizes.is_none());
}

/// Ensures cross-parent swaps keep each spatial slot's weight and rebuild keys for its new pane.
#[test]
fn cross_parent_swap_rekeys_weight_maps_by_layout_slot() {
    let p1 = pane_id("p1");
    let p2 = pane_id("p2");
    let p3 = pane_id("p3");
    let p4 = pane_id("p4");
    let left = split(
        "left",
        vec![
            TerminalLayoutNode::pane(p1.clone()),
            TerminalLayoutNode::pane(p2.clone()),
        ],
        Some(BTreeMap::from([
            ("p1".to_owned(), 0.4),
            ("p2".to_owned(), 0.6),
        ])),
    );
    let right = split(
        "right",
        vec![
            TerminalLayoutNode::pane(p3.clone()),
            TerminalLayoutNode::pane(p4.clone()),
        ],
        Some(BTreeMap::from([
            ("p3".to_owned(), 0.7),
            ("p4".to_owned(), 0.3),
        ])),
    );
    let layout = split(
        "root",
        vec![left, right],
        Some(BTreeMap::from([
            ("left".to_owned(), 0.5),
            ("right".to_owned(), 0.5),
        ])),
    );

    let next = layout
        .swap_panes(&p1, &p3)
        .expect("swap should preserve layout validity");
    next.validate()
        .expect("all rekeyed child-size maps must remain valid");

    let TerminalLayoutNode::Split { children, .. } = next else {
        panic!("root should remain a split")
    };
    let TerminalLayoutNode::Split {
        sizes: Some(left_sizes),
        ..
    } = &children[0]
    else {
        panic!("left split expected")
    };
    let TerminalLayoutNode::Split {
        sizes: Some(right_sizes),
        ..
    } = &children[1]
    else {
        panic!("right split expected")
    };
    assert_eq!(left_sizes.get("p3"), Some(&0.4));
    assert_eq!(left_sizes.get("p2"), Some(&0.6));
    assert_eq!(right_sizes.get("p1"), Some(&0.7));
    assert_eq!(right_sizes.get("p4"), Some(&0.3));
}

/// Rejects repeated split IDs before resize can update multiple branches at once.
#[test]
fn resize_rejects_duplicate_split_ids_in_persisted_tree() {
    let duplicate_a = split(
        "duplicate",
        vec![
            TerminalLayoutNode::pane(pane_id("p1")),
            TerminalLayoutNode::pane(pane_id("p2")),
        ],
        None,
    );
    let duplicate_b = split(
        "duplicate",
        vec![
            TerminalLayoutNode::pane(pane_id("p3")),
            TerminalLayoutNode::pane(pane_id("p4")),
        ],
        None,
    );
    let layout = split("root", vec![duplicate_a, duplicate_b], None);

    let result = layout.resize_split(
        "duplicate",
        BTreeMap::from([("p1".to_owned(), 0.5), ("p2".to_owned(), 0.5)]),
    );

    assert_eq!(
        result,
        Err(WorkspaceLayoutError::DuplicateSplit("duplicate".to_owned()))
    );
}

/// Rejects malformed recursive layouts at the same boundary used for restored v3 data.
#[test]
fn validation_rejects_duplicate_panes_empty_splits_and_stale_size_keys() {
    let duplicate_pane = split(
        "root",
        vec![
            TerminalLayoutNode::pane(pane_id("same")),
            TerminalLayoutNode::pane(pane_id("same")),
        ],
        None,
    );
    assert_eq!(
        duplicate_pane.validate(),
        Err(WorkspaceLayoutError::DuplicatePane(pane_id("same")))
    );

    let empty = split("empty", Vec::new(), None);
    assert!(matches!(
        empty.validate(),
        Err(WorkspaceLayoutError::InvalidStructure(_))
    ));

    let stale_sizes = split(
        "weighted",
        vec![
            TerminalLayoutNode::pane(pane_id("p1")),
            TerminalLayoutNode::pane(pane_id("p2")),
        ],
        Some(BTreeMap::from([
            ("old-p1".to_owned(), 0.5),
            ("p2".to_owned(), 0.5),
        ])),
    );
    assert_eq!(
        stale_sizes.validate(),
        Err(WorkspaceLayoutError::InvalidSplitSizes(
            "weighted".to_owned()
        ))
    );
}

/// Confirms persisted keys and enum values stay compatible with the existing v3 workspace format.
#[test]
fn layout_serializes_with_existing_v3_field_names() {
    let layout = split(
        "main",
        vec![
            TerminalLayoutNode::pane(pane_id("p1")),
            TerminalLayoutNode::pane(pane_id("p2")),
        ],
        Some(BTreeMap::from([
            ("p1".to_owned(), 0.4),
            ("p2".to_owned(), 0.6),
        ])),
    );

    let value = serde_json::to_value(&layout).expect("layout should serialize");
    assert_eq!(value["type"], "split");
    assert_eq!(value["direction"], "horizontal");
    assert_eq!(value["children"][0]["paneId"], "p1");
    assert_eq!(value["sizes"]["p2"], 0.6);
    assert_eq!(
        serde_json::from_value::<TerminalLayoutNode>(value).unwrap(),
        layout
    );
}

/// Checks weight-map migration when removing the last leaf collapses a nested split.
#[test]
fn removing_leaf_rekeys_weight_after_nested_split_collapses() {
    let p1 = pane_id("p1");
    let p2 = pane_id("p2");
    let nested = split(
        "inner",
        vec![
            TerminalLayoutNode::pane(p1.clone()),
            TerminalLayoutNode::pane(p2.clone()),
        ],
        None,
    );
    let root = split(
        "root",
        vec![nested, TerminalLayoutNode::pane(pane_id("p3"))],
        Some(BTreeMap::from([
            ("inner".to_owned(), 0.6),
            ("p3".to_owned(), 0.4),
        ])),
    );

    let next = root
        .remove_pane(&p1)
        .expect("pane exists")
        .expect("other panes remain");
    next.validate().expect("collapsed layout should be valid");
    let TerminalLayoutNode::Split {
        sizes: Some(sizes), ..
    } = next
    else {
        panic!("root should remain a split")
    };
    assert_eq!(sizes.get("p2"), Some(&0.6));
    assert_eq!(sizes.get("p3"), Some(&0.4));
}

/// 平铺追加必须复用同方向父 split 并保留既有比例，新 pane 获得平均值权重。
#[test]
fn appending_sibling_keeps_flat_split_and_average_weight() {
    let p1 = pane_id("p1");
    let p2 = pane_id("p2");
    let root = split(
        "root",
        vec![
            TerminalLayoutNode::pane(p1.clone()),
            TerminalLayoutNode::pane(p2.clone()),
        ],
        Some(BTreeMap::from([
            ("p1".to_owned(), 0.25),
            ("p2".to_owned(), 0.75),
        ])),
    );

    let p3 = pane_id("p3");
    let next = root
        .add_sibling_pane(&p1, p3.clone(), SplitDirection::Horizontal, false)
        .expect("append should succeed")
        .expect("matching parent split exists");
    next.validate().expect("flat sibling layout stays valid");
    let TerminalLayoutNode::Split {
        children, sizes, ..
    } = next
    else {
        panic!("root should remain a split");
    };
    assert_eq!(
        children,
        vec![
            TerminalLayoutNode::pane(p1),
            TerminalLayoutNode::pane(p3.clone()),
            TerminalLayoutNode::pane(p2),
        ]
    );
    let sizes = sizes.expect("explicit weights are preserved");
    assert_eq!(sizes.get("p1"), Some(&0.25));
    assert_eq!(sizes.get("p2"), Some(&0.75));
    assert_eq!(sizes.get(&p3.to_string()), Some(&0.5));
}

/// 父 split 方向不一致时只能由调用方退回普通分屏，避免跨方向拼接改变几何语义。
#[test]
fn appending_sibling_requires_matching_parent_direction() {
    let p1 = pane_id("p1");
    let p2 = pane_id("p2");
    let nested = TerminalLayoutNode::Split {
        id: "inner".to_owned(),
        direction: SplitDirection::Vertical,
        children: vec![
            TerminalLayoutNode::pane(p1.clone()),
            TerminalLayoutNode::pane(p2),
        ],
        sizes: None,
    };
    let root = split(
        "root",
        vec![nested, TerminalLayoutNode::pane(pane_id("p3"))],
        None,
    );

    assert_eq!(
        root.add_sibling_pane(&p1, pane_id("p4"), SplitDirection::Horizontal, false)
            .expect("validation passes"),
        None
    );
    assert!(
        root.add_sibling_pane(&p1, pane_id("p4"), SplitDirection::Vertical, true)
            .expect("validation passes")
            .is_some()
    );
}

/// 根节点只有单个 pane 时没有可复用的父 split。
#[test]
fn appending_sibling_from_single_pane_returns_none() {
    let p1 = pane_id("p1");
    let root = TerminalLayoutNode::pane(p1.clone());
    assert_eq!(
        root.add_sibling_pane(&p1, pane_id("p2"), SplitDirection::Horizontal, false)
            .expect("validation passes"),
        None
    );
}
