//! @author kongweiguang

use super::*;

/// 超限必须在分配前拒绝，且不能以随后的小写入复活已失败截图。
#[test]
fn rejects_over_limit_resize_without_allocating() {
    let mut buffer = Buffer::default();
    assert!(buffer.resize(MAX_PREVIEW_BYTES + 1).is_err());
    assert_eq!(buffer.bytes.capacity(), 0);
    assert!(buffer.resize(1).is_err());
}

/// 负定位、整数溢出和预算外定位都不能改变此前合法游标。
#[test]
fn rejects_invalid_seek_without_changing_position() {
    let mut buffer = Buffer::default();
    buffer.seek(17, STREAM_SEEK_SET).unwrap();
    assert!(buffer.seek(-18, STREAM_SEEK_CUR).is_err());
    assert!(buffer.seek(i64::MAX, STREAM_SEEK_CUR).is_err());
    assert!(
        buffer
            .seek(MAX_PREVIEW_BYTES as i64 + 1, STREAM_SEEK_SET)
            .is_err()
    );
    assert_eq!(buffer.position, 17);
}

/// 直接经过 COM vtable 检查编码器回写头部与短读，不能只验证底层 Vec 辅助函数。
#[test]
fn com_stream_round_trips_after_header_rewrite() {
    let stream: IStream = PreviewStream(Arc::new(Mutex::new(Buffer::default()))).into();
    let mut count = 0;
    let mut output = [0u8; 8];
    // SAFETY: 测试的输入、输出及计数指针始终覆盖明确传入的字节数。
    unsafe {
        assert_eq!(
            stream.Write(b"abcdef".as_ptr().cast(), 6, Some(&mut count)),
            S_OK
        );
        assert_eq!(count, 6);
        stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
        assert_eq!(stream.Write(b"PNG".as_ptr().cast(), 3, None), S_OK);
        stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
        assert_eq!(
            stream.Read(output.as_mut_ptr().cast(), 8, Some(&mut count)),
            S_FALSE
        );
    }
    assert_eq!(count, 6);
    assert_eq!(&output[..6], b"PNGdef");
}

/// 预算末尾的一字节写入必须立即失败，不能先扩容或解引用错误长度的数据。
#[test]
fn com_stream_rejects_write_past_budget_before_allocating() {
    let buffer = Arc::new(Mutex::new(Buffer::default()));
    let stream: IStream = PreviewStream(buffer.clone()).into();
    let mut written = 99;
    // SAFETY: 输入仅一字节，符合 Write 的 count；有界定位本身不会分配。
    unsafe {
        stream
            .Seek(MAX_PREVIEW_BYTES as i64, STREAM_SEEK_SET, None)
            .unwrap();
        assert_eq!(
            stream.Write(b"x".as_ptr().cast(), 1, Some(&mut written)),
            STG_E_MEDIUMFULL
        );
    }
    assert_eq!(written, 0);
    let buffer = buffer.lock().unwrap();
    assert_eq!(buffer.bytes.capacity(), 0);
    assert!(buffer.rejected);
}
