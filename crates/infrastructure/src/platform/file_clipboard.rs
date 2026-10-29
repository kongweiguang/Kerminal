//! 将已下载的本机路径写入操作系统文件剪贴板。
//!
//! @author kongweiguang

use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::time::Duration;

use async_trait::async_trait;
use kerminal_application::{AppError, AppResult, files::clipboard_download::FileClipboardPort};
use kerminal_domain::files::TransferKind;

/// 使用 Downloads 目录与原生文件列表剪贴板，复制完成后让用户可继续使用下载文件。
pub struct SystemFileClipboard {
    downloads_directory: PathBuf,
}

impl SystemFileClipboard {
    /// 解析与 main 相同的系统下载目录；不在构造时创建目录或触碰剪贴板。
    pub fn new() -> AppResult<Self> {
        let downloads_directory = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
            .ok_or_else(|| AppError::infrastructure("无法定位系统下载目录"))?;
        if !downloads_directory.is_absolute() {
            return Err(AppError::invalid("系统下载目录必须是绝对路径"));
        }
        Ok(Self {
            downloads_directory,
        })
    }
}

#[async_trait]
impl FileClipboardPort for SystemFileClipboard {
    /// 先在 blocking pool 创建同目录唯一目标，供统一 TransferService 执行原子提交。
    async fn reserve_download_target(
        &self,
        name: String,
        kind: TransferKind,
    ) -> AppResult<PathBuf> {
        self.ensure_supported()?;
        let directory = self.downloads_directory.clone();
        tokio::task::spawn_blocking(move || reserve_unique_download_path(&directory, &name, kind))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("创建剪贴板下载目标任务失败", source)
            })?
    }

    /// CF_HDROP 只发布完整路径；失败时不清理已下载文件，保留 main 的 Downloads 行为。
    async fn publish_files(&self, paths: Vec<PathBuf>) -> AppResult<()> {
        self.ensure_supported()?;
        let expected_directory = self.downloads_directory.clone();
        tokio::task::spawn_blocking(move || {
            validate_clipboard_paths(&expected_directory, &paths)?;
            #[cfg(windows)]
            {
                write_windows_file_clipboard(&paths)
            }
            #[cfg(not(windows))]
            {
                Err(unsupported_clipboard("写入"))
            }
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("写入系统文件剪贴板任务失败", source)
        })?
    }

    /// 仅删除下载目录直接子项；远端传输仍活动或结果未知时由 application 保留目标。
    async fn remove_download_target(&self, path: PathBuf, kind: TransferKind) -> AppResult<()> {
        let directory = self.downloads_directory.clone();
        tokio::task::spawn_blocking(move || remove_download_target(&directory, &path, kind))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("清理剪贴板下载目标任务失败", source)
            })?
    }

    /// Windows 提供 CF_HDROP；其它平台按 main 的系统文件列表能力返回不支持。
    fn ensure_supported(&self) -> AppResult<()> {
        #[cfg(windows)]
        {
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Err(unsupported_clipboard("写入"))
        }
    }
}

/// 预留名称采用 main 的非法字符替换和编号规则，并通过 create_new 抵御并发覆盖。
fn reserve_unique_download_path(
    downloads_directory: &Path,
    remote_name: &str,
    kind: TransferKind,
) -> AppResult<PathBuf> {
    let fallback = match kind {
        TransferKind::File => "remote-file",
        TransferKind::Directory => "remote-directory",
    };
    let name = safe_local_entry_name(remote_name, fallback);
    fs::create_dir_all(downloads_directory)
        .map_err(|source| AppError::infrastructure_with_source("创建系统下载目录失败", source))?;

    let initial = downloads_directory.join(&name);
    if reserve_local_path(&initial, kind)? {
        return Ok(initial);
    }

    let (stem, extension) = split_local_file_name(&name);
    for index in 2..1000 {
        let candidate = downloads_directory.join(format!("{stem} ({index}){extension}"));
        if reserve_local_path(&candidate, kind)? {
            return Ok(candidate);
        }
    }

    loop {
        let candidate =
            downloads_directory.join(format!("{stem}-{}{extension}", uuid::Uuid::new_v4()));
        if reserve_local_path(&candidate, kind)? {
            return Ok(candidate);
        }
    }
}

/// 删除只允许发生在下载目录第一层，且根据 symlink metadata 移除链接本身而不跟随。
fn remove_download_target(
    downloads_directory: &Path,
    path: &Path,
    _kind: TransferKind,
) -> AppResult<()> {
    if path.parent() != Some(downloads_directory) || path.file_name().is_none() {
        return Err(AppError::invalid("剪贴板下载目标不属于系统下载目录"));
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取剪贴板下载目标失败",
                source,
            ));
        }
    };
    let result = if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|source| AppError::infrastructure_with_source("删除剪贴板下载目标失败", source))
}

/// 以主平台 Windows 文件名规则替换危险字符，并为点名或空名提供稳定后备名。
fn safe_local_entry_name(name: &str, fallback: &str) -> String {
    let candidate = name
        .trim()
        .trim_matches(|ch| matches!(ch, '/' | '\\'))
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    let candidate = candidate.trim().trim_matches('.').to_owned();
    if candidate.is_empty() || candidate == ".." {
        fallback.to_owned()
    } else {
        candidate
    }
}

/// 保留常见扩展名位置，编号后的目标仍由文件管理器按原类型识别。
fn split_local_file_name(file_name: &str) -> (String, String) {
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(file_name)
        .to_owned();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    (stem, extension)
}

/// 通过 create_new 或 create_dir 原子占名，返回的文件/目录由传输队列覆盖填充。
fn reserve_local_path(path: &Path, kind: TransferKind) -> AppResult<bool> {
    let result = match kind {
        TransferKind::File => OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| ()),
        TransferKind::Directory => fs::create_dir(path),
    };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(source) => Err(AppError::infrastructure_with_source(
            "预留剪贴板下载目标失败",
            source,
        )),
    }
}

/// 写入前验证文件存在且直接位于本机下载目录，拒绝空列表和可被调用方替换的路径。
fn validate_clipboard_paths(downloads_directory: &Path, paths: &[PathBuf]) -> AppResult<()> {
    if paths.is_empty() {
        return Err(AppError::invalid("系统文件剪贴板路径不能为空"));
    }
    for path in paths {
        if !path.is_absolute() || path.parent() != Some(downloads_directory) {
            return Err(AppError::invalid("剪贴板路径不属于系统下载目录"));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            AppError::infrastructure_with_source("读取剪贴板下载结果失败", source)
        })?;
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(AppError::unsupported("系统文件剪贴板只支持本机文件或目录"));
        }
    }
    Ok(())
}

/// 固定不支持文案与 main 一致，避免非 Windows 构造出不可粘贴的伪成功状态。
#[cfg(not(windows))]
fn unsupported_clipboard(action: &str) -> AppError {
    AppError::unsupported(if action == "写入" {
        "当前平台暂不支持写入系统文件剪贴板"
    } else {
        "当前平台暂不支持系统文件剪贴板"
    })
}

#[cfg(windows)]
const CF_HDROP: u32 = 15;

#[cfg(windows)]
#[repr(C)]
struct DropFiles {
    p_files: u32,
    point: windows_sys::Win32::Foundation::POINT,
    f_nc: i32,
    f_wide: i32,
}

#[cfg(windows)]
struct OpenClipboardGuard {
    owner: windows_sys::Win32::Foundation::HWND,
}

#[cfg(windows)]
impl OpenClipboardGuard {
    /// 使用隐藏的 STATIC 窗口作为 clipboard owner，避免 NULL owner 使 EmptyClipboard 后写入失败。
    fn open() -> AppResult<Self> {
        use windows_sys::Win32::{
            Foundation::HWND,
            System::DataExchange::OpenClipboard,
            UI::WindowsAndMessaging::{CreateWindowExW, WS_POPUP},
        };

        const STATIC_CLASS: [u16; 7] = [83, 84, 65, 84, 73, 67, 0];
        const EMPTY_TITLE: [u16; 1] = [0];
        let owner: HWND = unsafe {
            CreateWindowExW(
                0,
                STATIC_CLASS.as_ptr(),
                EMPTY_TITLE.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if owner.is_null() {
            return Err(AppError::infrastructure_with_source(
                "创建系统剪贴板 owner 窗口失败",
                io::Error::last_os_error(),
            ));
        }

        for _ in 0..40 {
            if unsafe { OpenClipboard(owner) } != 0 {
                return Ok(Self { owner });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(owner);
        }
        Err(AppError::infrastructure_with_source(
            "系统剪贴板正被其他程序占用",
            io::Error::last_os_error(),
        ))
    }
}

#[cfg(windows)]
impl Drop for OpenClipboardGuard {
    /// 先关闭剪贴板，再销毁短命 owner；已交给系统的 CF_HDROP 内存由系统持有。
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::DataExchange::CloseClipboard();
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.owner);
        }
    }
}

#[cfg(windows)]
/// 构造 Unicode DROPFILES 负载；双 NUL 终止路径表以满足 Shell 文件列表格式。
fn dropfiles_payload(paths: &[PathBuf]) -> AppResult<Vec<u8>> {
    use std::{mem::size_of, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Foundation::POINT;

    let header = DropFiles {
        p_files: size_of::<DropFiles>() as u32,
        point: POINT { x: 0, y: 0 },
        f_nc: 0,
        f_wide: 1,
    };
    let mut payload = Vec::new();
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const DropFiles).cast::<u8>(),
            size_of::<DropFiles>(),
        )
    };
    payload.extend_from_slice(header_bytes);
    for path in paths {
        let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.is_empty() || encoded.contains(&0) {
            return Err(AppError::invalid("系统剪贴板路径无效"));
        }
        for unit in encoded.into_iter().chain(std::iter::once(0)) {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
    }
    payload.extend_from_slice(&0u16.to_le_bytes());
    Ok(payload)
}

#[cfg(windows)]
/// 用 GlobalAlloc 分配可转移所有权的 CF_HDROP 内存块，并在 API 失败时释放本地所有权。
fn write_windows_file_clipboard(paths: &[PathBuf]) -> AppResult<()> {
    use windows_sys::Win32::{
        Foundation::GlobalFree,
        System::{
            DataExchange::{EmptyClipboard, SetClipboardData},
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
    };

    let payload = dropfiles_payload(paths)?;
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.len()) };
    if memory.is_null() {
        return Err(AppError::infrastructure_with_source(
            "分配系统剪贴板内存失败",
            io::Error::last_os_error(),
        ));
    }
    let locked = unsafe { GlobalLock(memory) };
    if locked.is_null() {
        let source = io::Error::last_os_error();
        unsafe { GlobalFree(memory) };
        return Err(AppError::infrastructure_with_source(
            "锁定系统剪贴板内存失败",
            source,
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), locked.cast::<u8>(), payload.len());
        GlobalUnlock(memory);
    }

    let _clipboard = match OpenClipboardGuard::open() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            unsafe { GlobalFree(memory) };
            return Err(error);
        }
    };
    if unsafe { EmptyClipboard() } == 0 {
        let source = io::Error::last_os_error();
        unsafe { GlobalFree(memory) };
        return Err(AppError::infrastructure_with_source(
            "清空旧系统剪贴板失败",
            source,
        ));
    }
    if unsafe { SetClipboardData(CF_HDROP, memory) }.is_null() {
        let source = io::Error::last_os_error();
        unsafe { GlobalFree(memory) };
        return Err(AppError::infrastructure_with_source(
            "写入系统文件剪贴板失败",
            source,
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/platform_file_clipboard.rs"]
mod tests;
