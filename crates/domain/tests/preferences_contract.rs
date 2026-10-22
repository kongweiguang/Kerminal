// @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::preferences::{ConfigDocumentPath, ConfigPathError};

/// 配置别名使用已约定的目录结构，并保持简单历史文件名原样。
#[test]
fn maps_document_kinds_to_existing_relative_paths() {
    assert_eq!(
        ConfigDocumentPath::Settings.relative_path().unwrap(),
        PathBuf::from("settings.toml")
    );
    assert_eq!(
        ConfigDocumentPath::Profile("default".into())
            .relative_path()
            .unwrap(),
        PathBuf::from("profiles").join("default.toml")
    );
    assert_eq!(
        ConfigDocumentPath::HostGroups.relative_path().unwrap(),
        PathBuf::from("hosts").join("groups.toml")
    );
    assert_eq!(
        ConfigDocumentPath::Host("host-1".into())
            .relative_path()
            .unwrap(),
        PathBuf::from("hosts").join("host-1.toml")
    );
    assert_eq!(
        ConfigDocumentPath::Snippet("snippet_1".into())
            .relative_path()
            .unwrap(),
        PathBuf::from("snippets").join("snippet_1.toml")
    );
    assert_eq!(
        ConfigDocumentPath::Workflow("workflow.1".into())
            .relative_path()
            .unwrap(),
        PathBuf::from("workflows").join("workflow.1.toml")
    );
}

/// 文件仓储单独拒绝路径分隔符和 reserved groups；opaque domain ID 本身不受此限制。
#[test]
fn rejects_path_escape_and_reserved_host_file_name() {
    assert_eq!(
        ConfigDocumentPath::Host("../escape".into()).relative_path(),
        Err(ConfigPathError::InvalidId)
    );
    assert_eq!(
        ConfigDocumentPath::Host("groups".into()).relative_path(),
        Err(ConfigPathError::ReservedHostId)
    );
    assert_eq!(
        ConfigDocumentPath::Profile(String::new()).relative_path(),
        Err(ConfigPathError::EmptyId)
    );
}
