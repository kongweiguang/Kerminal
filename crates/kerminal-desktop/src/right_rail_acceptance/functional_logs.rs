//! 验证 Logs 生产历史列表的种子、分页、筛选、搜索、删除与清空闭环。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    ffi::OsStr,
    io,
};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_domain::{commands::CommandHistorySource, identity::HostId};
use kerminal_ui::{RootView, features::commands::RightRailAcceptanceLogsSnapshot};

use super::{
    RightRailCaptureContext, ToolRailToolId, activate_dialog_tool,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

const SEEDED_ENTRIES: usize = 10;
const USER_ENTRIES: usize = 9;
const FIRST_PAGE_ENTRIES: usize = 8;

/// 只在隔离 fixture pane 内串行执行 Logs 写入；每次变化都以生产 ready 与限定快照回读确认。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    let run_id = capture
        .output_directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("验收输出目录缺少唯一名称"))?;
    let prefix = format!("rr-ui-{run_id}-");
    let user_query = format!("{prefix}user-");

    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Logs,
        host_id,
        "logs functional",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "写入 Logs 验收种子",
        |root, window, cx| {
            root.begin_right_rail_acceptance_logs(host_id, run_id, window, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;

    let first_page = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(
        &first_page,
        run_id,
        &prefix,
        &prefix,
        None,
        1,
        2,
        SEEDED_ENTRIES,
        FIRST_PAGE_ENTRIES,
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-first-page",
        "ten_seed_entries_show_eight_rows_on_first_page",
    )
    .await?;

    poll_root(cx, root, window, "选择 Logs 第二页", |root, _, cx| {
        root.set_right_rail_acceptance_logs_page(host_id, run_id, 2, cx)
            .map(|()| Some(()))
    })
    .await?;
    let second_page = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(
        &second_page,
        run_id,
        &prefix,
        &prefix,
        None,
        2,
        2,
        SEEDED_ENTRIES,
        SEEDED_ENTRIES - FIRST_PAGE_ENTRIES,
    )?;
    verify_seed_sources(&first_page, &second_page, &prefix, run_id)?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-second-page",
        "same_seed_has_two_rows_on_second_page_and_nine_user_one_tool_sources",
    )
    .await?;

    poll_root(cx, root, window, "筛选 User 来源", |root, _, cx| {
        root.set_right_rail_acceptance_logs_source(
            host_id,
            run_id,
            Some(CommandHistorySource::User),
            cx,
        )
        .map(|()| Some(()))
    })
    .await?;
    let filtered = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(
        &filtered,
        run_id,
        &prefix,
        &prefix,
        Some(CommandHistorySource::User),
        1,
        2,
        USER_ENTRIES,
        FIRST_PAGE_ENTRIES,
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-user-filter",
        "production_source_filter_returns_nine_user_entries",
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "搜索本轮 User 记录",
        |root, window, cx| {
            root.set_right_rail_acceptance_logs_query(
                host_id,
                run_id,
                user_query.clone(),
                window,
                cx,
            )
            .map(|()| Some(()))
        },
    )
    .await?;
    let searched = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(
        &searched,
        run_id,
        &prefix,
        &user_query,
        Some(CommandHistorySource::User),
        1,
        2,
        USER_ENTRIES,
        FIRST_PAGE_ENTRIES,
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-user-query",
        "query_and_source_filter_return_only_this_run_user_entries",
    )
    .await?;

    let mut deleted_ids = HashSet::with_capacity(USER_ENTRIES);
    for deleted_count in 0..USER_ENTRIES {
        let before = read_snapshot(cx, root, window, host_id, run_id).await?;
        let expected_before = USER_ENTRIES - deleted_count;
        verify_snapshot(
            &before,
            run_id,
            &prefix,
            &user_query,
            Some(CommandHistorySource::User),
            1,
            expected_before.div_ceil(FIRST_PAGE_ENTRIES).max(1),
            expected_before,
            expected_before.min(FIRST_PAGE_ENTRIES),
        )?;
        let entry = before
            .entries()
            .first()
            .ok_or_else(|| io::Error::other("Logs 没有可删除的本轮 User 记录"))?;
        if entry.source() != CommandHistorySource::User
            || !entry.command().starts_with(&user_query)
            || !deleted_ids.insert(entry.id().to_owned())
        {
            return Err(io::Error::other("Logs 删除目标不是尚未删除的本轮 User 记录").into());
        }
        let deleted_id = entry.id().to_owned();
        poll_root(
            cx,
            root,
            window,
            "删除当前可见 User 记录",
            |root, _, cx| {
                root.delete_right_rail_acceptance_logs_entry(host_id, run_id, &deleted_id, cx)
                    .map(|()| Some(()))
            },
        )
        .await?;

        let after = read_snapshot(cx, root, window, host_id, run_id).await?;
        let expected_after = expected_before - 1;
        verify_snapshot(
            &after,
            run_id,
            &prefix,
            &user_query,
            Some(CommandHistorySource::User),
            1,
            expected_after.div_ceil(FIRST_PAGE_ENTRIES).max(1),
            expected_after,
            expected_after.min(FIRST_PAGE_ENTRIES),
        )?;
        if after
            .entries()
            .iter()
            .any(|candidate| candidate.id() == deleted_id)
        {
            return Err(io::Error::other("Logs 删除回读仍包含刚删除的 session entry ID").into());
        }
    }
    if deleted_ids.len() != USER_ENTRIES {
        return Err(io::Error::other("Logs 未逐条删除全部九条 User 记录").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-user-entries-deleted",
        "each_user_entry_id_absent_after_its_production_delete_readback",
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "恢复验收精确前缀搜索",
        |root, window, cx| {
            root.set_right_rail_acceptance_logs_query(host_id, run_id, prefix.clone(), window, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    let user_empty = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(
        &user_empty,
        run_id,
        &prefix,
        &prefix,
        Some(CommandHistorySource::User),
        1,
        1,
        0,
        0,
    )?;

    poll_root(
        cx,
        root,
        window,
        "移除 Logs 来源筛选",
        |root, _, cx| {
            root.set_right_rail_acceptance_logs_source(host_id, run_id, None, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    let clear_scope = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(&clear_scope, run_id, &prefix, &prefix, None, 1, 1, 1, 1)?;
    if clear_scope.entries()[0].source() != CommandHistorySource::Tool {
        return Err(io::Error::other("清空前的精确前缀快照未保留唯一 Tool 记录").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-clear-scope",
        "clear_is_enabled_only_for_exact_prefix_without_source_filter",
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "清空 fixture pane 历史",
        |root, _, cx| {
            root.clear_right_rail_acceptance_logs(host_id, run_id, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    let cleared = read_snapshot(cx, root, window, host_id, run_id).await?;
    verify_snapshot(&cleared, run_id, &prefix, &prefix, None, 1, 1, 0, 0)?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-cleared",
        "production_clear_readback_returns_no_entries_for_fixture_prefix",
    )
    .await?;
    Ok(())
}

/// 先等待 Root 的 seed/query readiness，再取只包含本轮前缀的 production snapshot。
///
/// @author kongweiguang
async fn read_snapshot(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    run_id: &str,
) -> Result<RightRailAcceptanceLogsSnapshot, Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "等待 Logs 生产查询回读",
        |root, _, cx| {
            if !root.right_rail_acceptance_logs_ready(host_id, run_id, cx)? {
                return Ok(None);
            }
            root.right_rail_acceptance_logs_snapshot(host_id, run_id, cx)
                .map(Some)
        },
    )
    .await
}

/// 核对 seed、查询键、来源、分页边界和当前页，避免仅凭操作请求认定功能成功。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
fn verify_snapshot(
    snapshot: &RightRailAcceptanceLogsSnapshot,
    run_id: &str,
    prefix: &str,
    query: &str,
    source: Option<CommandHistorySource>,
    page: usize,
    page_count: usize,
    total_entries: usize,
    page_entries: usize,
) -> Result<(), io::Error> {
    if snapshot.run_id() != run_id
        || snapshot.prefix() != prefix
        || snapshot.query() != query
        || snapshot.source() != source
        || snapshot.page() != page
        || snapshot.page_count() != page_count
        || snapshot.total_entries() != total_entries
        || snapshot.seeded_entries() != SEEDED_ENTRIES
        || snapshot.entries().len() != page_entries
        || snapshot
            .entries()
            .iter()
            .any(|entry| !entry.command().starts_with(prefix))
        || source.is_some_and(|expected| {
            snapshot
                .entries()
                .iter()
                .any(|entry| entry.source() != expected)
        })
    {
        return Err(io::Error::other(format!(
            "Logs snapshot 不符合预期：run={run_id}, query={query}, source={source:?}, page={page}/{page_count}, total={total_entries}, rows={page_entries}"
        )));
    }
    Ok(())
}

/// 合并两页真实记录验证 seed 命令集合及 9 User/1 Tool 来源，不假设列表排序顺序。
///
/// @author kongweiguang
fn verify_seed_sources(
    first_page: &RightRailAcceptanceLogsSnapshot,
    second_page: &RightRailAcceptanceLogsSnapshot,
    prefix: &str,
    run_id: &str,
) -> Result<(), io::Error> {
    let mut expected = HashMap::with_capacity(SEEDED_ENTRIES);
    for index in 1..=USER_ENTRIES {
        expected.insert(
            format!("{prefix}user-{index:02}"),
            CommandHistorySource::User,
        );
    }
    expected.insert(format!("{prefix}tool-01"), CommandHistorySource::Tool);

    let mut actual = HashMap::with_capacity(SEEDED_ENTRIES);
    let mut ids = HashSet::with_capacity(SEEDED_ENTRIES);
    for entry in first_page.entries().iter().chain(second_page.entries()) {
        actual.insert(entry.command().to_owned(), entry.source());
        ids.insert(entry.id().to_owned());
    }
    if actual != expected || ids.len() != SEEDED_ENTRIES {
        return Err(io::Error::other(format!(
            "Logs seed {run_id} 未回读到唯一的九条 User 与一条 Tool 记录"
        )));
    }
    Ok(())
}
