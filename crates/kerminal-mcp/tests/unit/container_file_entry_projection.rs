//! 容器目录项 MCP wire 字段兼容测试。
//!
//! @author kongweiguang

use kerminal_domain::files::{
    ContainerRuntime, FileEndpoint, FileEntry, FileEntryKind, RemotePath,
};
use serde_json::json;

use super::file_entry_value;

/// 当容器执行器没有协议原始行时，wire 仍显式返回 raw=null 以保留字段形状。
#[test]
fn missing_raw_line_is_projected_as_null() {
    let value = file_entry_value(file_entry(None));

    assert_eq!(value["raw"], serde_json::Value::Null);
    assert_eq!(value["name"], "app.conf");
    assert_eq!(value["kind"], "file");
}

/// 可用的 provider 原文原样通过 MCP projection，不由 mapper 合成或重排。
#[test]
fn available_raw_line_is_preserved() {
    let value = file_entry_value(file_entry(Some("-rw-r----- 1 root app 42 app.conf")));

    assert_eq!(value["raw"], json!("-rw-r----- 1 root app 42 app.conf"));
    assert!(value.get("isHidden").is_none());
}

/// 构造使用真实领域字段的容器目录项，raw 缺失保持 None。
fn file_entry(raw: Option<&str>) -> FileEntry {
    FileEntry {
        endpoint: FileEndpoint::Container {
            host_id: None,
            runtime: ContainerRuntime::Docker,
            container_id: "app".to_owned(),
            path: RemotePath::parse_container("/etc/app.conf").expect("fixture path is valid"),
        },
        name: "app.conf".to_owned(),
        kind: FileEntryKind::File,
        size: Some(42),
        modified: Some("2026-10-17T00:00:00Z".to_owned()),
        permissions: Some(0o640),
        is_hidden: false,
        raw: raw.map(ToOwned::to_owned),
    }
}
