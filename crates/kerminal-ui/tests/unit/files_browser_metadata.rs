//! 文件浏览器实际列宽与元数据时间投影的外置回归。
//!
//! @author kongweiguang

use super::*;

/// 仅构造已有元数据契约，避免测试依赖文件系统时钟或真实连接。
///
/// @author kongweiguang
fn entry(endpoint: FileEndpoint, modified: Option<&str>) -> FileEntry {
    FileEntry {
        endpoint,
        name: "sample.txt".to_owned(),
        kind: FileEntryKind::File,
        size: Some(64),
        modified: modified.map(str::to_owned),
        permissions: None,
        is_hidden: false,
        raw: None,
    }
}

/// 期望值按 main 的本机时区和当前年格式规则生成，避免测试依赖宿主固定时区。
///
/// @author kongweiguang
fn expected_local_modified(timestamp: DateTime<Utc>) -> String {
    let local = timestamp.with_timezone(&Local);
    if local.year() == Local::now().year() {
        local.format("%m-%d %H:%M").to_string()
    } else {
        local.format("%Y-%m-%d").to_string()
    }
}

/// main 默认时间列常驻，400px 起增加大小列，540px 前不显示权限列。
///
/// @author kongweiguang
#[test]
fn dock_width_keeps_size_and_modified_columns() {
    for width in [400., 480., 500., 520., 539.] {
        let columns = FileTableColumns::for_width(px(width));
        assert!(columns.size);
        assert!(columns.modified);
        assert!(!columns.permissions);
    }
}

/// 400px 以下只保留名称与时间列，防止大小和权限挤占可读名称空间。
///
/// @author kongweiguang
#[test]
fn narrow_width_reduces_metadata_before_name_space() {
    for width in [0., 240., 319., 399.] {
        let columns = FileTableColumns::for_width(px(width));
        assert!(!columns.size);
        assert!(columns.modified);
        assert!(!columns.permissions);
    }
    let columns = FileTableColumns::for_width(px(400.));
    assert!(columns.size);
    assert!(columns.modified);
    assert!(!columns.permissions);
}

/// 540px 起显示权限列；断点以下继续保留名称、大小和时间列。
///
/// @author kongweiguang
#[test]
fn right_dock_width_keeps_permission_column_reachable() {
    for width in [540., 600., 640., 670., 680.] {
        let columns = FileTableColumns::for_width(px(width));
        assert!(columns.size && columns.modified && columns.permissions);
    }
    for width in [400., 500., 539.] {
        let columns = FileTableColumns::for_width(px(width));
        assert!(columns.size && columns.modified && !columns.permissions);
    }
}

/// 同一时刻的 Local/SFTP 纳秒与容器整秒必须显示相同本地时间，不按字符串长度猜单位。
///
/// @author kongweiguang
#[test]
fn endpoint_timestamp_units_project_the_same_local_time() {
    let local = entry(
        FileEndpoint::Local {
            path: PathBuf::from("sample.txt"),
        },
        Some("1700000000123456789"),
    );
    let remote = entry(
        FileEndpoint::remote(kerminal_domain::identity::HostId::new(), "/sample.txt").unwrap(),
        Some("1700000000123456789"),
    );
    let container = entry(
        FileEndpoint::Container {
            host_id: None,
            runtime: kerminal_domain::files::ContainerRuntime::Docker,
            container_id: "test-container".to_owned(),
            path: RemotePath::parse_container("/sample.txt").unwrap(),
        },
        Some("1700000000"),
    );
    for value in [local, remote, container] {
        let timestamp = file_modified_timestamp(&value).unwrap();
        assert_eq!(
            format_file_modified(&value, true),
            expected_local_modified(timestamp)
        );
        assert_eq!(
            format_file_modified_tooltip(&value, true),
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        );
    }
}

/// 纳秒余数和闰日由库处理；缺失、非法或超出固定显示年份的值必须保持明确占位。
///
/// @author kongweiguang
#[test]
fn invalid_or_out_of_range_metadata_never_fabricates_a_date() {
    for modified in [None, Some(""), Some("invalid"), Some("i128 overflow")] {
        let value = entry(
            FileEndpoint::Local {
                path: PathBuf::from("sample.txt"),
            },
            modified,
        );
        assert_eq!(format_file_modified(&value, true), "—");
    }
    for raw in ["0", "951782400000000001"] {
        let value = entry(
            FileEndpoint::Local {
                path: PathBuf::from("sample.txt"),
            },
            Some(raw),
        );
        assert_eq!(
            format_file_modified(&value, true),
            expected_local_modified(file_modified_timestamp(&value).unwrap())
        );
    }
    for raw in [
        "253402300800000000000",
        "170141183460469231731687303715884105727",
        "170141183460469231731687303715884105728",
    ] {
        let value = entry(
            FileEndpoint::Local {
                path: PathBuf::from("sample.txt"),
            },
            Some(raw),
        );
        assert_eq!(format_file_modified(&value, true), "—");
    }
}

/// 表格中的 POSIX mode 保留特殊位，并按目录、文件和链接展示类型首字符。
///
/// @author kongweiguang
#[test]
fn symbolic_permissions_show_entry_type_and_special_bits() {
    use kerminal_domain::files::FileEntryKind;

    assert_eq!(
        crate::features::files::permissions::format_posix_mode(FileEntryKind::Directory, 0o755),
        "drwxr-xr-x"
    );
    assert_eq!(
        crate::features::files::permissions::format_posix_mode(FileEntryKind::File, 0o4755),
        "-rwsr-xr-x"
    );
    assert_eq!(
        crate::features::files::permissions::format_posix_mode(FileEntryKind::Symlink, 0o777),
        "lrwxrwxrwx"
    );
}
