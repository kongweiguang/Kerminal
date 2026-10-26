//! 通过可阻塞的 fake transport 固定 resize 顺序、reader 并发和失败补偿语义。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::sessions::{
    CreateSessionRequest, RenderSnapshot, RenderSnapshotRequest, SessionChangeKind, SessionFilter,
    SessionService, SessionTransport, SessionTransportControl, SessionTransportFactory,
    SessionTransportOpenError,
};
use kerminal_application::{AppResult, PublicErrorCode};
use kerminal_domain::identity::{PaneId, SessionId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionInfo, SessionLifecycle, SessionOwner,
    SessionTarget, TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::{Mutex, Notify, Semaphore, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const INITIAL_COLUMNS: u16 = 80;
const ROWS: u16 = 4;

struct ResizeFactory {
    peer: Arc<Mutex<Option<DuplexStream>>>,
    control: ResizeControl,
}

#[async_trait]
impl SessionTransportFactory for ResizeFactory {
    /// 使用真实 SessionManager reader/writer 与 VT engine，只替换不确定的系统 PTY 控制面。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let (manager_side, peer) = tokio::io::duplex(4096);
        *self.peer.lock().await = Some(peer);
        let (reader, writer) = tokio::io::split(manager_side);
        Ok(SessionTransport::new(reader, writer, self.control.clone()))
    }
}

#[derive(Clone)]
struct ResizeControl {
    peer: Arc<Mutex<Option<DuplexStream>>>,
    plans: Arc<Mutex<VecDeque<ResizePlan>>>,
    calls: Arc<Mutex<Vec<TerminalSize>>>,
    native_size: Arc<Mutex<TerminalSize>>,
    exit_status: watch::Sender<Option<SessionExitStatus>>,
}

struct ResizePlan {
    /// 模拟 resize 返回前 native 侧已发生的尺寸副作用；None 用于表示副作用结果未知。
    applied_size: Option<TerminalSize>,
    result: Result<(), &'static str>,
    entered: Option<Arc<Notify>>,
    gate: Option<Arc<Semaphore>>,
}

#[async_trait]
impl SessionTransportControl for ResizeControl {
    /// 在进入 ACK/错误之前记录可能已生效的 native 尺寸，以验证补偿不能假定 Err 无副作用。
    ///
    /// @author kongweiguang
    async fn resize(&self, size: TerminalSize) -> AppResult<()> {
        self.calls.lock().await.push(size);
        let plan = self.plans.lock().await.pop_front().unwrap_or(ResizePlan {
            applied_size: Some(size),
            result: Ok(()),
            entered: None,
            gate: None,
        });
        if let Some(applied_size) = plan.applied_size {
            *self.native_size.lock().await = applied_size;
        }
        if let Some(entered) = plan.entered {
            entered.notify_one();
        }
        if let Some(gate) = plan.gate {
            let permit = gate.acquire().await.map_err(|_| {
                kerminal_application::AppError::infrastructure("resize 测试闸门已关闭")
            })?;
            permit.forget();
        }
        plan.result
            .map_err(kerminal_application::AppError::infrastructure)
    }

    /// 关闭时释放测试 peer，让生产 reader 经 EOF 路径收尾，不泄漏会话任务。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.peer.lock().await.take();
        self.exit_status.send_replace(Some(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        }));
        Ok(())
    }

    /// 等待测试明确触发 EOF/关闭后再报告退出，避免真实 SessionManager child watcher 误判新会话已退出。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        let mut exit_status = self.exit_status.subscribe();
        loop {
            let current_status = exit_status.borrow().clone();
            if let Some(status) = current_status {
                return Ok(status);
            }
            exit_status.changed().await.map_err(|_| {
                kerminal_application::AppError::infrastructure("resize 测试退出回执通道意外关闭")
            })?;
        }
    }
}

/// 构造经生产 manager 的隔离本地会话，并让 fake transport 从确认尺寸开始计数。
///
/// @author kongweiguang
async fn create_session(
    close_wait: Duration,
) -> (
    Arc<SessionManager>,
    Arc<ResizeFactory>,
    SessionId,
    TerminalSize,
) {
    let initial_size =
        TerminalSize::new(INITIAL_COLUMNS, ROWS, 0, 0).expect("test terminal dimensions are valid");
    let peer = Arc::new(Mutex::new(None));
    let control = ResizeControl {
        peer: Arc::clone(&peer),
        plans: Arc::new(Mutex::new(VecDeque::new())),
        calls: Arc::new(Mutex::new(Vec::new())),
        native_size: Arc::new(Mutex::new(initial_size)),
        exit_status: watch::channel(None).0,
    };
    let factory = Arc::new(ResizeFactory { peer, control });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig {
            close_wait,
            ..SessionRuntimeConfig::default()
        },
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("resize-test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: initial_size,
            initial_palette: None,
        })
        .await
        .expect("create isolated resize test session");
    // 创建路径会先确认一次初始 PTY 尺寸；单独断言后清零，后续序列只比较被测调整而不掩盖创建行为。
    {
        let mut calls = factory.control.calls.lock().await;
        assert_eq!(*calls, vec![initial_size]);
        calls.clear();
    }
    (manager, factory, info.session_id, initial_size)
}

/// 等待固定 VT 序列在目标几何下写入指定 cell，超时只作失败看门狗而不驱动调度。
///
/// @author kongweiguang
async fn wait_for_target_cursor(
    manager: &SessionManager,
    session_id: &SessionId,
    target_size: TerminalSize,
) -> RenderSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read terminal snapshot while native resize is blocked");
            if snapshot.size == target_size
                && snapshot.cursor.row == 0
                && snapshot.cursor.column == 6
                && snapshot_has_cell(&snapshot, 0, 5, "X")
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader processes VT output using the pending target grid")
}

/// 等待 production lifecycle observer 消化 fake peer EOF，而不是依赖一次 yield 的调度偶然性。
///
/// @author kongweiguang
async fn wait_for_lifecycle(
    manager: &SessionManager,
    session_id: &SessionId,
    expected: SessionLifecycle,
) -> SessionInfo {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let info = manager
                .list(SessionFilter::default())
                .await
                .expect("list sessions while waiting for lifecycle observer")
                .into_iter()
                .find(|info| &info.session_id == session_id)
                .expect("session remains listed while lifecycle is observed");
            if info.lifecycle == expected {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session lifecycle reaches expected state")
}

/// 通过快照的 cell span 精确检查目标列，不用 UTF-8 字节偏移猜测终端光标。
///
/// @author kongweiguang
fn snapshot_has_cell(snapshot: &RenderSnapshot, row: u16, column: u16, text: &str) -> bool {
    snapshot
        .rows
        .iter()
        .find(|line| line.viewport_row == row)
        .is_some_and(|line| {
            line.runs.iter().any(|run| {
                run.cell_spans.iter().any(|span| {
                    span.column_range == (column..column.saturating_add(1))
                        && run.text.get(span.byte_range.clone()) == Some(text)
                })
            })
        })
}

/// 只用 peer 写入真实 VT 字节，并以 yield 等待 reader 的状态更新，不持有 session state 锁。
///
/// @author kongweiguang
async fn write_fixture_output(factory: &ResizeFactory, bytes: &[u8]) {
    factory
        .peer
        .lock()
        .await
        .as_mut()
        .expect("test peer remains open")
        .write_all(bytes)
        .await
        .expect("write fixed VT fixture");
}

/// 证明目标尺寸在 transport ACK 前已供 reader 使用，且公开 SessionInfo 仍保留确认尺寸。
///
/// @author kongweiguang
#[tokio::test]
async fn reader_uses_engine_first_resize_while_native_ack_is_blocked() {
    let (manager, factory, session_id, initial_size) = create_session(Duration::from_secs(2)).await;
    let target_size = TerminalSize::new(40, ROWS, 0, 0).expect("valid target dimensions");
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    factory.control.plans.lock().await.push_back(ResizePlan {
        applied_size: Some(target_size),
        result: Ok(()),
        entered: Some(Arc::clone(&entered)),
        gate: Some(Arc::clone(&gate)),
    });
    let mut changes = manager
        .subscribe(session_id.clone())
        .await
        .expect("subscribe before resize");
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read initial snapshot");
    let resize_manager = Arc::clone(&manager);
    let resize_session = session_id.clone();
    let resize =
        tokio::spawn(async move { resize_manager.resize(resize_session, target_size).await });

    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("native resize reaches its controlled await");
    let pending = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read pending engine-first snapshot");
    assert_eq!(pending.size, target_size);
    assert!(pending.revision > before.revision);
    let listed = manager
        .list(SessionFilter::default())
        .await
        .expect("list while transport resize is pending");
    assert_eq!(
        listed
            .iter()
            .find(|info| info.session_id == session_id)
            .expect("session remains listed")
            .size,
        initial_size,
        "public size remains the last native-confirmed size until ACK"
    );

    let mut fixture = b"\x1b[2J\x1b[H".to_vec();
    fixture.extend(std::iter::repeat_n(b'P', 45));
    fixture.extend_from_slice(b"\x1b[1AX");
    write_fixture_output(&factory, &fixture).await;
    let rendered = wait_for_target_cursor(&manager, &session_id, target_size).await;
    assert!(
        rendered.revision > pending.revision,
        "reader output advances the engine revision while native resize remains blocked"
    );

    gate.add_permits(1);
    resize
        .await
        .expect("resize task completes")
        .expect("native ACK commits resize");
    let committed = manager
        .list(SessionFilter::default())
        .await
        .expect("list after native ACK")
        .into_iter()
        .find(|info| info.session_id == session_id)
        .expect("session remains listed");
    assert_eq!(committed.size, target_size);
    let change = changes
        .recv(&CancellationToken::new())
        .await
        .expect("receive resize revision")
        .expect("resize change remains open");
    assert_eq!(change.kind, SessionChangeKind::Resized);
    assert_eq!(change.revision, rendered.revision);

    manager
        .close(session_id)
        .await
        .expect("close isolated resize test session");
}

/// 验证 native Err 即使可能已有副作用也会补偿一次，且失败视图回到最后确认尺寸。
///
/// @author kongweiguang
#[tokio::test]
async fn native_failure_compensates_and_publishes_engine_rollback() {
    let (manager, factory, session_id, initial_size) = create_session(Duration::from_secs(2)).await;
    let target_size = TerminalSize::new(40, ROWS, 0, 0).expect("valid target dimensions");
    factory.control.plans.lock().await.extend([
        ResizePlan {
            applied_size: Some(target_size),
            result: Err("fake resize failed after side effect"),
            entered: None,
            gate: None,
        },
        ResizePlan {
            applied_size: Some(initial_size),
            result: Ok(()),
            entered: None,
            gate: None,
        },
    ]);
    let mut changes = manager
        .subscribe(session_id.clone())
        .await
        .expect("subscribe before failed resize");
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read initial snapshot");

    let error = manager
        .resize(session_id.clone(), target_size)
        .await
        .expect_err("native resize error must not be reported as success");
    assert!(error.public_message().contains("已恢复到原尺寸"));
    assert_eq!(
        *factory.control.native_size.lock().await,
        initial_size,
        "one acknowledged compensating resize restores native dimensions"
    );
    assert_eq!(
        *factory.control.calls.lock().await,
        vec![target_size, initial_size]
    );
    let snapshot = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read restored snapshot");
    assert_eq!(snapshot.size, initial_size);
    assert!(snapshot.revision > before.revision);
    let change = changes
        .recv(&CancellationToken::new())
        .await
        .expect("receive rollback revision")
        .expect("resize subscription remains open");
    assert_eq!(change.kind, SessionChangeKind::Resized);
    assert_eq!(change.revision, snapshot.revision);

    manager
        .close(session_id)
        .await
        .expect("close isolated resize test session");
}

/// 自然退出与 native resize 重叠时只回滚可保留的 VT 历史网格，不向已结束 control 发补偿请求。
///
/// @author kongweiguang
#[tokio::test]
async fn natural_exit_during_native_failure_returns_conflict_without_compensation() {
    let (manager, factory, session_id, initial_size) = create_session(Duration::from_secs(2)).await;
    let target_size = TerminalSize::new(40, ROWS, 0, 0).expect("valid target dimensions");
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    factory.control.plans.lock().await.push_back(ResizePlan {
        applied_size: Some(target_size),
        result: Err("native resize failed as peer exited"),
        entered: Some(Arc::clone(&entered)),
        gate: Some(Arc::clone(&gate)),
    });
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read initial terminal history");

    let resize_manager = Arc::clone(&manager);
    let resize_session = session_id.clone();
    let resize =
        tokio::spawn(async move { resize_manager.resize(resize_session, target_size).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("native resize is in flight before natural exit");

    let peer = factory.peer.lock().await.take();
    drop(peer);
    factory
        .control
        .exit_status
        .send_replace(Some(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        }));
    let exited = wait_for_lifecycle(&manager, &session_id, SessionLifecycle::Exited).await;
    assert_eq!(exited.size, initial_size);

    gate.add_permits(1);
    let error = resize
        .await
        .expect("resize task completes after native control returns")
        .expect_err("natural exit rejects resize commit");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(error.public_message(), "会话已结束，未提交尺寸变更");
    assert!(
        std::error::Error::source(&error).is_some(),
        "the native resize failure remains available to internal diagnostics"
    );
    assert_eq!(
        *factory.control.calls.lock().await,
        vec![target_size],
        "an already-exited session must not receive a compensating native resize"
    );
    assert_eq!(
        *factory.control.native_size.lock().await,
        target_size,
        "the fake confirms the original resize may have taken effect before exit"
    );
    let historical_grid = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read restored historical grid after exit");
    assert_eq!(historical_grid.size, initial_size);
    assert!(historical_grid.revision > before.revision);

    manager
        .close(session_id)
        .await
        .expect("close naturally exited isolated test session");
}

/// 用明确的补偿 Err 表达 transport 状态未知，并证明显式同目标重试不会被本地尺寸短路。
///
/// @author kongweiguang
#[tokio::test]
async fn failed_compensation_reports_unknown_and_same_target_retry_reaches_transport() {
    let (manager, factory, session_id, initial_size) = create_session(Duration::from_secs(2)).await;
    let target_size = TerminalSize::new(40, ROWS, 0, 0).expect("valid target dimensions");
    factory.control.plans.lock().await.extend([
        ResizePlan {
            applied_size: Some(target_size),
            result: Err("fake resize result is unknown after side effect"),
            entered: None,
            gate: None,
        },
        ResizePlan {
            applied_size: None,
            result: Err("compensation explicitly failed with unknown native size"),
            entered: None,
            gate: None,
        },
        ResizePlan {
            applied_size: Some(target_size),
            result: Ok(()),
            entered: None,
            gate: None,
        },
    ]);
    let mut changes = manager
        .subscribe(session_id.clone())
        .await
        .expect("subscribe before unknown resize");
    let before = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read initial snapshot");

    let error = manager
        .resize(session_id.clone(), target_size)
        .await
        .expect_err("failed compensation must preserve the unknown transport state");
    assert!(error.public_message().contains("尺寸状态未确认"));
    assert_eq!(
        *factory.control.native_size.lock().await,
        target_size,
        "the first native call changed size and explicit compensation failure did not acknowledge restoration"
    );
    let restored = manager
        .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read local grid after failed compensation");
    assert_eq!(restored.size, initial_size);
    assert!(restored.revision > before.revision);
    let still_unconfirmed = manager
        .list(SessionFilter::default())
        .await
        .expect("list after failed compensation")
        .into_iter()
        .find(|info| info.session_id == session_id)
        .expect("session remains listed");
    assert_eq!(still_unconfirmed.size, initial_size);

    manager
        .resize(session_id.clone(), target_size)
        .await
        .expect("explicit retry with the same target must reach native transport");
    assert_eq!(
        *factory.control.calls.lock().await,
        vec![target_size, initial_size, target_size]
    );
    assert_eq!(*factory.control.native_size.lock().await, target_size);
    let committed = manager
        .list(SessionFilter::default())
        .await
        .expect("list after explicit retry")
        .into_iter()
        .find(|info| info.session_id == session_id)
        .expect("session remains listed");
    assert_eq!(committed.size, target_size);
    let change = changes
        .recv(&CancellationToken::new())
        .await
        .expect("receive resize recovery revision")
        .expect("resize subscription remains open");
    assert_eq!(change.kind, SessionChangeKind::Resized);

    manager
        .close(session_id)
        .await
        .expect("close isolated resize test session");
}

/// 补偿未 ACK 时保持 operation_gate，避免下一次 resize 越过可能仍在执行的旧尺寸 native 操作。
///
/// @author kongweiguang
#[tokio::test]
async fn next_resize_waits_until_native_compensation_acknowledges() {
    let (manager, factory, session_id, initial_size) = create_session(Duration::from_secs(2)).await;
    let first_target = TerminalSize::new(40, ROWS, 0, 0).expect("valid first target");
    let next_target = TerminalSize::new(60, ROWS, 0, 0).expect("valid next target");
    let compensation_entered = Arc::new(Notify::new());
    let compensation_gate = Arc::new(Semaphore::new(0));
    factory.control.plans.lock().await.extend([
        ResizePlan {
            applied_size: Some(first_target),
            result: Err("first native resize failed after side effect"),
            entered: None,
            gate: None,
        },
        ResizePlan {
            applied_size: Some(initial_size),
            result: Ok(()),
            entered: Some(Arc::clone(&compensation_entered)),
            gate: Some(Arc::clone(&compensation_gate)),
        },
        ResizePlan {
            applied_size: Some(next_target),
            result: Ok(()),
            entered: None,
            gate: None,
        },
    ]);

    let first_manager = Arc::clone(&manager);
    let first_session = session_id.clone();
    let first =
        tokio::spawn(async move { first_manager.resize(first_session, first_target).await });
    tokio::time::timeout(Duration::from_secs(2), compensation_entered.notified())
        .await
        .expect("first resize reaches blocked native compensation");

    let next_manager = Arc::clone(&manager);
    let next_session = session_id.clone();
    let next_started = Arc::new(Notify::new());
    let next_started_for_task = Arc::clone(&next_started);
    let mut next = tokio::spawn(async move {
        next_started_for_task.notify_one();
        next_manager.resize(next_session, next_target).await
    });
    tokio::time::timeout(Duration::from_secs(2), next_started.notified())
        .await
        .expect("next resize task starts while compensation is blocked");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut next)
            .await
            .is_err(),
        "next resize remains pending on operation_gate until compensation completes"
    );
    assert_eq!(
        *factory.control.calls.lock().await,
        vec![first_target, initial_size],
        "native control has not received the next target before compensation ACK"
    );

    compensation_gate.add_permits(1);
    let failed_first = first
        .await
        .expect("first resize task completes after compensation ACK")
        .expect_err("first native resize still reports its primary failure");
    assert!(failed_first.public_message().contains("已恢复到原尺寸"));
    next.await
        .expect("next resize task completes after it obtains operation_gate")
        .expect("next target is ACKed after prior compensation");
    assert_eq!(
        *factory.control.calls.lock().await,
        vec![first_target, initial_size, next_target],
        "native calls remain serialized in the submitted order"
    );
    assert_eq!(*factory.control.native_size.lock().await, next_target);

    manager
        .close(session_id)
        .await
        .expect("close isolated resize test session");
}
