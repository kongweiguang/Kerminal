//! Agent 外置文档生成策略的回归测试。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_domain::{
    agents::{AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch},
    identity::AgentSessionId,
};
use tokio_util::sync::CancellationToken;
use toml_edit::DocumentMut;

use crate::{
    AppError, AppResult,
    agents::{
        AgentProviderSessionSnapshot, AgentWorkspaceDocument, AgentWorkspaceDocumentContent,
        AgentWorkspaceDocumentKind, AgentWorkspaceDocumentSnapshot, AgentWorkspaceRepository,
        AgentWorkspaceRevision, AgentWorkspaceSessionSnapshot, AgentWorkspaceWriteReceipt,
    },
};

use super::{agent_instructions, codex_config};

struct CodexConfigRepository {
    session_id: AgentSessionId,
    snapshot: AgentWorkspaceDocumentSnapshot,
}

#[async_trait]
impl AgentWorkspaceRepository for CodexConfigRepository {
    /// 测试仓储只提供 Codex 文档的固定读取快照，不构造文件系统状态。
    fn workspace_root(&self) -> String {
        "C:/test/kerminal".to_owned()
    }

    /// 未使用的创建端口必须显式失败，避免测试 fake 静默伪造持久化成功。
    async fn create_session_root(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<String> {
        Err(AppError::unsupported("测试仓储不支持创建会话目录"))
    }

    /// 未使用的回滚端口显式失败，保持 fake 的能力边界可见。
    async fn rollback_created_session_root(
        &self,
        _agent_session_id: AgentSessionId,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试仓储不支持回滚会话目录"))
    }

    /// 聚合读取不属于本测试输入，避免 fake 提供虚构会话元数据。
    async fn read_session(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceSessionSnapshot> {
        Err(AppError::unsupported("测试仓储不支持读取会话聚合"))
    }

    /// 列表查询不属于本测试输入，避免 fake 误导调用方依赖空列表。
    async fn list_sessions(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceSessionSnapshot>> {
        Err(AppError::unsupported("测试仓储不支持列出会话"))
    }

    /// 只允许 helper 读取目标 session 的 Codex 配置，并原样返回同次读取的 CAS revision。
    async fn read_document(
        &self,
        agent_session_id: AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceDocumentSnapshot> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("测试读取已取消"));
        }
        if agent_session_id != self.session_id || kind != AgentWorkspaceDocumentKind::CodexConfig {
            return Err(AppError::invalid("测试读取目标与固定快照不匹配"));
        }
        Ok(self.snapshot.clone())
    }

    /// Provider 状态不参与 Codex patch 回归，拒绝提供虚构快照。
    async fn read_provider_session(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentProviderSessionSnapshot> {
        Err(AppError::unsupported("测试仓储不支持读取 Provider 元数据"))
    }

    /// Provider 写入不参与本测试，避免绕过真实 revision 验证。
    async fn write_provider_session(
        &self,
        _agent_session_id: AgentSessionId,
        _provider_session: AgentProviderSession,
        _expected_revision: AgentWorkspaceRevision,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceWriteReceipt> {
        Err(AppError::unsupported("测试仓储不支持写入 Provider 元数据"))
    }

    /// 批量提交不属于纯 patch 测试；CAS 输入由生成文档携带并在断言中核对。
    async fn write_documents(
        &self,
        _agent_session_id: AgentSessionId,
        _documents: Vec<AgentWorkspaceDocument>,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>> {
        Err(AppError::unsupported("测试仓储不支持提交文档"))
    }
}

/// 用合法领域会话驱动生产 helper，避免测试另造 Codex 配置 patch 逻辑。
fn codex_session() -> AgentSession {
    AgentSession::new(
        AgentSessionId::new(),
        AgentProvider::Codex,
        None,
        "Codex 回归会话".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "C:/test/kerminal".to_owned(),
        "C:/test/kerminal/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "C:/test/kerminal/agents/session".to_owned(),
        },
        None,
        None,
    )
    .expect("测试会话应满足领域不变量")
}

/// 外部 Agent 新会话应发现实际保留的 SSH、传输和显式终端入口，不能被引导调用旧容器工具。
///
/// @author kongweiguang
#[test]
fn agent_instructions_reference_the_retained_runtime_tools() {
    let session = codex_session();
    let instructions = agent_instructions(&session, "http://127.0.0.1:37657/mcp/agents/test");
    assert!(instructions.contains("`ssh.command`"));
    assert!(instructions.contains("`sftp.transfer.enqueue`"));
    assert!(instructions.contains("`sftp.transfer.list`"));
    assert!(instructions.contains("`terminal.create`"));
    assert!(!instructions.contains("`container.files."));
    assert!(!instructions.contains("`tmux."));
}

/// 真实 patch 应只更新 Kerminal 管理字段，保留注释、其他 server 和未知扩展键。
#[tokio::test]
async fn codex_config_patch_preserves_user_content_and_cas_revision() {
    let session = codex_session();
    let expected_revision = AgentWorkspaceRevision::Sha256("read-revision-41".to_owned());
    let source = r#"# user root comment
theme = "dark"
vendor_root = "keep-root"

# another MCP server remains user-owned
[mcp_servers.other]
command = "other-cli"
args = ["--keep"]
vendor_option = "keep-other"

# Kerminal section comment remains present
[mcp_servers.kerminal]
url = "http://127.0.0.1:3000/old"
default_tools_approval_mode = "manual"
tool_timeout_sec = 17
enabled = false
type = "stdio"
vendor_option = "keep-kerminal"
"#;
    let repository = CodexConfigRepository {
        session_id: session.agent_session_id.clone(),
        snapshot: AgentWorkspaceDocumentSnapshot {
            contents: Some(source.to_owned()),
            revision: expected_revision.clone(),
        },
    };

    let patched = codex_config(
        &repository,
        &session,
        "http://127.0.0.1:40137/mcp",
        CancellationToken::new(),
    )
    .await
    .expect("Codex patch 应保留合法用户配置并生成文档");

    assert_eq!(patched.kind, AgentWorkspaceDocumentKind::CodexConfig);
    assert_eq!(patched.expected_revision, expected_revision);
    let AgentWorkspaceDocumentContent::Text(patched_contents) = patched.content else {
        panic!("Codex configuration must remain a text document");
    };
    assert!(patched_contents.contains("# user root comment"));
    assert!(patched_contents.contains("# another MCP server remains user-owned"));
    assert!(patched_contents.contains("# Kerminal section comment remains present"));

    let document = patched_contents
        .parse::<DocumentMut>()
        .expect("patch 输出仍是有效 TOML");
    assert_eq!(document["theme"].as_str(), Some("dark"));
    assert_eq!(document["vendor_root"].as_str(), Some("keep-root"));
    assert_eq!(
        document["mcp_servers"]["other"]["command"].as_str(),
        Some("other-cli")
    );
    assert_eq!(
        document["mcp_servers"]["other"]["args"][0].as_str(),
        Some("--keep")
    );
    assert_eq!(
        document["mcp_servers"]["other"]["vendor_option"].as_str(),
        Some("keep-other")
    );
    assert_eq!(
        document["mcp_servers"]["kerminal"]["url"].as_str(),
        Some("http://127.0.0.1:40137/mcp")
    );
    assert_eq!(
        document["mcp_servers"]["kerminal"]["default_tools_approval_mode"].as_str(),
        Some("prompt")
    );
    assert_eq!(
        document["mcp_servers"]["kerminal"]["tool_timeout_sec"].as_integer(),
        Some(60)
    );
    assert_eq!(
        document["mcp_servers"]["kerminal"]["enabled"].as_bool(),
        Some(true)
    );
    assert_eq!(
        document["mcp_servers"]["kerminal"]["type"].as_str(),
        Some("stdio")
    );
    assert_eq!(
        document["mcp_servers"]["kerminal"]["vendor_option"].as_str(),
        Some("keep-kerminal")
    );
}

/// Debug 只显示文档种类，不得展开用户正文或 CAS revision 内容。
#[test]
fn workspace_document_debug_redacts_text_and_revision() {
    let document = AgentWorkspaceDocument {
        kind: AgentWorkspaceDocumentKind::AgentInstructions,
        content: AgentWorkspaceDocumentContent::Text("secret-token-value".to_owned()),
        expected_revision: AgentWorkspaceRevision::Sha256("private-revision".to_owned()),
    };

    let debug = format!("{document:?}");

    assert!(!debug.contains("secret-token-value"));
    assert!(!debug.contains("private-revision"));
    assert!(debug.contains("Text"));
}
