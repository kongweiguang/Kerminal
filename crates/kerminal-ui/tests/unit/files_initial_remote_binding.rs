//! 当前终端单栏文件目标同步与浏览历史边界回归。
//!
//! @author kongweiguang

use std::path::PathBuf;

use super::{
    CompactTargetUpdateAction, MAX_NAVIGATION_HISTORY, NavigationHistory,
    can_transfer_to_other_side, compact_target_update_action, should_apply_shared_host_local_start,
};
use kerminal_domain::{files::FileEndpoint, identity::HostId};

/// 为状态测试构造经过同一领域校验的远端目录 endpoint。
///
/// @author kongweiguang
fn remote_endpoint(host_id: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::remote(host_id.clone(), path).expect("valid remote directory")
}

/// SSH 跟随保持显式 opt-in；本机切换目录或 Tab 始终保留用户浏览路径。
///
/// @author kongweiguang
#[test]
fn compact_file_target_updates_preserve_or_follow_only_the_intended_path() {
    let host_a = HostId::new();
    let host_b = HostId::new();
    let terminal_a = remote_endpoint(&host_a, "/home/kong");
    let terminal_a_cwd = remote_endpoint(&host_a, "/srv/app");
    let browser_a = remote_endpoint(&host_a, "/srv/app/config");
    let terminal_b = remote_endpoint(&host_b, "/home/deploy");
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let local_browser = FileEndpoint::Local {
        path: PathBuf::from("C:\\work\\docs"),
    };
    let local_next = FileEndpoint::Local {
        path: PathBuf::from("C:\\Users\\24052"),
    };

    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a), false),
        CompactTargetUpdateAction::Unchanged
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a), true),
        CompactTargetUpdateAction::Unchanged
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a_cwd), false,),
        CompactTargetUpdateAction::PreservePath
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a_cwd), true,),
        CompactTargetUpdateAction::FollowDirectory
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_b), false,),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(None, &browser_a, Some(&terminal_a), false),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, None, true),
        CompactTargetUpdateAction::Unavailable
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&local), true),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(None, &local, Some(&local), false),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(Some(&local), &local_browser, Some(&local_next), false),
        CompactTargetUpdateAction::PreservePath
    );
    assert_eq!(
        compact_target_update_action(Some(&local), &local_browser, Some(&local_next), true),
        CompactTargetUpdateAction::PreservePath
    );
    assert_eq!(
        compact_target_update_action(Some(&local), &local_browser, Some(&terminal_a), true),
        CompactTargetUpdateAction::BindTarget
    );
}

/// 回退/前进保留完整 endpoint，分支导航截断旧前进项，历史长度固定有界。
///
/// @author kongweiguang
#[test]
fn navigation_history_branches_and_remains_bounded() {
    let host_id = HostId::new();
    let a = remote_endpoint(&host_id, "/srv/a");
    let b = remote_endpoint(&host_id, "/srv/b");
    let c = remote_endpoint(&host_id, "/srv/c");
    let d = remote_endpoint(&host_id, "/srv/d");
    let mut history = NavigationHistory::new(a.clone());

    history.push(b.clone());
    history.push(c.clone());
    assert_eq!(history.back(), Some(b));
    assert_eq!(history.back(), Some(a));
    history.push(d.clone());
    assert_eq!(history.forward(), None);
    assert_eq!(history.entries.last(), Some(&d));
    assert!(!history.can_go_forward());

    for index in 0..MAX_NAVIGATION_HISTORY + 5 {
        history.push(remote_endpoint(&host_id, &format!("/srv/{index}")));
    }
    assert_eq!(history.entries.len(), MAX_NAVIGATION_HISTORY);
    assert!(history.can_go_back());
}

/// 保存主机的本地起始目录只覆盖共享工作台的首个未浏览本机目标。
///
/// @author kongweiguang
#[test]
fn shared_host_local_start_preserves_manual_or_locked_left_targets() {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\Users\\24052"),
    };
    let remote = remote_endpoint(&HostId::new(), "/home/deploy");

    assert!(should_apply_shared_host_local_start(
        true, true, &local, 1, false
    ));
    assert!(!should_apply_shared_host_local_start(
        false, true, &local, 1, false
    ));
    assert!(!should_apply_shared_host_local_start(
        true, false, &local, 1, false
    ));
    assert!(!should_apply_shared_host_local_start(
        true, true, &local, 2, false
    ));
    assert!(!should_apply_shared_host_local_start(
        true, true, &local, 1, true
    ));
    assert!(!should_apply_shared_host_local_start(
        true, true, &remote, 1, false
    ));
}

/// 共享工作台不能把未选择主机时的本机占位当作右侧复制目标。
///
/// @author kongweiguang
#[test]
fn shared_workbench_copy_requires_explicit_right_target() {
    assert!(!can_transfer_to_other_side(true, true, 0));
    assert!(can_transfer_to_other_side(true, false, 0));
    assert!(can_transfer_to_other_side(false, true, 0));
    assert!(can_transfer_to_other_side(true, true, 1));
}
