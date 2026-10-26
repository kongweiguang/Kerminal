//! 通过生产 SessionManager 验证全选绑定校验和会话生命周期。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::AppResult;
use kerminal_application::sessions::{
    CreateSessionRequest, RenderSnapshotRequest, SessionService, SessionTransport,
    SessionTransportControl, SessionTransportFactory, SessionTransportOpenError, UiSessionBinding,
};
use kerminal_domain::identity::{PaneId, SessionId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionOwner, SessionTarget, TerminalSize,
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct SelectAllFactory {
    peer: Arc<Mutex<Option<DuplexStream>>>,
}

#[async_trait]
impl SessionTransportFactory for SelectAllFactory {
    /// 建立真实 SessionManager reader/writer 路径，并让测试显式控制 PTY 输出与 EOF。
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
        Ok(SessionTransport::new(
            reader,
            writer,
            SelectAllControl {
                peer: Arc::clone(&self.peer),
            },
        ))
    }
}

struct SelectAllControl {
    peer: Arc<Mutex<Option<DuplexStream>>>,
}

#[async_trait]
impl SessionTransportControl for SelectAllControl {
    /// 假 transport 无需调整容量，但保留完整控制端口行为。
    ///
    /// @author kongweiguang
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 仅在真实 close 流程中丢弃 peer，使 manager reader 看见 EOF 并收尾。
    ///
    /// @author kongweiguang
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.peer.lock().await.take();
        Ok(())
    }

    /// 测试会话使用确定退出状态，避免依赖机器上的 shell。
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

/// 创建唯一的可见 UI session，返回生产服务和负责显式释放 reader 的 peer holder。
///
/// @author kongweiguang
async fn create_ui_session() -> (
    Arc<SessionManager>,
    Arc<SelectAllFactory>,
    SessionId,
    SessionOwner,
) {
    let factory = Arc::new(SelectAllFactory {
        peer: Arc::new(Mutex::new(None)),
    });
    let manager = Arc::new(SessionManager::new(
        factory.clone(),
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            owner: owner.clone(),
            size: TerminalSize::new(24, 4, 0, 0).expect("valid test terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create isolated UI session");
    (manager, factory, info.session_id, owner)
}

/// 等待 production reader 把 peer 输出送入活动终端引擎，不依赖可合并 watch 的通知次数。
///
/// @author kongweiguang
async fn wait_until_output(manager: &dyn SessionService, session_id: &SessionId) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager
                .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
                .await
                .expect("read terminal render snapshot");
            if snapshot.rows.iter().any(|row| {
                row.runs
                    .iter()
                    .any(|run| run.text.contains("visible-content"))
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader publishes peer output");
}

/// 精确绑定成功时全选当前 session；陈旧代次或不存在目标都不能改变已有选择或重定向。
/// 无效目标样本复制 owner，保留完整原绑定用于关闭后的拒绝断言，确保生命周期覆盖不被部分移动打断。
///
/// @author kongweiguang
#[tokio::test]
async fn select_all_requires_exact_visible_session_binding() {
    let (manager, factory, session_id, owner) = create_ui_session().await;
    {
        let mut peer = factory.peer.lock().await;
        peer.as_mut()
            .expect("open test PTY peer")
            .write_all(b"visible-content")
            .await
            .expect("send terminal content");
    }
    wait_until_output(manager.as_ref(), &session_id).await;

    let context = manager
        .terminal_command_context(session_id.clone())
        .await
        .expect("read exact runtime generation");
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner,
        session_generation: context.session_generation,
    };
    manager
        .select_all_ui(binding.clone())
        .await
        .expect("select exact visible session");
    let selected = manager
        .selected_text(session_id.clone())
        .await
        .expect("read selected text")
        .expect("non-empty grid has a selection");
    assert!(selected.contains("visible-content"));

    let mut stale = binding.clone();
    stale.session_generation = stale.session_generation.wrapping_add(1);
    assert!(manager.select_all_ui(stale).await.is_err());
    assert_eq!(
        manager.selected_text(session_id.clone()).await.unwrap(),
        Some(selected.clone()),
        "stale generation must not mutate the current selection"
    );

    let missing_target = UiSessionBinding {
        session_id: SessionId::new(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation,
    };
    assert!(manager.select_all_ui(missing_target).await.is_err());
    assert_eq!(
        manager.selected_text(session_id).await.unwrap(),
        Some(selected),
        "an invalid target must not redirect to the only live session"
    );

    manager
        .close_all_until(Instant::now() + Duration::from_secs(5))
        .await
        .expect("close fake session and join reader");
    assert!(manager.select_all_ui(binding).await.is_err());
}
