//! 本地 PTY 输出队列的有界、保序与关闭期 drain 契约测试。
//!
//! @author kongweiguang

use super::{OUTPUT_CHUNK_BYTES, OUTPUT_QUEUE_CHUNKS, output_reader_channel, spawn_pty_reader};

use std::io::Cursor;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::io::AsyncReadExt;
use tokio::time::{Duration, sleep, timeout};

/// 生成跨 chunk 边界可识别的 payload，用于验证顺序与尾部字节数。
///
/// @author kongweiguang
fn patterned_output(chunks: usize) -> Vec<u8> {
    (0..(chunks * OUTPUT_CHUNK_BYTES + 137))
        .map(|index| (index % 251) as u8)
        .collect()
}

/// 运行期间即使队列满也等待 consumer，验证队列有界且在 consumer 恢复后不丢字节。
///
/// @author kongweiguang
#[tokio::test]
async fn running_output_backpressure_preserves_order_without_loss() {
    let expected = patterned_output(OUTPUT_QUEUE_CHUNKS + 3);
    let (mut output, sender) = output_reader_channel();
    let closing = Arc::new(AtomicBool::new(false));
    let child_exited = Arc::new(AtomicBool::new(false));
    let reader = spawn_pty_reader(
        Box::new(Cursor::new(expected.clone())),
        sender,
        Arc::clone(&closing),
        Arc::clone(&child_exited),
    );

    sleep(Duration::from_millis(20)).await;
    assert!(
        !reader.is_finished(),
        "运行态 producer 应在有界队列满时等待 consumer"
    );

    let mut actual = Vec::new();
    timeout(Duration::from_secs(2), output.read_to_end(&mut actual))
        .await
        .expect("consumer 恢复后读取 PTY 输出超时")
        .expect("读取 PTY 输出失败");
    let stats = timeout(Duration::from_secs(2), reader)
        .await
        .expect("运行态 reader worker 等待超时")
        .expect("运行态 reader worker 被取消")
        .expect("运行态 reader 失败");

    assert_eq!(actual, expected);
    assert_eq!(stats.bytes_read, expected.len() as u64);
    assert_eq!(stats.bytes_queued, expected.len() as u64);
    assert_eq!(stats.end_tail_discarded_bytes, 0);
}

/// 自然退出不会解除满队列背压；慢 consumer 恢复后仍须收到超过队列容量的全部输出。
///
/// @author kongweiguang
#[tokio::test]
async fn natural_exit_preserves_large_output_until_consumer_drains() {
    let expected = patterned_output(OUTPUT_QUEUE_CHUNKS + 7);
    let (mut output, sender) = output_reader_channel();
    let closing = Arc::new(AtomicBool::new(false));
    let child_exited = Arc::new(AtomicBool::new(true));
    let reader = spawn_pty_reader(
        Box::new(Cursor::new(expected.clone())),
        sender,
        Arc::clone(&closing),
        Arc::clone(&child_exited),
    );

    sleep(Duration::from_millis(20)).await;
    assert!(
        !reader.is_finished(),
        "自然退出不能以丢弃尾部的方式绕过未消费的有界队列"
    );

    let mut actual = Vec::new();
    timeout(Duration::from_secs(2), output.read_to_end(&mut actual))
        .await
        .expect("自然退出后的 PTY output drain 超时")
        .expect("自然退出后的 PTY output 读取失败");
    let stats = timeout(Duration::from_secs(2), reader)
        .await
        .expect("自然退出 reader worker 等待超时")
        .expect("自然退出 reader worker 被取消")
        .expect("自然退出 reader worker 失败");

    assert_eq!(actual, expected);
    assert_eq!(stats.bytes_read, expected.len() as u64);
    assert_eq!(stats.bytes_queued, expected.len() as u64);
    assert_eq!(stats.end_tail_discarded_bytes, 0);
}

/// 关闭时解除队列背压继续读取；已入队 prefix 仍可读，舍弃尾部按字节计数。
///
/// @author kongweiguang
#[tokio::test]
async fn close_drain_keeps_queued_prefix_and_counts_discarded_tail() {
    let expected = patterned_output(OUTPUT_QUEUE_CHUNKS + 5);
    let (mut output, sender) = output_reader_channel();
    let closing = Arc::new(AtomicBool::new(false));
    let child_exited = Arc::new(AtomicBool::new(false));
    let reader = spawn_pty_reader(
        Box::new(Cursor::new(expected.clone())),
        sender,
        Arc::clone(&closing),
        Arc::clone(&child_exited),
    );

    sleep(Duration::from_millis(20)).await;
    closing.store(true, Ordering::Release);
    let stats = timeout(Duration::from_secs(2), reader)
        .await
        .expect("close 状态下 reader 未能解除有界队列背压")
        .expect("关闭期 reader worker 被取消")
        .expect("关闭期 reader worker 失败");

    let mut queued = Vec::new();
    timeout(Duration::from_secs(2), output.read_to_end(&mut queued))
        .await
        .expect("关闭后读取已排队输出超时")
        .expect("关闭后读取已排队输出失败");
    assert!(!queued.is_empty(), "关闭不得丢弃已排队的输出 prefix");
    assert_eq!(queued, expected[..queued.len()]);
    assert_eq!(queued.len() as u64, stats.bytes_queued);
    assert_eq!(stats.bytes_read, expected.len() as u64);
    assert_eq!(
        stats.bytes_read,
        stats.bytes_queued + stats.end_tail_discarded_bytes
    );
    assert!(stats.end_tail_discarded_bytes > 0);
}
