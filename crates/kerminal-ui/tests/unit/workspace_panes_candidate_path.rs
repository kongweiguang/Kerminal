//! 外置纯单测候选：可移入 workspace_panes.rs 的测试模块后验证递归 slot 路径。
//!
//! @author kongweiguang

use super::candidate_path_child_index;
use kerminal_domain::{
    identity::PaneId,
    workspace::{SplitDirection, TerminalLayoutNode},
};

/// 每层只选包含 candidate 的直系 slot，并按 root 到目标的顺序累积索引。
/// 这个纯布局检查固定递归路由契约；实际 bounds 与 ResizableState 仍由同帧 Canvas 门禁验证。
#[test]
fn nested_candidate_path_keeps_each_split_slot_index() {
    let source = PaneId::parse("pane-source").expect("valid source pane id");
    let sibling = PaneId::parse("pane-sibling").expect("valid sibling pane id");
    let candidate = PaneId::parse("pane-candidate").expect("valid candidate pane id");
    let layout = TerminalLayoutNode::Split {
        id: "split-root".to_owned(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::Pane { pane_id: source },
            TerminalLayoutNode::Split {
                id: "split-nested".to_owned(),
                direction: SplitDirection::Vertical,
                children: vec![
                    TerminalLayoutNode::Pane { pane_id: sibling },
                    TerminalLayoutNode::Pane {
                        pane_id: candidate.clone(),
                    },
                ],
                sizes: None,
            },
        ],
        sizes: None,
    };

    let mut node = &layout;
    let mut path = Vec::new();
    while let TerminalLayoutNode::Split {
        id,
        direction,
        children,
        ..
    } = node
    {
        let child_index = candidate_path_child_index(children, &candidate)
            .expect("each ancestor has one child containing the candidate");
        path.push((id.clone(), *direction, child_index));
        node = &children[child_index];
    }

    assert_eq!(
        path,
        vec![
            ("split-root".to_owned(), SplitDirection::Horizontal, 1),
            ("split-nested".to_owned(), SplitDirection::Vertical, 1),
        ]
    );
}
