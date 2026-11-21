//! Agent 工作区模板按内置启停单元裁剪的回归测试。
//!
//! 模板是外部 Agent 的首要提示来源；插件关闭后若仍保留对应工具引用，Agent 会持续
//! 尝试不可用入口。这里直接驱生产 helper，避免测试另写一套文本规则。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_domain::{
    agents::{AgentProvider, AgentSession, AgentSessionLaunch},
    identity::AgentSessionId,
};

use crate::plugins::{PluginId, PluginRegistry, PluginRegistryBuilder};

use super::{agent_instructions_with, claude_instructions};

/// 测试复用生产模板入口，不再在 src 暴露 test-only 包装。
///
/// @author kongweiguang
fn agent_instructions(session: &AgentSession, endpoint: &str) -> String {
    agent_instructions_with(session, endpoint, None)
}

/// 用显式状态表冻结生产内置声明；其余单元保持默认启用。
///
/// @author kongweiguang
fn registry_with_disabled(disabled: &[&str]) -> PluginRegistry {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states = disabled
        .iter()
        .map(|id| (PluginId::new(*id).expect("valid builtin unit id"), false))
        .collect::<BTreeMap<_, _>>();
    builder
        .freeze_with_builtin_states(&states)
        .expect("启用视图应可冻结")
}

/// 复用合法领域会话，避免模板测试自行构造不满足不变量的输入。
///
/// @author kongweiguang
fn codex_session() -> AgentSession {
    AgentSession::new(
        AgentSessionId::new(),
        AgentProvider::Codex,
        None,
        "模板过滤会话".to_owned(),
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

/// 全启用时应先解释绑定终端的路由，再推荐独立的后台 SSH，避免模型把后台当成通用入口。
///
/// @author kongweiguang
#[test]
fn absent_registry_matches_full_enablement() {
    let session = codex_session();
    let endpoint = "http://127.0.0.1:37657/mcp/agents/test";
    let full = PluginRegistry::builtin().expect("内置声明应可冻结");

    let unfiltered = agent_instructions(&session, endpoint);
    let enabled = agent_instructions_with(&session, endpoint, Some(&full));
    assert_eq!(unfiltered, enabled);

    let unfiltered_claude = claude_instructions(endpoint, None);
    let enabled_claude = claude_instructions(endpoint, Some(&full));
    assert_eq!(unfiltered_claude, enabled_claude);

    for needle in [
        "`ssh.command`",
        "`sftp.transfer.enqueue`",
        "`sftp.transfer.list`",
        "`kerminal.agent.target_context`",
        "`terminal.create`",
    ] {
        assert!(unfiltered.contains(needle), "full template keeps {needle}");
    }

    let bound_pos = unfiltered
        .find("use `terminal.write` for commands on the bound terminal")
        .expect("bound terminal routing is present");
    let server_pos = unfiltered
        .find("Use `ssh.command` for one-shot server commands in background mode or on an explicitly requested different host")
        .expect("SSH advice respects bound terminal routing");
    assert!(bound_pos < server_pos);
    assert!(unfiltered.contains("`bindingGeneration`"));
    assert!(!unfiltered.contains("Run non-interactive server commands with `ssh.command`."));
}

/// 关闭 SSH 与 SFTP 时移除对应工具句，终端核心提示与文档结构保持完整。
///
/// @author kongweiguang
#[test]
fn disabled_ssh_and_sftp_trim_runtime_lines_only() {
    let session = codex_session();
    let endpoint = "http://127.0.0.1:37657/mcp/agents/test";
    let registry = registry_with_disabled(&["kerminal.ssh", "kerminal.sftp"]);

    let instructions = agent_instructions_with(&session, endpoint, Some(&registry));
    assert!(!instructions.contains("ssh.command"));
    assert!(!instructions.contains("sftp.transfer"));
    assert!(instructions.contains("`terminal.create`"));
    assert!(instructions.contains("`kerminal.help`"));
    assert!(instructions.contains("`kerminal.agent.target_context`"));
    // 裁剪后不得留下连续空行或空条目。
    assert!(!instructions.contains("\n\n\n"));
    assert!(!instructions.contains("\n- \n"));

    let claude = claude_instructions(endpoint, Some(&registry));
    assert!(!claude.contains("ssh.command"));
    assert!(!claude.contains("sftp.transfer"));
}

/// 关闭 Agent 单元时两种 provider 模板都不再推荐 target_context。
///
/// @author kongweiguang
#[test]
fn disabled_agents_remove_target_context_hints() {
    let session = codex_session();
    let endpoint = "http://127.0.0.1:37657/mcp/agents/test";
    let registry = registry_with_disabled(&["kerminal.agents"]);

    let instructions = agent_instructions_with(&session, endpoint, Some(&registry));
    assert!(!instructions.contains("target_context"));
    assert!(instructions.contains("`terminal.create`"));
    assert!(instructions.contains("`ssh.command`"));

    let claude = claude_instructions(endpoint, Some(&registry));
    assert!(!claude.contains("target_context"));
    assert!(claude.contains("@AGENTS.md"));
    assert!(claude.contains("MCP host policy"));
    assert!(!claude.contains("\n\n\n"));
}
