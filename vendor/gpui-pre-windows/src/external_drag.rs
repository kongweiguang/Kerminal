//! Windows outbound file drag, including ownership of asynchronously copied staging files.
//!
//! Local adaptations @author kongweiguang

mod stream;

use std::{
    mem::{ManuallyDrop, size_of},
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    DeferredFileDragProvider, ExternalDragPayload, ExternalFileDescriptor, ExternalFileDragOutcome,
    PreparedExternalFileDrag,
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
            DV_E_FORMATETC, DV_E_LINDEX, E_FAIL, E_NOTIMPL, GlobalFree, HANDLE,
            OLE_E_ADVISENOTSUPPORTED, RPC_S_CALLPENDING, S_OK,
        },
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::{
            Com::{
                COWAIT_DISPATCH_CALLS, COWAIT_DISPATCH_WINDOW_MESSAGES, CoWaitForMultipleHandles,
                DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IBindCtx, IDataObject,
                IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0,
                TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            DataExchange::RegisterClipboardFormatW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            Ole::{
                CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource,
                IDropSource_Impl, OleInitialize, OleUninitialize,
            },
            SystemInformation::GetTickCount,
            SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS},
            Threading::{
                AttachThreadInput, CreateEventW, GetCurrentProcessId, GetCurrentThreadId, SetEvent,
            },
        },
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON},
            Shell::{
                CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, DROPFILES, FD_ATTRIBUTES, FD_FILESIZE,
                FD_UNICODE, FILEDESCRIPTORW, IDataObjectAsyncCapability,
                IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
            },
            WindowsAndMessaging::{
                DispatchMessageW, GetWindowThreadProcessId, MSG, PM_NOREMOVE, PM_REMOVE,
                PeekMessageW, TranslateMessage,
            },
        },
    },
    core::{BOOL, ComObject, HRESULT, IUnknownImpl, Ref, Result as WinResult, implement},
};

const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 记录同一次原生手势的 Escape 消费状态，关联 UI 与 OLE 线程的迟到消息。
#[derive(Default)]
pub(crate) struct DragKeyboardState(Mutex<DragKeyboardDetails>);

#[derive(Default)]
struct DragKeyboardDetails {
    active: bool,
    focused: bool,
    cancel_requested: bool,
    ole_escape: bool,
    key_down: bool,
    key_up: bool,
    finished_at: Option<u32>,
}

impl DragKeyboardState {
    /// 开始一个独立手势，旧手势的状态不能影响本次输入。
    pub(crate) fn begin(&self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = DragKeyboardDetails {
            active: true,
            focused: true,
            ..Default::default()
        };
    }

    /// 系统拖放一结束便停止过滤普通新按键，仅保留取消键的待处理释放。
    fn finish(&self) {
        self.finish_at(unsafe { GetTickCount() });
    }

    /// 保留手势结束时的系统消息时钟；重复收尾不得覆盖第一次完成时刻。
    fn finish_at(&self, tick: u32) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.active {
            state.active = false;
            state.finished_at = Some(tick);
        }
    }

    /// OLE 先观察到 Escape 时，后到达窗口的同次按键仍须被消费。
    fn observe_ole_escape(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.active {
            state.cancel_requested = true;
        }
        if state.focused && !state.key_up {
            state.ole_escape = true;
        }
    }

    /// 只消费拖动期间或 OLE 已确认取消且尚未释放的 Escape。
    pub(crate) fn consume_key_down(&self, message_time: u32, is_repeat: bool) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let queued_before_finish = state
            .finished_at
            .is_some_and(|finished_at| finished_at.wrapping_sub(message_time) <= i32::MAX as u32);
        let pending_escape = (state.ole_escape || state.key_down) && !state.key_up;
        if state.active || (pending_escape && (queued_before_finish || is_repeat)) {
            if state.active {
                state.cancel_requested = true;
            }
            if state.active && state.key_up {
                state.ole_escape = false;
                state.key_up = false;
            }
            state.key_down = true;
            true
        } else {
            if !state.active {
                state.ole_escape = false;
                state.key_down = false;
                state.key_up = true;
            }
            false
        }
    }

    /// 窗口线程先消费按键时也通知 OLE，避免短按在跨线程轮询间被遗漏。
    fn cancellation_requested(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.active && state.cancel_requested
    }

    /// 与已消费的按下配对；结束后不拦截用户发起的下一次按键。
    pub(crate) fn consume_key_up(&self) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.active || ((state.ole_escape || state.key_down) && !state.key_up) {
            state.key_up = true;
            true
        } else {
            false
        }
    }

    /// 焦点切换后旧窗口可能收不到释放；丢弃待配对标记，防止拦截之后的新按键。
    pub(crate) fn focus_changed(&self, focused: bool) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.focused = focused;
        state.ole_escape = false;
        state.key_down = false;
        state.key_up = !focused;
    }
}

/// 线程提前失败或退出时也要结束键盘过滤，不影响后续普通操作。
struct DragKeyboardLifetime(Arc<DragKeyboardState>);

impl Drop for DragKeyboardLifetime {
    /// 异常路径也解除手势期过滤，不能让后续普通 Escape 失效。
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// OLE 源线程只在实际拖动期间共享源窗口的前台输入队列。
struct DragInputAttachment {
    drag_thread: u32,
    source_thread: u32,
}

impl DragInputAttachment {
    /// 确认源窗口仍属于当前进程，并先建立 OLE 消息队列；关联失败保留当前线程的 Win32 错误。
    fn attach(source_window: isize) -> WinResult<Self> {
        let mut process = 0;
        let source_thread = unsafe {
            GetWindowThreadProcessId(
                windows::Win32::Foundation::HWND(source_window as *mut _),
                Some(&mut process),
            )
        };
        let drag_thread = unsafe { GetCurrentThreadId() };
        if source_thread == 0
            || source_thread == drag_thread
            || process != unsafe { GetCurrentProcessId() }
        {
            return Err(E_FAIL.into());
        }
        let mut message = MSG::default();
        let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
        if !unsafe { AttachThreadInput(drag_thread, source_thread, true) }.as_bool() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self {
            drag_thread,
            source_thread,
        })
    }
}

impl Drop for DragInputAttachment {
    /// 退出系统拖放后立即解除关联，异步复制不能持续影响源窗口输入。
    fn drop(&mut self) {
        if !unsafe { AttachThreadInput(self.drag_thread, self.source_thread, false) }.as_bool() {
            log::warn!("failed to detach external drag input queues");
        }
    }
}

/// 仅在显式开启验收诊断时输出阶段，不记录文件名、路径或内容。
pub(crate) fn drag_diagnostic(stage: impl std::fmt::Display) {
    if std::env::var_os("KERMINAL_EXTERNAL_DRAG_DIAGNOSTICS").is_some_and(|value| value == "1") {
        eprintln!("KERMINAL_EXTERNAL_DRAG {stage}");
    }
}

/// 后台结果使用内核事件唤醒 STA，等待期间继续派发 COM，避免取消或完成回执死锁。
struct DeferredWork<T> {
    event: isize,
    result: Mutex<Option<Result<Arc<T>, String>>>,
}

impl<T: Send + Sync + 'static> DeferredWork<T> {
    /// 一次手势只启动一次工作；异常也转成明确失败并唤醒所有等待方。
    fn start(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> WinResult<Arc<Self>> {
        let event = unsafe { CreateEventW(None, true, false, windows::core::PCWSTR::null())? };
        let work = Arc::new(Self {
            event: event.0 as isize,
            result: Mutex::new(None),
        });
        let worker = work.clone();
        std::thread::Builder::new()
            .name("kerminal-virtual-file".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                    .unwrap_or_else(|_| Err("拖出准备任务意外终止".into()))
                    .map(Arc::new);
                *worker
                    .result
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(result);
                let _ = unsafe { SetEvent(HANDLE(worker.event as *mut _)) };
            })
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        Ok(work)
    }

    /// 不持锁派发 COM；取消后有界等待，异常时通知后台停止并保留未知结果。
    fn wait(
        &self,
        provider: &dyn DeferredFileDragProvider,
        drag_active: &DragKeyboardState,
    ) -> WinResult<Arc<T>> {
        self.wait_with_timeout(provider, drag_active, CANCEL_SETTLE_TIMEOUT)
    }

    /// 允许测试缩短取消截止时间，正常活动不受固定超时限制。
    fn wait_with_timeout(
        &self,
        provider: &dyn DeferredFileDragProvider,
        drag_active: &DragKeyboardState,
        cancellation_timeout: Duration,
    ) -> WinResult<Arc<T>> {
        let mut cancelled_at = None;
        loop {
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
            {
                return result.clone().map_err(|_| E_FAIL.into());
            }
            cancel_on_drag_escape(provider, drag_active, || unsafe {
                GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8001 != 0
            });
            if provider.is_cancelled()
                && cancelled_at.get_or_insert_with(Instant::now).elapsed() >= cancellation_timeout
            {
                return Err(E_FAIL.into());
            }
            let flags = (COWAIT_DISPATCH_CALLS.0 | COWAIT_DISPATCH_WINDOW_MESSAGES.0) as u32;
            if let Err(error) =
                unsafe { CoWaitForMultipleHandles(flags, 50, &[HANDLE(self.event as *mut _)]) }
            {
                if error.code() != RPC_S_CALLPENDING {
                    provider.cancel();
                    return Err(error);
                }
            }
        }
    }

    /// 收尾只检查已返回结果，不在 STA 阻塞或取走可供后续流复用的结果。
    fn completed(&self) -> Option<Result<Arc<T>, String>> {
        self.result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// 只在 DoDragDrop 期间读取 Escape；与放置结束共用锁，异步复制不受后续按键影响。
fn cancel_on_drag_escape(
    provider: &dyn DeferredFileDragProvider,
    drag_active: &DragKeyboardState,
    escape_pressed: impl FnOnce() -> bool,
) {
    let mut state = drag_active
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.active && escape_pressed() {
        state.cancel_requested = true;
        if state.focused && !state.key_up {
            state.ole_escape = true;
        }
        provider.cancel();
    }
}

impl<T> Drop for DeferredWork<T> {
    /// 最后一个后台或 COM owner 离开后关闭内核句柄。
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(HANDLE(self.event as *mut _)) };
    }
}

/// 放置回执与目标异步复制完成回执必须分开保存。
#[derive(Default)]
struct AsyncCopy {
    enabled: bool,
    in_operation: bool,
    result: Option<(HRESULT, u32)>,
    internal_drop: bool,
    owner: Option<ComObject<FileDataObject>>,
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct FileDataObject {
    source: DragDataSource,
    source_window: isize,
    copy: Mutex<AsyncCopy>,
    drag_active: Arc<DragKeyboardState>,
    streams: Arc<AtomicUsize>,
    activity: Arc<AtomicU64>,
}

enum DragDataSource {
    Local(Vec<PathBuf>),
    Virtual {
        provider: Arc<dyn DeferredFileDragProvider>,
        descriptors: Mutex<Option<Arc<DeferredWork<Vec<ExternalFileDescriptor>>>>>,
        prepared: Mutex<Option<Arc<DeferredWork<PreparedExternalFileDrag>>>>,
    },
}

impl FileDataObject {
    /// 交给 OLE 前拒绝空路径和相对路径，避免系统接收无法定位的文件。
    fn new(
        payload: &ExternalDragPayload,
        source_window: isize,
        drag_active: Arc<DragKeyboardState>,
    ) -> WinResult<Self> {
        let source = match payload {
            ExternalDragPayload::Files(paths) => {
                let paths = paths
                    .entries()
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>();
                if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
                    return Err(E_FAIL.into());
                }
                DragDataSource::Local(paths)
            }
            ExternalDragPayload::DeferredFiles(provider) => DragDataSource::Virtual {
                provider: provider.0.clone(),
                descriptors: Mutex::new(None),
                prepared: Mutex::new(None),
            },
        };
        Ok(Self {
            source,
            source_window,
            copy: Mutex::new(AsyncCopy::default()),
            drag_active,
            streams: Arc::new(AtomicUsize::new(0)),
            activity: Arc::new(AtomicU64::new(0)),
        })
    }

    /// 目标未回报完成时可能仍在读取，暂存文件不得自动删除。
    fn preserve_if_unconfirmed(&self) {
        if let DragDataSource::Virtual { prepared, .. } = &self.source {
            if let Some(Ok(value)) = prepared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .and_then(|work| work.completed())
            {
                value.owner.preserve();
            }
        }
    }

    /// 超时保留文件并解除 COM 自持引用，不能让失联目标永久占用线程和 owner。
    fn abandon_async(&self) {
        self.preserve_if_unconfirmed();
        let released = {
            let mut copy = self.copy.lock().unwrap_or_else(|error| error.into_inner());
            copy.enabled = false;
            copy.in_operation = false;
            copy.owner.take()
        };
        drop(released);
    }

    /// 目录清单仅启动一次；等待不持锁，COM 可重入处理取消。
    fn descriptors(&self) -> WinResult<Arc<Vec<ExternalFileDescriptor>>> {
        drag_diagnostic("descriptors_wait");
        let DragDataSource::Virtual {
            provider,
            descriptors,
            ..
        } = &self.source
        else {
            return Err(E_FAIL.into());
        };
        let work = {
            let mut slot = descriptors
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if slot.is_none() {
                let provider = provider.clone();
                *slot = Some(DeferredWork::start(move || provider.describe())?);
            }
            slot.as_ref().cloned().ok_or(E_FAIL)?
        };
        let result = work.wait(provider.as_ref(), &self.drag_active);
        drag_diagnostic(if result.is_ok() {
            "descriptors_ready"
        } else {
            "descriptors_failed"
        });
        result
    }

    /// 文件正文只在目标请求 FileContents 后入队，全部完成且校验清单后提供真实文件。
    fn prepared(&self) -> WinResult<Arc<PreparedExternalFileDrag>> {
        drag_diagnostic("contents_prepare_wait");
        let DragDataSource::Virtual {
            provider, prepared, ..
        } = &self.source
        else {
            return Err(E_FAIL.into());
        };
        let work = {
            let mut slot = prepared.lock().unwrap_or_else(|error| error.into_inner());
            if slot.is_none() {
                let provider = provider.clone();
                *slot = Some(DeferredWork::start(move || provider.prepare())?);
            }
            slot.as_ref().cloned().ok_or(E_FAIL)?
        };
        let result = work.wait(provider.as_ref(), &self.drag_active);
        drag_diagnostic(if result.is_ok() {
            "contents_prepare_ready"
        } else {
            "contents_prepare_failed"
        });
        result
    }

    /// 正在运行的后台准备不能在结果未知时失去其持久化 owner。
    fn pending_work(&self) -> bool {
        match &self.source {
            DragDataSource::Local(_) => false,
            DragDataSource::Virtual {
                descriptors,
                prepared,
                ..
            } => [
                descriptors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|work| work.completed().is_none()),
                prepared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|work| work.completed().is_none()),
            ]
            .into_iter()
            .any(|pending| pending),
        }
    }

    /// 返回脱敏准备错误，供原文件面板显示并阻断伪成功。
    fn work_failure(&self) -> Option<String> {
        match &self.source {
            DragDataSource::Local(_) => None,
            DragDataSource::Virtual {
                descriptors,
                prepared,
                ..
            } => descriptors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|work| work.completed())
                .and_then(Result::err)
                .or_else(|| {
                    prepared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .and_then(|work| work.completed())
                        .and_then(Result::err)
                }),
        }
    }
}

/// 只发布 Windows 文件列表格式，拖出语义始终是复制。
fn file_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

struct VirtualFormats {
    descriptor: u16,
    contents: u16,
    source: u16,
}

/// 注册系统虚拟文件格式和仅用于本窗口回拖的私有身份格式。
fn virtual_formats() -> WinResult<&'static VirtualFormats> {
    static FORMATS: OnceLock<VirtualFormats> = OnceLock::new();
    if let Some(formats) = FORMATS.get() {
        return Ok(formats);
    }
    let descriptor = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) };
    let contents = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) };
    let source = unsafe {
        RegisterClipboardFormatW(windows::core::w!("Kerminal.GPUI.VirtualDragSource.v1"))
    };
    if descriptor == 0 || contents == 0 || source == 0 {
        return Err(E_FAIL.into());
    }
    Ok(FORMATS.get_or_init(|| VirtualFormats {
        descriptor: descriptor as u16,
        contents: contents as u16,
        source: source as u16,
    }))
}

/// 格式探测保持无副作用，lindex 负一可用于只查询 FileContents 是否支持。
fn format_for(format: u16, tymed: u32, lindex: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex,
        tymed,
    }
}

/// 每个目录都独立描述，目标可创建空目录及带层级的多选文件。
fn descriptor_medium(entries: &[ExternalFileDescriptor]) -> WinResult<STGMEDIUM> {
    if entries.is_empty() || entries.len() > 10_000 {
        return Err(E_FAIL.into());
    }
    let size = size_of::<u32>()
        .checked_add(
            entries
                .len()
                .checked_mul(size_of::<FILEDESCRIPTORW>())
                .ok_or(E_FAIL)?,
        )
        .ok_or(E_FAIL)?;
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, size)? };
    let destination = unsafe { GlobalLock(handle) };
    if destination.is_null() {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(E_FAIL.into());
    }
    let result = (|| {
        unsafe {
            std::ptr::write_unaligned(destination.cast::<u32>(), entries.len() as u32);
        }
        for (index, entry) in entries.iter().enumerate() {
            let name = entry.relative_path.replace('/', "\\");
            let units = name.encode_utf16().collect::<Vec<_>>();
            if units.is_empty()
                || units.len() >= 260
                || units.contains(&0)
                || name
                    .split('\\')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(E_FAIL.into());
            }
            let mut descriptor = FILEDESCRIPTORW::default();
            descriptor.dwFlags = FD_UNICODE.0 as u32 | FD_ATTRIBUTES.0 as u32;
            descriptor.dwFileAttributes = if entry.is_directory {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_NORMAL.0
            };
            if let Some(size) = entry.size.filter(|_| !entry.is_directory) {
                descriptor.dwFlags |= FD_FILESIZE.0 as u32;
                descriptor.nFileSizeHigh = (size >> 32) as u32;
                descriptor.nFileSizeLow = size as u32;
            }
            let mut filename = [0u16; 260];
            filename[..units.len()].copy_from_slice(&units);
            descriptor.cFileName = filename;
            unsafe {
                std::ptr::write_unaligned(
                    destination
                        .cast::<u8>()
                        .add(size_of::<u32>() + index * size_of::<FILEDESCRIPTORW>())
                        .cast::<FILEDESCRIPTORW>(),
                    descriptor,
                );
            }
        }
        Ok(())
    })();
    let _ = unsafe { GlobalUnlock(handle) };
    if let Err(error) = result {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(error);
    }
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: handle },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

/// 回拖源窗口时用 HWND 身份恢复原 GPUI 载荷，不向系统发布不存在的本机路径。
fn source_marker_medium(source_window: isize) -> WinResult<STGMEDIUM> {
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, size_of::<isize>())? };
    let pointer = unsafe { GlobalLock(handle) };
    if pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(E_FAIL.into());
    }
    unsafe {
        std::ptr::write_unaligned(pointer.cast::<isize>(), source_window);
        let _ = GlobalUnlock(handle);
    }
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: handle },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

/// 只在匹配同一源窗口时允许 GPUI 恢复内部载荷。
pub(crate) fn is_source_window(data: &IDataObject, hwnd: windows::Win32::Foundation::HWND) -> bool {
    let Ok(formats) = virtual_formats() else {
        return false;
    };
    let format = format_for(formats.source, TYMED_HGLOBAL.0 as u32, -1);
    let Ok(mut medium) = (unsafe { data.GetData(&format) }) else {
        return false;
    };
    let same = unsafe {
        if medium.tymed != TYMED_HGLOBAL.0 as u32 {
            windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
            return false;
        }
        let handle = medium.u.hGlobal;
        if GlobalSize(handle) < size_of::<isize>() {
            windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
            return false;
        }
        let pointer = GlobalLock(handle);
        let same = !pointer.is_null()
            && std::ptr::read_unaligned(pointer.cast::<isize>()) == hwnd.0 as isize;
        if !pointer.is_null() {
            let _ = GlobalUnlock(handle);
        }
        windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
        same
    };
    same
}

/// 源窗口已接管内部复制后通知同一数据对象，避免误判为系统目标未知复制。
pub(crate) fn report_internal_drop(data: &IDataObject) {
    let Ok(formats) = virtual_formats() else {
        return;
    };
    let format = format_for(formats.source, TYMED_HGLOBAL.0 as u32, -1);
    let medium = STGMEDIUM::default();
    let _ = unsafe { data.SetData(&format, &medium, false) };
}

/// 为每次请求生成可移动 UTF-16 内存块，成功返回后才转移给 Windows。
fn file_list_medium(paths: &[PathBuf]) -> WinResult<STGMEDIUM> {
    let mut names = Vec::<u16>::new();
    for path in paths {
        let name = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if name.is_empty() || name.contains(&0) {
            return Err(E_FAIL.into());
        }
        names.extend(name);
        names.push(0);
    }
    names.push(0);
    let byte_len = names
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|len| len.checked_add(size_of::<DROPFILES>()))
        .ok_or(E_FAIL)?;
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len)? };
    let target = unsafe { GlobalLock(handle) };
    if target.is_null() {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(E_FAIL.into());
    }
    let header = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        fWide: BOOL(1),
        ..Default::default()
    };
    unsafe {
        std::ptr::write_unaligned(target.cast::<DROPFILES>(), header);
        std::ptr::copy_nonoverlapping(
            names.as_ptr().cast::<u8>(),
            target.cast::<u8>().add(size_of::<DROPFILES>()),
            names.len() * size_of::<u16>(),
        );
        let _ = GlobalUnlock(handle);
    }
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: handle },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

#[allow(non_snake_case)]
impl IDataObject_Impl for FileDataObject_Impl {
    /// 每次请求分配独立介质，因为目标负责释放返回的内存块。
    fn GetData(&self, format: *const FORMATETC) -> WinResult<STGMEDIUM> {
        if self.QueryGetData(format) != S_OK {
            return Err(DV_E_FORMATETC.into());
        }
        let format = unsafe { format.as_ref() }.ok_or(DV_E_FORMATETC)?;
        match &self.source {
            DragDataSource::Local(paths) => file_list_medium(paths),
            DragDataSource::Virtual { .. } => {
                let formats = virtual_formats()?;
                if format.cfFormat == formats.source {
                    drag_diagnostic("get_source_marker");
                    return source_marker_medium(self.source_window);
                }
                if format.cfFormat == formats.descriptor {
                    drag_diagnostic("get_descriptor");
                    return descriptor_medium(&self.descriptors()?);
                }
                if format.lindex < 0 {
                    return Err(DV_E_LINDEX.into());
                }
                let descriptors = self.descriptors()?;
                drag_diagnostic(format_args!("get_contents index={}", format.lindex));
                let entry = descriptors.get(format.lindex as usize).ok_or(DV_E_LINDEX)?;
                if entry.is_directory {
                    return Err(DV_E_LINDEX.into());
                }
                let prepared = self.prepared()?;
                let (root, _) = prepared
                    .paths
                    .entries()
                    .get(entry.root_index)
                    .ok_or(DV_E_LINDEX)?;
                let mut components = entry.relative_path.split('/');
                let root_name = components.next().ok_or(DV_E_LINDEX)?;
                if root.file_name().and_then(|name| name.to_str()) != Some(root_name) {
                    return Err(E_FAIL.into());
                }
                let mut path = root.clone();
                for component in components {
                    if component.is_empty()
                        || component == "."
                        || component == ".."
                        || component.contains('\\')
                    {
                        return Err(E_FAIL.into());
                    }
                    path.push(component);
                }
                let stream = stream::open_stream(
                    &path,
                    prepared.owner.clone(),
                    self.streams.clone(),
                    self.activity.clone(),
                )?;
                Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(stream)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                })
            }
        }
    }
    /// 只支持独立分配的介质，不写入目标提供的缓冲区。
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    /// Shell 在放置前会探测格式，此路径不能分配内存或读取磁盘。
    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return DV_E_FORMATETC;
        };
        if matches!(&self.source, DragDataSource::Virtual { .. })
            && let Ok(ids) = virtual_formats()
        {
            if format.cfFormat == ids.descriptor {
                drag_diagnostic(format_args!("query_descriptor tymed={:#x}", format.tymed));
            } else if format.cfFormat == ids.contents {
                drag_diagnostic(format_args!("query_contents tymed={:#x}", format.tymed));
            }
        }
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_FORMATETC;
        }
        let accepted = match &self.source {
            DragDataSource::Local(_) => {
                format.cfFormat == CF_HDROP.0
                    && format.lindex == -1
                    && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
            }
            DragDataSource::Virtual { .. } => virtual_formats().ok().is_some_and(|ids| {
                ((format.cfFormat == ids.descriptor || format.cfFormat == ids.source)
                    && format.lindex == -1
                    && format.tymed & TYMED_HGLOBAL.0 as u32 != 0)
                    || (format.cfFormat == ids.contents
                        && format.lindex >= -1
                        && format.tymed & TYMED_ISTREAM.0 as u32 != 0)
            }),
        };
        if accepted { S_OK } else { DV_E_FORMATETC }
    }
    /// 不提供等价格式，避免暗示其它表示也可用。
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    /// 未接管目标介质时明确拒绝，避免错误声称已处理或释放内存。
    fn SetData(
        &self,
        format: *const FORMATETC,
        _: *const STGMEDIUM,
        release: BOOL,
    ) -> WinResult<()> {
        let marker = unsafe { format.as_ref() }
            .and_then(|format| {
                virtual_formats()
                    .ok()
                    .map(|ids| format.cfFormat == ids.source)
            })
            .unwrap_or(false);
        if !marker || release.as_bool() {
            return Err(E_NOTIMPL.into());
        }
        self.copy
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .internal_drop = true;
        if let DragDataSource::Virtual { provider, .. } = &self.source {
            provider.cancel();
        }
        Ok(())
    }
    /// 复用系统格式枚举器，避免自行维护 COM 枚举器生命周期。
    fn EnumFormatEtc(&self, direction: u32) -> WinResult<IEnumFORMATETC> {
        drag_diagnostic(format_args!("enum_formats direction={direction}"));
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        match &self.source {
            DragDataSource::Local(_) => unsafe { SHCreateStdEnumFmtEtc(&[file_format()]) },
            DragDataSource::Virtual { .. } => {
                let ids = virtual_formats()?;
                unsafe {
                    SHCreateStdEnumFmtEtc(&[
                        format_for(ids.descriptor, TYMED_HGLOBAL.0 as u32, -1),
                        format_for(ids.contents, TYMED_ISTREAM.0 as u32, 0),
                        format_for(ids.source, TYMED_HGLOBAL.0 as u32, -1),
                    ])
                }
            }
        }
    }
    /// 路径列表不可变，不注册 OLE 更新通知。
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    /// 不存在更新通知订阅，因此不能取消订阅。
    fn DUnadvise(&self, _: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    /// 不存在更新通知订阅，因此不返回枚举器。
    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[allow(non_snake_case)]
impl IDataObjectAsyncCapability_Impl for FileDataObject_Impl {
    /// 异步模式持有自身引用，DoDragDrop 返回后也不能提前销毁。
    fn SetAsyncMode(&self, enabled: BOOL) -> WinResult<()> {
        drag_diagnostic(format_args!("async_mode enabled={}", enabled.as_bool()));
        let released = {
            let mut copy = self
                .copy
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?;
            if copy.in_operation && !enabled.as_bool() {
                return Err(E_FAIL.into());
            }
            copy.enabled = enabled.as_bool();
            if copy.enabled {
                if copy.owner.is_none() {
                    copy.owner = Some(self.to_object());
                }
                None
            } else {
                copy.owner.take()
            }
        };
        drop(released);
        Ok(())
    }
    /// 返回实际协商模式，不推测目标是否采用异步复制。
    fn GetAsyncMode(&self) -> WinResult<BOOL> {
        Ok(BOOL::from(
            self.copy
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?
                .enabled,
        ))
    }
    /// 从目标开始异步读取到 EndOperation 期间保持 COM 与暂存 owner 存活。
    fn StartOperation(&self, _: Ref<IBindCtx>) -> WinResult<()> {
        drag_diagnostic("async_start");
        let mut copy = self
            .copy
            .lock()
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        if !copy.enabled || copy.in_operation {
            return Err(E_FAIL.into());
        }
        if copy.owner.is_none() {
            copy.owner = Some(self.to_object());
        }
        copy.in_operation = true;
        Ok(())
    }
    /// 只返回目标明确登记的运行状态。
    fn InOperation(&self) -> WinResult<BOOL> {
        Ok(BOOL::from(
            self.copy
                .lock()
                .map_err(|_| windows::core::Error::from(E_FAIL))?
                .in_operation,
        ))
    }
    /// 在释放暂存 owner 之前记录目标完成回执。
    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, effects: u32) -> WinResult<()> {
        drag_diagnostic(format_args!(
            "async_end result={:#x} effects={effects:#x}",
            result.0 as u32
        ));
        let released = {
            let mut copy = self.copy.lock().unwrap_or_else(|error| error.into_inner());
            if !copy.in_operation {
                return Ok(());
            }
            copy.result = Some((result, effects));
            copy.in_operation = false;
            copy.owner.take()
        };
        drop(released);
        if result.is_err() || effects & DROPEFFECT_COPY.0 == 0 {
            if let DragDataSource::Virtual { provider, .. } = &self.source {
                provider.cancel();
            }
        }
        Ok(())
    }
}

#[implement(IDropSource)]
struct FileDropSource {
    provider: Option<Arc<dyn DeferredFileDragProvider>>,
    keyboard: Arc<DragKeyboardState>,
    saw_button_down: AtomicBool,
    first_query: AtomicBool,
}

#[allow(non_snake_case)]
impl IDropSource_Impl for FileDropSource_Impl {
    /// Escape 取消；只有原左键释放才能提交当前拖动。
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        let keys_down = keys.0 & MK_LBUTTON.0 != 0;
        let physical_down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 };
        if self.first_query.swap(false, Ordering::AcqRel) {
            drag_diagnostic(format_args!(
                "continue_first keys_down={keys_down} physical_down={physical_down}"
            ));
        }
        if keys_down {
            self.saw_button_down.store(true, Ordering::Release);
        }
        if escape.as_bool() || self.keyboard.cancellation_requested() {
            drag_diagnostic("continue_escape");
            self.keyboard.observe_ole_escape();
            if let Some(provider) = &self.provider {
                provider.cancel();
            }
            DRAGDROP_S_CANCEL
        } else if !keys_down && (self.saw_button_down.load(Ordering::Acquire) || !physical_down) {
            drag_diagnostic("continue_drop");
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    /// 使用 Windows 原生复制和不可放置指针，不自造平台状态。
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// 检查物理按键状态，防止延迟准备完成后启动已经释放的拖动。
fn gesture_is_down() -> bool {
    unsafe {
        GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 && GetAsyncKeyState(VK_ESCAPE.0 as i32) >= 0
    }
}

/// 用户在不可放置区域松手时没有复制目标；存在复制回执或内部受理则不能降级为取消。
fn unaccepted_drop_is_cancelled(
    result: HRESULT,
    effect: DROPEFFECT,
    copy_result: Option<(HRESULT, u32)>,
    internal_drop: bool,
) -> bool {
    result == DRAGDROP_S_DROP
        && effect == DROPEFFECT_NONE
        && copy_result.is_none()
        && !internal_drop
}

/// OLE 使用独立 STA；只有目标回报完成后才能释放暂存材料。
pub(crate) fn run_drag(
    payload: ExternalDragPayload,
    source_window: isize,
    keyboard: Arc<DragKeyboardState>,
    ready: impl FnOnce() -> bool,
    ended: impl FnOnce(),
) -> ExternalFileDragOutcome {
    let _keyboard_lifetime = DragKeyboardLifetime(keyboard.clone());
    drag_diagnostic("worker_begin");
    let local_paths = match &payload {
        ExternalDragPayload::Files(paths) => Some(paths.clone()),
        _ => None,
    };
    let provider = match &payload {
        ExternalDragPayload::DeferredFiles(value) => Some(value.0.clone()),
        _ => None,
    };
    let result = (|| {
        if !gesture_is_down() || !ready() || !gesture_is_down() {
            drag_diagnostic("gesture_not_ready");
            if let Some(provider) = &provider {
                provider.cancel();
            }
            return Err(ExternalFileDragOutcome::Cancelled);
        }
        unsafe { OleInitialize(None) }
            .map_err(|_| ExternalFileDragOutcome::Failed("无法初始化系统拖放".into()))?;
        struct OleGuard;
        impl Drop for OleGuard {
            fn drop(&mut self) {
                unsafe { OleUninitialize() }
            }
        }
        let _ole = OleGuard;
        let object = ComObject::new(
            FileDataObject::new(&payload, source_window, keyboard.clone())
                .map_err(|_| ExternalFileDragOutcome::Failed("拖出路径无效".into()))?,
        );
        let data: IDataObject = object.to_interface();
        let capability: IDataObjectAsyncCapability = object.to_interface();
        let input = DragInputAttachment::attach(source_window)
            .map_err(|_| ExternalFileDragOutcome::Failed("无法关联系统拖放输入".into()))?;
        if !gesture_is_down() {
            if let Some(provider) = &provider {
                provider.cancel();
            }
            return Err(ExternalFileDragOutcome::Cancelled);
        }
        unsafe { capability.SetAsyncMode(true) }
            .map_err(|_| ExternalFileDragOutcome::Failed("无法启动系统拖放".into()))?;
        let source: IDropSource = FileDropSource {
            provider: provider.clone(),
            keyboard: keyboard.clone(),
            saw_button_down: AtomicBool::new(false),
            first_query: AtomicBool::new(true),
        }
        .into();
        let mut effect = DROPEFFECT_NONE;
        // 清除历史按键边沿，等待期间只处理本次拖动的 Escape。
        let _ = unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) };
        drag_diagnostic("do_drag_drop_begin");
        let result = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
        keyboard.finish();
        drop(input);
        drag_diagnostic(format_args!(
            "do_drag_drop_end result={:#x} effect={:#x}",
            result.0 as u32, effect.0
        ));
        ended();
        if result == DRAGDROP_S_CANCEL {
            if let Some(provider) = &provider {
                provider.cancel();
            }
        }
        let mut deadline = Instant::now() + Duration::from_secs(60 * 60);
        let mut cancellation_deadline = None;
        let mut last_activity = object.get().activity.load(Ordering::Acquire);
        loop {
            let in_operation = match unsafe { capability.InOperation() } {
                Ok(value) => value.as_bool(),
                Err(_) => {
                    object.get().abandon_async();
                    return Err(ExternalFileDragOutcome::OutcomeUnknown);
                }
            };
            let pending = object.get().pending_work();
            let streams = object.get().streams.load(Ordering::Acquire);
            if !in_operation && !pending && streams == 0 {
                break;
            }
            let activity = object.get().activity.load(Ordering::Acquire);
            if provider
                .as_ref()
                .is_some_and(|provider| provider.is_cancelled())
            {
                let limit = cancellation_deadline
                    .get_or_insert_with(|| Instant::now() + CANCEL_SETTLE_TIMEOUT);
                deadline = deadline.min(*limit);
            }
            if activity != last_activity
                || (pending
                    && provider
                        .as_ref()
                        .is_some_and(|provider| !provider.is_cancelled()))
            {
                last_activity = activity;
                if cancellation_deadline.is_none() {
                    deadline = Instant::now() + Duration::from_secs(60 * 60);
                }
            }
            if Instant::now() >= deadline {
                if let Some(provider) = &provider {
                    provider.cancel();
                }
                object.get().abandon_async();
                return Err(ExternalFileDragOutcome::OutcomeUnknown);
            }
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let (copy_result, internal_drop) = {
            let copy = object
                .get()
                .copy
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (copy.result, copy.internal_drop)
        };
        let _ = unsafe { capability.SetAsyncMode(false) };
        if internal_drop {
            return Ok(ExternalFileDragOutcome::Completed);
        }
        if result == DRAGDROP_S_CANCEL {
            return Err(ExternalFileDragOutcome::Cancelled);
        }
        if let Some(error) = object.get().work_failure() {
            return Err(ExternalFileDragOutcome::Failed(error));
        }
        if unaccepted_drop_is_cancelled(result, effect, copy_result, internal_drop) {
            return Err(ExternalFileDragOutcome::Cancelled);
        }
        if result != DRAGDROP_S_DROP || effect != DROPEFFECT_COPY {
            return Err(ExternalFileDragOutcome::Failed("系统拖放未完成".into()));
        }
        match copy_result {
            Some((result, copy_effect))
                if result.is_ok() && copy_effect & DROPEFFECT_COPY.0 != 0 =>
            {
                Ok(ExternalFileDragOutcome::Completed)
            }
            Some(_) => Err(ExternalFileDragOutcome::Failed(
                "目标未完成复制，请重试".into(),
            )),
            None if matches!(&object.get().source, DragDataSource::Local(_)) => {
                Ok(ExternalFileDragOutcome::Completed)
            }
            None => {
                object.get().preserve_if_unconfirmed();
                Err(ExternalFileDragOutcome::OutcomeUnknown)
            }
        }
    })();
    keyboard.finish();
    let outcome = result.unwrap_or_else(|outcome| outcome);
    drag_diagnostic(match &outcome {
        ExternalFileDragOutcome::Cancelled => "outcome_cancelled",
        ExternalFileDragOutcome::Completed => "outcome_completed",
        ExternalFileDragOutcome::OutcomeUnknown => "outcome_unknown",
        ExternalFileDragOutcome::Failed(_) => "outcome_failed",
    });
    if let Some(provider) = provider {
        provider.finish(outcome.clone());
    }
    if let Some(paths) = local_paths {
        paths.finish(outcome.clone());
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ExternalFileDragOwner, FileDragPaths};
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::System::Ole::ReleaseStgMedium;

    struct TestOwner(Arc<AtomicBool>);

    impl ExternalFileDragOwner for TestOwner {
        /// 用可观察标志验证未知状态先保留暂存，再释放 COM 资源。
        fn preserve(&self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct TestProvider(Arc<TestOwner>);
    impl DeferredFileDragProvider for TestProvider {
        /// 测试仅关注 owner，不触发真实文件清单。
        fn describe(&self) -> Result<Vec<ExternalFileDescriptor>, String> {
            Ok(Vec::new())
        }
        /// 返回稳定本机样本，模拟准备结果持有暂存。
        fn prepare(&self) -> Result<PreparedExternalFileDrag, String> {
            Ok(PreparedExternalFileDrag::new(
                FileDragPaths::new([(PathBuf::from(r"C:\stage\file.txt"), false)]),
                self.0.clone(),
            ))
        }
        /// 样本没有后台网络任务。
        fn cancel(&self) {}
        /// 样本未触发真实手势取消。
        fn is_cancelled(&self) -> bool {
            false
        }
        /// 样本不投递界面状态。
        fn finish(&self, _: ExternalFileDragOutcome) {}
    }

    /// 已失联的异步目标不能留下 COM 自引用环，暂存仍必须可恢复。
    #[test]
    fn abandoning_async_copy_preserves_files_and_releases_self_reference() {
        let preserved = Arc::new(AtomicBool::new(false));
        let owner = Arc::new(TestOwner(preserved.clone()));
        let weak_owner = Arc::downgrade(&owner);
        let provider = Arc::new(TestProvider(owner.clone()));
        let payload = ExternalDragPayload::DeferredFiles(gpui::DeferredFileDrag(provider.clone()));
        let object = ComObject::new(
            FileDataObject::new(&payload, 1, Arc::new(DragKeyboardState::default()))
                .expect("virtual source"),
        );
        drop(payload);
        let work = DeferredWork::start(move || provider.prepare()).expect("test preparation");
        while work.completed().is_none() {
            std::thread::yield_now();
        }
        if let DragDataSource::Virtual { prepared, .. } = &object.get().source {
            *prepared.lock().expect("prepared state") = Some(work);
        }
        drop(owner);
        let capability: IDataObjectAsyncCapability = object.to_interface();
        unsafe {
            capability.SetAsyncMode(true).expect("enable async mode");
            capability.StartOperation(None).expect("start operation");
        }
        object.get().abandon_async();
        assert!(preserved.load(Ordering::Acquire));
        {
            let state = object.get().copy.lock().expect("copy state");
            assert!(!state.enabled && !state.in_operation && state.owner.is_none());
        }
        drop(capability);
        drop(object);
        assert!(
            weak_owner.upgrade().is_none(),
            "COM self reference must be released"
        );
    }

    /// 系统取得独立可释放的 UTF-16 路径列表，多个名称以双 NUL 结束。
    #[test]
    fn native_file_list_contains_complete_unicode_paths() {
        let paths = [
            PathBuf::from(r"C:\stage\一.txt"),
            PathBuf::from(r"C:\stage\目录"),
        ];
        let mut medium = file_list_medium(&paths).expect("file drop medium");
        assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);
        unsafe {
            let handle = medium.u.hGlobal;
            let pointer = GlobalLock(handle);
            assert!(!pointer.is_null());
            let header = std::ptr::read_unaligned(pointer.cast::<DROPFILES>());
            let offset = header.pFiles;
            let wide = header.fWide;
            assert_eq!(offset, size_of::<DROPFILES>() as u32);
            assert!(wide.as_bool());
            let expected = paths
                .iter()
                .flat_map(|path| path.as_os_str().encode_wide().chain(std::iter::once(0)))
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let actual = std::slice::from_raw_parts(
                pointer.cast::<u8>().add(offset as usize).cast::<u16>(),
                expected.len(),
            );
            assert_eq!(actual, expected);
            let _ = GlobalUnlock(handle);
            ReleaseStgMedium(&mut medium);
        }
    }
}
