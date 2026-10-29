//! MCP SSH 命令工具到应用连接端口的适配；本模块不创建 SSH transport 或拼接网络路由。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, CommandStreamHandle, ConnectionIntent, RemoteConnectionLease,
        ShellScriptRequest,
        management::{HostGroupSnapshot, ManagedHost},
    },
};
use kerminal_domain::identity::HostId;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::time::{Instant as TokioInstant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const MIN_TIMEOUT_SECONDS: u64 = 1;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_OUTPUT_BYTES: usize = 16 * 1024;
const MIN_OUTPUT_BYTES: usize = 256;
const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_COMMAND_CHARS: usize = 16 * 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SUMMARY_SAMPLE_CHARS: usize = 160;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolvedCommandArgs {
    command: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    group_id: Option<String>,
    #[serde(default)]
    group_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    proxy_protocol: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HostSelector {
    host_id: Option<String>,
    group_id: Option<String>,
    group_name: Option<String>,
    name: Option<String>,
    host: Option<String>,
    username: Option<String>,
    port: Option<u16>,
}

/// 同一已解析目标同时约束连接身份与兼容响应字段，借用配置避免复制含认证引用的主机记录。
struct SshCommandTarget<'a> {
    host: &'a ManagedHost,
    group_name: Option<&'a str>,
    include_group: bool,
}

/// 两种 SSH 工具共有的单次命令输入；代理只属于远端脚本环境，限额仍在执行入口统一规范化。
struct SshCommandInput<'a> {
    command: &'a str,
    proxy_url: Option<&'a str>,
    proxy_protocol: Option<&'a str>,
    timeout_seconds: Option<u64>,
    max_output_bytes: Option<usize>,
}

struct StreamCapture {
    bytes: Vec<u8>,
    received: usize,
    limit: usize,
}

struct CommandOutput {
    exit_code: Option<i32>,
    success: bool,
    stdout: StreamCapture,
    stderr: StreamCapture,
    duration_ms: u128,
}

impl StreamCapture {
    /// 仅缓存各流的前 N 字节，继续计数并排空后续数据以免远端 SSH channel 被背压阻塞。
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8 * 1024)),
            received: 0,
            limit,
        }
    }

    /// 以饱和计数保留观察到的长度，同时只把上限以内的字节交给外部 MCP host。
    fn push(&mut self, bytes: &[u8]) {
        self.received = self.received.saturating_add(bytes.len());
        let remaining = self.limit.saturating_sub(self.bytes.len());
        let visible = bytes.len().min(remaining);
        self.bytes.extend_from_slice(&bytes[..visible]);
    }

    /// 返回实际保留长度；与旧工具契约一致，stdoutBytes/stderrBytes 表示可见字节数。
    fn captured_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// 标记真实输出超出保留上限，但仍允许命令 reader 将数据流排空。
    fn truncated(&self) -> bool {
        self.received > self.bytes.len()
    }

    /// 只在响应边界以有损 UTF-8 转换，远端原始字节和每流截断边界保持独立。
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// 只保留一个 SSH 执行入口，hostId 与名称定位共享相同限额、消歧及连接回收。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    args: Map<String, Value>,
    cancel: CancellationToken,
) -> CallToolResult {
    match tool_id {
        "ssh.command" => command(services, args, cancel).await,
        _ => failure(AppError::invalid("SSH tool id")),
    }
}

/// 合并入口仍保留合取 selector 与 hostId 优先规则，旧 hostId 请求无需迁移参数。
/// 输入与目标分开建模，避免归并命令参数时改变候选范围或复制主机认证引用。
///
/// @author kongweiguang
pub(crate) async fn command(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancel: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<ResolvedCommandArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let selector = HostSelector::from_args(&args);
    if !selector.has_any() {
        return selector_failure(
            "SSH 目标需要 hostId，或至少一个主机选择条件。",
            "missingTarget",
            json!({}),
            Vec::new(),
            vec!["提供 hostId，或提供 groupName/name/host/username/port 组合后重试。"],
        );
    }
    let host_service = match services.require_host_management() {
        Ok(service) => service,
        Err(error) => return failure(error),
    };
    let hosts = match host_service.list_hosts().await {
        Ok(hosts) => hosts,
        Err(error) => return failure(error),
    };
    let groups = match host_service.load_groups().await {
        Ok(groups) => groups,
        Err(error) => return failure(error),
    };
    let group_names = group_name_index(&groups);
    let resolved = match resolve_host(&selector, &hosts, &group_names) {
        Ok(host) => host,
        Err(TargetResolutionError::NotFound) => {
            let candidates = host_candidates(&hosts, &group_names);
            return selector_failure(
                "未找到匹配的 SSH 主机。",
                "targetNotFound",
                json!({
                    "candidateCount": hosts.len(),
                    "candidates": candidates.iter().take(10).collect::<Vec<_>>(),
                }),
                candidate_entities(&hosts, &group_names),
                vec!["检查 hosts/*.toml 中的主机配置，或提供更明确的 hostId。"],
            );
        }
        Err(TargetResolutionError::Ambiguous(matches)) => {
            let candidates = host_candidates(&matches, &group_names);
            return selector_failure(
                &format!("匹配到 {} 个 SSH 主机，请补充 hostId。", matches.len()),
                "ambiguousTarget",
                json!({
                    "candidateCount": matches.len(),
                    "candidates": candidates.iter().take(10).collect::<Vec<_>>(),
                }),
                candidate_entities(&matches, &group_names),
                vec!["从 candidates 中选择一个 hostId 后重试。"],
            );
        }
        Err(TargetResolutionError::Invalid(error)) => return failure(error),
    };
    let group_name = resolved
        .group_id
        .as_deref()
        .and_then(|id| group_names.get(id))
        .cloned();
    execute(
        services,
        SshCommandTarget {
            host: &resolved,
            group_name: group_name.as_deref(),
            include_group: true,
        },
        SshCommandInput {
            command: &args.command,
            proxy_url: args.proxy_url.as_deref(),
            proxy_protocol: args.proxy_protocol.as_deref(),
            timeout_seconds: args.timeout_seconds,
            max_output_bytes: args.max_output_bytes,
        },
        cancel,
    )
    .await
}

impl HostSelector {
    /// 对可选字符串沿用旧工具的 trim-and-ignore-empty 规则，port 零不视为有效 selector。
    fn from_args(args: &ResolvedCommandArgs) -> Self {
        Self {
            host_id: trimmed(args.host_id.as_deref()),
            group_id: trimmed(args.group_id.as_deref()),
            group_name: trimmed(args.group_name.as_deref()),
            name: trimmed(args.name.as_deref()),
            host: trimmed(args.host.as_deref()),
            username: trimmed(args.username.as_deref()),
            port: args.port.filter(|port| *port != 0),
        }
    }

    /// 拒绝空 selector 集合，防止意外扫描全部主机并产生不可预测的目标选择。
    fn has_any(&self) -> bool {
        self.host_id.is_some()
            || self.group_id.is_some()
            || self.group_name.is_some()
            || self.name.is_some()
            || self.host.is_some()
            || self.username.is_some()
            || self.port.is_some()
    }
}

/// 只保留非空首尾 trim 后的值，避免空白条件把 selector 误变成全匹配。
fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

enum TargetResolutionError {
    NotFound,
    Ambiguous(Vec<ManagedHost>),
    Invalid(AppError),
}

/// hostId 使用精确身份匹配；其它条件按旧逻辑合取并对名称、主机和用户名忽略大小写。
fn resolve_host(
    selector: &HostSelector,
    hosts: &[ManagedHost],
    group_names: &HashMap<String, String>,
) -> Result<ManagedHost, TargetResolutionError> {
    if let Some(id) = selector.host_id.as_deref() {
        let parsed = HostId::parse(id).map_err(|_| {
            TargetResolutionError::Invalid(AppError::invalid("SSH host id selector"))
        })?;
        return hosts
            .iter()
            .find(|host| host.id == parsed)
            .cloned()
            .ok_or(TargetResolutionError::NotFound);
    }
    let matches = hosts
        .iter()
        .filter(|host| {
            selector
                .group_id
                .as_deref()
                .is_none_or(|value| host.group_id.as_deref() == Some(value))
                && selector.group_name.as_deref().is_none_or(|value| {
                    host.group_id
                        .as_deref()
                        .and_then(|id| group_names.get(id))
                        .is_some_and(|group_name| group_name.eq_ignore_ascii_case(value))
                })
                && selector
                    .name
                    .as_deref()
                    .is_none_or(|value| host.name.eq_ignore_ascii_case(value))
                && selector
                    .host
                    .as_deref()
                    .is_none_or(|value| host.endpoint.host.eq_ignore_ascii_case(value))
                && selector
                    .username
                    .as_deref()
                    .is_none_or(|value| host.endpoint.username.eq_ignore_ascii_case(value))
                && selector
                    .port
                    .is_none_or(|value| host.endpoint.port == value)
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(TargetResolutionError::NotFound),
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => Err(TargetResolutionError::Ambiguous(matches)),
    }
}

/// 为 groupName selector 和候选响应建立稳定 ID→名称索引。
fn group_name_index(groups: &HostGroupSnapshot) -> HashMap<String, String> {
    groups
        .groups
        .iter()
        .map(|group| (group.id.clone(), group.name.clone()))
        .collect()
}

/// 返回与旧工具相同的 host/group 摘要，不把认证模式或 vault 引用带到 Agent 响应。
fn host_candidates(hosts: &[ManagedHost], group_names: &HashMap<String, String>) -> Vec<Value> {
    hosts
        .iter()
        .map(|host| {
            json!({
                "hostId": host.id.as_str(),
                "groupId": host.group_id,
                "groupName": host.group_id.as_deref().and_then(|id| group_names.get(id)),
                "name": host.name,
                "host": host.endpoint.host,
                "port": host.endpoint.port,
                "username": host.endpoint.username,
            })
        })
        .collect()
}

/// 让解析失败的 entities 与旧工具的候选主机引用保持一致，同时统一限制返回数量。
fn candidate_entities(hosts: &[ManagedHost], group_names: &HashMap<String, String>) -> Vec<Value> {
    hosts
        .iter()
        .take(10)
        .map(|host| {
            let group_name = host
                .group_id
                .as_deref()
                .and_then(|group_id| group_names.get(group_id))
                .map(String::as_str);
            host_entity(host, group_name, true)
        })
        .collect()
}

/// 两个 wire 入口共享脚本、代理环境与限额校验；借用输入只活到本次请求，不扩散到连接 owner。
/// 连接与执行继续共享一个绝对 deadline 和 child token，目标上下文只携带已解析的固定身份。
///
/// @author kongweiguang
async fn execute(
    services: &McpServices,
    target: SshCommandTarget<'_>,
    input: SshCommandInput<'_>,
    cancel: CancellationToken,
) -> CallToolResult {
    let script = match normalize_script(input.command) {
        Ok(script) => script,
        Err(error) => return failure(error),
    };
    let environment = match proxy_environment(input.proxy_url, input.proxy_protocol) {
        Ok(environment) => environment,
        Err(error) => return failure(error),
    };
    let timeout_seconds = input
        .timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS);
    let max_output_bytes = input
        .max_output_bytes
        .unwrap_or(DEFAULT_OUTPUT_BYTES)
        .clamp(MIN_OUTPUT_BYTES, MAX_OUTPUT_BYTES);
    let host_entity = host_entity(target.host, target.group_name, target.include_group);
    let remote = match services.require_remote_connections() {
        Ok(remote) => remote,
        Err(error) => return failure(error),
    };
    let deadline = TokioInstant::now() + Duration::from_secs(timeout_seconds);
    let operation_cancel = cancel.child_token();
    let lease = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            operation_cancel.cancel();
            return failure(AppError::cancelled("SSH command was cancelled"));
        }
        result = timeout_at(deadline, remote.connect(target.host.id.clone(), ConnectionIntent::Exec, operation_cancel.clone())) => {
            match result {
                Ok(Ok(lease)) => lease,
                Ok(Err(error)) => return remote_failure(error, vec![host_entity]),
                Err(_) => {
                    operation_cancel.cancel();
                    return remote_failure(AppError::timeout("SSH command connection"), vec![host_entity]);
                }
            }
        }
    };
    execute_on_lease(
        lease,
        target,
        ShellScriptRequest {
            script,
            environment,
        },
        max_output_bytes,
        deadline,
        cancel,
        operation_cancel,
    )
    .await
}

/// 已认证 lease 独占本次 exec；连接目标及响应身份沿用同一借用上下文，避免在执行阶段重新解析。
/// 取消、deadline 与清理顺序保持独立且显式，任何退出分支仍负责关闭本次 channel 与 lease。
///
/// @author kongweiguang
async fn execute_on_lease(
    lease: RemoteConnectionLease,
    target: SshCommandTarget<'_>,
    request: ShellScriptRequest,
    max_output_bytes: usize,
    deadline: TokioInstant,
    cancel: CancellationToken,
    operation_cancel: CancellationToken,
) -> CallToolResult {
    let host_entity = host_entity(target.host, target.group_name, target.include_group);
    let mut stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            operation_cancel.cancel();
            close_lease(&lease, "SSH command cancelled").await;
            return failure(AppError::cancelled("SSH command was cancelled"));
        }
        result = timeout_at(deadline, lease.exec_shell(request, operation_cancel.clone())) => {
            match result {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    operation_cancel.cancel();
                    close_lease(&lease, "SSH command setup failed").await;
                    return remote_failure(error, vec![host_entity]);
                }
                Err(_) => {
                    operation_cancel.cancel();
                    close_lease(&lease, "SSH command setup timed out").await;
                    return remote_failure(AppError::timeout("SSH command setup"), vec![host_entity]);
                }
            }
        }
    };
    let started = Instant::now();
    let output = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            operation_cancel.cancel();
            close_stream_and_lease(&mut stream, &lease, "SSH command cancelled").await;
            return failure(AppError::cancelled("SSH command was cancelled"));
        }
        result = timeout_at(deadline, collect_output(&mut stream, max_output_bytes, started)) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    operation_cancel.cancel();
                    close_stream_and_lease(&mut stream, &lease, "SSH command stream failed").await;
                    return remote_failure(error, vec![host_entity]);
                }
                Err(_) => {
                    operation_cancel.cancel();
                    close_stream_and_lease(&mut stream, &lease, "SSH command timed out").await;
                    return remote_failure(AppError::timeout("SSH command execution"), vec![host_entity]);
                }
            }
        }
    };
    close_stream_and_lease(&mut stream, &lease, "SSH command completed").await;
    command_result(
        target.host,
        target.group_name,
        target.include_group,
        output,
        max_output_bytes,
    )
}

/// 消费两条独立输出流直至真实 Exited 事件；超限后仍继续排空，避免 SSH 服务端停止写入。
async fn collect_output(
    stream: &mut CommandStreamHandle,
    max_output_bytes: usize,
    started: Instant,
) -> AppResult<CommandOutput> {
    let mut stdout = StreamCapture::new(max_output_bytes);
    let mut stderr = StreamCapture::new(max_output_bytes);
    loop {
        match stream.next_event().await? {
            Some(CommandEvent::Stdout(bytes)) => stdout.push(&bytes),
            Some(CommandEvent::Stderr(bytes)) => stderr.push(&bytes),
            Some(CommandEvent::Exited { code, signal }) => {
                let exit_code = code.and_then(|code| i32::try_from(code).ok());
                return Ok(CommandOutput {
                    success: exit_code == Some(0) && signal.is_none(),
                    exit_code,
                    stdout,
                    stderr,
                    duration_ms: started.elapsed().as_millis(),
                });
            }
            None => {
                return Err(AppError::infrastructure(
                    "SSH command stream ended before exit status",
                ));
            }
        }
    }
}

/// 以同一绝对期限关闭本次 channel 与其独占 lease，避免相对超时累计或丢弃 owner 清理。
async fn close_stream_and_lease(
    stream: &mut CommandStreamHandle,
    lease: &RemoteConnectionLease,
    reason: &'static str,
) {
    let deadline = TokioInstant::now() + CLOSE_TIMEOUT;
    if let Err(error) = stream.close_until(deadline).await {
        tracing::warn!(%error, "SSH command channel close failed");
    }
    close_lease_until(lease, deadline, reason).await;
}

/// 将 MCP 命令清理 deadline 交给连接 owner，超时取消与 russh task join 由 owner 收口。
async fn close_lease(lease: &RemoteConnectionLease, reason: &'static str) {
    close_lease_until(lease, TokioInstant::now() + CLOSE_TIMEOUT, reason).await;
}

/// 在既有关闭期限内由应用 owner 执行最终 cleanup，不以外层 timeout 丢弃 close future。
async fn close_lease_until(
    lease: &RemoteConnectionLease,
    deadline: TokioInstant,
    reason: &'static str,
) {
    if let Err(error) = lease.close_until(deadline, reason).await {
        tracing::warn!(%error, "SSH command lease close failed");
    }
}

/// 结构化输出和摘要读取同一 CommandOutput，保持各流长度、截断与退出状态对应，且不复制捕获缓冲。
/// 非零退出继续使用旧版 remoteCommandFailed 投影，原始响应字段和摘要脱敏边界保持分离。
///
/// @author kongweiguang
fn command_result(
    host: &ManagedHost,
    group_name: Option<&str>,
    include_group: bool,
    output: CommandOutput,
    max_output_bytes: usize,
) -> CallToolResult {
    let host_data = host_details(host, include_group);
    let host_entity = host_entity(host, group_name, include_group);
    let stdout = output.stdout.text();
    let stderr = output.stderr.text();
    let exit_code = output.exit_code;
    let stdout_bytes = output.stdout.captured_bytes();
    let stderr_bytes = output.stderr.captured_bytes();
    let stdout_truncated = output.stdout.truncated();
    let stderr_truncated = output.stderr.truncated();
    let duration_ms = output.duration_ms;
    let summary = summarize_command(host, &output, &stdout, &stderr);
    let data = json!({
        "hostId": host.id.as_str(),
        "groupId": if include_group { host.group_id.as_deref() } else { None },
        "host": host_data,
        "command": { "maxOutputBytes": max_output_bytes },
        "output": {
            "success": output.success,
            "exitCode": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "stdoutBytes": stdout_bytes,
            "stderrBytes": stderr_bytes,
            "stdoutTruncated": stdout_truncated,
            "stderrTruncated": stderr_truncated,
            "durationMs": duration_ms,
        },
    });
    let entities = vec![host_entity];
    if output.success {
        success(summary, data, entities)
    } else {
        let exit_code = exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "未知".to_owned());
        CallToolResult::structured_error(json!({
            "summary": summary,
            "error": format!("远程命令退出码 {exit_code}"),
            "data": data,
            "entities": entities,
            "errorKind": "remoteCommandFailed",
            "recoverable": true,
            "nextHints": ["检查远程命令、权限和工作目录后重试。"],
        }))
    }
}

/// 只在 MCP 响应边界构造 host DTO，防止 group 元数据缺失时伪造关联关系。
fn host_details(host: &ManagedHost, include_group: bool) -> Value {
    json!({
        "id": host.id.as_str(),
        "groupId": if include_group { host.group_id.as_deref() } else { None },
        "name": host.name,
        "host": host.endpoint.host,
        "port": host.endpoint.port,
        "username": host.endpoint.username,
    })
}

/// 生成不包含认证配置、密钥路径或 vault ref 的实体引用。
fn host_entity(host: &ManagedHost, group_name: Option<&str>, include_group: bool) -> Value {
    let mut entity = json!({
        "type": "remoteHost",
        "id": host.id.as_str(),
        "name": host.name,
        "host": host.endpoint.host,
        "port": host.endpoint.port,
        "username": host.endpoint.username,
    });
    if include_group {
        entity["groupId"] = json!(host.group_id.as_deref());
        entity["groupName"] = json!(group_name);
    }
    entity
}

/// 把远程连接失败映射为冻结工具契约的 recoverable remoteFailure，保留脱敏应用错误文案。
fn remote_failure(error: AppError, entities: Vec<Value>) -> CallToolResult {
    let message = error.public_message();
    CallToolResult::structured_error(json!({
        "summary": message,
        "error": message,
        "data": {},
        "entities": entities,
        "errorKind": "remoteFailure",
        "recoverable": true,
        "nextHints": ["检查 SSH 主机、凭据、网络和主机密钥信任状态后重试。"],
    }))
}

/// 保留旧版 target-resolution errorKind 和候选字段；输入字符串仅在 JSON 中作为数据返回。
fn selector_failure(
    message: &str,
    error_kind: &str,
    data: Value,
    entities: Vec<Value>,
    next_hints: Vec<&str>,
) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "summary": message,
        "error": message,
        "data": data,
        "entities": entities,
        "errorKind": error_kind,
        "recoverable": true,
        "nextHints": next_hints,
    }))
}

/// 复制旧 MCP proxyUrl 语义：只设置远端 HTTP(S)/SOCKS 环境，不修改 SSH proxy/jump route。
fn proxy_environment(
    proxy_url: Option<&str>,
    proxy_protocol: Option<&str>,
) -> AppResult<BTreeMap<String, String>> {
    let Some(proxy_url) = proxy_url else {
        return Ok(BTreeMap::new());
    };
    if proxy_url
        .chars()
        .any(|character| matches!(character, '\0' | '\n' | '\r'))
    {
        return Err(AppError::invalid(
            "proxyUrl cannot contain control characters",
        ));
    }
    if !["http://", "https://", "socks5h://"]
        .iter()
        .any(|prefix| proxy_url.starts_with(prefix))
    {
        return Err(AppError::invalid("proxyUrl scheme is unsupported"));
    }
    let protocol = proxy_protocol.unwrap_or_else(|| {
        if proxy_url.starts_with("socks5h://") {
            "socks5"
        } else {
            "http"
        }
    });
    let mut environment = BTreeMap::new();
    match protocol {
        "http" => {
            for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                environment.insert(name.to_owned(), proxy_url.to_owned());
            }
        }
        "socks5" => {
            environment.insert("ALL_PROXY".to_owned(), proxy_url.to_owned());
            environment.insert("all_proxy".to_owned(), proxy_url.to_owned());
        }
        _ => return Err(AppError::invalid("proxyProtocol must be http or socks5")),
    }
    for name in ["NO_PROXY", "no_proxy"] {
        environment.insert(name.to_owned(), "localhost,127.0.0.1".to_owned());
    }
    Ok(environment)
}

/// 延续冻结服务的脚本规范化：统一换行、裁边空白、拒绝 NUL/空脚本并限制 16 Ki 字符。
fn normalize_script(command: &str) -> AppResult<String> {
    if command.contains('\0') {
        return Err(AppError::invalid("SSH command cannot contain NUL"));
    }
    let normalized = command.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return Err(AppError::invalid("SSH command is required"));
    }
    if trimmed.chars().count() > MAX_COMMAND_CHARS {
        return Err(AppError::invalid("SSH command exceeds its length limit"));
    }
    Ok(if trimmed.ends_with('\n') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}\n")
    })
}

/// 复用已有输出观测作为摘要上下文，防止平铺参数把 stdout/stderr 指标错配；文本沿用响应边界的解码结果。
/// 保留旧版中英布局与摘要脱敏逻辑，不再复制或解码原始流缓冲。
///
/// @author kongweiguang
fn summarize_command(
    host: &ManagedHost,
    output: &CommandOutput,
    stdout: &str,
    stderr: &str,
) -> String {
    let exit_code = output
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "未知".to_owned());
    format!(
        "远程命令已执行：{}（{}@{}:{}），退出码：{}，{}，{}，耗时：{} ms。",
        host.name,
        host.endpoint.username,
        host.endpoint.host,
        host.endpoint.port,
        exit_code,
        summarize_stream(
            "stdout",
            stdout,
            output.stdout.captured_bytes(),
            output.stdout.truncated(),
        ),
        summarize_stream(
            "stderr",
            stderr,
            output.stderr.captured_bytes(),
            output.stderr.truncated(),
        ),
        output.duration_ms,
    )
}

/// 限制摘要只展示前四行和 160 字符，掩码常见凭据赋值，原始字段仍遵循旧 JSON 契约。
fn summarize_stream(label: &str, text: &str, bytes: usize, truncated: bool) -> String {
    let sample = text.lines().take(4).collect::<Vec<_>>().join(" ");
    let sample = sample.split_whitespace().collect::<Vec<_>>().join(" ");
    let (sample, redacted) = redact_summary_sample(&sample);
    let sample = sample
        .chars()
        .take(MAX_SUMMARY_SAMPLE_CHARS)
        .collect::<String>();
    let truncated_label = if truncated { "，已截断" } else { "" };
    let redacted_label = if redacted { "，已脱敏" } else { "" };
    if sample.is_empty() {
        format!("{label}：{bytes} 字节{truncated_label}{redacted_label}")
    } else {
        format!("{label}：{bytes} 字节{truncated_label}{redacted_label}，片段：{sample}")
    }
}

/// 对摘要副本中 password/token/secret/authorization 等常见赋值隐藏等号后内容。
fn redact_summary_sample(value: &str) -> (String, bool) {
    let markers = [
        "password=",
        "passwd=",
        "token=",
        "secret=",
        "api_key=",
        "apikey=",
        "access_key=",
        "authorization=",
        "authorization: bearer ",
    ];
    let mut output = value.to_owned();
    let mut redacted = false;
    for marker in markers {
        let marker_lower = marker.to_ascii_lowercase();
        let mut search_from = 0;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(relative) = lower[search_from..].find(&marker_lower) else {
                break;
            };
            let start = search_from + relative + marker.len();
            let end = lower[start..]
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, '&' | ';' | ',')
                })
                .map(|relative| start + relative)
                .unwrap_or(output.len());
            if start < end {
                output.replace_range(start..end, "[redacted]");
                redacted = true;
            }
            search_from = start + "[redacted]".len();
            if search_from >= output.len() {
                break;
            }
        }
    }
    (output, redacted)
}

/// 转成有界 stdout/stderr capture；测试不依赖 SSH 网络或外部服务。
#[cfg(test)]
#[path = "../tests/unit/ssh_tools.rs"]
mod tests;
