//! 外部插件运行服务与宿主适配器之间的纯数据端口。
//! @author kongweiguang

use std::{
    error::Error,
    fmt::{Display, Formatter},
    sync::{Arc, Weak},
    time::Instant,
};

use async_trait::async_trait;
use kerminal_domain::plugins::{PluginId, PluginRuntimeState};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub use crate::external_contributions::PreparedPlugin;
use crate::{AppError, AppResult};

/// 实例代次与进程代次独立，视图关闭重开时旧进程回调也不能命中新实例。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PluginViewIdentity {
    pub plugin: PluginId,
    pub view: String,
    pub instance: String,
    pub generation: u64,
}

/// 缓存仅保存已校验的状态，以便同步 ContentInstance 快照不等待进程。
#[derive(Clone, Debug, PartialEq)]
pub struct PluginViewSavedState {
    pub state_version: u32,
    pub state: Value,
}

/// 缓存和修订号在同一个实例锁下读取，供订阅者过滤快照之前的缓冲事件。
#[derive(Clone, Debug, PartialEq)]
pub struct PluginViewDataSnapshot {
    pub revision: u64,
    pub data: Value,
}

/// 业务错误由明确插件协议错误码产生，文本经过限长和控制字符清理。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginBusinessError {
    code: i32,
    message: String,
}

impl PluginBusinessError {
    /// 仅允许业务错误范围携带插件文案，避免把协议或系统异常直接展示给用户。
    pub fn new(code: i32, message: &str) -> Option<Self> {
        use kerminal_plugin_protocol::error_code::{PLUGIN_BUSINESS_MAX, PLUGIN_BUSINESS_MIN};
        if !(PLUGIN_BUSINESS_MIN..=PLUGIN_BUSINESS_MAX).contains(&code) {
            return None;
        }
        let message: String = message.chars().filter_map(|character| {
            if character.is_control() {
                Some(' ')
            } else if matches!(character, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}') {
                None
            } else {
                Some(character)
            }
        }).take(200).collect();
        Some(Self {
            code,
            message: message.trim().to_owned(),
        })
    }

    /// 保留协议业务分类，调用方可据此提供插件自己的针对性反馈。
    pub fn code(&self) -> i32 {
        self.code
    }

    /// 消费方显式提取受控文案；AppError 的通用展示仍保持固定脱敏文本。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for PluginBusinessError {
    /// 此类型只在调用方显式解包业务错误时展示，内容已经限长并剔除控制字符。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PluginBusinessError {}

/// 仅提取已验证的插件业务错误，不能把普通内部错误 source 暴露给界面。
pub fn plugin_business_error(error: &AppError) -> Option<&PluginBusinessError> {
    error.source()?.downcast_ref::<PluginBusinessError>()
}

/// 观察器只做同步内存校验和提交，真实传输在读取后续帧之前调用。
pub type PluginResponseObserver = Box<dyn FnOnce(&Value) -> AppResult<()> + Send + 'static>;

/// 服务发布的事件始终携带宿主确认过的实例身份；订阅落后时须重新读取缓存。
#[derive(Clone, Debug)]
pub enum PluginRuntimeEvent {
    ProcessState {
        plugin: PluginId,
        state: PluginRuntimeState,
    },
    ViewUpdate {
        identity: PluginViewIdentity,
        process_generation: u64,
        revision: u64,
        patch: Value,
    },
    ViewState {
        identity: PluginViewIdentity,
        state: PluginViewSavedState,
    },
    ViewClosed {
        identity: PluginViewIdentity,
        state: Option<PluginViewSavedState>,
    },
    Progress {
        plugin: PluginId,
        generation: u64,
        params: Value,
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
}

/// 仅传递协议错误码和受控文案，避免跨进程暴露宿主错误源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginHostError {
    pub code: i32,
    pub message: String,
}

/// 应用服务是进程回调与 WebView 的共同权限门禁。
#[async_trait]
pub trait PluginHostRouter: Send + Sync + 'static {
    /// 视图更新只能同步修改内存；读取器按消息顺序调用，不能在这里执行 I/O。
    fn ordered_view_update(
        &self,
        _plugin: &PluginId,
        _generation: u64,
        _params: Value,
    ) -> Result<Value, PluginHostError> {
        Err(PluginHostError {
            code: kerminal_plugin_protocol::error_code::METHOD_NOT_FOUND,
            message: "method not found".into(),
        })
    }

    /// 进程身份和代次由监督器提供，不能采信插件自报的身份。
    async fn route(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, PluginHostError>;
}

/// 权限检查之后才调用的宿主能力，具体 UI、Vault 等实现由组合根注入。
#[async_trait]
pub trait PluginHostApi: Send + Sync + 'static {
    /// 实例为空时不得把对话框等操作隐式绑定到当前窗口。
    async fn invoke(
        &self,
        plugin: &PluginId,
        generation: u64,
        instance: Option<&PluginViewIdentity>,
        method: &str,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, PluginHostError>;
}

/// 原始进程消息只作为应用用例的输入，不直接暴露给界面。
#[derive(Clone, Debug)]
pub struct PluginProcessMessage {
    pub plugin: PluginId,
    pub generation: u64,
    pub method: String,
    pub params: Value,
}

/// 进程所有权和 I/O 留在基础设施；每次调用都固定绝对截止时间与代次。
// 观察器与原有调用参数必须原子传给传输层，以便按帧顺序提交视图状态。
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait PluginProcessPort: Send + Sync + 'static {
    /// 弱引用打破服务与进程监督器的所有权环，关闭后不保留应用服务。
    fn register_host_router(&self, router: Weak<dyn PluginHostRouter>) -> AppResult<()>;

    /// 读取受监督进程的实际状态，供事件丢失时重新对账。
    fn state(&self, plugin: &PluginId) -> AppResult<PluginRuntimeState>;

    /// 同一插件只启动一个进程；返回已完成握手的当前进程代次。
    async fn activate(&self, plugin: &PluginId, deadline: Instant) -> AppResult<u64>;

    /// 只有明确的用户重试能解除失败状态，缺少实现时必须返回错误。
    async fn retry(&self, _plugin: &PluginId, _deadline: Instant) -> AppResult<u64> {
        Err(crate::AppError::unsupported("插件进程不支持重试"))
    }

    /// 开发重载要求旧代次彻底回收后再启动，未实现的进程适配器必须明确拒绝。
    async fn restart(&self, _plugin: &PluginId, _deadline: Instant) -> AppResult<u64> {
        Err(crate::AppError::unsupported("插件进程不支持重启"))
    }

    /// 传输层负责有界请求表、取消消息和旧代次拒绝，不自动重放副作用。
    async fn call(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value>;

    /// 观察器必须在读取后续传入视图帧前提交；默认路径仅供串行模拟器使用。
    async fn call_observed(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
        observer: PluginResponseObserver,
    ) -> AppResult<Value> {
        let response = self
            .call(plugin, generation, method, params, deadline, cancellation)
            .await?;
        observer(&response)?;
        Ok(response)
    }

    /// 通知不等待业务响应，但仍受代次、分帧和发送截止时间约束。
    async fn notify(
        &self,
        plugin: &PluginId,
        generation: u64,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> AppResult<()>;

    /// 订阅只反映受监督进程实际发送的消息，落后由用例层显式处理。
    fn subscribe(&self) -> broadcast::Receiver<PluginProcessMessage>;

    /// 所有插件共享调用方提供的截止时间，超时不得冒充资源已回收。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()>;
}

/// 界面只调用应用服务并消费冻结资源；不会直接创建进程或读取文件。
#[async_trait]
pub trait PluginRuntime: Send + Sync + 'static {
    /// 获得启动时冻结的资源，即使进程尚未启动也可先构建原生视图。
    fn prepared_plugin(&self, id: &PluginId) -> AppResult<Arc<PreparedPlugin>>;

    /// 注册宿主实例并请求打开；超时和取消不改变其他实例的绑定。
    async fn open(
        &self,
        identity: PluginViewIdentity,
        scope: Value,
        state: Option<PluginViewSavedState>,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value>;

    /// 普通视图调用固定当前实例身份，调用方负责按具体动作选择超时。
    async fn invoke(
        &self,
        identity: &PluginViewIdentity,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<Value>;

    /// 同步读取最近一次有效状态；未取得快照时返回 None 而不伪造空状态。
    fn cached_state(
        &self,
        identity: &PluginViewIdentity,
    ) -> AppResult<Option<PluginViewSavedState>>;

    /// 请求新的异步快照并更新缓存，成功事件必须关联原实例代次。
    async fn snapshot(
        &self,
        identity: &PluginViewIdentity,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> AppResult<PluginViewSavedState>;

    /// 返回关闭回执中的最终状态；宿主在移除实例前负责持久化该状态。
    async fn close(
        &self,
        identity: &PluginViewIdentity,
        deadline: Instant,
    ) -> AppResult<Option<PluginViewSavedState>>;

    /// 接收状态、补丁和最终关闭通知；Lagged 后重读缓存恢复。
    fn subscribe(&self) -> broadcast::Receiver<PluginRuntimeEvent>;
}
