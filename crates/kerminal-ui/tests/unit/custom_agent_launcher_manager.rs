// @author kongweiguang

use kerminal_application::preferences::{AgentLauncherSettings, CustomAgentDefinition};

use super::{
    CustomAgentManagerWriteOutcome, RECOVERY_IDLE, RECOVERY_PREFERENCES_REVIEW, RECOVERY_READBACK,
    RECOVERY_RETRY, launcher_key, manager_closes_after_confirmation, manager_save_disabled,
    recovery_flags_for_outcome, remove_custom_agent, requires_readback_status, upsert_custom_agent,
    validate_custom_agent_fields,
};

/// 写入失败自动触发读回；读回本身失败则必须经 Preferences 人工核对。
///
/// @author kongweiguang
#[test]
fn failed_manager_outcomes_require_readback_before_retry() {
    let failures = [
        CustomAgentManagerWriteOutcome::Failed("write failed".to_owned()),
        CustomAgentManagerWriteOutcome::Conflict("conflict".to_owned()),
        CustomAgentManagerWriteOutcome::OutcomeUnknown("unknown".to_owned()),
    ];

    for failure in failures {
        assert_eq!(recovery_flags_for_outcome(&failure), RECOVERY_READBACK);
    }

    let status = requires_readback_status("pre-submit gate rejected".to_owned());
    assert!(status.contains("读回") || status.contains("Read back"));
    assert!(status.contains("重新提交") || status.contains("resubmitting"));
    assert_eq!(
        recovery_flags_for_outcome(&CustomAgentManagerWriteOutcome::ReadBackFailed(
            "readback failed".to_owned(),
        )),
        RECOVERY_PREFERENCES_REVIEW
    );
}

/// 只有真实读回结果允许重提且偏好状态已清洁，才开放表单再次保存。
///
/// @author kongweiguang
#[test]
fn readback_controls_retry_or_preferences_review() {
    assert_eq!(
        recovery_flags_for_outcome(&CustomAgentManagerWriteOutcome::ReadBack {
            settings: settings_with("Pi", "pi"),
            retry_allowed: true,
        }),
        RECOVERY_RETRY
    );
    assert_eq!(
        recovery_flags_for_outcome(&CustomAgentManagerWriteOutcome::ReadBack {
            settings: settings_with("Pi", "pi"),
            retry_allowed: false,
        }),
        RECOVERY_PREFERENCES_REVIEW
    );
}

/// Root 对删除事务返回 keep-open 回执时，设置基线更新但管理器不能被关闭。
///
/// @author kongweiguang
#[test]
fn keep_open_receipt_preserves_manager_and_requires_no_readback() {
    assert_eq!(
        recovery_flags_for_outcome(&CustomAgentManagerWriteOutcome::ConfirmedKeepOpen(
            settings_with("Pi", "pi"),
        )),
        RECOVERY_IDLE
    );
    assert!(!manager_closes_after_confirmation(true));
    assert!(manager_closes_after_confirmation(false));
}

/// 结构基线不依赖外部配置文件，确保唯一性和平台字符上限遵循应用层契约。
///
/// @author kongweiguang
fn settings_with(name: &str, command: &str) -> AgentLauncherSettings {
    AgentLauncherSettings {
        selected_agent_key: "builtin:codex".to_owned(),
        custom_agents: vec![CustomAgentDefinition {
            id: "5c9bf7f4-f9b9-44c0-9260-fbc235d3026c".to_owned(),
            name: name.to_owned(),
            command: command.to_owned(),
        }],
    }
}

/// 名称唯一性采用 application 的 Unicode 小写规则，空白和长度上限在提交前拒绝。
///
/// @author kongweiguang
#[test]
fn validates_name_command_and_case_insensitive_uniqueness() {
    let settings = settings_with("Pi", "pi");
    assert!(validate_custom_agent_fields(&settings, None, " pi ", "pi").is_err());
    let unicode_name = settings_with("Ägent", "agent");
    assert!(validate_custom_agent_fields(&unicode_name, None, "äGENT", "agent").is_err());
    assert!(validate_custom_agent_fields(&settings, None, "New", "\t ").is_err());
    assert!(validate_custom_agent_fields(&settings, None, &"n".repeat(65), "pi").is_err());
    assert!(validate_custom_agent_fields(&settings, None, "New", &"x".repeat(4097)).is_err());
    assert!(
        validate_custom_agent_fields(
            &settings,
            Some("5c9bf7f4-f9b9-44c0-9260-fbc235d3026c"),
            " pi ",
            "pi"
        )
        .is_ok()
    );

    let mut full = settings_with("Pi", "pi");
    full.custom_agents = (0..32)
        .map(|index| CustomAgentDefinition::new(format!("Agent {index}"), "agent".to_owned()))
        .collect();
    assert!(validate_custom_agent_fields(&full, None, "Agent 32", "agent").is_err());
}

/// 空表单不能提交；有效表单只受写入和设置核对状态限制。
///
/// @author kongweiguang
#[test]
fn save_button_requires_valid_editor_and_respects_recovery_gates() {
    let settings = settings_with("Pi", "pi");
    let empty_fields_valid = validate_custom_agent_fields(&settings, None, "", "").is_ok();
    let valid_fields = validate_custom_agent_fields(&settings, None, "New Agent", "codex").is_ok();

    assert!(manager_save_disabled(
        empty_fields_valid,
        false,
        false,
        false
    ));
    assert!(!manager_save_disabled(valid_fields, false, false, false));
    assert!(manager_save_disabled(true, true, false, false));
    assert!(manager_save_disabled(true, false, true, false));
    assert!(manager_save_disabled(true, false, false, true));
}

/// 编辑沿用 UUID，新增/编辑项成为提交选择；删除当前选择项时回退 Codex。
///
/// @author kongweiguang
#[test]
fn upsert_preserves_identity_and_deleting_selection_falls_back_to_codex() {
    let mut settings = settings_with("Pi", "pi");
    let id = upsert_custom_agent(
        &mut settings,
        Some("5c9bf7f4-f9b9-44c0-9260-fbc235d3026c"),
        "Pi Local".to_owned(),
        "pi --model local".to_owned(),
    )
    .expect("existing custom Agent can be edited");
    assert_eq!(id, "5c9bf7f4-f9b9-44c0-9260-fbc235d3026c");
    assert_eq!(settings.selected_agent_key, format!("custom:{id}"));
    assert_eq!(settings.custom_agents[0].name, "Pi Local");
    assert!(launcher_key(&settings.selected_agent_key).is_some());
    assert!(launcher_key("custom:not-a-uuid").is_none());
    assert!(remove_custom_agent(&mut settings, &id));
    assert_eq!(settings.selected_agent_key, "builtin:codex");
    assert!(!remove_custom_agent(&mut settings, &id));
}

/// 删除非当前 launcher 不应改变选择，保证列表维护不会意外切换后续启动器。
///
/// @author kongweiguang
#[test]
fn removing_unselected_custom_agent_preserves_launcher_selection() {
    let mut settings = settings_with("Pi", "pi");
    settings.selected_agent_key = "builtin:claude".to_owned();
    assert!(remove_custom_agent(
        &mut settings,
        "5c9bf7f4-f9b9-44c0-9260-fbc235d3026c"
    ));
    assert_eq!(settings.selected_agent_key, "builtin:claude");
    assert!(settings.custom_agents.is_empty());
}

/// 每次保存或删除只改目标定义，列表里的其他已保存 Agent 必须原样保留。
///
/// @author kongweiguang
#[test]
fn saving_and_deleting_one_agent_preserves_other_definitions() {
    let target_id = "5c9bf7f4-f9b9-44c0-9260-fbc235d3026c";
    let mut settings = settings_with("Pi", "pi");
    settings.custom_agents.push(CustomAgentDefinition {
        id: "a7d9fbc6-0a46-4d70-996e-791640a98a67".to_owned(),
        name: "Claude".to_owned(),
        command: "claude".to_owned(),
    });

    assert_eq!(
        upsert_custom_agent(
            &mut settings,
            Some(target_id),
            "Pi Local".to_owned(),
            "pi --model local".to_owned(),
        )
        .expect("editing one definition should succeed"),
        target_id
    );
    assert_eq!(settings.custom_agents.len(), 2);
    assert_eq!(settings.custom_agents[1].name, "Claude");
    assert_eq!(settings.custom_agents[1].command, "claude");

    assert!(remove_custom_agent(&mut settings, target_id));
    assert_eq!(settings.custom_agents.len(), 1);
    assert_eq!(settings.custom_agents[0].name, "Claude");
    assert_eq!(settings.custom_agents[0].command, "claude");
    assert_eq!(settings.selected_agent_key, "builtin:codex");
}
