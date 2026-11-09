//! 插件进程到所属窗口的有界交接；视图身份由应用层提供并由窗口生命周期登记。
//!
//! @author kongweiguang

use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

use gpui_kit::{
    ClipboardEntry, ClipboardItem, Context, Task, Window,
    component::{WindowExt as _, notification::Notification},
};
use kerminal_application::{
    AppError, AppResult,
    plugin_host_api::PluginUiPort,
    plugin_ports::{PluginHostError, PluginViewIdentity},
};
use kerminal_domain::plugins::PluginId;
use kerminal_plugin_protocol::error_code;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::RootView;
use crate::plugins::host_api::{
    PluginUiQueueError, PluginUiReceiver, PluginUiRequest, PluginUiResponse, PluginUiRouteGuard,
    PluginUiRoutes, PluginUiSender, plugin_ui_queue,
};

const UI_QUEUE_CAPACITY: usize = 128;

type UiResult = Result<Value, PluginHostError>;
type UiSender = PluginUiSender<UiCommand, UiResult>;
type UiReceiver = PluginUiReceiver<UiCommand, UiResult>;
type UiRequest = PluginUiRequest<UiCommand, UiResult>;
type UiGuard = PluginUiRouteGuard<PluginViewIdentity, UiCommand, UiResult>;

struct UiCommand {
    plugin: PluginId,
    method: String,
    params: Value,
    cancellation: CancellationToken,
}

/// 一个桥接实例绑定一个明确的主窗口；视图请求必须先按完整身份登记。
pub struct PluginUiBridge {
    main_sender: UiSender,
    main_receiver: Mutex<Option<UiReceiver>>,
    routes: PluginUiRoutes<PluginViewIdentity, UiCommand, UiResult>,
}

/// 实例拥有此句柄；关闭或释放它会取消该实例尚未提交的请求。
pub struct PluginUiRegistration {
    guard: UiGuard,
}

impl PluginUiBridge {
    /// 桥接器只建立有界内存队列，窗口接收任务由 Root 生命周期持有。
    pub fn new() -> AppResult<Arc<Self>> {
        let (main_sender, main_receiver) = plugin_ui_queue(UI_QUEUE_CAPACITY)
            .map_err(|_| AppError::conflict("插件界面队列初始化失败"))?;
        Ok(Arc::new(Self {
            main_sender,
            main_receiver: Mutex::new(Some(main_receiver)),
            routes: PluginUiRoutes::new(),
        }))
    }

    /// 身份只能登记到此桥接器所属的主窗口；重复身份不能覆盖现有实例。
    pub fn register_instance(
        &self,
        identity: PluginViewIdentity,
    ) -> AppResult<PluginUiRegistration> {
        let guard = self
            .routes
            .register(identity, self.main_sender.clone())
            .map_err(|_| AppError::conflict("插件界面实例登记失败"))?;
        Ok(PluginUiRegistration { guard })
    }

    /// 只允许一个 Root 消费该窗口队列，防止两个窗口竞争同一个插件请求。
    fn take_receiver(&self) -> AppResult<UiReceiver> {
        self.main_receiver
            .lock()
            .map_err(|_| AppError::conflict("插件界面队列状态不可用"))?
            .take()
            .ok_or_else(|| AppError::conflict("插件界面队列已经安装"))
    }
}

impl PluginUiRegistration {
    /// 显式关闭与释放的效果相同，但可在视图资源异步回收之前撤销请求权限。
    pub fn close(&mut self) {
        self.guard.close();
    }
}

impl PluginUiPort for PluginUiBridge {
    /// 权限由应用层校验；这里继续核对实例归属、原窗口路由和取消生命周期。
    fn invoke<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        plugin: &'life1 PluginId,
        instance: Option<&'life2 PluginViewIdentity>,
        method: &'life3 str,
        params: Value,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = UiResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(host_error(error_code::CANCELLED));
            }
            if !params.is_object() || instance.is_some_and(|identity| identity.plugin != *plugin) {
                return Err(host_error(error_code::INVALID_PARAMS));
            }
            if instance.is_none()
                && matches!(
                    method,
                    "ui.confirm" | "ui.prompt" | "ui.pickFile" | "terminal.paste"
                )
            {
                return Err(host_error(error_code::INVALID_PARAMS));
            }
            let command = UiCommand {
                plugin: plugin.clone(),
                method: method.to_owned(),
                params,
                cancellation: cancellation.clone(),
            };
            let response = match instance {
                Some(identity) => self.routes.try_send(identity, command),
                None => self.main_sender.try_send(command),
            }
            .map_err(queue_error)?;
            await_response(response, cancellation).await
        })
    }
}

/// 已提交结果优先于取消；取消先取得门禁时窗口不能再提交同步副作用。
async fn await_response(
    mut response: PluginUiResponse<UiResult>,
    cancellation: CancellationToken,
) -> UiResult {
    let mut cancelled = Box::pin(cancellation.cancelled());
    poll_fn(|cx| {
        if let Poll::Ready(result) = Pin::new(&mut response).poll(cx) {
            return Poll::Ready(result.map_err(queue_error).and_then(|value| value));
        }
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(match response.cancel_if_pending() {
                Ok(Some(result)) => result,
                Ok(None) => Err(host_error(error_code::CANCELLED)),
                Err(error) => Err(queue_error(error)),
            });
        }
        Poll::Pending
    })
    .await
}

/// 内部同步错误不暴露细节，过载和实例失效保留可处理的协议分类。
fn queue_error(error: PluginUiQueueError) -> PluginHostError {
    let code = match error {
        PluginUiQueueError::Full => error_code::OVERLOADED,
        PluginUiQueueError::Closed | PluginUiQueueError::MissingRoute => error_code::STALE,
        PluginUiQueueError::Poisoned
        | PluginUiQueueError::InvalidCapacity
        | PluginUiQueueError::DuplicateRoute => error_code::INTERNAL_ERROR,
    };
    host_error(code)
}

/// 仅返回稳定文案，避免把宿主内部错误、用户数据或路径带入插件协议。
fn host_error(code: i32) -> PluginHostError {
    let message = match code {
        error_code::INVALID_PARAMS => "invalid params",
        error_code::METHOD_NOT_FOUND => "method not found",
        error_code::CANCELLED => "cancelled",
        error_code::OVERLOADED => "overloaded",
        error_code::STALE => "stale",
        _ => "internal error",
    };
    PluginHostError {
        code,
        message: message.to_owned(),
    }
}

/// 只返回真正的文本条目；空文本与没有文本不同，文件路径不能冒充剪贴板文本。
fn clipboard_text(item: Option<ClipboardItem>) -> Option<String> {
    let item = item?;
    let mut text = None::<String>;
    for entry in item.into_entries() {
        if let ClipboardEntry::String(value) = entry {
            text.get_or_insert_with(String::new).push_str(&value.text);
        }
    }
    text
}

impl RootView {
    /// 接收循环绑定本 Root 与窗口；调用方必须保存返回的 Task，释放时自动关闭接收端。
    pub fn install_plugin_ui(
        &mut self,
        bridge: &PluginUiBridge,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AppResult<Task<()>> {
        let mut receiver = bridge.take_receiver()?;
        Ok(cx.spawn_in(window, async move |this, cx| {
            loop {
                let request = match receiver.next().await {
                    Ok(Some(request)) => request,
                    Ok(None) => break,
                    Err(_) => {
                        tracing::error!("插件界面队列读取失败");
                        break;
                    }
                };
                if this
                    .update_in(cx, |root, window, cx| {
                        root.dispatch_plugin_ui(request, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        }))
    }

    /// 同步能力只在原窗口的回复门禁内提交；交互能力需要单独协调弹层与取消生命周期。
    fn dispatch_plugin_ui(
        &mut self,
        mut request: UiRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = request.take_message() else {
            return;
        };
        let shutdown = self.host_shutdown_requested;
        request.complete_with(|| {
            if command.cancellation.is_cancelled() {
                return Err(host_error(error_code::CANCELLED));
            }
            if shutdown {
                return Err(host_error(error_code::STALE));
            }
            match command.method.as_str() {
                "ui.notify" => {
                    let level = command
                        .params
                        .get("level")
                        .and_then(Value::as_str)
                        .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                    let message = command
                        .params
                        .get("message")
                        .and_then(Value::as_str)
                        .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                    let message = format!("[{}] {message}", command.plugin.as_str());
                    let notification = match level {
                        "info" => Notification::info(message),
                        "success" => Notification::success(message),
                        "warning" | "warn" => Notification::warning(message),
                        "error" => Notification::error(message),
                        _ => return Err(host_error(error_code::INVALID_PARAMS)),
                    };
                    window.push_notification(notification, cx);
                    Ok(json!({}))
                }
                "clipboard.read" => {
                    if !command
                        .params
                        .as_object()
                        .is_some_and(|params| params.is_empty())
                    {
                        return Err(host_error(error_code::INVALID_PARAMS));
                    }
                    Ok(json!({"text": clipboard_text(cx.read_from_clipboard())}))
                }
                "clipboard.write" => {
                    let text = command
                        .params
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                    Ok(json!({}))
                }
                "shell.openExternal" => {
                    let url = command
                        .params
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|text| url::Url::parse(text).ok())
                        .filter(|url| {
                            url.scheme() == "https"
                                && url.host_str().is_some()
                                && url.username().is_empty()
                                && url.password().is_none()
                        })
                        .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                    cx.open_url(url.as_str());
                    Ok(json!({}))
                }
                _ => Err(host_error(error_code::METHOD_NOT_FOUND)),
            }
        });
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_plugin_host_api.rs"]
mod tests;
