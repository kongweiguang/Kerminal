//! Windows 延迟文件流的真实 COM 调用与暂存生命周期回归。
//!
//! @author kongweiguang

use std::{
    ffi::c_void,
    fs::OpenOptions,
    io::Write as _,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    DeferredFileDragProvider, ExternalFileDescriptor, ExternalFileDragOutcome,
    ExternalFileDragOwner, FileDragPaths, PreparedExternalFileDrag,
};
use windows::{
    Win32::{
        Foundation::{
            DRAGDROP_S_DROP, E_FAIL, E_NOTIMPL, S_FALSE, S_OK, STG_E_ACCESSDENIED,
            STG_E_INVALIDFUNCTION, STG_E_INVALIDPOINTER, STG_E_MEDIUMFULL, STG_E_WRITEFAULT,
        },
        System::Com::{
            ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_NONAME,
            STATSTG, STGC, STGM_READ, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END,
            STREAM_SEEK_SET,
        },
        System::Ole::{DROPEFFECT_COPY, DROPEFFECT_NONE, OleInitialize, OleUninitialize},
    },
    core::{HRESULT, Ref, Result as WinResult, implement},
};

use super::super::{
    DeferredWork, DragKeyboardState, cancel_on_drag_escape, unaccepted_drop_is_cancelled,
};
use super::open_stream;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    /// 独占创建本次测试文件，防止清理时触碰同名的既有用户文件。
    ///
    /// @author kongweiguang
    fn new(contents: &[u8]) -> Self {
        for _ in 0..128 {
            let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kerminal-ole-stream-test-{}-{id}",
                std::process::id(),
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(contents)
                        .expect("write isolated stream fixture");
                    return Self(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated stream fixture: {error}"),
            }
        }
        panic!("could not allocate an isolated stream fixture");
    }
}

impl Drop for Fixture {
    /// 只删除由本实例独占创建的测试文件。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Owner(Arc<AtomicBool>);

impl ExternalFileDragOwner for Owner {
    /// 此替身仅观察所有权，测试不触发持久化或目录清理。
    ///
    /// @author kongweiguang
    fn preserve(&self) {}
}

impl Drop for Owner {
    /// 用可观察信号证明所有 COM 引用结束前暂存 owner 不会释放。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// 真实 COM 目标可以短写或在写入部分数据后失败；该替身记录每个已接收字节。
#[implement(IStream)]
struct PartialSink {
    contents: Arc<Mutex<Vec<u8>>>,
    max_chunk: usize,
    fail_after: Option<usize>,
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for PartialSink_Impl {
    /// 接收端无需支持读取，避免测试意外依赖无关的目标行为。
    ///
    /// @author kongweiguang
    fn Read(&self, _: *mut c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }

    /// 可在返回失败前确认部分字节，以验证 CopyTo 的错误统计。
    ///
    /// @author kongweiguang
    fn Write(&self, data: *const c_void, requested: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe { written.write(0) };
        }
        if requested != 0 && data.is_null() {
            return STG_E_INVALIDPOINTER;
        }
        let Ok(mut contents) = self.contents.lock() else {
            return STG_E_WRITEFAULT;
        };
        let available = self
            .fail_after
            .map(|limit| limit.saturating_sub(contents.len()))
            .unwrap_or(usize::MAX);
        let count = (requested as usize).min(self.max_chunk).min(available);
        if count > 0 {
            // SAFETY: the caller provides requested readable bytes for this synchronous call.
            contents
                .extend_from_slice(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), count) });
        }
        if !written.is_null() {
            unsafe { written.write(count as u32) };
        }
        if self.fail_after.is_some_and(|limit| contents.len() >= limit) {
            STG_E_MEDIUMFULL
        } else {
            S_OK
        }
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for PartialSink_Impl {
    /// 目标替身只实现写入，未使用的 COM 操作明确拒绝。
    ///
    /// @author kongweiguang
    fn Seek(&self, _: i64, _: STREAM_SEEK, _: *mut u64) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 写入测试不改变接收端文件长度。
    ///
    /// @author kongweiguang
    fn SetSize(&self, _: u64) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 防止测试把源与目标角色反转。
    ///
    /// @author kongweiguang
    fn CopyTo(&self, _: Ref<'_, IStream>, _: u64, _: *mut u64, _: *mut u64) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 接收端不维护独立事务。
    ///
    /// @author kongweiguang
    fn Commit(&self, _: &STGC) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 接收端不维护回滚草稿。
    ///
    /// @author kongweiguang
    fn Revert(&self) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 接收端不提供区域锁。
    ///
    /// @author kongweiguang
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 未提供的区域锁不能被伪装成成功释放。
    ///
    /// @author kongweiguang
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 测试通过共享记录读取目标字节，无需目标元数据。
    ///
    /// @author kongweiguang
    fn Stat(&self, _: *mut STATSTG, _: &STATFLAG) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 接收端没有独立游标，不参与源流克隆测试。
    ///
    /// @author kongweiguang
    fn Clone(&self) -> WinResult<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// 文件长度、EOF、零长度请求和只读限制通过 COM vtable 而非内部方法验证。
///
/// @author kongweiguang
#[test]
fn stream_reports_length_eof_and_readonly_operations() {
    for contents in [&b"hello"[..], &b""[..]] {
        let fixture = Fixture::new(contents);
        let owner: Arc<dyn ExternalFileDragOwner> =
            Arc::new(Owner(Arc::new(AtomicBool::new(false))));
        let counter = Arc::new(AtomicUsize::new(0));
        let stream = open_stream(
            &fixture.0,
            owner,
            counter.clone(),
            Arc::new(AtomicU64::new(0)),
        )
        .expect("open fixture stream");
        let mut stat = STATSTG::default();
        unsafe {
            stream
                .Stat(&mut stat, STATFLAG_NONAME)
                .expect("stream stat")
        };
        assert_eq!(stat.r#type, STGTY_STREAM.0 as u32);
        assert_eq!(stat.grfMode, STGM_READ);
        assert_eq!(stat.cbSize, contents.len() as u64);
        assert!(stat.pwcsName.is_null());

        let mut buffer = [0_u8; 8];
        let mut read = u32::MAX;
        let status = unsafe { stream.Read(buffer.as_mut_ptr().cast(), 8, Some(&mut read)) };
        assert_eq!(status, S_FALSE);
        assert_eq!(read as usize, contents.len());
        assert_eq!(&buffer[..read as usize], contents);
        assert_eq!(
            unsafe { stream.Read(buffer.as_mut_ptr().cast(), 1, Some(&mut read)) },
            S_FALSE
        );
        assert_eq!(read, 0);
        assert_eq!(unsafe { stream.Read(std::ptr::null_mut(), 0, None) }, S_OK);
        assert_eq!(
            unsafe { stream.Read(std::ptr::null_mut(), 1, None) },
            STG_E_INVALIDPOINTER
        );

        let mut written = u32::MAX;
        assert_eq!(
            unsafe { stream.Write(buffer.as_ptr().cast(), 1, Some(&mut written)) },
            STG_E_ACCESSDENIED
        );
        assert_eq!(written, 0);
        assert_eq!(
            unsafe { stream.SetSize(0) }
                .expect_err("readonly stream")
                .code(),
            STG_E_ACCESSDENIED
        );
        drop(stream);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }
}

/// 三种定位方式使用逻辑游标，负位置和溢出不能改变原位置。
///
/// @author kongweiguang
#[test]
fn stream_seek_checks_origins_negative_offsets_and_overflow() {
    let fixture = Fixture::new(b"abcdef");
    let owner: Arc<dyn ExternalFileDragOwner> = Arc::new(Owner(Arc::new(AtomicBool::new(false))));
    let stream = open_stream(
        &fixture.0,
        owner,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicU64::new(0)),
    )
    .expect("open stream");
    let mut position = 0_u64;
    unsafe {
        stream
            .Seek(2, STREAM_SEEK_SET, Some(&mut position))
            .expect("seek from start");
        assert_eq!(position, 2);
        stream
            .Seek(2, STREAM_SEEK_CUR, Some(&mut position))
            .expect("seek from current");
        assert_eq!(position, 4);
        stream
            .Seek(-1, STREAM_SEEK_END, Some(&mut position))
            .expect("seek from end");
        assert_eq!(position, 5);
        assert_eq!(
            stream
                .Seek(-7, STREAM_SEEK_END, None)
                .expect_err("negative position")
                .code(),
            STG_E_INVALIDFUNCTION
        );
        stream
            .Seek(0, STREAM_SEEK_CUR, Some(&mut position))
            .expect("position unchanged");
        assert_eq!(position, 5);
        assert_eq!(
            stream
                .Seek(0, STREAM_SEEK(99), None)
                .expect_err("invalid origin")
                .code(),
            STG_E_INVALIDFUNCTION
        );
        stream
            .Seek(i64::MAX, STREAM_SEEK_SET, None)
            .expect("maximum offset");
        assert!(stream.Seek(1, STREAM_SEEK_CUR, None).is_err());
        stream
            .Seek(0, STREAM_SEEK_CUR, Some(&mut position))
            .expect("position unchanged");
        assert_eq!(position, i64::MAX as u64);
    }
}

/// COM 克隆保持创建时游标，引用计数及暂存 owner 一直到最后一个接口释放。
///
/// @author kongweiguang
#[test]
fn clone_has_independent_cursor_and_retains_owner_until_final_release() {
    let fixture = Fixture::new(b"abcdef");
    let dropped = Arc::new(AtomicBool::new(false));
    let owner: Arc<dyn ExternalFileDragOwner> = Arc::new(Owner(dropped.clone()));
    let counter = Arc::new(AtomicUsize::new(0));
    let activity = Arc::new(AtomicU64::new(0));
    let stream = open_stream(&fixture.0, owner.clone(), counter.clone(), activity.clone())
        .expect("open stream");
    drop(owner);
    let mut first = [0_u8; 2];
    unsafe { assert_eq!(stream.Read(first.as_mut_ptr().cast(), 2, None), S_OK) };
    assert_eq!(&first, b"ab");
    let clone = unsafe { stream.Clone().expect("clone stream") };
    assert_eq!(counter.load(Ordering::Acquire), 2);
    let mut left = [0_u8; 2];
    let mut right = [0_u8; 2];
    unsafe {
        assert_eq!(stream.Read(left.as_mut_ptr().cast(), 2, None), S_OK);
        assert_eq!(clone.Read(right.as_mut_ptr().cast(), 2, None), S_OK);
    }
    assert_eq!(&left, b"cd");
    assert_eq!(&right, b"cd");
    assert_eq!(activity.load(Ordering::Relaxed), 6);
    let other_reference = stream.clone();
    drop(stream);
    assert_eq!(counter.load(Ordering::Acquire), 2);
    drop(clone);
    assert_eq!(counter.load(Ordering::Acquire), 1);
    assert!(!dropped.load(Ordering::Acquire));
    drop(other_reference);
    assert_eq!(counter.load(Ordering::Acquire), 0);
    assert!(dropped.load(Ordering::Acquire));
}

/// 部分写入必须被续写，写失败时 pcbRead 和 pcbWritten 分别保留已经发生的事实。
///
/// @author kongweiguang
#[test]
fn copy_to_handles_short_writes_and_reports_partial_failure() {
    for fail_after in [None, Some(3)] {
        let fixture = Fixture::new(b"hello");
        let owner: Arc<dyn ExternalFileDragOwner> =
            Arc::new(Owner(Arc::new(AtomicBool::new(false))));
        let activity = Arc::new(AtomicU64::new(0));
        let stream = open_stream(
            &fixture.0,
            owner,
            Arc::new(AtomicUsize::new(0)),
            activity.clone(),
        )
        .expect("open stream");
        let received = Arc::new(Mutex::new(Vec::new()));
        let target: IStream = PartialSink {
            contents: received.clone(),
            max_chunk: 2,
            fail_after,
        }
        .into();
        let mut read = u64::MAX;
        let mut written = u64::MAX;
        let result = unsafe { stream.CopyTo(&target, 10, Some(&mut read), Some(&mut written)) };
        if fail_after.is_some() {
            assert_eq!(
                result.expect_err("target reports failure").code(),
                STG_E_MEDIUMFULL
            );
            assert_eq!(read, 5);
            assert_eq!(written, 3);
            assert_eq!(activity.load(Ordering::Relaxed), 8);
            assert_eq!(&*received.lock().expect("sink lock"), b"hel");
        } else {
            result.expect("copy succeeds at EOF");
            assert_eq!(read, 5);
            assert_eq!(written, 5);
            assert_eq!(activity.load(Ordering::Relaxed), 10);
            assert_eq!(&*received.lock().expect("sink lock"), b"hello");
        }
    }
}

/// 目标多次短写时仍完整复制超过单个缓冲区的数据，并持续更新活动信号。
///
/// @author kongweiguang
#[test]
fn copy_to_streams_multiple_chunks_without_loading_the_entire_file() {
    let contents = (0..(2 * 64 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let fixture = Fixture::new(&contents);
    let owner: Arc<dyn ExternalFileDragOwner> = Arc::new(Owner(Arc::new(AtomicBool::new(false))));
    let activity = Arc::new(AtomicU64::new(0));
    let stream = open_stream(
        &fixture.0,
        owner,
        Arc::new(AtomicUsize::new(0)),
        activity.clone(),
    )
    .expect("open stream");
    let received = Arc::new(Mutex::new(Vec::new()));
    let target: IStream = PartialSink {
        contents: received.clone(),
        max_chunk: 4096,
        fail_after: None,
    }
    .into();
    let mut read = 0_u64;
    let mut written = 0_u64;
    unsafe {
        stream
            .CopyTo(
                &target,
                contents.len() as u64,
                Some(&mut read),
                Some(&mut written),
            )
            .expect("copy all chunks");
    }
    assert_eq!(read, contents.len() as u64);
    assert_eq!(written, contents.len() as u64);
    assert_eq!(*received.lock().expect("sink lock"), contents);
    assert_eq!(activity.load(Ordering::Relaxed), 2 * read);
}

/// 打开失败时不得持有任何活动流计数，调用方可以继续安全回收暂存。
///
/// @author kongweiguang
#[test]
fn failed_open_does_not_increment_stream_counter() {
    let fixture = Fixture::new(b"data");
    let owner: Arc<dyn ExternalFileDragOwner> = Arc::new(Owner(Arc::new(AtomicBool::new(false))));
    let counter = Arc::new(AtomicUsize::new(0));
    assert!(
        open_stream(
            &fixture.0.with_extension("missing"),
            owner,
            counter.clone(),
            Arc::new(AtomicU64::new(0)),
        )
        .is_err()
    );
    assert_eq!(counter.load(Ordering::Acquire), 0);
}

struct WaitProvider(AtomicBool);

impl DeferredFileDragProvider for WaitProvider {
    /// 等待测试不请求文件清单。
    fn describe(&self) -> Result<Vec<ExternalFileDescriptor>, String> {
        Err("unused".into())
    }
    /// 等待测试不准备正文。
    fn prepare(&self) -> Result<PreparedExternalFileDrag, String> {
        Err("unused".into())
    }
    /// 保留可观察的取消状态，验证手势阶段门禁。
    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// 验证目标失联时等待方不会无限阻塞，且放置后不受按键影响。
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    /// 不向界面投递回执。
    fn finish(&self, _: ExternalFileDragOutcome) {}
}

/// 被取消的后台工作即使尚未返回，也不能永久阻塞 COM 等待线程。
///
/// @author kongweiguang
#[test]
fn cancelled_com_wait_has_a_bounded_grace_period() {
    unsafe { OleInitialize(None).expect("initialize test STA") };
    let (release, waiting) = std::sync::mpsc::channel::<()>();
    let work = DeferredWork::start(move || {
        let _ = waiting.recv();
        Ok(())
    })
    .expect("start isolated work");
    let started = Instant::now();
    let result = work.wait_with_timeout(
        &WaitProvider(AtomicBool::new(true)),
        &DragKeyboardState::default(),
        Duration::from_millis(25),
    );
    let elapsed = started.elapsed();
    let _ = release.send(());
    unsafe { OleUninitialize() };
    assert!(result.is_err());
    assert!(elapsed >= Duration::from_millis(25));
    assert!(elapsed < Duration::from_secs(5));
}

/// 阻塞清单读取时允许 Escape 取消，系统放置结束后不读取或处理后续按键。
///
/// @author kongweiguang
#[test]
fn escape_cancels_only_during_the_native_drag() {
    let provider = WaitProvider(AtomicBool::new(false));
    let active = DragKeyboardState::default();
    cancel_on_drag_escape(&provider, &active, || {
        panic!("inactive drag must not poll keys")
    });
    assert!(!provider.is_cancelled());
    active.begin();
    cancel_on_drag_escape(&provider, &active, || true);
    assert!(provider.is_cancelled());
    provider.0.store(false, Ordering::Release);
    active.finish();
    cancel_on_drag_escape(&provider, &active, || {
        panic!("completed drag must not poll keys")
    });
    assert!(!provider.is_cancelled());
}

/// 取消键在窗口和 OLE 两个线程的不同先后顺序下仍只消费一次按下与释放。
///
/// @author kongweiguang
#[test]
fn escape_events_remain_scoped_to_their_native_gesture() {
    let state = DragKeyboardState::default();

    state.begin();
    assert!(state.consume_key_down(10, false));
    assert!(state.cancellation_requested());
    state.finish_at(20);
    assert!(state.consume_key_up());
    assert!(!state.consume_key_down(30, false));
    assert!(!state.consume_key_up());

    state.begin();
    state.observe_ole_escape();
    state.finish_at(20);
    assert!(state.consume_key_down(10, false));
    assert!(state.consume_key_up());
    assert!(!state.consume_key_down(30, false));

    state.begin();
    assert!(state.consume_key_down(10, false));
    assert!(state.consume_key_up());
    state.observe_ole_escape();
    state.finish_at(20);
    assert!(!state.consume_key_down(30, false));

    state.begin();
    state.finish_at(20);
    assert!(!state.consume_key_down(30, false));
    assert!(!state.consume_key_up());

    state.begin();
    state.observe_ole_escape();
    state.finish_at(20);
    state.focus_changed(false);
    state.focus_changed(true);
    assert!(!state.consume_key_down(30, false));

    state.begin();
    state.observe_ole_escape();
    state.finish_at(20);
    assert!(!state.consume_key_down(30, false));
    assert!(!state.consume_key_up());

    state.begin();
    state.observe_ole_escape();
    state.finish_at(2);
    assert!(state.consume_key_down(u32::MAX, false));
    assert!(state.consume_key_up());
}

/// 无目标效果的松手是取消；真实失败回执和已受理目标不能被隐藏。
///
/// @author kongweiguang
#[test]
fn outside_drop_is_cancelled_only_without_copy_or_internal_acceptance() {
    assert!(unaccepted_drop_is_cancelled(
        DRAGDROP_S_DROP,
        DROPEFFECT_NONE,
        None,
        false
    ));
    assert!(!unaccepted_drop_is_cancelled(
        E_FAIL,
        DROPEFFECT_NONE,
        None,
        false
    ));
    assert!(!unaccepted_drop_is_cancelled(
        DRAGDROP_S_DROP,
        DROPEFFECT_COPY,
        None,
        false
    ));
    assert!(!unaccepted_drop_is_cancelled(
        DRAGDROP_S_DROP,
        DROPEFFECT_NONE,
        Some((E_FAIL, 0)),
        false
    ));
    assert!(!unaccepted_drop_is_cancelled(
        DRAGDROP_S_DROP,
        DROPEFFECT_NONE,
        None,
        true
    ));
}

/// 本机路径载荷可能被平台克隆，但失败只会反馈到原面板一次。
///
/// @author kongweiguang
#[test]
fn local_drag_feedback_is_delivered_once_across_clones() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let captured = outcomes.clone();
    let paths = FileDragPaths::new([(PathBuf::from(r"C:\test\file.txt"), false)])
        .on_finished(move |outcome| captured.lock().expect("outcome lock").push(outcome));
    let clone = paths.clone();
    paths.finish(ExternalFileDragOutcome::Failed("failed".into()));
    clone.finish(ExternalFileDragOutcome::Completed);
    let outcomes = outcomes.lock().expect("outcome lock");
    assert!(
        matches!(outcomes.as_slice(), [ExternalFileDragOutcome::Failed(message)] if message == "failed")
    );
}
