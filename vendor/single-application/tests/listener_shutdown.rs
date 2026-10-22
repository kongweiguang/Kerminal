//! Kerminal 回归用例：单实例 owner join 后必须真正释放本地 socket。
//!
//! @author kongweiguang

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use single_application::{
    PrimaryInstanceShutdownError, SecondaryInstanceSendError, SingleApplication,
};
use tokio::{
    runtime::Builder,
    time::{Instant, timeout},
};

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// 用唯一 socket name 隔离并发测试，使结果只反映 listener owner 的 join 行为。
fn unique_instance_id() -> String {
    let sequence = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
    format!("kerminal-listener-test-{}-{sequence}", std::process::id())
}

/// 验证重复启动在关闭前仍受保护，shutdown join 后同一 ID 可立即重新成为 primary。
#[test]
fn shutdown_joins_listener_and_releases_socket() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build");
    let id = unique_instance_id();

    runtime.block_on(async {
        let mut primary = match SingleApplication::<String>::new(&id) {
            SingleApplication::PrimaryInstance(primary) => primary,
            SingleApplication::SecondaryInstance(_) => panic!("first instance must be primary"),
        };
        let shutdown = primary.shutdown_handle();

        let secondary = match SingleApplication::<String>::new(&id) {
            SingleApplication::SecondaryInstance(secondary) => secondary,
            SingleApplication::PrimaryInstance(_) => {
                panic!("live listener must reject a second primary")
            }
        };
        assert!(matches!(
            secondary.try_send_message("normal wire".to_owned()).await,
            Ok(())
        ));
        assert_eq!(
            timeout(Duration::from_secs(2), primary.recv())
                .await
                .expect("normal wire should be received"),
            Some("normal wire".to_owned())
        );

        assert!(matches!(
            secondary.try_send_message("x".repeat(128 * 1024)).await,
            Err(SecondaryInstanceSendError::Oversized)
        ));
        assert!(
            timeout(Duration::from_millis(50), primary.recv())
                .await
                .is_err()
        );

        shutdown
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .expect("listener task must stop before deadline");
        shutdown
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .expect("completed shutdown must be idempotent");

        match SingleApplication::<String>::new(&id) {
            SingleApplication::PrimaryInstance(primary) => {
                primary
                    .shutdown_handle()
                    .shutdown_until(Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("reacquired listener must also be joined");
            }
            SingleApplication::SecondaryInstance(_) => {
                panic!("released socket must allow another primary instance")
            }
        }
    });
}

/// 在 current-thread runtime 建立前构造已过期输入，使首次 shutdown 不会让出执行权给 listener。
/// Tokio timer 按毫秒向上取整，now() 的零预算仍可能 yield 后得到正常 join；这里不实际等待，
/// 而是保留严格超时断言，再验证同一 owner 重试前 socket 仍受保护、重试后才能重新成为 primary。
///
/// @author kongweiguang
#[test]
fn timed_out_shutdown_keeps_owner_for_retry() {
    let expired_deadline = Instant::now() - Duration::from_secs(1);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build");
    let id = unique_instance_id();

    runtime.block_on(async {
        let primary = match SingleApplication::<String>::new(&id) {
            SingleApplication::PrimaryInstance(primary) => primary,
            SingleApplication::SecondaryInstance(_) => panic!("first instance must be primary"),
        };
        let shutdown = primary.shutdown_handle();
        let first_shutdown_result = shutdown.shutdown_until(expired_deadline).await;
        assert!(
            matches!(
                &first_shutdown_result,
                Err(PrimaryInstanceShutdownError::TimedOut)
            ),
            "first shutdown must retain a pending owner: {first_shutdown_result:?}"
        );
        assert!(
            matches!(
                SingleApplication::<String>::new(&id),
                SingleApplication::SecondaryInstance(_)
            ),
            "timed-out owner must retain the original listener before the runtime yields"
        );
        shutdown
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .expect("same owner should finish after its earlier timeout");
        drop(primary);

        match SingleApplication::<String>::new(&id) {
            SingleApplication::PrimaryInstance(primary) => {
                primary
                    .shutdown_handle()
                    .shutdown_until(Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("reacquired listener must also be joined");
            }
            SingleApplication::SecondaryInstance(_) => {
                panic!("completed retry should release the socket")
            }
        }
    });
}

/// 主实例关闭后 secondary 返回连接错误，不把无主监听状态报告为发送成功。
#[test]
fn secondary_reports_closed_primary_connection() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build");
    let id = unique_instance_id();

    runtime.block_on(async {
        let primary = match SingleApplication::<String>::new(&id) {
            SingleApplication::PrimaryInstance(primary) => primary,
            SingleApplication::SecondaryInstance(_) => panic!("first instance must be primary"),
        };
        let shutdown = primary.shutdown_handle();
        let secondary = match SingleApplication::<String>::new(&id) {
            SingleApplication::SecondaryInstance(secondary) => secondary,
            SingleApplication::PrimaryInstance(_) => {
                panic!("live listener must reject a second primary")
            }
        };
        shutdown
            .shutdown_until(Instant::now() + Duration::from_secs(2))
            .await
            .expect("primary listener should be released");
        drop(primary);

        assert!(matches!(
            secondary.try_send_message("focus".to_owned()).await,
            Err(SecondaryInstanceSendError::Connect(_))
        ));
    });
}

/// 验证 typed shutdown error 对 timeout 与任务异常保留区分，避免宿主把未知关闭当成功。
#[test]
fn shutdown_error_has_stable_safe_display() {
    assert_eq!(
        PrimaryInstanceShutdownError::TimedOut.to_string(),
        "等待单实例监听器关闭超时"
    );
}
