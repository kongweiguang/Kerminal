//! SFTP 写流水线 ACK、窗口及可信断点规则回归测试。
//!
//! @author kongweiguang

#[path = "../src/files/transfer/pipeline.rs"]
mod pipeline;

use pipeline::{SftpWriteAckPipeline, WritePipelineError};

/// 只有 flush 已收齐 ACK 且 sync 完成后才增加确认偏移。
#[test]
fn checkpoint_advances_only_after_all_acks_and_sync() {
    let mut pipeline = SftpWriteAckPipeline::new(100, 2, 4).expect("valid policy");
    let first = pipeline.reserve_write(4).expect("first packet fits");
    let second = pipeline.reserve_write(4).expect("second packet fits");

    pipeline.acknowledge(first).expect("first ACK matches");
    assert_eq!(pipeline.confirmed_offset(), 100);
    assert_eq!(
        pipeline.confirm_synced(),
        Err(WritePipelineError::AcknowledgementsPending),
    );

    pipeline
        .acknowledge(second)
        .expect("flush confirms second packet");
    assert_eq!(pipeline.confirmed_offset(), 100);
    assert_eq!(pipeline.confirm_synced(), Ok(108));
    assert_eq!(pipeline.confirmed_offset(), 108);
    assert_eq!(pipeline.in_flight_bytes(), 0);
}

/// 窗口预算同时包括未 ACK 字节和 ACK 后尚未持久化的字节。
#[test]
fn request_window_is_bounded_until_sync() {
    let mut pipeline = SftpWriteAckPipeline::new(0, 2, 4).expect("valid policy");
    let first = pipeline.reserve_write(4).expect("first packet fits");
    pipeline
        .reserve_write(4)
        .expect("second packet fills window");
    assert_eq!(pipeline.in_flight_bytes(), pipeline.window_bytes());
    assert_eq!(
        pipeline.reserve_write(1),
        Err(WritePipelineError::WindowFull),
    );

    pipeline.acknowledge(first).expect("first ACK matches");
    assert_eq!(pipeline.in_flight_bytes(), pipeline.window_bytes());
    assert_eq!(
        pipeline.reserve_write(1),
        Err(WritePipelineError::WindowFull),
    );
}

/// 乱序或重复 ACK 会被拒绝，不能让更后面的数据越过缺失写请求进入检查点。
#[test]
fn acknowledgements_must_match_fifo_write_order() {
    let mut pipeline = SftpWriteAckPipeline::new(0, 2, 4).expect("valid policy");
    let first = pipeline.reserve_write(2).expect("first packet fits");
    let second = pipeline.reserve_write(2).expect("second packet fits");
    assert_eq!(
        pipeline.acknowledge(second),
        Err(WritePipelineError::AcknowledgementOutOfOrder),
    );
    pipeline.acknowledge(first).expect("first ACK matches");
    assert_eq!(
        pipeline.acknowledge(first),
        Err(WritePipelineError::AcknowledgementOutOfOrder),
    );
}

/// 失败后重建流水线只能从最近一次确认偏移继续，遗留 partial 长度不参与推断。
#[test]
fn failed_pipeline_keeps_last_confirmed_checkpoint() {
    let mut pipeline = SftpWriteAckPipeline::new(512, 2, 4).expect("valid policy");
    let first = pipeline.reserve_write(4).expect("packet fits");
    pipeline.acknowledge(first).expect("server ACK received");
    pipeline.confirm_synced().expect("durable checkpoint");
    let _unconfirmed = pipeline.reserve_write(4).expect("next packet fits");

    assert_eq!(pipeline.confirmed_offset(), 516);
    assert_eq!(pipeline.in_flight_bytes(), 4);
    let retry = SftpWriteAckPipeline::new(pipeline.confirmed_offset(), 2, 4)
        .expect("retry starts at trusted offset");
    assert_eq!(retry.confirmed_offset(), 516);
}

/// 网络策略无效、空写和超 packet 请求均在发送前拒绝。
#[test]
fn invalid_window_and_write_sizes_are_rejected() {
    assert_eq!(
        SftpWriteAckPipeline::new(0, 0, 4).err(),
        Some(WritePipelineError::InvalidPolicy),
    );
    let mut pipeline = SftpWriteAckPipeline::new(0, 1, 4).expect("valid policy");
    assert_eq!(
        pipeline.reserve_write(0),
        Err(WritePipelineError::EmptyWrite),
    );
    assert_eq!(
        pipeline.reserve_write(5),
        Err(WritePipelineError::PacketTooLarge),
    );
}
