//! 原生视图与应用运行服务之间的异步边界，不在窗口线程执行 I/O。
//!
//! @author kongweiguang

use std::{future::Future, pin::Pin, sync::Arc, time::Instant};

use kerminal_application::{
    AppResult,
    plugin_ports::{PluginViewIdentity, PluginViewSavedState, PreparedPlugin},
};
use kerminal_domain::plugins::{PluginId, PluginRuntimeState};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// 已提交的异步工作拥有请求数据，允许交给应用共享执行器调度。
pub type RuntimeFuture<T> = Pin<Box<dyn Future<Output = AppResult<T>> + Send + 'static>>;

/// 订阅器由单个视图实体独占，接收 future 借用它以维持消息顺序。
pub type RuntimeEventFuture<'a> =
    Pin<Box<dyn Future<Output = AppResult<Option<RuntimeEvent>>> + Send + 'a>>;

/// 所有调用绑定宿主实例和预期进程代次；首次打开可等待服务启动进程。
/// `view.open` 的 params 为 `{scope, state_version?, state?}`；两项状态字段必须同时
/// 存在或同时省略。适配器将其转换成应用层 open 参数，不原样转发。
/// `view.action`/`view.fetch` 使用协议字段，适配器根据 identity 固定实例。
/// `view.snapshot`/`view.close` 按应用端口返回 `{state_version?, state?}`；
/// `view.visibility` 为 `{visible, focused}` 通知，不伪造请求回执。
pub struct RuntimeRequest {
    pub identity: PluginViewIdentity,
    pub process_generation: Option<u64>,
    pub method: String,
    pub params: Value,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
}

/// 回复必须带实际进程代次，渲染器才能拒绝重启前发出的迟到结果。
#[derive(Clone, Debug)]
pub struct RuntimeReply {
    pub process_generation: u64,
    pub value: Value,
}

/// 原生视图需要区分更新、进程状态、进度和订阅丢失，不把丢失解释为正常关闭。
#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    ViewUpdate {
        identity: PluginViewIdentity,
        process_generation: u64,
        patch: Value,
    },
    ViewState {
        identity: PluginViewIdentity,
        state: PluginViewSavedState,
    },
    ProcessState {
        plugin: PluginId,
        state: PluginRuntimeState,
    },
    Progress {
        identity: PluginViewIdentity,
        process_generation: u64,
        token: String,
        message: Option<String>,
        percent: Option<f64>,
    },
    ResourcesReloaded {
        plugin: PluginId,
        prepared: Arc<PreparedPlugin>,
    },
    ViewReopened {
        identity: PluginViewIdentity,
        process_generation: u64,
    },
    ViewReopenFailed {
        identity: PluginViewIdentity,
        process_generation: u64,
        message: String,
    },
    ResourcesReloadFailed {
        plugin: PluginId,
        message: String,
    },
    ResyncRequired {
        skipped: u64,
    },
}

/// 订阅关闭和消息溢出分别表达，消费者不得把缺失更新当作完整状态。
pub trait RuntimeSubscription: Send + 'static {
    /// 使用调用方的协作取消，避免窗口关闭后永久占用共享执行器。
    fn recv(&mut self, cancellation: CancellationToken) -> RuntimeEventFuture<'_>;
}

/// 工厂只持有应用适配器；进程、磁盘和传输实现不得依赖 GPUI。
pub trait AsyncViewRuntime: Send + Sync + 'static {
    /// 测试适配器可不提供开发资源；桌面运行时返回最近一次确认的冻结版本。
    fn prepared_plugin(&self, _plugin: &PluginId) -> AppResult<Option<Arc<PreparedPlugin>>> {
        Ok(None)
    }

    /// 只重开已登记的目标实例，不重放动作；不支持该能力的测试端口明确返回错误。
    fn retry_reopen(
        &self,
        _identity: PluginViewIdentity,
        _deadline: Instant,
        _cancellation: CancellationToken,
    ) -> RuntimeFuture<u64> {
        Box::pin(async {
            Err(kerminal_application::AppError::unsupported(
                "插件视图重试不可用",
            ))
        })
    }

    /// 带回执请求不自动重放，超时或取消后由用户或用例明确决定下一步。
    fn request(&self, request: RuntimeRequest) -> RuntimeFuture<RuntimeReply>;

    /// 显隐等通知不伪造请求回执，但仍固定代次和绝对截止时间。
    fn notify(&self, request: RuntimeRequest) -> RuntimeFuture<()>;

    /// 订阅必须在发起打开之前创建，防止首个更新在监听开始前丢失。
    fn subscribe(&self, identity: &PluginViewIdentity) -> AppResult<Box<dyn RuntimeSubscription>>;

    /// 恢复状态来自应用层已确认缓存，不能阻塞同步 ContentInstance 快照。
    fn cached_state(
        &self,
        identity: &PluginViewIdentity,
    ) -> AppResult<Option<PluginViewSavedState>>;

    /// 订阅溢出后读取最新完整数据；缺失快照必须显式返回 None 并等待恢复。
    /// 实现必须在返回快照前原子推进该实例的订阅游标，过滤不晚于快照修订的事件；
    /// 调用方同时丢弃已经交付但尚未应用的缓冲补丁，避免旧消息覆盖较新快照。
    fn cached_data(&self, identity: &PluginViewIdentity) -> AppResult<Option<Value>>;

    /// 凭据只交给宿主 Vault；返回可持久化引用，不把明文传给插件 RPC。
    fn store_secret(
        &self,
        identity: PluginViewIdentity,
        binding: String,
        secret: Zeroizing<String>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> RuntimeFuture<String>;
}
