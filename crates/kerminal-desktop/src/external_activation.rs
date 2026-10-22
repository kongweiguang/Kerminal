//! 单实例与进程首次启动共用的版本化外部启动消息边界。
//!
//! @author kongweiguang

use std::ffi::OsString;

use kerminal_application::{AppError, AppResult, platform::ExternalActivation};
use tokio::sync::mpsc::{self, error::TrySendError};

const MESSAGE_VERSION: u64 = 1;
const MAX_TRANSPORT_FRAME_BYTES: usize = 128 * 1024;
const ACTIVATION_QUEUE_CAPACITY: usize = 16;

/// Root 消费的类型化单实例动作；远程目标参数继续由 application 白名单解析。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivationRequest {
    FocusWindow,
    External(ExternalActivation),
}

/// 向有界 Root 队列提交请求；发送成功只表示本地队列接收，不代表路由或连接成功。
#[derive(Clone)]
pub struct ActivationQueueSender {
    sender: mpsc::Sender<ActivationRequest>,
}

/// 创建固定容量队列；超出容量时调用方等待消费或明确报告请求未排队。
pub fn activation_queue() -> (ActivationQueueSender, mpsc::Receiver<ActivationRequest>) {
    let (sender, receiver) = mpsc::channel(ACTIVATION_QUEUE_CAPACITY);
    (ActivationQueueSender { sender }, receiver)
}

impl ActivationQueueSender {
    /// 在不等待 UI 事件循环的路径中提交一个本地请求，满队列时返回明确错误。
    pub fn try_submit(&self, request: ActivationRequest) -> AppResult<()> {
        validate_request(&request)?;
        match self.sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(AppError::conflict("外部启动请求队列已满")),
            Err(TrySendError::Closed(_)) => Err(AppError::infrastructure("外部启动请求队列已关闭")),
        }
    }

    /// 在单实例监听器中顺序等待有界队列空位，避免高频二次启动造成无界缓冲。
    pub async fn submit(&self, request: ActivationRequest) -> AppResult<()> {
        validate_request(&request)?;
        self.sender
            .send(request)
            .await
            .map_err(|_| AppError::infrastructure("外部启动请求队列已关闭"))
    }
}

/// 把平台原生参数无损转换为 UTF-8 契约，不能表示的 Windows/Unix 参数明确拒绝。
pub fn activation_from_os(
    arguments: impl IntoIterator<Item = OsString>,
    working_directory: Option<OsString>,
) -> AppResult<Option<ExternalActivation>> {
    const MAX_ARGUMENTS: usize = 128;
    const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
    const MAX_DIRECTORY_BYTES: usize = 4 * 1024;

    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.len() <= 1 {
        return Ok(None);
    }
    if arguments.len() > MAX_ARGUMENTS {
        return Err(AppError::invalid("外部启动参数超出安全限制"));
    }

    let mut converted = Vec::with_capacity(arguments.len());
    for argument in arguments {
        if argument.len() > MAX_ARGUMENT_BYTES {
            return Err(AppError::invalid("外部启动参数超出安全限制"));
        }
        converted.push(
            argument
                .into_string()
                .map_err(|_| AppError::invalid("外部启动参数包含非 Unicode 文本"))?,
        );
    }
    let working_directory = working_directory
        .map(|directory| {
            if directory.len() > MAX_DIRECTORY_BYTES {
                return Err(AppError::invalid("外部启动工作目录超出安全限制"));
            }
            directory
                .into_string()
                .map_err(|_| AppError::invalid("外部启动工作目录包含非 Unicode 文本"))
        })
        .transpose()?;

    let activation = ExternalActivation {
        arguments: converted,
        working_directory,
    };
    validate_external_activation(&activation)?;
    Ok(Some(activation))
}

/// 编码带协议版本的消息，并按 single-application 实际 JSON string frame 大小硬限制。
pub fn encode_message(request: &ActivationRequest) -> AppResult<String> {
    validate_request(request)?;
    let value = match request {
        ActivationRequest::FocusWindow => serde_json::json!({
            "version": MESSAGE_VERSION,
            "kind": "activate",
        }),
        ActivationRequest::External(activation) => serde_json::json!({
            "version": MESSAGE_VERSION,
            "kind": "external",
            "arguments": &activation.arguments,
            "workingDirectory": &activation.working_directory,
        }),
    };
    let wire = serde_json::to_string(&value)
        .map_err(|error| AppError::infrastructure_with_source("外部启动消息编码失败", error))?;
    ensure_transport_frame_limit(&wire)?;
    Ok(wire)
}

/// 解析版本化消息并拒绝未知字段、未知版本、损坏 JSON 和超限参数。
pub fn decode_message(wire: &str) -> AppResult<ActivationRequest> {
    if wire == "activate" {
        return Ok(ActivationRequest::FocusWindow);
    }
    ensure_transport_frame_limit(wire)?;
    let value: serde_json::Value =
        serde_json::from_str(wire).map_err(|_| AppError::invalid("外部启动消息格式无效"))?;
    let object = value
        .as_object()
        .ok_or_else(|| AppError::invalid("外部启动消息格式无效"))?;
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| AppError::invalid("外部启动消息版本无效"))?;
    if version != MESSAGE_VERSION {
        return Err(AppError::invalid("外部启动消息版本不受支持"));
    }

    let kind = object
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::invalid("外部启动消息类型无效"))?;
    match kind {
        "activate" if object.len() == 2 => Ok(ActivationRequest::FocusWindow),
        "external" if has_exact_external_fields(object) => {
            let activation = serde_json::from_value::<ExternalActivation>(serde_json::json!({
                "arguments": object.get("arguments"),
                "workingDirectory": object.get("workingDirectory"),
            }))
            .map_err(|_| AppError::invalid("外部启动参数格式无效"))?;
            validate_external_activation(&activation)?;
            Ok(ActivationRequest::External(activation))
        }
        _ => Err(AppError::invalid("外部启动消息类型或字段无效")),
    }
}

/// 验证队列请求的 App 数据契约，避免初始启动和 IPC 分支使用不同上限。
fn validate_request(request: &ActivationRequest) -> AppResult<()> {
    if let ActivationRequest::External(activation) = request {
        validate_external_activation(activation)?;
    }
    Ok(())
}

/// IPC 外部请求必须包含程序名和至少一个实参，聚焦动作使用独立消息类型。
fn validate_external_activation(activation: &ExternalActivation) -> AppResult<()> {
    activation.validate()?;
    if activation.arguments.len() < 2 {
        return Err(AppError::invalid("外部启动参数不完整"));
    }
    Ok(())
}

/// 外部 envelope 必须恰有四个字段，避免未知扩展字段绕过版本升级边界。
fn has_exact_external_fields(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    object.len() == 4
        && object.contains_key("version")
        && object.contains_key("kind")
        && object.contains_key("arguments")
        && object.contains_key("workingDirectory")
}

/// 计算 serde_json 对字符串消息的精确 frame 长度，避免为恶意文本再分配副本。
fn ensure_transport_frame_limit(wire: &str) -> AppResult<()> {
    let mut frame_bytes = 2_usize;
    for character in wire.chars() {
        let escaped_bytes = match character {
            '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        frame_bytes = frame_bytes
            .checked_add(escaped_bytes)
            .ok_or_else(|| AppError::invalid("外部启动消息超出安全限制"))?;
        if frame_bytes > MAX_TRANSPORT_FRAME_BYTES {
            return Err(AppError::invalid("外部启动消息超出安全限制"));
        }
    }
    Ok(())
}
