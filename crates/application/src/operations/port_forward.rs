//! 端口转发用例协调领域状态与独立隧道运行时资源。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::operations::{
    ForwardEndpoint, ForwardKind, ForwardState, PortForward, PortForwardConfig,
    ProxyApplicationOutcome,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::terminal_proxy::FutureProxyPolicyStore;
use crate::{AppError, AppResult, PublicErrorCode};

const STOP_TIMEOUT: Duration = Duration::from_secs(6);

/// 已启动的端口转发视图同时保留配置、实际绑定端点与稳定失败码。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortForwardSnapshot {
    pub forward: PortForward,
    pub actual_bind: ForwardEndpoint,
    pub proxy_url: Option<String>,
    pub proxy_application: ProxyApplicationOutcome,
    pub failure_code: Option<PublicErrorCode>,
}

/// 用户保存的转发定义只持有稳定 ID 与经过领域校验的配置，不保存任何运行资源。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortForwardDefinition {
    pub id: String,
    pub config: PortForwardConfig,
}

impl PortForwardDefinition {
    /// 仓储边界复核稳定标识和领域配置，防止从磁盘绕过新建/编辑用例写入非法定义。
    pub fn validate(&self) -> AppResult<()> {
        validate_definition_id(&self.id)?;
        validate_forward_config(&self.config)
    }
}

/// 定义仓储以原子文档保存转发配置；缺失文档可视为空，损坏文档必须作为错误返回。
#[async_trait]
pub trait PortForwardDefinitionRepository: Send + Sync + 'static {
    /// 读取全部当前格式定义；实现不得把解析失败降级为空列表。
    async fn list(&self) -> AppResult<Vec<PortForwardDefinition>>;

    /// 仅在稳定 ID 尚不存在时新增，重复 ID 不能覆盖其它窗口刚保存的定义。
    async fn create(&self, definition: PortForwardDefinition) -> AppResult<()>;

    /// 只替换与编辑基线完全一致的定义，避免长时间打开的表单覆盖外部修改。
    async fn update(
        &self,
        expected: PortForwardDefinition,
        definition: PortForwardDefinition,
    ) -> AppResult<()>;

    /// 删除同一配置基线的精确 ID；配置已变化时保留新内容并返回冲突。
    async fn delete(&self, expected: PortForwardDefinition) -> AppResult<bool>;
}

/// 工作台转发清单把持久定义与运行实例合并，停止定义的实际监听地址保持为空。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedPortForwardSnapshot {
    pub forward: PortForward,
    pub persistent: bool,
    pub actual_bind: Option<ForwardEndpoint>,
    pub proxy_url: Option<String>,
    pub proxy_application: Option<ProxyApplicationOutcome>,
    pub failure_code: Option<PublicErrorCode>,
}

/// 转发数据面资源由 infra 独占，应用层只观察失败并请求幂等关闭。
#[async_trait]
pub trait PortForwardRuntimeHandle: Send + Sync + 'static {
    /// 返回操作系统或 SSH server 确认的监听 endpoint。
    fn actual_bind(&self) -> ForwardEndpoint;

    /// 订阅隧道、listener 或共享连接故障，允许活动实例及时进入 Failed。
    fn subscribe_failure(&self) -> watch::Receiver<Option<PublicErrorCode>>;

    /// 按 composition root 的绝对 deadline 释放本转发 listener、活动 channel 与独立 SSH lease。
    async fn close_until(&self, deadline: Instant) -> AppResult<()>;
}

/// 按四类 ForwardKind 建立数据面；端口转发服务不直接绑定 socket 或实现 SOCKS5。
#[async_trait]
pub trait PortForwardRuntime: Send + Sync + 'static {
    /// 创建独立转发资源；失败时 adapter 必须回收尚未交付的临时监听器。
    async fn start(
        &self,
        id: String,
        config: PortForwardConfig,
        cancel: CancellationToken,
    ) -> AppResult<Arc<dyn PortForwardRuntimeHandle>>;
}

struct PortForwardRecord {
    forward: Mutex<PortForward>,
    runtime: Arc<dyn PortForwardRuntimeHandle>,
    failure_code: Mutex<Option<PublicErrorCode>>,
    stop_lock: Mutex<()>,
    monitor_cancel: CancellationToken,
    monitor_task: StdMutex<Option<JoinHandle<()>>>,
    future_proxy_policy: FutureProxyPolicyStore,
    changes: watch::Sender<u64>,
}

struct ForwardLifecycle {
    accepting: bool,
    starts: HashMap<String, CancellationToken>,
    shutdown_deadline: Option<Instant>,
}

/// 运行态转发目录；句柄按稳定 ID 关闭，网络故障通过 service 快照可见。
#[derive(Clone)]
pub struct PortForwardService {
    runtime: Arc<dyn PortForwardRuntime>,
    records: Arc<StdMutex<HashMap<String, Arc<PortForwardRecord>>>>,
    lifecycle: Arc<StdMutex<ForwardLifecycle>>,
    start_count: watch::Sender<usize>,
    changes: watch::Sender<u64>,
    future_proxy_policy: FutureProxyPolicyStore,
    definitions: Option<Arc<dyn PortForwardDefinitionRepository>>,
    definition_lock: Arc<Mutex<()>>,
}

impl PortForwardService {
    /// 注入单一数据面适配器并创建本地策略存储，退出时可取消尚未发布的启动和后续代理尝试。
    pub fn new(runtime: Arc<dyn PortForwardRuntime>) -> Self {
        let (start_count, _) = watch::channel(0);
        let (changes, _) = watch::channel(0);
        Self {
            runtime,
            records: Arc::new(StdMutex::new(HashMap::new())),
            lifecycle: Arc::new(StdMutex::new(ForwardLifecycle {
                accepting: true,
                starts: HashMap::new(),
                shutdown_deadline: None,
            })),
            start_count,
            changes,
            future_proxy_policy: FutureProxyPolicyStore::new(),
            definitions: None,
            definition_lock: Arc::new(Mutex::new(())),
        }
    }

    /// 注入桌面专用定义仓储；MCP 直接启动能力仍可使用无持久定义的原始 runtime 服务。
    pub fn with_definition_repository(
        mut self,
        repository: Arc<dyn PortForwardDefinitionRepository>,
    ) -> Self {
        self.definitions = Some(repository);
        self
    }

    /// 让转发生命周期与代理用例共享同一策略存储，不创建平行订阅或第二份策略表。
    pub fn with_future_proxy_policy_store(mut self, store: FutureProxyPolicyStore) -> Self {
        self.future_proxy_policy = store;
        self
    }

    /// 返回本服务使用的共享策略存储，组合根可将同一实例注入代理用例。
    pub fn future_proxy_policy_store(&self) -> FutureProxyPolicyStore {
        self.future_proxy_policy.clone()
    }

    /// 订阅运行目录 revision；通知只表示快照可能变化，调用方应重新读取完整列表。
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// 先登记可取消的启动，再发布成功创建的 handle，避免退出快照漏掉并发启动。
    pub async fn start(
        &self,
        config: PortForwardConfig,
        cancel: CancellationToken,
    ) -> AppResult<PortForwardSnapshot> {
        let id = uuid::Uuid::new_v4().to_string();
        self.start_with_id(id, config, cancel).await
    }

    /// 运行定义使用其稳定 ID 作为 runtime owner，因而停止、代理策略和重启后的再次启动都指向同一条记录。
    pub async fn start_definition(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> AppResult<PortForwardSnapshot> {
        let _definition_guard = self.definition_lock.lock().await;
        let definitions = self.definition_repository()?;
        let definition = definitions
            .list()
            .await?
            .into_iter()
            .find(|definition| definition.id == id)
            .ok_or_else(|| AppError::not_found("端口转发定义不存在"))?;
        self.start_with_id(definition.id, definition.config, cancel)
            .await
    }

    /// 新建保存定义；单一应用门禁串行化校验与写入，避免两个同时创建覆盖同一文档。
    pub async fn create_definition(
        &self,
        config: PortForwardConfig,
    ) -> AppResult<PortForwardDefinition> {
        let id = uuid::Uuid::new_v4().to_string();
        self.create_definition_with_id(id, config).await
    }

    /// UI 在提交前保留稳定 ID，未知回执后可只核对该条原事务，不重复创建另一份配置。
    pub async fn create_definition_with_id(
        &self,
        id: String,
        config: PortForwardConfig,
    ) -> AppResult<PortForwardDefinition> {
        let definition = PortForwardDefinition { id, config };
        definition.validate()?;
        let _definition_guard = self.definition_lock.lock().await;
        let repository = self.definition_repository()?;
        repository.create(definition.clone()).await?;
        publish_forward_change(&self.changes);
        Ok(definition)
    }

    /// 未知提交只回读原 ID；既不重放写入也不隐式启动 listener。
    pub async fn get_definition(&self, id: &str) -> AppResult<Option<PortForwardDefinition>> {
        validate_definition_id(id)?;
        let _definition_guard = self.definition_lock.lock().await;
        let definition = self
            .definition_repository()?
            .list()
            .await?
            .into_iter()
            .find(|definition| definition.id == id);
        if let Some(definition) = definition.as_ref() {
            definition.validate()?;
        }
        Ok(definition)
    }

    /// 编辑前拒绝运行中实例并核对原定义仍存在，避免表单迟到回执覆盖另一份配置。
    pub async fn update_definition(
        &self,
        expected: PortForwardDefinition,
        config: PortForwardConfig,
    ) -> AppResult<PortForwardDefinition> {
        validate_forward_config(&config)?;
        expected.validate()?;
        if expected.config.host_id != config.host_id {
            return Err(AppError::invalid("编辑转发不能隐式改变 SSH 主机"));
        }
        let _definition_guard = self.definition_lock.lock().await;
        if lock_recover(&self.records).contains_key(&expected.id) {
            return Err(AppError::conflict("运行中的端口转发不能编辑定义"));
        }
        let repository = self.definition_repository()?;
        let definition = PortForwardDefinition {
            id: expected.id.clone(),
            config,
        };
        repository.update(expected, definition.clone()).await?;
        publish_forward_change(&self.changes);
        Ok(definition)
    }

    /// 删除只移除已停止的配置；运行资源必须先经 stop 确认回收。
    pub async fn delete_definition(&self, expected: PortForwardDefinition) -> AppResult<()> {
        expected.validate()?;
        let _definition_guard = self.definition_lock.lock().await;
        if lock_recover(&self.records).contains_key(&expected.id) {
            return Err(AppError::conflict("请先停止端口转发，再删除定义"));
        }
        let repository = self.definition_repository()?;
        if !repository.delete(expected).await? {
            return Err(AppError::not_found("端口转发定义不存在"));
        }
        publish_forward_change(&self.changes);
        Ok(())
    }

    /// 工作台读取定义和活动实例的并集，停止定义不伪造 runtime endpoint。
    pub async fn list_managed(&self) -> AppResult<Vec<ManagedPortForwardSnapshot>> {
        let definitions = self.definition_repository()?.list().await?;
        let mut items = HashMap::with_capacity(definitions.len());
        let mut definition_ids = std::collections::HashSet::with_capacity(definitions.len());
        for definition in definitions {
            definition
                .validate()
                .map_err(|_| AppError::configuration_corrupt("端口转发定义"))?;
            if !definition_ids.insert(definition.id.clone()) {
                return Err(AppError::configuration_corrupt("端口转发定义"));
            }
            let forward = PortForward {
                id: definition.id,
                config: definition.config,
                state: ForwardState::Stopped,
            };
            items.insert(
                forward.id.clone(),
                ManagedPortForwardSnapshot {
                    forward,
                    persistent: true,
                    actual_bind: None,
                    proxy_url: None,
                    proxy_application: None,
                    failure_code: None,
                },
            );
        }
        for snapshot in self.list().await {
            let id = snapshot.forward.id.clone();
            let persistent = items.contains_key(&id);
            items.insert(
                id,
                ManagedPortForwardSnapshot {
                    forward: snapshot.forward,
                    persistent,
                    actual_bind: Some(snapshot.actual_bind),
                    proxy_url: snapshot.proxy_url,
                    proxy_application: Some(snapshot.proxy_application),
                    failure_code: snapshot.failure_code,
                },
            );
        }
        let mut items: Vec<_> = items.into_values().collect();
        items.sort_by(|left, right| {
            left.forward
                .config
                .name
                .cmp(&right.forward.config.name)
                .then_with(|| left.forward.id.cmp(&right.forward.id))
        });
        Ok(items)
    }

    /// 运行 handle 一旦产生就先登记所有权；取消和退出只改变回执，清理失败仍保留同一 ID 可重试。
    async fn start_with_id(
        &self,
        id: String,
        config: PortForwardConfig,
        cancel: CancellationToken,
    ) -> AppResult<PortForwardSnapshot> {
        validate_definition_id(&id)?;
        validate_forward_config(&config)?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("端口转发启动已取消"));
        }

        let operation_cancel = cancel.child_token();
        {
            let mut lifecycle = lock_recover(&self.lifecycle);
            if !lifecycle.accepting {
                return Err(AppError::cancelled("应用正在退出，已停止接收端口转发"));
            }
            if lifecycle.starts.contains_key(&id) || lock_recover(&self.records).contains_key(&id) {
                return Err(AppError::conflict("同一端口转发定义正在运行或启动"));
            }
            lifecycle
                .starts
                .insert(id.clone(), operation_cancel.clone());
            self.start_count.send_replace(lifecycle.starts.len());
        }
        let _permit = StartPermit {
            id: id.clone(),
            lifecycle: self.lifecycle.clone(),
            start_count: self.start_count.clone(),
        };

        let forward = PortForward::starting(id.clone(), config.clone());
        let runtime = self
            .runtime
            .start(id.clone(), config, operation_cancel.clone())
            .await?;
        let record = Arc::new(PortForwardRecord {
            forward: Mutex::new(forward),
            runtime,
            failure_code: Mutex::new(None),
            stop_lock: Mutex::new(()),
            monitor_cancel: CancellationToken::new(),
            monitor_task: StdMutex::new(None),
            future_proxy_policy: self.future_proxy_policy.clone(),
            changes: self.changes.clone(),
        });
        // 同一 ID 从 intake 到本函数返回一直被 starts 保留；只有当前启动可发布，stop 只会移除。
        lock_recover(&self.records).insert(id.clone(), record.clone());
        let monitor = spawn_failure_monitor(record.clone());
        *lock_recover(&record.monitor_task) = Some(monitor);
        publish_forward_change(&self.changes);

        let activation = {
            let mut forward = record.forward.lock().await;
            let lifecycle = lock_recover(&self.lifecycle);
            if !lifecycle.accepting || operation_cancel.is_cancelled() {
                Err(AppError::cancelled("端口转发启动已取消"))
            } else {
                forward
                    .activate()
                    .map_err(|_| AppError::conflict("端口转发在启动完成前已停止或失败"))
            }
        };
        if let Err(error) = activation {
            let deadline = lock_recover(&self.lifecycle)
                .shutdown_deadline
                .map_or(Instant::now() + STOP_TIMEOUT, |deadline| {
                    deadline.min(Instant::now() + STOP_TIMEOUT)
                });
            self.stop_record_until(&id, &record, deadline).await?;
            return Err(error);
        }
        publish_forward_change(&self.changes);

        self.get(&id)
            .await
            .ok_or_else(|| AppError::infrastructure("端口转发启动后未能读取状态"))
    }

    /// 无持久仓储时明确拒绝定义操作，避免 UI 把不可保存状态展示成空列表。
    fn definition_repository(&self) -> AppResult<Arc<dyn PortForwardDefinitionRepository>> {
        self.definitions
            .clone()
            .ok_or_else(|| AppError::unsupported("端口转发定义存储未配置"))
    }

    /// 列出当前仍有资源记录的转发，失败实例保留以便用户查看并再次释放。
    pub async fn list(&self) -> Vec<PortForwardSnapshot> {
        let records: Vec<_> = lock_recover(&self.records).values().cloned().collect();
        let mut snapshots = Vec::with_capacity(records.len());
        for record in records {
            snapshots.push(snapshot_record(&record).await);
        }
        snapshots.sort_by(|left, right| left.forward.id.cmp(&right.forward.id));
        snapshots
    }

    /// 查询指定实例；未知 ID 硬失败，不回退到当前或任意其他转发。
    pub async fn get(&self, id: &str) -> Option<PortForwardSnapshot> {
        let record = lock_recover(&self.records).get(id).cloned()?;
        Some(snapshot_record(&record).await)
    }

    /// 关闭指定实例；固定清理预算避免网络关闭等待无限延长用户操作。
    pub async fn stop(&self, id: &str) -> AppResult<()> {
        self.stop_until(id, Instant::now() + STOP_TIMEOUT).await
    }

    /// 同步关闭转发 intake、后续代理策略和未发布启动，让组合根可先 quiesce 再并行 join。
    pub fn begin_shutdown(&self, deadline: Instant) {
        self.future_proxy_policy.cancel_all();
        let mut lifecycle = lock_recover(&self.lifecycle);
        lifecycle.accepting = false;
        lifecycle.shutdown_deadline = Some(
            lifecycle
                .shutdown_deadline
                .map_or(deadline, |current| current.min(deadline)),
        );
        for cancel in lifecycle.starts.values() {
            cancel.cancel();
        }
        self.start_count.send_replace(lifecycle.starts.len());
    }

    /// 退出期间反复收集新发布 owner，每个 owner 只尝试一次，不能漏掉取消启动后迟到的失败资源。
    pub async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        self.begin_shutdown(deadline);
        let mut starts = self.start_count.subscribe();

        let mut stops = JoinSet::new();
        let mut scheduled: HashSet<String> = HashSet::new();
        let mut first_error = None;
        loop {
            let records: Vec<(String, Arc<PortForwardRecord>)> = lock_recover(&self.records)
                .iter()
                .filter_map(|(id, record)| {
                    if scheduled.insert(id.clone()) {
                        Some((id.clone(), Arc::clone(record)))
                    } else {
                        None
                    }
                })
                .collect();
            for (id, record) in records {
                let service = self.clone();
                stops.spawn(async move { service.stop_record_until(&id, &record, deadline).await });
            }
            let has_starts = *starts.borrow_and_update() > 0;
            if stops.is_empty() && !has_starts {
                // 最后一个 start 可能在本轮收集之后才发布失败 owner；计数归零后再扫一次才能宣告完成。
                if lock_recover(&self.records)
                    .keys()
                    .any(|id| !scheduled.contains(id))
                {
                    continue;
                }
                break;
            }
            if !stops.is_empty() {
                match stops.join_next().await {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => {
                        retain_cleanup_error(&mut first_error, error);
                    }
                    Some(Err(error)) => {
                        retain_cleanup_error(
                            &mut first_error,
                            AppError::infrastructure_with_source("端口转发清理任务异常结束", error),
                        );
                    }
                    None => {}
                }
                continue;
            }
            tokio::select! {
                _ = sleep_until(deadline) => {
                    return Err(aggregate_cleanup_error(
                        first_error.take(),
                        AppError::timeout("端口转发退出清理"),
                    ));
                }
                changed = starts.changed(), if has_starts => {
                    if changed.is_err() {
                        retain_cleanup_error(
                            &mut first_error,
                            AppError::infrastructure("端口转发启动状态订阅已关闭"),
                        );
                    }
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    /// 公开停止先捕获精确 owner，再进入有界关闭，稳定 ID 的下一代运行不能被迟到 stop 移除。
    async fn stop_until(&self, id: &str, deadline: Instant) -> AppResult<()> {
        self.future_proxy_policy.cancel_by_forward(id);
        let record = lock_recover(&self.records)
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::not_found("端口转发不存在"))?;
        self.stop_record_until(id, &record, deadline).await
    }

    /// 取消、退出和显式关闭共用同一清理 owner；超时或 watcher 失败都保留记录，不冒充已释放。
    /// @author kongweiguang
    async fn stop_record_until(
        &self,
        id: &str,
        record: &Arc<PortForwardRecord>,
        deadline: Instant,
    ) -> AppResult<()> {
        self.future_proxy_policy.cancel_by_forward(id);
        let _stop_guard = timeout_at(deadline, record.stop_lock.lock())
            .await
            .map_err(|_| AppError::timeout("等待端口转发清理所有权"))?;
        {
            let mut forward = record.forward.lock().await;
            if forward.state == ForwardState::Stopped {
                drop(forward);
                cancel_failure_monitor_until(record, deadline).await?;
                self.remove_record_if_owned(id, record);
                return Ok(());
            }
            if forward.state != ForwardState::Closing {
                forward
                    .begin_close()
                    .map_err(|_| AppError::conflict("端口转发不能进入关闭状态"))?;
            }
        }
        publish_forward_change(&record.changes);

        let close_result = match timeout_at(deadline, record.runtime.close_until(deadline)).await {
            Ok(result) => result,
            Err(_) => Err(AppError::timeout("端口转发资源清理")),
        };
        match close_result {
            Ok(()) => {
                if let Err(error) = cancel_failure_monitor_until(record, deadline).await {
                    mark_forward_failed(record, error.public_code()).await?;
                    return Err(error);
                }
                let mut forward = record.forward.lock().await;
                if forward.state == ForwardState::Failed {
                    forward
                        .begin_close()
                        .map_err(|_| AppError::conflict("失败转发不能进入最终关闭状态"))?;
                }
                forward
                    .stop()
                    .map_err(|_| AppError::conflict("端口转发不能完成关闭"))?;
                drop(forward);
                self.remove_record_if_owned(id, record);
                Ok(())
            }
            Err(error) => {
                let mut cleanup_error = error;
                if let Err(state_error) =
                    mark_forward_failed(record, cleanup_error.public_code()).await
                {
                    cleanup_error = cleanup_error.with_source(state_error);
                }
                if let Err(monitor_error) = cancel_failure_monitor_until(record, deadline).await {
                    cleanup_error = cleanup_error.with_source(monitor_error);
                }
                Err(cleanup_error)
            }
        }
    }

    /// 同一稳定定义可再次启动；迟到旧关闭仅能移除自己持有的 Arc，不能删除新一代 owner。
    /// @author kongweiguang
    fn remove_record_if_owned(&self, id: &str, record: &Arc<PortForwardRecord>) {
        let mut records = lock_recover(&self.records);
        if records
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, record))
        {
            records.remove(id);
            publish_forward_change(&self.changes);
        }
    }
}

/// 转发配置从 UI、MCP 与磁盘进入同一领域校验，避免持久化路径放宽运行规则。
fn validate_forward_config(config: &PortForwardConfig) -> AppResult<()> {
    config
        .validate()
        .map_err(|_| AppError::invalid("端口转发配置无效"))?;
    if !is_valid_endpoint(&config.bind)
        || config
            .destination
            .as_ref()
            .is_some_and(|endpoint| !is_valid_endpoint(endpoint))
    {
        return Err(AppError::invalid("端口转发端点无效"));
    }
    Ok(())
}

/// 文件反序列化不会经过 endpoint 构造器，因此在应用边界复核并要求主机已规范化。
fn is_valid_endpoint(endpoint: &ForwardEndpoint) -> bool {
    ForwardEndpoint::new(endpoint.host.clone(), endpoint.port.get())
        .is_ok_and(|normalized| normalized == *endpoint)
}

/// 定义 ID 只用于文档内稳定关联，仍限制为安全短标识以避免跨边界歧义。
fn validate_definition_id(id: &str) -> AppResult<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AppError::invalid("端口转发定义标识无效"));
    }
    Ok(())
}

/// 合并清理故障时保留首个公开错误类别，并把后续错误串入内部诊断链。
fn aggregate_cleanup_error(primary: Option<AppError>, next: AppError) -> AppError {
    match primary {
        Some(primary) => primary.with_source(next),
        None => next,
    }
}

/// 收集并发清理结果，确保 shutdown 不因先完成的一个错误丢失其他失败证据。
fn retain_cleanup_error(errors: &mut Option<AppError>, next: AppError) {
    *errors = Some(aggregate_cleanup_error(errors.take(), next));
}

struct StartPermit {
    id: String,
    lifecycle: Arc<StdMutex<ForwardLifecycle>>,
    start_count: watch::Sender<usize>,
}

/// 增加运行目录版本号；watch 只合并通知，读者始终重新读取完整快照。
fn publish_forward_change(changes: &watch::Sender<u64>) {
    changes.send_modify(|revision| *revision = revision.wrapping_add(1));
}

impl Drop for StartPermit {
    /// 退出启动调用时更新计数，使应用关闭能确认所有启动都已结束或回滚。
    fn drop(&mut self) {
        let mut lifecycle = lock_recover(&self.lifecycle);
        lifecycle.starts.remove(&self.id);
        self.start_count.send_replace(lifecycle.starts.len());
    }
}

/// 监听数据面失败并更新领域状态；资源关闭仍由 stop/shutdown 重试。
fn spawn_failure_monitor(record: Arc<PortForwardRecord>) -> JoinHandle<()> {
    let mut failures = record.runtime.subscribe_failure();
    let cancel = record.monitor_cancel.clone();
    tokio::spawn(async move {
        loop {
            let failure = *failures.borrow_and_update();
            if let Some(code) = failure {
                observe_failure_and_close(&record, code).await;
                return;
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                changed = failures.changed() => {
                    if changed.is_err() {
                        observe_failure_and_close(&record, PublicErrorCode::Infrastructure).await;
                        return;
                    }
                }
            }
        }
    })
}

/// 失败转换同步取消该转发的后续代理尝试；关闭失败仍可见且允许显式 stop 重试。
async fn observe_failure_and_close(record: &PortForwardRecord, failure_code: PublicErrorCode) {
    if let Err(error) = mark_forward_failed(record, failure_code).await {
        *record.failure_code.lock().await = Some(error.public_code());
    }
    if let Err(error) = record
        .runtime
        .close_until(Instant::now() + STOP_TIMEOUT)
        .await
    {
        *record.failure_code.lock().await = Some(error.public_code());
        publish_forward_change(&record.changes);
    }
}

/// 标记仍有运行态资源的失败、保存脱敏错误码并同步取消关联的后续代理尝试。
async fn mark_forward_failed(record: &PortForwardRecord, code: PublicErrorCode) -> AppResult<()> {
    let mut forward = record.forward.lock().await;
    if matches!(
        forward.state,
        ForwardState::Starting | ForwardState::Active | ForwardState::Closing
    ) {
        forward
            .fail()
            .map_err(|_| AppError::conflict("端口转发失败后的状态转换无效"))?;
    }
    *record.failure_code.lock().await = Some(code);
    record.future_proxy_policy.cancel_by_forward(&forward.id);
    drop(forward);
    publish_forward_change(&record.changes);
    Ok(())
}

/// 取消并 join 故障观察任务，避免应用退出后留下无主 watcher。
async fn cancel_failure_monitor_until(
    record: &PortForwardRecord,
    deadline: Instant,
) -> AppResult<()> {
    record.monitor_cancel.cancel();
    let mut task = match lock_recover(&record.monitor_task).take() {
        Some(task) => task,
        None => return Ok(()),
    };
    match timeout_at(deadline, &mut task).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(AppError::infrastructure_with_source(
            "端口转发故障监控未能退出",
            error,
        )),
        Err(error) => {
            task.abort();
            let _ = task.await;
            Err(AppError::infrastructure_with_source(
                "端口转发故障监控退出超时",
                error,
            ))
        }
    }
}

/// 构造无副作用状态副本，避免调用方持有运行态锁或直接改写聚合。
async fn snapshot_record(record: &PortForwardRecord) -> PortForwardSnapshot {
    let forward = record.forward.lock().await.clone();
    let actual_bind = record.runtime.actual_bind();
    let proxy_url = proxy_url(&forward.config.kind, &actual_bind);
    let proxy_application =
        ProxyApplicationOutcome::not_applied(forward.config.proxy_apply_scope, proxy_url.is_some());
    PortForwardSnapshot {
        forward,
        actual_bind,
        proxy_url,
        proxy_application,
        failure_code: *record.failure_code.lock().await,
    }
}

/// 从运行时确认的 bind 构造可交给调用方的 SOCKS5h URL，不将代理注入任何外部环境。
fn proxy_url(kind: &ForwardKind, actual_bind: &ForwardEndpoint) -> Option<String> {
    if !matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic) {
        return None;
    }
    let host = match actual_bind.host.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        host => host,
    };
    let endpoint = ForwardEndpoint::new(host, actual_bind.port.get()).ok()?;
    Some(format!("socks5h://{endpoint}"))
}

/// 锁中毒后保留已有运行目录，避免一次观察 panic 令资源句柄不可再关闭。
fn lock_recover<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
