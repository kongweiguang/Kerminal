//! 远端原始名称投影与本机路径语义隔离的回归。
//!
//! @author kongweiguang

use super::{
    FileAttributes, FileEndpoint, RemotePath, project_sftp_directory, temporary_remote_sibling,
};
use kerminal_application::PublicErrorCode;
use kerminal_domain::identity::HostId;

/// 一个合法反斜杠名称不能阻断普通兄弟项，返回的 endpoint 保留原始 POSIX 身份。
///
/// @author kongweiguang
#[test]
fn mixed_posix_names_preserve_every_entry_and_identity() {
    let endpoint = FileEndpoint::remote(HostId::new(), "/srv/data").expect("endpoint");
    let listing = project_sftp_directory(
        endpoint.clone(),
        [
            (r"backup\2026.txt".to_string(), FileAttributes::empty()),
            ("ordinary.txt".to_string(), FileAttributes::empty()),
        ]
        .into_iter(),
    )
    .expect("complete listing");
    assert_eq!(listing.entries.len(), 2);
    assert_eq!(listing.entries[0].name, r"backup\2026.txt");
    assert_eq!(
        listing.entries[0].endpoint.remote_path().unwrap().as_str(),
        r"/srv/data/backup\2026.txt"
    );
    assert_eq!(listing.entries[1].name, "ordinary.txt");
    assert_eq!(listing.directory, endpoint);
}

/// 控制字符混入正常清单必须明确拒绝整次读取，不能给冲突预检一个缺项的成功结果。
///
/// @author kongweiguang
#[test]
fn unsupported_control_names_do_not_publish_partial_listing_or_raw_controls() {
    let endpoint = FileEndpoint::remote(HostId::new(), "/srv/data").expect("endpoint");
    let error = project_sftp_directory(
        endpoint,
        [
            ("ordinary.txt".to_string(), FileAttributes::empty()),
            ("line\nbreak.txt".to_string(), FileAttributes::empty()),
        ]
        .into_iter(),
    )
    .expect_err("unsupported listing");
    assert_eq!(error.public_code(), PublicErrorCode::Unsupported);
    assert!(error.public_message().contains("重命名"));
    assert!(!error.public_message().chars().any(char::is_control));
}

/// 编辑暂存名不依赖用户 basename，长名称和反斜杠目录仍在原父目录内生成短 sibling。
///
/// @author kongweiguang
#[test]
fn edit_staging_names_stay_short_and_inside_the_original_parent() {
    let target =
        RemotePath::parse(&format!(r"/srv/backup\2026/{}.txt", "a".repeat(250))).expect("target");
    let staging = temporary_remote_sibling(&target).expect("staging");
    assert_eq!(staging.parent(), target.parent());
    assert!(staging.as_str().rsplit('/').next().unwrap().len() < 100);
    assert_ne!(staging, target);
    assert!(temporary_remote_sibling(&RemotePath::parse("/").unwrap()).is_err());
}
