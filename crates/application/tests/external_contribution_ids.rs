//! 外部贡献身份与旧配置共享入口，防止工具栏持久化截断命名空间。
//!
//! @author kongweiguang

use kerminal_application::{plugins::ToolId, preferences::ToolRailToolId};

/// 名称扩展不得改变旧工具映射，也不能在序列化时丢掉插件身份。
#[test]
fn namespaced_tool_identity_round_trips_without_changing_builtins() {
    for value in [
        "sftp",
        "example.notes",
        "acme.notes/panel",
        "acme.notes-/panel",
        "acme.notes-",
    ] {
        let identity = ToolRailToolId::from_id(value).expect("valid identity");
        assert_eq!(identity.as_str(), value);
        let encoded = serde_json::to_string(&identity).expect("serialize");
        let decoded: ToolRailToolId = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, identity);
    }
}

/// 长身份必须仍满足 publisher.plugin 语法，不能用越界长度掩盖无效的命名空间前缀。
#[test]
fn identity_bounds_reject_paths_and_preserve_long_contributions() {
    let longest = format!("{}.{}/{}", "a".repeat(39), "a".repeat(63), "b".repeat(63));
    assert_eq!(ToolRailToolId::from_id(&longest).unwrap().as_str(), longest);
    assert!(ToolRailToolId::from_id(&"a".repeat(193)).is_err());
    for value in [
        "/panel",
        "acme.notes/",
        "acme.notes/a/b",
        "acme.notes/../x",
        "a/..",
        "a/ b",
    ] {
        assert!(ToolId::new(value).is_err(), "{value}");
    }
}
