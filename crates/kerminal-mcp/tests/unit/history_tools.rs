//! `history.search` 的输入兼容与安全摘要测试。
//!
//! @author kongweiguang

use kerminal_domain::commands::{CommandHistoryEntry, CommandHistorySource, CommandHistoryTarget};

use crate::history_tools::{HistorySearchArgs, build_history_request, summarize_history};

/// 只填充历史值对象的必需字段，测试关注查询映射而非 SQLite 适配细节。
fn history_entry(command: String, target: CommandHistoryTarget) -> CommandHistoryEntry {
    CommandHistoryEntry {
        id: String::new(),
        command,
        source: CommandHistorySource::User,
        target,
        session_id: None,
        pane_id: None,
        tab_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd: None,
        shell: None,
        created_at: String::new(),
    }
}

/// MCP 参数保留大小写的冻结枚举和值字段，不把历史外的 Workflow source 扩展给外部 host。
#[test]
fn maps_frozen_history_filters_and_rejects_unpublished_sources() {
    let request = build_history_request(HistorySearchArgs {
        query: Some(" git ".to_owned()),
        target: Some("ssh".to_owned()),
        source: Some("snippet".to_owned()),
        pane_id: None,
        remote_host_id: None,
        session_id: None,
        limit: Some(900),
    })
    .expect("frozen filter values should parse");
    assert_eq!(request.query.as_deref(), Some(" git "));
    assert_eq!(request.target, Some(CommandHistoryTarget::Ssh));
    assert_eq!(request.source, Some(CommandHistorySource::Snippet));
    assert_eq!(request.limit, Some(900));

    let unsupported = build_history_request(HistorySearchArgs {
        query: None,
        target: None,
        source: Some("workflow".to_owned()),
        pane_id: None,
        remote_host_id: None,
        session_id: None,
        limit: None,
    });
    assert!(unsupported.is_err());
}

/// 空结果提示和最近五条样本保持现有 MCP 文本契约且不输出整段历史集合。
#[test]
fn summarizes_empty_history_and_only_five_recent_entries() {
    assert_eq!(summarize_history(&[]), "没有匹配的命令历史。");
    let entries = (0..6)
        .map(|index| history_entry(format!("命令 {index}"), CommandHistoryTarget::Local))
        .collect::<Vec<_>>();
    let summary = summarize_history(&entries);
    assert!(summary.starts_with("找到 6 条命令历史。最近记录："));
    assert!(summary.contains("本地: 命令 4"));
    assert!(!summary.contains("命令 5"));
}

/// 摘要的字符截断不会切断多字节 UTF-8 字符。
#[test]
fn truncates_unicode_summary_on_character_boundaries() {
    let entry = history_entry("界".repeat(200), CommandHistoryTarget::Ssh);
    let summary = summarize_history(&[entry]);
    assert!(summary.contains(&format!("SSH: {}...", "界".repeat(160))));
}
