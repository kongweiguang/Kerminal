//! 转发目标文案是 HostId 绑定之上的展示投影，不泄露内部 ID 到可见标签。
//!
//! @author kongweiguang

use super::{
    ForwardScenarioCard, forward_host_id_detail, forward_host_label, forward_kind_for_scenario,
    forward_scenario_card, forward_socks_kind,
};
use kerminal_domain::identity::HostId;
use kerminal_domain::operations::ForwardKind;

/// 规范化已有目标标签的 SSH 前缀，避免方向摘要再重复协议标识。
///
/// @author kongweiguang
#[test]
fn forward_host_label_strips_duplicate_ssh_prefix() {
    let host_id = HostId::new();

    assert_eq!(
        forward_host_label(&host_id, Some("  SSH · r41 (隔离)  ")),
        "r41 (隔离)"
    );
    assert_eq!(
        forward_host_label(&host_id, Some("Build host")),
        "Build host"
    );
}

/// 无有效保存名称时使用中性标签；HostId 只在专用身份详情中保留。
///
/// @author kongweiguang
#[test]
fn forward_host_label_hides_id_but_keeps_identity_detail() {
    let host_id = HostId::new();
    let id = host_id.as_str();
    let fallback = forward_host_label(&host_id, Some(id));

    assert_ne!(fallback, id);
    assert!(fallback.to_ascii_lowercase().contains("ssh"));
    assert_eq!(forward_host_label(&host_id, Some("  ")), fallback);
    assert!(forward_host_id_detail(&host_id).contains(id));
}

/// 三张操作卡仅是四种既有领域类型的展示分组，普通 SSH 方向保持一一映射。
///
/// @author kongweiguang
#[test]
fn forward_scenario_cards_group_existing_tunnel_kinds() {
    assert_eq!(
        forward_scenario_card(ForwardKind::Local),
        ForwardScenarioCard::HostService
    );
    assert_eq!(
        forward_scenario_card(ForwardKind::Remote),
        ForwardScenarioCard::LocalService
    );
    assert_eq!(
        forward_scenario_card(ForwardKind::Dynamic),
        ForwardScenarioCard::Socks
    );
    assert_eq!(
        forward_scenario_card(ForwardKind::RemoteDynamic),
        ForwardScenarioCard::Socks
    );
}

/// SOCKS 卡进入时默认本机 -D，用户已选远端 -R 后再次打开卡片则保留方向。
///
/// @author kongweiguang
#[test]
fn forward_scenario_selection_preserves_socks_direction() {
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::HostService, ForwardKind::Remote),
        ForwardKind::Local
    );
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::LocalService, ForwardKind::Local),
        ForwardKind::Remote
    );
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::Socks, ForwardKind::Remote),
        ForwardKind::Dynamic
    );
    assert_eq!(
        forward_kind_for_scenario(ForwardScenarioCard::Socks, ForwardKind::RemoteDynamic),
        ForwardKind::RemoteDynamic
    );
    assert_eq!(forward_socks_kind(false), ForwardKind::Dynamic);
    assert_eq!(forward_socks_kind(true), ForwardKind::RemoteDynamic);
}
