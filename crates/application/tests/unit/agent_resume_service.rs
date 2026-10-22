//! Agent provider metadata compatibility tests.
//!
//! @author kongweiguang

use kerminal_domain::{
    agents::{AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch},
    identity::AgentSessionId,
};

use super::{
    AgentProviderSessionStatus, AgentResumeCapability, AgentTextPreview, AppError,
    StoredMcpEndpointContext, initial_provider_session, project_agent_text_preview,
    project_resume_capability, project_resume_capability_result, provider_session_for_resume,
    validate_agent_text_controls, validate_agent_text_is_reviewed, validate_mcp_endpoint,
};

/// 构造无真实 workspace 或 CLI 依赖的隔离会话身份，用于 endpoint 路径校验。
///
/// @author kongweiguang
fn endpoint_fixture_session() -> AgentSession {
    let id = AgentSessionId::new();
    let session_root = format!("C:/fixture/.kerminal/agents/sessions/{id}");
    AgentSession::new(
        id,
        AgentProvider::Codex,
        None,
        "Endpoint compatibility fixture".to_owned(),
        "2026-10-19T00:00:00Z".to_owned(),
        "C:/fixture/.kerminal".to_owned(),
        session_root.clone(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: session_root,
        },
        None,
        None,
    )
    .expect("isolated endpoint session should satisfy domain invariants")
}

/// 为同一 schema 构造 endpoint context，令路径差异成为唯一变量。
///
/// @author kongweiguang
fn stored_endpoint_context(
    session: &AgentSession,
    endpoint: Option<String>,
) -> StoredMcpEndpointContext {
    StoredMcpEndpointContext {
        schema_version: 1,
        agent_session_id: session.agent_session_id.to_string(),
        endpoint,
        transport: "streamable-http".to_owned(),
        tools_only: true,
        generated_at: "2026-10-19T00:00:00Z".to_owned(),
    }
}

/// 运行时只接受当前全局路由；旧 scoped 路由必须先经数据迁移清理。
///
/// @author kongweiguang
#[test]
fn stored_endpoint_accepts_only_current_global_route() {
    let session = endpoint_fixture_session();
    let id = session.agent_session_id.as_str();
    let base_endpoint = "http://127.0.0.1:37657/mcp";
    let scoped_endpoint = format!("http://127.0.0.1:37657/mcp/agents/{id}");

    stored_endpoint_context(&session, Some(base_endpoint.to_owned()))
        .validate_for(&session)
        .expect("current global route should remain readable");
    assert!(validate_mcp_endpoint(base_endpoint).is_ok());
    assert!(validate_mcp_endpoint(&scoped_endpoint).is_err());

    let other_id = AgentSessionId::new();
    let rejected = [
        scoped_endpoint,
        format!("http://127.0.0.1:37657/mcp/agents/{other_id}"),
        "http://127.0.0.1:37657/mcp/extra".to_owned(),
        "http://127.0.0.1:37657/mcp?token=fixture".to_owned(),
        "http://127.0.0.1:37657/mcp#fragment".to_owned(),
        "http://fixture@127.0.0.1:37657/mcp".to_owned(),
        "http://fixture:secret@127.0.0.1:37657/mcp".to_owned(),
        "https://127.0.0.1:37657/mcp".to_owned(),
        "http://localhost:37657/mcp".to_owned(),
        "http://127.0.0.1/mcp".to_owned(),
        "http://127.0.0.1:37657/mcp/unknown".to_owned(),
    ];
    for endpoint in rejected {
        assert!(
            stored_endpoint_context(&session, Some(endpoint))
                .validate_for(&session)
                .is_err(),
            "unapproved endpoint path or authority must remain invalid"
        );
    }

    let mut foreign_document = stored_endpoint_context(
        &session,
        Some(format!("http://127.0.0.1:37657/mcp/agents/{other_id}")),
    );
    foreign_document.agent_session_id = other_id.to_string();
    assert!(foreign_document.validate_for(&session).is_err());
}

/// 缺失 provider 文件必须先经过一次性迁移，运行时不能再猜测恢复策略。
#[test]
fn missing_provider_metadata_requires_migration_before_any_resume() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Pi,
        AgentProvider::Custom,
    ] {
        assert!(
            provider_session_for_resume(&AgentProviderSessionStatus::Missing, provider).is_err(),
            "missing metadata must not be inferred for {provider:?}"
        );
    }
}

/// 同一初始工厂保留内建恢复能力，并确保 Custom 不会得到推测出的恢复命令。
#[test]
fn initial_provider_metadata_keeps_builtin_and_custom_capabilities_explicit() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Pi,
    ] {
        let builtin = initial_provider_session(provider);
        assert_eq!(builtin.provider, provider);
        assert!(builtin.resume_supported);
        assert!(builtin.resume_command.is_some());
    }

    let codex = initial_provider_session(AgentProvider::Codex);
    assert_eq!(codex.resume_command.as_deref(), Some("codex resume --last"));

    let custom = initial_provider_session(AgentProvider::Custom);
    assert!(!custom.resume_supported);
    assert_eq!(custom.resume_command, None);
    assert_eq!(custom.provider_session_id, None);
}

/// 损坏、未来版本和 provider 错配必须保留原文件并失败，不能按缺失重建。
#[test]
fn invalid_provider_metadata_is_not_replaced_with_defaults() {
    for status in [
        AgentProviderSessionStatus::Malformed,
        AgentProviderSessionStatus::UnsupportedSchemaVersion(2),
        AgentProviderSessionStatus::ProviderMismatch,
        AgentProviderSessionStatus::Available(AgentProviderSession {
            schema_version: 1,
            provider: AgentProvider::Claude,
            provider_session_id: None,
            resume_command: Some("claude --continue".to_owned()),
            resume_supported: true,
            last_resume_at: None,
        }),
    ] {
        assert!(provider_session_for_resume(&status, AgentProvider::Codex).is_err());
    }
}

/// 能力投影只采信匹配且通过恢复策略校验的文档，不从 Custom provider 类型猜测结果。
///
/// @author kongweiguang
#[test]
fn resume_capability_uses_validated_provider_metadata_without_probe() {
    let codex = endpoint_fixture_session();
    let supported = AgentProviderSessionStatus::Available(AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Codex,
        provider_session_id: None,
        resume_command: Some("codex resume --last".to_owned()),
        resume_supported: true,
        last_resume_at: None,
    });
    assert_eq!(
        project_resume_capability(&codex, &supported),
        AgentResumeCapability::Supported
    );
    for unavailable in [
        AgentProviderSessionStatus::Missing,
        AgentProviderSessionStatus::Malformed,
        AgentProviderSessionStatus::UnsupportedSchemaVersion(2),
        AgentProviderSessionStatus::ProviderMismatch,
    ] {
        assert_eq!(
            project_resume_capability(&codex, &unavailable),
            AgentResumeCapability::MetadataUnavailable
        );
    }
    let mismatched = AgentProviderSessionStatus::Available(AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Claude,
        provider_session_id: None,
        resume_command: Some("claude --continue".to_owned()),
        resume_supported: true,
        last_resume_at: None,
    });
    assert_eq!(
        project_resume_capability(&codex, &mismatched),
        AgentResumeCapability::MetadataUnavailable
    );

    let custom_id = AgentSessionId::new();
    let custom_root = format!("C:/fixture/.kerminal/agents/sessions/{custom_id}");
    let custom = AgentSession::new(
        custom_id,
        AgentProvider::Custom,
        None,
        "Custom capability fixture".to_owned(),
        "2026-10-19T00:00:00Z".to_owned(),
        "C:/fixture/.kerminal".to_owned(),
        custom_root.clone(),
        AgentSessionLaunch {
            command_label: "Custom".to_owned(),
            shell: "pwsh".to_owned(),
            args: Vec::new(),
            cwd: custom_root,
        },
        None,
        None,
    )
    .expect("custom capability fixture should satisfy session invariants");
    let custom_unsupported = AgentProviderSessionStatus::Available(AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Custom,
        provider_session_id: None,
        resume_command: None,
        resume_supported: false,
        last_resume_at: None,
    });
    assert_eq!(
        project_resume_capability(&custom, &custom_unsupported),
        AgentResumeCapability::Unsupported
    );
    let custom_supported = AgentProviderSessionStatus::Available(AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Custom,
        provider_session_id: None,
        resume_command: Some("custom-cli resume --session".to_owned()),
        resume_supported: true,
        last_resume_at: None,
    });
    assert_eq!(
        project_resume_capability(&custom, &custom_supported),
        AgentResumeCapability::Supported
    );
    let incomplete_custom = AgentProviderSessionStatus::Available(AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Custom,
        provider_session_id: None,
        resume_command: None,
        resume_supported: true,
        last_resume_at: None,
    });
    assert_eq!(
        project_resume_capability(&custom, &incomplete_custom),
        AgentResumeCapability::MetadataUnavailable
    );
}

/// Provider 读取故障仅隐藏能力；取消仍会终止原查询，避免把中断误报为成功。
///
/// @author kongweiguang
#[test]
fn resume_capability_projection_failure_is_soft_but_cancellation_is_not() {
    let session = endpoint_fixture_session();
    assert_eq!(
        project_resume_capability_result(
            &session,
            Err(AppError::configuration_corrupt("Agent provider metadata")),
        )
        .expect("optional metadata failure should not fail list/get projection"),
        AgentResumeCapability::MetadataUnavailable
    );
    assert_eq!(
        project_resume_capability_result(
            &session,
            Err(AppError::cancelled("Agent list was cancelled")),
        )
        .expect_err("request cancellation must remain visible")
        .public_code(),
        crate::PublicErrorCode::Cancelled
    );
}

/// 临时预览复用终端统一脱敏规则，并保留用户确认所需的空白和多行结构。
///
/// @author kongweiguang
#[test]
fn agent_text_preview_redacts_without_rewriting_surrounding_text() {
    let preview = project_agent_text_preview(
        "  command output\npassword=fixture-secret\tcomplete  ".to_owned(),
        512,
    )
    .expect("bounded plain text should produce a preview");

    assert!(preview.redacted);
    assert!(!preview.truncated);
    assert!(preview.text.starts_with("  command output\n"));
    assert!(preview.text.ends_with("\tcomplete  "));
    assert!(preview.text.contains("password=[已脱敏]"));
    assert!(!preview.text.contains("fixture-secret"));
    validate_agent_text_is_reviewed(&preview.text)
        .expect("the exact redacted preview is safe to submit without another rewrite");
    let debug_probe = AgentTextPreview {
        text: "fixture-secret".to_owned(),
        redacted: false,
        truncated: false,
    };
    assert!(!format!("{debug_probe:?}").contains("fixture-secret"));
}

/// 预览预算按 UTF-8 字节裁剪，且不接受零预算或超出一次原子提交能力的上限。
///
/// @author kongweiguang
#[test]
fn agent_text_preview_truncates_at_utf8_boundary_within_submission_budget() {
    let preview = project_agent_text_preview("你好".to_owned(), 4)
        .expect("preview can be shorter than the source text");

    assert_eq!(preview.text, "你");
    assert!(preview.truncated);
    assert!(!preview.redacted);
    assert!(project_agent_text_preview("text".to_owned(), 0).is_err());
    assert!(project_agent_text_preview("text".to_owned(), 32 * 1024 + 1).is_err());
}

/// 预览与提交允许 LF/TAB，但拒绝会改变终端控制流的其它控制字符。
///
/// @author kongweiguang
#[test]
fn agent_text_rejects_terminal_control_characters_but_keeps_newlines_and_tabs() {
    validate_agent_text_controls("first line\nsecond\tcolumn")
        .expect("visible multiline transcript characters remain supported");
    for forbidden in ["\r", "\0", "\u{1b}", "\u{7f}"] {
        assert!(validate_agent_text_controls(forbidden).is_err());
        assert!(project_agent_text_preview(forbidden.to_owned(), 32).is_err());
    }
    assert!(validate_agent_text_is_reviewed("password=fixture-secret").is_err());
}
