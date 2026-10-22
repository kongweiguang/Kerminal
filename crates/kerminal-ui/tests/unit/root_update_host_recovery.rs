//! 更新后单实例 listener 恢复回执的身份匹配契约测试。
//!
//! @author kongweiguang

use kerminal_application::updates::DownloadedUpdateId;

use super::super::updates::UpdateInstallRequest;
use super::pending_install_matches;

#[test]
/// 只有同一 update ID 与版本的 pending 安装请求可接收 listener 恢复失败回执。
fn recovery_failure_matches_exact_pending_update() {
    let update_id = DownloadedUpdateId::new();
    let other_id = DownloadedUpdateId::new();
    let request = UpdateInstallRequest {
        update_id,
        version: "0.4.2".to_owned(),
    };

    assert!(pending_install_matches(Some(&request), update_id, "0.4.2"));
    assert!(!pending_install_matches(Some(&request), update_id, "0.4.3"));
    assert!(!pending_install_matches(Some(&request), other_id, "0.4.2"));
    assert!(!pending_install_matches(None, update_id, "0.4.2"));
}
