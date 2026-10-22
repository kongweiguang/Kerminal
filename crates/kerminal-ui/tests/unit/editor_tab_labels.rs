//! 编辑器标签在同名文件间显示足够路径信息的边界测试。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::{files::FileEndpoint, identity::HostId};

use super::{
    disambiguated_tab_titles_with_names, editor_visible_context_label, endpoint_target_label,
};
use crate::features::files::FileTargetNameState;

/// 通过生产路径解析器创建 SSH 文件端点，不在测试里复制远端路径拆分规则。
///
/// @author kongweiguang
fn remote(host: &str, path: &str) -> FileEndpoint {
    FileEndpoint::remote(HostId::parse(host).expect("host id is valid"), path)
        .expect("remote path is valid")
}

/// 把 endpoint/title 输入生产标签投影，验证真实渲染使用的纯辅助逻辑。
///
/// @author kongweiguang
fn titles(endpoints: &[FileEndpoint]) -> Vec<String> {
    let input = endpoints
        .iter()
        .map(|endpoint| (endpoint, "同名.txt"))
        .collect::<Vec<_>>();
    disambiguated_tab_titles_with_names(&input, &FileTargetNameState::default())
}

#[test]
/// 没有 basename 冲突时继续展示简短原始标题。
///
/// @author kongweiguang
fn unique_basename_stays_concise() {
    let endpoints = [
        FileEndpoint::Local {
            path: PathBuf::from("workspace/README.md"),
        },
        FileEndpoint::Local {
            path: PathBuf::from("workspace/notes.txt"),
        },
    ];
    let input = endpoints
        .iter()
        .map(|endpoint| {
            (
                endpoint,
                match endpoint {
                    FileEndpoint::Local { path } => path.file_name().unwrap().to_str().unwrap(),
                    _ => unreachable!(),
                },
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        disambiguated_tab_titles_with_names(&input, &FileTargetNameState::default()),
        ["README.md", "notes.txt"]
    );
}

#[test]
/// 相同文件名优先附加最短的不同父目录，远端路径始终使用 POSIX 分隔符。
///
/// @author kongweiguang
fn same_basename_uses_distinct_parent_paths() {
    let endpoints = [
        remote("host-a", "/srv/project/目录甲/同名.txt"),
        remote("host-a", "/srv/project/目录乙/同名.txt"),
    ];

    assert_eq!(titles(&endpoints), ["目录甲/同名.txt", "目录乙/同名.txt"]);
}

#[test]
/// 父目录同名时继续向上扩展，直到展示片段能区分两个文档。
///
/// @author kongweiguang
fn duplicate_parent_names_climb_until_paths_differ() {
    let endpoints = [
        remote("host-a", "/srv/one/甲/同名.txt"),
        remote("host-a", "/srv/two/甲/同名.txt"),
    ];

    assert_eq!(titles(&endpoints), ["one/甲/同名.txt", "two/甲/同名.txt"]);
}

#[test]
/// 相同远端路径位于不同主机时补充稳定主机标识，不把文件路径误当作唯一目标。
///
/// @author kongweiguang
fn identical_remote_paths_include_host_identity() {
    let endpoints = [
        remote("host-a", "/srv/shared/同名.txt"),
        remote("host-b", "/srv/shared/同名.txt"),
    ];
    let labels = titles(&endpoints);

    assert!(labels[0].contains("host-a"));
    assert!(labels[1].contains("host-b"));
    assert!(!labels[0].contains("host-b"));
    assert!(!labels[1].contains("host-a"));
    assert_ne!(labels[0], labels[1]);
}

#[test]
/// 无障碍目标名称同时保留精确主机身份和完整远端路径。
///
/// @author kongweiguang
fn target_accessibility_label_contains_host_and_full_path() {
    let endpoint = remote("prod-west", "/srv/project/甲/同名.txt");
    let label = endpoint_target_label(&endpoint, &FileTargetNameState::default());

    assert!(label.contains("prod-west"));
    assert!(label.contains("/srv/project/甲/同名.txt"));
}

#[test]
/// 编辑器标题只显示连接名与文件名，并仅移除应用添加的 SSH 前缀，不改写保存名称。
///
/// @author kongweiguang
fn remote_editor_context_keeps_saved_name_and_shows_basename() {
    let endpoint = remote("prod-west", "/srv/project/甲/同名.txt");
    let label = editor_visible_context_label(&endpoint, "SSH · SSH · r41（隔离）");

    assert_eq!(label, "SSH · r41（隔离） · 同名.txt");
    assert!(!label.contains("/srv/project/甲"));
    assert!(!label.contains("prod-west"));

    let root_file = remote("prod-west", "/同名.txt");
    assert_eq!(
        editor_visible_context_label(&root_file, "SSH · SSH · r41（隔离）"),
        "SSH · r41（隔离） · 同名.txt"
    );
}

#[cfg(windows)]
#[test]
/// Windows 本地路径在必须完整区分时保留原始盘符，不猜测或重写根目录。
///
/// @author kongweiguang
fn windows_paths_keep_their_drive_when_required() {
    let endpoints = [
        FileEndpoint::Local {
            path: PathBuf::from(r"C:\work\同名.txt"),
        },
        FileEndpoint::Local {
            path: PathBuf::from(r"D:\work\同名.txt"),
        },
    ];
    let labels = titles(&endpoints);

    assert!(labels[0].contains(r"C:\work\同名.txt"));
    assert!(labels[1].contains(r"D:\work\同名.txt"));
}
