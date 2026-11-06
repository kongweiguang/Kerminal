//! 左键同目标分屏的实时目录传递回归；复用 Root 内存服务，不访问用户配置或真实主机。
//!
//! @author kongweiguang

use super::*;

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_domain::workspace::SplitDirection;
use kerminal_terminal::{TerminalEngine, TerminalLimits};

use crate::{
    features::{
        workspace_controller::WorkspaceResourceOperation, workspace_panes::WorkspacePaneEvent,
    },
    root::{
        workspace_pane_for_terminal_target, workspace_render::WorkspaceRenderEvent,
        workspace_runtime::WorkspaceRuntime,
    },
};

/// 从 OSC7 到 Root 分屏事件再到真实打开目标，验证中文、空格与引号路径不退回旧启动目录。
/// 点击非聚焦 pane 也只能复制该来源；失败重试必须冻结点击时目录，不改写来源的持久参数。
///
/// @author kongweiguang
fn assert_split_directory(
    app: &mut TestAppContext,
    target: SessionTarget,
    directory: &str,
    directory_uri: &str,
) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let (mut source, _) = workspace_pane_for_terminal_target(Some(&target), "source", None);
            source.current_cwd = Some("/stale-saved-directory".to_owned());
            let tab_id = TabId::new();
            let source_id = source.id.clone();
            let (mut focused, _) = workspace_pane_for_terminal_target(Some(&target), "focused", None);
            focused.current_cwd = Some("/wrong-focused-directory".to_owned());
            let focused_id = focused.id.clone();
            let mut controller = WorkspaceController::from_missing().unwrap();
            let mut workspace = controller.snapshot().clone();
            let mut tab = workspace_terminal_tab(&tab_id, &source);
            if let WorkspaceSessionTab::Terminal { layout, .. } = &mut tab {
                *layout = kerminal_domain::workspace::TerminalLayoutNode::Split {
                    id: "split-directory-source-layout".to_owned(),
                    direction: SplitDirection::Horizontal,
                    children: vec![
                        kerminal_domain::workspace::TerminalLayoutNode::pane(source_id.clone()),
                        kerminal_domain::workspace::TerminalLayoutNode::pane(focused_id.clone()),
                    ],
                    sizes: None,
                };
            }
            workspace.terminal_tabs = vec![tab];
            workspace.terminal_panes = vec![source.clone(), focused.clone()];
            workspace.active_tab_id = Some(tab_id.clone());
            workspace.focused_pane_id = Some(focused_id);
            workspace.selected_machine_id = Some(source.machine_id.clone());
            controller
                .synchronize_snapshot(workspace)
                .unwrap();
            root.workspace_runtime = WorkspaceRuntime::from_snapshot(controller.snapshot()).unwrap();
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            let session_id = SessionId::new();
            let size = TerminalSize::default();
            root.workspace_runtime
                .install_session(&source_id, session_id.clone(), target.clone(), size)
                .unwrap();
            let mut engine = TerminalEngine::new(session_id.clone(), size, TerminalLimits::default())
                .unwrap();
            engine.process_output(format!("\x1b]7;{directory_uri}\x07").as_bytes());
            let snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
            assert_eq!(snapshot.current_working_directory.as_deref(), Some(directory));
            root.workspace_runtime
                .accept_initial_snapshot(&source_id, &session_id, snapshot)
                .unwrap();
            // 换代时旧帧可读，但旧目录不能回退进分屏候选；新首帧才恢复操作。
            root.workspace_runtime
                .invalidate_session_snapshot(&source_id, &session_id)
                .unwrap();
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Split {
                        pane_id: source_id.clone(),
                        direction: SplitDirection::Vertical,
                        before: false,
                    },
                },
                window,
                cx,
            );
            assert!(root.workspace_controller.as_ref().unwrap().pending_resource_operation().is_none());
            let (_, token, request) = root.workspace_runtime
                .begin_snapshot(&source_id).unwrap().unwrap();
            assert!(root.workspace_runtime
                .accept_snapshot(&source_id, &session_id, token, engine.render_snapshot(request))
                .unwrap());
            root.handle_workspace_render_event(
                WorkspaceRenderEvent::Pane {
                    tab_id: tab_id.clone(),
                    event: WorkspacePaneEvent::Split {
                        pane_id: source_id,
                        direction: SplitDirection::Vertical,
                        before: false,
                    },
                },
                window,
                cx,
            );
            let controller = root.workspace_controller.as_mut().unwrap();
            let operation = controller.pending_resource_operation().unwrap().clone();
            let WorkspaceResourceOperation::CreatePane { pane, .. } = &operation else {
                panic!("split must use the existing resource lifecycle");
            };
            assert_eq!(pane.current_cwd.as_deref(), Some(directory));
            assert_eq!(pane.target, source.target);
            assert_eq!(controller.snapshot().terminal_panes, vec![source, focused]);
            controller
                .complete_resource_operation(
                    operation.operation_id(),
                    Err(crate::features::workspace_controller::WorkspaceRestoreFailure::ResourceFailure),
                )
                .unwrap();
            assert_eq!(controller.retry_pending_resource_operation(), Some(operation.clone()));
            root.restore_workspace_pane(tab_id, pane.as_ref().clone(), cx);
        })
    });
    settle(&runtime, visual);
    let creates = sessions.creates.lock().unwrap();
    assert_eq!(creates.len(), 1);
    let working_directory = match &creates[0].target {
        SessionTarget::Local(target) => target.working_directory.clone(),
        SessionTarget::Ssh(target) => target.working_directory.clone(),
        _ => panic!("directory inheritance must preserve the terminal protocol"),
    };
    assert_eq!(working_directory, Some(PathBuf::from(directory)));
}

/// 标签 DTO 与生产启动目标使用同一个 pane 身份，避免测试绕过布局归属校验。
///
/// @author kongweiguang
fn workspace_terminal_tab(tab_id: &TabId, pane: &WorkspaceSessionPane) -> WorkspaceSessionTab {
    WorkspaceSessionTab::Terminal {
        id: tab_id.clone(),
        tab_group_id: None,
        title: "Terminal".to_owned(),
        machine_id: pane.machine_id.clone(),
        layout: kerminal_domain::workspace::TerminalLayoutNode::pane(pane.id.clone()),
    }
}

/// 本机 PowerShell 的 file URI 必须解码为盘符路径，并优先于启动目录和旧 current_cwd。
///
/// @author kongweiguang
#[gpui_kit::test]
fn local_left_split_inherits_live_directory(app: &mut TestAppContext) {
    assert_split_directory(
        app,
        SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("pwsh.exe"),
            args: Vec::new(),
            working_directory: Some(PathBuf::from("C:/startup")),
            environment: BTreeMap::new(),
        }),
        "C:/项目 dir/it's here",
        "file:///C:/%E9%A1%B9%E7%9B%AE%20dir/it%27s%20here",
    );
}

/// SSH 左键只复制原 host 的实时目录，不把 URI authority 当作另一台主机。
///
/// @author kongweiguang
#[gpui_kit::test]
fn ssh_left_split_inherits_live_directory(app: &mut TestAppContext) {
    assert_split_directory(
        app,
        SessionTarget::Ssh(SshSessionTarget {
            host_id: HostId::new(),
            working_directory: Some(PathBuf::from("/startup")),
            command: None,
        }),
        "/srv/项目 dir/it's here",
        "file://same-host/srv/%E9%A1%B9%E7%9B%AE%20dir/it%27s%20here",
    );
}
