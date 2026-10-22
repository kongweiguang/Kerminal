//! Native RDP client discovery and process launch plans.
//!
//! @author kongweiguang

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};

use crate::process_tree::spawn::ProcessSpawnOptions;
use kerminal_application::{
    AppError, AppResult,
    connections::{RdpClientLifecycle, RdpLaunchTarget},
};
use kerminal_domain::connections::HostEndpoint;
use tokio::process::Command;

#[cfg(any(target_os = "windows", target_os = "macos"))]
use super::artifact::create_rdp_artifact;
#[cfg(target_os = "linux")]
use super::artifact::format_rdp_address;
use super::{RdpProbeAvailability, artifact::TemporaryRdpArtifact};

/// 预先解析的启动方式与受管配置文件；plan 不存储明文凭据。
pub(super) struct RdpLaunchPlan {
    program: PathBuf,
    args: Vec<OsString>,
    pub(super) artifact: Option<TemporaryRdpArtifact>,
    pub(super) endpoint: HostEndpoint,
    pub(super) lifecycle: RdpClientLifecycle,
}

impl RdpLaunchPlan {
    /// 创建不含明文密码参数的命令；进程树和 drop 策略交由统一 spawn wrapper 配置。
    pub(super) fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command
    }

    /// 把 RDP 客户端的原始平台创建标志交给统一 wrapper，避免被 Windows 包装标志覆盖。
    ///
    /// @author kongweiguang
    pub(super) fn spawn_options(&self) -> ProcessSpawnOptions {
        #[cfg(target_os = "windows")]
        let windows_creation_flags = 0x0800_0000;
        #[cfg(not(target_os = "windows"))]
        let windows_creation_flags = 0;

        ProcessSpawnOptions {
            kill_on_drop: true,
            windows_creation_flags,
        }
    }

    /// 启动前取消时显式清理配置文件，失败通过调用方可见错误返回。
    pub(super) async fn cleanup(mut self) -> AppResult<()> {
        match self.artifact.take() {
            Some(artifact) => artifact.cleanup().await,
            None => Ok(()),
        }
    }
}

/// 为 endpoint 构造当前平台命令；非 Windows 平台由客户端交互提示认证。
/// Linux FreeRDP 直接接收目标参数，不创建临时配置文件，因此运行目录仅供 Win/macOS 使用。
///
/// @author kongweiguang
pub(super) async fn build_launch_plan(
    runtime_directory: &Path,
    target: RdpLaunchTarget,
) -> AppResult<RdpLaunchPlan> {
    target.display.validate()?;
    #[cfg(target_os = "windows")]
    {
        build_windows_plan(runtime_directory, target).await
    }
    #[cfg(target_os = "macos")]
    {
        build_macos_plan(runtime_directory, target).await
    }
    #[cfg(target_os = "linux")]
    {
        let _ = runtime_directory;
        build_linux_plan(target)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = (runtime_directory, target);
        Err(AppError::unsupported("当前平台没有可用的 RDP 客户端适配器"))
    }
}

/// 仅探测当前平台的本机启动能力；macOS 的 `open` 不证明已安装 RDP 文件关联。
pub(super) fn probe_client_availability() -> AppResult<RdpProbeAvailability> {
    #[cfg(target_os = "windows")]
    {
        resolve_windows_client()?;
        Ok(RdpProbeAvailability::WindowsMstsc)
    }
    #[cfg(target_os = "linux")]
    {
        resolve_freerdp_client()?;
        Ok(RdpProbeAvailability::LinuxFreeRdp)
    }
    #[cfg(target_os = "macos")]
    {
        let launcher = Path::new("/usr/bin/open");
        if launcher.is_file() {
            Ok(RdpProbeAvailability::MacSystemLauncher)
        } else {
            Err(AppError::not_found("macOS 系统启动器不可用"))
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        Err(AppError::unsupported("当前平台没有可用的 RDP 客户端适配器"))
    }
}

/// 先探测系统 mstsc，再在后台创建 DPAPI 文件，避免 UI/runtime worker 承担同步磁盘操作。
///
/// @author kongweiguang
#[cfg(target_os = "windows")]
async fn build_windows_plan(
    runtime_directory: &Path,
    target: RdpLaunchTarget,
) -> AppResult<RdpLaunchPlan> {
    let program = resolve_windows_client()?;
    let endpoint = target.endpoint;
    let artifact_endpoint = endpoint.clone();
    let password = target.password;
    let display = target.display;
    let directory = runtime_directory.to_path_buf();
    let artifact = tokio::task::spawn_blocking(move || {
        let encrypted = password
            .as_ref()
            .filter(|password| !password.is_empty())
            .map(|password| super::windows_security::protect_rdp_password(password.as_str()))
            .transpose()?;
        create_rdp_artifact(
            &directory,
            &artifact_endpoint,
            encrypted.as_deref(),
            &display,
        )
    })
    .await
    .map_err(|error| AppError::infrastructure_with_source("RDP 临时文件 task 异常退出", error))??;
    let args = vec![artifact.path().as_os_str().to_os_string()];
    Ok(RdpLaunchPlan {
        program,
        args,
        artifact: Some(artifact),
        endpoint,
        lifecycle: RdpClientLifecycle::ManagedProcess,
    })
}

/// 通过 LaunchServices 委派系统客户端；仅把 `open` broker 视为受管进程，不把它冒充 RDP 客户端。
///
/// @author kongweiguang
#[cfg(target_os = "macos")]
async fn build_macos_plan(
    runtime_directory: &Path,
    target: RdpLaunchTarget,
) -> AppResult<RdpLaunchPlan> {
    let program = PathBuf::from("/usr/bin/open");
    if !program.is_file() {
        return Err(AppError::not_found(
            "macOS LaunchServices open 客户端不可用",
        ));
    }
    // 旧版 macOS adapter 不把 vault 密码写入文件，继续交由系统客户端提示认证。
    drop(target.password);
    let endpoint = target.endpoint;
    let display = target.display;
    let artifact_endpoint = endpoint.clone();
    let directory = runtime_directory.to_path_buf();
    let artifact = tokio::task::spawn_blocking(move || {
        create_rdp_artifact(&directory, &artifact_endpoint, None, &display)
    })
    .await
    .map_err(|error| AppError::infrastructure_with_source("RDP 临时文件 task 异常退出", error))??;
    let args = vec![
        OsString::from("-W"),
        artifact.path().as_os_str().to_os_string(),
    ];
    Ok(RdpLaunchPlan {
        program,
        args,
        artifact: Some(artifact),
        endpoint,
        lifecycle: RdpClientLifecycle::DelegatedToSystem,
    })
}

/// Linux 使用已安装 FreeRDP 图形客户端；密码交由客户端交互流程，不放进 argv 或文件。
///
/// @author kongweiguang
#[cfg(target_os = "linux")]
fn build_linux_plan(target: RdpLaunchTarget) -> AppResult<RdpLaunchPlan> {
    let program = resolve_freerdp_client()?;
    drop(target.password);
    let endpoint = target.endpoint;
    let mut args = vec![OsString::from(format!(
        "/v:{}",
        format_rdp_address(&endpoint.host, endpoint.port)
    ))];
    let username = endpoint.username.trim();
    if !username.is_empty() {
        args.push(OsString::from(format!("/u:{username}")));
    }
    if target.display.fullscreen {
        args.push(OsString::from("/f"));
    } else if let (Some(width), Some(height)) = (target.display.width, target.display.height) {
        args.push(OsString::from(format!("/size:{width}x{height}")));
    }
    args.push(OsString::from("+clipboard"));
    Ok(RdpLaunchPlan {
        program,
        args,
        artifact: None,
        endpoint,
        lifecycle: RdpClientLifecycle::ManagedProcess,
    })
}

/// 按 Windows 系统目录优先、PATH 兜底的顺序查找 MSTSC，不运行命令猜测版本。
#[cfg(target_os = "windows")]
fn resolve_windows_client() -> AppResult<PathBuf> {
    let system_root = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot"));
    if let Some(root) = system_root {
        let candidate = PathBuf::from(root).join("System32").join("mstsc.exe");
        if is_runnable_file(&candidate) {
            return Ok(candidate);
        }
    }
    find_executable_in_path(&["mstsc.exe"]).ok_or(AppError::not_found(
        "Windows 远程桌面客户端 mstsc.exe 不可用",
    ))
}

/// 依次查找 FreeRDP 2/3 的 X11 或 Wayland 前端，缺失时明确提示安装客户端。
#[cfg(target_os = "linux")]
fn resolve_freerdp_client() -> AppResult<PathBuf> {
    find_executable_in_path(&[
        "xfreerdp3",
        "wlfreerdp3",
        "xfreerdp",
        "wlfreerdp",
        "xfreerdp2",
    ])
    .ok_or(AppError::not_found(
        "未找到 FreeRDP 图形客户端，请安装 xfreerdp 或 wlfreerdp",
    ))
}

/// 只按 PATH 的文件名与可执行权限探测，不通过 shell 或输出解析执行外部程序。
fn find_executable_in_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let directories = std::env::split_paths(&path).collect::<Vec<_>>();
    find_executable_in(directories.iter().map(PathBuf::as_path), names)
}

/// 在给定目录顺序中返回第一个实际可执行文件，隔离测试也复用同一探测规则。
pub(super) fn find_executable_in<'a>(
    directories: impl IntoIterator<Item = &'a Path>,
    names: &[&str],
) -> Option<PathBuf> {
    directories.into_iter().find_map(|directory| {
        names
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| is_runnable_file(candidate))
    })
}

/// Windows 检查文件存在，Unix 还要求至少一个执行位，避免把不可启动文件当作客户端。
pub(super) fn is_runnable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
