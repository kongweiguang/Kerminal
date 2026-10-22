//! 连接面板详情与独立管理目录的展示模式回归。
//!
//! @author kongweiguang

use kerminal_application::connections::management::{
    HostAuthenticationMode, HostGroup, HostGroupSnapshot, HostRouteSettings, HostSftpOptions,
    HostTerminalOptions, ManagedHost, RdpDisplayOptions,
};
use kerminal_domain::{
    connections::{CredentialRef, HostEndpoint, RemoteProtocol, SshAuthentication},
    files::FileEndpoint,
    identity::HostId,
    preferences::ConfigRevision,
};

use super::{
    ConflictReloadTarget, ConnectionsEvent, ConnectionsPanel, ConnectionsPresentation,
    GroupSaveReconciliation, HostCredentialRecoveryAction, HostSaveReadback,
    HostSaveReadbackDecision, HostSnapshotReadGeneration, PendingAction, PendingHostSaveRecovery,
    SavedSelectionTarget, apply_host_snapshot_read_if_current, conflict_editor_reload_enabled,
    conflict_reload_blocks_editor_mutation, conflict_reload_response_matches,
    group_save_reconciliation_after_acknowledgement, group_save_reconciliation_after_readback,
    group_save_review_facts, host_draft_from_managed_host, host_editor_replacement_blocked,
    host_rows_contain_protocol, host_save_readback_decision, host_save_readback_matches_pending,
    host_save_readback_summary, host_sftp_capable, saved_selection_event,
    sftp_host_creation_saved_event, should_continue_local_host_action_after_discard,
    unified_editor_requires_discard, upsert_saved_host_row,
};

/// 粘贴密钥在 Vault 写入前没有引用；预校验必须同时考虑待写入材料与明确清除意图。
/// @author kongweiguang
#[test]
fn private_key_save_accepts_paste_and_rejects_cleared_saved_material() {
    use kerminal_application::connections::{
        credentials::SecretValue,
        management::{HostCredentialChange, HostCredentialSlot},
    };

    let empty = SshAuthentication::PrivateKey {
        path: None,
        secret: None,
        passphrase: None,
    };
    let pasted = [HostCredentialChange::Replace {
        slot: HostCredentialSlot::TargetPrivateKey,
        secret: SecretValue::new("fixture private key".to_owned()),
    }];
    assert!(super::private_key_save_has_material(Some(&empty), &pasted));
    assert!(!super::private_key_save_has_material(Some(&empty), &[]));

    let saved = SshAuthentication::PrivateKey {
        path: None,
        secret: Some(CredentialRef::new("vault/fixture-key-ref").unwrap()),
        passphrase: None,
    };
    assert!(super::private_key_save_has_material(Some(&saved), &[]));
    let cleared = [HostCredentialChange::Clear {
        slot: HostCredentialSlot::TargetPrivateKey,
    }];
    assert!(!super::private_key_save_has_material(
        Some(&saved),
        &cleared
    ));

    let file = SshAuthentication::PrivateKey {
        path: Some(std::path::PathBuf::from("fixture-key")),
        secret: None,
        passphrase: None,
    };
    assert!(super::private_key_save_has_material(Some(&file), &cleared));
    let passphrase = [HostCredentialChange::Replace {
        slot: HostCredentialSlot::TargetKeyPassphrase,
        secret: SecretValue::new("fixture passphrase".to_owned()),
    }];
    assert!(!super::private_key_save_has_material(
        Some(&empty),
        &passphrase
    ));
}

/// 关闭统一表单必须汇总远程与本地主机缓存草稿，远程未知结果也不能静默丢弃。
/// @author kongweiguang
#[test]
fn unified_editor_discard_gate_aggregates_both_cached_forms_and_unknown_remote_save() {
    assert!(unified_editor_requires_discard(true, false, false));
    assert!(unified_editor_requires_discard(false, true, false));
    assert!(unified_editor_requires_discard(false, false, true));
    assert!(!unified_editor_requires_discard(false, false, false));
}

/// 本地脏导航的确认继续待执行动作；统一表单取消没有导航动作时必须关闭全部编辑缓存。
/// @author kongweiguang
#[test]
fn discard_continues_only_an_active_local_navigation_action() {
    assert!(should_continue_local_host_action_after_discard(true, true));
    assert!(!should_continue_local_host_action_after_discard(
        true, false
    ));
    assert!(!should_continue_local_host_action_after_discard(
        false, true
    ));
}

/// 构造有稳定 ID/revision 的主机快照，覆盖真实目录替换与新建两种回执形态。
///
/// @author kongweiguang
fn managed_host_snapshot(id: HostId, name: &str, revision: &str) -> ManagedHost {
    ManagedHost {
        id,
        group_id: None,
        name: name.to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "sftp.example.test".to_owned(),
            port: 22,
            username: "tester".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: None,
        terminal_options: HostTerminalOptions::default(),
        sftp_options: HostSftpOptions::default(),
        rdp_display: RdpDisplayOptions::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256(revision.to_owned()),
    }
}

/// 驱动与面板回调相同的代次门禁：保存注入行后旧回执被丢弃，新读取才可替换目录快照。
/// 覆盖插入新 HostId 与更新既有 HostId，避免只验证代次相等比较而遗漏列表回写。
///
/// @author kongweiguang
#[test]
fn confirmed_host_save_rejects_stale_directory_read_before_fresh_read() {
    for (scenario, host_id, saved_name, saved_revision, old_name) in [
        (
            "new host",
            HostId::new(),
            "newly saved",
            "new-revision",
            None,
        ),
        (
            "updated host",
            HostId::new(),
            "updated name",
            "updated-revision",
            Some("before save"),
        ),
    ] {
        let stale_rows = old_name
            .map(|name| vec![managed_host_snapshot(host_id.clone(), name, "old-revision")])
            .unwrap_or_default();
        let mut generation = HostSnapshotReadGeneration::default();
        let old_read_generation = generation.capture();
        let mut rows = stale_rows.clone();
        let current_groups = vec![HostGroup {
            id: "current-groups".to_owned(),
            name: "Current groups".to_owned(),
            sort_order: 0,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        }];
        let stale_groups = vec![HostGroup {
            id: "stale-groups".to_owned(),
            name: "Stale groups".to_owned(),
            sort_order: 0,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        }];
        let mut groups = current_groups.clone();
        let current_group_revision = Some(ConfigRevision::from_sha256("current-groups".to_owned()));
        let stale_group_revision = Some(ConfigRevision::from_sha256("stale-groups".to_owned()));
        let mut group_revision = current_group_revision.clone();

        // finish_save 的成功路径调用这两个生产 helper，再发回投事件并请求刷新。
        generation.invalidate_after_confirmed_save();
        let saved_host = managed_host_snapshot(host_id.clone(), saved_name, saved_revision);
        upsert_saved_host_row(&mut rows, saved_host.clone());

        assert!(
            !apply_host_snapshot_read_if_current(
                &generation,
                old_read_generation,
                &mut rows,
                &mut groups,
                &mut group_revision,
                stale_rows,
                HostGroupSnapshot {
                    groups: stale_groups,
                    revision: stale_group_revision,
                },
            ),
            "{scenario}: pre-save read must be discarded"
        );
        assert_eq!(
            rows,
            vec![saved_host.clone()],
            "{scenario}: saved row survives"
        );
        assert_eq!(
            groups, current_groups,
            "{scenario}: stale groups are discarded"
        );
        assert_eq!(group_revision, current_group_revision);

        let fresh_read_generation = generation.capture();
        let fresh_groups = vec![HostGroup {
            id: "fresh-groups".to_owned(),
            name: "Fresh groups".to_owned(),
            sort_order: 0,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        }];
        let fresh_group_revision = Some(ConfigRevision::from_sha256("fresh-groups".to_owned()));
        assert!(apply_host_snapshot_read_if_current(
            &generation,
            fresh_read_generation,
            &mut rows,
            &mut groups,
            &mut group_revision,
            vec![saved_host.clone()],
            HostGroupSnapshot {
                groups: fresh_groups.clone(),
                revision: fresh_group_revision.clone(),
            },
        ));
        assert_eq!(rows, vec![saved_host], "{scenario}: fresh read is applied");
        assert_eq!(groups, fresh_groups);
        assert_eq!(group_revision, fresh_group_revision);
    }
}

/// 菜单捕获目标只允许匹配当前快照中的同一主机与协议，重载后的协议变化必须拒绝。
/// @author kongweiguang
#[test]
fn connect_request_requires_current_host_protocol_match() {
    let host_id = HostId::new();
    let other_host_id = HostId::new();
    let missing_host_id = HostId::new();
    let rows = [
        (&host_id, RemoteProtocol::Ssh),
        (&other_host_id, RemoteProtocol::Telnet),
    ];

    assert!(host_rows_contain_protocol(
        rows.iter().copied(),
        &host_id,
        RemoteProtocol::Ssh
    ));
    assert!(!host_rows_contain_protocol(
        rows.iter().copied(),
        &host_id,
        RemoteProtocol::Telnet
    ));
    assert!(!host_rows_contain_protocol(
        rows.iter().copied(),
        &missing_host_id,
        RemoteProtocol::Ssh
    ));
}

/// SFTP 信任等待恢复的仍是原待处理动作，因此开始回执不会为同一请求生成新 ID。
/// @author kongweiguang
#[test]
fn sftp_started_event_preserves_request_id_when_pending_action_is_resumed() {
    let request_id = uuid::Uuid::new_v4().to_string();
    let action = PendingAction::Sftp {
        endpoint: FileEndpoint::remote(HostId::new(), "/")
            .expect("test remote endpoint should be valid"),
        request_id: request_id.clone(),
    };
    let resumed_action = action.clone();
    let expected = Some(ConnectionsEvent::SftpOpenStarted { request_id });

    assert_eq!(action.sftp_started_event(), expected);
    assert_eq!(resumed_action.sftp_started_event(), expected);
}

/// TransferTab 的 host-key 重试沿用同一请求身份，便于 Root 在用户切页后仅后台插入结果。
/// @author kongweiguang
#[test]
fn sftp_transfer_tab_started_event_preserves_request_id() {
    let request_id = uuid::Uuid::new_v4().to_string();
    let action = PendingAction::SftpTransferTab {
        request_id: request_id.clone(),
        endpoint: FileEndpoint::remote(HostId::new(), "/")
            .expect("test remote endpoint should be valid"),
        title: "Transfer".to_owned(),
    };

    assert_eq!(
        action.sftp_started_event(),
        Some(ConnectionsEvent::SftpTransferTabOpenStarted { request_id })
    );
}

/// 只有本次 SFTP picker 新建流程发成功回执；协议能力区分 SSH/SFTP 与其它保存结果。
/// @author kongweiguang
#[test]
fn sftp_host_creation_receipt_is_scoped_and_capability_checked() {
    let host_id = HostId::new();
    assert_eq!(
        sftp_host_creation_saved_event(None, host_id.clone(), RemoteProtocol::Sftp, true,),
        None
    );

    assert_eq!(
        sftp_host_creation_saved_event(
            Some("picker-flow".to_owned()),
            host_id.clone(),
            RemoteProtocol::Sftp,
            true,
        ),
        Some(ConnectionsEvent::SftpHostCreationSaved {
            request_id: "picker-flow".to_owned(),
            host_id: host_id.clone(),
            can_open_sftp: true,
        })
    );
    assert!(!host_sftp_capable(RemoteProtocol::Sftp, false));
    assert!(!host_sftp_capable(RemoteProtocol::Rdp, true));
    assert!(host_sftp_capable(RemoteProtocol::Ssh, true));
}

/// 主机选择始终对应详情展示；完整目录由左侧导航独立承载。
///
/// @author kongweiguang
#[test]
fn host_presentation_has_no_duplicate_directory_mode() {
    let mut presentation = ConnectionsPresentation::default();

    assert_eq!(presentation, ConnectionsPresentation::HostDetails);

    presentation.show_host_details();
    assert_eq!(presentation, ConnectionsPresentation::HostDetails);
}

/// 分组编辑不因主机选择而关闭，结束时统一回到详情态，由 Root generation 恢复原工作区。
///
/// @author kongweiguang
#[test]
fn group_rename_uses_full_width_mode_and_returns_to_details() {
    let mut presentation = ConnectionsPresentation::default();

    presentation.show_group_editor();
    assert!(presentation.shows_group_editor());
    presentation.show_host_details();
    assert!(presentation.shows_group_editor());
    presentation.finish_group_editor();
    assert_eq!(presentation, ConnectionsPresentation::HostDetails);

    presentation.show_group_editor();
    presentation.show_host_details();
    assert!(presentation.shows_group_editor());
    presentation.finish_group_editor();
    assert_eq!(presentation, ConnectionsPresentation::HostDetails);
}

/// 保存失败或旧组回执不得关闭当前草稿；只有同一重命名的成功回执结束编辑。
///
/// @author kongweiguang
#[test]
fn group_editor_failure_and_stale_receipt_preserve_current_draft() {
    assert!(!ConnectionsPanel::should_finish_group_editor(
        false,
        true,
        Some("servers"),
        Some("servers"),
        "Servers",
        "Servers"
    ));
    assert!(!ConnectionsPanel::should_finish_group_editor(
        true,
        true,
        Some("old-group"),
        Some("new-group"),
        "Old",
        "New"
    ));
    assert!(!ConnectionsPanel::should_finish_group_editor(
        true,
        false,
        Some("servers"),
        Some("servers"),
        "Servers",
        "Servers"
    ));
    assert!(ConnectionsPanel::should_finish_group_editor(
        true,
        true,
        None,
        None,
        "New group",
        "New group"
    ));
    assert!(!ConnectionsPanel::should_finish_group_editor(
        true,
        true,
        None,
        None,
        "Old draft",
        "Newer draft"
    ));
}

/// 普通或早期刷新的成功回执不能确认未知保存；显式核对失败仍保持门禁。
/// 用户确认只解除门禁，不证明上一次事务成功。
/// @author kongweiguang
#[test]
fn ordinary_or_early_reload_does_not_advance_unknown_group_save() {
    assert_eq!(
        group_save_reconciliation_after_readback(
            Some(GroupSaveReconciliation::NeedsReadback),
            false,
            false,
        ),
        Some(GroupSaveReconciliation::NeedsReadback)
    );
    assert_eq!(
        group_save_reconciliation_after_readback(
            Some(GroupSaveReconciliation::NeedsReadback),
            true,
            false,
        ),
        Some(GroupSaveReconciliation::NeedsReadback)
    );
    assert_eq!(
        group_save_reconciliation_after_readback(
            Some(GroupSaveReconciliation::NeedsReadback),
            false,
            true,
        ),
        Some(GroupSaveReconciliation::NeedsReadback)
    );
    assert_eq!(
        group_save_reconciliation_after_readback(
            Some(GroupSaveReconciliation::NeedsReadback),
            true,
            true,
        ),
        Some(GroupSaveReconciliation::AwaitingUserDecision)
    );
    assert_eq!(
        group_save_reconciliation_after_acknowledgement(
            Some(GroupSaveReconciliation::AwaitingUserDecision),
            false,
        ),
        Some(GroupSaveReconciliation::AwaitingUserDecision)
    );
    assert_eq!(
        group_save_reconciliation_after_acknowledgement(
            Some(GroupSaveReconciliation::AwaitingUserDecision),
            true,
        ),
        None
    );
}

/// 核对只显示与草稿有关的当前事实，长列表有界且不会将同名项判定为上次成功。
/// @author kongweiguang
#[test]
fn group_save_review_facts_are_relevant_and_bounded() {
    let groups = (0..5)
        .map(|index| HostGroup {
            id: format!("id-{index}"),
            name: if index < 4 { "Servers" } else { "Other" }.to_owned(),
            sort_order: index,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        })
        .collect::<Vec<_>>();

    let renamed = group_save_review_facts(&groups, Some("id-1"), "Servers");
    assert!(renamed.contains("id-1 · Servers"));
    assert!(!renamed.contains("id-2 · Servers"));

    let missing = group_save_review_facts(&groups, Some("removed"), "Servers");
    assert!(missing.contains("removed"));
    assert!(missing.contains("was not found") || missing.contains("未找到"));

    let created = group_save_review_facts(&groups, None, "servers");
    assert!(created.contains("id-0 · Servers"));
    assert!(created.contains("id-2 · Servers"));
    assert!(created.contains("1 more"));
    assert!(
        created.contains("not proof of prior save success")
            || created.contains("不代表上次保存成功")
    );

    let not_found = group_save_review_facts(&groups, None, "New group");
    assert!(not_found.contains("New group"));
    assert!(not_found.contains("No group named") || not_found.contains("未找到"));
}

/// 新建缺失或编辑 baseline revision 未变只允许保留草稿重试；候选和第三方版本分开处理。
/// @author kongweiguang
#[test]
fn unknown_host_save_readback_requires_explicit_evidence_before_retry_or_adoption() {
    let host_id = HostId::new();
    let baseline = managed_host_snapshot(host_id.clone(), "before", "baseline");
    let mut candidate = host_draft_from_managed_host(&baseline);
    candidate.name = "requested".to_owned();
    let edit_recovery = PendingHostSaveRecovery {
        host_id: host_id.clone(),
        candidate: candidate.clone(),
        baseline_revision: Some(baseline.revision.clone()),
        credential_changes: Vec::new(),
        credential_cleanup: None,
    };

    assert_eq!(
        host_save_readback_decision(&edit_recovery, Some(&baseline)),
        HostSaveReadbackDecision::ConfirmedUnsubmitted
    );

    let committed = managed_host_snapshot(host_id.clone(), "requested", "committed");
    assert_eq!(
        host_save_readback_decision(&edit_recovery, Some(&committed)),
        HostSaveReadbackDecision::ConfirmedCommitted
    );

    let third_party = managed_host_snapshot(host_id.clone(), "external", "external-revision");
    assert_eq!(
        host_save_readback_decision(&edit_recovery, Some(&third_party)),
        HostSaveReadbackDecision::RequiresAdoption
    );

    let new_id = HostId::new();
    let mut new_candidate = host_draft_from_managed_host(&managed_host_snapshot(
        new_id.clone(),
        "new host",
        "unused-baseline",
    ));
    new_candidate.id = Some(new_id.clone());
    let new_recovery = PendingHostSaveRecovery {
        host_id: new_id,
        candidate: new_candidate,
        baseline_revision: None,
        credential_changes: Vec::new(),
        credential_cleanup: None,
    };
    assert_eq!(
        host_save_readback_decision(&new_recovery, None),
        HostSaveReadbackDecision::ConfirmedUnsubmitted
    );
}

/// 即使私钥/密码引用变成新值，UI 也不能证明密文对应本次明文输入，必须让用户采用读回。
/// @author kongweiguang
#[test]
fn unknown_host_save_with_replaced_credential_requires_adoption() {
    let host_id = HostId::new();
    let old_reference = CredentialRef::new("vault/old-reference").unwrap();
    let new_reference = CredentialRef::new("vault/new-reference").unwrap();
    let mut baseline = managed_host_snapshot(host_id.clone(), "before", "baseline");
    baseline.authentication_mode = HostAuthenticationMode::Password;
    baseline.authentication = Some(SshAuthentication::Password {
        secret: old_reference,
    });
    let mut candidate = host_draft_from_managed_host(&baseline);
    candidate.name = "requested".to_owned();
    let recovery = PendingHostSaveRecovery {
        host_id: host_id.clone(),
        candidate,
        baseline_revision: Some(baseline.revision),
        credential_changes: vec![(
            kerminal_application::connections::management::HostCredentialSlot::TargetPassword,
            HostCredentialRecoveryAction::Replace,
        )],
        credential_cleanup: None,
    };
    let mut readback = managed_host_snapshot(host_id, "requested", "committed");
    readback.authentication_mode = HostAuthenticationMode::Password;
    readback.authentication = Some(SshAuthentication::Password {
        secret: new_reference,
    });

    assert_eq!(
        host_save_readback_decision(&recovery, Some(&readback)),
        HostSaveReadbackDecision::RequiresAdoption
    );
}

/// 恢复确认页只展示同 ID 最新读回中的普通连接字段，不把 vault 引用送进 UI 摘要。
/// @author kongweiguang
#[test]
fn unknown_host_save_readback_summary_is_generation_scoped_and_non_secret() {
    let host_id = HostId::new();
    let mut host = managed_host_snapshot(host_id.clone(), "readback host", "revision");
    host.note = Some("current note".to_owned());
    host.authentication_mode = HostAuthenticationMode::Password;
    host.authentication = Some(SshAuthentication::Password {
        secret: CredentialRef::new("vault/private-ref").unwrap(),
    });
    let readback = HostSaveReadback {
        generation: 9,
        host_id: host_id.clone(),
        host: Some(host),
        decision: HostSaveReadbackDecision::RequiresAdoption,
    };

    let summary = host_save_readback_summary(&readback, 9, &host_id).unwrap();
    assert!(summary.exists);
    assert_eq!(summary.host_id, host_id.to_string());
    assert_eq!(summary.name.as_deref(), Some("readback host"));
    assert_eq!(summary.protocol, Some(RemoteProtocol::Ssh));
    assert_eq!(summary.host.as_deref(), Some("sftp.example.test"));
    assert_eq!(summary.port, Some(22));
    assert_eq!(summary.username.as_deref(), Some("tester"));
    assert_eq!(summary.note.as_deref(), Some("current note"));
    assert!(!format!("{summary:?}").contains("vault/private-ref"));
    assert!(host_save_readback_summary(&readback, 8, &host_id).is_none());
    assert!(host_save_readback_summary(&readback, 9, &HostId::new()).is_none());

    let missing = HostSaveReadback {
        generation: 10,
        host_id: host_id.clone(),
        host: None,
        decision: HostSaveReadbackDecision::RequiresAdoption,
    };
    let missing_summary = host_save_readback_summary(&missing, 10, &host_id).unwrap();
    assert!(!missing_summary.exists);
    assert!(missing_summary.name.is_none());
}

/// 只有同 HostId 的最新显式读回回执可改变恢复状态，入口覆盖必须继续被 unknown 门禁拦截。
/// @author kongweiguang
#[test]
fn unknown_host_save_blocks_replacement_and_rejects_late_readback() {
    let host_id = HostId::new();
    let recovery = PendingHostSaveRecovery {
        host_id: host_id.clone(),
        candidate: host_draft_from_managed_host(&managed_host_snapshot(
            host_id.clone(),
            "candidate",
            "baseline",
        )),
        baseline_revision: None,
        credential_changes: Vec::new(),
        credential_cleanup: None,
    };

    assert!(host_editor_replacement_blocked(false, true, true));
    assert!(host_editor_replacement_blocked(true, false, false));
    assert!(!host_editor_replacement_blocked(false, false, false));
    assert!(host_save_readback_matches_pending(
        8,
        8,
        true,
        Some(&recovery),
        &host_id,
    ));
    assert!(!host_save_readback_matches_pending(
        7,
        8,
        true,
        Some(&recovery),
        &host_id,
    ));
    assert!(!host_save_readback_matches_pending(
        8,
        8,
        false,
        Some(&recovery),
        &host_id,
    ));
    assert!(!host_save_readback_matches_pending(
        8,
        8,
        true,
        Some(&recovery),
        &HostId::new(),
    ));
}

/// 冲突重读只在存在稳定目标且无保存、探测或其它事务时开放。
/// @author kongweiguang
#[test]
fn conflicted_editor_reload_gate_requires_current_conflict_target_and_idle_state() {
    assert!(conflict_editor_reload_enabled(true, true, false));
    assert!(!conflict_editor_reload_enabled(false, true, false));
    assert!(!conflict_editor_reload_enabled(true, false, false));
    assert!(!conflict_editor_reload_enabled(true, true, true));
}

/// 确认或读取阶段禁止其它动作，以免有效的冲突回执覆盖后来输入。
/// @author kongweiguang
#[test]
fn conflicted_editor_reload_blocks_mutations_until_cancelled_or_finished() {
    assert!(!conflict_reload_blocks_editor_mutation(false, false));
    assert!(conflict_reload_blocks_editor_mutation(true, false));
    assert!(conflict_reload_blocks_editor_mutation(false, true));
    assert!(conflict_reload_blocks_editor_mutation(true, true));
}

/// 远程冲突读取只接受仍绑定原 HostId 的同代回执。
/// @author kongweiguang
#[test]
fn conflicted_editor_reload_rejects_late_or_retargeted_remote_readback() {
    let host_id = HostId::new();
    let target = ConflictReloadTarget::Remote(host_id.clone());
    assert!(conflict_reload_response_matches(
        4,
        4,
        Some(&target),
        &ConflictReloadTarget::Remote(host_id.clone()),
    ));
    assert!(!conflict_reload_response_matches(
        3,
        4,
        Some(&target),
        &ConflictReloadTarget::Remote(host_id.clone()),
    ));
    assert!(!conflict_reload_response_matches(
        4,
        4,
        Some(&target),
        &ConflictReloadTarget::Remote(HostId::new()),
    ));
    assert!(!conflict_reload_response_matches(
        4,
        4,
        None,
        &ConflictReloadTarget::Remote(host_id),
    ));
}

/// 保存确认事件携带精确远端或本地稳定 ID，Root 据此选择对应导航行。
/// @author kongweiguang
#[test]
fn saved_selection_event_preserves_host_and_profile_identity() {
    let host_id = HostId::new();
    assert_eq!(
        saved_selection_event(SavedSelectionTarget::Host(host_id.clone())),
        ConnectionsEvent::HostSaved { host_id },
    );
    assert_eq!(
        saved_selection_event(SavedSelectionTarget::LocalProfile("profile-1".to_owned())),
        ConnectionsEvent::LocalProfileSaved {
            profile_id: "profile-1".to_owned(),
        },
    );
}
