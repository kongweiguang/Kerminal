//! 只读本机暂存文件的 OLE 流；克隆共享文件所有权但保留独立游标。
//!
//! Local adaptations @author kongweiguang

use std::{
    ffi::c_void,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use gpui::ExternalFileDragOwner;
use windows::{
    Win32::{
        Foundation::{
            E_FAIL, S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_FILENOTFOUND, STG_E_INVALIDFLAG,
            STG_E_INVALIDFUNCTION, STG_E_INVALIDPOINTER, STG_E_READFAULT, STG_E_SEEKERROR,
            STG_E_WRITEFAULT,
        },
        System::Com::{
            ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_NONAME,
            STATSTG, STGC, STGM_READ, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END,
            STREAM_SEEK_SET,
        },
    },
    core::{HRESULT, Ref, Result as WinResult, implement},
};

/// 最后释放计数前先丢弃文件和暂存 owner，避免后台清理与尚未关闭的流竞争。
struct StreamLease(Arc<AtomicUsize>);

impl StreamLease {
    /// 只有成功取得计数的流才能交给 COM，失败不会留下永远活动的流。
    ///
    /// @author kongweiguang
    fn acquire(streams: Arc<AtomicUsize>) -> WinResult<Self> {
        streams
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        Ok(Self(streams))
    }
}

impl Drop for StreamLease {
    /// 每个独立 COM 流实例恰好递减一次；同一接口的 AddRef 不创建新流。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 文件句柄串行访问；共享句柄的实际指针不等于任一个独立流的逻辑位置。
#[implement(IStream)]
struct FileStream {
    cursor: Mutex<u64>,
    file: Arc<Mutex<File>>,
    owner: Arc<dyn ExternalFileDragOwner>,
    activity: Arc<AtomicU64>,
    // 结构字段按声明顺序析构，因此 lease 必须位于文件和 owner 之后。
    lease: StreamLease,
}

/// 创建只读流；源已由传输队列完整准备，任何打开失败都不能增加活动流计数。
///
/// @author kongweiguang
pub(super) fn open_stream(
    path: &Path,
    owner: Arc<dyn ExternalFileDragOwner>,
    streams: Arc<AtomicUsize>,
    activity: Arc<AtomicU64>,
) -> WinResult<IStream> {
    let file = File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => windows::core::Error::from(STG_E_FILENOTFOUND),
        std::io::ErrorKind::PermissionDenied => windows::core::Error::from(STG_E_ACCESSDENIED),
        _ => windows::core::Error::from(STG_E_READFAULT),
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
    if !metadata.is_file() {
        return Err(STG_E_INVALIDFUNCTION.into());
    }
    make_stream(Arc::new(Mutex::new(file)), 0, owner, streams, activity)
}

/// 为原始流及其克隆建立独立游标；所有克隆共享已打开的文件，避免路径被替换后切换对象。
///
/// @author kongweiguang
fn make_stream(
    file: Arc<Mutex<File>>,
    cursor: u64,
    owner: Arc<dyn ExternalFileDragOwner>,
    streams: Arc<AtomicUsize>,
    activity: Arc<AtomicU64>,
) -> WinResult<IStream> {
    let lease = StreamLease::acquire(streams)?;
    Ok(FileStream {
        cursor: Mutex::new(cursor),
        file,
        owner,
        activity,
        lease,
    }
    .into())
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for FileStream_Impl {
    /// 完整填充请求或读取到 EOF；错误时仍回报已经读取的字节并保留真实游标。
    ///
    /// @author kongweiguang
    fn Read(&self, destination: *mut c_void, requested: u32, bytes_read: *mut u32) -> HRESULT {
        if !bytes_read.is_null() {
            unsafe { bytes_read.write(0) };
        }
        if requested == 0 {
            return S_OK;
        }
        if destination.is_null() || (requested as u64) > isize::MAX as u64 {
            return STG_E_INVALIDPOINTER;
        }
        // SAFETY: ISequentialStream::Read requires a writable buffer of requested bytes.
        let buffer =
            unsafe { std::slice::from_raw_parts_mut(destination.cast::<u8>(), requested as usize) };
        let mut cursor = match self.cursor.lock() {
            Ok(cursor) => cursor,
            Err(_) => return STG_E_READFAULT,
        };
        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(_) => return STG_E_READFAULT,
        };
        if file.seek(SeekFrom::Start(*cursor)).is_err() {
            return STG_E_SEEKERROR;
        }
        let mut completed = 0usize;
        let mut failure = None;
        while completed < buffer.len() {
            let end = buffer.len().min(completed.saturating_add(64 * 1024));
            match file.read(&mut buffer[completed..end]) {
                Ok(0) => break,
                Ok(count) => {
                    completed += count;
                    self.activity.fetch_add(count as u64, Ordering::Relaxed);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    failure = Some(STG_E_READFAULT);
                    break;
                }
            }
        }
        if !bytes_read.is_null() {
            unsafe { bytes_read.write(completed as u32) };
        }
        match cursor.checked_add(completed as u64) {
            Some(position) if position <= i64::MAX as u64 => *cursor = position,
            _ => return STG_E_SEEKERROR,
        }
        if let Some(failure) = failure {
            failure
        } else if completed == buffer.len() {
            S_OK
        } else {
            S_FALSE
        }
    }

    /// 源副本只供目标读取，拒绝写入并明确返回零已写字节。
    ///
    /// @author kongweiguang
    fn Write(&self, _: *const c_void, _: u32, bytes_written: *mut u32) -> HRESULT {
        if !bytes_written.is_null() {
            unsafe { bytes_written.write(0) };
        }
        STG_E_ACCESSDENIED
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for FileStream_Impl {
    /// 使用受检整数计算逻辑位置，允许越过 EOF，但拒绝负值和 Windows 文件偏移溢出。
    ///
    /// @author kongweiguang
    fn Seek(&self, displacement: i64, origin: STREAM_SEEK, output: *mut u64) -> WinResult<()> {
        let mut cursor = self
            .cursor
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_SEEKERROR))?;
        let base = match origin {
            STREAM_SEEK_SET => 0_i128,
            STREAM_SEEK_CUR => i128::from(*cursor),
            STREAM_SEEK_END => {
                let file = self
                    .file
                    .lock()
                    .map_err(|_| windows::core::Error::from(STG_E_SEEKERROR))?;
                i128::from(
                    file.metadata()
                        .map_err(|_| windows::core::Error::from(STG_E_SEEKERROR))?
                        .len(),
                )
            }
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let position = base + i128::from(displacement);
        if position < 0 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        if position > i128::from(i64::MAX) {
            return Err(STG_E_SEEKERROR.into());
        }
        *cursor = position as u64;
        if !output.is_null() {
            unsafe { output.write(*cursor) };
        }
        Ok(())
    }

    /// 只读源流不能截断或扩展暂存文件。
    ///
    /// @author kongweiguang
    fn SetSize(&self, _: u64) -> WinResult<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    /// 每次只搬运固定大小的缓冲区，目标短写会继续写完，失败保留双方真实字节计数。
    ///
    /// @author kongweiguang
    fn CopyTo(
        &self,
        destination: Ref<'_, IStream>,
        requested: u64,
        bytes_read: *mut u64,
        bytes_written: *mut u64,
    ) -> WinResult<()> {
        let mut total_read = 0_u64;
        let mut total_written = 0_u64;
        let result = (|| -> WinResult<()> {
            let destination = destination
                .as_ref()
                .ok_or_else(|| windows::core::Error::from(STG_E_INVALIDPOINTER))?;
            let mut buffer = [0_u8; 64 * 1024];
            while total_read < requested {
                let amount = (requested - total_read).min(buffer.len() as u64) as u32;
                let mut count = 0_u32;
                let status = self.Read(buffer.as_mut_ptr().cast(), amount, &mut count);
                total_read += u64::from(count);
                status.ok()?;
                if count == 0 {
                    break;
                }
                let mut written = 0_usize;
                while written < count as usize {
                    let remaining = count - written as u32;
                    let mut partial = 0_u32;
                    // SAFETY: buffer covers the requested unread bytes for this synchronous call.
                    let result = unsafe {
                        destination.Write(
                            buffer[written..count as usize].as_ptr().cast(),
                            remaining,
                            Some(&mut partial),
                        )
                    };
                    if partial > remaining {
                        return Err(STG_E_WRITEFAULT.into());
                    }
                    total_written += u64::from(partial);
                    if partial != 0 {
                        self.activity
                            .fetch_add(u64::from(partial), Ordering::Relaxed);
                    }
                    result.ok()?;
                    if partial == 0 {
                        return Err(STG_E_WRITEFAULT.into());
                    }
                    written += partial as usize;
                }
                if status == S_FALSE {
                    break;
                }
            }
            Ok(())
        })();
        if !bytes_read.is_null() {
            unsafe { bytes_read.write(total_read) };
        }
        if !bytes_written.is_null() {
            unsafe { bytes_written.write(total_written) };
        }
        result
    }

    /// 只读流没有可提交的更改，成功表示无需持久化动作。
    ///
    /// @author kongweiguang
    fn Commit(&self, _: &STGC) -> WinResult<()> {
        Ok(())
    }

    /// 源流没有事务草稿，不提供回滚语义。
    ///
    /// @author kongweiguang
    fn Revert(&self) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    /// 拖出的不可变源不提供区域锁；目标不可把锁成功当作文件修改权。
    ///
    /// @author kongweiguang
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    /// 不支持区域锁的流也不能伪报解锁成功。
    ///
    /// @author kongweiguang
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> WinResult<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    /// 报告已打开文件的真实长度；流名由虚拟文件描述符提供，不暴露本机暂存路径。
    ///
    /// @author kongweiguang
    fn Stat(&self, output: *mut STATSTG, flags: &STATFLAG) -> WinResult<()> {
        if output.is_null() {
            return Err(STG_E_INVALIDPOINTER.into());
        }
        if flags.0 != 0 && *flags != STATFLAG_NONAME {
            return Err(STG_E_INVALIDFLAG.into());
        }
        let file = self
            .file
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
        let size = file
            .metadata()
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?
            .len();
        let stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: size,
            grfMode: STGM_READ,
            ..Default::default()
        };
        unsafe { output.write(stat) };
        Ok(())
    }

    /// 克隆从当前逻辑位置开始，使用同一文件对象但不共享可变游标。
    ///
    /// @author kongweiguang
    fn Clone(&self) -> WinResult<IStream> {
        let cursor = *self
            .cursor
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_SEEKERROR))?;
        make_stream(
            self.file.clone(),
            cursor,
            self.owner.clone(),
            self.lease.0.clone(),
            self.activity.clone(),
        )
    }
}

#[cfg(test)]
#[path = "../../tests/unit/external_drag_stream.rs"]
mod tests;
