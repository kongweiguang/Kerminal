//! 使用有限文件元数据检查发现当前平台可用的本地 Shell。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    local_shells::{LocalShellCandidate, LocalShellDiscovery, MAX_LOCAL_SHELL_CANDIDATES},
};
use tokio_util::sync::CancellationToken;

const MAX_PATH_DIRECTORIES: usize = 256;

/// 无状态平台发现器只查询安装路径，不运行 shell、VS 初始化批处理或用户 Profile。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLocalShellDiscovery;

impl SystemLocalShellDiscovery {
    /// 创建无状态发现器，使桌面 composition 显式注入而 UI 无需直接访问系统环境。
    pub const fn new() -> Self {
        Self
    }
}

impl LocalShellDiscovery for SystemLocalShellDiscovery {
    /// 按当前操作系统列出已存在且可执行的候选；取消在每次受界路径探测间生效。
    fn discover(&self, cancellation: &CancellationToken) -> AppResult<Vec<LocalShellCandidate>> {
        ensure_not_cancelled(cancellation)?;
        #[cfg(windows)]
        {
            discover_windows(cancellation)
        }
        #[cfg(unix)]
        {
            discover_unix(cancellation)
        }
        #[cfg(not(any(windows, unix)))]
        {
            Err(AppError::unsupported("当前平台不支持本地 Shell 候选发现"))
        }
    }
}

/// 每次系统元数据检查前响应取消，避免已关闭的编辑器接收过期结果。
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("本地 Shell 发现已取消"))
    } else {
        Ok(())
    }
}

/// 沿 PATH 的有限前缀查找绝对可执行文件，并跳过当前目录等可变相对入口。
fn find_in_path(name: &OsStr, cancellation: &CancellationToken) -> AppResult<Option<PathBuf>> {
    let path_value = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path_value).take(MAX_PATH_DIRECTORIES) {
        ensure_not_cancelled(cancellation)?;
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// 只接受普通文件；Unix 还要求至少一个执行位，Windows 由 `.exe` 路径和文件存在性限定。
fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0);
    }
    #[cfg(windows)]
    {
        path.extension()
            .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("exe"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// 同一 shell 只返回一个 PATH 优先项，并让取消和候选数量限制贯穿收集过程。
fn push_candidate(
    candidates: &mut Vec<LocalShellCandidate>,
    seen_ids: &mut HashSet<String>,
    id: &str,
    display_name: &str,
    program: PathBuf,
    args: Vec<OsString>,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    ensure_not_cancelled(cancellation)?;
    if candidates.len() >= MAX_LOCAL_SHELL_CANDIDATES || seen_ids.contains(id) {
        return Ok(());
    }
    if !program.is_absolute() || !is_executable_file(&program) {
        return Ok(());
    }
    seen_ids.insert(id.to_owned());
    candidates.push(LocalShellCandidate::new(
        id,
        display_name,
        program.into_os_string(),
        args,
    )?);
    Ok(())
}

/// Windows 候选按常见开发流程排序，系统目录兜底不改变现有默认 PowerShell 解析行为。
#[cfg(windows)]
fn discover_windows(cancellation: &CancellationToken) -> AppResult<Vec<LocalShellCandidate>> {
    let mut candidates = Vec::new();
    let mut seen_ids = HashSet::new();
    let system_root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute());

    for (id, label, executable, args) in [
        (
            "pwsh",
            "PowerShell 7",
            OsString::from("pwsh.exe"),
            vec![OsString::from("-NoLogo")],
        ),
        (
            "powershell",
            "Windows PowerShell",
            OsString::from("powershell.exe"),
            vec![OsString::from("-NoLogo")],
        ),
        (
            "cmd",
            "Command Prompt",
            OsString::from("cmd.exe"),
            Vec::new(),
        ),
        (
            "wsl",
            "WSL (default distribution)",
            OsString::from("wsl.exe"),
            Vec::new(),
        ),
    ] {
        ensure_not_cancelled(cancellation)?;
        let mut selected_path = find_in_path(&executable, cancellation)?;
        if selected_path.is_none() {
            selected_path = match (id, system_root.as_deref()) {
                ("pwsh", _) => find_installed_pwsh(cancellation)?,
                ("powershell", Some(root)) => Some(
                    root.join("System32")
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join(&executable),
                ),
                ("cmd" | "wsl", Some(root)) => Some(root.join("System32").join(&executable)),
                _ => None,
            }
            .filter(|path| is_executable_file(path));
        }
        if let Some(path) = selected_path {
            push_candidate(
                &mut candidates,
                &mut seen_ids,
                id,
                label,
                path,
                args,
                cancellation,
            )?;
        }
    }

    if let Some(path) = find_git_bash(cancellation)? {
        push_candidate(
            &mut candidates,
            &mut seen_ids,
            "git-bash",
            "Git Bash",
            path,
            vec![OsString::from("--login")],
            cancellation,
        )?;
    }
    discover_visual_studio_prompts(&mut candidates, &mut seen_ids, cancellation)?;
    Ok(candidates)
}

/// 在 PowerShell 常见安装根目录中补查 PATH 未登记的正式版、预览版与长期支持版。
#[cfg(windows)]
fn find_installed_pwsh(cancellation: &CancellationToken) -> AppResult<Option<PathBuf>> {
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(root) = env::var_os(variable).map(PathBuf::from) else {
            continue;
        };
        if !root.is_absolute() {
            continue;
        }
        for version in ["7", "7-preview", "7-lts"] {
            ensure_not_cancelled(cancellation)?;
            let candidate = root.join("PowerShell").join(version).join("pwsh.exe");
            if is_executable_file(&candidate) {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
}

/// 在典型 Git for Windows 安装位置或 PATH 中定位 bash.exe，不把任意 Bash 误标为 Git Bash。
#[cfg(windows)]
fn find_git_bash(cancellation: &CancellationToken) -> AppResult<Option<PathBuf>> {
    let mut known_paths = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = env::var_os(variable).map(PathBuf::from) {
            known_paths.push(root.join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        known_paths.push(
            local_app_data
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    for path in known_paths {
        ensure_not_cancelled(cancellation)?;
        if path.is_absolute() && is_git_bash_path(&path) && is_executable_file(&path) {
            return Ok(Some(path));
        }
    }

    let path_value = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path_value).take(MAX_PATH_DIRECTORIES) {
        ensure_not_cancelled(cancellation)?;
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join("bash.exe");
        if is_git_bash_path(&candidate) && is_executable_file(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// 检查路径中是否有 Git for Windows 安装目录，避免重复展示其它 Bash 发行版。
#[cfg(windows)]
fn is_git_bash_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("Git")
    })
}

/// 只枚举固定版本与 edition 位置，并返回每个版本的首个现存 VsDevCmd 启动项。
#[cfg(windows)]
fn discover_visual_studio_prompts(
    candidates: &mut Vec<LocalShellCandidate>,
    seen_ids: &mut HashSet<String>,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    let command_prompt = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32").join("cmd.exe"))
        .filter(|path| path.is_absolute() && is_executable_file(path));
    let Some(command_prompt) = command_prompt else {
        return Ok(());
    };

    if let Some(install) = env::var_os("VSINSTALLDIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        let script = install.join("Common7").join("Tools").join("VsDevCmd.bat");
        if script.is_file() {
            add_vs_prompt(
                candidates,
                seen_ids,
                "vsdevcmd-current",
                "Visual Studio Developer Command Prompt",
                &command_prompt,
                &script,
                cancellation,
            )?;
        }
    }

    let roots = [
        env::var_os("ProgramFiles(x86)"),
        env::var_os("ProgramFiles"),
    ];
    for year in ["2026", "2022", "2019", "2017"] {
        let mut found_for_year = false;
        for root in &roots {
            let Some(root) = root.as_ref().map(PathBuf::from) else {
                continue;
            };
            if !root.is_absolute() {
                continue;
            }
            for edition in [
                "Community",
                "Professional",
                "Enterprise",
                "BuildTools",
                "Preview",
            ] {
                ensure_not_cancelled(cancellation)?;
                let script = root
                    .join("Microsoft Visual Studio")
                    .join(year)
                    .join(edition)
                    .join("Common7")
                    .join("Tools")
                    .join("VsDevCmd.bat");
                if !script.is_file() {
                    continue;
                }
                let id = format!("vsdevcmd-{year}");
                let label = format!("VS Developer Command Prompt {year} ({edition})");
                add_vs_prompt(
                    candidates,
                    seen_ids,
                    &id,
                    &label,
                    &command_prompt,
                    &script,
                    cancellation,
                )?;
                found_for_year = true;
                break;
            }
            if found_for_year {
                break;
            }
        }
    }
    Ok(())
}

/// 把已发现的微软开发者批处理作为候选参数，仅在用户之后启动该候选时才会执行它。
#[cfg(windows)]
fn add_vs_prompt(
    candidates: &mut Vec<LocalShellCandidate>,
    seen_ids: &mut HashSet<String>,
    id: &str,
    label: &str,
    command_prompt: &Path,
    startup_script: &Path,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    let command = format!("call \"{}\"", startup_script.display());
    push_candidate(
        candidates,
        seen_ids,
        id,
        label,
        command_prompt.to_path_buf(),
        vec![OsString::from("/K"), OsString::from(command)],
        cancellation,
    )
}

/// Unix shell 按常见命令提供稳定身份；PATH 之外只检查系统常见 bin 目录。
#[cfg(unix)]
fn discover_unix(cancellation: &CancellationToken) -> AppResult<Vec<LocalShellCandidate>> {
    let mut candidates = Vec::new();
    let mut seen_ids = HashSet::new();
    for (id, label, command) in [
        ("bash", "Bash", "bash"),
        ("zsh", "Zsh", "zsh"),
        ("fish", "Fish", "fish"),
        ("sh", "POSIX Shell", "sh"),
        ("dash", "Dash", "dash"),
        ("ksh", "KornShell", "ksh"),
        ("mksh", "MirBSD Korn Shell", "mksh"),
        ("pwsh", "PowerShell 7", "pwsh"),
    ] {
        ensure_not_cancelled(cancellation)?;
        if let Some(path) = find_in_path(OsStr::new(command), cancellation)?
            .or(find_in_unix_system_dirs(command, cancellation)?)
        {
            push_candidate(
                &mut candidates,
                &mut seen_ids,
                id,
                label,
                path,
                Vec::new(),
                cancellation,
            )?;
        }
    }
    Ok(candidates)
}

/// 在少量标准安装目录内兜底查找，仍要求绝对路径和 Unix 执行权限。
#[cfg(unix)]
fn find_in_unix_system_dirs(
    command: &str,
    cancellation: &CancellationToken,
) -> AppResult<Option<PathBuf>> {
    for directory in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        ensure_not_cancelled(cancellation)?;
        let candidate = Path::new(directory).join(command);
        if is_executable_file(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "../../tests/unit/local_shells.rs"]
mod tests;
