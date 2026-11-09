//! @author kongweiguang

//! WebView2 只写入有硬上限的内存流；不经磁盘、DOM 或同步消息泵生成背板。

use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
    time::Instant,
};

use kerminal_application::{AppError, AppResult};
use webview2_com::{
    CapturePreviewCompletedHandler,
    Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
};
use windows::{
    Win32::{
        Foundation::{
            E_FAIL, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, S_FALSE, S_OK, STG_E_INVALIDFUNCTION,
            STG_E_MEDIUMFULL,
        },
        System::Com::{
            ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC,
            STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
        },
    },
    core::{HRESULT, Ref, Result as WindowsResult, implement},
};
use wry::WebViewExtWindows as _;

const MAX_PREVIEW_BYTES: usize = 32 * 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Default)]
struct Buffer {
    bytes: Vec<u8>,
    position: usize,
    rejected: bool,
}

impl Buffer {
    /// 先验证大小并尝试分配，拒绝发生后整次截图作废，不能返回截断但看似成功的 PNG。
    fn resize(&mut self, size: usize) -> WindowsResult<()> {
        if self.rejected || size > MAX_PREVIEW_BYTES {
            self.rejected = true;
            return Err(STG_E_MEDIUMFULL.into());
        }
        if size > self.bytes.len()
            && self
                .bytes
                .try_reserve_exact(size - self.bytes.len())
                .is_err()
        {
            self.rejected = true;
            return Err(E_OUTOFMEMORY.into());
        }
        self.bytes.resize(size, 0);
        Ok(())
    }

    /// 允许编码器回写头部和有界稀疏定位，但任何定位都不能绕过写入上限。
    fn seek(&mut self, offset: i64, origin: STREAM_SEEK) -> WindowsResult<()> {
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => self.position as i64,
            STREAM_SEEK_END => self.bytes.len() as i64,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let position = base.checked_add(offset).ok_or(STG_E_INVALIDFUNCTION)?;
        if !(0..=MAX_PREVIEW_BYTES as i64).contains(&position) {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        self.position = position as usize;
        Ok(())
    }
}

#[implement(IStream)]
struct PreviewStream(Arc<Mutex<Buffer>>);

impl ISequentialStream_Impl for PreviewStream_Impl {
    /// COM 调用方拥有缓冲区；只复制已写入区间，短读按 IStream 约定返回 S_FALSE。
    fn Read(&self, output: *mut c_void, count: u32, read: *mut u32) -> HRESULT {
        // SAFETY: 可选结果指针和数据缓冲区由 COM 调用方提供，零字节操作不解引用空数据指针。
        unsafe {
            if !read.is_null() {
                read.write(0);
            }
        }
        if output.is_null() && count != 0 {
            return E_POINTER;
        }
        let Ok(mut buffer) = self.0.lock() else {
            return E_FAIL;
        };
        let available = buffer.bytes.len().saturating_sub(buffer.position);
        let copied = available.min(count as usize);
        unsafe {
            if copied != 0 {
                std::ptr::copy_nonoverlapping(
                    buffer.bytes.as_ptr().add(buffer.position),
                    output.cast::<u8>(),
                    copied,
                );
            }
            if !read.is_null() {
                read.write(copied as u32);
            }
        }
        buffer.position += copied;
        if copied == count as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    /// 在读取输入或扩容之前检查最终位置；失败不提交任何部分写入。
    fn Write(&self, input: *const c_void, count: u32, written: *mut u32) -> HRESULT {
        // SAFETY: COM 调用方保证非空输入覆盖 count 字节，结果指针允许省略。
        unsafe {
            if !written.is_null() {
                written.write(0);
            }
        }
        if input.is_null() && count != 0 {
            return E_POINTER;
        }
        let Ok(mut buffer) = self.0.lock() else {
            return E_FAIL;
        };
        let Some(end) = buffer.position.checked_add(count as usize) else {
            buffer.rejected = true;
            return STG_E_MEDIUMFULL;
        };
        let size = buffer.bytes.len().max(end);
        if let Err(error) = buffer.resize(size) {
            return error.code();
        }
        unsafe {
            if count != 0 {
                std::ptr::copy_nonoverlapping(
                    input.cast::<u8>(),
                    buffer.bytes.as_mut_ptr().add(buffer.position),
                    count as usize,
                );
            }
            if !written.is_null() {
                written.write(count);
            }
        }
        buffer.position = end;
        S_OK
    }
}

impl IStream_Impl for PreviewStream_Impl {
    /// 与写入共享同一位置锁，编码器的回写不能越过内存预算。
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, position: *mut u64) -> WindowsResult<()> {
        let mut buffer = self.0.lock().map_err(|_| E_FAIL)?;
        buffer.seek(offset, origin)?;
        if !position.is_null() {
            // SAFETY: 非空输出指针由 COM 调用方提供。
            unsafe { position.write(buffer.position as u64) };
        }
        Ok(())
    }

    /// 提前预分配也遵守相同硬限额，不允许通过 SetSize 绕过 Write 检查。
    fn SetSize(&self, size: u64) -> WindowsResult<()> {
        let mut buffer = self.0.lock().map_err(|_| E_FAIL)?;
        let size = usize::try_from(size).map_err(|_| STG_E_MEDIUMFULL)?;
        buffer.resize(size)
    }

    /// 截图流没有对外复制用途；拒绝隐式复制以避免预算外的第二份缓冲区。
    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> WindowsResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 内存写入立即生效，无磁盘刷新或事务等待。
    fn Commit(&self, _: &STGC) -> WindowsResult<()> {
        Ok(())
    }

    /// 不承诺回滚语义，失败由截图请求整体丢弃处理。
    fn Revert(&self) -> WindowsResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 流仅属于一次截图请求，不向编码器提供跨进程区域锁。
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> WindowsResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 与 LockRegion 一致，不伪造未实现的锁状态。
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> WindowsResult<()> {
        Err(E_NOTIMPL.into())
    }

    /// 仅报告流类型与真实字节数，不分配或返回任何文件名。
    fn Stat(&self, output: *mut STATSTG, _: &STATFLAG) -> WindowsResult<()> {
        if output.is_null() {
            return Err(E_POINTER.into());
        }
        let buffer = self.0.lock().map_err(|_| E_FAIL)?;
        // SAFETY: 非空 STATSTG 输出由 COM 调用方提供。
        unsafe {
            output.write(STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: buffer.bytes.len() as u64,
                ..Default::default()
            });
        }
        Ok(())
    }

    /// 不允许产生独立游标或共享写入别名，保持一次请求一个有界结果。
    fn Clone(&self) -> WindowsResult<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// UI 线程只提交原生异步请求；回调移出同一有界缓冲区，主线程泵与解码由宿主决定。
pub(super) fn capture(
    webview: &wry::WebView,
    callback: impl FnOnce(AppResult<super::PreviewCapture>) + 'static,
) -> AppResult<()> {
    let started = Instant::now();
    let buffer = Arc::new(Mutex::new(Buffer::default()));
    let stream: IStream = PreviewStream(buffer.clone()).into();
    let handler = CapturePreviewCompletedHandler::create(Box::new(move |status| {
        if let Err(error) = status {
            callback(Err(AppError::infrastructure_with_source(
                "WebView2 未能生成背板截图",
                error,
            )));
            return Ok(());
        }
        let result = {
            buffer.lock().ok().and_then(|mut buffer| {
                (!buffer.rejected && buffer.bytes.starts_with(PNG_SIGNATURE))
                    .then(|| std::mem::take(&mut buffer.bytes))
            })
        };
        callback(
            result
                .map(|bytes| super::PreviewCapture {
                    data: super::PreviewData::Png(bytes),
                    api_duration: started.elapsed(),
                })
                .ok_or_else(|| AppError::infrastructure("WebView2 背板PNG无效或超限")),
        );
        Ok(())
    }));
    // SAFETY: WebView2 在所属 UI 线程调用；引擎在异步操作期间持有 stream/handler 的 COM 引用。
    unsafe {
        webview.webview().CapturePreview(
            COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
            &stream,
            &handler,
        )
    }
    .map_err(|error| AppError::infrastructure_with_source("无法请求原生 WebView 背板截图", error))
}

#[cfg(test)]
#[path = "../../../../tests/unit/plugin_webview_preview.rs"]
mod tests;
