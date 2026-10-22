//! 持久本地主机字段投影与输入解析的无窗口回归。
//!
//! @author kongweiguang

use super::{
    LocalHostDefaultValues, LocalHostEditorLoadIntent, LocalHostEditorMode, LocalHostInputBaseline,
    LocalHostState, LocalHostUnknownMutation, LocalHostValidationField,
    local_host_default_readback, local_host_default_readback_can_release,
    local_host_default_target, local_host_needs_save, local_host_profile_revisions,
    local_host_validation_field, parse_profile_arguments, parse_profile_environment,
    project_explicit_local_defaults, project_untouched_local_defaults,
};
use kerminal_application::preferences::{ProfileRevision, VersionedProfile};
use kerminal_domain::preferences::ConfigRevision;
use kerminal_domain::preferences::TerminalProfile;
use std::collections::BTreeMap;

/// 构造只用于列表纯逻辑测试的最小稳定 Profile 快照。
fn profile_row(id: &str, name: &str, is_default: bool) -> VersionedProfile {
    VersionedProfile {
        profile: TerminalProfile {
            id: id.to_owned(),
            name: name.to_owned(),
            shell: "shell".to_owned(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            is_default,
            sidebar_group_id: None,
            sort_order: 0,
            created_at: "created".to_owned(),
            updated_at: "updated".to_owned(),
        },
        revision: ConfigRevision::from_sha256(format!("revision-{id}")),
    }
}

/// 给每个本地主机字段构造一致的原始输入快照，验证默认值异步到达时的保护规则。
fn baseline(name: &str, shell: &str, cwd: &str, args: &str, env: &str) -> LocalHostInputBaseline {
    LocalHostInputBaseline {
        name: name.to_owned(),
        shell: shell.to_owned(),
        cwd: cwd.to_owned(),
        args: args.to_owned(),
        env: env.to_owned(),
        group_id: None,
    }
}

#[test]
/// 默认目标晚于用户输入返回时，只填补仍未修改的对应字段，不污染名称基线。
fn late_default_result_only_projects_fields_unchanged_since_editor_opened() {
    let original = baseline("本地主机", "", "", "", "");
    let current = baseline("用户改过的名称", "", "", "--login\n--noprofile", "K=custom");
    let defaults = LocalHostDefaultValues {
        shell: "pwsh.exe".to_owned(),
        cwd: "C:\\work".to_owned(),
        args: "-NoLogo\n-NoProfile".to_owned(),
        env: "K=default\nTERM=xterm-256color".to_owned(),
    };

    let projection = project_untouched_local_defaults(&original, &current, defaults);

    assert_eq!(projection.shell.as_deref(), Some("pwsh.exe"));
    assert_eq!(projection.cwd.as_deref(), Some("C:\\work"));
    assert_eq!(projection.args, None);
    assert_eq!(projection.env, None);
    assert_eq!(projection.baseline.name, original.name);
    assert_eq!(projection.baseline.shell, "pwsh.exe");
    assert_eq!(projection.baseline.cwd, "C:\\work");
    assert_eq!(projection.baseline.args, original.args);
    assert_eq!(projection.baseline.env, original.env);
    assert_ne!(current.name, original.name);
}

#[test]
/// 未触碰的字段均可按平台默认值填充，保持多行参数及环境变量的原始语义。
fn untouched_default_fields_are_all_projected() {
    let original = baseline("Local", "", "", "", "");
    let projection = project_untouched_local_defaults(
        &original,
        &original,
        LocalHostDefaultValues {
            shell: "bash".to_owned(),
            cwd: "/workspace".to_owned(),
            args: "-l\n-i".to_owned(),
            env: "TERM=xterm".to_owned(),
        },
    );

    assert_eq!(projection.shell.as_deref(), Some("bash"));
    assert_eq!(projection.cwd.as_deref(), Some("/workspace"));
    assert_eq!(projection.args.as_deref(), Some("-l\n-i"));
    assert_eq!(projection.env.as_deref(), Some("TERM=xterm"));
}

/// 显式恢复覆盖请求时已有字段，但不覆盖请求发出后用户继续编辑的值。
/// @author kongweiguang
#[test]
fn explicit_default_request_overrides_prior_values_and_preserves_later_edits() {
    let initial = baseline("Local", "old-shell", "old-cwd", "old-args", "OLD=1");
    let expected = baseline(
        "Local",
        "custom-shell",
        "custom-cwd",
        "custom-args",
        "CUSTOM=1",
    );
    let current = baseline(
        "Local",
        "custom-shell",
        "newer-cwd",
        "custom-args",
        "CUSTOM=1",
    );
    let defaults = LocalHostDefaultValues {
        shell: "pwsh.exe".to_owned(),
        cwd: "C:\\work".to_owned(),
        args: "-NoLogo".to_owned(),
        env: "TERM=xterm".to_owned(),
    };

    let projection = project_explicit_local_defaults(&expected, &current, defaults, &initial);

    assert_eq!(projection.shell.as_deref(), Some("pwsh.exe"));
    assert_eq!(projection.cwd, None);
    assert_eq!(projection.args.as_deref(), Some("-NoLogo"));
    assert_eq!(projection.env.as_deref(), Some("TERM=xterm"));
    assert_eq!(projection.baseline, initial);
}

#[test]
/// 空参数编辑区表达零参数，显式空行仍代表一个空参数以保持现有表单兼容性。
fn argument_lines_preserve_explicit_empty_arguments() {
    assert_eq!(parse_profile_arguments(""), Vec::<String>::new());
    assert_eq!(
        parse_profile_arguments("--flag\n\nvalue"),
        vec!["--flag".to_owned(), "".to_owned(), "value".to_owned()]
    );
}

#[test]
/// 环境变量以首个等号拆分，值可继续包含等号，空行忽略且重复键拒绝。
fn environment_lines_split_once_and_reject_duplicate_keys() {
    let values = parse_profile_environment("TOKEN=a=b=c\n\nTERM=xterm").unwrap();
    assert_eq!(values.get("TOKEN").map(String::as_str), Some("a=b=c"));
    assert_eq!(values.get("TERM").map(String::as_str), Some("xterm"));
    assert!(parse_profile_environment("MISSING_SEPARATOR").is_err());
    assert!(parse_profile_environment("DUP=first\nDUP=second").is_err());
}

/// Profile 校验只聚焦已知字段，复合参数限制按原始启动参数决定聚焦终端页的哪一项。
/// @author kongweiguang
#[test]
fn local_profile_validation_targets_properties_or_terminal_fields() {
    assert_eq!(
        local_host_validation_field("profile 名称不能为空或过长", "", ""),
        Some(LocalHostValidationField::Name),
    );
    assert_eq!(
        local_host_validation_field("profile shell 路径不能为空或过长", "", ""),
        Some(LocalHostValidationField::Shell),
    );
    assert_eq!(
        local_host_validation_field("profile 参数或环境变量超出限制", "-NoLogo", ""),
        Some(LocalHostValidationField::Environment),
    );
    assert_eq!(
        local_host_validation_field(
            "profile 参数或环境变量超出限制",
            &"x".repeat(16 * 1024 + 1),
            "",
        ),
        Some(LocalHostValidationField::Arguments),
    );
    assert_eq!(
        local_host_validation_field("profile 参数或环境变量超出限制", "", &"x".repeat(4097)),
        Some(LocalHostValidationField::WorkingDirectory),
    );
    assert_eq!(
        local_host_validation_field("unmapped profile error", "", ""),
        None,
    );
}

#[test]
/// 创建与复制没有 revision，因此即使 pristine 也必须保存；干净编辑则不应制造无效写入。
fn create_and_duplicate_need_save_without_raw_edits_but_clean_edit_does_not() {
    let revision = ConfigRevision::from_sha256("revision".to_owned());

    assert!(local_host_needs_save(None, false));
    assert!(local_host_needs_save(None, true));
    assert!(!local_host_needs_save(Some(&revision), false));
    assert!(local_host_needs_save(Some(&revision), true));
}

#[test]
/// 保存、未知结果、删除核对及读回期间统一封锁选择/关闭等变更入口。
fn mutation_gate_blocks_each_in_flight_or_unresolved_operation() {
    let mut state = LocalHostState::default();
    assert!(!state.mutation_blocked());

    state.saving = true;
    assert!(state.mutation_blocked());
    state.saving = false;

    state.save_outcome_unknown = true;
    assert!(state.mutation_blocked());
    state.save_outcome_unknown = false;

    state.delete_checking = true;
    assert!(state.mutation_blocked());
    state.delete_checking = false;

    state.readback_in_progress = true;
    assert!(state.mutation_blocked());
}

#[test]
/// 关闭正在加载的本地主机编辑器会撤销唯一有效意图，迟到的 get_profile 回执不能重开表单。
fn cancelled_editor_load_rejects_its_late_readback() {
    let mut state = LocalHostState {
        editor_load_generation: 7,
        editor_load_pending: true,
        editor_load_intent: Some(LocalHostEditorLoadIntent {
            profile_id: "host-a".to_owned(),
            mode: LocalHostEditorMode::Edit,
            generation: 7,
        }),
        ..LocalHostState::default()
    };
    assert!(state.accepts_editor_load("host-a", LocalHostEditorMode::Edit, 7));
    assert!(state.can_cancel_editor_load());
    assert!(state.mutation_blocked());

    state.cancel_editor_load();

    assert!(!state.editor_load_pending);
    assert!(state.editor_load_intent.is_none());
    assert!(!state.accepts_editor_load("host-a", LocalHostEditorMode::Edit, 7));
    assert!(!state.can_cancel_editor_load());
    assert!(!state.mutation_blocked());

    let owner = LocalHostUnknownMutation::Save {
        profile_id: "host-b".to_owned(),
    };
    state.save_outcome_unknown = true;
    state.unknown_mutation = Some(owner.clone());
    state.cancel_editor_load();
    assert!(state.save_outcome_unknown);
    assert_eq!(state.unknown_mutation, Some(owner));
}

#[test]
/// 核对读取必须从空快照开始；普通列表刷新不能抢走其代次或解除未知写入所有权。
/// @author kongweiguang
fn explicit_readback_invalidates_old_result_and_rejects_watcher_refresh() {
    let mut state = LocalHostState::default();
    let owner = LocalHostUnknownMutation::Save {
        profile_id: "host-a".to_owned(),
    };
    state.save_outcome_unknown = true;
    state.unknown_mutation = Some(owner.clone());
    state.unknown_target_missing = true;

    let generation = state.begin_list_load(true).expect("首次核对可启动");
    assert!(!state.unknown_target_missing);
    assert!(state.unknown_readback.is_none());
    assert!(state.loading);
    assert!(state.readback_in_progress);
    assert_eq!(state.begin_list_load(false), None);
    assert_eq!(state.begin_list_load(true), None);
    assert_eq!(state.generation, generation);
    assert!(state.save_outcome_unknown);
    assert_eq!(state.unknown_mutation, Some(owner));
}

#[test]
/// 默认切换必须按完整列表提交每个 Profile ID 与 revision，并拒绝不在快照中的目标。
fn default_target_and_cas_projection_use_stable_ids_for_every_row() {
    let rows = vec![
        profile_row("host-a", "Same display name", false),
        profile_row("host-b", "Same display name", true),
        profile_row("host-c", "Another host", false),
    ];

    assert_eq!(
        local_host_default_target(&rows, "host-b").map(|row| row.profile.id.as_str()),
        Some("host-b")
    );
    assert!(local_host_default_target(&rows, "Same display name").is_none());
    assert!(local_host_default_target(&rows, "removed-id").is_none());
    assert_eq!(
        local_host_profile_revisions(&rows),
        vec![
            ProfileRevision {
                profile_id: "host-a".to_owned(),
                revision: rows[0].revision.clone(),
            },
            ProfileRevision {
                profile_id: "host-b".to_owned(),
                revision: rows[1].revision.clone(),
            },
            ProfileRevision {
                profile_id: "host-c".to_owned(),
                revision: rows[2].revision.clone(),
            },
        ]
    );
}

#[test]
/// 未知默认操作只按读回的稳定 ID 和全体默认计数分类，目标变化不会伪报原请求成功。
fn default_readback_counts_real_defaults_and_preserves_target_identity() {
    let mut rows = vec![
        profile_row("host-a", "Same display name", false),
        profile_row("host-b", "Same display name", true),
    ];

    let target_not_default = local_host_default_readback(&rows, "host-a");
    assert_eq!(target_not_default.default_profile_ids, vec!["host-b"]);
    assert!(target_not_default.target_present);
    assert!(!target_not_default.target_is_default);
    assert!(local_host_default_readback_can_release(&target_not_default));

    let target_missing = local_host_default_readback(&rows, "removed-id");
    assert_eq!(target_missing.default_profile_ids, vec!["host-b"]);
    assert!(!target_missing.target_present);
    assert!(!target_missing.target_is_default);
    assert!(local_host_default_readback_can_release(&target_missing));

    rows[0].profile.is_default = true;
    let target_default = local_host_default_readback(&rows, "host-a");
    assert_eq!(target_default.default_profile_ids, vec!["host-a", "host-b"]);
    assert!(target_default.target_present);
    assert!(target_default.target_is_default);
    assert!(!local_host_default_readback_can_release(&target_default));
}
