//! 四类 SSH 端口转发工具的受控参数映射。
//!
//! @author kongweiguang

use kerminal_application::{AppError, operations::PortForwardSnapshot};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, PortForwardConfig, ProxyApplyScope, ProxyProtocol,
        RemoteAccessScope,
    },
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateArgs {
    host_id: String,
    kind: String,
    source_port: u16,
    name: Option<String>,
    bind_host: Option<String>,
    local_bind_host: Option<String>,
    remote_bind_host: Option<String>,
    target_host: Option<String>,
    target_port: Option<u16>,
    proxy_protocol: Option<String>,
    remote_access_scope: Option<String>,
    proxy_apply_scope: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CloseArgs {
    forward_id: String,
}

/// 对外仅接受冻结契约中的三个工具 ID，关闭与列表不持有任何额外转发状态。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    match tool_id {
        "port_forward.create" => create(services, arguments, cancellation).await,
        "port_forward.list" => list(services, arguments).await,
        "port_forward.close" => close(services, arguments, cancellation).await,
        _ => failure(AppError::unsupported("未注册的端口转发工具")),
    }
}

/// 保留有效代理协议与 scope 元数据，并按既有端点优先级解析绑定地址与转发方向。
async fn create(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: CreateArgs = parse_args(arguments)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("端口转发创建已取消"));
        }
        let mut kind = parse_kind(&args.kind)?;
        match args.proxy_protocol.as_deref() {
            Some("socks5") | None => {}
            Some(_) => return Err(AppError::unsupported("只支持 SOCKS5 动态转发")),
        }
        if kind == ForwardKind::Remote
            && args.proxy_protocol.as_deref() == Some("socks5")
            && args.target_host.is_none()
            && args.target_port.is_none()
        {
            kind = ForwardKind::RemoteDynamic;
        }
        let host_id =
            HostId::parse(&args.host_id).map_err(|_| AppError::invalid("端口转发 hostId 无效"))?;
        let is_remote = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
        let bind_host = select_bind_host(&args, is_remote)?;
        let bind = ForwardEndpoint::new(bind_host, args.source_port)
            .map_err(|_| AppError::invalid("端口转发监听地址或端口无效"))?;
        let dynamic = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
        if dynamic && (args.target_host.is_some() || args.target_port.is_some()) {
            return Err(AppError::invalid("动态转发不能指定固定目标端点"));
        }
        let destination = if dynamic {
            None
        } else {
            let host = args
                .target_host
                .as_deref()
                .filter(|host| !host.trim().is_empty())
                .ok_or_else(|| AppError::invalid("普通端口转发必须提供 targetHost"))?;
            let port = args
                .target_port
                .ok_or_else(|| AppError::invalid("普通端口转发必须提供 targetPort"))?;
            Some(
                ForwardEndpoint::new(host, port)
                    .map_err(|_| AppError::invalid("端口转发目标地址或端口无效"))?,
            )
        };
        let name = args
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("{kind:?} {}", args.source_port));
        let proxy_apply_scope = parse_proxy_apply_scope(args.proxy_apply_scope.as_deref())?;
        let remote_access_scope = match args.remote_access_scope.as_deref() {
            Some(scope) => Some(parse_remote_access_scope(scope)?),
            None if is_remote => Some(infer_remote_access_scope(&bind.host)),
            None => None,
        };
        let config = PortForwardConfig {
            host_id,
            name,
            kind,
            bind,
            destination,
            proxy_protocol: (args.proxy_protocol.is_some() || dynamic)
                .then_some(ProxyProtocol::Socks5),
            remote_access_scope,
            proxy_apply_scope,
        };
        config
            .validate()
            .map_err(|_| AppError::invalid("端口转发配置无效"))?;
        let snapshot = services
            .require_operations()?
            .port_forwards
            .start(config, cancellation)
            .await?;
        snapshot_value(snapshot)
    }
    .await;

    match result {
        Ok(data) => success("端口转发已启动".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}

/// 本机或远端监听优先使用对应 bindHost，再用通用 bindHost，scope 不隐式改写地址。
fn select_bind_host(args: &CreateArgs, is_remote: bool) -> Result<String, AppError> {
    let preferred = if is_remote {
        args.remote_bind_host.as_deref()
    } else {
        args.local_bind_host.as_deref()
    };
    Ok(preferred
        .or(args.bind_host.as_deref())
        .filter(|host| !host.trim().is_empty())
        .unwrap_or("127.0.0.1")
        .to_owned())
}

/// 把旧协议 remote+socks5 且无固定目标规范化为远端动态转发。
fn parse_proxy_apply_scope(value: Option<&str>) -> Result<ProxyApplyScope, AppError> {
    match value.unwrap_or("none") {
        "none" => Ok(ProxyApplyScope::None),
        "currentTerminal" => Ok(ProxyApplyScope::CurrentTerminal),
        "futureTerminals" => Ok(ProxyApplyScope::FutureTerminals),
        "userProfile" => Ok(ProxyApplyScope::UserProfile),
        "toolOnly" => Ok(ProxyApplyScope::ToolOnly),
        _ => Err(AppError::invalid("proxyApplyScope 无效")),
    }
}

/// 解析冻结 MCP schema 中的远端监听 scope；它作为请求元数据回显，不替代实际 bind。
fn parse_remote_access_scope(value: &str) -> Result<RemoteAccessScope, AppError> {
    match value {
        "loopback" => Ok(RemoteAccessScope::Loopback),
        "privateNetwork" => Ok(RemoteAccessScope::PrivateNetwork),
        "allInterfaces" => Ok(RemoteAccessScope::AllInterfaces),
        "custom" => Ok(RemoteAccessScope::Custom),
        _ => Err(AppError::invalid("remoteAccessScope 无效")),
    }
}

/// 未显式给出范围时按绑定地址分类；动态地址解析与接口探测不由此用例臆造。
fn infer_remote_access_scope(host: &str) -> RemoteAccessScope {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    {
        return RemoteAccessScope::Loopback;
    }
    if matches!(host, "0.0.0.0" | "::") {
        return RemoteAccessScope::AllInterfaces;
    }
    if host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| matches!(address, std::net::IpAddr::V4(ip) if ip.is_private()))
    {
        return RemoteAccessScope::PrivateNetwork;
    }
    RemoteAccessScope::Custom
}

/// 将小写 wire 值映射为固定的四种转发方向，未知值不会降级为本地转发。
fn parse_kind(value: &str) -> Result<ForwardKind, AppError> {
    match value {
        "local" => Ok(ForwardKind::Local),
        "remote" => Ok(ForwardKind::Remote),
        "dynamic" => Ok(ForwardKind::Dynamic),
        "remoteDynamic" => Ok(ForwardKind::RemoteDynamic),
        _ => Err(AppError::invalid("端口转发 kind 无效")),
    }
}

/// 把已记录实例转成稳定 JSON，保留实际分配端点和运行故障码。
fn snapshot_value(snapshot: PortForwardSnapshot) -> Result<Value, AppError> {
    let forward = snapshot.forward;
    Ok(json!({
        "id": forward.id,
        "config": serde_json::to_value(forward.config)
            .map_err(|_| AppError::infrastructure("端口转发配置无法编码"))?,
        "state": serde_json::to_value(forward.state)
            .map_err(|_| AppError::infrastructure("端口转发状态无法编码"))?,
        "actualBind": serde_json::to_value(snapshot.actual_bind)
            .map_err(|_| AppError::infrastructure("端口转发监听地址无法编码"))?,
        "proxyUrl": snapshot.proxy_url,
        "proxyApplication": serde_json::to_value(snapshot.proxy_application)
            .map_err(|_| AppError::infrastructure("代理应用状态无法编码"))?,
        "failureCode": snapshot.failure_code.map(|code| code.as_str())
    }))
}

/// 列表以一份快照作为 data，并同时把实例投影为 MCP entity。
async fn list(services: &McpServices, arguments: Map<String, Value>) -> CallToolResult {
    let result = async {
        let _: EmptyArgs = parse_args(arguments)?;
        let snapshots = services.require_operations()?.port_forwards.list().await;
        snapshots
            .into_iter()
            .map(snapshot_value)
            .collect::<Result<Vec<_>, _>>()
    }
    .await;
    match result {
        Ok(items) => success(
            format!("当前有 {} 个端口转发", items.len()),
            json!({ "items": items }),
            items,
        ),
        Err(error) => failure(error),
    }
}

/// 停止指定转发；stop 的幂等语义由 application 生命周期管理。
async fn close(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let result = async {
        let args: CloseArgs = parse_args(arguments)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("端口转发关闭已取消"));
        }
        services
            .require_operations()?
            .port_forwards
            .stop(&args.forward_id)
            .await?;
        Ok(json!({ "forwardId": args.forward_id, "closed": true }))
    }
    .await;
    match result {
        Ok(data) => success("端口转发已关闭".to_owned(), data.clone(), vec![data]),
        Err(error) => failure(error),
    }
}
