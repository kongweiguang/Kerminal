//! Kerminal Plugin RPC v1 的双向消息形状与宿主方法支持列表。
//! @author kongweiguang

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ProtocolError, invalid};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_RPC_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PENDING_REQUESTS: usize = 64;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;
pub const MAX_VIEW_STATE_BYTES: usize = 256 * 1024;
pub const MAX_VIEW_PATCH_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcMessage {
    Request(RpcRequest),
    Notification(RpcNotification),
    Response(RpcResponse),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RpcPeer {
    Host,
    Plugin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_params"
    )]
    pub params: Option<Value>,
}

impl RpcRequest {
    /// 请求 ID 由发起方显式命名，双向 RPC 的计数器不能互相覆盖。
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            method: method.into(),
            params: Some(params),
        }
    }

    /// 只校验信封和方向，实例、代次及参数由收到消息的宿主校验。
    pub fn validate(&self, peer: RpcPeer) -> Result<(), ProtocolError> {
        validate_header(&self.jsonrpc, &self.method, self.params.as_ref())?;
        let prefix = match peer {
            RpcPeer::Host => "h:",
            RpcPeer::Plugin => "p:",
        };
        if !self.id.starts_with(prefix)
            || self.id.len() <= prefix.len()
            || self.id.len() > 128
            || self.id.chars().any(char::is_control)
        {
            return Err(invalid("rpc.id", "请求 ID 与发起方不匹配"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_params"
    )]
    pub params: Option<Value>,
}

impl RpcNotification {
    /// 通知没有 ID，不会意外占用请求配额或被当成回复。
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: Some(params),
        }
    }

    /// 协议保留的 $/ 方法同样允许通过信封检查。
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_header(&self.jsonrpc, &self.method, self.params.as_ref())
    }
}

/// 保留显式 null，让信封校验区分非法参数与省略参数。
fn present_params<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// JSON-RPC 参数只接受对象或数组，具体方法还需校验对应参数结构。
fn validate_header(
    version: &str,
    method: &str,
    params: Option<&Value>,
) -> Result<(), ProtocolError> {
    if version != "2.0"
        || method.is_empty()
        || method.len() > 128
        || method.chars().any(char::is_control)
        || params.is_some_and(|value| !value.is_object() && !value.is_array())
    {
        return Err(invalid("rpc", "无效的 JSON-RPC 信封"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(deserialize_with = "present_response_id")]
    pub id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_result"
    )]
    pub result: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_error"
    )]
    pub error: Option<RpcError>,
}

/// 响应必须显式包含 id，即便无法识别请求而使用 null。
fn present_response_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

/// null 是合法成功结果，不能与缺失的 result 合并。
fn present_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// 显式的 null 不是有效错误对象，不能把它当作缺失字段。
fn present_error<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<RpcError>, D::Error> {
    RpcError::deserialize(deserializer).map(Some)
}

impl RpcResponse {
    /// 回复必须恰有一个结果；无法识别请求时仅错误回复允许空 ID。
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.jsonrpc != "2.0"
            || self.result.is_some() == self.error.is_some()
            || self.id.as_ref().is_some_and(|id| {
                id.len() < 3
                    || id.len() > 128
                    || id.chars().any(char::is_control)
                    || !(id.starts_with("h:") || id.starts_with("p:"))
            })
            || (self.id.is_none() && self.error.is_none())
        {
            return Err(invalid("rpc.response", "无效的 JSON-RPC 回复"));
        }
        Ok(())
    }
}

pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const PERMISSION_DENIED: i32 = -32001;
    pub const CANCELLED: i32 = -32002;
    pub const OVERLOADED: i32 = -32003;
    pub const STALE: i32 = -32004;
    pub const TIMEOUT: i32 = -32005;
    pub const PLUGIN_BUSINESS_MIN: i32 = -32199;
    pub const PLUGIN_BUSINESS_MAX: i32 = -32100;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginIdentity {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    pub protocol: u32,
    pub host_api: String,
    pub plugin: PluginIdentity,
    pub locale: String,
    pub theme: String,
    pub data_dir: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitializeResult {
    pub protocol: u32,
    pub plugin: PluginIdentity,
}

impl InitializeResult {
    /// 进程必须证明自己对应本次清单，旧进程不能借重启复用身份。
    pub fn validate_for(&self, request: &InitializeParams) -> Result<(), ProtocolError> {
        if self.protocol != PROTOCOL_VERSION
            || self.protocol != request.protocol
            || self.plugin != request.plugin
        {
            return Err(invalid("initialize", "插件协议或身份不匹配"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewOpenParams {
    pub view: String,
    pub instance: String,
    pub scope: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewOpenResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewVisibilityParams {
    pub instance: String,
    pub visible: bool,
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewActionParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    pub instance: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_key: Option<String>,
    pub data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewActionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toast: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewFetchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    pub instance: String,
    pub node: String,
    pub range: [u64; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<ViewFetchSort>,
}

/// 表格排序只传递列键和方向，宿主与插件使用同一份线协议类型。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewFetchSort {
    pub column: String,
    pub direction: ViewSortDirection,
}

/// 明确方向枚举，避免宿主与插件对不同字符串作出不一致的解释。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ViewSortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewFetchResult {
    pub total: u64,
    pub rows: Vec<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewInstanceParams {
    pub instance: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewStateResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewUpdateParams {
    pub instance: String,
    pub patch: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandRunParams {
    pub command: String,
    pub context: CommandContext,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostOperationParams {
    pub host_type: String,
    pub config: Value,
    pub secrets: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostTestResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostOpenResult {
    pub tab: String,
    pub state: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventParams {
    pub name: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelParams {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgressParams {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
}

/// 只校验宿主是否实现该方法；插件启用后不再按权限拒绝任何已知能力。
pub fn host_method_supported(method: &str) -> Result<(), ProtocolError> {
    match method {
        "view.update" | "settings.get" | "state.get" | "state.set" | "log" | "$/progress"
        | "ui.notify" | "ui.confirm" | "ui.prompt" | "ui.pickFile" | "secrets.get"
        | "secrets.set" | "secrets.delete" | "clipboard.read" | "clipboard.write"
        | "shell.openExternal" | "hosts.list" | "terminal.paste" | "events.subscribe" => Ok(()),
        _ => Err(invalid("rpc.method", "未知的宿主方法")),
    }
}
