//! Agent workspace repository 测试；只操作独立临时配置根。
//!
//! @author kongweiguang

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use kerminal_application::agents::{
    AgentProviderSessionStatus, AgentResumeCheckpointPhase, AgentWorkspaceDocument,
    AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind, AgentWorkspaceDocumentSnapshot,
    AgentWorkspaceRepository, AgentWorkspaceRevision, CommitAgentResumeCheckpointRequest,
    PrepareAgentResumeCheckpointRequest,
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch, AgentSessionScope,
    },
    identity::{AgentSessionId, SessionId},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::FileAgentWorkspaceRepository;

struct TestRoot(PathBuf);

impl TestRoot {
    /// 为并行测试创建唯一目录，避免引入仅供夹具使用的额外 package dependency。
    fn new() -> Self {
        let nonce = Uuid::new_v4();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time follows the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kerminal-agent-repo-{timestamp}-{nonce}"));
        fs::create_dir_all(&path).expect("temporary config root is created");
        Self(path)
    }

    /// 暴露临时目录作为配置根，不返回可由调用方拼接的工作区路径。
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestRoot {
    /// 测试结束后只删除自己以 UUID 创建的根目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 为 adapter 测试创建独立临时配置根，不读取或修改用户的 `.kerminal`。
fn test_repository() -> (TestRoot, FileAgentWorkspaceRepository) {
    let root = TestRoot::new();
    let repository = FileAgentWorkspaceRepository::new(PathBuf::from(root.path()));
    (root, repository)
}

/// 为当前持久化回归构造完整 scope，避免把合法暂态缺省写成损坏文件。
///
/// @author kongweiguang
fn valid_session(agent_session_id: AgentSessionId) -> AgentSession {
    AgentSession::new(
        agent_session_id,
        AgentProvider::Codex,
        None,
        "Repository test session".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "C:/test/kerminal".to_owned(),
        "C:/test/kerminal/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "C:/test/kerminal/agents/session".to_owned(),
        },
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("test session satisfies the domain invariants")
}

/// 为 checkpoint 测试构造仅用于验证脱敏的恢复命令，不涉及实际 CLI 调用。
///
/// @author kongweiguang
fn valid_provider_session() -> AgentProviderSession {
    AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Codex,
        provider_session_id: Some("provider-session-test-id".to_owned()),
        resume_command: Some("codex resume --secret-test-command".to_owned()),
        resume_supported: true,
        last_resume_at: None,
    }
}

/// 在隔离临时根内建立 session/provider 基线，返回两份精确读取 revision 与 typed 值。
///
/// @author kongweiguang
async fn seed_resume_documents(
    repository: &FileAgentWorkspaceRepository,
    agent_session_id: &AgentSessionId,
) -> (
    AgentSession,
    AgentProviderSession,
    AgentWorkspaceRevision,
    AgentWorkspaceRevision,
) {
    repository
        .create_session_root(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("session root is created");
    let session = valid_session(agent_session_id.clone());
    let provider = valid_provider_session();
    repository
        .write_documents(
            agent_session_id.clone(),
            vec![
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::Session,
                    content: AgentWorkspaceDocumentContent::Session(Box::new(session.clone())),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::Provider,
                    content: AgentWorkspaceDocumentContent::Provider(provider.clone()),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
            ],
            CancellationToken::new(),
        )
        .await
        .expect("session and provider bases are committed");
    let session_snapshot = repository
        .read_session(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("session base is readable");
    let provider_snapshot = repository
        .read_provider_session(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("provider base is readable");
    let AgentProviderSessionStatus::Available(provider) = provider_snapshot.status else {
        panic!("provider base is available");
    };
    (
        session_snapshot.session,
        provider,
        session_snapshot.revision,
        provider_snapshot.revision,
    )
}

/// 构造与读取基线绑定的 resume marker，测试不手写磁盘 schema 或跳过仓储 CAS。
///
/// @author kongweiguang
async fn prepare_resume_checkpoint(
    repository: &FileAgentWorkspaceRepository,
    agent_session_id: &AgentSessionId,
    session: AgentSession,
    provider: AgentProviderSession,
    base_session_revision: AgentWorkspaceRevision,
    base_provider_revision: AgentWorkspaceRevision,
) -> kerminal_application::agents::AgentResumeCheckpointSnapshot {
    repository
        .prepare_resume_checkpoint(
            PrepareAgentResumeCheckpointRequest {
                agent_session_id: agent_session_id.clone(),
                attempt_id: Uuid::new_v4(),
                terminal_session_id: SessionId::new(),
                base_session_revision,
                base_provider_revision,
                intended_session: session,
                intended_provider: provider,
            },
            CancellationToken::new(),
        )
        .await
        .expect("resume checkpoint is durable")
}

/// 通过原 checkpoint 和原 typed 意图提交恢复结果，覆盖真实 application repository 方法。
///
/// @author kongweiguang
async fn commit_resume_checkpoint(
    repository: &FileAgentWorkspaceRepository,
    agent_session_id: &AgentSessionId,
    prepared: kerminal_application::agents::AgentResumeCheckpointSnapshot,
    session: AgentSession,
    provider: AgentProviderSession,
) -> kerminal_application::agents::AgentResumeCheckpointSnapshot {
    repository
        .commit_resume_checkpoint(
            CommitAgentResumeCheckpointRequest {
                agent_session_id: agent_session_id.clone(),
                prepared,
                intended_session: session,
                intended_provider: provider,
            },
            CancellationToken::new(),
        )
        .await
        .expect("resume intended metadata and committed marker are durable")
}

/// 固定 document kind 往返到受管路径，且不要求测试调用方构造原生路径字符串。
#[tokio::test]
async fn fixed_documents_round_trip_under_session_root() {
    let (_root, repository) = test_repository();
    let agent_id = AgentSessionId::new();
    let session_root = repository
        .create_session_root(agent_id.clone(), CancellationToken::new())
        .await
        .expect("session root is created");
    assert!(PathBuf::from(&session_root).is_absolute());

    repository
        .write_documents(
            agent_id.clone(),
            vec![
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::AgentInstructions,
                    content: AgentWorkspaceDocumentContent::Text("managed rules\n".to_owned()),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::McpEndpointContext,
                    content: AgentWorkspaceDocumentContent::Text(
                        "{\"endpoint\":\"http://127.0.0.1:3000/mcp\"}".to_owned(),
                    ),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
            ],
            CancellationToken::new(),
        )
        .await
        .expect("document transaction is committed");

    assert_eq!(
        repository
            .read_document(
                agent_id.clone(),
                AgentWorkspaceDocumentKind::AgentInstructions,
                CancellationToken::new(),
            )
            .await
            .expect("instructions are readable")
            .contents
            .as_deref(),
        Some("managed rules\n")
    );
    assert_eq!(
        repository
            .read_document(
                agent_id,
                AgentWorkspaceDocumentKind::McpEndpointContext,
                CancellationToken::new(),
            )
            .await
            .expect("context is readable")
            .contents
            .as_deref(),
        Some("{\"endpoint\":\"http://127.0.0.1:3000/mcp\"}")
    );
}

/// 同一事务重复声明一个受管路径时应在写盘前拒绝，不产生 journal 或 staging。
#[tokio::test]
async fn duplicate_document_kind_is_rejected_before_writing() {
    let (_root, repository) = test_repository();
    let agent_id = AgentSessionId::new();
    repository
        .create_session_root(agent_id.clone(), CancellationToken::new())
        .await
        .expect("session root is created");

    let result = repository
        .write_documents(
            agent_id.clone(),
            vec![
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::AgentInstructions,
                    content: AgentWorkspaceDocumentContent::Text("first".to_owned()),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::AgentInstructions,
                    content: AgentWorkspaceDocumentContent::Text("second".to_owned()),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
            ],
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        repository
            .read_document(
                agent_id,
                AgentWorkspaceDocumentKind::AgentInstructions,
                CancellationToken::new(),
            )
            .await
            .expect("missing document remains readable as absent"),
        AgentWorkspaceDocumentSnapshot {
            contents: None,
            revision: AgentWorkspaceRevision::Missing,
        }
    );
}

/// 应用传入读取时的磁盘 revision；外部编辑后旧基线必须冲突且保留编辑者正文。
#[tokio::test]
async fn stale_revision_preserves_external_edit() {
    let (_root, repository) = test_repository();
    let agent_id = AgentSessionId::new();
    repository
        .create_session_root(agent_id.clone(), CancellationToken::new())
        .await
        .expect("session root is created");
    repository
        .write_documents(
            agent_id.clone(),
            vec![AgentWorkspaceDocument {
                kind: AgentWorkspaceDocumentKind::AgentInstructions,
                content: AgentWorkspaceDocumentContent::Text("initial".to_owned()),
                expected_revision: AgentWorkspaceRevision::Missing,
            }],
            CancellationToken::new(),
        )
        .await
        .expect("initial document is committed");
    let snapshot = repository
        .read_document(
            agent_id.clone(),
            AgentWorkspaceDocumentKind::AgentInstructions,
            CancellationToken::new(),
        )
        .await
        .expect("document snapshot is readable");
    let path = repository
        .document_path(&agent_id, AgentWorkspaceDocumentKind::AgentInstructions)
        .expect("kind maps to fixed path");
    fs::write(&path, "external edit").expect("external editor replaces the document");

    let result = repository
        .write_documents(
            agent_id.clone(),
            vec![AgentWorkspaceDocument {
                kind: AgentWorkspaceDocumentKind::AgentInstructions,
                content: AgentWorkspaceDocumentContent::Text("stale overwrite".to_owned()),
                expected_revision: snapshot.revision,
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(&path).expect("external edit remains"),
        "external edit"
    );
}

/// 文件路径被外部替换为目录时，提交在 staging 前失败且不触碰同组 session 文档。
///
/// @author kongweiguang
#[tokio::test]
async fn invalid_target_path_fails_before_multi_document_staging() {
    let (_root, repository) = test_repository();
    let agent_id = AgentSessionId::new();
    repository
        .create_session_root(agent_id.clone(), CancellationToken::new())
        .await
        .expect("session root is created");
    let instructions = repository
        .document_path(&agent_id, AgentWorkspaceDocumentKind::AgentInstructions)
        .expect("kind maps to fixed path");
    tokio::fs::create_dir(&instructions)
        .await
        .expect("directory conflict is prepared");

    let result = repository
        .write_documents(
            agent_id.clone(),
            vec![
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::Session,
                    content: AgentWorkspaceDocumentContent::Session(Box::new(valid_session(
                        agent_id.clone(),
                    ))),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
                AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::AgentInstructions,
                    content: AgentWorkspaceDocumentContent::Text("new instructions".to_owned()),
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
            ],
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert!(instructions.is_dir());
    assert_eq!(
        repository
            .read_document(
                agent_id.clone(),
                AgentWorkspaceDocumentKind::Session,
                CancellationToken::new(),
            )
            .await
            .expect("failed transaction leaves no session file"),
        AgentWorkspaceDocumentSnapshot {
            contents: None,
            revision: AgentWorkspaceRevision::Missing,
        }
    );
}

/// 预先取消的创建必须在 filesystem adapter 触碰配置根前失败。
#[tokio::test]
async fn cancelled_create_does_not_create_session_directory() {
    let (_root, repository) = test_repository();
    let agent_id = AgentSessionId::new();
    let session_root = repository
        .document_path(&agent_id, AgentWorkspaceDocumentKind::Session)
        .expect("session kind maps to a fixed path")
        .parent()
        .expect("session metadata has a parent")
        .to_path_buf();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(
        repository
            .create_session_root(agent_id, cancellation)
            .await
            .is_err()
    );
    assert!(!session_root.exists());
}

/// session 工作区里的目录不能伪装受管普通文件，也不能被读接口跟随。
#[tokio::test]
async fn document_read_rejects_directory_at_file_path() {
    let (_root, repository) = test_repository();
    let agent_id = AgentSessionId::new();
    repository
        .create_session_root(agent_id.clone(), CancellationToken::new())
        .await
        .expect("session root is created");
    let instructions = repository
        .document_path(&agent_id, AgentWorkspaceDocumentKind::AgentInstructions)
        .expect("kind maps to fixed path");
    tokio::fs::create_dir(instructions)
        .await
        .expect("directory conflict is prepared");

    assert!(
        repository
            .read_document(
                agent_id,
                AgentWorkspaceDocumentKind::AgentInstructions,
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
}

/// 验证 Prepared→Committed→CAS 清除闭环，checkpoint 仅保留身份和摘要而不复制命令或文档正文。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_checkpoint_is_redacted_committed_and_cleared_by_exact_revision() {
    let (root, repository) = test_repository();
    let agent_session_id = AgentSessionId::new();
    let (session, provider, base_session_revision, base_provider_revision) =
        seed_resume_documents(&repository, &agent_session_id).await;
    let mut intended_provider = provider;
    intended_provider.last_resume_at = Some("2026-10-20T00:00:00Z".to_owned());
    let attempt_id = Uuid::new_v4();
    let request = PrepareAgentResumeCheckpointRequest {
        agent_session_id: agent_session_id.clone(),
        attempt_id,
        terminal_session_id: SessionId::new(),
        base_session_revision,
        base_provider_revision,
        intended_session: session.clone(),
        intended_provider: intended_provider.clone(),
    };
    let prepared = repository
        .prepare_resume_checkpoint(request, CancellationToken::new())
        .await
        .expect("the marker is durable before runtime creation");
    assert_eq!(
        prepared.checkpoint.phase,
        AgentResumeCheckpointPhase::Prepared
    );

    let marker = root
        .path()
        .join("agents/sessions")
        .join(agent_session_id.as_str())
        .join(".agent-resume-checkpoint.json");
    let marker_text = fs::read_to_string(&marker).expect("checkpoint marker is readable");
    assert!(!marker_text.contains("resumeCommand"));
    assert!(!marker_text.contains("provider-session-test-id"));
    assert!(!marker_text.contains("secret-test-command"));

    let readback = repository
        .read_resume_checkpoint(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("checkpoint read is valid")
        .expect("prepared marker remains visible");
    assert_eq!(readback, prepared);

    let committed = repository
        .commit_resume_checkpoint(
            CommitAgentResumeCheckpointRequest {
                agent_session_id: agent_session_id.clone(),
                prepared: prepared.clone(),
                intended_session: session,
                intended_provider: intended_provider.clone(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("session, provider and committed marker use one journal");
    assert_eq!(
        committed.checkpoint.phase,
        AgentResumeCheckpointPhase::Committed
    );
    assert_ne!(committed.revision, prepared.revision);
    let AgentProviderSessionStatus::Available(read_provider) = repository
        .read_provider_session(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("committed provider is readable")
        .status
    else {
        panic!("provider remains valid after resume commit");
    };
    assert_eq!(read_provider, intended_provider);

    assert!(
        repository
            .clear_resume_checkpoint(agent_session_id.clone(), prepared, CancellationToken::new(),)
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .read_resume_checkpoint(agent_session_id.clone(), CancellationToken::new())
            .await
            .expect("stale clear leaves marker readable"),
        Some(committed.clone())
    );
    repository
        .clear_resume_checkpoint(
            agent_session_id.clone(),
            committed,
            CancellationToken::new(),
        )
        .await
        .expect("the exact committed marker is cleared");
    assert_eq!(
        repository
            .read_resume_checkpoint(agent_session_id, CancellationToken::new())
            .await
            .expect("cleared marker read succeeds"),
        None
    );
}

/// 启动失败清理仅在 Prepared 的 Session/Provider 仍匹配原 base 时允许删除 marker。
///
/// @author kongweiguang
#[tokio::test]
async fn prepared_resume_checkpoint_clear_requires_unchanged_base_documents() {
    let (_root, repository) = test_repository();
    let agent_session_id = AgentSessionId::new();
    let (session, provider, session_revision, provider_revision) =
        seed_resume_documents(&repository, &agent_session_id).await;
    let prepared = prepare_resume_checkpoint(
        &repository,
        &agent_session_id,
        session,
        provider,
        session_revision,
        provider_revision,
    )
    .await;

    let mut external_session = valid_session(agent_session_id.clone());
    external_session.title = "Prepared external edit".to_owned();
    external_session.updated_at = "2026-10-20T00:02:00Z".to_owned();
    repository
        .write_documents(
            agent_session_id.clone(),
            vec![AgentWorkspaceDocument {
                kind: AgentWorkspaceDocumentKind::Session,
                content: AgentWorkspaceDocumentContent::Session(Box::new(external_session.clone())),
                expected_revision: prepared.checkpoint.base_session_revision.clone(),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("external edit advances the Prepared base");
    assert!(
        repository
            .clear_resume_checkpoint(
                agent_session_id.clone(),
                prepared.clone(),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .read_resume_checkpoint(agent_session_id.clone(), CancellationToken::new())
            .await
            .expect("conflicting Prepared marker remains readable"),
        Some(prepared)
    );
    assert_eq!(
        repository
            .read_session(agent_session_id, CancellationToken::new())
            .await
            .expect("external session remains readable")
            .session,
        external_session
    );

    let unchanged_id = AgentSessionId::new();
    let (session, provider, session_revision, provider_revision) =
        seed_resume_documents(&repository, &unchanged_id).await;
    let unchanged = prepare_resume_checkpoint(
        &repository,
        &unchanged_id,
        session,
        provider,
        session_revision,
        provider_revision,
    )
    .await;
    repository
        .clear_resume_checkpoint(unchanged_id.clone(), unchanged, CancellationToken::new())
        .await
        .expect("launch failure cleanup may clear an unchanged Prepared checkpoint");
    assert_eq!(
        repository
            .read_resume_checkpoint(unchanged_id, CancellationToken::new())
            .await
            .expect("cleared Prepared marker read succeeds"),
        None
    );
}

/// 已完成恢复后仅当 Session/Provider 仍匹配 intended revisions 才能清除 Committed marker。
///
/// @author kongweiguang
#[tokio::test]
async fn committed_resume_checkpoint_clear_preserves_third_provider_revision() {
    let (_root, repository) = test_repository();
    let agent_session_id = AgentSessionId::new();
    let (session, provider, session_revision, provider_revision) =
        seed_resume_documents(&repository, &agent_session_id).await;
    let mut intended_provider = provider;
    intended_provider.last_resume_at = Some("2026-10-20T00:03:00Z".to_owned());
    let prepared = prepare_resume_checkpoint(
        &repository,
        &agent_session_id,
        session.clone(),
        intended_provider.clone(),
        session_revision,
        provider_revision,
    )
    .await;
    let committed = commit_resume_checkpoint(
        &repository,
        &agent_session_id,
        prepared,
        session,
        intended_provider.clone(),
    )
    .await;

    let mut external_provider = intended_provider;
    external_provider.last_resume_at = Some("third-party provider revision".to_owned());
    repository
        .write_provider_session(
            agent_session_id.clone(),
            external_provider.clone(),
            committed.checkpoint.intended_provider_revision.clone(),
            CancellationToken::new(),
        )
        .await
        .expect("external provider edit advances the Committed intent");
    assert!(
        repository
            .clear_resume_checkpoint(
                agent_session_id.clone(),
                committed.clone(),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .read_resume_checkpoint(agent_session_id.clone(), CancellationToken::new())
            .await
            .expect("conflicting Committed marker remains readable"),
        Some(committed)
    );
    let AgentProviderSessionStatus::Available(read_provider) = repository
        .read_provider_session(agent_session_id, CancellationToken::new())
        .await
        .expect("external provider remains readable")
        .status
    else {
        panic!("external provider edit remains valid");
    };
    assert_eq!(read_provider, external_provider);
}

/// 外部 session 改动必须阻止旧 Prepared 的三文件提交，并保留 checkpoint 供人工核对。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_checkpoint_commit_conflict_preserves_external_session_and_prepared_marker() {
    let (_root, repository) = test_repository();
    let agent_session_id = AgentSessionId::new();
    let (session, provider, base_session_revision, base_provider_revision) =
        seed_resume_documents(&repository, &agent_session_id).await;
    let request = PrepareAgentResumeCheckpointRequest {
        agent_session_id: agent_session_id.clone(),
        attempt_id: Uuid::new_v4(),
        terminal_session_id: SessionId::new(),
        base_session_revision,
        base_provider_revision,
        intended_session: session.clone(),
        intended_provider: provider.clone(),
    };
    let prepared = repository
        .prepare_resume_checkpoint(request, CancellationToken::new())
        .await
        .expect("checkpoint is durable");

    let mut external_session = valid_session(agent_session_id.clone());
    external_session.title = "External editor wins".to_owned();
    external_session.updated_at = "2026-10-20T00:01:00Z".to_owned();
    repository
        .write_documents(
            agent_session_id.clone(),
            vec![AgentWorkspaceDocument {
                kind: AgentWorkspaceDocumentKind::Session,
                content: AgentWorkspaceDocumentContent::Session(Box::new(external_session.clone())),
                expected_revision: prepared.checkpoint.base_session_revision.clone(),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("another writer updates the session through its own CAS");

    let result = repository
        .commit_resume_checkpoint(
            CommitAgentResumeCheckpointRequest {
                agent_session_id: agent_session_id.clone(),
                prepared: prepared.clone(),
                intended_session: session,
                intended_provider: provider,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        repository
            .read_resume_checkpoint(agent_session_id.clone(), CancellationToken::new())
            .await
            .expect("prepared checkpoint is retained"),
        Some(prepared)
    );
    assert_eq!(
        repository
            .read_session(agent_session_id, CancellationToken::new())
            .await
            .expect("external session remains readable")
            .session,
        external_session
    );
}

/// 预备阶段的 base CAS 冲突必须发生在 marker 安装前，确保调用方不能据旧元数据启动 CLI。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_checkpoint_prepare_conflict_does_not_create_marker() {
    let (root, repository) = test_repository();
    let agent_session_id = AgentSessionId::new();
    let (session, mut provider, base_session_revision, base_provider_revision) =
        seed_resume_documents(&repository, &agent_session_id).await;
    provider.last_resume_at = Some("external update".to_owned());
    repository
        .write_provider_session(
            agent_session_id.clone(),
            provider.clone(),
            base_provider_revision.clone(),
            CancellationToken::new(),
        )
        .await
        .expect("external provider update advances the CAS base");

    let result = repository
        .prepare_resume_checkpoint(
            PrepareAgentResumeCheckpointRequest {
                agent_session_id: agent_session_id.clone(),
                attempt_id: Uuid::new_v4(),
                terminal_session_id: SessionId::new(),
                base_session_revision,
                base_provider_revision,
                intended_session: session,
                intended_provider: provider,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    let marker = root
        .path()
        .join("agents/sessions")
        .join(agent_session_id.as_str())
        .join(".agent-resume-checkpoint.json");
    assert!(!marker.exists());
}
