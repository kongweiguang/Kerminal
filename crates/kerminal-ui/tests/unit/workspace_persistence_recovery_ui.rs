//! 工作区持久化恢复浮层的生产控件交互与隔离内存仓储。
//!
//! @author kongweiguang

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use gpui_kit::{
    ElementId, Entity, Role, TestAppContext, VisualTestContext, component::WindowExt as _, px, size,
};
use kerminal_application::{
    AppError, AppResult,
    files::{FileAccessProvider, FileBookmarkRepository, FileService, editor::FileEditorService},
    workspace::{
        WorkspaceSessionDocument, WorkspaceSessionExpectedRevision, WorkspaceSessionLoad,
        WorkspaceSessionRepository, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
        WorkspaceSessionService, WorkspaceSessionSnapshot, WorkspaceSessionTab,
    },
};
use kerminal_domain::{
    files::{
        BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint,
        FilePreview, FileRevision, LineEnding, TextDocument, WriteTextRequest, WriteTextResult,
    },
    identity::HostId,
};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use crate::{
    features::workspace_controller::{WorkspaceController, WorkspacePersistenceState},
    root::{RootView, workspace_files::WorkspaceFileRuntime},
};

/// 仅一个不可连接的内存端点可被打开，避免测试接触本机文件或真实 SSH。
fn endpoint() -> FileEndpoint {
    FileEndpoint::remote(HostId::parse("fixture-host").unwrap(), "/fixture/dirty.txt").unwrap()
}

struct MemoryFile;

#[async_trait]
impl FileAccessProvider for MemoryFile {
    /// 限定唯一虚拟文件，防止错误目标被当作测试输入。
    fn supports(&self, candidate: &FileEndpoint) -> bool {
        candidate == &endpoint()
    }

    /// 目录枚举不属于此回归，意外调用应明确失败。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        Err(AppError::unsupported("memory fixture has no directories"))
    }

    /// 文件正文只经编辑器读取，避免预览入口引入第二份状态。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("memory fixture has no previews"))
    }

    /// 返回可编辑的内存正文，让脏稿由生产编辑器和用户输入产生。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        endpoint: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Ok(TextDocument {
            endpoint,
            content: "original\n".into(),
            line_ending: LineEnding::Lf,
            revision: FileRevision {
                size: 9,
                modified: None,
                permissions: None,
                content_digest: Some("fixture-original".into()),
            },
            readonly: false,
            truncated: false,
        })
    }

    /// 恢复工作区布局不应保存文件正文。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("memory fixture cannot save files"))
    }

    /// 不允许测试顺带创建真实目录。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture cannot mutate files"))
    }

    /// 不允许测试顺带更改文件身份。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture cannot mutate files"))
    }

    /// 不允许测试顺带修改文件权限。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture cannot mutate files"))
    }

    /// 布局恢复不能删除底层文件。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture cannot mutate files"))
    }
}

struct NoBookmarks;

#[async_trait]
impl FileBookmarkRepository for NoBookmarks {
    /// 不读取用户书签，保证 fixture 与实际配置根隔离。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 恢复布局不应写入书签。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported(
            "memory fixture cannot save bookmarks",
        ))
    }
}

#[derive(Clone, Copy)]
enum ReadResult {
    Fail,
    Current,
}

#[derive(Clone, Copy)]
enum SaveResult {
    Cancel,
    Fail,
    Commit,
}

struct MemoryRepositoryState {
    current: WorkspaceSessionLoad,
    reads: VecDeque<ReadResult>,
    saves: VecDeque<SaveResult>,
    read_count: usize,
    attempts: Vec<(WorkspaceSessionExpectedRevision, WorkspaceSessionDocument)>,
}

struct MemoryWorkspaceRepository(Mutex<MemoryRepositoryState>);

#[async_trait]
impl WorkspaceSessionRepository for MemoryWorkspaceRepository {
    /// 受控失败与当前内存值分开，确保失败回读不会伪造未提交结论。
    async fn load(&self, _: CancellationToken) -> AppResult<WorkspaceSessionLoad> {
        let mut state = self.0.lock().unwrap();
        state.read_count += 1;
        match state.reads.pop_front() {
            Some(ReadResult::Current) => Ok(state.current.clone()),
            Some(ReadResult::Fail) => Err(AppError::infrastructure("fixture read failed")),
            None => Err(AppError::unsupported("unexpected workspace read")),
        }
    }

    /// 内存 CAS 校验当前 revision，并分别记录取消、失败和真正提交。
    async fn save(
        &self,
        _: CancellationToken,
        document: &WorkspaceSessionDocument,
        expected: WorkspaceSessionExpectedRevision,
    ) -> AppResult<WorkspaceSessionRevision> {
        let mut state = self.0.lock().unwrap();
        state.attempts.push((expected.clone(), document.clone()));
        let matches_revision = match (&expected, &state.current) {
            (WorkspaceSessionExpectedRevision::Missing, WorkspaceSessionLoad::Missing) => true,
            (
                WorkspaceSessionExpectedRevision::Revision(expected),
                WorkspaceSessionLoad::Loaded { revision, .. },
            ) => expected == revision,
            _ => false,
        };
        if !matches_revision {
            return Err(AppError::conflict("fixture revision changed"));
        }
        match state.saves.pop_front() {
            Some(SaveResult::Cancel) => Err(AppError::cancelled("fixture save cancelled")),
            Some(SaveResult::Fail) => Err(AppError::infrastructure("fixture save failed")),
            Some(SaveResult::Commit) => {
                let revision = WorkspaceSessionRevision::new("fixture-committed");
                state.current = WorkspaceSessionLoad::Loaded {
                    document: document.clone(),
                    revision: revision.clone(),
                };
                Ok(revision)
            }
            None => Err(AppError::unsupported("unexpected workspace save")),
        }
    }
}

/// 推进两个隔离执行器，避免把异步请求已投递误认为真实回执已处理。
fn settle(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..32 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
}

/// 从真实 Root 打开内存文件宿主，先建立可编辑正文再注入持久化故障。
fn fixture(app: &mut TestAppContext) -> (Runtime, Entity<RootView>, &mut VisualTestContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |mut services| {
            let files = FileService::new(vec![Arc::new(MemoryFile)], Arc::new(NoBookmarks))
                .expect("isolated file service");
            services.file_editor = Some(FileEditorService::new(files));
            services
        });
    settle(&runtime, visual);
    visual.simulate_resize(size(px(1200.), px(800.)));
    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let controller = WorkspaceController::from_missing().unwrap();
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            assert!(root.open_file_in_host_workspace(endpoint(), None, window, cx));
        });
    });
    settle(&runtime, visual);
    let editor = root.read_with(visual, |root, app| {
        let editor = root
            .file_editor_workspace
            .clone()
            .expect("production editor");
        let projection = editor.read(app).workspace_projection();
        assert!(
            projection
                .tabs
                .iter()
                .any(|tab| tab.endpoint == endpoint() && !tab.loading)
        );
        editor
    });
    visual.update(|window, app| {
        assert!(editor.update(app, |editor, cx| editor.focus_active_editor(window, cx)));
        window.draw(app).clear(app);
    });
    visual.simulate_input("unsaved fixture draft");
    settle(&runtime, visual);
    assert!(editor.read_with(visual, |editor, _| editor.has_unsaved_changes()));
    assert!(draft_text(visual).contains("unsaved fixture draft"));
    (runtime, root, visual)
}

/// 读取真实多行编辑器的无障碍值，比较完整正文而不只看 dirty 标志。
fn draft_text(visual: &mut VisualTestContext) -> String {
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let matches: Vec<_> = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| {
                node.visible()
                    && node.role() == Some(Role::MultilineTextInput)
                    && node
                        .label()
                        .is_some_and(|label| label.ends_with("dirty.txt"))
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "the production editor must be uniquely visible"
        );
        matches[0]
            .value()
            .expect("editor accessibility value")
            .to_owned()
    })
}

/// 对已绘制的生产按钮派发鼠标事件；缺失、歧义或不可点击均不能算作交互覆盖。
fn click(visual: &mut VisualTestContext, id: &str, labels: Option<&[&str]>) {
    let id: ElementId = id.to_owned().into();
    let position = visual.update(|window, app| {
        window.draw(app).clear(app);
        let matches: Vec<_> = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| {
                node.visible()
                    && node.role() == Some(Role::Button)
                    && node.path().contains(&id)
                    && labels.is_none_or(|labels| {
                        node.label().is_some_and(|label| labels.contains(&label))
                    })
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected one visible button {id:?}: {matches:?}"
        );
        assert!(
            matches[0].supports_click(),
            "button {id:?} must accept clicks"
        );
        matches[0].bounds().center()
    });
    visual.simulate_click(position, Default::default());
    visual.run_until_parked();
}

/// 只按真实控件树判断动作是否可见，不能把内部状态机映射冒充界面断言。
fn button_visible(visual: &mut VisualTestContext, id: &str) -> bool {
    let id: ElementId = id.to_owned().into();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        gpui_kit::base::test_support::snapshots(window)
            .iter()
            .any(|node| {
                node.visible() && node.role() == Some(Role::Button) && node.path().contains(&id)
            })
    })
}

/// 每个取消或失败都同时比较权威工作区、原编辑器实体和实际正文。
fn assert_retained(
    root: &Entity<RootView>,
    visual: &mut VisualTestContext,
    expected: &WorkspaceSessionSnapshot,
    editor_id: gpui_kit::EntityId,
    draft: &str,
) {
    root.read_with(visual, |root, app| {
        assert_eq!(
            root.workspace_controller.as_ref().unwrap().snapshot(),
            expected
        );
        let editor = root
            .file_editor_workspace
            .as_ref()
            .expect("original editor");
        assert_eq!(editor.entity_id(), editor_id);
        let projection = editor.read(app).workspace_projection();
        assert!(
            projection
                .tabs
                .iter()
                .any(|tab| tab.endpoint == endpoint() && tab.dirty && !tab.loading)
        );
    });
    assert_eq!(draft_text(visual), draft);
}

/// 在真实文件宿主存在后安装待恢复回执，避免测试初始化绕过生产控制器。
fn install_recovery(
    root: &Entity<RootView>,
    visual: &mut VisualTestContext,
    repository: Arc<MemoryWorkspaceRepository>,
    outcome: WorkspaceSessionSaveOutcome,
) -> (WorkspaceSessionSnapshot, gpui_kit::EntityId, String) {
    let draft = draft_text(visual);
    let (snapshot, editor_id) = visual.update(|_, app| {
        root.update(app, |root, cx| {
            let controller = root.workspace_controller.as_mut().unwrap();
            let request = controller
                .begin_save()
                .unwrap()
                .expect("file workspace is dirty");
            assert!(matches!(
                request.source_snapshot.terminal_tabs.as_slice(),
                [WorkspaceSessionTab::FileWorkspace { .. }]
            ));
            controller
                .finish_save(&request.request_id, outcome)
                .unwrap();
            let snapshot = controller.snapshot().clone();
            let editor_id = root.file_editor_workspace.as_ref().unwrap().entity_id();
            root.services.workspace_sessions = Some(WorkspaceSessionService::new(repository));
            root.workspace_persistence_error = None;
            cx.notify();
            (snapshot, editor_id)
        })
    });
    (snapshot, editor_id, draft)
}

/// 读回失败和取消保存后必须保留完整文件草稿，只有再次点击才能提交 CAS。
#[gpui_kit::test]
fn unknown_readback_and_explicit_retry_use_production_buttons(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let repository = Arc::new(MemoryWorkspaceRepository(Mutex::new(
        MemoryRepositoryState {
            current: WorkspaceSessionLoad::Missing,
            reads: VecDeque::from([ReadResult::Fail, ReadResult::Current]),
            saves: VecDeque::from([SaveResult::Cancel, SaveResult::Commit]),
            read_count: 0,
            attempts: Vec::new(),
        },
    )));
    let (snapshot, editor_id, draft) = install_recovery(
        &root,
        visual,
        repository.clone(),
        WorkspaceSessionSaveOutcome::OutcomeUnknown,
    );
    settle(&runtime, visual);
    assert!(button_visible(visual, "workspace-persistence-readback"));
    assert!(!button_visible(
        visual,
        "workspace-persistence-retry-uncommitted"
    ));
    assert!(repository.0.lock().unwrap().attempts.is_empty());

    click(visual, "workspace-persistence-readback", None);
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::OutcomeUnknown { .. }
        ));
        assert!(root.workspace_persistence_error.is_some());
    });
    assert_eq!(repository.0.lock().unwrap().read_count, 1);
    assert!(repository.0.lock().unwrap().attempts.is_empty());
    assert_retained(&root, visual, &snapshot, editor_id, &draft);

    click(visual, "workspace-persistence-readback", None);
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
        ))
    });
    assert!(button_visible(
        visual,
        "workspace-persistence-retry-uncommitted"
    ));
    assert!(!button_visible(visual, "workspace-persistence-readback"));
    assert_eq!(repository.0.lock().unwrap().read_count, 2);
    assert!(
        repository.0.lock().unwrap().attempts.is_empty(),
        "readback must not write automatically"
    );
    assert_retained(&root, visual, &snapshot, editor_id, &draft);

    click(visual, "workspace-persistence-retry-uncommitted", None);
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::Failed
        ));
        assert!(root.workspace_persistence_error.is_some());
    });
    assert!(button_visible(visual, "workspace-persistence-retry-failed"));
    {
        let state = repository.0.lock().unwrap();
        assert_eq!(state.attempts.len(), 1);
        assert_eq!(
            state.attempts[0].0,
            WorkspaceSessionExpectedRevision::Missing
        );
        assert_eq!(state.attempts[0].1.workspace_snapshot().unwrap(), snapshot);
        assert_eq!(state.current, WorkspaceSessionLoad::Missing);
    }
    assert_retained(&root, visual, &snapshot, editor_id, &draft);

    click(visual, "workspace-persistence-retry-failed", None);
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::Clean
        ))
    });
    let state = repository.0.lock().unwrap();
    assert_eq!(state.attempts.len(), 2);
    assert_eq!(
        state.attempts[1].0,
        WorkspaceSessionExpectedRevision::Missing
    );
    let WorkspaceSessionLoad::Loaded { document, .. } = &state.current else {
        panic!("explicit retry must commit");
    };
    assert_eq!(document.workspace_snapshot().unwrap(), snapshot);
    drop(state);
    assert_retained(&root, visual, &snapshot, editor_id, &draft);
}

/// 冲突覆盖必须经真实二次确认；取消和失败均不得替换布局或编辑正文。
#[gpui_kit::test]
fn conflict_overwrite_confirmation_preserves_draft_on_cancel_and_failure(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let mut external = WorkspaceSessionDocument::empty_v3();
    let mut external_snapshot = external.workspace_snapshot().unwrap();
    external_snapshot.selected_machine_id = Some("external".into());
    external.update_known_fields(&external_snapshot).unwrap();
    let latest = WorkspaceSessionLoad::Loaded {
        document: external,
        revision: WorkspaceSessionRevision::new("external-revision"),
    };
    let repository = Arc::new(MemoryWorkspaceRepository(Mutex::new(
        MemoryRepositoryState {
            current: latest.clone(),
            reads: VecDeque::new(),
            saves: VecDeque::from([SaveResult::Fail, SaveResult::Commit]),
            read_count: 0,
            attempts: Vec::new(),
        },
    )));
    let (snapshot, editor_id, draft) = install_recovery(
        &root,
        visual,
        repository.clone(),
        WorkspaceSessionSaveOutcome::Conflict {
            latest: latest.clone(),
        },
    );
    settle(&runtime, visual);
    click(visual, "workspace-persistence-overwrite", None);
    visual.update(|window, app| assert!(window.has_active_dialog(app)));
    assert!(
        repository.0.lock().unwrap().attempts.is_empty(),
        "opening confirmation cannot write"
    );
    click(visual, "cancel", Some(&["取消", "Cancel"]));
    settle(&runtime, visual);
    visual.update(|window, app| assert!(!window.has_active_dialog(app)));
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::Conflict { .. }
        ))
    });
    assert!(repository.0.lock().unwrap().attempts.is_empty());
    assert_retained(&root, visual, &snapshot, editor_id, &draft);

    click(visual, "workspace-persistence-overwrite", None);
    visual.update(|window, app| assert!(window.has_active_dialog(app)));
    assert!(
        repository.0.lock().unwrap().attempts.is_empty(),
        "confirmation still requires an explicit choice"
    );
    click(visual, "ok", Some(&["覆盖工作区", "Overwrite workspace"]));
    settle(&runtime, visual);
    visual.update(|window, app| assert!(!window.has_active_dialog(app)));
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::Failed
        ));
        assert!(root.workspace_persistence_error.is_some());
    });
    {
        let state = repository.0.lock().unwrap();
        assert_eq!(state.read_count, 0);
        assert_eq!(state.attempts.len(), 1);
        assert_eq!(
            state.attempts[0].0,
            WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(
                "external-revision"
            ))
        );
        assert_eq!(state.attempts[0].1.workspace_snapshot().unwrap(), snapshot);
        assert_eq!(state.current, latest);
    }
    assert_retained(&root, visual, &snapshot, editor_id, &draft);

    click(visual, "workspace-persistence-retry-failed", None);
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .persistence_state(),
            WorkspacePersistenceState::Clean
        ))
    });
    let state = repository.0.lock().unwrap();
    assert_eq!(state.attempts.len(), 2);
    assert_eq!(
        state.attempts[1].0,
        WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(
            "external-revision"
        ))
    );
    let WorkspaceSessionLoad::Loaded { document, .. } = &state.current else {
        panic!("explicit retry must commit");
    };
    assert_eq!(document.workspace_snapshot().unwrap(), snapshot);
    drop(state);
    assert_retained(&root, visual, &snapshot, editor_id, &draft);
}
