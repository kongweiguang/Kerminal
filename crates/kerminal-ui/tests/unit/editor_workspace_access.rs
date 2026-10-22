//! Workspace 文件访问权限与文件系统权限的合并规则。
//!
//! @author kongweiguang

use super::{
    clean_tab_can_reload_without_confirmation, editor_file_full_path,
    editor_tab_generation_matches, effective_readonly, user_reload_request_is_available,
};
use kerminal_domain::files::editor::EditorPhase;

/// 验证只读策略只能收紧权限，且可编辑 workspace 不会解除 provider 的只读限制。
#[test]
fn workspace_access_never_loosens_file_system_readonly() {
    assert!(effective_readonly(true, false));
    assert!(effective_readonly(false, true));
    assert!(!effective_readonly(false, false));
}

/// Dirty/Conflict/Unknown 仍可进入原确认读回；任一标签的保存都可能仍拥有后续确认，必须拒绝新重载。
///
/// @author kongweiguang
#[test]
fn user_reload_request_preserves_existing_confirmation_and_io_owners() {
    assert!(user_reload_request_is_available(
        false,
        false,
        false,
        [None]
    ));
    assert!(!user_reload_request_is_available(
        true,
        false,
        false,
        [None]
    ));
    assert!(!user_reload_request_is_available(
        false,
        true,
        false,
        [None]
    ));
    assert!(!user_reload_request_is_available(
        false,
        false,
        true,
        [None]
    ));
    assert!(!user_reload_request_is_available(
        false,
        false,
        false,
        [Some(EditorPhase::Saving)],
    ));
}

/// 只有 Clean/Ready 的当前缓冲可直接读回；未知、冲突、加载和关闭意图均保留确认。
///
/// @author kongweiguang
#[test]
fn clean_ready_reload_skips_confirmation_only_when_safe() {
    assert!(clean_tab_can_reload_without_confirmation(
        Some(EditorPhase::Ready),
        false,
        true,
        false,
        false,
    ));
    for (phase, dirty, has_editor, loading, close_pending) in [
        (Some(EditorPhase::Ready), true, true, false, false),
        (Some(EditorPhase::Ready), false, false, false, false),
        (Some(EditorPhase::Conflict), false, true, false, false),
        (Some(EditorPhase::OutcomeUnknown), false, true, false, false),
        (Some(EditorPhase::Failed), false, true, false, false),
        (Some(EditorPhase::Saving), false, true, false, false),
        (Some(EditorPhase::Ready), false, true, true, false),
        (Some(EditorPhase::Ready), false, true, false, true),
        (None, false, false, false, false),
    ] {
        assert!(!clean_tab_can_reload_without_confirmation(
            phase,
            dirty,
            has_editor,
            loading,
            close_pending,
        ));
    }
}

/// A 的保存回执仍可能发起确认，B 即使 Ready 也不能抢占同一根层确认槽；保存收尾后恢复入口。
///
/// @author kongweiguang
#[test]
fn another_tab_save_blocks_reload_confirmation_until_receipt_finishes() {
    assert!(!user_reload_request_is_available(
        false,
        false,
        false,
        [Some(EditorPhase::Saving), Some(EditorPhase::Ready)],
    ));
    assert!(user_reload_request_is_available(
        false,
        false,
        false,
        [Some(EditorPhase::Ready), Some(EditorPhase::Ready)],
    ));
}

/// 右键菜单仅对捕获代次仍存活的同一编辑器标签执行，关闭或重开后失效。
///
/// @author kongweiguang
#[test]
fn editor_tab_actions_require_the_captured_load_generation() {
    assert!(editor_tab_generation_matches(Some(7), 7));
    assert!(!editor_tab_generation_matches(Some(8), 7));
    assert!(!editor_tab_generation_matches(None, 7));
}

/// 复制只取 endpoint 的路径文本，不把远端身份或协议包装写入剪贴板。
///
/// @author kongweiguang
#[test]
fn editor_file_copy_path_uses_endpoint_path_only() {
    let endpoint = kerminal_domain::files::FileEndpoint::Remote {
        host_id: kerminal_domain::identity::HostId::parse("ssh-host-1")
            .expect("valid test host id"),
        path: kerminal_domain::files::RemotePath::parse("/srv/app/notes.txt")
            .expect("valid remote path"),
    };
    assert_eq!(
        editor_file_full_path(&endpoint).as_deref(),
        Some("/srv/app/notes.txt")
    );
}
