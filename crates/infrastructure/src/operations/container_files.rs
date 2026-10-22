//! Docker/Podman 容器文件系统适配器。
//!
//! 文件内容通过 exec stdin/stdout 传递，路径始终作为 argv；目录枚举使用 BusyBox/GNU 通用 stat 格式。
//!
//! @author kongweiguang

use std::{error::Error, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    files::{FileAccessProvider, MAX_TEXT_FILE_READ_BYTES},
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        OperationsCommandExecutor,
    },
};
use kerminal_domain::{
    files::{
        ContainerRuntime, DeleteTargetKind, DirectoryListing, FileEndpoint, FileEntry,
        FileEntryKind, FilePreview, FileRevision, LineEnding, RemotePath, TextDocument,
        WriteTextRequest, WriteTextResult, WriteTextStrategy,
    },
    identity::HostId,
};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const DIRECTORY_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const EXIT_NOT_FOUND: i32 = 40;
const EXIT_CONFLICT: i32 = 41;
const EXIT_WRONG_TYPE: i32 = 42;

const METADATA_READ_SCRIPT: &str = "set -eu; p=$1; limit=$2; [ ! -L \"$p\" ] || exit 42; [ -f \"$p\" ] || exit 40; size=$(stat -c '%s' -- \"$p\") || exit 127; modified=$(stat -c '%Y' -- \"$p\") || exit 127; mode=$(stat -c '%a' -- \"$p\") || exit 127; printf '%s\\0%s\\0%s\\0' \"$size\" \"$modified\" \"$mode\"; head -c \"$limit\" -- \"$p\"";
const LIST_DIRECTORY_SCRIPT: &str = "set -eu; p=$1; [ -d \"$p\" ] || exit 40; for entry in \"$p\"/* \"$p\"/.[!.]* \"$p\"/..?*; do [ -e \"$entry\" ] || [ -L \"$entry\" ] || continue; name=${entry##*/}; if [ -L \"$entry\" ]; then kind=l; elif [ -d \"$entry\" ]; then kind=d; elif [ -f \"$entry\" ]; then kind=f; else kind=o; fi; metadata=$(stat -c '%s %Y %a' -- \"$entry\") || exit 127; size=${metadata%% *}; metadata=${metadata#* }; modified=${metadata%% *}; mode=${metadata#* }; printf '%s\\0%s\\0%s\\0%s\\0%s\\0' \"$name\" \"$kind\" \"$size\" \"$modified\" \"$mode\"; done";

/// 通过已装配的运维命令端口访问本机或已保存主机上的容器文件，不持有第二套 SSH 连接。
pub struct ContainerFileProvider {
    commands: Arc<dyn OperationsCommandExecutor>,
}

impl ContainerFileProvider {
    /// 注入共享结构化命令端口；本适配器不接受 shell 原文或临时凭据。
    pub fn new(commands: Arc<dyn OperationsCommandExecutor>) -> Self {
        Self { commands }
    }

    /// 通过 runtime exec 构造固定脚本调用，确保容器路径仅作为独立参数传递。
    async fn run(
        &self,
        cancellation: CancellationToken,
        endpoint: &FileEndpoint,
        script: &'static str,
        arguments: Vec<String>,
        input: Option<Vec<u8>>,
        max_output_bytes: usize,
    ) -> AppResult<CommandExecutionResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("容器文件操作已取消"));
        }
        let (host_id, runtime, container_id, _) = container_endpoint(endpoint)?;
        let target = host_id.map_or_else(
            || CommandExecutionTarget::Local,
            |host_id| CommandExecutionTarget::Host(host_id.clone()),
        );
        let mut command_arguments = vec![
            "exec".to_owned(),
            "--interactive".to_owned(),
            container_id.to_owned(),
            "sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "kerminal".to_owned(),
        ];
        command_arguments.extend(arguments);
        let invocation = CommandInvocation::new(runtime_program(runtime), command_arguments);
        let limits = CommandExecutionLimits {
            timeout: COMMAND_TIMEOUT,
            max_output_bytes,
        };
        let result = match input {
            Some(input) => {
                self.commands
                    .execute_with_input(target, invocation, input, limits, cancellation)
                    .await?
            }
            None => {
                self.commands
                    .execute(target, invocation, limits, cancellation)
                    .await?
            }
        };
        ensure_success(&result).map(|()| result)
    }
}

#[async_trait]
impl FileAccessProvider for ContainerFileProvider {
    /// 只匹配容器 endpoint；Local 和 SFTP 目标由各自 provider 处理。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Container { .. })
    }

    /// 用 shell glob 枚举并按 NUL 分隔返回元数据，兼容 Alpine BusyBox 而不依赖 GNU find 扩展。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        let endpoint = normalize_container_endpoint(&endpoint)?;
        let (_, _, _, path) = container_endpoint(&endpoint)?;
        let result = self
            .run(
                cancellation,
                &endpoint,
                LIST_DIRECTORY_SCRIPT,
                vec![path.as_str().to_owned()],
                None,
                DIRECTORY_OUTPUT_LIMIT,
            )
            .await?;
        ensure_complete(&result)?;
        let mut entries = parse_directory_entries(&result.stdout, &endpoint)?;
        entries.sort_by(|left, right| {
            let left_directory = left.kind == FileEntryKind::Directory;
            let right_directory = right.kind == FileEntryKind::Directory;
            right_directory
                .cmp(&left_directory)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(DirectoryListing {
            directory: endpoint,
            entries,
        })
    }

    /// 读取不超过调用方预算的二进制前缀，并附带远端文件大小和基础元数据。
    async fn preview_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<FilePreview> {
        let endpoint = normalize_container_endpoint(&endpoint)?;
        let (_, _, _, path) = container_endpoint(&endpoint)?;
        let result = self
            .run(
                cancellation,
                &endpoint,
                METADATA_READ_SCRIPT,
                vec![path.as_str().to_owned(), max_bytes.to_string()],
                None,
                max_bytes.saturating_add(96),
            )
            .await?;
        ensure_complete(&result)?;
        let (revision, bytes) = parse_metadata_prefix(&result.stdout)?;
        let truncated = revision.size > bytes.len() as u64;
        let binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
        Ok(FilePreview {
            endpoint,
            bytes,
            total_size: Some(revision.size),
            truncated,
            binary,
            revision: Some(revision),
        })
    }

    /// 只为完整 UTF-8 文本建立内容摘要；超限前缀可预览但被标记只读。
    async fn read_text_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<TextDocument> {
        let preview = self
            .preview_file(
                cancellation,
                endpoint,
                max_bytes.min(MAX_TEXT_FILE_READ_BYTES),
            )
            .await?;
        if preview.binary {
            return Err(AppError::unsupported("容器文件不是 UTF-8 文本"));
        }
        let truncated = preview.truncated;
        let content = if truncated {
            String::from_utf8_lossy(&preview.bytes).into_owned()
        } else {
            String::from_utf8(preview.bytes.clone())
                .map_err(|_| AppError::unsupported("容器文件不是有效 UTF-8 文本"))?
        };
        let revision = match preview.revision {
            Some(mut revision) if !truncated => {
                revision.content_digest = Some(sha256_hex(&preview.bytes));
                revision
            }
            Some(revision) => revision,
            None => return Err(AppError::infrastructure("容器文件元数据缺失")),
        };
        Ok(TextDocument {
            endpoint: preview.endpoint,
            line_ending: line_ending(&content),
            content,
            revision,
            readonly: truncated,
            truncated,
        })
    }

    /// 在容器内以同目录临时文件提交，CreateNew 使用硬链接防止覆盖既有路径。
    async fn write_text_file(
        &self,
        cancellation: CancellationToken,
        mut request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        request.endpoint = normalize_container_endpoint(&request.endpoint)?;
        let (_, _, _, path) = container_endpoint(&request.endpoint)?;
        let content = request.content.into_bytes();
        if content.len() > MAX_TEXT_FILE_READ_BYTES {
            return Err(AppError::invalid("容器文本超过编辑大小上限"));
        }
        let expected = match &request.strategy {
            WriteTextStrategy::CompareRevision(revision) => {
                let current = self
                    .read_text_file(
                        cancellation.child_token(),
                        request.endpoint.clone(),
                        MAX_TEXT_FILE_READ_BYTES,
                    )
                    .await?;
                if current.truncated || !revision_matches(revision, &current.revision) {
                    return Err(AppError::conflict("容器文件已被其他操作修改"));
                }
                Some(revision.clone())
            }
            WriteTextStrategy::CreateNew | WriteTextStrategy::ExplicitOverwrite => None,
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("容器文本保存已取消"));
        }
        let script = write_script(&request.strategy);
        let mut arguments = vec![path.as_str().to_owned()];
        if let Some(revision) = expected {
            arguments.extend([
                revision.size.to_string(),
                revision.modified.unwrap_or_default(),
                revision
                    .permissions
                    .map(|mode| format!("{mode:o}"))
                    .unwrap_or_default(),
                revision.content_digest.unwrap_or_default(),
            ]);
        }
        // 一旦提交命令发出，调用方取消不能丢弃可能已完成 rename 的回执；命令仍由固定超时和应用 shutdown 管理。
        let result = self
            .run(
                CancellationToken::new(),
                &request.endpoint,
                script,
                arguments,
                Some(content.clone()),
                256,
            )
            .await
            .map_err(map_container_mutation_outcome_error)?;
        let revision = parse_metadata_prefix(&result.stdout).map(|(mut revision, _)| {
            revision.content_digest = Some(sha256_hex(&content));
            revision
        })?;
        Ok(WriteTextResult {
            bytes_written: content.len() as u64,
            revision,
        })
    }

    /// 递归创建目录；已存在目录幂等成功，文件或符号链接冲突明确返回 Conflict。
    async fn create_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<()> {
        let endpoint = normalize_container_endpoint(&endpoint)?;
        let (_, _, _, path) = container_endpoint(&endpoint)?;
        self.run(
            cancellation,
            &endpoint,
            "set -eu; p=$1; if [ -e \"$p\" ] || [ -L \"$p\" ]; then [ -d \"$p\" ] && [ ! -L \"$p\" ] || exit 42; exit 0; fi; if mkdir -p -- \"$p\"; then [ -d \"$p\" ] && [ ! -L \"$p\" ] || exit 42; else status=$?; if [ -e \"$p\" ] || [ -L \"$p\" ]; then [ -d \"$p\" ] && [ ! -L \"$p\" ] && exit 0; exit 42; fi; exit \"$status\"; fi",
            vec![path.as_str().to_owned()],
            None,
            1024,
        )
        .await
        .map_err(map_container_mutation_outcome_error)?;
        Ok(())
    }

    /// 重命名前再次核对两端点属于同一容器，且不覆盖已存在目标。
    async fn rename(
        &self,
        cancellation: CancellationToken,
        source: FileEndpoint,
        destination: FileEndpoint,
    ) -> AppResult<()> {
        let source = normalize_container_endpoint(&source)?;
        let destination = normalize_container_endpoint(&destination)?;
        if !source.same_filesystem(&destination) {
            return Err(AppError::unsupported("容器重命名需要位于同一容器文件系统"));
        }
        let (_, _, _, source_path) = container_endpoint(&source)?;
        let (_, _, _, destination_path) = container_endpoint(&destination)?;
        self.run(
            cancellation,
            &source,
            "set -eu; src=$1; dst=$2; [ -e \"$src\" ] || [ -L \"$src\" ] || exit 40; [ ! -e \"$dst\" ] && [ ! -L \"$dst\" ] || exit 41; mv -nT -- \"$src\" \"$dst\"; [ ! -e \"$src\" ] && [ ! -L \"$src\" ] || exit 41",
            vec![source_path.as_str().to_owned(), destination_path.as_str().to_owned()],
            None,
            1024,
        )
        .await
        .map_err(map_container_mutation_outcome_error)?;
        Ok(())
    }

    /// 使用显式八进制权限值调用 chmod，不接收任意 mode 文本。
    async fn chmod(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        mode: u32,
    ) -> AppResult<()> {
        let endpoint = normalize_container_endpoint(&endpoint)?;
        if mode > 0o7777 {
            return Err(AppError::invalid("容器文件权限超出支持范围"));
        }
        let (_, _, _, path) = container_endpoint(&endpoint)?;
        self.run(
            cancellation,
            &endpoint,
            "set -eu; p=$1; mode=$2; [ -e \"$p\" ] || [ -L \"$p\" ] || exit 40; chmod -- \"$mode\" \"$p\"",
            vec![path.as_str().to_owned(), format!("{mode:o}")],
            None,
            1024,
        )
        .await
        .map_err(map_container_mutation_outcome_error)?;
        Ok(())
    }

    /// 容器目录支持递归删除；根目录、父路径和符号链接目录均在脚本边界拒绝。
    async fn delete(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        kind: DeleteTargetKind,
    ) -> AppResult<()> {
        let endpoint = normalize_container_endpoint(&endpoint)?;
        let (_, _, _, path) = container_endpoint(&endpoint)?;
        if path.as_str() == "/" || contains_parent_segment(path.as_str()) {
            return Err(AppError::invalid("不能删除容器根目录或父级路径"));
        }
        let script = match kind {
            DeleteTargetKind::File => {
                "set -eu; p=$1; if [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then exit 0; fi; [ ! -d \"$p\" ] || [ -L \"$p\" ] || exit 42; rm -f -- \"$p\""
            }
            DeleteTargetKind::Directory => {
                "set -eu; p=$1; if [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then exit 0; fi; [ -d \"$p\" ] && [ ! -L \"$p\" ] || exit 42; rm -rf -- \"$p\""
            }
        };
        self.run(
            cancellation,
            &endpoint,
            script,
            vec![path.as_str().to_owned()],
            None,
            1024,
        )
        .await
        .map_err(map_container_mutation_outcome_error)?;
        Ok(())
    }
}

/// 在容器文件入口集中做路径归一化，避免 MCP、UI 和传输调用各自解释反斜杠及父级段。
fn normalize_container_endpoint(endpoint: &FileEndpoint) -> AppResult<FileEndpoint> {
    let FileEndpoint::Container {
        host_id,
        runtime,
        container_id,
        path,
    } = endpoint
    else {
        return Err(AppError::unsupported("容器文件适配器只处理容器目标"));
    };
    validate_container_id(container_id)?;
    let path = RemotePath::parse_container(path.as_str())
        .map_err(|_| AppError::invalid("容器文件路径无效或越过根目录"))?;
    if path.as_str().len() > 4096 {
        return Err(AppError::invalid("容器文件路径超过长度限制"));
    }
    Ok(FileEndpoint::Container {
        host_id: host_id.clone(),
        runtime: *runtime,
        container_id: container_id.clone(),
        path,
    })
}

/// 只接受容器端点、绝对路径和保守容器名，避免路径逃逸和 flag 注入。
fn container_endpoint(
    endpoint: &FileEndpoint,
) -> AppResult<(Option<&HostId>, ContainerRuntime, &str, &RemotePath)> {
    let FileEndpoint::Container {
        host_id,
        runtime,
        container_id,
        path,
    } = endpoint
    else {
        return Err(AppError::unsupported("容器文件适配器只处理容器目标"));
    };
    validate_container_id(container_id)?;
    if path.as_str().len() > 4096
        || !path.as_str().starts_with('/')
        || contains_parent_segment(path.as_str())
    {
        return Err(AppError::invalid("容器文件路径必须是安全的绝对路径"));
    }
    Ok((host_id.as_ref(), *runtime, container_id, path))
}

/// 拒绝以选项语法开头或含分隔控制字符的容器标识。
fn validate_container_id(value: &str) -> AppResult<()> {
    let mut characters = value.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        || value.len() > 128
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
    {
        return Err(AppError::invalid("容器标识格式无效"));
    }
    Ok(())
}

/// 检查每个 POSIX 路径段，删除和读取都不能绕过容器绝对路径边界。
fn contains_parent_segment(path: &str) -> bool {
    path.split('/').any(|segment| segment == "..")
}

/// 把运行时枚举映射为固定 CLI 名称，配置数据不能指定可执行文件。
fn runtime_program(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::Podman => "podman",
    }
}

/// 将结构化退出码映射为可恢复的文件错误，原始 stderr 不进入用户可见消息。
fn ensure_success(result: &CommandExecutionResult) -> AppResult<()> {
    if !result.input_complete {
        return Err(AppError::outcome_unknown_with_source(
            "容器文件输入未确认完整送达，请先重新读取目标",
            IncompleteContainerInput {
                result: result.clone(),
            },
        ));
    }
    if result.truncated {
        return Err(AppError::infrastructure("容器文件命令输出超过读取上限"));
    }
    match result.exit_code {
        Some(0) => Ok(()),
        Some(EXIT_NOT_FOUND) => Err(AppError::not_found("容器文件或目录不存在")),
        Some(EXIT_CONFLICT) => Err(AppError::conflict("容器文件目标已存在或已变化")),
        Some(EXIT_WRONG_TYPE) => Err(AppError::conflict("容器文件类型与操作不匹配")),
        Some(127) => Err(AppError::unsupported(
            "容器需要 POSIX sh 和兼容的 stat 与文件工具",
        )),
        code => Err(AppError::remote_exit(code)),
    }
}

/// 保存未完整发送时的原始有界回执，供内部诊断核对退出码与 stdout/stderr，不进入日志文案。
struct IncompleteContainerInput {
    result: CommandExecutionResult,
}

impl fmt::Debug for IncompleteContainerInput {
    /// 诊断只显示长度与状态，容器文件正文可能包含敏感数据，不展开字节内容。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IncompleteContainerInput")
            .field("exit_code", &self.result.exit_code)
            .field("stdout_bytes", &self.result.stdout.len())
            .field("stderr_bytes", &self.result.stderr.len())
            .field("truncated", &self.result.truncated)
            .finish()
    }
}

impl fmt::Display for IncompleteContainerInput {
    /// 错误链保留结果对象，但显示固定摘要以免文件正文流入用户日志。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("容器文件命令未确认完整 stdin 交付")
    }
}

impl Error for IncompleteContainerInput {}

/// 已触及容器运行时后，取消、超时或传输故障都可能丢失变更回执，必须先回读核验。
///
/// @author kongweiguang
fn map_container_mutation_outcome_error(error: AppError) -> AppError {
    match error.public_code() {
        PublicErrorCode::Cancelled | PublicErrorCode::Timeout | PublicErrorCode::Infrastructure => {
            AppError::outcome_unknown_with_source("容器文件操作结果未知，请先重新读取目标", error)
        }
        _ => error,
    }
}

/// 结构化列表不得接受被输出预算截断的 NUL 记录。
fn ensure_complete(result: &CommandExecutionResult) -> AppResult<()> {
    if result.truncated {
        Err(AppError::infrastructure("容器目录条目超过读取上限"))
    } else {
        Ok(())
    }
}

/// 按五字段记录解析 shell 枚举结果；保留不完整记录拒绝，防止截断元数据误指向路径。
///
/// @author kongweiguang
fn parse_directory_entries(output: &[u8], directory: &FileEndpoint) -> AppResult<Vec<FileEntry>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields: Vec<&[u8]> = output.split(|byte| *byte == 0).collect();
    if fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    let (entries, remainder) = fields.as_chunks::<5>();
    if !remainder.is_empty() {
        return Err(AppError::infrastructure("容器目录元数据格式无效"));
    }
    entries
        .iter()
        .map(|entry| {
            let name = std::str::from_utf8(entry[0])
                .map_err(|_| AppError::unsupported("容器文件名不是有效 UTF-8"))?
                .to_owned();
            let child_path = match directory {
                FileEndpoint::Container { path, .. } => path
                    .join_name(&name)
                    .map_err(|_| AppError::unsupported("容器文件名包含不支持的字符"))?,
                _ => return Err(AppError::unsupported("容器目录端点无效")),
            };
            let endpoint = match directory {
                FileEndpoint::Container {
                    host_id,
                    runtime,
                    container_id,
                    ..
                } => FileEndpoint::Container {
                    host_id: host_id.clone(),
                    runtime: *runtime,
                    container_id: container_id.clone(),
                    path: child_path,
                },
                _ => return Err(AppError::unsupported("容器目录端点无效")),
            };
            let kind = match entry[1] {
                [b'f'] => FileEntryKind::File,
                [b'd'] => FileEntryKind::Directory,
                [b'l'] => FileEntryKind::Symlink,
                _ => FileEntryKind::Other,
            };
            let size = std::str::from_utf8(entry[2])
                .ok()
                .and_then(|value| value.parse().ok());
            let modified = std::str::from_utf8(entry[3]).ok().map(str::to_owned);
            let permissions = std::str::from_utf8(entry[4])
                .ok()
                .and_then(|value| u32::from_str_radix(value, 8).ok());
            let is_hidden = name.starts_with('.');
            Ok(FileEntry {
                endpoint,
                name,
                kind,
                size,
                modified,
                permissions,
                is_hidden,
                raw: None,
            })
        })
        .collect()
}

/// 解析元数据前缀并把余下字节视为文件内容，避免用文本分隔符切坏二进制预览。
fn parse_metadata_prefix(output: &[u8]) -> AppResult<(FileRevision, Vec<u8>)> {
    let mut separators = output
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == 0).then_some(index));
    let first = separators
        .next()
        .ok_or_else(|| AppError::infrastructure("容器文件元数据缺失"))?;
    let second = separators
        .next()
        .ok_or_else(|| AppError::infrastructure("容器文件元数据缺失"))?;
    let third = separators
        .next()
        .ok_or_else(|| AppError::infrastructure("容器文件元数据缺失"))?;
    let size = parse_ascii_u64(&output[..first])?;
    let modified = parse_ascii_string(&output[first + 1..second])?;
    let mode = parse_ascii_string(&output[second + 1..third])?;
    let permissions = u32::from_str_radix(&mode, 8)
        .map_err(|_| AppError::infrastructure("容器文件权限元数据无效"))?;
    Ok((
        FileRevision {
            size,
            modified: Some(modified),
            permissions: Some(permissions),
            content_digest: None,
        },
        output[third + 1..].to_vec(),
    ))
}

/// 在 revision 字段始终使用 ASCII 的约束下解析十进制大小。
fn parse_ascii_u64(value: &[u8]) -> AppResult<u64> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AppError::infrastructure("容器文件大小元数据无效"))
}

/// 解析有限长度 ASCII metadata，避免把二进制内容转换成路径或时间字段。
fn parse_ascii_string(value: &[u8]) -> AppResult<String> {
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| AppError::infrastructure("容器文件 metadata 编码无效"))
}

/// 构造固定提交脚本；expected metadata 均由 argv 传入而不是拼接入脚本。
fn write_script(strategy: &WriteTextStrategy) -> &'static str {
    match strategy {
        WriteTextStrategy::CreateNew => {
            "set -eu; p=$1; dir=${p%/*}; [ -n \"$dir\" ] || dir=/; [ -d \"$dir\" ] || exit 40; [ ! -e \"$p\" ] && [ ! -L \"$p\" ] || exit 41; tmp=$(mktemp \"$dir/.kerminal-write.XXXXXXXX\"); trap 'rm -f -- \"$tmp\"' EXIT; cat > \"$tmp\"; chmod 644 \"$tmp\"; ln -- \"$tmp\" \"$p\" || exit 41; rm -- \"$tmp\"; trap - EXIT; size=$(stat -c '%s' -- \"$p\"); modified=$(stat -c '%Y' -- \"$p\"); mode=$(stat -c '%a' -- \"$p\"); printf '%s\\0%s\\0%s\\0' \"$size\" \"$modified\" \"$mode\""
        }
        WriteTextStrategy::ExplicitOverwrite => {
            "set -eu; p=$1; dir=${p%/*}; [ -n \"$dir\" ] || dir=/; [ -d \"$dir\" ] || exit 40; [ ! -L \"$p\" ] || exit 42; [ ! -d \"$p\" ] || exit 42; mode=644; if [ -e \"$p\" ]; then mode=$(stat -c '%a' -- \"$p\"); fi; tmp=$(mktemp \"$dir/.kerminal-write.XXXXXXXX\"); trap 'rm -f -- \"$tmp\"' EXIT; cat > \"$tmp\"; chmod \"$mode\" \"$tmp\"; mv -fT -- \"$tmp\" \"$p\"; trap - EXIT; size=$(stat -c '%s' -- \"$p\"); modified=$(stat -c '%Y' -- \"$p\"); mode=$(stat -c '%a' -- \"$p\"); printf '%s\\0%s\\0%s\\0' \"$size\" \"$modified\" \"$mode\""
        }
        WriteTextStrategy::CompareRevision(_) => {
            "set -eu; p=$1; expected_size=$2; expected_modified=$3; expected_mode=$4; expected_digest=$5; dir=${p%/*}; [ -n \"$dir\" ] || dir=/; [ -d \"$dir\" ] || exit 40; [ -f \"$p\" ] && [ ! -L \"$p\" ] || exit 42; size=$(stat -c '%s' -- \"$p\"); modified=$(stat -c '%Y' -- \"$p\"); mode=$(stat -c '%a' -- \"$p\"); [ \"$size\" = \"$expected_size\" ] || exit 41; [ -z \"$expected_modified\" ] || [ \"$modified\" = \"$expected_modified\" ] || exit 41; [ -z \"$expected_mode\" ] || [ \"$mode\" = \"$expected_mode\" ] || exit 41; if [ -n \"$expected_digest\" ]; then digest=$(sha256sum -- \"$p\"); digest=${digest%% *}; [ \"$digest\" = \"$expected_digest\" ] || exit 41; fi; tmp=$(mktemp \"$dir/.kerminal-write.XXXXXXXX\"); trap 'rm -f -- \"$tmp\"' EXIT; cat > \"$tmp\"; chmod \"$mode\" \"$tmp\"; mv -fT -- \"$tmp\" \"$p\"; trap - EXIT; size=$(stat -c '%s' -- \"$p\"); modified=$(stat -c '%Y' -- \"$p\"); mode=$(stat -c '%a' -- \"$p\"); printf '%s\\0%s\\0%s\\0' \"$size\" \"$modified\" \"$mode\""
        }
    }
}

/// 比较用户打开文件时得到的完整摘要，若无摘要则比较可用的 metadata。
fn revision_matches(expected: &FileRevision, current: &FileRevision) -> bool {
    if let Some(expected_digest) = &expected.content_digest {
        current.content_digest.as_ref() == Some(expected_digest)
    } else {
        expected.size == current.size
            && expected.modified == current.modified
            && expected.permissions == current.permissions
    }
}

/// 根据原始文本识别行尾形态，保存时不把 CRLF 文档悄悄转换成 LF。
fn line_ending(text: &str) -> LineEnding {
    let crlf = text.as_bytes().windows(2).any(|window| window == b"\r\n");
    let bare_lf =
        text.as_bytes().iter().enumerate().any(|(index, byte)| {
            *byte == b'\n' && (index == 0 || text.as_bytes()[index - 1] != b'\r')
        });
    match (crlf, bare_lf) {
        (true, true) => LineEnding::Mixed,
        (true, false) => LineEnding::Crlf,
        (false, _) => LineEnding::Lf,
    }
}

/// 对文件内容计算 SHA-256，作为编辑冲突检测中的强 revision 字段。
fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
