// @author kongweiguang
//! Root 更新退出门禁的纯状态测试，不创建 GPUI 窗口或触发桌面关闭。

use kerminal_application::updates::DownloadedUpdateId;

use super::{GuardRequestResult, UpdateInstallGuard};

#[test]
/// 一个根窗口只保护一个安装句柄，重复请求幂等且不同句柄不能覆盖当前门禁。
fn guard_rejects_replacement_and_accepts_exact_duplicate() {
    let first_id = DownloadedUpdateId::new();
    let second_id = DownloadedUpdateId::new();
    let mut guard = UpdateInstallGuard::default();

    assert_eq!(
        guard.begin(first_id, "0.4.1".to_owned()),
        GuardRequestResult::Started
    );
    assert_eq!(
        guard.begin(first_id, "0.4.1".to_owned()),
        GuardRequestResult::AlreadyPending
    );
    assert_eq!(
        guard.begin(second_id, "0.4.2".to_owned()),
        GuardRequestResult::Busy
    );
    assert_eq!(
        guard.pending().map(|request| request.update_id),
        Some(first_id)
    );
}

#[test]
/// 迟到的其他句柄回执不能解除当前门禁，精确成功回执才消费请求。
fn approval_consumes_only_matching_handle() {
    let update_id = DownloadedUpdateId::new();
    let mut guard = UpdateInstallGuard::default();
    guard.begin(update_id, "0.4.1".to_owned());

    assert!(guard.approve(DownloadedUpdateId::new()).is_none());
    let approved = guard.approve(update_id).expect("matching guard approval");

    assert_eq!(approved.update_id, update_id);
    assert_eq!(approved.version, "0.4.1");
    assert!(guard.pending().is_none());
}

#[test]
/// 用户取消时保留当前句柄供稍后安装，不把旧取消回执应用到其它更新。
fn cancellation_consumes_only_matching_handle() {
    let update_id = DownloadedUpdateId::new();
    let mut guard = UpdateInstallGuard::default();
    guard.begin(update_id, "0.4.1".to_owned());

    assert!(guard.cancel(DownloadedUpdateId::new()).is_none());
    assert!(guard.pending().is_some());
    assert_eq!(
        guard
            .cancel(update_id)
            .expect("matching cancellation")
            .update_id,
        update_id
    );
    assert!(guard.pending().is_none());
}
