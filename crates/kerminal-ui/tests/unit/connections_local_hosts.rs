//! 持久本地主机字段投影与输入解析的无窗口回归。
//!
//! @author kongweiguang

use super::{
    LocalHostDefaultRequest, LocalHostDefaultValues, LocalHostEditorLoadIntent,
    LocalHostEditorMode, LocalHostInputBaseline, LocalHostState, LocalHostUnknownMutation,
    LocalHostValidationField, format_environment, local_host_draft, local_host_needs_save,
    local_host_projection, local_host_validation_field, local_shell_candidate_fields,
    parse_profile_arguments, parse_profile_environment, profile_to_draft,
    project_explicit_local_defaults, project_untouched_local_defaults,
};
use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Entity, KeyDownEvent, Keystroke, TestAppContext, VisualTestContext, point, px,
    size,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, ProfileService,
    connections::management::{
        HostDocumentRevision, HostDraft, HostGroupDeletionReceipt, HostGroupDraft,
        HostGroupSnapshot, HostManagementRepository, HostManagementService,
        LocalHostSaveRecoveryToken, ManagedHost, ManagedLocalHost,
    },
    local_shells::LocalShellCandidate,
    preferences::{
        DefaultLocalShellProvider, ProfileDraft, ProfileRepository, ProfileRevision,
        VersionedProfile,
    },
};
use kerminal_domain::identity::HostId;
use kerminal_domain::preferences::{
    ConfigRevision, ConfigurationMode, ConfigurationRecoveryState, TerminalProfile,
};
use kerminal_domain::sessions::LocalSessionTarget;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use std::{ffi::OsString, path::PathBuf};
use tokio::{
    runtime::{Builder, Runtime},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

use super::super::panel::ConnectionsPanel;

/// 列表与编辑器必须保留主机自己的身份与配置，只有编辑器读回才携带环境值。
/// @author kongweiguang
#[test]
fn local_host_projection_keeps_host_identity_and_explicit_environment() {
    let host = ManagedLocalHost {
        id: HostId::parse("local-host").expect("测试 ID 有效"),
        group_id: Some("team".to_owned()),
        name: "PowerShell".to_owned(),
        shell: "pwsh.exe".to_owned(),
        args: vec!["-NoLogo".to_owned()],
        cwd: Some("C:\\work".to_owned()),
        environment_ref: None,
        sort_order: 3,
        created_at: "created".to_owned(),
        updated_at: "updated".to_owned(),
        revision: ConfigRevision::from_sha256("revision".to_owned()),
        migration_origin: None,
    };
    let list_row = local_host_projection(host.clone(), BTreeMap::new());
    assert_eq!(list_row.profile.id, "local-host");
    assert_eq!(list_row.profile.sidebar_group_id.as_deref(), Some("team"));
    assert!(!list_row.profile.is_default);
    assert!(list_row.profile.env.is_empty());

    let environment = BTreeMap::from([("PASSWORD".to_owned(), "secret".to_owned())]);
    let editor_row = local_host_projection(host, environment.clone());
    let draft = local_host_draft(profile_to_draft(&editor_row.profile)).expect("草稿可保存");
    assert_eq!(draft.id.as_ref().map(HostId::as_str), Some("local-host"));
    assert_eq!(draft.group_id.as_deref(), Some("team"));
    assert_eq!(draft.env, environment);
    assert_eq!(draft.args, vec!["-NoLogo"]);
}

/// 候选经可逆格式投影后保留空参数和多行参数，同时拒绝 OS 不可启动的空字符。
/// @author kongweiguang
#[test]
fn local_shell_candidate_fields_preserve_special_argv_and_reject_nulls() {
    let program = std::env::current_exe().expect("测试 executable 路径可用");
    let expected_program = program
        .as_os_str()
        .to_str()
        .expect("测试 executable 路径可表示为文本");
    let candidate = LocalShellCandidate::new(
        "test-shell",
        "Test shell",
        program.as_os_str().to_os_string(),
        vec![OsString::from("--login"), OsString::new()],
    )
    .expect("测试候选有效");
    let fields = local_shell_candidate_fields(&candidate).expect("候选字段可无损投影");

    assert_eq!(fields.0, expected_program);
    assert_eq!(fields.1, "--login\n\"\"");
    assert_eq!(
        parse_profile_arguments(&fields.1).unwrap(),
        vec!["--login".to_owned(), String::new()]
    );

    let multiline = LocalShellCandidate::new(
        "test-shell-newline",
        "Test shell with multiline argument",
        PathBuf::from(format!("{}-shell", program.display())).into_os_string(),
        vec![OsString::from("--login\n--interactive")],
    )
    .expect("候选发现契约不解析 argv 字符含义");
    let fields = local_shell_candidate_fields(&multiline).unwrap();
    assert_eq!(
        parse_profile_arguments(&fields.1).unwrap(),
        ["--login\n--interactive"]
    );
    let empty_argument = LocalShellCandidate::new(
        "empty-arg",
        "Empty argument",
        program.clone().into_os_string(),
        vec![OsString::new()],
    )
    .unwrap();
    let fields = local_shell_candidate_fields(&empty_argument).unwrap();
    assert_eq!(parse_profile_arguments(&fields.1).unwrap(), [""]);
    let null_argument = LocalShellCandidate::new(
        "null-arg",
        "Null argument",
        program.into_os_string(),
        vec![OsString::from("bad\0arg")],
    )
    .unwrap();
    assert!(local_shell_candidate_fields(&null_argument).is_err());
}

/// 共享可执行文件的开发者环境以 argv 区分，不能把其他参数变体显示为已选中。
/// @author kongweiguang
#[test]
fn shell_candidate_selection_matches_program_and_arguments() {
    let program = std::env::current_exe().unwrap();
    let plain = LocalShellCandidate::new(
        "plain",
        "Plain",
        program.clone().into_os_string(),
        Vec::new(),
    )
    .unwrap();
    let developer = LocalShellCandidate::new(
        "developer",
        "Developer",
        program.clone().into_os_string(),
        vec![OsString::from("/k"), OsString::from("developer.cmd")],
    )
    .unwrap();
    let program = program.to_str().unwrap();
    assert!(super::local_shell_candidate_matches(&plain, program, ""));
    assert!(!super::local_shell_candidate_matches(
        &plain,
        program,
        "/k\ndeveloper.cmd"
    ));
    assert!(super::local_shell_candidate_matches(
        &developer,
        program,
        "/k\ndeveloper.cmd"
    ));
    assert!(!super::local_shell_candidate_matches(
        &developer, program, ""
    ));
}

/// Shell 扫描的空结果、失败与取消使用不同状态，且失败不得丢掉上次可用候选。
/// @author kongweiguang
#[test]
fn local_shell_discovery_distinguishes_empty_failure_and_cancellation() {
    let candidate = LocalShellCandidate::new(
        "test-shell",
        "Test shell",
        std::env::current_exe()
            .expect("测试 executable 路径可用")
            .into_os_string(),
        Vec::new(),
    )
    .expect("测试候选有效");
    let mut state = LocalHostState {
        shell_candidates: vec![candidate],
        shell_discovery_loaded: true,
        shell_discovery_loading: true,
        ..LocalHostState::default()
    };

    state.complete_shell_discovery(Err(AppError::unsupported("扫描不可用")));
    assert!(!state.shell_discovery_loading);
    assert!(state.shell_discovery_loaded);
    assert_eq!(state.shell_candidates.len(), 1);
    assert!(state.shell_discovery_error.is_some());

    state.complete_shell_discovery(Err(AppError::cancelled("用户取消扫描")));
    assert!(!state.shell_discovery_loading);
    assert!(state.shell_discovery_loaded);
    assert_eq!(state.shell_candidates.len(), 1);
    assert!(state.shell_discovery_error.is_none());

    state.complete_shell_discovery(Ok(Vec::new()));
    assert!(!state.shell_discovery_loading);
    assert!(state.shell_discovery_loaded);
    assert!(state.shell_candidates.is_empty());
    assert!(state.shell_discovery_error.is_none());
}

/// 取消会停止当前 adapter token 并使在途 generation 失效，但保留已扫描候选。
/// @author kongweiguang
#[test]
fn cancelling_local_shell_discovery_invalidates_the_request_and_keeps_candidates() {
    let cancellation = CancellationToken::new();
    let candidate = LocalShellCandidate::new(
        "test-shell",
        "Test shell",
        std::env::current_exe()
            .expect("测试 executable 路径可用")
            .into_os_string(),
        Vec::new(),
    )
    .expect("测试候选有效");
    let mut state = LocalHostState {
        shell_candidates: vec![candidate],
        shell_discovery_generation: 7,
        shell_discovery_loading: true,
        shell_discovery_cancellation: Some(cancellation.clone()),
        ..LocalHostState::default()
    };
    let generation = state.shell_discovery_generation;

    state.cancel_shell_discovery();

    assert!(cancellation.is_cancelled());
    assert_eq!(state.shell_discovery_generation, generation + 1);
    assert!(!state.shell_discovery_loading);
    assert_eq!(state.shell_candidates.len(), 1);
}

/// 零参数 Shell 候选被显式选择后，迟到的系统默认目标不得再补入其它 Shell 的参数。
/// @author kongweiguang
#[test]
fn explicit_zero_argument_shell_choice_rejects_a_late_default_target_projection() {
    let candidate = LocalShellCandidate::new(
        "test-bash",
        "Test Bash",
        std::env::current_exe()
            .expect("测试 executable 路径可用")
            .into_os_string(),
        Vec::new(),
    )
    .expect("测试候选有效");
    let (selected_shell, selected_args) =
        local_shell_candidate_fields(&candidate).expect("Shell 字段可表示");
    assert!(selected_args.is_empty());

    let original = baseline("Local", "", "", "", "");
    let selected = baseline("Local", &selected_shell, "", &selected_args, "");
    let late_defaults = LocalHostDefaultValues {
        shell: "pwsh.exe".to_owned(),
        cwd: String::new(),
        args: "-NoLogo".to_owned(),
        env: String::new(),
    };
    let projection = project_untouched_local_defaults(&original, &selected, late_defaults);
    assert!(projection.shell.is_none());
    assert!(projection.args.is_none());

    let mut state = LocalHostState {
        default_target_request: Some(LocalHostDefaultRequest {
            profile_id: None,
            explicit_override: None,
        }),
        ..LocalHostState::default()
    };
    let stale_generation = state.default_target_generation;
    state.invalidate_default_target_resolution();

    assert!(state.default_target_request.is_none());
    assert!(!state.accepts_default_target_generation(stale_generation));
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
/// 默认目标晚于参数编辑时，不另换 Shell 造成命令混用；目录和环境仍按各自基线保护。
/// @author kongweiguang
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

    assert_eq!(projection.shell, None);
    assert_eq!(projection.cwd.as_deref(), Some("C:\\work"));
    assert_eq!(projection.args, None);
    assert_eq!(projection.env, None);
    assert_eq!(projection.baseline.name, original.name);
    assert_eq!(projection.baseline.shell, original.shell);
    assert_eq!(projection.baseline.cwd, "C:\\work");
    assert_eq!(projection.baseline.args, original.args);
    assert_eq!(projection.baseline.env, original.env);
    assert_ne!(current.name, original.name);
}

/// 手动输入和文件选择都只改程序字段；默认及主动 Profile 回执均不得把其它 Shell 参数混入。
/// @author kongweiguang
#[test]
fn late_profile_command_projection_keeps_program_and_arguments_together() {
    let original = baseline("Local", "", "", "", "");
    for current in [
        baseline("Local", "cmd.exe", "", "", ""),
        baseline("Local", "", "", "/k\ncustom.cmd", ""),
    ] {
        let defaults = LocalHostDefaultValues {
            shell: "pwsh.exe".to_owned(),
            cwd: "workspace".to_owned(),
            args: "-NoLogo".to_owned(),
            env: "TERM=xterm".to_owned(),
        };
        let automatic = project_untouched_local_defaults(&original, &current, defaults.clone());
        let explicit = project_explicit_local_defaults(&original, &current, defaults, &original);
        for projection in [automatic, explicit] {
            assert!(projection.shell.is_none());
            assert!(projection.args.is_none());
            assert_eq!(projection.cwd.as_deref(), Some("workspace"));
            assert_eq!(projection.env.as_deref(), Some("TERM=xterm"));
        }
    }
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
/// 空编辑区表达零参数，显式空行保持兼容，引用项可明确表达单个空参数。
fn argument_lines_preserve_explicit_empty_arguments() {
    assert_eq!(parse_profile_arguments("").unwrap(), Vec::<String>::new());
    assert_eq!(
        parse_profile_arguments("--flag\n\nvalue").unwrap(),
        vec!["--flag".to_owned(), "".to_owned(), "value".to_owned()]
    );
    assert_eq!(parse_profile_arguments("\"\"").unwrap(), [""]);
    assert!(parse_profile_arguments("\"unfinished").is_err());
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

/// 值内换行和等号不得生成新变量；既有合法键的空白和引号也不能在改名保存时改变。
/// @author kongweiguang
#[test]
fn environment_text_roundtrip_keeps_multiline_values_and_exact_keys() {
    let values = BTreeMap::from([
        ("TOKEN".to_owned(), "first\nEXTRA=second".to_owned()),
        (
            "OTHER".to_owned(),
            "second line\nwithout separator\r\n\"quote\"".to_owned(),
        ),
        (" spaced key ".to_owned(), r"C:\temp\new".to_owned()),
        ("line\nkey".to_owned(), String::new()),
    ]);
    assert_eq!(
        parse_profile_environment(&format_environment(&values)).unwrap(),
        values
    );
    assert!(parse_profile_environment("\"KEY=value").is_err());
    assert!(parse_profile_environment("A=ok\n\"A=second\nline\"").is_err());
    assert!(parse_profile_environment("KEY=bad\0value").is_err());
}

/// 生产编辑器仅改名称时须保留单空参数和多行参数/环境，不依赖纯编解码测试相互抵消。
/// @author kongweiguang
#[gpui_kit::test]
fn renaming_local_editor_preserves_typed_arguments_and_environment(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = local_delete_fixture(app, delayed_local_host_repository());
    for args in [
        vec![String::new()],
        vec!["-Command".to_owned(), "first\nsecond".to_owned()],
    ] {
        visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                let env = BTreeMap::from([("TOKEN".to_owned(), "first\nEXTRA=second".to_owned())]);
                let draft = ProfileDraft {
                    id: Some("local-roundtrip".to_owned()),
                    name: "original".to_owned(),
                    shell: "pwsh.exe".to_owned(),
                    args: args.clone(),
                    cwd: None,
                    env: env.clone(),
                    sidebar_group_id: None,
                    sort_order: 0,
                };
                let editor = panel.make_local_host_editor(
                    draft,
                    Some(ConfigRevision::from_sha256("saved".to_owned())),
                    LocalHostEditorMode::Edit,
                    window,
                    cx,
                );
                editor
                    .name
                    .update(cx, |input, cx| input.replace_all("renamed", window, cx));
                let submitted = editor.draft(cx).unwrap();
                assert_eq!(submitted.name, "renamed");
                assert_eq!(submitted.args, args);
                assert_eq!(submitted.env, env);
            })
        });
    }
}

/// 两种列表的格式错误由生产保存入口定位到各自字段，且不能进入仓储写入。
/// @author kongweiguang
#[gpui_kit::test]
fn local_list_syntax_errors_focus_their_own_field(app: &mut TestAppContext) {
    use super::super::panel::ConnectionsEditorKind;
    let (_runtime, panel, visual) = local_delete_fixture(app, delayed_local_host_repository());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.activate_local_host_editor(window, cx);
            assert_eq!(panel.active_editor_kind, Some(ConnectionsEditorKind::Local));
            for field in [
                LocalHostValidationField::Arguments,
                LocalHostValidationField::Environment,
            ] {
                let editor = panel.local_hosts.editor.as_ref().unwrap();
                editor
                    .args
                    .update(cx, |input, cx| input.set_value("", window, cx));
                editor
                    .env
                    .update(cx, |input, cx| input.set_value("", window, cx));
                let input = if field == LocalHostValidationField::Arguments {
                    &editor.args
                } else {
                    &editor.env
                };
                input.update(cx, |input, cx| input.set_value("\"unfinished", window, cx));
                panel.save_local_host(window, cx);
                assert!(!panel.local_hosts.saving);
                assert_eq!(
                    panel.local_hosts.editor.as_ref().unwrap().validation_field,
                    Some(field)
                );
            }
        })
    });
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

/// 主机新校验文案要跳到真实字段，不能只在表单底部显示笼统失败。
/// @author kongweiguang
#[test]
fn local_host_validation_focuses_the_failed_field() {
    assert_eq!(
        local_host_validation_field("本地主机名称不能为空或过长", "", ""),
        Some(LocalHostValidationField::Name),
    );
    assert_eq!(
        local_host_validation_field("本地主机 shell 路径不能为空或过长", "", ""),
        Some(LocalHostValidationField::Shell),
    );
    assert_eq!(
        local_host_validation_field("本地主机环境变量超出限制", "", ""),
        Some(LocalHostValidationField::Environment),
    );
    assert_eq!(
        local_host_validation_field("本地主机参数或工作目录超出限制", "", &"x".repeat(4097)),
        Some(LocalHostValidationField::WorkingDirectory),
    );
}

#[test]
/// 保存、未知结果、待清理凭据、删除核对及读回期间统一封锁变更入口。
fn mutation_gate_blocks_each_in_flight_or_unresolved_operation() {
    let mut state = LocalHostState::default();
    assert!(!state.mutation_blocked());

    state.saving = true;
    assert!(state.mutation_blocked());
    state.saving = false;

    state.save_outcome_unknown = true;
    assert!(state.mutation_blocked());
    state.save_outcome_unknown = false;

    state.cleanup_pending = true;
    assert!(state.mutation_blocked());
    state.cleanup_pending = false;

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

/// 只延迟显式选中的一次读取，其他目录读取仍经生产用例独立完成。
struct DelayedLocalHosts {
    host: Mutex<Option<ManagedLocalHost>>,
    read_gate: Mutex<Option<Arc<Semaphore>>>,
    list_gate: Mutex<Option<Arc<Semaphore>>>,
    reads_started: AtomicUsize,
    lists_started: AtomicUsize,
    deleted_revisions: Mutex<Vec<ConfigRevision>>,
    recovery: Mutex<Option<LocalHostSaveRecoveryToken>>,
    recoveries_started: AtomicUsize,
    recoveries_finished: AtomicUsize,
}

#[async_trait]
impl HostManagementRepository for DelayedLocalHosts {
    /// 测试只允许内存仓储完成本地主机确认删除。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Ok(())
    }
    /// 远程目录保持空，避免本地回归启动外部连接。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(Vec::new())
    }
    /// 本测试不读取远程主机。
    async fn load_host(&self, _: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(None)
    }
    /// 先冻结一次目录快照，再等测试释放，以重现正在读取时发起删除。
    async fn list_local_hosts(&self) -> AppResult<Vec<ManagedLocalHost>> {
        let snapshot = self.host.lock().unwrap().clone().into_iter().collect();
        let gate = self.list_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.lists_started.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.unwrap().forget();
        }
        Ok(snapshot)
    }
    /// 在信号量释放前保留读取时的快照，便于验证取消后迟到回执不能覆盖新意图。
    async fn load_local_host(&self, id: &HostId) -> AppResult<Option<ManagedLocalHost>> {
        let snapshot = self
            .host
            .lock()
            .unwrap()
            .clone()
            .filter(|host| &host.id == id);
        let gate = self.read_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.reads_started.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.unwrap().forget();
        }
        Ok(snapshot)
    }
    /// 只有登记同一目标和 revision 的恢复记录后才能删除，模拟真实 CAS 次序。
    async fn delete_local_host(
        &self,
        id: &HostId,
        revision: &ConfigRevision,
    ) -> AppResult<ManagedLocalHost> {
        let recovery = self.recovery.lock().unwrap();
        if !recovery.as_ref().is_some_and(|token| {
            token.host_id() == id && token.baseline_revision() == Some(revision)
        }) {
            return Err(AppError::conflict("test recovery target mismatch"));
        }
        drop(recovery);
        let mut host = self.host.lock().unwrap();
        let current = host
            .as_ref()
            .filter(|host| &host.id == id)
            .ok_or_else(|| AppError::not_found("test host missing"))?;
        if &current.revision != revision {
            return Err(AppError::conflict("test revision changed"));
        }
        self.deleted_revisions
            .lock()
            .unwrap()
            .push(revision.clone());
        Ok(host.take().unwrap())
    }
    /// 删除前登记恢复身份，以检查测试替身遵循真实仓储的写入顺序。
    async fn begin_local_host_recovery(&self, token: &LocalHostSaveRecoveryToken) -> AppResult<()> {
        let mut recovery = self.recovery.lock().unwrap();
        if recovery.is_some() {
            return Err(AppError::conflict("test recovery already pending"));
        }
        *recovery = Some(token.clone());
        self.recoveries_started.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    /// 启动检查读取同一份恢复记录，避免把未清理的删除伪装成完成。
    async fn pending_local_host_recovery(&self) -> AppResult<Option<LocalHostSaveRecoveryToken>> {
        Ok(self.recovery.lock().unwrap().clone())
    }
    /// 只清理当前操作的恢复记录，重复清理不存在的记录保持幂等。
    async fn finish_local_host_recovery(
        &self,
        token: &LocalHostSaveRecoveryToken,
    ) -> AppResult<()> {
        let mut recovery = self.recovery.lock().unwrap();
        match recovery.as_ref() {
            Some(current) if current != token => {
                Err(AppError::conflict("test recovery identity mismatch"))
            }
            Some(_) => {
                *recovery = None;
                self.recoveries_finished.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            None => Ok(()),
        }
    }
    /// 本回归不保存远程主机。
    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("test"))
    }
    /// 本回归不删除远程主机。
    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("test"))
    }
    /// 返回空分组快照以完成面板真实初始化。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: Vec::new(),
            revision: None,
        })
    }
    /// 本回归不保存分组。
    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("test"))
    }
    /// 本回归不修改分组事务。
    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported("test"))
    }
}

struct EmptyLocalProfiles;

#[async_trait]
impl ProfileRepository for EmptyLocalProfiles {
    /// 列表刷新正常读取空模板目录，与主机快照分开。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }
    /// 没有测试模板版本。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }
    /// 没有测试模板详情。
    async fn get_profile(&self, _: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }
    /// 本测试不保存模板。
    async fn save_profile(
        &self,
        _: ProfileDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported("test"))
    }
    /// 本测试不删除模板。
    async fn delete_profile(&self, _: &str, _: ConfigRevision) -> AppResult<bool> {
        Err(AppError::unsupported("test"))
    }
    /// 本测试不变更默认模板。
    async fn set_default_profile(
        &self,
        _: &str,
        _: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported("test"))
    }
}

struct NoLocalShell;

impl DefaultLocalShellProvider for NoLocalShell {
    /// 本回归不解析系统 Shell，确保没有外部环境依赖。
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        Err(AppError::unsupported("test"))
    }
}

/// 隔离仓储使用固定本地主机，不涉及用户配置、环境秘密或 OS 进程。
fn delayed_local_host_repository() -> Arc<DelayedLocalHosts> {
    Arc::new(DelayedLocalHosts {
        host: Mutex::new(Some(ManagedLocalHost {
            id: HostId::parse("local-delete-test").unwrap(),
            group_id: None,
            name: "Original".into(),
            shell: "test-shell".into(),
            args: Vec::new(),
            cwd: None,
            environment_ref: None,
            sort_order: 0,
            created_at: "created".into(),
            updated_at: "updated".into(),
            revision: ConfigRevision::from_sha256("1".repeat(64)),
            migration_origin: None,
        })),
        read_gate: Mutex::new(None),
        list_gate: Mutex::new(None),
        reads_started: AtomicUsize::new(0),
        lists_started: AtomicUsize::new(0),
        deleted_revisions: Mutex::new(Vec::new()),
        recovery: Mutex::new(None),
        recoveries_started: AtomicUsize::new(0),
        recoveries_finished: AtomicUsize::new(0),
    })
}

/// 真实面板、共享执行器和受控仓储组成无原生窗口的异步竞态测试。
fn local_delete_fixture(
    app: &mut TestAppContext,
    repository: Arc<DelayedLocalHosts>,
) -> (Runtime, Entity<ConnectionsPanel>, &mut VisualTestContext) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let hosts = HostManagementService::new(repository);
    let profiles = ProfileService::new(Arc::new(EmptyLocalProfiles), Arc::new(NoLocalShell));
    app.update(crate::init);
    let mounted = Rc::new(RefCell::new(None));
    let slot = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| {
            ConnectionsPanel::new(
                hosts,
                None,
                executor,
                ConfigurationRecoveryState {
                    mode: ConfigurationMode::ReadWrite,
                    issues: Vec::new(),
                },
                window,
                cx,
            )
            .with_profiles(profiles, cx)
        });
        *slot.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = mounted.borrow_mut().take().unwrap();
    pump_local_delete(&runtime, visual);
    (runtime, panel, visual)
}

/// 推进 Tokio 仓储回执与 GPUI 实体更新，显式受控的读取仍保持在途。
fn pump_local_delete(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..12 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.run_until_parked();
}

/// 根据生产按钮的真实 AccessKit Click 能力核对加载门禁，而不是推断可点击状态。
fn local_delete_confirm_clickable(visual: &mut VisualTestContext) -> bool {
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let id = gpui_kit::ElementId::from("connections-delete-local-host-confirm");
        let button =
            gpui_kit::base::test_support::find(window, &[], &id).expect("删除确认按钮必须已绘制");
        assert_eq!(button.role(), Some(gpui_kit::Role::Button));
        button.supports_click()
    })
}

/// watcher 刷新不得使删除核对失效；取消不写入，再确认时必须使用最新 revision。
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_check_survives_watcher_reload_and_confirms_latest_revision(
    app: &mut TestAppContext,
) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    let latest_revision = ConfigRevision::from_sha256("2".repeat(64));
    {
        let mut host = repository.host.lock().unwrap();
        let host = host.as_mut().unwrap();
        host.name = "Current".into();
        host.revision = latest_revision.clone();
    }
    let gate = Arc::new(Semaphore::new(0));
    *repository.read_gate.lock().unwrap() = Some(gate.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert_eq!(repository.reads_started.load(Ordering::SeqCst), 1);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.delete_checking));

    // 配置 watcher 和手动刷新共用此生产入口，读取本身仍由真实 application service 执行。
    visual.update(|_, app| panel.update(app, |panel, cx| panel.reload_local_hosts(cx)));
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(state.delete_checking);
        assert!(!state.loading);
        assert_eq!(state.rows[0].revision, latest_revision);
    });

    gate.add_permits(1);
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.delete_checking);
        let pending = state.pending_delete.as_ref().unwrap();
        assert_eq!(pending.profile.name, "Current");
        assert_eq!(pending.revision, latest_revision);
    });
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_none()));

    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert_eq!(
        visual.update(|_, app| panel
            .read(app)
            .local_hosts
            .pending_delete
            .as_ref()
            .map(|host| host.revision.clone())),
        Some(latest_revision.clone())
    );
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.loading, "列表读取未结束，不能确认删除");
        assert!(!state.mutation_blocked(), "删除提交仍被操作门禁阻止");
    });
    assert!(local_delete_confirm_clickable(visual));
    visual.update(|_, app| panel.update(app, |panel, cx| panel.confirm_delete_local_host(cx)));
    pump_local_delete(&runtime, visual);
    assert_eq!(
        *repository.deleted_revisions.lock().unwrap(),
        vec![latest_revision]
    );
    assert!(repository.host.lock().unwrap().is_none());
    assert_eq!(repository.recoveries_started.load(Ordering::SeqCst), 1);
    assert_eq!(repository.recoveries_finished.load(Ordering::SeqCst), 1);
    assert!(repository.recovery.lock().unwrap().is_none());
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.loading && !state.delete_checking && !state.saving);
        assert!(!state.cleanup_pending);
        assert!(state.pending_delete.is_none());
        assert!(state.rows.is_empty());
        assert!(!state.mutation_blocked());
    });
}

/// 删除核对不能使已在途的目录回执过期；两个读取各自结束后都必须解除忙碌状态。
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_check_preserves_inflight_list_completion(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    let gate = Arc::new(Semaphore::new(0));
    *repository.list_gate.lock().unwrap() = Some(gate.clone());
    visual.update(|_, app| panel.update(app, |panel, cx| panel.reload_local_hosts(cx)));
    pump_local_delete(&runtime, visual);
    assert_eq!(repository.lists_started.load(Ordering::SeqCst), 1);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.loading));

    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.delete_checking);
        assert!(state.pending_delete.is_some());
        assert!(state.loading);
    });
    assert!(!local_delete_confirm_clickable(visual));
    visual.update(|_, app| panel.update(app, |panel, cx| panel.confirm_delete_local_host(cx)));
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(state.loading && !state.saving);
        assert!(state.pending_delete.is_some());
    });
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
    gate.add_permits(1);
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.loading && !state.delete_checking);
        assert!(state.pending_delete.is_some());
    });
    assert!(local_delete_confirm_clickable(visual));
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(!visual.update(|_, app| panel.read(app).local_hosts.mutation_blocked()));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}

/// 清理阻塞不能让确认框遮住恢复入口；写入中和未知回执仍保留确认上下文。
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_confirmation_allows_cancel_when_cleanup_is_pending(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));

    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.local_hosts.saving = true;
            panel.cancel_delete_local_host(cx);
            assert!(panel.local_hosts.pending_delete.is_some());
            panel.local_hosts.saving = false;
            panel.local_hosts.save_outcome_unknown = true;
            panel.cancel_delete_local_host(cx);
            assert!(panel.local_hosts.pending_delete.is_some());
            panel.local_hosts.save_outcome_unknown = false;
            panel.local_hosts.cleanup_pending = true;
            cx.notify();
        });
    });
    assert!(!local_delete_confirm_clickable(visual));
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let id = gpui_kit::ElementId::from("connections-delete-local-host-cancel");
        let cancel = gpui_kit::base::test_support::find(window, &[], &id).unwrap();
        assert!(cancel.supports_click());
    });
    visual.update(|_, app| panel.update(app, |panel, cx| panel.confirm_delete_local_host(cx)));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    visual.update(|window, app| {
        let state = &panel.read(app).local_hosts;
        assert!(state.pending_delete.is_none());
        assert!(state.cleanup_pending);
        window.draw(app).clear(app);
        let id = gpui_kit::ElementId::from("connections-review-local-host-unknown-banner");
        let review = gpui_kit::base::test_support::find(window, &[], &id)
            .expect("关闭确认后必须显示清理恢复入口");
        assert!(review.supports_click());
    });
}

/// 删除确认必须以限定宽度的居中弹层出现。
/// 确认卡一旦回到面板内容流，就会把空状态工作区撑成横跨窗口的大弹框。
///
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_confirmation_is_a_bounded_centered_overlay(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository);
    visual.simulate_resize(size(px(1280.), px(800.)));
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });

    let overlay = visual
        .debug_bounds("connections-delete-local-host-overlay-bounds")
        .expect("删除确认遮罩必须覆盖面板");
    let card = visual
        .debug_bounds("rr-connections-delete-local-host-card")
        .expect("删除确认卡必须已绘制");
    assert!(overlay.size.width >= px(1279.), "遮罩应覆盖整个面板宽度");
    assert!(overlay.size.height >= px(799.), "遮罩应覆盖整个面板高度");
    assert!(
        card.size.width <= px(520.),
        "确认卡宽度必须受限，实际 {}",
        card.size.width
    );
    assert!(
        card.size.height < overlay.size.height * 0.5,
        "确认卡不应占满面板高度"
    );
    assert!(
        (card.center().x - overlay.center().x).abs() <= px(1.),
        "确认卡必须水平居中"
    );
    assert!(
        (card.center().y - overlay.center().y).abs() <= px(1.),
        "确认卡必须垂直居中"
    );
}

/// 点击遮罩等同取消：删除确认不因误触背景而丢失，也不在取消后继续滞留。
///
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_confirmation_dismisses_from_scrim_click(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    visual.simulate_resize(size(px(1280.), px(800.)));
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });
    let overlay = visual
        .debug_bounds("connections-delete-local-host-overlay-bounds")
        .expect("删除确认遮罩必须覆盖面板");
    visual.simulate_click(
        point(overlay.origin.x + px(6.), overlay.origin.y + px(6.)),
        Default::default(),
    );
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });
    assert!(
        visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_none()),
        "点击遮罩必须取消删除确认"
    );
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}

/// Escape 与遮罩点击共用同一取消入口；确认中的删除不会被按键穿透提交。
///
/// @author kongweiguang
#[gpui_kit::test]
fn escape_cancels_pending_delete_confirmation(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));
    visual.update(|window, app| {
        let event = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Default::default(),
                key: "escape".to_owned(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        };
        panel.update(app, |panel, cx| {
            panel.handle_confirmation_escape(&event, window, cx)
        });
    });
    assert!(
        !visual.update(|_, app| panel.read(app).has_pending_deletion_confirmation()),
        "Escape 必须取消最上层删除确认"
    );
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}

/// 取消在途只读核对立即释放门禁；旧回执不能覆盖之后重新发起的确认。
/// @author kongweiguang
#[gpui_kit::test]
fn cancelled_local_delete_check_rejects_late_receipt(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    let gate = Arc::new(Semaphore::new(0));
    *repository.read_gate.lock().unwrap() = Some(gate.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert_eq!(repository.reads_started.load(Ordering::SeqCst), 1);
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(!visual.update(|_, app| panel.read(app).local_hosts.mutation_blocked()));

    let latest = ConfigRevision::from_sha256("2".repeat(64));
    repository.host.lock().unwrap().as_mut().unwrap().revision = latest.clone();
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    gate.add_permits(1);
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.delete_checking && !state.loading);
        assert_eq!(state.pending_delete.as_ref().unwrap().revision, latest);
    });
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}
