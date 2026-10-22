//! 运维应用用例的隔离生命周期测试。
//!
//! @author kongweiguang

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    operations::{
        PortForwardDefinition, PortForwardDefinitionRepository, PortForwardRuntime,
        PortForwardRuntimeHandle, PortForwardService,
    },
};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, ForwardState, PortForwardConfig, ProxyApplicationOutcome,
        ProxyApplicationReason, ProxyApplyScope, ProxyProtocol,
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct RuntimeFixture {
    handle: Arc<HandleFixture>,
}

struct LateRuntimeFixture {
    handle: Arc<HandleFixture>,
    entered: Arc<tokio::sync::Notify>,
    cancellation_observed: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl PortForwardRuntime for LateRuntimeFixture {
    /// 模拟底层已创建资源却迟于取消才交回 handle，暴露启动回滚丢 owner 的竞态。
    async fn start(
        &self,
        _id: String,
        _config: PortForwardConfig,
        cancel: CancellationToken,
    ) -> AppResult<Arc<dyn PortForwardRuntimeHandle>> {
        self.entered.notify_one();
        cancel.cancelled().await;
        self.cancellation_observed.notify_one();
        self.release.notified().await;
        Ok(self.handle.clone())
    }
}

struct HandleFixture {
    bind: ForwardEndpoint,
    failure: watch::Sender<Option<PublicErrorCode>>,
    close_count: AtomicUsize,
    fail_first_close: AtomicBool,
    fail_all_closes: AtomicBool,
    received_deadline: Mutex<Option<tokio::time::Instant>>,
}

#[async_trait]
impl PortForwardRuntime for RuntimeFixture {
    /// 测试运行时只生成 watch failure 与关闭计数，不创建真实网络监听器。
    async fn start(
        &self,
        _id: String,
        _config: PortForwardConfig,
        _cancel: CancellationToken,
    ) -> AppResult<Arc<dyn PortForwardRuntimeHandle>> {
        Ok(self.handle.clone())
    }
}

#[async_trait]
impl PortForwardRuntimeHandle for HandleFixture {
    /// 返回夹具中预设的实际绑定端点。
    fn actual_bind(&self) -> ForwardEndpoint {
        self.bind.clone()
    }

    /// 订阅夹具故障状态，验证活动中断可以更新领域聚合。
    fn subscribe_failure(&self) -> watch::Receiver<Option<PublicErrorCode>> {
        self.failure.subscribe()
    }

    /// 计数运行时释放请求以验证停止操作确实关闭底层资源。
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.close_count.fetch_add(1, Ordering::SeqCst);
        *self
            .received_deadline
            .lock()
            .expect("deadline mutex poisoned") = Some(deadline);
        if self.fail_first_close.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("测试 forward 首次释放失败"));
        }
        if self.fail_all_closes.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("测试 forward 释放失败"));
        }
        Ok(())
    }
}

/// 验证活动隧道的断线将状态转为 Failed，随后显式 stop 仍释放句柄。
/// @author kongweiguang
#[tokio::test]
async fn active_forward_failure_remains_visible_and_can_be_stopped() {
    let (failure, _receiver) = watch::channel(None);
    let bind = endpoint(127, 0, 0, 1, 51001);
    let handle = Arc::new(HandleFixture {
        bind: bind.clone(),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture {
        handle: handle.clone(),
    }));
    let snapshot = service
        .start(config(), CancellationToken::new())
        .await
        .expect("fixture runtime starts");
    assert_eq!(snapshot.forward.state, ForwardState::Active);
    assert_eq!(snapshot.actual_bind, bind);

    handle
        .failure
        .send(Some(PublicErrorCode::Infrastructure))
        .expect("service monitor subscribes before failure");
    let failed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(snapshot) = service.get(&snapshot.forward.id).await
                && snapshot.forward.state == ForwardState::Failed
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active failure observed");
    assert_eq!(failed.failure_code, Some(PublicErrorCode::Infrastructure));

    service
        .stop(&failed.forward.id)
        .await
        .expect("failed forward can be stopped");
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 2);
    assert!(service.get(&failed.forward.id).await.is_none());
}

/// 验证异步断线会发布目录 revision，使 UI 无需手动刷新也能重新读取 Failed 状态。
#[tokio::test]
async fn asynchronous_forward_failure_publishes_snapshot_change() {
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 51007),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture {
        handle: handle.clone(),
    }));
    let mut changes = service.subscribe_changes();
    let snapshot = service
        .start(config(), CancellationToken::new())
        .await
        .expect("fixture runtime starts");
    let _ = changes.borrow_and_update();

    handle
        .failure
        .send(Some(PublicErrorCode::Infrastructure))
        .expect("service monitor subscribes before failure");
    tokio::time::timeout(std::time::Duration::from_secs(1), changes.changed())
        .await
        .expect("async failure publishes promptly")
        .expect("service revision feed remains open");

    let current = service
        .get(&snapshot.forward.id)
        .await
        .expect("failed forward remains available for cleanup");
    assert_eq!(current.forward.state, ForwardState::Failed);
    service
        .stop(&snapshot.forward.id)
        .await
        .expect("fixture cleanup succeeds");
}

/// 验证取消请求不会调用底层 runtime，也不会创建活动实例。
#[tokio::test]
async fn cancelled_start_is_rejected_before_runtime_creation() {
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 51002),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture {
        handle: handle.clone(),
    }));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = service.start(config(), cancel).await;
    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert!(service.list().await.is_empty());
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 0);
}

/// 退出关闭并发转发时逐个尝试资源；一次失败保留实例供下一次 shutdown 重试。
#[tokio::test]
async fn shutdown_continues_closing_forwards_after_one_failure() {
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 51003),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(true),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture {
        handle: handle.clone(),
    }));
    service
        .start(config(), CancellationToken::new())
        .await
        .expect("第一个 fixture forward 已启动");
    service
        .start(config(), CancellationToken::new())
        .await
        .expect("第二个 fixture forward 已启动");

    let first_shutdown = service
        .shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await;
    assert!(first_shutdown.is_err());
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 2);
    assert_eq!(service.list().await.len(), 1);
    assert!(matches!(
        service.start(config(), CancellationToken::new()).await,
        Err(AppError::Cancelled { .. })
    ));

    service
        .shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .expect("残留 failed forward 可再次释放");
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 3);
    assert!(service.list().await.is_empty());
}

/// 验证并发 forward 清理都失败时，退出结果保留后续失败作为诊断来源。
#[tokio::test]
async fn shutdown_retains_all_concurrent_close_failures() {
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 51006),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(true),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture {
        handle: handle.clone(),
    }));
    service
        .start(config(), CancellationToken::new())
        .await
        .expect("第一个 fixture forward 已启动");
    service
        .start(config(), CancellationToken::new())
        .await
        .expect("第二个 fixture forward 已启动");

    let error = service
        .shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .expect_err("两个 forward 的资源关闭均失败");

    assert_eq!(handle.close_count.load(Ordering::SeqCst), 2);
    let source = std::error::Error::source(&error).expect("保留第二个关闭失败");
    assert!(source.downcast_ref::<AppError>().is_some());
}

/// 验证退出清理把 composition root 的同一个绝对 deadline 原样交给运行时资源。
#[tokio::test]
async fn shutdown_forwards_the_shared_absolute_deadline() {
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 51004),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture {
        handle: handle.clone(),
    }));
    service
        .start(config(), CancellationToken::new())
        .await
        .expect("fixture forward starts");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    service
        .shutdown_until(deadline)
        .await
        .expect("forward closes within the shared deadline");

    assert_eq!(
        *handle
            .received_deadline
            .lock()
            .expect("deadline mutex poisoned"),
        Some(deadline)
    );
}

/// 动态转发只返回可用 URL 和未应用原因，不把请求范围伪装成终端环境变更。
#[tokio::test]
async fn dynamic_forward_exposes_proxy_url_without_claiming_application() {
    let (failure, _receiver) = watch::channel(None);
    let bind = ForwardEndpoint::new("0.0.0.0", 51005).expect("valid wildcard endpoint");
    let handle = Arc::new(HandleFixture {
        bind,
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let service = PortForwardService::new(Arc::new(RuntimeFixture { handle }));
    let mut config = config();
    config.name = "dynamic test".to_owned();
    config.kind = ForwardKind::Dynamic;
    config.bind = endpoint(0, 0, 0, 0, 51005);
    config.destination = None;
    config.proxy_protocol = Some(ProxyProtocol::Socks5);
    config.proxy_apply_scope = ProxyApplyScope::ToolOnly;

    let snapshot = service
        .start(config, CancellationToken::new())
        .await
        .expect("dynamic forward starts");

    assert_eq!(
        snapshot.proxy_url.as_deref(),
        Some("socks5h://127.0.0.1:51005")
    );
    assert_eq!(
        snapshot.proxy_application,
        ProxyApplicationOutcome {
            scope: ProxyApplyScope::ToolOnly,
            applied: false,
            reason: ProxyApplicationReason::CallerMustApplyProxyUrl,
        }
    );
    service
        .stop(&snapshot.forward.id)
        .await
        .expect("dynamic forward closes");
}

/// 构造固定本机转发配置，测试值不引用用户配置目录或外部服务。
fn config() -> PortForwardConfig {
    PortForwardConfig {
        host_id: HostId::new(),
        name: "test-forward".to_owned(),
        kind: ForwardKind::Local,
        bind: endpoint(127, 0, 0, 1, 51001),
        destination: Some(endpoint(127, 0, 0, 1, 5432)),
        proxy_protocol: None,
        remote_access_scope: None,
        proxy_apply_scope: ProxyApplyScope::None,
    }
}

/// 构造测试 IPv4 endpoint，失败代表测试常量本身无效。
fn endpoint(a: u8, b: u8, c: u8, d: u8, port: u16) -> ForwardEndpoint {
    ForwardEndpoint::new(format!("{a}.{b}.{c}.{d}"), port).expect("valid test endpoint")
}

#[derive(Default)]
struct DefinitionsFixture(Mutex<Vec<PortForwardDefinition>>, AtomicBool);

#[async_trait]
impl PortForwardDefinitionRepository for DefinitionsFixture {
    /// 仓储夹具只复制当前定义，不创建任何隧道或文件。
    async fn list(&self) -> AppResult<Vec<PortForwardDefinition>> {
        Ok(self.0.lock().unwrap().clone())
    }

    /// 与真实仓储一致拒绝重复 ID，防止测试无条件 upsert 掩盖应用层缺陷。
    async fn create(&self, definition: PortForwardDefinition) -> AppResult<()> {
        let mut definitions = self.0.lock().unwrap();
        if definitions.iter().any(|item| item.id == definition.id) {
            return Err(AppError::conflict("duplicate definition"));
        }
        definitions.push(definition);
        if self.1.swap(false, Ordering::SeqCst) {
            return Err(AppError::outcome_unknown("fixture commit receipt lost"));
        }
        Ok(())
    }

    /// 更新只允许配置基线完全相等，不接受表单持有的过期副本。
    async fn update(
        &self,
        expected: PortForwardDefinition,
        definition: PortForwardDefinition,
    ) -> AppResult<()> {
        let mut definitions = self.0.lock().unwrap();
        let item = definitions
            .iter_mut()
            .find(|item| item.id == expected.id)
            .ok_or_else(|| AppError::conflict("missing definition"))?;
        if *item != expected {
            return Err(AppError::conflict("stale definition"));
        }
        *item = definition;
        Ok(())
    }

    /// 精确删除基线，缺失和冲突具有不同含义。
    async fn delete(&self, expected: PortForwardDefinition) -> AppResult<bool> {
        let mut definitions = self.0.lock().unwrap();
        let Some(index) = definitions.iter().position(|item| item.id == expected.id) else {
            return Ok(false);
        };
        if definitions[index] != expected {
            return Err(AppError::conflict("stale definition"));
        }
        definitions.remove(index);
        Ok(true)
    }
}

/// 保存本身不启动；停止后定义保留，重新装配同一仓储只恢复停止态而不恢复 listener。
#[tokio::test]
async fn saved_forward_lifecycle_keeps_stable_identity_without_auto_start() {
    let definitions = Arc::new(DefinitionsFixture::default());
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 55001),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(false),
        fail_all_closes: AtomicBool::new(false),
        received_deadline: Mutex::new(None),
    });
    let runtime = Arc::new(RuntimeFixture {
        handle: handle.clone(),
    });
    let service =
        PortForwardService::new(runtime.clone()).with_definition_repository(definitions.clone());
    let saved = service.create_definition(config()).await.unwrap();
    let rows = service.list_managed().await.unwrap();
    assert_eq!(rows[0].forward.id, saved.id);
    assert_eq!(rows[0].forward.state, ForwardState::Stopped);
    assert!(rows[0].actual_bind.is_none());
    assert!(service.list().await.is_empty());
    let active = service
        .start_definition(&saved.id, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(active.forward.id, saved.id);
    assert_eq!(
        service
            .update_definition(saved.clone(), saved.config.clone())
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Conflict
    );
    assert_eq!(
        service
            .delete_definition(saved.clone())
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Conflict
    );
    service.stop(&saved.id).await.unwrap();
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 1);
    let restarted = PortForwardService::new(runtime).with_definition_repository(definitions);
    let rows = restarted.list_managed().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].forward.state, ForwardState::Stopped);
    assert!(rows[0].actual_bind.is_none());
    restarted.delete_definition(saved).await.unwrap();
    assert!(restarted.list_managed().await.unwrap().is_empty());
}

/// 提交已落盘但回执丢失时，原 ID 只读核对能找到原事务，不创建第二条定义或启动隧道。
#[tokio::test]
async fn unknown_definition_save_can_be_reviewed_by_original_id_without_replay() {
    let definitions = Arc::new(DefinitionsFixture::default());
    definitions.1.store(true, Ordering::SeqCst);
    let (failure, _receiver) = watch::channel(None);
    let runtime = Arc::new(RuntimeFixture {
        handle: Arc::new(HandleFixture {
            bind: endpoint(127, 0, 0, 1, 55001),
            failure,
            close_count: AtomicUsize::new(0),
            fail_first_close: AtomicBool::new(false),
            fail_all_closes: AtomicBool::new(false),
            received_deadline: Mutex::new(None),
        }),
    });
    let service = PortForwardService::new(runtime).with_definition_repository(definitions);
    let id = "original-save-transaction".to_owned();
    let config = config();
    let error = service
        .create_definition_with_id(id.clone(), config.clone())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    let saved = service.get_definition(&id).await.unwrap().unwrap();
    assert_eq!(saved.id, id);
    assert_eq!(saved.config, config);
    assert_eq!(service.list_managed().await.unwrap().len(), 1);
    assert!(service.list().await.is_empty());
}

/// 取消后的首次清理失败保留运行记录及稳定 ID，后续显式 stop 释放的是同一个 handle。
#[tokio::test]
async fn cancelled_late_start_retains_owner_when_first_close_fails() {
    let (service, handle, entered, observed, release) = late_runtime(false);
    let cancellation = CancellationToken::new();
    let worker = {
        let service = service.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move { service.start(config(), cancellation).await })
    };
    entered.notified().await;
    cancellation.cancel();
    observed.notified().await;
    release.notify_one();
    assert!(worker.await.unwrap().is_err());
    let rows = service.list().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].forward.state, ForwardState::Failed);
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 1);
    service.stop(&rows[0].forward.id).await.unwrap();
    assert!(service.list().await.is_empty());
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 2);
}

/// 退出等待启动结束时仍扫描迟到 owner；所有关闭失败则保留记录供下一次 shutdown 重试。
#[tokio::test]
async fn shutdown_tracks_late_cancelled_owner_and_keeps_failed_cleanup_retryable() {
    let (service, handle, entered, observed, release) = late_runtime(true);
    let start = {
        let service = service.clone();
        tokio::spawn(async move { service.start(config(), CancellationToken::new()).await })
    };
    entered.notified().await;
    let shutdown = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
                .await
        })
    };
    observed.notified().await;
    release.notify_one();
    assert!(start.await.unwrap().is_err());
    assert!(shutdown.await.unwrap().is_err());
    let rows = service.list().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].forward.state, ForwardState::Failed);
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 2);
    handle.fail_all_closes.store(false, Ordering::SeqCst);
    service
        .shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .unwrap();
    assert!(service.list().await.is_empty());
    assert_eq!(handle.close_count.load(Ordering::SeqCst), 3);
}

type LateRuntime = (
    PortForwardService,
    Arc<HandleFixture>,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
);

/// 无外部进程的门控夹具，把取消、迟到 handle 和清理失败按确定顺序交给用例。
fn late_runtime(fail_all_closes: bool) -> LateRuntime {
    let (failure, _receiver) = watch::channel(None);
    let handle = Arc::new(HandleFixture {
        bind: endpoint(127, 0, 0, 1, 55002),
        failure,
        close_count: AtomicUsize::new(0),
        fail_first_close: AtomicBool::new(true),
        fail_all_closes: AtomicBool::new(fail_all_closes),
        received_deadline: Mutex::new(None),
    });
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let service = PortForwardService::new(Arc::new(LateRuntimeFixture {
        handle: handle.clone(),
        entered: entered.clone(),
        cancellation_observed: observed.clone(),
        release: release.clone(),
    }));
    (service, handle, entered, observed, release)
}
