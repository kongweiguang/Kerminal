//! Docker 与 Podman 的进程外命令用例。
//!
//! 所有运行时调用都通过结构化参数进入 command port，交互式 exec 交给共享会话 registry。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kerminal_domain::{
    operations::{
        ComposeAction, ComposeActionReceipt, ComposeMetadata, ComposeProjectTarget,
        ContainerAction, ContainerInspect, ContainerListQuery, ContainerMutationReceipt,
        ContainerRuntime, ContainerStats, ContainerStatus, ContainerSummary, ContainerTarget,
    },
    sessions::{
        ContainerSessionTarget as SessionContainerTarget, SessionInfo, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult, PublicErrorCode,
    sessions::{CreateSessionRequest, SessionService},
};

use super::{
    CommandExecutionLimits, CommandExecutionTarget, CommandInvocation, OperationsCommandExecutor,
    execute_with_deadline, runtime_program,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const JSON_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const LOG_OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

/// Docker/Podman 运行时用例；仅记住已核实的完整 Compose 目标，不持有进程或连接。
#[derive(Clone)]
pub struct ContainerRuntimeService {
    commands: Arc<dyn OperationsCommandExecutor>,
    sessions: Arc<dyn SessionService>,
    compose_projects: Arc<Mutex<Vec<ComposeProjectTarget>>>,
}

impl ContainerRuntimeService {
    /// 注入同一结构化命令端口和唯一会话 registry，避免容器终端另起读写循环。
    pub fn new(
        commands: Arc<dyn OperationsCommandExecutor>,
        sessions: Arc<dyn SessionService>,
    ) -> Self {
        Self {
            commands,
            sessions,
            compose_projects: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 按运行时实际输出格式解析列表；畸形或截断输出不能降级为空列表。
    ///
    /// Docker 输出逐行 JSON 对象，Podman 3.x 输出一个 JSON 数组，因此由解析入口按
    /// 已选 runtime 分流，避免把 Podman 数组首行 `[` 误当成容器对象。
    ///
    /// @author kongweiguang
    pub async fn list(
        &self,
        query: ContainerListQuery,
        cancel: CancellationToken,
    ) -> AppResult<Vec<ContainerSummary>> {
        let mut arguments = vec!["ps".to_owned(), "--no-trunc".to_owned()];
        if query.include_stopped {
            arguments.push("--all".to_owned());
        }
        arguments.extend(["--format".to_owned(), "{{json .}}".to_owned()]);
        let host_id = query.host_id.clone();
        let output = self
            .execute_runtime(
                host_id.clone(),
                query.runtime,
                arguments,
                JSON_OUTPUT_LIMIT,
                cancel,
            )
            .await?;
        ensure_complete(&output)?;
        ensure_success(&output)?;
        let containers = parse_container_list_output(&output.stdout, host_id, query.runtime)?;
        for container in &containers {
            let Some(metadata) = container.compose.as_ref() else {
                continue;
            };
            let (Some(project), Some(working_dir)) =
                (metadata.project.as_ref(), metadata.working_dir.as_ref())
            else {
                continue;
            };
            let target = ComposeProjectTarget {
                host_id: container.target.host_id.clone(),
                runtime: container.target.runtime,
                project: project.clone(),
                working_dir: working_dir.clone(),
                config_files: metadata.config_files.clone(),
            };
            if validate_compose_target(&target).is_ok() {
                self.remember_compose_project(target)?;
            }
        }
        Ok(containers)
    }

    /// 从应用共享目录返回精确目标；拆除后无容器标签也可再次启动，不按项目名猜测路径。
    pub fn known_compose_projects(
        &self,
        host_id: &Option<kerminal_domain::identity::HostId>,
        runtime: ContainerRuntime,
    ) -> AppResult<Vec<ComposeProjectTarget>> {
        let projects = self
            .compose_projects
            .lock()
            .map_err(|_| AppError::infrastructure("Compose 项目目录不可用"))?;
        Ok(projects
            .iter()
            .filter(|target| &target.host_id == host_id && target.runtime == runtime)
            .cloned()
            .collect())
    }

    /// 只记录完整且已校验的执行范围，配置文件顺序参与身份判定，不能合并同名项目。
    fn remember_compose_project(&self, target: ComposeProjectTarget) -> AppResult<()> {
        let mut projects = self
            .compose_projects
            .lock()
            .map_err(|_| AppError::infrastructure("Compose 项目目录不可用"))?;
        if !projects.contains(&target) {
            if projects.len() >= 2048 {
                return Err(AppError::conflict(
                    "Compose 项目目录已满，请重启后重新读取目标",
                ));
            }
            projects.push(target);
        }
        Ok(())
    }

    /// 读取完整 inspect JSON；容器标识先做约束校验，避免标志参数和路径歧义。
    pub async fn inspect(
        &self,
        target: ContainerTarget,
        cancel: CancellationToken,
    ) -> AppResult<ContainerInspect> {
        validate_container_id(&target.container_id)?;
        let output = self
            .execute_runtime(
                target.host_id.clone(),
                target.runtime,
                vec![
                    "inspect".to_owned(),
                    "--type".to_owned(),
                    "container".to_owned(),
                    target.container_id.clone(),
                ],
                JSON_OUTPUT_LIMIT,
                cancel,
            )
            .await?;
        ensure_complete(&output)?;
        ensure_success(&output)?;
        let document: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| AppError::infrastructure("容器检查结果无法解析"))?;
        let item = document
            .as_array()
            .and_then(|items| items.first())
            .ok_or_else(|| AppError::not_found("容器不存在"))?;
        parse_container_inspect(item, target)
    }

    /// 读取单次 stats，不启动后台轮询；仅对 Podman 的精确 cgroup v2 限制补充安全原因。
    ///
    /// stderr 的其它内容继续保持远端退出错误，避免泄露任意主机诊断文本。
    pub async fn stats(
        &self,
        target: ContainerTarget,
        cancel: CancellationToken,
    ) -> AppResult<ContainerStats> {
        validate_container_id(&target.container_id)?;
        let output = self
            .execute_runtime(
                target.host_id.clone(),
                target.runtime,
                vec![
                    "stats".to_owned(),
                    "--no-stream".to_owned(),
                    "--format".to_owned(),
                    "{{json .}}".to_owned(),
                    target.container_id.clone(),
                ],
                JSON_OUTPUT_LIMIT,
                cancel,
            )
            .await?;
        ensure_complete(&output)?;
        ensure_stats_success(&output, target.runtime)?;
        let line = output
            .stdout
            .split(|byte| *byte == b'\n')
            .find(|line| !line.iter().all(u8::is_ascii_whitespace))
            .ok_or_else(|| AppError::not_found("容器统计结果为空"))?;
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| AppError::infrastructure("容器统计结果无法解析"))?;
        Ok(ContainerStats {
            target,
            cpu_percent: json_string(&value, &["CPUPerc", "cpuPercent"]),
            memory_percent: json_string(&value, &["MemPerc", "memoryPercent"]),
            memory_usage: json_string(&value, &["MemUsage", "memoryUsage"]),
            network_io: json_string(&value, &["NetIO", "networkIO"]),
            block_io: json_string(&value, &["BlockIO", "blockIO"]),
            pids: json_string(&value, &["PIDs", "Pids", "pids"]),
            raw: String::from_utf8_lossy(line).into_owned(),
        })
    }

    /// 查询有界尾部日志；分别保留 stdout/stderr，不把两条流拼成错误顺序。
    pub async fn logs(
        &self,
        target: ContainerTarget,
        tail_lines: u32,
        cancel: CancellationToken,
    ) -> AppResult<ContainerLogs> {
        validate_container_id(&target.container_id)?;
        let tail_lines = tail_lines.clamp(1, 10_000);
        let output = self
            .execute_runtime(
                target.host_id.clone(),
                target.runtime,
                vec![
                    "logs".to_owned(),
                    "--timestamps".to_owned(),
                    "--tail".to_owned(),
                    tail_lines.to_string(),
                    target.container_id.clone(),
                ],
                LOG_OUTPUT_LIMIT,
                cancel,
            )
            .await?;
        ensure_success(&output)?;
        Ok(ContainerLogs {
            target,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            truncated: output.truncated,
        })
    }

    /// 执行有限生命周期动作；删除不存在的容器按幂等成功返回 applied=false。
    pub async fn mutate(
        &self,
        target: ContainerTarget,
        action: ContainerAction,
        cancel: CancellationToken,
    ) -> AppResult<ContainerMutationReceipt> {
        self.mutate_with_force(target, action, true, cancel).await
    }

    /// 删除容器时把强制标记作为显式策略，且命令回执丢失时要求调用方先回读状态。
    pub async fn mutate_with_force(
        &self,
        target: ContainerTarget,
        action: ContainerAction,
        force: bool,
        cancel: CancellationToken,
    ) -> AppResult<ContainerMutationReceipt> {
        validate_container_id(&target.container_id)?;
        let mut arguments = match action {
            ContainerAction::Start => vec!["start".to_owned()],
            ContainerAction::Stop => vec!["stop".to_owned(), "--time".to_owned(), "10".to_owned()],
            ContainerAction::Restart => {
                vec!["restart".to_owned(), "--time".to_owned(), "10".to_owned()]
            }
            ContainerAction::Remove => {
                let mut arguments = vec!["rm".to_owned()];
                if force {
                    arguments.push("--force".to_owned());
                }
                arguments
            }
        };
        arguments.push(target.container_id.clone());
        let output = self
            .execute_runtime(
                target.host_id.clone(),
                target.runtime,
                arguments,
                64 * 1024,
                cancel,
            )
            .await
            .map_err(map_mutation_outcome_error)?;
        let missing_remove = action == ContainerAction::Remove
            && !output.stderr.is_empty()
            && String::from_utf8_lossy(&output.stderr)
                .to_ascii_lowercase()
                .contains("no such container");
        if output.exit_code != Some(0) && !missing_remove {
            return Err(AppError::remote_exit(output.exit_code));
        }
        Ok(ContainerMutationReceipt {
            target,
            action,
            applied: !missing_remove,
            refresh_required: true,
        })
    }

    /// 根据完整目标执行 Compose；Stop 保留容器，Down 拆除容器和项目网络但从不附加删卷参数。
    pub async fn compose_action(
        &self,
        target: ComposeProjectTarget,
        action: ComposeAction,
        cancel: CancellationToken,
    ) -> AppResult<ComposeActionReceipt> {
        validate_compose_target(&target)?;
        self.remember_compose_project(target.clone())?;
        let mut arguments = vec![
            "compose".to_owned(),
            "--project-name".to_owned(),
            target.project.clone(),
        ];
        for file in &target.config_files {
            arguments.extend(["--file".to_owned(), file.clone()]);
        }
        match action {
            ComposeAction::Up => arguments.extend(["up".to_owned(), "--detach".to_owned()]),
            ComposeAction::Stop => arguments.push("stop".to_owned()),
            ComposeAction::Down => arguments.push("down".to_owned()),
            ComposeAction::Restart => arguments.push("restart".to_owned()),
        }
        let output = execute_with_deadline(
            self.commands.as_ref(),
            target
                .host_id
                .clone()
                .map_or(CommandExecutionTarget::Local, CommandExecutionTarget::Host),
            CommandInvocation {
                program: runtime_program(target.runtime).to_owned(),
                arguments,
                environment: Default::default(),
                working_directory: Some(target.working_dir.clone()),
            },
            CommandExecutionLimits {
                timeout: Duration::from_secs(10 * 60),
                max_output_bytes: 2 * 1024 * 1024,
            },
            cancel,
        )
        .await
        .map_err(map_mutation_outcome_error)?;
        ensure_complete(&output)?;
        ensure_success(&output)?;
        Ok(ComposeActionReceipt {
            target,
            action,
            refresh_required: true,
        })
    }

    /// 在唯一会话 registry 中创建可取消的容器 exec PTY；目标 host_id=None 明确表示本机 CLI。
    ///
    /// 取消信号必须贯穿 SessionManager 的 provider 打开过程，避免 UI 目标代次失效后留下未发布 PTY。
    ///
    /// @author kongweiguang
    pub async fn open_exec(
        &self,
        target: ContainerTarget,
        command: Vec<String>,
        owner: SessionOwner,
        size: TerminalSize,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        validate_container_id(&target.container_id)?;
        if command.is_empty()
            || command
                .iter()
                .any(|part| part.is_empty() || part.chars().any(char::is_control))
        {
            return Err(AppError::invalid("容器 exec 命令无效"));
        }
        let session_target = SessionTarget::Container(SessionContainerTarget {
            host_id: target.host_id,
            runtime: target.runtime,
            container_id: target.container_id,
            command: command.into_iter().map(Into::into).collect(),
        });
        self.sessions
            .create_cancellable(
                CreateSessionRequest {
                    target: session_target,
                    owner,
                    size,
                },
                cancellation,
            )
            .await
    }

    /// 以 argv 调用固定 runtime 程序；宿主选择为本地 CLI 或已保存 SSH 主机。
    async fn execute_runtime(
        &self,
        host_id: Option<kerminal_domain::identity::HostId>,
        runtime: ContainerRuntime,
        arguments: Vec<String>,
        max_output_bytes: usize,
        cancel: CancellationToken,
    ) -> AppResult<super::CommandExecutionResult> {
        execute_with_deadline(
            self.commands.as_ref(),
            host_id.map_or(CommandExecutionTarget::Local, CommandExecutionTarget::Host),
            CommandInvocation::new(runtime_program(runtime), arguments),
            CommandExecutionLimits {
                timeout: COMMAND_TIMEOUT,
                max_output_bytes,
            },
            cancel,
        )
        .await
    }
}

/// 命令进入运行时边界后，取消、超时或连接故障都可能丢失已提交动作的回执。
fn map_mutation_outcome_error(error: AppError) -> AppError {
    match error.public_code() {
        PublicErrorCode::Cancelled | PublicErrorCode::Timeout | PublicErrorCode::Infrastructure => {
            AppError::outcome_unknown_with_source(
                "容器变更结果未知，请先刷新容器状态再决定是否重试",
                error,
            )
        }
        _ => error,
    }
}

/// 标签来自运行时输出，按结构校验后才允许形成 Compose 工作目录和配置 argv。
fn validate_compose_target(target: &ComposeProjectTarget) -> AppResult<()> {
    if target.project.is_empty()
        || target.project.len() > 128
        || !target
            .project
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !is_absolute_compose_directory(&target.working_dir)
        || target.working_dir.len() > 4096
        || target.working_dir.chars().any(char::is_control)
        || target.config_files.is_empty()
        || target.config_files.len() > 32
        || target
            .config_files
            .iter()
            .any(|path| path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control))
    {
        return Err(AppError::invalid("Compose 项目元数据无效"));
    }
    Ok(())
}

/// 标签必须携带绝对执行目录；跨主机接受 POSIX、盘符和 UNC 表示，但不猜测相对路径的基准。
fn is_absolute_compose_directory(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }
    let bytes = path.as_bytes();
    let drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let unc_path = path.strip_prefix("\\\\").is_some_and(|tail| {
        let mut parts = tail.split('\\').filter(|part| !part.is_empty());
        parts.next().is_some() && parts.next().is_some()
    });
    drive_path || unc_path
}

/// 容器日志流保持两个输出通道和截断标记，调用方可以在 UI 中分别呈现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerLogs {
    pub target: ContainerTarget,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// 校验 container id/name 的保守字符集，避免 flag 解析和跨平台路径输入歧义。
fn validate_container_id(value: &str) -> AppResult<()> {
    let mut characters = value.chars();
    let valid_first = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric());
    if !valid_first
        || value.len() > 128
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
    {
        return Err(AppError::invalid("容器标识格式无效"));
    }
    Ok(())
}

/// 拒绝截断后的结构化输出，避免把不完整 JSON 当成成功列表或状态。
fn ensure_complete(output: &super::CommandExecutionResult) -> AppResult<()> {
    if output.truncated {
        Err(AppError::infrastructure("容器命令输出超过读取上限"))
    } else {
        Ok(())
    }
}

/// 非零退出码作为远端命令结果返回，不吞掉 Docker/Podman 操作失败。
fn ensure_success(output: &super::CommandExecutionResult) -> AppResult<()> {
    if output.exit_code == Some(0) {
        Ok(())
    } else {
        Err(AppError::remote_exit(output.exit_code))
    }
}

/// 仅把 Podman 已确认的 cgroup v2 限制翻译为具体文案，其他退出仍保留远端退出契约。
///
/// 远端 stderr 不可信且可能包含主机信息；只允许 CLI 的一个固定前缀，再对诊断全文精确匹配。
/// 保留退出码分类并固定公开文案，不回显或搜索任意远端 stderr。
/// 保留 `RemoteExit` 机器码，使桌面与 MCP 调用方不必迁移既有分支契约。
/// @author kongweiguang
fn ensure_stats_success(
    output: &super::CommandExecutionResult,
    runtime: ContainerRuntime,
) -> AppResult<()> {
    if output.exit_code == Some(0) {
        return Ok(());
    }

    let mut stderr = output.stderr.as_slice();
    while stderr
        .last()
        .is_some_and(|byte| matches!(*byte, b'\r' | b'\n'))
    {
        stderr = &stderr[..stderr.len() - 1];
    }
    let stderr = stderr.strip_prefix(b"Error: ").unwrap_or(stderr);
    if runtime == ContainerRuntime::Podman
        && output.exit_code == Some(125)
        && stderr == b"Container stats resource only available for cgroup v2"
    {
        let source = AppError::remote_exit(Some(125));
        return Err(AppError::DiagnosticSource {
            code: PublicErrorCode::RemoteExit,
            message: "Podman 容器统计不可用：该环境需要 cgroup v2",
            source: Arc::new(source),
        });
    }

    Err(AppError::remote_exit(output.exit_code))
}

/// 按 Docker 格式化对象或 Podman `ListContainer` 对象的实际字段形状构造摘要。
///
/// @author kongweiguang
fn parse_container_summary(
    value: &Value,
    host_id: Option<kerminal_domain::identity::HostId>,
    runtime: ContainerRuntime,
) -> AppResult<ContainerSummary> {
    if !value.is_object() {
        return Err(container_list_shape_error());
    }
    let (id, name, image, status_text, labels, ports) = match runtime {
        ContainerRuntime::Docker => (
            required_json_string(value, "ID")?,
            required_json_string(value, "Names")?,
            required_json_string(value, "Image")?,
            required_json_string(value, "Status")?,
            parse_docker_labels(required_json_string(value, "Labels")?.as_str())?,
            parse_docker_ports(required_json_string(value, "Ports")?.as_str()),
        ),
        ContainerRuntime::Podman => (
            required_json_string(value, "Id")?,
            parse_podman_names(value.get("Names").ok_or_else(container_list_shape_error)?)?,
            required_json_string(value, "Image")?,
            required_json_string(value, "Status")?,
            parse_podman_labels(value.get("Labels").ok_or_else(container_list_shape_error)?)?,
            parse_podman_ports(value.get("Ports").ok_or_else(container_list_shape_error)?)?,
        ),
    };
    validate_container_id(&id)?;
    let compose = compose_metadata(&labels);
    Ok(ContainerSummary {
        target: ContainerTarget {
            host_id,
            runtime,
            container_id: id.clone(),
        },
        short_id: id.chars().take(12).collect(),
        name,
        image,
        status: parse_container_status(&status_text),
        status_text,
        ports,
        labels,
        compose,
    })
}

/// Docker 将每项摘要输出为一行对象，Podman 将同一命令输出为完整数组。
///
/// 空结果按两种运行时的合法空输出处理；任何非空的畸形记录都使整次列表失败，避免
/// 部分解析掩盖容器或字段丢失。
///
/// @author kongweiguang
fn parse_container_list_output(
    output: &[u8],
    host_id: Option<kerminal_domain::identity::HostId>,
    runtime: ContainerRuntime,
) -> AppResult<Vec<ContainerSummary>> {
    if output.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    match runtime {
        ContainerRuntime::Docker => output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(|line| {
                let value: Value = serde_json::from_slice(line)
                    .map_err(|_| AppError::infrastructure("Docker 容器列表行无法解析"))?;
                parse_container_summary(&value, host_id.clone(), runtime)
            })
            .collect(),
        ContainerRuntime::Podman => {
            let value: Value = serde_json::from_slice(output)
                .map_err(|_| AppError::infrastructure("Podman 容器列表无法解析"))?;
            let rows = value.as_array().ok_or_else(container_list_shape_error)?;
            rows.iter()
                .map(|row| parse_container_summary(row, host_id.clone(), runtime))
                .collect()
        }
    }
}

/// 读取列表对象中的必需字符串，类型不符时拒绝不完整摘要。
///
/// @author kongweiguang
fn required_json_string(value: &Value, key: &str) -> AppResult<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(container_list_shape_error)
}

/// 统一列表结构错误，避免把原始输出内容带入用户可见错误。
///
/// @author kongweiguang
fn container_list_shape_error() -> AppError {
    AppError::infrastructure("容器列表字段格式无效")
}

/// Podman `ListContainer.Names` 是字符串数组；界面摘要保留全部名称顺序。
///
/// @author kongweiguang
fn parse_podman_names(value: &Value) -> AppResult<String> {
    let names = value
        .as_array()
        .ok_or_else(container_list_shape_error)?
        .iter()
        .map(|name| {
            name.as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .ok_or_else(container_list_shape_error)
        })
        .collect::<AppResult<Vec<_>>>()?;
    if names.is_empty() {
        return Err(container_list_shape_error());
    }
    Ok(names.join(", "))
}

/// Podman 的 Labels 是 map；空 map 或 nil map 对应空标签，其余类型必须匹配。
///
/// @author kongweiguang
fn parse_podman_labels(value: &Value) -> AppResult<Vec<(String, String)>> {
    let Some(labels) = value.as_object() else {
        return if value.is_null() {
            Ok(Vec::new())
        } else {
            Err(container_list_shape_error())
        };
    };
    let mut parsed = labels
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(container_list_shape_error)
        })
        .collect::<AppResult<Vec<_>>>()?;
    parsed.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(parsed)
}

/// Docker 的 `ps --format` 将 Labels 展示为逗号分隔字符串；不丢弃无分隔片段，
/// 而把它追加到前一标签值，以保留 Compose 配置文件路径中的逗号。
///
/// @author kongweiguang
fn parse_docker_labels(value: &str) -> AppResult<Vec<(String, String)>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut parsed = Vec::new();
    let mut current: Option<(String, String)> = None;
    for part in value.split(',') {
        if let Some((key, label_value)) = part.split_once('=') {
            if key.is_empty() {
                return Err(container_list_shape_error());
            }
            if let Some(previous) = current.replace((key.to_owned(), label_value.to_owned())) {
                parsed.push(previous);
            }
        } else if let Some((_, label_value)) = current.as_mut() {
            label_value.push(',');
            label_value.push_str(part);
        } else {
            return Err(container_list_shape_error());
        }
    }
    if let Some(last) = current {
        parsed.push(last);
    }
    parsed.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(parsed)
}

/// Docker 的 Ports 是已格式化文本；沿其 comma-space 分隔保留每条展示绑定。
///
/// @author kongweiguang
fn parse_docker_ports(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(", ").map(str::to_owned).collect()
    }
}

/// Podman 的 Ports 是 nullable `[]ocicni.PortMapping`，按 3.4.4 的 JSON 字段逐条映射。
///
/// @author kongweiguang
fn parse_podman_ports(value: &Value) -> AppResult<Vec<String>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    value
        .as_array()
        .ok_or_else(container_list_shape_error)?
        .iter()
        .map(parse_podman_port)
        .collect()
}

/// 将 Podman CNI 端口映射格式化为列表摘要文本：空 hostIP 按运行时展示为 0.0.0.0，IPv6 保留方括号。
///
/// 字段缺失或类型错误时拒绝整条记录，避免静默丢失端口绑定。
///
/// @author kongweiguang
fn parse_podman_port(value: &Value) -> AppResult<String> {
    let integer = |key| {
        value
            .get(key)
            .and_then(Value::as_i64)
            .filter(|port| (0..=u16::MAX as i64).contains(port))
            .map(|port| port.to_string())
            .ok_or_else(container_list_shape_error)
    };
    let host_port = integer("hostPort")?;
    let container_port = integer("containerPort")?;
    let host_ip = required_json_string(value, "hostIP")?;
    let protocol = required_json_string(value, "protocol")?;
    if protocol.is_empty() {
        return Err(container_list_shape_error());
    }
    let formatted_host = if host_ip.is_empty() {
        "0.0.0.0".to_owned()
    } else if host_ip.contains(':') && !host_ip.starts_with('[') {
        format!("[{host_ip}]")
    } else {
        host_ip
    };
    let binding = format!("{formatted_host}:{host_port}");
    Ok(format!("{binding}->{container_port}/{protocol}"))
}

/// 从完整 inspect 结构映射 UI 所需字段，保留 command 数组而不合并成 shell 文本。
fn parse_container_inspect(value: &Value, target: ContainerTarget) -> AppResult<ContainerInspect> {
    let config = value.get("Config").unwrap_or(&Value::Null);
    let state = value.get("State").unwrap_or(&Value::Null);
    let id = json_string(value, &["Id", "ID"])
        .ok_or_else(|| AppError::infrastructure("容器检查缺少标识"))?;
    let name = json_string(value, &["Name"])
        .unwrap_or_else(|| target.container_id.clone())
        .trim_start_matches('/')
        .to_owned();
    Ok(ContainerInspect {
        target,
        id,
        name,
        image: json_string(config, &["Image"]).unwrap_or_default(),
        status: json_string(state, &["Status"]).unwrap_or_default(),
        running: state
            .get("Running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        created: json_string(value, &["Created"]),
        started_at: json_string(state, &["StartedAt"]),
        finished_at: json_string(state, &["FinishedAt"]),
        entrypoint: json_array_strings(config, "Entrypoint"),
        command: json_array_strings(config, "Cmd"),
        working_dir: json_string(config, &["WorkingDir"]),
        user: json_string(config, &["User"]),
        ports: value
            .pointer("/NetworkSettings/Ports")
            .and_then(Value::as_object)
            .map(|ports| ports.keys().cloned().collect())
            .unwrap_or_default(),
        networks: value
            .pointer("/NetworkSettings/Networks")
            .and_then(Value::as_object)
            .map(|networks| networks.keys().cloned().collect())
            .unwrap_or_default(),
    })
}

/// 将 Docker/Podman 的单个统计字段统一为展示文本，不推导不存在的计量值。
fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|field| match field {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
    })
}

/// 读取配置中的 argv 数组；非字符串项被忽略，不将嵌套 JSON stringify 成命令。
fn json_array_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// 从 Compose 标准标签构造管理投影，不依赖 Compose 进程的本地缓存。
fn compose_metadata(labels: &[(String, String)]) -> Option<ComposeMetadata> {
    let get = |key: &str| {
        labels
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let project = get("com.docker.compose.project");
    let service = get("com.docker.compose.service");
    if project.is_none() && service.is_none() {
        return None;
    }
    let config_files = get("com.docker.compose.config_files")
        .map(|value| value.split(',').map(str::to_owned).collect::<Vec<String>>())
        .unwrap_or_default();
    let config_paths = config_files.clone();
    Some(ComposeMetadata {
        project,
        service,
        working_dir: get("com.docker.compose.project.working_dir"),
        config_files,
        config_paths,
        container_number: get("com.docker.compose.container-number"),
        oneoff: get("com.docker.compose.oneoff")
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
    })
}

/// 将常见 runtime 状态映射为稳定枚举，同时保留原始状态文本用于产品展示。
fn parse_container_status(status: &str) -> ContainerStatus {
    let status = status.trim().to_ascii_lowercase();
    if status.starts_with("up ") || status == "running" {
        ContainerStatus::Running
    } else if status.starts_with("exited") {
        ContainerStatus::Exited
    } else if status.starts_with("paused") {
        ContainerStatus::Paused
    } else if status.starts_with("restarting") {
        ContainerStatus::Restarting
    } else if status.starts_with("created") {
        ContainerStatus::Created
    } else if status.starts_with("dead") {
        ContainerStatus::Dead
    } else {
        ContainerStatus::Unknown
    }
}
