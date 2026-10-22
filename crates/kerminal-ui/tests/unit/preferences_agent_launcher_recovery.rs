//! 自定义 Agent Preferences 读回收据必须区分真实提交与同步拒绝，并核对完整基线。
//!
//! @author kongweiguang

use super::{
    AgentLauncherReadBackReceipt, AgentLauncherRecovery, agent_launcher_readback_confirms_write,
    agent_launcher_readback_preserves_baseline, agent_launcher_readback_recovery,
    readback_matches_clean_preferences, stale_agent_launcher_change,
};
use crate::features::preferences::DraftStatus;
use kerminal_application::{
    PublicErrorCode,
    preferences::{CustomAgentDefinition, SettingsSnapshot, VersionedSettings},
};
use kerminal_domain::preferences::ConfigRevision;

/// 读回与同步拒绝前的旧值相同，不能伪装成一次已写入请求的成功回执。
///
/// @author kongweiguang
#[test]
fn readback_confirms_only_a_write_that_was_actually_submitted() {
    let settings = kerminal_application::preferences::SettingsSnapshot::default();
    let recovery = AgentLauncherRecovery {
        baseline: settings.clone(),
        baseline_revision: Some(ConfigRevision::from_sha256("baseline".to_owned())),
        submitted: settings.clone(),
        submitted_write: false,
        failure_code: PublicErrorCode::Invalid,
        readback: None,
    };

    assert!(!agent_launcher_readback_confirms_write(
        &recovery,
        &settings.agent_launcher
    ));

    let submitted = AgentLauncherRecovery {
        submitted_write: true,
        ..recovery
    };
    assert!(agent_launcher_readback_confirms_write(
        &submitted,
        &settings.agent_launcher
    ));

    let mut different = settings.agent_launcher.clone();
    different.selected_agent_key = "builtin:claude".to_owned();
    assert!(!agent_launcher_readback_confirms_write(
        &submitted, &different
    ));
}

/// 只接受无并行编辑且完整值、revision 都等于真实读回的 clean Ready 草稿。
///
/// @author kongweiguang
#[test]
fn retry_requires_exact_full_settings_and_revision() {
    let settings = kerminal_application::preferences::SettingsSnapshot::default();
    let revision = Some(ConfigRevision::from_sha256("current".to_owned()));
    let readback = VersionedSettings {
        settings: settings.clone(),
        revision: revision.clone(),
    };

    assert!(readback_matches_clean_preferences(
        &settings,
        &revision,
        DraftStatus::Ready,
        false,
        &readback,
    ));
    assert!(!readback_matches_clean_preferences(
        &settings,
        &revision,
        DraftStatus::Ready,
        true,
        &readback,
    ));
    assert!(!readback_matches_clean_preferences(
        &settings,
        &revision,
        DraftStatus::Failed,
        false,
        &readback,
    ));

    let mut parallel_change = settings.clone();
    parallel_change.window_opacity = 41;
    assert!(!readback_matches_clean_preferences(
        &parallel_change,
        &revision,
        DraftStatus::Ready,
        false,
        &readback,
    ));

    let changed_revision = Some(ConfigRevision::from_sha256("later".to_owned()));
    assert!(!readback_matches_clean_preferences(
        &settings,
        &changed_revision,
        DraftStatus::Ready,
        false,
        &readback,
    ));
}

/// 原始 TOML 注释变化会更换 revision，即使 watcher 解析出的 Settings 值未变也必须阻止旧管理器写入。
///
/// @author kongweiguang
#[test]
fn comment_only_revision_change_rejects_the_open_manager_baseline() {
    let settings = SettingsSnapshot::default();
    let baseline = VersionedSettings {
        settings: settings.clone(),
        revision: Some(ConfigRevision::from_sha256("opened".to_owned())),
    };
    let current = VersionedSettings {
        settings,
        revision: Some(ConfigRevision::from_sha256("comment-only-edit".to_owned())),
    };
    let mut requested = baseline.settings.agent_launcher.clone();
    requested.selected_agent_key = "builtin:claude".to_owned();

    let recovery = stale_agent_launcher_change(&baseline, &current, &requested)
        .expect("raw revision changes must invalidate the manager baseline");

    assert_eq!(recovery.baseline, baseline.settings);
    assert_eq!(recovery.baseline_revision, baseline.revision);
    assert_eq!(recovery.submitted.agent_launcher, requested);
    assert!(!recovery.submitted_write);
    assert_eq!(recovery.failure_code, PublicErrorCode::Conflict);
}

/// 外部已改动 Agent 目录时同时保留打开基线和用户请求，并禁止读回后以旧草稿覆盖外部定义。
///
/// @author kongweiguang
#[test]
fn external_agent_change_is_preserved_and_blocks_retry() {
    let baseline_settings = SettingsSnapshot::default();
    let baseline = VersionedSettings {
        settings: baseline_settings.clone(),
        revision: Some(ConfigRevision::from_sha256("opened".to_owned())),
    };
    let mut current = baseline.clone();
    let external_agent = CustomAgentDefinition::new("外部 Agent", "claude");
    current
        .settings
        .agent_launcher
        .custom_agents
        .push(external_agent.clone());
    current.settings.agent_launcher.selected_agent_key = format!("custom:{}", external_agent.id);
    current.revision = Some(ConfigRevision::from_sha256(
        "external-agent-edit".to_owned(),
    ));

    let requested_agent = CustomAgentDefinition::new("本地草稿 Agent", "codex");
    let mut requested = baseline_settings.agent_launcher.clone();
    requested.custom_agents.push(requested_agent.clone());
    requested.selected_agent_key = format!("custom:{}", requested_agent.id);
    let recovery = stale_agent_launcher_change(&baseline, &current, &requested)
        .expect("external Agent edits must retain a conflict recovery snapshot");

    assert_eq!(
        recovery.baseline.agent_launcher,
        baseline_settings.agent_launcher
    );
    assert_eq!(recovery.submitted.agent_launcher, requested);
    assert_eq!(recovery.failure_code, PublicErrorCode::Conflict);
    assert!(!recovery.submitted_write);
    assert!(!agent_launcher_readback_preserves_baseline(
        &recovery,
        &current.settings.agent_launcher
    ));
}

/// 基线未变时允许新提交；读回仅有非 Agent 偏好变化时仍可安全重试，且完整读回需与当前草稿一致。
///
/// @author kongweiguang
#[test]
fn unchanged_agent_baseline_allows_retry_after_unrelated_settings_change() {
    let baseline_settings = SettingsSnapshot::default();
    let baseline = VersionedSettings {
        settings: baseline_settings.clone(),
        revision: Some(ConfigRevision::from_sha256("opened".to_owned())),
    };
    let mut requested = baseline_settings.agent_launcher.clone();
    requested.selected_agent_key = "builtin:claude".to_owned();

    assert!(stale_agent_launcher_change(&baseline, &baseline, &requested).is_none());

    let mut submitted = baseline_settings.clone();
    submitted.agent_launcher = requested;
    let recovery = AgentLauncherRecovery {
        baseline: baseline_settings.clone(),
        baseline_revision: baseline.revision.clone(),
        submitted,
        submitted_write: true,
        failure_code: PublicErrorCode::OutcomeUnknown,
        readback: None,
    };
    let mut readback_settings = baseline_settings;
    readback_settings.window_opacity = 85;
    let readback = VersionedSettings {
        settings: readback_settings.clone(),
        revision: Some(ConfigRevision::from_sha256(
            "unrelated-preference-edit".to_owned(),
        )),
    };

    assert!(agent_launcher_readback_preserves_baseline(
        &recovery,
        &readback.settings.agent_launcher
    ));
    assert!(!agent_launcher_readback_confirms_write(
        &recovery,
        &readback.settings.agent_launcher
    ));
    assert!(readback_matches_clean_preferences(
        &readback.settings,
        &readback.revision,
        DraftStatus::Ready,
        false,
        &readback,
    ));
}

/// 同步拒绝可能没有来得及保存 recovery；首次读回应沿用窗口打开时的 A1 基线，而非 watcher 的 A2。
///
/// @author kongweiguang
#[test]
fn missing_recovery_keeps_opening_baseline_after_external_agent_reload() {
    let opening_agent = CustomAgentDefinition::new("A1", "codex --profile opening");
    let mut opening_settings = SettingsSnapshot::default();
    opening_settings.agent_launcher.custom_agents = vec![opening_agent.clone()];
    opening_settings.agent_launcher.selected_agent_key = format!("custom:{}", opening_agent.id);
    let baseline = VersionedSettings {
        settings: opening_settings.clone(),
        revision: Some(ConfigRevision::from_sha256("opened-A1".to_owned())),
    };

    let external_agent = CustomAgentDefinition::new("A2", "claude --model external");
    let mut reloaded_settings = opening_settings;
    reloaded_settings.agent_launcher.custom_agents = vec![external_agent.clone()];
    reloaded_settings.agent_launcher.selected_agent_key = format!("custom:{}", external_agent.id);
    let readback = VersionedSettings {
        settings: reloaded_settings,
        revision: Some(ConfigRevision::from_sha256("reloaded-A2".to_owned())),
    };

    let recovery = agent_launcher_readback_recovery(&baseline, None);

    assert_eq!(recovery.baseline, baseline.settings);
    assert_eq!(recovery.baseline_revision, baseline.revision);
    assert_eq!(recovery.submitted, baseline.settings);
    assert!(!recovery.submitted_write);

    let clean_preferences_accept_readback = readback_matches_clean_preferences(
        &readback.settings,
        &readback.revision,
        DraftStatus::Ready,
        false,
        &readback,
    );
    let retry_allowed = clean_preferences_accept_readback
        && agent_launcher_readback_preserves_baseline(&recovery, &readback.settings.agent_launcher);
    assert!(clean_preferences_accept_readback);
    assert!(!retry_allowed);
}

/// 已有写入结果未知时，读回不得用新打开参数覆盖原提交的 baseline、请求和核对收据。
///
/// @author kongweiguang
#[test]
fn existing_unknown_write_recovery_is_preserved_verbatim() {
    let old_baseline_agent = CustomAgentDefinition::new("提交前", "codex --profile old");
    let mut old_baseline = SettingsSnapshot::default();
    old_baseline.agent_launcher.custom_agents = vec![old_baseline_agent.clone()];
    old_baseline.agent_launcher.selected_agent_key = format!("custom:{}", old_baseline_agent.id);
    let baseline_revision = Some(ConfigRevision::from_sha256("original-baseline".to_owned()));

    let submitted_agent = CustomAgentDefinition::new("未知结果提交", "codex --profile new");
    let mut submitted = old_baseline.clone();
    submitted.agent_launcher.custom_agents = vec![submitted_agent.clone()];
    submitted.agent_launcher.selected_agent_key = format!("custom:{}", submitted_agent.id);

    let readback = AgentLauncherReadBackReceipt {
        versioned: VersionedSettings {
            settings: old_baseline.clone(),
            revision: baseline_revision.clone(),
        },
        retry_allowed: true,
    };
    let original_recovery = AgentLauncherRecovery {
        baseline: old_baseline.clone(),
        baseline_revision: baseline_revision.clone(),
        submitted: submitted.clone(),
        submitted_write: true,
        failure_code: PublicErrorCode::OutcomeUnknown,
        readback: Some(readback.clone()),
    };

    let replacement_agent = CustomAgentDefinition::new("后来打开", "pi --approve");
    let mut replacement_settings = SettingsSnapshot::default();
    replacement_settings.agent_launcher.custom_agents = vec![replacement_agent.clone()];
    replacement_settings.agent_launcher.selected_agent_key =
        format!("custom:{}", replacement_agent.id);
    let replacement_baseline = VersionedSettings {
        settings: replacement_settings,
        revision: Some(ConfigRevision::from_sha256("later-opening".to_owned())),
    };

    let preserved =
        agent_launcher_readback_recovery(&replacement_baseline, Some(original_recovery));

    assert_eq!(preserved.baseline, old_baseline);
    assert_eq!(preserved.baseline_revision, baseline_revision);
    assert_eq!(preserved.submitted, submitted);
    assert!(preserved.submitted_write);
    assert_eq!(preserved.failure_code, PublicErrorCode::OutcomeUnknown);
    let preserved_readback = preserved
        .readback
        .expect("existing recovery readback receipt must remain available");
    assert_eq!(preserved_readback.versioned, readback.versioned);
    assert_eq!(preserved_readback.retry_allowed, readback.retry_allowed);
}
