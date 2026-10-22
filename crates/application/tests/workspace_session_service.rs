//! 工作区 CAS 用例覆盖安全合并、并发冲突与提交结果未知。
//!
//! @author kongweiguang

use std::{collections::VecDeque, sync::Arc};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    workspace::{
        WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceSessionDocument,
        WorkspaceSessionExpectedRevision, WorkspaceSessionLoad, WorkspaceSessionPane,
        WorkspaceSessionRepository, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
        WorkspaceSessionService, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::TerminalLayoutNode,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
enum SaveResponse {
    Conflict,
    Saved(WorkspaceSessionRevision),
    OutcomeUnknown,
}

#[derive(Clone)]
struct SaveRecord {
    document: WorkspaceSessionDocument,
    expected: WorkspaceSessionExpectedRevision,
}

struct FakeRepository {
    latest: WorkspaceSessionLoad,
    responses: Mutex<VecDeque<SaveResponse>>,
    saves: Mutex<Vec<SaveRecord>>,
    load_count: Mutex<usize>,
}

impl FakeRepository {
    /// 保存明确的 load 快照与逐次写入回执，测试不访问磁盘或用户配置目录。
    fn new(latest: WorkspaceSessionLoad, responses: Vec<SaveResponse>) -> Self {
        Self {
            latest,
            responses: Mutex::new(responses.into()),
            saves: Mutex::new(Vec::new()),
            load_count: Mutex::new(0),
        }
    }
}

#[async_trait]
impl WorkspaceSessionRepository for FakeRepository {
    /// 返回用例预置的冲突后快照，模拟其他进程在 CAS 之间写入的数据。
    async fn load(&self, cancellation: CancellationToken) -> AppResult<WorkspaceSessionLoad> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test cancelled"));
        }
        *self.load_count.lock().await += 1;
        Ok(self.latest.clone())
    }

    /// 记录完整候选和 CAS 基线，再按场景回传确定冲突、保存或未知结果。
    async fn save(
        &self,
        cancellation: CancellationToken,
        document: &WorkspaceSessionDocument,
        expected_revision: WorkspaceSessionExpectedRevision,
    ) -> AppResult<WorkspaceSessionRevision> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test cancelled"));
        }
        self.saves.lock().await.push(SaveRecord {
            document: document.clone(),
            expected: expected_revision,
        });
        match self.responses.lock().await.pop_front() {
            Some(SaveResponse::Conflict) => Err(AppError::conflict("test CAS conflict")),
            Some(SaveResponse::Saved(revision)) => Ok(revision),
            Some(SaveResponse::OutcomeUnknown) => {
                Err(AppError::outcome_unknown("test commit unknown"))
            }
            None => Err(AppError::conflict("unexpected extra save")),
        }
    }
}

/// 构造正式 application document，所有测试都在内存仓储中验证 CAS 状态转换。
fn document(value: Value) -> WorkspaceSessionDocument {
    WorkspaceSessionDocument::try_from_value(value).expect("test document should be valid v3")
}

/// 在给定 typed 快照后追加一个本地终端，使连续保存经过真实 v3 pane codec。
fn append_local_terminal(snapshot: &mut WorkspaceSessionSnapshot, suffix: &str, prompt: &str) {
    let pane_id = PaneId::parse(&format!("pane-{suffix}")).expect("valid pane ID");
    let tab_id = TabId::parse(&format!("tab-{suffix}")).expect("valid tab ID");
    let title = format!("Terminal {suffix}");
    snapshot.terminal_panes.push(WorkspaceSessionPane {
        id: pane_id.clone(),
        title: title.clone(),
        machine_id: "local".to_owned(),
        mode: WorkspacePaneMode::Local,
        target: Some(WorkspaceTargetRef::Local { profile_id: None }),
        remote_host_id: None,
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: prompt.to_owned(),
        status: WorkspaceMachineStatus::Online,
        latency_ms: None,
        local_machine_scope: None,
        output_history: None,
    });
    snapshot.terminal_tabs.push(WorkspaceSessionTab::Terminal {
        id: tab_id.clone(),
        tab_group_id: None,
        title,
        machine_id: "local".to_owned(),
        layout: TerminalLayoutNode::Pane {
            pane_id: pane_id.clone(),
        },
    });
    snapshot.active_tab_id = Some(tab_id);
    snapshot.focused_pane_id = Some(pane_id);
    snapshot
        .validate()
        .expect("fixture snapshot should be valid");
}

/// 首次创建只使用 Missing CAS；外部仅新增根扩展时，重试合并扩展并保留新 revision。
#[tokio::test]
async fn first_save_safely_merges_unknown_fields_after_missing_cas_conflict() {
    let base = WorkspaceSessionDocument::empty_v3();
    let base_snapshot = base.workspace_snapshot().unwrap();
    let mut desired = base_snapshot.clone();
    desired.selected_machine_id = Some("machine-a".to_owned());

    let mut latest_value = WorkspaceSessionDocument::empty_v3().into_value();
    latest_value["futureRoot"] = json!({"writer": "other process"});
    let latest_revision = WorkspaceSessionRevision::new("revision-latest");
    let repository = Arc::new(FakeRepository::new(
        WorkspaceSessionLoad::Loaded {
            document: document(latest_value),
            revision: latest_revision.clone(),
        },
        vec![
            SaveResponse::Conflict,
            SaveResponse::Saved(WorkspaceSessionRevision::new("revision-saved")),
        ],
    ));
    let service = WorkspaceSessionService::new(repository.clone());

    let outcome = service
        .save_workspace_snapshot(
            CancellationToken::new(),
            &base,
            WorkspaceSessionExpectedRevision::Missing,
            &base_snapshot,
            &desired,
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        WorkspaceSessionSaveOutcome::Saved {
            revision: WorkspaceSessionRevision::new("revision-saved")
        }
    );
    let saves = repository.saves.lock().await;
    assert_eq!(saves.len(), 2);
    assert_eq!(saves[0].expected, WorkspaceSessionExpectedRevision::Missing);
    assert_eq!(
        saves[1].expected,
        WorkspaceSessionExpectedRevision::Revision(latest_revision)
    );
    assert_eq!(
        saves[1].document.as_value()["futureRoot"]["writer"],
        json!("other process")
    );
    assert_eq!(
        saves[1].document.as_value()["selectedMachineId"],
        json!("machine-a")
    );
}

/// 最新 owned 字段变化代表真实布局冲突，应用必须保留运行态且停止在一次写入。
#[tokio::test]
async fn owned_snapshot_change_is_reported_as_conflict_without_retry() {
    let base = WorkspaceSessionDocument::empty_v3();
    let base_snapshot = base.workspace_snapshot().unwrap();
    let mut desired = base_snapshot.clone();
    desired.selected_machine_id = Some("machine-local".to_owned());

    let mut latest_value = WorkspaceSessionDocument::empty_v3().into_value();
    latest_value["selectedMachineId"] = json!("machine-external");
    let repository = Arc::new(FakeRepository::new(
        WorkspaceSessionLoad::Loaded {
            document: document(latest_value),
            revision: WorkspaceSessionRevision::new("revision-external"),
        },
        vec![SaveResponse::Conflict],
    ));
    let service = WorkspaceSessionService::new(repository.clone());

    let outcome = service
        .save_workspace_snapshot(
            CancellationToken::new(),
            &base,
            WorkspaceSessionExpectedRevision::Missing,
            &base_snapshot,
            &desired,
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        WorkspaceSessionSaveOutcome::Conflict {
            latest: WorkspaceSessionLoad::Loaded { .. }
        }
    ));
    assert_eq!(repository.saves.lock().await.len(), 1);
    assert_eq!(*repository.load_count.lock().await, 1);
}

/// 提交结果未知直接交给回读状态机，不能执行 load 后自动判定或重试。
#[tokio::test]
async fn unknown_commit_is_returned_without_blind_retry() {
    let base = WorkspaceSessionDocument::empty_v3();
    let base_snapshot = base.workspace_snapshot().unwrap();
    let mut desired = base_snapshot.clone();
    desired.selected_machine_id = Some("machine-a".to_owned());
    let repository = Arc::new(FakeRepository::new(
        WorkspaceSessionLoad::Missing,
        vec![SaveResponse::OutcomeUnknown],
    ));
    let service = WorkspaceSessionService::new(repository.clone());

    let outcome = service
        .save_workspace_snapshot(
            CancellationToken::new(),
            &base,
            WorkspaceSessionExpectedRevision::Missing,
            &base_snapshot,
            &desired,
        )
        .await
        .unwrap();

    assert_eq!(outcome, WorkspaceSessionSaveOutcome::OutcomeUnknown);
    assert_eq!(repository.saves.lock().await.len(), 1);
    assert_eq!(*repository.load_count.lock().await, 0);
}

/// 已取消的首次创建在 repository 入口前终止，保持 no-clobber 文件状态不变。
#[tokio::test]
async fn cancelled_save_does_not_call_repository() {
    let base = WorkspaceSessionDocument::empty_v3();
    let base_snapshot = base.workspace_snapshot().unwrap();
    let mut desired = base_snapshot.clone();
    desired.selected_machine_id = Some("machine-a".to_owned());
    let repository = Arc::new(FakeRepository::new(
        WorkspaceSessionLoad::Missing,
        vec![SaveResponse::Saved(WorkspaceSessionRevision::new("unused"))],
    ));
    let service = WorkspaceSessionService::new(repository.clone());
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(
        service
            .save_workspace_snapshot(
                cancellation,
                &base,
                WorkspaceSessionExpectedRevision::Missing,
                &base_snapshot,
                &desired,
            )
            .await
            .is_err()
    );
    assert!(repository.saves.lock().await.is_empty());
}

/// 缺失 revision 只能配对 canonical missing baseline，不能借它覆盖已加载文档。
#[tokio::test]
async fn missing_revision_rejects_nonempty_baseline_before_repository_write() {
    let mut value = WorkspaceSessionDocument::empty_v3().into_value();
    value["futureRoot"] = json!({"preserve": true});
    let base = document(value);
    let base_snapshot = base.workspace_snapshot().unwrap();
    let mut desired = base_snapshot.clone();
    desired.selected_machine_id = Some("machine-a".to_owned());
    let repository = Arc::new(FakeRepository::new(
        WorkspaceSessionLoad::Missing,
        vec![SaveResponse::Saved(WorkspaceSessionRevision::new("unused"))],
    ));
    let service = WorkspaceSessionService::new(repository.clone());

    let error = service
        .save_workspace_snapshot(
            CancellationToken::new(),
            &base,
            WorkspaceSessionExpectedRevision::Missing,
            &base_snapshot,
            &desired,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert!(repository.saves.lock().await.is_empty());
}

/// 保存回执后的候选文档必须能作为下一次 CAS 的基线，包括 UI 使用的显式空 prompt。
#[tokio::test]
async fn saved_candidate_with_empty_prompt_can_be_reused_as_next_baseline() {
    let base = WorkspaceSessionDocument::empty_v3();
    let base_snapshot = base.workspace_snapshot().unwrap();
    let mut first_desired = base_snapshot.clone();
    append_local_terminal(&mut first_desired, "first", "");
    let first_revision = WorkspaceSessionRevision::new("revision-first");
    let second_revision = WorkspaceSessionRevision::new("revision-second");
    let repository = Arc::new(FakeRepository::new(
        WorkspaceSessionLoad::Missing,
        vec![
            SaveResponse::Saved(first_revision.clone()),
            SaveResponse::Saved(second_revision.clone()),
        ],
    ));
    let service = WorkspaceSessionService::new(repository.clone());

    let first_outcome = service
        .save_workspace_snapshot(
            CancellationToken::new(),
            &base,
            WorkspaceSessionExpectedRevision::Missing,
            &base_snapshot,
            &first_desired,
        )
        .await
        .unwrap();
    assert_eq!(
        first_outcome,
        WorkspaceSessionSaveOutcome::Saved {
            revision: first_revision.clone()
        }
    );
    let saved_document = repository.saves.lock().await[0].document.clone();
    assert_eq!(saved_document.workspace_snapshot().unwrap(), first_desired);

    let mut second_desired = first_desired.clone();
    append_local_terminal(&mut second_desired, "second", "");
    let second_outcome = service
        .save_workspace_snapshot(
            CancellationToken::new(),
            &saved_document,
            WorkspaceSessionExpectedRevision::Revision(first_revision),
            &first_desired,
            &second_desired,
        )
        .await
        .unwrap();

    assert_eq!(
        second_outcome,
        WorkspaceSessionSaveOutcome::Saved {
            revision: second_revision
        }
    );
    assert_eq!(repository.saves.lock().await.len(), 2);
}

/// 当前 reader 不再替缺失/null prompt 造值，避免加载后将不完整会话误当作可保存状态。
///
/// @author kongweiguang
#[test]
fn missing_or_null_prompt_is_rejected_by_current_reader() {
    let base = WorkspaceSessionDocument::empty_v3();
    let mut snapshot = base.workspace_snapshot().unwrap();
    append_local_terminal(&mut snapshot, "legacy", "");
    let mut candidate = base;
    candidate.update_known_fields(&snapshot).unwrap();

    let mut missing = candidate.clone().into_value();
    missing["terminalPanes"][0]
        .as_object_mut()
        .unwrap()
        .remove("prompt");
    assert!(WorkspaceSessionDocument::try_from_value(missing).is_err());

    let mut null = candidate.into_value();
    null["terminalPanes"][0]["prompt"] = Value::Null;
    assert!(WorkspaceSessionDocument::try_from_value(null).is_err());
}
