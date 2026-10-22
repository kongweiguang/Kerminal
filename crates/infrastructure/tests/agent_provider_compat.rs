//! Agent 当前 session 边界、provider 元数据和 fail-closed 契约测试。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::agents::{
    AgentProviderSessionStatus, AgentWorkspaceDocument, AgentWorkspaceDocumentContent,
    AgentWorkspaceDocumentKind, AgentWorkspaceRepository, AgentWorkspaceRevision,
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch, AgentSessionScope,
    },
    identity::AgentSessionId,
};
use kerminal_infrastructure::agents::FileAgentWorkspaceRepository;
use tokio_util::sync::CancellationToken;

const LEGACY_SESSION: &str =
    include_str!("../../../tests/fixtures/agents/agent_session_v0_3_45.toml");
const LEGACY_PROVIDER: &str =
    include_str!("../../../tests/fixtures/agents/provider_session_v0_3_45.toml");
const SESSION_ID: &str = "ags-fixture";

struct IsolatedRoot {
    root: PathBuf,
    _directory: tempfile::TempDir,
}

impl IsolatedRoot {
    /// 每个契约用例使用独立临时数据根，不接触用户配置目录。
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create isolated data root");
        Self {
            root: directory.path().to_path_buf(),
            _directory: directory,
        }
    }

    /// 将冻结旧版 fixture 放到真实 session/provider 路径中供 adapter 读取。
    fn seed_legacy_session(&self, session: &str, provider: Option<&str>) {
        let session_root = self.root.join("agents").join("sessions").join(SESSION_ID);
        fs::create_dir_all(&session_root).expect("create synthetic session directory");
        fs::write(session_root.join("session.toml"), session).expect("write legacy session");
        if let Some(provider) = provider {
            fs::write(session_root.join("provider.toml"), provider)
                .expect("write legacy provider metadata");
        }
    }

    /// 返回固定兼容路径，便于断言失败后原始字节未被覆盖。
    fn session_file(&self, name: &str) -> PathBuf {
        self.root
            .join("agents")
            .join("sessions")
            .join(SESSION_ID)
            .join(name)
    }
}

/// 用当前仓储写入器建立有效 session，避免 provider 写入测试被旧格式 session 阻断。
///
/// @author kongweiguang
async fn seed_current_session(root: &IsolatedRoot) {
    let repository = FileAgentWorkspaceRepository::new(root.root.clone());
    repository
        .create_session_root(session_id(), CancellationToken::new())
        .await
        .expect("create current session directory");
    repository
        .write_documents(
            session_id(),
            vec![AgentWorkspaceDocument {
                kind: AgentWorkspaceDocumentKind::Session,
                content: AgentWorkspaceDocumentContent::Session(Box::new(new_session())),
                expected_revision: AgentWorkspaceRevision::Missing,
            }],
            CancellationToken::new(),
        )
        .await
        .expect("write current session through repository");
}

/// 将 synthetic baseline ID 转成应用端口使用的 opaque identity。
fn session_id() -> AgentSessionId {
    AgentSessionId::parse(SESSION_ID).expect("valid synthetic session id")
}

/// 创建一个不包含 provider 私密内容的 schema 1 值，用于失败写入尝试。
fn replacement_provider() -> AgentProviderSession {
    AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Codex,
        provider_session_id: Some("replacement-session-fixture".to_owned()),
        resume_command: Some("codex resume --last".to_owned()),
        resume_supported: true,
        last_resume_at: None,
    }
}

/// 为当前格式写入构造完整 scope，避免测试把暂态缺省状态持久化。
///
/// @author kongweiguang
fn new_session() -> AgentSession {
    AgentSession::new(
        session_id(),
        AgentProvider::Codex,
        None,
        "Provider compatibility session".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "C:/test/kerminal".to_owned(),
        "C:/test/kerminal/agents/ags-fixture".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "C:/test/kerminal/agents/ags-fixture".to_owned(),
        },
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("test session satisfies the domain invariants")
}

#[tokio::test]
/// 普通 reader 拒绝旧 session 且不覆盖原文；provider 读取需在迁移后独立验收。
///
/// @author kongweiguang
async fn ordinary_reader_rejects_legacy_session_without_rewriting_it() {
    let root = IsolatedRoot::new();
    root.seed_legacy_session(LEGACY_SESSION, Some(LEGACY_PROVIDER));
    let repository = FileAgentWorkspaceRepository::new(root.root.clone());

    let original_session = fs::read(root.session_file("session.toml")).unwrap();
    let error = repository
        .read_session(session_id(), CancellationToken::new())
        .await
        .expect_err("legacy session data must be migrated before current reads");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::ConfigurationCorrupt
    );
    assert_eq!(
        fs::read(root.session_file("session.toml")).unwrap(),
        original_session,
        "ordinary reads must not rewrite migration input"
    );
}

#[tokio::test]
/// 新 session 与 provider 同批写入时共享同一个 journal，provider 与 session 身份必须相符。
///
/// @author kongweiguang
async fn writes_session_and_provider_as_one_valid_pair() {
    let root = IsolatedRoot::new();
    let repository = FileAgentWorkspaceRepository::new(root.root.clone());
    repository
        .create_session_root(session_id(), CancellationToken::new())
        .await
        .expect("create isolated session directory");
    let provider: AgentProviderSession = toml::from_str(LEGACY_PROVIDER).unwrap();
    let session = new_session();

    repository
        .write_documents(
            session_id(),
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
        .expect("commit new session and provider in one journal transaction");
    assert!(root.session_file("session.toml").is_file());
    assert!(root.session_file("provider.toml").is_file());
    let provider_snapshot = repository
        .read_provider_session(session_id(), CancellationToken::new())
        .await
        .expect("read the committed provider document");
    let AgentProviderSessionStatus::Available(mut provider_value) = provider_snapshot.status else {
        panic!("committed provider metadata must be available");
    };
    assert_eq!(
        provider_value.provider_session_id.as_deref(),
        Some("provider-session-fixture")
    );
    assert_eq!(
        provider_value.resume_command.as_deref(),
        Some("codex resume --last")
    );
    let debug = format!("{provider_value:?}");
    assert!(!debug.contains("provider-session-fixture"));
    assert!(!debug.contains("codex resume --last"));
    provider_value.last_resume_at = Some("1720000010".to_owned());
    repository
        .write_provider_session(
            session_id(),
            provider_value,
            provider_snapshot.revision,
            CancellationToken::new(),
        )
        .await
        .expect("update current provider metadata through the shared journal");
    let rewritten_provider = fs::read_to_string(root.session_file("provider.toml")).unwrap();
    assert!(rewritten_provider.contains("provider_session_id = \"provider-session-fixture\""));
    assert!(rewritten_provider.contains("resume_command = \"codex resume --last\""));
    assert!(rewritten_provider.contains("last_resume_at = \"1720000010\""));

    let mismatch_root = IsolatedRoot::new();
    let mismatch_repository = FileAgentWorkspaceRepository::new(mismatch_root.root.clone());
    mismatch_repository
        .create_session_root(session_id(), CancellationToken::new())
        .await
        .expect("create second isolated session directory");
    let mut mismatched_provider = provider;
    mismatched_provider.provider = AgentProvider::Claude;
    assert!(
        mismatch_repository
            .write_documents(
                session_id(),
                vec![
                    AgentWorkspaceDocument {
                        kind: AgentWorkspaceDocumentKind::Session,
                        content: AgentWorkspaceDocumentContent::Session(Box::new(new_session())),
                        expected_revision: AgentWorkspaceRevision::Missing,
                    },
                    AgentWorkspaceDocument {
                        kind: AgentWorkspaceDocumentKind::Provider,
                        content: AgentWorkspaceDocumentContent::Provider(mismatched_provider),
                        expected_revision: AgentWorkspaceRevision::Missing,
                    },
                ],
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert!(!mismatch_root.session_file("session.toml").exists());
    assert!(!mismatch_root.session_file("provider.toml").exists());
}

#[tokio::test]
/// 未来 schema 和损坏 provider 均可见且拒绝普通 CAS 写入，保留原始字节等待显式恢复。
///
/// @author kongweiguang
async fn future_or_malformed_provider_is_never_replaced_by_normal_write() {
    let cases = [
        (
            "schema_version = 99\nprovider = \"codex\"\nresume_supported = true\n",
            AgentProviderSessionStatus::UnsupportedSchemaVersion(99),
        ),
        (
            "schema_version = 99\nprovider = \"codex\"\nresume_supported = true\nnew_resume_field = \"preserve-me\"\n",
            AgentProviderSessionStatus::UnsupportedSchemaVersion(99),
        ),
        (
            "schema_version = 1\nprovider = \"codex\"\nresume_supported = nope\n",
            AgentProviderSessionStatus::Malformed,
        ),
        (
            "schema_version = 1\nprovider = \"claude\"\nresume_supported = true\n",
            AgentProviderSessionStatus::ProviderMismatch,
        ),
    ];
    for (contents, expected_status) in cases {
        let root = IsolatedRoot::new();
        seed_current_session(&root).await;
        fs::write(root.session_file("provider.toml"), contents)
            .expect("write synthetic malformed provider document");
        let repository = FileAgentWorkspaceRepository::new(root.root.clone());
        let snapshot = repository
            .read_provider_session(session_id(), CancellationToken::new())
            .await
            .expect("classify provider file");
        assert_eq!(snapshot.status, expected_status);
        let original = fs::read(root.session_file("provider.toml")).unwrap();

        assert!(
            repository
                .write_provider_session(
                    session_id(),
                    replacement_provider(),
                    snapshot.revision,
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(root.session_file("provider.toml")).unwrap(),
            original
        );
    }
}

#[tokio::test]
/// 已有合法 session 缺少 provider 文件时返回 Missing 与独立 CAS 基线。
///
/// @author kongweiguang
async fn missing_provider_is_not_confused_with_corrupt_provider() {
    let root = IsolatedRoot::new();
    seed_current_session(&root).await;
    let repository = FileAgentWorkspaceRepository::new(root.root.clone());

    let snapshot = repository
        .read_provider_session(session_id(), CancellationToken::new())
        .await
        .expect("read absent provider metadata");
    assert_eq!(snapshot.status, AgentProviderSessionStatus::Missing);
    assert_eq!(snapshot.revision, AgentWorkspaceRevision::Missing);
    assert!(snapshot.path.ends_with("provider.toml"));
}

#[tokio::test]
/// 坏或未来 session 无法借助已知 revision 覆盖，防止通用文档写入抹掉恢复证据。
///
/// @author kongweiguang
async fn future_session_schema_is_preserved_on_write_attempt() {
    let root = IsolatedRoot::new();
    let future_session = LEGACY_SESSION.replacen("schema_version = 1", "schema_version = 23", 1);
    root.seed_legacy_session(&future_session, Some(LEGACY_PROVIDER));
    let repository = FileAgentWorkspaceRepository::new(root.root.clone());
    let raw = repository
        .read_document(
            session_id(),
            AgentWorkspaceDocumentKind::Session,
            CancellationToken::new(),
        )
        .await
        .expect("raw revision remains available for diagnostics");
    let original = fs::read(root.session_file("session.toml")).unwrap();

    assert!(
        repository
            .write_documents(
                session_id(),
                vec![AgentWorkspaceDocument {
                    kind: AgentWorkspaceDocumentKind::Session,
                    content: AgentWorkspaceDocumentContent::Session(Box::new(new_session())),
                    expected_revision: raw.revision,
                }],
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(root.session_file("session.toml")).unwrap(),
        original
    );
}
