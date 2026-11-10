//! 正式插件网页消息的实例边界；只解析与跟踪请求，不在窗口线程执行插件能力。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use kerminal_application::{AppError, AppResult, plugin_ports::PluginViewIdentity};
use kerminal_plugin_protocol::runtime::{RpcMessage, RpcPeer};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MAX_BRIDGE_BYTES: usize = 64 * 1024;
const MAX_IN_FLIGHT: usize = 64;

pub mod renderer;

/// 网页只可请求打开宿主命令面板，不可传入任意宿主动作名称。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.webview.commandPalette", no_json, no_register)]
pub struct OpenWebViewCommandPalette;
/// 顶层字段使用类型化反序列化，重复身份或未知信封字段不能被静默覆盖。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeEnvelope {
    view_instance: String,
    generation: u64,
    message: RpcMessage,
}

/// 注入脚本复用 SDK 的实际浏览器实现；参数只以 JSON 字面量插入，不能成为脚本。
pub(crate) fn browser_bootstrap(
    identity: &PluginViewIdentity,
    generation: u64,
    locale: &str,
    theme: &str,
) -> AppResult<String> {
    if generation > 9_007_199_254_740_991 || identity.instance.is_empty() {
        return Err(AppError::invalid("插件网页实例身份无效"));
    }
    let values = serde_json::to_string(&json!({
        "identity":{"view_instance":identity.instance,"generation":generation},
        "environment":{"locale":locale,"theme":theme}
    }))
    .map_err(|_| AppError::invalid("插件网页初始化参数无效"))?;
    Ok(format!(
        r#"(()=>{{
{sdk}
const bootstrap={values};
const transport={{
  send(envelope){{ window.ipc.postMessage(JSON.stringify(envelope)); }},
  subscribe(listener){{
    const handler=(event)=>listener(event.detail);
    window.addEventListener('kerminal:message',handler);
    return ()=>window.removeEventListener('kerminal:message',handler);
  }}
}};
Object.defineProperty(window,'kerminal',{{value:createBrowserBridge(transport,bootstrap.identity,bootstrap.environment),writable:false,configurable:false}});
window.addEventListener('keydown',(event)=>{{
  if (!event.isComposing && !event.repeat && (event.ctrlKey || event.metaKey) && event.shiftKey && !event.altKey && event.code==='KeyP') {{
    event.preventDefault();
    transport.send({{...bootstrap.identity,message:{{jsonrpc:'2.0',method:'webview.commandPalette',params:{{}}}}}});
  }}
}},true);
const heartbeat=window.setInterval(()=>transport.send({{...bootstrap.identity,message:{{jsonrpc:'2.0',method:'webview.heartbeat',params:{{}}}}}}),1000);
window.addEventListener('pagehide',()=>{{window.clearInterval(heartbeat);window.kerminal.close();}},{{once:true}});
window.addEventListener('kerminal:message',(event)=>{{
  const frame=event.detail;
  if (frame?.view_instance!==bootstrap.identity.view_instance || frame.generation!==bootstrap.identity.generation || frame.message?.method!=='theme.changed') return;
  const params=frame.message.params;
  if (!params || typeof params!=='object') return;
  const apply=()=>{{
    if (!document.documentElement) return;
    for (const [key,value] of Object.entries(params.tokens ?? {{}})) {{
      if (key.startsWith('--k-') && typeof value==='string') document.documentElement.style.setProperty(key,value);
    }}
    document.documentElement.style.colorScheme=params.theme==='dark'?'dark':'light';
  }};
  apply();
  if (!document.documentElement) document.addEventListener('DOMContentLoaded',apply,{{once:true}});
}});
}})();"#,
        sdk = include_str!("../../resources/plugin-browser-sdk.js")
    ))
}

/// 只有宿主确认的实例和代次才能消费事件；打开后的目标不会跟随当前焦点变化。
pub(crate) struct WebViewBridgeSession {
    identity: PluginViewIdentity,
    bridge_generation: u64,
    active: BTreeMap<String, (u64, CancellationToken)>,
    next_sequence: u64,
    host_pending: BTreeSet<String>,
    next_host_request: u64,
    closed: bool,
}

/// 页面请求、撤销和宿主请求回执采用不同通路，避免把 view.action 误交给宿主权限路由。
pub(crate) enum WebViewBridgeMessage {
    Request {
        id: String,
        method: String,
        params: Value,
        cancellation: CancellationToken,
        sequence: u64,
    },
    Cancelled,
    CommandPalette,
    Heartbeat,
    Response(Value),
}

impl WebViewBridgeSession {
    /// 桥接代次独立于插件进程和宿主视图代次，并保证 JavaScript 精确表示。
    pub(crate) fn new(identity: PluginViewIdentity, bridge_generation: u64) -> AppResult<Self> {
        if identity.instance.is_empty() || bridge_generation > 9_007_199_254_740_991 {
            return Err(AppError::invalid("插件网页实例身份无效"));
        }
        Ok(Self {
            identity,
            bridge_generation,
            active: BTreeMap::new(),
            next_sequence: 0,
            host_pending: BTreeSet::new(),
            next_host_request: 0,
            closed: false,
        })
    }

    /// 先验证字节数和外层身份，再验证 RPC 结构；过期消息不能消费当前页面请求槽位。
    pub(crate) fn accept(&mut self, body: &str) -> AppResult<WebViewBridgeMessage> {
        if self.closed || body.len() > MAX_BRIDGE_BYTES {
            return Err(AppError::invalid("插件网页消息已失效或超过大小限制"));
        }
        let envelope: BridgeEnvelope =
            serde_json::from_str(body).map_err(|_| AppError::invalid("插件网页消息无效"))?;
        if envelope.view_instance != self.identity.instance
            || envelope.generation != self.bridge_generation
        {
            return Err(AppError::conflict("插件网页实例已失效"));
        }
        match envelope.message {
            RpcMessage::Request(request) => {
                request
                    .validate(RpcPeer::Plugin)
                    .map_err(|_| AppError::invalid("插件网页请求无效"))?;
                let params = request.params.unwrap_or_else(|| json!({}));
                if !params.is_object() {
                    return Err(AppError::invalid("插件网页 RPC 参数无效"));
                }
                if self.active.contains_key(&request.id) || self.active.len() >= MAX_IN_FLIGHT {
                    return Err(AppError::conflict("插件网页请求不可用"));
                }
                let sequence = self
                    .next_sequence
                    .checked_add(1)
                    .ok_or_else(|| AppError::conflict("插件网页请求序列已耗尽"))?;
                self.next_sequence = sequence;
                let cancellation = CancellationToken::new();
                self.active
                    .insert(request.id.clone(), (sequence, cancellation.clone()));
                Ok(WebViewBridgeMessage::Request {
                    id: request.id,
                    method: request.method,
                    params,
                    cancellation,
                    sequence,
                })
            }
            RpcMessage::Notification(notification) => {
                notification
                    .validate()
                    .map_err(|_| AppError::invalid("插件网页通知无效"))?;
                if notification.method == "webview.heartbeat"
                    && notification.params.as_ref().is_some_and(|params| {
                        params.as_object().is_some_and(|object| object.is_empty())
                    })
                {
                    return Ok(WebViewBridgeMessage::Heartbeat);
                }
                if notification.method == "webview.commandPalette"
                    && notification.params.as_ref().is_none_or(|params| {
                        params.as_object().is_some_and(|object| object.is_empty())
                    })
                {
                    return Ok(WebViewBridgeMessage::CommandPalette);
                }
                if notification.method != "$/cancel" {
                    return Err(AppError::invalid("插件网页通知不受支持"));
                }
                let params = notification.params.unwrap_or_else(|| json!({}));
                let id = params
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::invalid("插件网页取消身份无效"))?;
                if let Some((_, token)) = self.active.remove(id) {
                    token.cancel();
                }
                Ok(WebViewBridgeMessage::Cancelled)
            }
            RpcMessage::Response(response) => {
                response
                    .validate()
                    .map_err(|_| AppError::invalid("插件网页回执无效"))?;
                if !response
                    .id
                    .as_deref()
                    .is_some_and(|id| self.host_pending.remove(id))
                {
                    return Err(AppError::invalid("插件网页回执身份无效"));
                }
                Ok(WebViewBridgeMessage::Response(
                    serde_json::to_value(response)
                        .map_err(|_| AppError::invalid("插件网页回执无效"))?,
                ))
            }
        }
    }

    /// 宿主发起的生命周期请求分配单调递增身份，页面无法提前伪造未登记的回执。
    pub(crate) fn begin_host_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> AppResult<(String, Value)> {
        if self.closed || self.host_pending.len() >= MAX_IN_FLIGHT || !params.is_object() {
            return Err(AppError::conflict("插件网页生命周期请求不可用"));
        }
        let sequence = self
            .next_host_request
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("插件网页生命周期序列已耗尽"))?;
        let id = format!("h:{sequence}");
        let request =
            kerminal_plugin_protocol::runtime::RpcRequest::new(id.clone(), method, params);
        request
            .validate(RpcPeer::Host)
            .map_err(|_| AppError::invalid("插件网页生命周期请求无效"))?;
        let message = serde_json::to_value(request)
            .map_err(|_| AppError::invalid("插件网页生命周期请求无效"))?;
        self.next_host_request = sequence;
        self.host_pending.insert(id.clone());
        Ok((
            id,
            json!({"view_instance":self.identity.instance,"generation":self.bridge_generation,"message":message}),
        ))
    }

    /// 发送失败或超时后释放宿主请求；迟到回执不能被另一个生命周期请求接收。
    pub(crate) fn cancel_host_request(&mut self, id: &str) {
        self.host_pending.remove(id);
    }

    /// 进程恢复保留 DOM，但撤销此前进程上的全部请求；序列不重置以隔离迟到结果。
    pub(crate) fn cancel_requests(&mut self) {
        self.host_pending.clear();
        for (_, token) in std::mem::take(&mut self.active).into_values() {
            token.cancel();
        }
    }

    /// 进程中断时明确结算网页 Promise 后再撤销请求，避免保留 DOM 的按钮永久处于 busy。
    pub(crate) fn abort_requests(&mut self) -> Vec<Value> {
        let replies = std::mem::take(&mut self.active).into_iter().map(|(id, (_, token))| {
            token.cancel();
            json!({"view_instance":self.identity.instance,"generation":self.bridge_generation,
                "message":{"jsonrpc":"2.0","id":id,"error":{"code":-32004,"message":"view unavailable"}}})
        }).collect();
        self.host_pending.clear();
        replies
    }

    /// 推送只携带本页面代次，不创建无消费者的请求槽位。
    pub(crate) fn notification(&self, method: &str, params: Value) -> Value {
        json!({"view_instance":self.identity.instance,"generation":self.bridge_generation,
            "message":{"jsonrpc":"2.0","method":method,"params":params}})
    }

    /// 仅在请求仍属当前页面时允许发送回执；取消或关闭后的迟到结果直接丢弃。
    pub(crate) fn finish(
        &mut self,
        id: &str,
        sequence: u64,
        result: Result<Value, (i32, String)>,
    ) -> Option<Value> {
        if self
            .active
            .get(id)
            .is_none_or(|(current, _)| *current != sequence)
        {
            return None;
        }
        let (_, token) = self.active.remove(id)?;
        if self.closed || token.is_cancelled() {
            return None;
        }
        let message = match result {
            Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        };
        Some(
            json!({"view_instance":self.identity.instance,"generation":self.bridge_generation,"message":message}),
        )
    }

    /// 释放页面时取消宿主侧任务；不可重放的动作不能在新代次自动重试。
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.cancel_requests();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/external_webview_bridge.rs"]
mod tests;

impl Drop for WebViewBridgeSession {
    /// 页面所有权结束时同样取消请求，避免异常卸载遗留后台操作。
    fn drop(&mut self) {
        self.close();
    }
}
