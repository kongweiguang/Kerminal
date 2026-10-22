//! 分屏按钮与主机选择必须发出原 pane 的稳定方向意图。
//!
//! @author kongweiguang

use super::{
    SplitDirectionIntent, WorkspacePaneEvent, split_button_icon, split_event,
    split_target_request_event,
};
use gpui_kit::assets::IconName;
use kerminal_domain::{identity::PaneId, workspace::SplitDirection};

/// 类型化方向保留来源 PaneId 与插入位置，不依赖之后的焦点变化。
///
/// @author kongweiguang
#[test]
fn split_directions_keep_pane_identity_and_explicit_order() {
    let pane_id = PaneId::new();
    let intents = [
        SplitDirectionIntent {
            direction: SplitDirection::Horizontal,
            before: false,
        },
        SplitDirectionIntent {
            direction: SplitDirection::Horizontal,
            before: true,
        },
        SplitDirectionIntent {
            direction: SplitDirection::Vertical,
            before: false,
        },
        SplitDirectionIntent {
            direction: SplitDirection::Vertical,
            before: true,
        },
    ];
    let events = intents
        .into_iter()
        .map(|intent| split_event(pane_id.clone(), intent))
        .collect::<Vec<_>>();

    assert_eq!(
        events,
        vec![
            WorkspacePaneEvent::Split {
                pane_id: pane_id.clone(),
                direction: SplitDirection::Horizontal,
                before: false,
            },
            WorkspacePaneEvent::Split {
                pane_id: pane_id.clone(),
                direction: SplitDirection::Horizontal,
                before: true,
            },
            WorkspacePaneEvent::Split {
                pane_id: pane_id.clone(),
                direction: SplitDirection::Vertical,
                before: false,
            },
            WorkspacePaneEvent::Split {
                pane_id,
                direction: SplitDirection::Vertical,
                before: true,
            },
        ]
    );
}

/// 目标选择入口只携带触发 pane 和方向意图，确认前不生成资源或切换 pane。
///
/// @author kongweiguang
#[test]
fn target_picker_request_keeps_the_original_pane_and_direction() {
    let pane_id = PaneId::parse("pane-origin").expect("valid pane id");
    let event = split_target_request_event(
        pane_id.clone(),
        SplitDirectionIntent {
            direction: SplitDirection::Horizontal,
            before: false,
        },
    );

    assert_eq!(
        event,
        WorkspacePaneEvent::ChooseSplitTarget {
            pane_id,
            direction: SplitDirection::Horizontal,
            before: false,
        }
    );
}

/// 列/行图标固定表示分屏轴，布局状态变化不会把按钮图形翻转成抽屉箭头。
///
/// @author kongweiguang
#[test]
fn split_icons_remain_fixed_for_each_axis() {
    assert_eq!(
        split_button_icon(SplitDirection::Horizontal),
        IconName::Columns2
    );
    assert_eq!(split_button_icon(SplitDirection::Vertical), IconName::Rows2);
}
