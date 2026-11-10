//! 文件宿主的只读上下文投影，不依赖窗口、终端或真实用户目录。
//!
//! @author kongweiguang

use kerminal_application::workspace::{
    FileWorkspaceItem, FileWorkspaceState, WorkspaceFileAccess, WorkspaceFileSource,
    WorkspaceSessionTab, WorkspaceTargetRef,
};
use kerminal_domain::identity::{HostId, TabId};

use super::{active_tab_location, tab_kind, tab_machine_id, tab_target_summary};
use crate::features::context::{
    ContextConnectionKind, ContextLocationConfidence, ContextLocationSource,
};

/// 同一 owner 内保留目录与文档，确保位置投影不会误用其它终端或第一个文档。
/// @author kongweiguang
fn workspace() -> WorkspaceSessionTab {
    WorkspaceSessionTab::FileWorkspace {
        id: TabId::parse("context-file-owner").expect("valid owner ID"),
        tab_group_id: None,
        title: "Files".into(),
        machine_id: "file-machine".into(),
        target: WorkspaceTargetRef::Ssh {
            host_id: HostId::parse("context-file-host").expect("valid host ID"),
        },
        state: FileWorkspaceState {
            items: vec![
                FileWorkspaceItem::Document {
                    id: TabId::parse("context-document").expect("valid document ID"),
                    title: "config.toml".into(),
                    path: "/srv/config.toml".into(),
                    root_path: Some("/srv".into()),
                    access: WorkspaceFileAccess::Readonly,
                    source: WorkspaceFileSource::Sftp,
                },
                FileWorkspaceItem::Directory {
                    id: TabId::parse("context-directory").expect("valid directory ID"),
                    path: "/srv/project".into(),
                },
            ],
            active_item_id: Some(TabId::parse("context-document").expect("valid document ID")),
            source_tab_ids: Vec::new(),
        },
    }
}

/// 文档路径来自显式活动项；取消选择后只允许首目录兜底，不激活后台文档。
/// @author kongweiguang
#[test]
fn file_workspace_location_uses_active_item_then_first_directory() {
    let mut tab = workspace();
    let location = active_tab_location(Some(&tab));
    assert_eq!(location.path.as_deref(), Some("/srv/config.toml"));
    assert_eq!(location.source, ContextLocationSource::WorkspaceFile);
    assert_eq!(location.confidence, ContextLocationConfidence::High);

    let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut tab else {
        panic!("fixture is a file workspace");
    };
    state.active_item_id = None;
    assert_eq!(
        active_tab_location(Some(&tab)).path.as_deref(),
        Some("/srv/project"),
    );

    let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut tab else {
        panic!("fixture is a file workspace");
    };
    state
        .items
        .retain(|item| matches!(item, FileWorkspaceItem::Document { .. }));
    let location = active_tab_location(Some(&tab));
    assert_eq!(location.path, None);
    assert_eq!(location.source, ContextLocationSource::Unavailable);
}

/// 宿主分类与来源不受内部活动项影响，也不产生终端 target 或 pane。
/// @author kongweiguang
#[test]
fn file_workspace_context_retains_owner_machine_and_explicit_host() {
    let tab = workspace();
    assert_eq!(tab_kind(&tab), ContextConnectionKind::FileWorkspace);
    assert_eq!(tab_machine_id(&tab), Some("file-machine"));
    let target = tab_target_summary(&tab).expect("explicit SSH file host");
    assert_eq!(target.kind, ContextConnectionKind::Ssh);
    assert_eq!(
        target.host_id,
        Some(HostId::parse("context-file-host").expect("valid host ID")),
    );
    assert!(tab.terminal_layout().is_none());
}
