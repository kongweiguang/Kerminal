//! 可见终端绑定下的退出历史尺寸回流、失效目标拒绝与 revision guarded 拖选。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::sessions::{
    CreateSessionRequest, PlainTextSnapshotRequest, RenderSnapshotRequest, SelectionViewport,
    SessionChangeKind, SessionService, SessionTransport, SessionTransportControl,
    SessionTransportFactory, SessionTransportOpenError, UiSessionBinding,
};
use kerminal_application::{AppError, AppResult, PublicErrorCode};
use kerminal_domain::identity::{PaneId, SessionId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionInfo, SessionLifecycle, SessionOwner,
    SessionTarget, TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use kerminal_terminal::snapshot::{
    RenderSnapshot, TerminalPoint, TerminalSelection, TerminalSelectionKind,
};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const LONG_PROMPT: &str = "PS C:\\Users\\operator\\Documents\\work\\projects\\terminal\\components\\ui\\selection\\resize\\nested\\very-long-product-name\\workspace\\source> ";

#[derive(Clone)]
struct EndedHistoryFactory {
    resize_calls: Arc<AtomicUsize>,
}

struct EndedHistoryControl {
    resize_calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct LiveSelectionFactory {
    output_peer: Arc<Mutex<Option<DuplexStream>>>,
}

struct LiveSelectionControl {
    output_peer: Arc<Mutex<Option<DuplexStream>>>,
}

impl LiveSelectionFactory {
    /// Keep the test PTY peer available so output can be injected at exact snapshot boundaries.
    ///
    /// @author kongweiguang
    async fn write_output(&self, bytes: &[u8]) {
        let mut peer = self.output_peer.lock().await;
        peer.as_mut()
            .expect("live test transport retains its output peer")
            .write_all(bytes)
            .await
            .expect("write deterministic terminal output");
    }
}

#[async_trait]
impl SessionTransportControl for LiveSelectionControl {
    /// No native process exists; the session manager still exercises its real engine resize path.
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// Releasing the duplex peer gives the production reader a clean EOF during test cleanup.
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.output_peer.lock().await.take();
        Ok(())
    }

    /// Report the deterministic natural exit after the reader observes the released peer.
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

#[async_trait]
impl SessionTransportFactory for LiveSelectionFactory {
    /// Feed a known initial row and retain its writer to inject output between UI snapshot and Start.
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test transport open cancelled").into());
        }
        let (reader, mut output_peer) = tokio::io::duplex(4096);
        output_peer
            .write_all(b"anchor")
            .await
            .expect("write initial row for selection test");
        *self.output_peer.lock().await = Some(output_peer);
        Ok(SessionTransport::new(
            reader,
            tokio::io::sink(),
            LiveSelectionControl {
                output_peer: Arc::clone(&self.output_peer),
            },
        ))
    }
}

#[async_trait]
impl SessionTransportControl for EndedHistoryControl {
    /// Keep the test's native-size call count observable so ended-history resizing proves it did not touch transport.
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        self.resize_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// The output reader is already at EOF; close has no external resource to release.
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        Ok(())
    }

    /// Confirm the fixed natural-exit status used by the real session EOF path.
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

#[async_trait]
impl SessionTransportFactory for EndedHistoryFactory {
    /// Feed one fixed long prompt through the production reader, then EOF, without launching a shell process.
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test transport open cancelled").into());
        }
        let (reader, mut output_peer) = tokio::io::duplex(4096);
        output_peer
            .write_all(LONG_PROMPT.as_bytes())
            .await
            .expect("write fixed historical prompt");
        drop(output_peer);
        Ok(SessionTransport::new(
            reader,
            tokio::io::sink(),
            EndedHistoryControl {
                resize_calls: Arc::clone(&self.resize_calls),
            },
        ))
    }
}

/// Create a real manager-owned UI/MCP session whose reader reaches EOF after fixed historical output.
///
/// @author kongweiguang
async fn create_ended_history_session(
    owner: SessionOwner,
) -> (Arc<SessionManager>, Arc<EndedHistoryFactory>, SessionInfo) {
    let factory = Arc::new(EndedHistoryFactory {
        resize_calls: Arc::new(AtomicUsize::new(0)),
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("ended-history-test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner,
            size: TerminalSize::new(80, 6, 800, 120).expect("valid initial grid"),
        })
        .await
        .expect("create fixed ended-history session");
    (manager, factory, info)
}

/// Create a running UI session whose retained writer makes output timing deterministic in tests.
///
/// @author kongweiguang
async fn create_live_selection_session(
    owner: SessionOwner,
) -> (Arc<SessionManager>, Arc<LiveSelectionFactory>, SessionInfo) {
    let factory = Arc::new(LiveSelectionFactory {
        output_peer: Arc::new(Mutex::new(None)),
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("live-selection-test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner,
            size: TerminalSize::new(80, 6, 800, 120).expect("valid initial grid"),
        })
        .await
        .expect("create live selection test session");
    (manager, factory, info)
}

/// Wait for a real manager snapshot revision to advance after the retained PTY peer receives bytes.
///
/// @author kongweiguang
async fn wait_for_revision_after(
    manager: &SessionManager,
    session_id: &SessionId,
    revision: u64,
) -> kerminal_terminal::snapshot::RenderSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read live selection snapshot");
            if snapshot.revision > revision {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader publishes output revision")
}

/// Wait for a rendered marker rather than assuming one asynchronous PTY read consumes a full write.
///
/// @author kongweiguang
async fn wait_for_render_marker(
    manager: &SessionManager,
    session_id: &SessionId,
    marker: &str,
) -> RenderSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read terminal while waiting for rendered marker");
            if snapshot
                .rows
                .iter()
                .any(|row| row.runs.iter().any(|run| run.text.contains(marker)))
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("terminal output reaches rendered viewport")
}

/// Build the exact coordinate evidence captured for a terminal row, so tests exercise content anchoring across output.
///
/// @author kongweiguang
fn selection_viewport_for_line(snapshot: &RenderSnapshot, line: i64) -> SelectionViewport {
    let endpoint_row_witness = snapshot
        .rows
        .iter()
        .find(|row| row.absolute_line == line)
        .expect("selection endpoint row exists in its source snapshot")
        .witness();
    SelectionViewport {
        size: snapshot.size,
        scrollback_offset: snapshot.scrollback_offset,
        scroll_origin: snapshot.scroll_origin,
        grid_epoch: snapshot.grid_epoch,
        endpoint_row_witness: Some(endpoint_row_witness),
    }
}

/// Wait for the real reader EOF path to publish Exited before exercising historical view behavior.
///
/// @author kongweiguang
async fn wait_for_exited(manager: &SessionManager, session_id: &SessionId) -> SessionInfo {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let info = manager
                .list(Default::default())
                .await
                .expect("list session while waiting for EOF");
            if let Some(info) = info.into_iter().find(|info| &info.session_id == session_id)
                && info.lifecycle == SessionLifecycle::Exited
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader publishes a confirmed exited lifecycle")
}

/// Ended UI history can reflow and publish a resize without calling the dead transport.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn exited_ui_history_reflows_without_transport_resize() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, factory, created) = create_ended_history_session(owner.clone()).await;
    let exited = wait_for_exited(&manager, &created.session_id).await;
    assert_eq!(exited.lifecycle, SessionLifecycle::Exited);
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read ended session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    let before = manager
        .plaintext_snapshot(
            created.session_id.clone(),
            PlainTextSnapshotRequest {
                include_scrollback: true,
                max_lines: 100,
                max_bytes: None,
            },
        )
        .await
        .expect("read historical prompt before reflow");
    let before_text = before.text.replace(['\n', '\r'], "");
    assert!(before_text.contains(LONG_PROMPT.trim_end()));
    let mut changes = manager
        .subscribe(created.session_id.clone())
        .await
        .expect("subscribe to resize revision");
    let never_cancelled = CancellationToken::new();

    let target_size = TerminalSize::new(20, 5, 200, 100).expect("valid narrow grid");
    let calls_before = factory.resize_calls.load(Ordering::Acquire);
    manager
        .resize_ui_view(binding, target_size)
        .await
        .expect("reflow retained history after process exit");

    let rendered = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read reflowed historical grid");
    assert_eq!(rendered.size, target_size);
    let after = manager
        .plaintext_snapshot(
            created.session_id.clone(),
            PlainTextSnapshotRequest {
                include_scrollback: true,
                max_lines: 100,
                max_bytes: None,
            },
        )
        .await
        .expect("read historical prompt after reflow");
    let after_text = after.text.replace(['\n', '\r'], "");
    assert!(after_text.contains(LONG_PROMPT.trim_end()));
    assert_eq!(factory.resize_calls.load(Ordering::Acquire), calls_before);
    assert_eq!(
        changes
            .recv(&never_cancelled)
            .await
            .expect("receive resize revision")
            .expect("session remains observable")
            .kind,
        SessionChangeKind::Resized
    );
    let final_info = wait_for_exited(&manager, &created.session_id).await;
    assert_eq!(final_info.size, target_size);
    manager
        .close(created.session_id)
        .await
        .expect("close exited test session");
}

/// A stale generation cannot resize an ended grid or call its retired transport.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn stale_generation_cannot_resize_exited_ui_history() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, factory, created) = create_ended_history_session(owner.clone()).await;
    wait_for_exited(&manager, &created.session_id).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read current generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation.wrapping_add(1),
    };
    let target_size = TerminalSize::new(30, 4, 300, 80).expect("valid target grid");
    assert!(manager.resize_ui_view(binding, target_size).await.is_err());
    let current = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read unchanged grid after stale request");
    assert_eq!(current.size, created.size);
    assert_eq!(factory.resize_calls.load(Ordering::Acquire), 1);
    manager
        .close(created.session_id)
        .await
        .expect("close exited test session");
}

/// A non-UI owner cannot borrow an exact matching binding to resize a visible view.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn non_ui_owner_cannot_resize_history_through_ui_binding() {
    let (manager, factory, created) = create_ended_history_session(SessionOwner::Mcp).await;
    wait_for_exited(&manager, &created.session_id).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read non-UI session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner: SessionOwner::Mcp,
        session_generation: context.session_generation,
    };
    let target_size = TerminalSize::new(40, 5, 400, 100).expect("valid target grid");
    assert!(manager.resize_ui_view(binding, target_size).await.is_err());
    let current = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read unchanged non-UI grid");
    assert_eq!(current.size, created.size);
    assert_eq!(factory.resize_calls.load(Ordering::Acquire), 1);
    manager
        .close(created.session_id)
        .await
        .expect("close exited test session");
}

/// A cancelled selection request cannot mutate an ended generation's shared engine selection.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn cancelled_ui_selection_does_not_change_existing_selection() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, _factory, created) = create_ended_history_session(owner.clone()).await;
    wait_for_exited(&manager, &created.session_id).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read ended session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    let selection = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 2 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection(binding.clone(), Some(selection), None)
        .await
        .expect("establish selection before cancellation");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let stale_move = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 4 },
        kind: TerminalSelectionKind::Character,
    };
    assert!(
        manager
            .set_ui_selection_cancellable(binding, Some(stale_move), Some(17), cancellation)
            .await
            .is_err()
    );
    let snapshot = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read selection after cancelled update");
    assert_eq!(snapshot.selection, Some(selection));
    manager
        .close(created.session_id)
        .await
        .expect("close exited test session");
}

/// Output between the displayed snapshot and Start rejects the stale anchor, while fresh gestures survive later output.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn ui_selection_start_requires_current_snapshot_but_move_keeps_anchor() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, factory, created) = create_live_selection_session(owner.clone()).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read live session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let text = manager
                .plaintext_snapshot(
                    created.session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 20,
                        max_bytes: None,
                    },
                )
                .await
                .expect("read initial live row");
            if text.text.contains("anchor") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader publishes initial anchor row");

    let existing_selection = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 2 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection(binding.clone(), Some(existing_selection), None)
        .await
        .expect("install selection that stale Start must preserve");
    let displayed = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read the coordinates displayed to the user");
    factory.write_output(b"!").await;
    let advanced = wait_for_revision_after(&manager, &created.session_id, displayed.revision).await;
    let mut changes = manager
        .subscribe(created.session_id.clone())
        .await
        .expect("subscribe after the output revision");
    let stale_start = TerminalSelection {
        start: TerminalPoint { line: 0, column: 1 },
        end: TerminalPoint { line: 0, column: 1 },
        kind: TerminalSelectionKind::Character,
    };
    let stale_error = manager
        .set_ui_selection_cancellable_with_revision(
            binding.clone(),
            Some(stale_start),
            Some(10),
            Some(displayed.revision),
            CancellationToken::new(),
        )
        .await
        .expect_err("output after displayed snapshot invalidates a new anchor");
    assert_eq!(stale_error.public_code(), PublicErrorCode::Conflict);
    assert!(
        manager
            .set_ui_selection_cancellable_with_revision(
                binding.clone(),
                Some(stale_start),
                Some(10),
                None,
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    let never_cancelled = CancellationToken::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), changes.recv(&never_cancelled))
            .await
            .is_err()
    );
    let after_stale_start = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read selection after stale Start");
    assert_eq!(after_stale_start.revision, advanced.revision);
    assert_eq!(after_stale_start.selection, Some(existing_selection));

    let fresh_start = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 0 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection_cancellable_with_revision(
            binding.clone(),
            Some(fresh_start),
            Some(11),
            Some(after_stale_start.revision),
            CancellationToken::new(),
        )
        .await
        .expect("current snapshot starts a new gesture after stale token tombstone");
    let accepted_start = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read accepted anchor");
    factory.write_output(b"\r\nnext").await;
    wait_for_revision_after(&manager, &created.session_id, accepted_start.revision).await;

    let moved_end = TerminalSelection {
        start: TerminalPoint { line: 1, column: 0 },
        end: TerminalPoint { line: 1, column: 4 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection_cancellable_with_revision(
            binding,
            Some(moved_end),
            Some(11),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("same accepted gesture can extend after new terminal output");
    let moved = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read moved anchor and endpoint");
    assert_eq!(
        moved.selection,
        Some(TerminalSelection {
            start: fresh_start.start,
            end: moved_end.end,
            kind: TerminalSelectionKind::Character,
        })
    );
    manager
        .close(created.session_id)
        .await
        .expect("close live selection test session");
}

/// A selection-only revision change keeps older grid coordinates valid for a rapid follow-up gesture.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn ui_selection_start_accepts_snapshot_after_selection_only_revision_change() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, _factory, created) = create_live_selection_session(owner.clone()).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read live session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let text = manager
                .plaintext_snapshot(
                    created.session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 20,
                        max_bytes: None,
                    },
                )
                .await
                .expect("read initial live row");
            if text.text.contains("anchor") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader publishes initial anchor row");

    let displayed = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("capture grid revision before click selection");
    manager
        .set_selection(
            created.session_id.clone(),
            Some(TerminalSelection {
                start: TerminalPoint { line: 0, column: 0 },
                end: TerminalPoint { line: 0, column: 5 },
                kind: TerminalSelectionKind::Line,
            }),
        )
        .await
        .expect("install double/triple-click style selection");
    let after_click_selection = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read selection-only revision");
    assert!(after_click_selection.revision > displayed.revision);

    let fresh_anchor = TerminalSelection {
        start: TerminalPoint { line: 0, column: 1 },
        end: TerminalPoint { line: 0, column: 1 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection_cancellable_with_revision(
            binding.clone(),
            Some(fresh_anchor),
            Some(31),
            Some(displayed.revision),
            CancellationToken::new(),
        )
        .await
        .expect("selection-only revision does not invalidate unchanged grid coordinates");
    let started = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read new gesture after the old but grid-valid snapshot");
    assert!(started.revision > after_click_selection.revision);
    assert_eq!(started.selection, None);
    let moved_selection = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 5 },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection_cancellable_with_revision(
            binding,
            Some(moved_selection),
            Some(31),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("accepted empty anchor extends on its first Move");
    let moved = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read non-empty selection produced by Move");
    assert_eq!(
        moved.selection,
        Some(TerminalSelection {
            start: fresh_anchor.start,
            end: moved_selection.end,
            kind: TerminalSelectionKind::Character,
        })
    );
    manager
        .close(created.session_id)
        .await
        .expect("close live selection test session");
}

/// Rebase a captured viewport after several PTY chunks, keep its Alacritty anchor on Move, and reject a resized viewport.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn ui_selection_viewport_rebases_output_and_rejects_stale_size() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, factory, created) = create_live_selection_session(owner.clone()).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read live session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    let displayed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read initial selection viewport");
            if snapshot
                .rows
                .iter()
                .any(|row| row.runs.iter().any(|run| run.text.contains("anchor")))
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader publishes the anchor row");
    let anchor_line = displayed
        .rows
        .iter()
        .find(|row| row.runs.iter().any(|run| run.text.contains("anchor")))
        .expect("find the displayed anchor line")
        .absolute_line;
    let viewport = selection_viewport_for_line(&displayed, anchor_line);

    factory.write_output(b"\r\none\r\ntwo\r\nthree").await;
    let first_output =
        wait_for_revision_after(&manager, &created.session_id, displayed.revision).await;
    factory
        .write_output(b"\r\nfour\r\nfive\r\nsix\r\nseven")
        .await;
    wait_for_revision_after(&manager, &created.session_id, first_output.revision).await;

    let old_viewport_start = TerminalSelection {
        start: TerminalPoint {
            line: anchor_line,
            column: 0,
        },
        end: TerminalPoint {
            line: anchor_line,
            column: 6,
        },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection_cancellable_in_viewport(
            binding.clone(),
            Some(old_viewport_start),
            Some(41),
            viewport,
            CancellationToken::new(),
        )
        .await
        .expect("a grid-compatible viewport start rebases after multiple output chunks");
    let started = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("capture the accepted gesture revision before its next Move");
    let started_text = manager
        .selected_text(created.session_id.clone())
        .await
        .expect("read rebased selection")
        .expect("anchor remains selected");
    assert!(
        started_text.contains("anchor"),
        "selected text: {started_text:?}"
    );

    factory.write_output(b"\r\neight").await;
    let after_move_output =
        wait_for_revision_after(&manager, &created.session_id, started.revision).await;
    let moved_line = after_move_output
        .rows
        .iter()
        .find(|row| row.runs.iter().any(|run| run.text.contains("eight")))
        .expect("find the current move endpoint line")
        .absolute_line;
    let current_viewport = selection_viewport_for_line(&after_move_output, moved_line);
    let moved_end = TerminalSelection {
        start: old_viewport_start.start,
        end: TerminalPoint {
            line: moved_line,
            column: 4,
        },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection_cancellable_in_viewport(
            binding.clone(),
            Some(moved_end),
            Some(41),
            current_viewport,
            CancellationToken::new(),
        )
        .await
        .expect("same gesture moves its endpoint while retaining the engine anchor");
    let moved_text = manager
        .selected_text(created.session_id.clone())
        .await
        .expect("read moved selection")
        .expect("moved range remains selected");
    assert!(
        moved_text.contains("anchor"),
        "selected text: {moved_text:?}"
    );
    assert!(
        moved_text.contains("eight"),
        "selected text: {moved_text:?}"
    );

    let pre_resize = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("capture old viewport before resizing");
    let old_endpoint_line = pre_resize
        .selection
        .map(|selection| selection.end.line)
        .expect("selection remains active before the resize");
    let old_viewport = selection_viewport_for_line(&pre_resize, old_endpoint_line);
    let resized = TerminalSize::new(80, 7, 800, 140).expect("valid resized grid");
    manager
        .resize_ui_view(binding.clone(), resized)
        .await
        .expect("resize live viewport");
    let after_resize = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read reflowed grid after resize");
    let confirmed_after_resize = manager
        .selected_text(created.session_id.clone())
        .await
        .expect("read selection after reflow");
    let stale_size_error = manager
        .set_ui_selection_cancellable_in_viewport(
            binding,
            Some(TerminalSelection {
                start: after_resize.selection.unwrap_or(old_viewport_start).start,
                end: TerminalPoint {
                    line: after_resize.rows[0].absolute_line,
                    column: 3,
                },
                kind: TerminalSelectionKind::Character,
            }),
            Some(42),
            old_viewport,
            CancellationToken::new(),
        )
        .await
        .expect_err("a viewport from the old grid cannot start another selection");
    assert_eq!(stale_size_error.public_code(), PublicErrorCode::Conflict);
    let after_stale_size = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read selection after stale viewport rejection");
    assert_eq!(after_stale_size.selection, after_resize.selection);
    assert_eq!(
        manager
            .selected_text(created.session_id.clone())
            .await
            .expect("read confirmed selection after rejection"),
        confirmed_after_resize
    );
    manager
        .close(created.session_id)
        .await
        .expect("close live viewport selection session");
}

/// A stale viewport cannot select replacement text that reused its old row and must preserve the confirmed range.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn ui_selection_viewport_rejects_replaced_row_witness() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, factory, created) = create_live_selection_session(owner.clone()).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read live session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    let displayed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read source row before overwrite");
            if snapshot
                .rows
                .iter()
                .any(|row| row.runs.iter().any(|run| run.text.contains("anchor")))
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial row is rendered");
    let anchor_line = displayed
        .rows
        .iter()
        .find(|row| row.runs.iter().any(|run| run.text.contains("anchor")))
        .expect("find source row")
        .absolute_line;
    let old_viewport = selection_viewport_for_line(&displayed, anchor_line);
    let confirmed = TerminalSelection {
        start: TerminalPoint {
            line: anchor_line,
            column: 0,
        },
        end: TerminalPoint {
            line: anchor_line,
            column: 6,
        },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection(binding.clone(), Some(confirmed), None)
        .await
        .expect("establish a confirmed selection before replacing its row");

    factory.write_output(b"\r\x1b[2Kreplacement").await;
    let replaced = wait_for_render_marker(&manager, &created.session_id, "replacement").await;
    assert!(
        replaced
            .rows
            .iter()
            .any(|row| row.runs.iter().any(|run| run.text.contains("replacement"))),
        "the original row was overwritten before the stale gesture arrives"
    );
    let confirmed_after_overwrite = replaced.selection;
    let confirmed_text_after_overwrite = manager
        .selected_text(created.session_id.clone())
        .await
        .expect("read selection after terminal overwrite");
    let stale_error = manager
        .set_ui_selection_cancellable_in_viewport(
            binding,
            Some(confirmed),
            Some(42),
            old_viewport,
            CancellationToken::new(),
        )
        .await
        .expect_err("row witness prevents a stale gesture from selecting replacement text");
    assert_eq!(stale_error.public_code(), PublicErrorCode::Conflict);
    let after_rejection = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read the confirmed selection after stale viewport rejection");
    assert_eq!(after_rejection.selection, confirmed_after_overwrite);
    assert_eq!(
        manager
            .selected_text(created.session_id.clone())
            .await
            .expect("read selected text after stale viewport rejection"),
        confirmed_text_after_overwrite
    );
    manager
        .close(created.session_id)
        .await
        .expect("close overwritten row selection session");
}

/// A captured row removed from bounded scrollback cannot be resurrected by a late selection start.
///
/// @author kongweiguang
#[tokio::test(flavor = "current_thread")]
async fn ui_selection_viewport_rejects_trimmed_row() {
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let (manager, factory, created) = create_live_selection_session(owner.clone()).await;
    let context = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("read live session generation");
    let binding = UiSessionBinding {
        session_id: created.session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    let displayed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read source row before history trimming");
            if snapshot
                .rows
                .iter()
                .any(|row| row.runs.iter().any(|run| run.text.contains("anchor")))
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial row is rendered");
    let anchor_line = displayed
        .rows
        .iter()
        .find(|row| row.runs.iter().any(|run| run.text.contains("anchor")))
        .expect("find source row")
        .absolute_line;
    let old_viewport = selection_viewport_for_line(&displayed, anchor_line);
    let old_selection = TerminalSelection {
        start: TerminalPoint {
            line: anchor_line,
            column: 0,
        },
        end: TerminalPoint {
            line: anchor_line,
            column: 6,
        },
        kind: TerminalSelectionKind::Character,
    };
    manager
        .set_ui_selection(binding.clone(), Some(old_selection), None)
        .await
        .expect("establish selection before bounded history rolls past it");
    assert_eq!(
        manager
            .set_scrollback_limit(created.session_id.clone(), 2)
            .await
            .expect("bound scrollback for trim regression"),
        2
    );

    let output = (0..12)
        .map(|line| format!("\r\nTRIM-{line:02}"))
        .collect::<String>();
    factory.write_output(output.as_bytes()).await;
    let history = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let history = manager
                .plaintext_snapshot(
                    created.session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 64,
                        max_bytes: Some(16 * 1024),
                    },
                )
                .await
                .expect("read bounded history after output");
            if history.text.contains("TRIM-11") && !history.text.contains("anchor") {
                return history;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the old row is trimmed after the final output marker arrives");
    assert!(history.total_scrollback_lines <= 2);
    let confirmed_after_trim = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("capture the live selection after trimming")
        .selection;
    let selected_text_after_trim = manager
        .selected_text(created.session_id.clone())
        .await
        .expect("read selection after trimming");
    let stale_error = manager
        .set_ui_selection_cancellable_in_viewport(
            binding,
            Some(old_selection),
            Some(43),
            old_viewport,
            CancellationToken::new(),
        )
        .await
        .expect_err("a trimmed snapshot row cannot be selected again");
    assert_eq!(stale_error.public_code(), PublicErrorCode::Conflict);
    let after_rejection = manager
        .render_snapshot(created.session_id.clone(), RenderSnapshotRequest::default())
        .await
        .expect("read selection after trimmed-row rejection");
    assert_eq!(after_rejection.selection, confirmed_after_trim);
    assert_eq!(
        manager
            .selected_text(created.session_id.clone())
            .await
            .expect("read selected text after trimmed-row rejection"),
        selected_text_after_trim
    );
    manager
        .close(created.session_id)
        .await
        .expect("close trimmed row selection session");
}
