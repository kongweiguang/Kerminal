//! 串口领域参数到官方 Tokio 串口驱动设置的契约测试。
//!
//! @author kongweiguang

use std::{path::PathBuf, time::Duration};

use kerminal_application::{
    AppResult,
    sessions::{SessionOpenCleanupOwner, SessionTransportControl, SessionTransportFactory},
};
use kerminal_domain::sessions::{
    SerialDataBits, SerialFlowControl, SerialParity, SerialSessionTarget, SerialStopBits,
    SessionExitStatus, SessionTarget, TerminalSize,
};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinHandle,
};
use tokio_serial::{DataBits, FlowControl, Parity, StopBits};
use tokio_util::sync::CancellationToken;

use super::{
    SerialControlState, SerialOpenCleanupOwner, SerialRuntimeConfig, SerialSessionTransportControl,
    SerialSessionTransportFactory, SerialSettings,
};

/// 确认 UI/旧配置归一后的 5N2 软件流控参数不会被驱动适配器降级。
#[test]
fn maps_each_serial_setting_without_fallback() {
    let target = SerialSessionTarget {
        device: PathBuf::from("COM7"),
        baud_rate: 115_200,
        data_bits: SerialDataBits::Five,
        stop_bits: SerialStopBits::Two,
        parity: SerialParity::Odd,
        flow_control: SerialFlowControl::Software,
    };

    let settings = SerialSettings::from_target(&target).expect("valid serial settings");

    assert_eq!(settings.baud_rate, 115_200);
    assert_eq!(settings.data_bits, DataBits::Five);
    assert_eq!(settings.stop_bits, StopBits::Two);
    assert_eq!(settings.parity, Parity::Odd);
    assert_eq!(settings.flow_control, FlowControl::Software);
}

/// 拒绝超出冻结旧版有效范围的波特率，避免依赖驱动自己的隐式纠正。
#[test]
fn rejects_baud_rate_outside_legacy_range() {
    let mut target = SerialSessionTarget::new(PathBuf::from("COM7"));
    target.baud_rate = 4_000_001;

    assert!(SerialSettings::from_target(&target).is_err());
}

/// 关闭 deadline 用尽时仍保留 JoinHandle，后续关闭可等待同一设备 owner 释放。
///
/// @author kongweiguang
#[tokio::test]
async fn close_timeout_retains_serial_owner_for_retry() {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let gate = std::sync::Arc::new(Semaphore::new(0));
    let worker_gate = std::sync::Arc::clone(&gate);
    let task: JoinHandle<AppResult<SessionExitStatus>> = tokio::spawn(async move {
        task_cancellation.cancelled().await;
        let _permit = worker_gate.acquire().await.expect("test gate remains open");
        Ok(SessionExitStatus {
            code: None,
            signal: Some("serial session closed".to_owned()),
            success: true,
        })
    });
    let control = SerialSessionTransportControl {
        cancellation,
        state: Mutex::new(SerialControlState {
            task: Some(task),
            finished: None,
        }),
    };

    assert!(
        control
            .close_until(tokio::time::Instant::now() + Duration::from_millis(20))
            .await
            .is_err()
    );
    assert!(control.state.lock().await.task.is_some());

    gate.add_permits(1);
    control
        .close_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("retry closes after the worker releases its permit");
    assert!(control.state.lock().await.task.is_none());
}

/// 阻塞设备 open 未完成时，取消错误携带 owner；deadline 超时后重复关闭仍能 join 同一任务。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_blocking_open_retains_cleanup_owner_until_worker_finishes() {
    let (release, wait) = std::sync::mpsc::channel();
    let task = tokio::task::spawn_blocking(move || {
        let _ = wait.recv();
        Err(tokio_serial::Error::new(
            tokio_serial::ErrorKind::Unknown,
            "simulated open failure",
        ))
    });
    let owner = SerialOpenCleanupOwner::new(task);

    assert!(
        owner
            .close_until(tokio::time::Instant::now() + Duration::from_millis(20))
            .await
            .is_err()
    );
    assert!(owner.state.lock().await.task.is_some());

    release.send(()).expect("release blocking open");
    owner
        .close_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("retry joins worker after the device operation ends");
    assert!(owner.state.lock().await.task.is_none());
}

/// 在设备 open 尚未启动前取消不创建 cleanup owner，避免无意义地占住 session slot。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_serial_open_before_spawn_has_no_cleanup_owner() {
    let factory = SerialSessionTransportFactory::new(SerialRuntimeConfig::default())
        .expect("valid runtime config");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let target = SessionTarget::Serial(SerialSessionTarget::new(PathBuf::from("COM7")));

    let error = factory
        .open(
            &target,
            TerminalSize::new(80, 24, 0, 0).expect("valid terminal size"),
            cancellation,
        )
        .await
        .err()
        .expect("pre-cancelled open must fail");
    let (_, cleanup) = error.into_parts();

    assert!(cleanup.is_none());
}
