//! SFTP 传输策略快照与运行中预算更新回归测试。
//!
//! @author kongweiguang

use std::time::Duration;

pub use kerminal_application::{AppError, AppResult};
use kerminal_domain::identity::HostId;
use tokio_util::sync::CancellationToken;

pub mod preferences {
    pub use kerminal_application::preferences::SftpPerformanceSettings;
}

#[path = "../src/transfers/policy.rs"]
mod policy;

use policy::{SftpTransferPolicySnapshot, SftpTransferPolicyStore};
use preferences::SftpPerformanceSettings;

/// 用窄范围并发设置保持每个测试对 admission 行为的预期清晰。
fn settings(global_transfers: usize, host_transfers: usize) -> SftpPerformanceSettings {
    SftpPerformanceSettings {
        global_transfers,
        host_transfers,
        ..SftpPerformanceSettings::default()
    }
}

/// 有效策略保留网络参数，并根据 packet 与 pipeline 深度计算有界在途窗口。
#[test]
fn snapshot_preserves_effective_sftp_settings() -> AppResult<()> {
    let snapshot = SftpTransferPolicySnapshot::from_settings(SftpPerformanceSettings::default())?;
    assert_eq!(snapshot.global_transfers(), 4);
    assert_eq!(snapshot.host_transfers(), 2);
    assert_eq!(snapshot.pipeline_depth(), 8);
    assert_eq!(snapshot.packet_bytes(), 256 * 1024);
    assert_eq!(snapshot.pipeline_window_bytes(), 2 * 1024 * 1024);
    assert_eq!(snapshot.idle_timeout(), Duration::from_secs(180));
    Ok(())
}

/// 策略保存边界拒绝越界值，调用方必须先归一化用户设置而不是隐藏错误。
/// @author kongweiguang
#[test]
fn policy_rejects_out_of_range_or_inconsistent_values() {
    let invalid = SftpPerformanceSettings {
        global_transfers: 17,
        ..SftpPerformanceSettings::default()
    };
    assert!(matches!(
        SftpTransferPolicySnapshot::from_settings(invalid),
        Err(AppError::Invalid { .. })
    ));

    let mut inconsistent = settings(2, 3);
    inconsistent.pipeline_depth = 257;
    assert!(SftpTransferPolicySnapshot::from_settings(inconsistent).is_err());
}

/// 配置快照是值对象，更新只改变之后读取的策略而不改写已入队副本。
#[test]
fn updates_replace_snapshots_without_mutating_captured_values() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(2, 1))?;
    let captured = store.snapshot();
    let updated = store.update(settings(4, 2))?;

    assert_eq!(captured.global_transfers(), 2);
    assert_eq!(captured.host_transfers(), 1);
    assert_eq!(updated.global_transfers(), 4);
    assert_eq!(store.snapshot(), updated);
    Ok(())
}

/// 无效热更新保留先前有效快照，不让错误输入影响正在使用的配额。
#[test]
fn invalid_update_keeps_previous_policy() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(2, 1))?;
    let previous = store.snapshot();
    let invalid = settings(2, 3);

    assert!(store.update(invalid).is_err());
    assert_eq!(store.snapshot(), previous);
    Ok(())
}

/// 降低并发上限不撤销在途 permit；等旧任务全部释放后才按新上限接纳任务。
#[tokio::test]
async fn lowering_limits_waits_for_existing_permits_to_drain() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(2, 2))?;
    let host_a = HostId::new();
    let host_b = HostId::new();
    let first = store
        .acquire([host_a.clone()], &CancellationToken::new())
        .await?;
    let second = store.acquire([host_b], &CancellationToken::new()).await?;
    store.update(settings(1, 1))?;

    let waiter_store = store.clone();
    let waiter_host = host_a.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire([waiter_host], &CancellationToken::new())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter)
            .await
            .is_err()
    );

    drop(first);
    drop(second);
    let permit = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("capacity update wakes waiters")
        .expect("admission task does not panic")?;
    assert_eq!(permit.hosts(), &[host_a]);
    Ok(())
}

/// 同主机并发可动态放宽；跨主机请求只占一次全局 permit 且主机集合自动去重。
#[tokio::test]
async fn host_limits_update_live_and_host_ids_are_deduplicated() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(3, 1))?;
    let host = HostId::new();
    let first = store
        .acquire([host.clone()], &CancellationToken::new())
        .await?;
    let waiter_store = store.clone();
    let waiter_host = host.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire(
                [waiter_host.clone(), waiter_host],
                &CancellationToken::new(),
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter)
            .await
            .is_err()
    );

    store.update(settings(3, 2))?;
    let second = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("host limit update wakes waiters")
        .expect("admission task does not panic")?;
    assert_eq!(second.hosts().len(), 1);
    drop((first, second));
    Ok(())
}

/// 同一主机的显式覆盖必须限制新准入，而不能只保存在主机配置中供 UI 回显。
///
/// @author kongweiguang
#[tokio::test]
async fn host_override_limits_same_host_admission() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(4, 2))?;
    let host = HostId::new();
    let active = store
        .acquire_with_host_limits([(host.clone(), Some(1))], &CancellationToken::new())
        .await?;

    let waiter_store = store.clone();
    let waiter_host = host.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits([(waiter_host, Some(1))], &CancellationToken::new())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter)
            .await
            .is_err()
    );

    drop(active);
    let second = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("释放主机许可后唤醒覆盖策略等待者")
        .expect("准入任务不 panic")?;
    assert_eq!(second.hosts(), &[host]);
    Ok(())
}

/// 主机覆盖缺省值应继承 settings.toml 的每主机并发，而非绕过该预算。
///
/// @author kongweiguang
#[tokio::test]
async fn missing_host_override_inherits_global_host_limit() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(4, 2))?;
    let host = HostId::new();
    let first = store
        .acquire_with_host_limits([(host.clone(), None)], &CancellationToken::new())
        .await?;
    let second = store
        .acquire_with_host_limits([(host.clone(), None)], &CancellationToken::new())
        .await?;

    let waiter_store = store.clone();
    let waiter_host = host.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits([(waiter_host, None)], &CancellationToken::new())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter)
            .await
            .is_err()
    );

    drop(first);
    let third = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("释放默认主机许可后唤醒等待者")
        .expect("准入任务不 panic")?;
    assert_eq!(third.hosts(), &[host]);
    drop((second, third));
    Ok(())
}

/// 单主机显式值即使高于全局限制，也不能把全局任务预算扩容。
///
/// @author kongweiguang
#[tokio::test]
async fn host_override_above_global_limit_remains_globally_capped() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(2, 1))?;
    let host_a = HostId::new();
    let host_b = HostId::new();
    let host_c = HostId::new();
    let first = store
        .acquire_with_host_limits([(host_a, Some(8))], &CancellationToken::new())
        .await?;
    let second = store
        .acquire_with_host_limits([(host_b, Some(8))], &CancellationToken::new())
        .await?;

    let waiter_store = store.clone();
    let waiter_host = host_c.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits([(waiter_host, Some(8))], &CancellationToken::new())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter)
            .await
            .is_err()
    );

    drop(first);
    let third = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("释放全局许可后唤醒等待者")
        .expect("准入任务不 panic")?;
    assert_eq!(third.hosts(), &[host_c]);
    drop((second, third));
    Ok(())
}

/// 一次跨主机任务须原子占用两端预算，释放后两个主机等待者都可继续。
///
/// @author kongweiguang
#[tokio::test]
async fn cross_host_transfer_reserves_both_host_limits() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(4, 2))?;
    let host_a = HostId::new();
    let host_b = HostId::new();
    let cross_host = store
        .acquire_with_host_limits(
            [(host_a.clone(), Some(1)), (host_b.clone(), Some(1))],
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(cross_host.hosts().len(), 2);

    let waiter_a_store = store.clone();
    let waiter_a_host = host_a.clone();
    let mut waiter_a = tokio::spawn(async move {
        waiter_a_store
            .acquire_with_host_limits([(waiter_a_host, Some(1))], &CancellationToken::new())
            .await
    });
    let waiter_b_store = store.clone();
    let waiter_b_host = host_b.clone();
    let mut waiter_b = tokio::spawn(async move {
        waiter_b_store
            .acquire_with_host_limits([(waiter_b_host, Some(1))], &CancellationToken::new())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter_a)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter_b)
            .await
            .is_err()
    );

    drop(cross_host);
    let permit_a = tokio::time::timeout(Duration::from_secs(1), waiter_a)
        .await
        .expect("释放跨主机许可后唤醒 A")
        .expect("A 准入任务不 panic")?;
    let permit_b = tokio::time::timeout(Duration::from_secs(1), waiter_b)
        .await
        .expect("释放跨主机许可后唤醒 B")
        .expect("B 准入任务不 panic")?;
    assert_eq!(permit_a.hosts(), &[host_a]);
    assert_eq!(permit_b.hosts(), &[host_b]);
    Ok(())
}

/// 同一跨端任务重复提到主机时只计一次，避免多占额度造成虚假饥饿。
///
/// @author kongweiguang
#[tokio::test]
async fn repeated_host_endpoint_is_counted_once_per_transfer() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(3, 2))?;
    let host = HostId::new();
    let first = store
        .acquire_with_host_limits(
            [(host.clone(), Some(2)), (host.clone(), Some(2))],
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(first.hosts().len(), 1);

    let second = tokio::time::timeout(
        Duration::from_millis(100),
        store.acquire_with_host_limits([(host.clone(), Some(2))], &CancellationToken::new()),
    )
    .await
    .expect("重复端点只能占一个主机许可")?;
    assert_eq!(second.hosts(), &[host]);
    Ok(())
}

/// 热更新只收紧后续准入，不撤销已授予的覆盖许可或放任新任务越过新上限。
///
/// @author kongweiguang
#[tokio::test]
async fn settings_update_preserves_existing_override_permit() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(2, 2))?;
    let host = HostId::new();
    let active = store
        .acquire_with_host_limits([(host.clone(), Some(2))], &CancellationToken::new())
        .await?;

    store.update(settings(1, 1))?;
    assert_eq!(active.hosts(), std::slice::from_ref(&host));
    let waiter_store = store.clone();
    let waiter_host = host.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits([(waiter_host, None)], &CancellationToken::new())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut waiter)
            .await
            .is_err()
    );

    drop(active);
    let after_update = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("旧许可释放后使用更新后的策略准入")
        .expect("准入任务不 panic")?;
    assert_eq!(after_update.hosts(), &[host]);
    Ok(())
}

/// 已入队快照的每主机上限在热更新后保持不变，新快照仍可按新上限准入。
///
/// @author kongweiguang
#[tokio::test]
async fn queued_snapshot_preserves_host_limit_after_policy_update() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(4, 1))?;
    let host = HostId::new();
    let queued_policy = store.snapshot();
    let active = store
        .acquire_with_host_limits_at_snapshot(
            [(host.clone(), None)],
            queued_policy,
            &CancellationToken::new(),
        )
        .await?;

    let waiter_store = store.clone();
    let waiter_host = host.clone();
    let mut queued_waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits_at_snapshot(
                [(waiter_host, None)],
                queued_policy,
                &CancellationToken::new(),
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );

    let updated_policy = store.update(settings(4, 2))?;
    let new_task = store
        .acquire_with_host_limits_at_snapshot(
            [(host.clone(), None)],
            updated_policy,
            &CancellationToken::new(),
        )
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );

    drop(active);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );
    drop(new_task);
    let old_task = tokio::time::timeout(Duration::from_secs(1), queued_waiter)
        .await
        .expect("释放旧任务快照占用后应唤醒等待者")
        .expect("等待任务不 panic")?;
    assert_eq!(old_task.hosts(), std::slice::from_ref(&host));
    drop(old_task);

    let first = store
        .acquire_with_host_limits_at_snapshot(
            [(host.clone(), None)],
            updated_policy,
            &CancellationToken::new(),
        )
        .await?;
    let second = store
        .acquire_with_host_limits_at_snapshot(
            [(host.clone(), None)],
            updated_policy,
            &CancellationToken::new(),
        )
        .await?;
    drop((first, second));
    Ok(())
}

/// 不同配置版本的任务共用全局活动计数，旧快照不能因更新而提前越过自身上限。
///
/// @author kongweiguang
#[tokio::test]
async fn queued_snapshot_shares_global_active_count_after_policy_update() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(2, 1))?;
    let host_a = HostId::new();
    let host_b = HostId::new();
    let host_c = HostId::new();
    let host_d = HostId::new();
    let queued_policy = store.snapshot();
    let first = store
        .acquire_with_host_limits_at_snapshot(
            [(host_a.clone(), None)],
            queued_policy,
            &CancellationToken::new(),
        )
        .await?;
    let second = store
        .acquire_with_host_limits_at_snapshot(
            [(host_b.clone(), None)],
            queued_policy,
            &CancellationToken::new(),
        )
        .await?;

    let waiter_store = store.clone();
    let waiter_host = host_c.clone();
    let mut queued_waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits_at_snapshot(
                [(waiter_host, None)],
                queued_policy,
                &CancellationToken::new(),
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );

    let updated_policy = store.update(settings(4, 2))?;
    let third = store
        .acquire_with_host_limits_at_snapshot(
            [(host_c.clone(), None)],
            updated_policy,
            &CancellationToken::new(),
        )
        .await?;
    let fourth = store
        .acquire_with_host_limits_at_snapshot(
            [(host_d, None)],
            updated_policy,
            &CancellationToken::new(),
        )
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );

    drop(first);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );
    drop(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut queued_waiter)
            .await
            .is_err()
    );
    drop(third);
    let old_task = tokio::time::timeout(Duration::from_secs(1), queued_waiter)
        .await
        .expect("活动数低于旧上限后应唤醒等待者")
        .expect("等待任务不 panic")?;
    assert_eq!(old_task.hosts(), &[host_c]);
    drop((old_task, fourth));
    Ok(())
}

/// 等待中的任务能响应 caller cancellation，不占用全局或主机配额。
#[tokio::test]
async fn waiting_admission_is_cancellable() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(1, 1))?;
    let host = HostId::new();
    let active = store
        .acquire([host.clone()], &CancellationToken::new())
        .await?;
    let cancellation = CancellationToken::new();
    let queued_policy = store.snapshot();
    let waiter_store = store.clone();
    let waiter_host = host;
    let waiter_cancel = cancellation.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits_at_snapshot(
                [(waiter_host, None)],
                queued_policy,
                &waiter_cancel,
            )
            .await
    });
    tokio::task::yield_now().await;
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), &mut waiter)
        .await
        .expect("cancellation wakes waiter")
        .expect("admission task does not panic");
    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    drop(active);
    let after_cancel = store
        .acquire_with_host_limits_at_snapshot(
            [(HostId::new(), None)],
            queued_policy,
            &CancellationToken::new(),
        )
        .await?;
    drop(after_cancel);
    Ok(())
}

/// 关闭策略入口会唤醒并拒绝等待者，避免 shutdown 时留下无主 admission future。
#[tokio::test]
async fn close_wakes_waiters_and_rejects_new_admissions() -> AppResult<()> {
    let store = SftpTransferPolicyStore::new(settings(1, 1))?;
    let host = HostId::new();
    let queued_policy = store.snapshot();
    let active = store
        .acquire([host.clone()], &CancellationToken::new())
        .await?;
    let waiter_store = store.clone();
    let waiter_host = host.clone();
    let mut waiter = tokio::spawn(async move {
        waiter_store
            .acquire_with_host_limits_at_snapshot(
                [(waiter_host, None)],
                queued_policy,
                &CancellationToken::new(),
            )
            .await
    });
    tokio::task::yield_now().await;
    store.close();

    let result = tokio::time::timeout(Duration::from_secs(1), &mut waiter)
        .await
        .expect("close wakes waiter")
        .expect("admission task does not panic");
    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert!(store.update(settings(1, 1)).is_err());
    assert!(matches!(
        store.acquire([host], &CancellationToken::new()).await,
        Err(AppError::Cancelled { .. })
    ));
    drop(active);
    Ok(())
}
