use std::{
    fs::File,
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    sync::Arc,
};

use calloop::{LoopHandle, PostAction};
use filedescriptor::Pipe;
use strum::IntoEnumIterator;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::platform::{PIPE_READ_TIMEOUT, read_fd_with_timeout};
use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat, hash};

/// Text mime types that we'll offer to other programs.
pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const FILE_LIST_MIME_TYPE: &str = "text/uri-list";

/// Text mime types that we'll accept from other programs.
pub(crate) const ALLOWED_TEXT_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "UTF8_STRING"];

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, ()>,
    self_mime: String,

    // Internal clipboard
    contents: Option<Arc<ClipboardItem>>,
    primary_contents: Option<Arc<ClipboardItem>>,

    // External clipboard
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<WlDataOffer>>,
    cached_primary_read: Option<ClipboardItem>,
    current_primary_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}

/// Wayland 的异步发送源必须在 fd 写完前持有图片数据，避免为每个请求复制大图。
enum ClipboardSendPayload {
    Owned(Vec<u8>),
    Image {
        item: Arc<ClipboardItem>,
        entry_index: usize,
    },
}

impl ClipboardSendPayload {
    /// 返回本次 MIME 请求对应的数据切片；图片直接借用共享 clipboard item 中的原始字节。
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Image { item, entry_index } => match item.entries().get(*entry_index) {
                Some(gpui::ClipboardEntry::Image(image)) => image.bytes.as_slice(),
                _ => &[],
            },
        }
    }
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|t| t == mime_type)
    }

    fn read_bytes(&self, connection: &Connection, mime_type: &str) -> Option<Vec<u8>> {
        let pipe = Pipe::new().unwrap();
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        connection.flush().unwrap();

        match read_fd_with_timeout(fd, PIPE_READ_TIMEOUT) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                log::error!("error reading clipboard pipe: {err:?}");
                None
            }
        }
    }

    fn read_text(&self, connection: &Connection) -> Option<ClipboardItem> {
        let mime_type = self.mime_types.iter().find(|&mime_type| {
            ALLOWED_TEXT_MIME_TYPES
                .iter()
                .any(|&allowed| allowed == mime_type)
        })?;
        let bytes = self.read_bytes(connection, mime_type)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                log::error!("Failed to convert clipboard content to UTF-8: {}", e);
                return None;
            }
        };

        // Normalize the text to unix line endings, otherwise
        // copying from eg: firefox inserts a lot of blank
        // lines, and that is super annoying.
        let result = text_content.replace("\r\n", "\n");
        Some(ClipboardItem::new_string(result))
    }

    fn read_image(&self, connection: &Connection) -> Option<ClipboardItem> {
        for format in ImageFormat::iter() {
            let mime_type = format.mime_type();
            if !self.has_mime_type(mime_type) {
                continue;
            }

            if let Some(bytes) = self.read_bytes(connection, mime_type) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }
}

impl Clipboard {
    pub fn new(connection: Connection, loop_handle: LoopHandle<'static, ()>) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime: format!("pid/{}", std::process::id()),

            contents: None,
            primary_contents: None,

            cached_read: None,
            current_offer: None,
            cached_primary_read: None,
            current_primary_offer: None,
        }
    }

    /// 以共享所有权保留当前项，使多个并行 Wayland fd 请求共用同一份图片缓冲区。
    pub fn set(&mut self, item: ClipboardItem) {
        self.contents = Some(Arc::new(item));
    }

    /// 主选择与普通剪贴板共用同一数据源语义，但保留独立内容和生命周期。
    pub fn set_primary(&mut self, item: ClipboardItem) {
        self.primary_contents = Some(Arc::new(item));
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.cached_read = None;
        self.current_offer = data_offer;
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.cached_primary_read = None;
        self.current_primary_offer = data_offer;
    }

    pub fn self_mime(&self) -> String {
        self.self_mime.clone()
    }

    /// 严格按请求 MIME 选择文本或匹配格式的图像，不把不匹配内容伪装成其他格式。
    pub fn send(&self, mime_type: String, fd: OwnedFd) {
        if let Some(item) = self.contents.as_ref()
            && let Some(payload) = payload_for_mime(Arc::clone(item), &mime_type)
        {
            self.send_payload(fd, payload);
        }
    }

    /// 主选择请求使用与普通 clipboard 相同的 MIME 校验和异步 fd 所有权规则。
    pub fn send_primary(&self, mime_type: String, fd: OwnedFd) {
        if let Some(item) = self.primary_contents.as_ref()
            && let Some(payload) = payload_for_mime(Arc::clone(item), &mime_type)
        {
            self.send_payload(fd, payload);
        }
    }

    /// 只公布当前 item 实际可提供的 MIME，保留内部自引用类型用于避免重复读取。
    pub fn clipboard_mime_types(&self) -> Vec<String> {
        source_mime_types(self.contents.as_deref(), &self.self_mime)
    }

    /// 主选择独立公布其内容类型，避免把另一剪贴板的格式误报给请求方。
    pub fn primary_mime_types(&self) -> Vec<String> {
        source_mime_types(self.primary_contents.as_deref(), &self.self_mime)
    }

    /// 保持对外拖放数据的现有 owned-buffer 接口，并交给统一的 fd 生命周期处理。
    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        self.send_payload(fd, ClipboardSendPayload::Owned(bytes));
    }

    /// 由 calloop source 持有 fd 与 payload；完成、写错或 source 删除时一并释放二者。
    fn send_payload(&self, fd: OwnedFd, payload: ClipboardSendPayload) {
        let mut written = 0;
        if let Err(error) = self.loop_handle.insert_source(
            calloop::generic::Generic::new(
                File::from(fd),
                calloop::Interest::WRITE,
                calloop::Mode::Level,
            ),
            move |_, file, _| {
                let file = unsafe { file.get_mut() };
                loop {
                    let bytes = payload.bytes();
                    if written == bytes.len() {
                        break Ok(PostAction::Remove);
                    }
                    match file.write(&bytes[written..]) {
                        Ok(0) => break Ok(PostAction::Remove),
                        Ok(count) => written += count,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            break Ok(PostAction::Continue);
                        }
                        Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break Ok(PostAction::Remove),
                    }
                }
            },
        ) {
            log::error!("failed to queue Wayland clipboard transfer: {error:?}");
        }
    }

    /// 同进程读取仍复用保存的原始 item，避免在 Wayland MIME 往返中损失多格式内容。
    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_offer.as_ref()?;
        if let Some(cached) = self.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.contents.as_deref().cloned();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_read = Some(item.clone());
        Some(item)
    }

    /// 主选择读取与普通 clipboard 使用相同缓存和自有 MIME 快路径。
    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_primary_offer.as_ref()?;
        if let Some(cached) = self.cached_primary_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.primary_contents.as_deref().cloned();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_primary_read = Some(item.clone());
        Some(item)
    }
}

/// 为字符串 entry、外部路径和图片 entry 选择 Wayland 可以正确解释的实际字节。
fn payload_for_mime(
    item: Arc<ClipboardItem>,
    requested_mime: &str,
) -> Option<ClipboardSendPayload> {
    if TEXT_MIME_TYPES.contains(&requested_mime) {
        return clipboard_text_payload(&item).map(ClipboardSendPayload::Owned);
    }

    let requested_format = ImageFormat::from_mime_type(requested_mime)?;
    let entry_index = item.entries().iter().position(|entry| {
        matches!(entry, gpui::ClipboardEntry::Image(image) if image.format == requested_format && !image.bytes.is_empty())
    })?;
    Some(ClipboardSendPayload::Image { item, entry_index })
}

/// 空字符串仍是有效的文本 clipboard 内容；仅在没有任何字符串 entry 时返回无文本。
fn clipboard_text_payload(item: &ClipboardItem) -> Option<Vec<u8>> {
    if let Some(text) = item.text() {
        return Some(text.into_bytes());
    }
    item.entries()
        .iter()
        .any(|entry| matches!(entry, gpui::ClipboardEntry::String(_)))
        .then(Vec::new)
}

/// MIME 声明不克隆文本缓冲区，同时保持 `ClipboardItem::text` 的文本优先和路径回退规则。
fn has_text_payload(item: &ClipboardItem) -> bool {
    let has_nonempty_text = item
        .entries()
        .iter()
        .any(|entry| matches!(entry, gpui::ClipboardEntry::String(text) if !text.text.is_empty()));
    if has_nonempty_text {
        return true;
    }

    let has_nonempty_path = item.entries().iter().any(|entry| {
        matches!(entry, gpui::ClipboardEntry::ExternalPaths(paths) if paths.0.iter().any(|path| !path.as_os_str().is_empty()))
    });
    has_nonempty_path
        || item
            .entries()
            .iter()
            .any(|entry| matches!(entry, gpui::ClipboardEntry::String(_)))
}

/// 只公布真实可服务的 MIME，并对重复图像格式去重，避免声明不存在的文本或编码。
fn source_mime_types(item: Option<&ClipboardItem>, self_mime: &str) -> Vec<String> {
    let mut mime_types = Vec::new();
    if let Some(item) = item {
        if has_text_payload(item) {
            mime_types.extend(TEXT_MIME_TYPES.map(str::to_owned));
        }
        for entry in item.entries() {
            if let gpui::ClipboardEntry::Image(image) = entry
                && !image.bytes.is_empty()
            {
                let mime_type = image.format.mime_type();
                if !mime_types.iter().any(|existing| existing == mime_type) {
                    mime_types.push(mime_type.to_owned());
                }
            }
        }
    }
    mime_types.push(self_mime.to_owned());
    mime_types
}

#[cfg(test)]
#[path = "../../../tests/wayland_clipboard.rs"]
mod tests;
