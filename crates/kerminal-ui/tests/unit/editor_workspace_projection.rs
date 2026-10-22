use super::{EditorTabProjection, EditorWorkspaceProjection, workspace_projection_content_changed};
use kerminal_domain::files::FileEndpoint;
use std::path::PathBuf;

/// 用稳定本机路径构造身份，避免测试快照依赖真实文件系统状态。
///
/// @author kongweiguang
fn local_endpoint(path: &str) -> FileEndpoint {
    FileEndpoint::Local {
        path: PathBuf::from(path),
    }
}

/// 只构造 Files 可见的标签字段，使回归聚焦投影变化而不复制编辑器状态。
///
/// @author kongweiguang
fn tab(endpoint: FileEndpoint, active: bool) -> EditorTabProjection {
    EditorTabProjection {
        title: "file".to_owned(),
        dirty: false,
        readonly: false,
        loading: false,
        active,
        endpoint,
    }
}

/// 版本号单独变化不应通知 FilesPane，也避免编辑器普通重绘产生空广播。
///
/// @author kongweiguang
#[test]
fn unchanged_projection_content_does_not_trigger_an_event() {
    let endpoint = local_endpoint("C:/workspace/a.txt");
    let previous = EditorWorkspaceProjection {
        revision: 3,
        tabs: vec![tab(endpoint.clone(), true)],
        active_endpoint: Some(endpoint.clone()),
    };
    let current = EditorWorkspaceProjection {
        revision: 4,
        tabs: vec![tab(endpoint.clone(), true)],
        active_endpoint: Some(endpoint),
    };

    assert!(!workspace_projection_content_changed(&previous, &current));
}

/// 文件关闭和活动标签切换都改变 FilesPane 可见列表，需要推送最新快照。
///
/// @author kongweiguang
#[test]
fn close_and_active_endpoint_changes_trigger_projection_updates() {
    let first = local_endpoint("C:/workspace/a.txt");
    let second = local_endpoint("C:/workspace/b.txt");
    let previous = EditorWorkspaceProjection {
        revision: 8,
        tabs: vec![tab(first.clone(), true), tab(second.clone(), false)],
        active_endpoint: Some(first.clone()),
    };
    let after_close = EditorWorkspaceProjection {
        revision: 9,
        tabs: vec![tab(first.clone(), true)],
        active_endpoint: Some(first.clone()),
    };
    let after_activate = EditorWorkspaceProjection {
        revision: 9,
        tabs: vec![tab(first.clone(), false), tab(second.clone(), true)],
        active_endpoint: Some(second),
    };

    assert!(workspace_projection_content_changed(
        &previous,
        &after_close
    ));
    assert!(workspace_projection_content_changed(
        &previous,
        &after_activate
    ));
}
