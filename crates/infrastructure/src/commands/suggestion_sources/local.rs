//! 有界本地目录、PATH 和 Git 建议，不执行用户输入或访问 shell 历史文件。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    path::{Component, Path, PathBuf, Prefix},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use kerminal_application::{AppError, AppResult, commands::CommandSuggestionProvider};
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionActivation, CommandSuggestionCandidate,
    CommandSuggestionReplacementRange, CommandSuggestionRequest, CommandSuggestionSensitivity,
    SuggestionProviderKind,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

use super::{
    prefix_score, requested,
    syntax::{self, ParsedPrefix, ShellFamily},
};

const DIRECTORY_TTL: Duration = Duration::from_secs(2);
const COMMAND_TTL: Duration = Duration::from_secs(20);
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_CACHE_DIRECTORIES: usize = 32;
const MAX_PATH_DIRECTORIES: usize = 64;
const MAX_CANDIDATES: usize = 100;

#[derive(Clone)]
struct LocalEntry {
    name: String,
    directory: bool,
    executable: bool,
}

type CachedEntries = (Instant, Arc<Vec<LocalEntry>>);

/// 本地来源仅缓存有限目录投影；Git 的既有唯一来源由远端适配器按目标分流。
#[derive(Default)]
pub struct LocalCommandSuggestionProvider {
    directories: Mutex<HashMap<PathBuf, CachedEntries>>,
    commands: Mutex<Option<(Instant, Arc<Vec<String>>)>>,
}

impl LocalCommandSuggestionProvider {
    /// 构造不触碰磁盘或用户配置，真实目录只在有明确本地上下文的查询中读取。
    pub fn new() -> Self {
        Self::default()
    }

    /// 缓存仅保存目录投影并限制条数与寿命；缺失目录视为无匹配，其它 IO 错误可见。
    async fn directory_entries(
        &self,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> AppResult<Arc<Vec<LocalEntry>>> {
        if let Some((created, entries)) = self.directories.lock().await.get(path)
            && created.elapsed() < DIRECTORY_TTL
        {
            return Ok(Arc::clone(entries));
        }
        let mut directory = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("本地路径建议已取消")),
            result = tokio::fs::read_dir(path) => match result {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Arc::new(Vec::new())),
                Err(error) => return Err(AppError::infrastructure_with_source("读取本地建议目录失败", error)),
            }
        };
        let mut entries = Vec::new();
        let mut complete = true;
        loop {
            let entry = tokio::select! {
                _ = cancellation.cancelled() => return Err(AppError::cancelled("本地路径建议已取消")),
                result = directory.next_entry() => result.map_err(|error| AppError::infrastructure_with_source("读取本地建议目录失败", error))?,
            };
            let Some(entry) = entry else { break };
            if entries.len() >= MAX_DIRECTORY_ENTRIES {
                complete = false;
                break;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.chars().any(char::is_control) {
                continue;
            }
            let metadata = tokio::select! {
                _ = cancellation.cancelled() => return Err(AppError::cancelled("本地路径建议已取消")),
                result = entry.metadata() => match result { Ok(value) => value, Err(_) => continue },
            };
            entries.push(LocalEntry {
                executable: is_executable(&name, &metadata),
                directory: metadata.is_dir(),
                name,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        let entries = Arc::new(entries);
        if complete {
            let mut cache = self.directories.lock().await;
            if cache.len() >= MAX_CACHE_DIRECTORIES
                && !cache.contains_key(path)
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, (created, _))| *created)
                    .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
            cache.insert(path.to_owned(), (Instant::now(), Arc::clone(&entries)));
        }
        Ok(entries)
    }

    /// 仅枚举光标指向的单层目录；相对路径必须基于已确认的实时 cwd。
    async fn paths(
        &self,
        request: &CommandSuggestionRequest,
        prefix: &ParsedPrefix,
        cancellation: &CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        let path_context = prefix.token.value.contains(['/', '\\'])
            || syntax::command_name(prefix).is_some_and(syntax::accepts_bare_path);
        if !path_context {
            return Ok(Vec::new());
        }
        let (parent, typed) = split_path(&prefix.token.value);
        let Some(directory) = resolve_directory(parent, request.cwd.as_deref()) else {
            return Ok(Vec::new());
        };
        let entries = self.directory_entries(&directory, cancellation).await?;
        let separator = if cfg!(windows) && parent.contains('\\') {
            '\\'
        } else if parent.contains('/')
            || matches!(prefix.shell, ShellFamily::Posix | ShellFamily::Fish)
        {
            '/'
        } else {
            std::path::MAIN_SEPARATOR
        };
        let mut candidates = Vec::new();
        #[cfg(windows)]
        let mut case_insensitive = None;
        #[cfg(windows)]
        let mut case_probes = 0usize;
        for entry in entries.iter() {
            if entry.name.starts_with('.') && !typed.starts_with('.') {
                continue;
            }
            let suffix = if let Some(suffix) = entry.name.strip_prefix(typed) {
                suffix
            } else {
                #[cfg(not(windows))]
                {
                    continue;
                }
                #[cfg(windows)]
                {
                    let Some(suffix) = case_variant_suffix(&entry.name, typed) else {
                        continue;
                    };
                    let insensitive = match case_insensitive {
                        Some(value) => value,
                        None => {
                            let value =
                                directory_is_case_insensitive(&directory, cancellation).await?;
                            case_insensitive = Some(value);
                            value
                        }
                    };
                    if !insensitive || case_probes >= MAX_CANDIDATES {
                        continue;
                    }
                    case_probes += 1;
                    let original = directory.join(&entry.name);
                    let variant = directory.join(format!("{typed}{suffix}"));
                    if !same_windows_entry(original, variant, cancellation).await? {
                        continue;
                    }
                    suffix
                }
            };
            let mut display = format!("{parent}{}", entry.name);
            let mut completed = format!("{parent}{typed}{suffix}");
            if entry.directory {
                display.push(separator);
                completed.push(separator);
            }
            let Some(replacement) =
                syntax::append_literal(&request.input, request.cursor, prefix, &completed)
            else {
                continue;
            };
            candidates.push(local_candidate(
                request,
                prefix,
                SuggestionProviderKind::LocalPath,
                &display,
                replacement,
                if entry.directory {
                    "本地目录"
                } else {
                    "本地文件"
                },
                prefix_score(typed, &entry.name, entry.directory),
            ));
            if candidates.len() >= MAX_CANDIDATES {
                break;
            }
        }
        Ok(candidates)
    }

    /// PATH 来源只读应用继承的本机环境，不执行 shell 或猜测用户 profile 的动态修改。
    async fn commands(
        &self,
        request: &CommandSuggestionRequest,
        prefix: &ParsedPrefix,
        cancellation: &CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if !syntax::is_command_position(prefix)
            || prefix.token.value.is_empty()
            || prefix.token.value.contains(['/', '\\'])
        {
            return Ok(Vec::new());
        }
        let cached = self
            .commands
            .lock()
            .await
            .clone()
            .filter(|(created, _)| created.elapsed() < COMMAND_TTL);
        let names = if let Some((_, names)) = cached {
            names
        } else {
            let mut names = HashSet::new();
            let mut complete = true;
            let path = std::env::var_os("PATH").unwrap_or_default();
            for directory in std::env::split_paths(&path)
                .filter(|path| path.is_absolute())
                .take(MAX_PATH_DIRECTORIES)
            {
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled("本地命令建议已取消"));
                }
                if is_network_or_device_path(&directory) {
                    complete = false;
                    continue;
                }
                match self.directory_entries(&directory, cancellation).await {
                    Ok(entries) => names.extend(
                        entries
                            .iter()
                            .filter(|entry| entry.executable && !entry.directory)
                            .map(|entry| entry.name.clone()),
                    ),
                    Err(error) if cancellation.is_cancelled() => return Err(error),
                    Err(_) => complete = false,
                }
            }
            let mut names = names.into_iter().collect::<Vec<_>>();
            names.sort();
            names.truncate(8_192);
            let names = Arc::new(names);
            if complete {
                *self.commands.lock().await = Some((Instant::now(), Arc::clone(&names)));
            }
            names
        };
        Ok(names
            .iter()
            .filter(|name| name.starts_with(&prefix.token.value))
            .filter_map(|name| {
                let replacement =
                    syntax::append_literal(&request.input, request.cursor, prefix, name)?;
                Some(local_candidate(
                    request,
                    prefix,
                    SuggestionProviderKind::LocalCommand,
                    name,
                    replacement,
                    "本机 PATH 命令",
                    prefix_score(&prefix.token.value, name, false) - 0.1,
                ))
            })
            .take(MAX_CANDIDATES)
            .collect())
    }
}

#[async_trait::async_trait]
impl CommandSuggestionProvider for LocalCommandSuggestionProvider {
    /// 本地路径和命令具有独立状态，Git 保留既有唯一来源标识。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        vec![
            SuggestionProviderKind::LocalPath,
            SuggestionProviderKind::LocalCommand,
        ]
    }

    /// 本地文件和继承的 PATH 不能用于 SSH、串口或其他目标。
    fn supports_provider_target(
        &self,
        provider: SuggestionProviderKind,
        target: CommandHistoryTarget,
    ) -> bool {
        target == CommandHistoryTarget::Local
            && matches!(
                provider,
                SuggestionProviderKind::LocalPath | SuggestionProviderKind::LocalCommand
            )
    }

    /// 光标不在行尾或 shell 语法无法可靠解析时不触发本地 IO。
    async fn suggest(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("本地建议查询已取消"));
        }
        if request.target != CommandHistoryTarget::Local
            || request.cursor != request.input.chars().count()
            || request.input.len() > 32_000
            || request.input.chars().any(char::is_control)
        {
            return Ok(Vec::new());
        }
        let Some(prefix) = syntax::parse_prefix(
            &request.input,
            request.cursor,
            syntax::named_shell_family(request.shell.as_deref()),
        ) else {
            return Ok(Vec::new());
        };
        let mut candidates = Vec::new();
        if requested(&request, SuggestionProviderKind::LocalPath) {
            candidates.extend(self.paths(&request, &prefix, &cancellation).await?);
        }
        if requested(&request, SuggestionProviderKind::LocalCommand) {
            candidates.extend(self.commands(&request, &prefix, &cancellation).await?);
        }
        Ok(candidates)
    }
}

/// 本地 Git 仅执行固定只读 argv；不注入用户输入，且取消时终止子进程。
pub(super) async fn git_suggestions(
    request: &CommandSuggestionRequest,
    cancellation: CancellationToken,
) -> AppResult<Vec<CommandSuggestionCandidate>> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("本地 Git 建议已取消"));
    }
    if !requested(request, SuggestionProviderKind::Git)
        || request.cursor != request.input.chars().count()
        || request.input.len() > 32_000
        || request.input.chars().any(char::is_control)
    {
        return Ok(Vec::new());
    }
    let Some(cwd) = request.cwd.as_deref().filter(|cwd| {
        Path::new(cwd).is_absolute()
            && !is_network_or_device_path(Path::new(cwd))
            && !cwd.chars().any(char::is_control)
    }) else {
        return Ok(Vec::new());
    };
    let Some(prefix) = syntax::parse_prefix(
        &request.input,
        request.cursor,
        syntax::named_shell_family(request.shell.as_deref()),
    ) else {
        return Ok(Vec::new());
    };
    let words = prefix
        .preceding_words
        .iter()
        .map(|word| word.value.as_str())
        .collect::<Vec<_>>();
    if words.first().copied() != Some("git") {
        return Ok(Vec::new());
    }
    let choices = if words.len() == 1 {
        [
            "add", "branch", "checkout", "clone", "commit", "diff", "fetch", "log", "merge",
            "pull", "push", "rebase", "restore", "status", "switch", "tag",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
    } else if words.len() == 2
        && matches!(
            words[1],
            "branch"
                | "checkout"
                | "diff"
                | "log"
                | "merge"
                | "rebase"
                | "restore"
                | "show"
                | "switch"
                | "tag"
        )
        && !prefix.token.value.starts_with('-')
    {
        read_git_refs(cwd, &cancellation).await?
    } else {
        Vec::new()
    };
    Ok(choices
        .into_iter()
        .filter(|name| name.starts_with(&prefix.token.value) && !name.starts_with('-'))
        .filter_map(|name| {
            let replacement =
                syntax::append_literal(&request.input, request.cursor, &prefix, &name)?;
            Some(local_candidate(
                request,
                &prefix,
                SuggestionProviderKind::Git,
                &name,
                replacement,
                "本地 Git",
                prefix_score(&prefix.token.value, &name, false) - 0.05,
            ))
        })
        .take(MAX_CANDIDATES)
        .collect())
}

/// Git 子进程不继承可重定向仓库的变量，不提示认证，并限制运行时间及读取数量。
/// Windows 使用无窗口进程，避免后台自动建议打断当前桌面。
async fn read_git_refs(cwd: &str, cancellation: &CancellationToken) -> AppResult<Vec<String>> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args([
            "-c",
            "core.fsmonitor=false",
            "for-each-ref",
            "--count=500",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::infrastructure_with_source(
                "启动本地 Git 建议失败",
                error,
            ));
        }
    };
    let operation = async {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::infrastructure("本地 Git 输出不可用"))?;
        let mut bytes = Vec::new();
        stdout
            .take(256 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("读取本地 Git 引用失败", error)
            })?;
        if bytes.len() > 256 * 1024 {
            return Err(AppError::invalid("本地 Git 引用输出超过限制"));
        }
        let status = child.wait().await.map_err(|error| {
            AppError::infrastructure_with_source("等待本地 Git 完成失败", error)
        })?;
        Ok::<_, AppError>((status, bytes))
    };
    let outcome = tokio::select! {
        _ = cancellation.cancelled() => Err(AppError::cancelled("本地 Git 建议已取消")),
        result = tokio::time::timeout(Duration::from_secs(2), operation) => match result {
            Ok(result) => result,
            Err(_) => Err(AppError::timeout("本地 Git 建议超时")),
        }
    };
    let (status, stdout) = match outcome {
        Ok(output) => output,
        Err(error) => {
            stop_git_process(&mut child).await?;
            return Err(error);
        }
    };
    if !status.success() {
        return Ok(Vec::new());
    }
    let text =
        std::str::from_utf8(&stdout).map_err(|_| AppError::invalid("本地 Git 引用编码无效"))?;
    let mut refs = text
        .lines()
        .filter(|line| !line.is_empty() && !line.chars().any(char::is_control))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    refs.sort();
    refs.dedup();
    Ok(refs)
}

/// 出错时先终止并有限等待子进程，无法确认回收时不能把它当作已完成。
async fn stop_git_process(child: &mut tokio::process::Child) -> AppResult<()> {
    let _ = child.start_kill();
    match tokio::time::timeout(Duration::from_millis(750), child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(AppError::infrastructure_with_source(
            "等待本地 Git 子进程退出失败",
            error,
        )),
        Err(_) => Err(AppError::outcome_unknown("本地 Git 子进程未能在期限内回收")),
    }
}

/// 分离用户实际键入的路径前缀；Unix 允许反斜杠作为普通文件名字符。
fn split_path(value: &str) -> (&str, &str) {
    let separator = if cfg!(windows) {
        value.rfind(['/', '\\'])
    } else {
        value.rfind('/')
    };
    match separator {
        Some(index) => (&value[..=index], &value[index + 1..]),
        None => ("", value),
    }
}

/// 按 Unicode 标量边界比较大小写变体；最终文件名仍须由实际文件系统确认。
#[cfg(windows)]
fn case_variant_suffix<'a>(name: &'a str, typed: &str) -> Option<&'a str> {
    let mut positions = name.char_indices();
    for _ in typed.chars() {
        positions.next()?;
    }
    let end = positions.next().map_or(name.len(), |(index, _)| index);
    (name[..end].to_uppercase() == typed.to_uppercase()).then_some(&name[end..])
}

/// 查询目录大小写属性使用 blocking worker；不可确认时保留严格大小写匹配。
#[cfg(windows)]
async fn directory_is_case_insensitive(
    path: &Path,
    cancellation: &CancellationToken,
) -> AppResult<bool> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("本地路径建议已取消"));
    }
    let path = path.to_owned();
    tokio::select! {
        _ = cancellation.cancelled() => Err(AppError::cancelled("本地路径建议已取消")),
        result = tokio::task::spawn_blocking(move || windows_directory_is_case_insensitive(&path)) => {
            result.map_err(|source| AppError::infrastructure_with_source("查询本地目录大小写属性失败", source))
        }
    }
}

/// 仅当系统确认目录标志为零时允许尝试大小写变体，未知标志或系统错误均保守降级。
#[cfg(windows)]
fn windows_directory_is_case_insensitive(path: &Path) -> bool {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FileCaseSensitiveInfo,
        GetFileInformationByHandleEx,
    };

    let Ok(directory) = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    else {
        return false;
    };
    let mut info = FILE_CASE_SENSITIVE_INFO::default();
    // SAFETY: 目录句柄在调用期间保持有效，info 指向正确大小的可写结构。
    let success = unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle(),
            FileCaseSensitiveInfo,
            (&mut info as *mut FILE_CASE_SENSITIVE_INFO).cast(),
            std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
        )
    };
    success != 0 && info.Flags == 0
}

/// 大小写变体须指向原目录项的同一文件身份，避免不同对象或无法确认的路径被误选。
#[cfg(windows)]
async fn same_windows_entry(
    original: PathBuf,
    variant: PathBuf,
    cancellation: &CancellationToken,
) -> AppResult<bool> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("本地路径建议已取消"));
    }
    tokio::select! {
        _ = cancellation.cancelled() => Err(AppError::cancelled("本地路径建议已取消")),
        result = tokio::task::spawn_blocking(move || {
            match (windows_file_identity(&original), windows_file_identity(&variant)) {
                (Some(original), Some(variant)) => original == variant,
                _ => false,
            }
        }) => result.map_err(|source| AppError::infrastructure_with_source("核对本地路径身份失败", source)),
    }
}

/// 使用文件系统给出的 128 位身份而非名称或时间戳；无法取得可靠身份时不猜测。
#[cfg(windows)]
fn windows_file_identity(path: &Path) -> Option<(u64, [u8; 16])> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info = FILE_ID_INFO::default();
    // SAFETY: 文件句柄在调用期间保持有效，info 是足够大的可写 FILE_ID_INFO。
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 || info.FileId.Identifier.iter().all(|byte| *byte == 0) {
        return None;
    }
    Some((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// 拒绝未知 cwd、网络与设备路径和未经 shell 展开的 home/变量，避免误扫进程启动目录。
fn resolve_directory(parent: &str, cwd: Option<&str>) -> Option<PathBuf> {
    if parent.starts_with(['~', '$']) || parent.chars().any(char::is_control) {
        return None;
    }
    let path = Path::new(parent);
    let directory = if path.is_absolute() {
        path.to_owned()
    } else {
        let cwd = Path::new(cwd?);
        if !cwd.is_absolute() {
            return None;
        }
        cwd.join(path)
    };
    (!is_network_or_device_path(&directory)).then_some(directory)
}

/// Windows 只接受普通盘符前缀；UNC 和设备命名空间可能触发无法取消的远端或设备 I/O。
/// 使用平台解析后的前缀识别混合分隔符，并保留对 verbatim 盘符路径的既有保守限制。
fn is_network_or_device_path(path: &Path) -> bool {
    cfg!(windows)
        && matches!(
            path.components().next(),
            Some(Component::Prefix(prefix)) if !matches!(prefix.kind(), Prefix::Disk(_))
        )
}

/// Unix 按文件执行位过滤；Windows 仅接受系统 PATHEXT 或 PowerShell 脚本扩展。
fn is_executable(name: &str, metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = name;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        let Some(extension) = Path::new(name).extension().and_then(|value| value.to_str()) else {
            return false;
        };
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
        extension.eq_ignore_ascii_case("ps1")
            || extensions.split(';').any(|value| {
                value
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(extension)
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = name;
        false
    }
}

/// 生成稳定候选 ID 和 token 范围；最终追加合法性仍由聚合器统一复核。
fn local_candidate(
    request: &CommandSuggestionRequest,
    prefix: &ParsedPrefix,
    provider: SuggestionProviderKind,
    display: &str,
    replacement: String,
    description: &str,
    score: f64,
) -> CommandSuggestionCandidate {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    provider.hash(&mut hasher);
    request.cwd.hash(&mut hasher);
    display.hash(&mut hasher);
    let id = format!("local:{:016x}", hasher.finish());
    CommandSuggestionCandidate {
        id: id.clone(),
        provider,
        activation: CommandSuggestionActivation::Insert,
        display_text: display.to_owned(),
        replacement_text: replacement,
        replacement_range: CommandSuggestionReplacementRange {
            start: prefix.token.start,
            end: request.cursor,
        },
        suffix: String::new(),
        score,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: Some(description.to_owned()),
        source_explanation: Some(description.to_owned()),
        merged_source_explanations: Vec::new(),
        source_id: Some(id),
        allowed_presentations: CommandSuggestionCandidate::allowed_for(
            CommandSuggestionSensitivity::Normal,
        ),
        accept_boundaries: Vec::new(),
        context_key: request.context_key.clone(),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/local_command_suggestion_sources.rs"]
mod tests;
