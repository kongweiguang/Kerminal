// @author kongweiguang

use std::sync::Arc;

use gpui::{ClipboardEntry, ClipboardItem, ClipboardString, Image, ImageFormat};

use super::{
    ClipboardSendPayload, TEXT_MIME_TYPES, clipboard_text_payload, payload_for_mime,
    source_mime_types,
};

const PNG_BYTES: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xB5, 0x1C, 0x0C,
    0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xFC, 0xFF, 0x1F, 0x00,
    0x03, 0x03, 0x02, 0x00, 0xEF, 0xA3, 0xDF, 0x47, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

/// 构造原始图像项以验证 Wayland 只按 entry 格式发送已编码字节。
fn image_item(format: ImageFormat, bytes: &[u8]) -> ClipboardItem {
    ClipboardItem {
        entries: vec![ClipboardEntry::Image(Image {
            format,
            bytes: bytes.to_vec(),
            id: 1,
        })],
    }
}

/// 图片项只公布它实际持有的图像 MIME，不把图像数据冒充为文本。
#[test]
fn image_only_item_advertises_image_and_self_mime_only() {
    let item = image_item(ImageFormat::Png, PNG_BYTES);
    let mime_types = source_mime_types(Some(&item), "pid/test");

    assert_eq!(
        mime_types,
        vec!["image/png".to_owned(), "pid/test".to_owned()]
    );
    assert!(
        !TEXT_MIME_TYPES
            .iter()
            .any(|text_mime| mime_types.iter().any(|mime| mime == *text_mime))
    );
}

/// `image/png` 请求应逐字节返回 image entry，不进行编码、前缀或文本转换。
#[test]
fn png_request_returns_the_original_png_bytes() {
    let item = Arc::new(image_item(ImageFormat::Png, PNG_BYTES));
    let payload = payload_for_mime(item, "image/png").expect("PNG MIME must select PNG entry");

    assert!(matches!(&payload, ClipboardSendPayload::Image { .. }));
    assert_eq!(payload.bytes(), PNG_BYTES);
}

/// 空字符串仍是可用文本项；Wayland 必须公布并以零字节响应文本请求。
#[test]
fn empty_string_remains_an_advertised_text_payload() {
    let item = ClipboardItem::new_string(String::new());
    let mime_types = source_mime_types(Some(&item), "pid/test");

    assert!(
        TEXT_MIME_TYPES
            .iter()
            .all(|text_mime| mime_types.iter().any(|mime| mime == *text_mime))
    );
    let payload = payload_for_mime(Arc::new(item), "text/plain")
        .expect("an empty String entry is still text");
    assert_eq!(payload.bytes(), b"");
}

/// 文本 MIME 保持原有拼接语义，图像 MIME 则只选择对应格式 entry。
#[test]
fn mixed_item_routes_each_mime_to_its_matching_entry() {
    let item = ClipboardItem {
        entries: vec![
            ClipboardEntry::String(ClipboardString::new("clipboard text".to_owned())),
            ClipboardEntry::Image(Image {
                format: ImageFormat::Png,
                bytes: PNG_BYTES.to_vec(),
                id: 2,
            }),
        ],
    };
    let mime_types = source_mime_types(Some(&item), "pid/test");

    assert!(
        TEXT_MIME_TYPES
            .iter()
            .all(|text_mime| mime_types.iter().any(|mime| mime == *text_mime))
    );
    assert!(mime_types.iter().any(|mime| mime == "image/png"));
    assert_eq!(
        payload_for_mime(Arc::new(item.clone()), "UTF8_STRING")
            .expect("text MIME selects text")
            .bytes(),
        b"clipboard text"
    );
    assert_eq!(
        payload_for_mime(Arc::new(item), "image/png")
            .expect("PNG MIME selects image")
            .bytes(),
        PNG_BYTES
    );
}

/// MIME 不匹配时不发送其他内容，避免静默把文字伪装为 PNG 或反向转换。
#[test]
fn unsupported_or_missing_mime_has_no_payload() {
    let text_item = Arc::new(ClipboardItem::new_string("only text".to_owned()));
    let png_item = Arc::new(image_item(ImageFormat::Png, PNG_BYTES));

    assert!(payload_for_mime(Arc::clone(&text_item), "image/png").is_none());
    assert!(payload_for_mime(png_item, "text/plain").is_none());
    assert!(payload_for_mime(text_item, "application/octet-stream").is_none());
}

/// 正在写入的 image payload 持有 item，传输结束或取消后即释放这份共享所有权。
#[test]
fn image_payload_releases_shared_clipboard_item_when_dropped() {
    let item = Arc::new(image_item(ImageFormat::Png, PNG_BYTES));
    let weak = Arc::downgrade(&item);
    let payload = payload_for_mime(Arc::clone(&item), "image/png").expect("PNG payload");

    drop(item);
    assert!(weak.upgrade().is_some());
    drop(payload);
    assert!(weak.upgrade().is_none());
}

/// 非文本、非图片项目不声明外部数据格式，只保留进程内自引用 MIME。
#[test]
fn empty_item_advertises_no_content_mime() {
    assert_eq!(
        source_mime_types(
            Some(&ClipboardItem {
                entries: Vec::new()
            }),
            "pid/test"
        ),
        vec!["pid/test".to_owned()]
    );
    assert_eq!(
        clipboard_text_payload(&ClipboardItem {
            entries: Vec::new()
        }),
        None
    );
}
