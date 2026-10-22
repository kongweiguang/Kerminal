use super::WinChild;
use crate::cmdbuilder::CommandBuilder;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::{ensure, Error};
use filedescriptor::{FileDescriptor, OwnedHandle};
use lazy_static::lazy_static;
use shared_library::{dynamic_library::DynamicLibrary, LoadingError};
use std::io::Error as IoError;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle};
use std::path::Path;
use std::sync::Mutex;
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;

pub type HPCON = HANDLE;

pub const PSUEDOCONSOLE_INHERIT_CURSOR: DWORD = 0x1;
pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = 0x2;
pub const PSEUDOCONSOLE_WIN32_INPUT_MODE: DWORD = 0x4;
#[allow(dead_code)]
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = 0x8;

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, DWORD, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);
type ReleasePseudoConsoleFn = unsafe extern "system" fn(HPCON) -> HRESULT;

/// 固定持有创建该 HPCON 时选中的模块，避免从另一 DLL 误调用同名 Release 函数。
#[allow(non_snake_case)]
struct ConPtyFuncs {
    _library_guard: DynamicLibrary,
    CreatePseudoConsole: CreatePseudoConsoleFn,
    ResizePseudoConsole: ResizePseudoConsoleFn,
    ClosePseudoConsole: ClosePseudoConsoleFn,
    ReleasePseudoConsole: Option<ReleasePseudoConsoleFn>,
}

impl ConPtyFuncs {
    /// 必需符号决定该 DLL 是否可作 ConPTY backend；Release 只在同一 DLL 中可选探测。
    ///
    /// @author kongweiguang
    fn open(path: &Path) -> Result<Self, LoadingError> {
        let library = DynamicLibrary::open(Some(path))
            .map_err(|descr| LoadingError::LibraryNotFound { descr })?;
        let create_symbol =
            unsafe { library.symbol::<()>("CreatePseudoConsole") }.map_err(|_| {
                LoadingError::SymbolNotFound {
                    symbol: "CreatePseudoConsole",
                }
            })?;
        let resize_symbol =
            unsafe { library.symbol::<()>("ResizePseudoConsole") }.map_err(|_| {
                LoadingError::SymbolNotFound {
                    symbol: "ResizePseudoConsole",
                }
            })?;
        let close_symbol = unsafe { library.symbol::<()>("ClosePseudoConsole") }.map_err(|_| {
            LoadingError::SymbolNotFound {
                symbol: "ClosePseudoConsole",
            }
        })?;
        let release_symbol = unsafe { library.symbol::<()>("ReleasePseudoConsole") }.ok();

        // SAFETY: 每个地址均来自上述仍由 `_library_guard` 持有的同一模块，且按 Win32 ABI 声明。
        let create =
            unsafe { std::mem::transmute::<*mut (), CreatePseudoConsoleFn>(create_symbol) };
        // SAFETY: 同上，符号签名与 ConPTY API 一致。
        let resize =
            unsafe { std::mem::transmute::<*mut (), ResizePseudoConsoleFn>(resize_symbol) };
        // SAFETY: 同上，符号签名与 ConPTY API 一致。
        let close = unsafe { std::mem::transmute::<*mut (), ClosePseudoConsoleFn>(close_symbol) };
        // SAFETY: Release 是可选符号；存在时来自创建 HPCON 的同一模块并使用其 Win32 ABI。
        let release = release_symbol.map(|symbol| unsafe {
            std::mem::transmute::<*mut (), ReleasePseudoConsoleFn>(symbol)
        });

        Ok(Self {
            _library_guard: library,
            CreatePseudoConsole: create,
            ResizePseudoConsole: resize,
            ClosePseudoConsole: close,
            ReleasePseudoConsole: release,
        })
    }
}

/// 先确认系统 ConPTY 基线，再优先采用随应用部署的 backend，保持旧 Windows 启动兼容。
///
/// @author kongweiguang
fn load_conpty() -> ConPtyFuncs {
    // If the kernel doesn't export these functions then their system is
    // too old and we cannot run.
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );

    // We prefer to use a sideloaded conpty.dll and openconsole.exe host deployed
    // alongside the application.  We check for this after checking for kernel
    // support so that we don't try to proceed and do something crazy.
    if let Ok(sideloaded) = ConPtyFuncs::open(Path::new("conpty.dll")) {
        sideloaded
    } else {
        kernel
    }
}

lazy_static! {
    static ref CONPTY: ConPtyFuncs = load_conpty();
}

pub struct PsuedoCon {
    con: HPCON,
    released: bool,
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        unsafe { (CONPTY.ClosePseudoConsole)(self.con) };
    }
}

impl PsuedoCon {
    /// 初始化一次 ConPTY 并保留同一 backend 对应的最终 Close owner。
    ///
    /// @author kongweiguang
    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result = unsafe {
            (CONPTY.CreatePseudoConsole)(
                size,
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                PSUEDOCONSOLE_INHERIT_CURSOR
                    | PSEUDOCONSOLE_RESIZE_QUIRK
                    | PSEUDOCONSOLE_WIN32_INPUT_MODE,
                &mut con,
            )
        };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {}",
            result
        );
        Ok(Self {
            con,
            released: false,
        })
    }

    /// 只在系统确认成功后解除 console 对 client 的持有；实际 HPCON 与管道仍由最终 Drop 清理。
    ///
    /// @author kongweiguang
    pub fn release_after_child_exit(&mut self) -> Result<bool, Error> {
        if self.released {
            return Ok(true);
        }
        let Some(release) = CONPTY.ReleasePseudoConsole else {
            return Ok(false);
        };
        // 此时调用方已确认原生 child 退出；仅 S_OK 才能确认 Release，不关闭本端 reader/writer。
        let result = unsafe { release(self.con) };
        ensure!(
            result == S_OK,
            "failed to release pseudo console: HRESULT {}",
            result
        );
        self.released = true;
        Ok(true)
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        let result = unsafe { (CONPTY.ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    /// 按 portable-pty 既有方式启动进程，保留未请求 Job 原子归属时的兼容行为。
    ///
    /// @author kongweiguang
    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        self.spawn_command_with_job(cmd, None)
    }

    /// 在同一个 STARTUPINFOEX 属性表内同时设置 ConPTY 与 Job List，失败时不退回普通启动。
    ///
    /// @author kongweiguang
    pub fn spawn_command_in_job(
        &self,
        cmd: CommandBuilder,
        job: BorrowedHandle<'_>,
    ) -> anyhow::Result<WinChild> {
        self.spawn_command_with_job(cmd, Some(job))
    }

    /// 构造完整启动属性后只调用一次 CreateProcessW，让 Job 成员关系早于 child 首条指令；失败时保留系统错误链，不泄露命令行或工作目录。
    ///
    /// ConPTY 不能假定宿主标准流未重定向：启用 `STARTF_USESTDHANDLES` 并显式把三个标准句柄留空，
    /// 可阻止控制台子进程把宿主的重定向流当作自己的标准流；`bInheritHandles` 仍为 false，
    /// 避免把宿主的其它句柄传入子进程。ConPTY 与 Job 属性继续在同一个 `STARTUPINFOEXW` 中创建。
    ///
    /// @author kongweiguang
    fn spawn_command_with_job(
        &self,
        cmd: CommandBuilder,
        job: Option<BorrowedHandle<'_>>,
    ) -> anyhow::Result<WinChild> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = ptr::null_mut();
        si.StartupInfo.hStdOutput = ptr::null_mut();
        si.StartupInfo.hStdError = ptr::null_mut();
        let mut attrs = ProcThreadAttributeList::with_capacity(if job.is_some() { 2 } else { 1 })?;
        attrs.set_pty(self.con)?;
        if let Some(job) = job.as_ref() {
            attrs.set_job(job)?;
        }
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = cmd.cmdline()?;

        let cwd = cmd.current_directory();

        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
                cwd.as_ref()
                    .map(|c| c.as_slice().as_ptr())
                    .unwrap_or(ptr::null()),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            return Err(Error::new(IoError::last_os_error()).context("CreateProcessW failed"));
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        Ok(WinChild {
            proc: Mutex::new(proc),
        })
    }
}
