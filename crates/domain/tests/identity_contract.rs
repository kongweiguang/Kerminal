//! 不透明标识的持久化兼容契约。
//!
//! @author kongweiguang

use kerminal_domain::identity::{AgentSessionId, HostId, PaneId, SessionId, TabId, TransferId};

/// 旧主机 ID 保持原样解析，路径转义由文件仓储而非领域 ID 类型负责。
#[test]
fn parses_opaque_ids_without_uuid_or_path_normalization() {
    let original = "legacy:group/host\\alias";
    let host_id = HostId::parse(original).expect("opaque id should remain readable");

    assert_eq!(host_id.as_str(), original);
    assert_eq!(host_id.to_string(), original);
}

/// 所有会话、视图和传输 ID 都以 JSON 字符串表示，保持已有数据结构的契约。
#[test]
fn all_identity_types_roundtrip_as_strings() {
    let ids = [
        serde_json::to_string(&SessionId::new()).unwrap(),
        serde_json::to_string(&PaneId::new()).unwrap(),
        serde_json::to_string(&TabId::new()).unwrap(),
        serde_json::to_string(&AgentSessionId::new()).unwrap(),
        serde_json::to_string(&TransferId::new()).unwrap(),
    ];

    assert!(
        ids.iter()
            .all(|value| value.starts_with('"') && value.ends_with('"'))
    );
    assert!(serde_json::from_str::<SessionId>(&ids[0]).is_ok());
}

/// 只拒绝无法安全传递的空值或控制字符，不损失合法历史标识。
#[test]
fn rejects_empty_and_control_characters() {
    assert!(HostId::parse("").is_err());
    assert!(HostId::parse("host\nname").is_err());
}
