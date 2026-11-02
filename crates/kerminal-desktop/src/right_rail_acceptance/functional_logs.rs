//! 验证 Logs 生产历史列表、行内操作及原终端未提交输入。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    ffi::OsStr,
    io,
};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity, InputEvent as _};
use gpui_pre::{KeyDownEvent, KeyUpEvent, Keystroke};
use kerminal_application::preferences::ThemeMode;
use kerminal_domain::{commands::CommandHistorySource, identity::HostId};
use kerminal_ui::{RootView, features::commands::RightRailAcceptanceLogsSnapshot};

use super::{
    RightRailCaptureContext, ToolRailToolId, activate_dialog_tool,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

#[path = "functional_logs/native_actions.rs"]
mod native_actions;
use native_actions::{click_selector, verify_copy_with_clipboard_restore};

const SEEDED_ENTRIES: usize = 24;
const USER_ENTRIES: usize = 23;

/// 只在隔离 fixture pane 内串行执行 Logs 写入；每次变化按实际窗口容量和限定快照回读确认。
/// 复制及填入提示会改变可用高度，遍历前先结束旧反馈并重新建立分页基准。
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
        SEEDED_ENTRIES.div_ceil(first_page.page_size()).max(1),
        SEEDED_ENTRIES,
        SEEDED_ENTRIES.min(first_page.page_size()),
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-first-page",
        "seed_entries_fill_first_page_to_measured_panel_capacity",
    )
    .await?;

    for (mode, operation) in [
        (ThemeMode::Light, "logs-layout-light"),
        (ThemeMode::Dark, "logs-layout-dark"),
        (ThemeMode::System, "logs-layout-system"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            })
        })?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "complete_rows_and_aligned_pagination_in_native_window",
        )
        .await?;
        verify_history_geometry(cx, window, &first_page)?;
    }
    verify_copy_and_fill(cx, root, window, capture, records, run_id, &first_page).await?;

    // 主动翻页结束填入反馈；复制提示仍可保留，不能复用操作前的容量和页数。
    if read_snapshot(cx, root, window, host_id, run_id)
        .await?
        .page_count()
        > 1
    {
        for page in [2, 1] {
            poll_root(
                cx,
                root,
                window,
                "重建 Logs 遍历基准",
                |root, _, cx| {
                    root.set_right_rail_acceptance_logs_page(host_id, run_id, page, cx)
                        .map(|()| Some(()))
                },
            )
            .await?;
            let settled = read_snapshot(cx, root, window, host_id, run_id).await?;
            if settled.fill_confirmed() {
                return Err(io::Error::other("主动翻页后仍保留旧填入成功提示").into());
            }
        }
    }
    let first_page = read_snapshot(cx, root, window, host_id, run_id).await?;

    let mut remaining_pages = Vec::new();
    for page in 2..=first_page.page_count() {
        poll_root(cx, root, window, "选择 Logs 后续页", |root, _, cx| {
            root.set_right_rail_acceptance_logs_page(host_id, run_id, page, cx)
                .map(|()| Some(()))
        })
        .await?;
        let page_snapshot = read_snapshot(cx, root, window, host_id, run_id).await?;
        if page_snapshot.fill_confirmed() {
            return Err(io::Error::other("主动翻页后仍保留旧填入成功提示").into());
        }
        verify_snapshot(
            &page_snapshot,
            run_id,
            &prefix,
            &prefix,
            None,
            page,
            SEEDED_ENTRIES.div_ceil(page_snapshot.page_size()).max(1),
            SEEDED_ENTRIES,
            (SEEDED_ENTRIES - (page - 1) * page_snapshot.page_size())
                .min(page_snapshot.page_size()),
        )?;
        remaining_pages.push(page_snapshot);
    }
    if !remaining_pages.is_empty() {
        checkpoint(
            cx,
            window,
            capture,
            records,
            "logs-last-page",
            "all_pages_partition_seed_entries_with_measured_capacity",
        )
        .await?;
    }
    verify_seed_sources(&first_page, &remaining_pages, &prefix, run_id)?;

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
        USER_ENTRIES.div_ceil(filtered.page_size()).max(1),
        USER_ENTRIES,
        USER_ENTRIES.min(filtered.page_size()),
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-user-filter",
        "production_source_filter_returns_only_seed_user_entries",
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
        USER_ENTRIES.div_ceil(searched.page_size()).max(1),
        USER_ENTRIES,
        USER_ENTRIES.min(searched.page_size()),
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
            expected_before.div_ceil(before.page_size()).max(1),
            expected_before,
            expected_before.min(before.page_size()),
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
            expected_after.div_ceil(after.page_size()).max(1),
            expected_after,
            expected_after.min(after.page_size()),
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
        return Err(io::Error::other("Logs 未逐条删除全部 User 种子记录").into());
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

/// 通过真实按钮点击验证复制及未提交填入，始终限定本轮 fixture 的记录、终端和光标。
/// 成功提示会挤占列表空间，须在布局稳定后核对反馈与完整行，防止被动重排误作废操作。
///
/// @author kongweiguang
async fn verify_copy_and_fill(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    run_id: &str,
    snapshot: &RightRailAcceptanceLogsSnapshot,
) -> Result<(), Box<dyn Error>> {
    let entry = snapshot
        .entries()
        .first()
        .ok_or_else(|| io::Error::other("日志验收缺少可见的种子命令"))?;
    let entry_id = entry.id().to_owned();
    let command = entry.command().to_owned();
    if !command.starts_with(snapshot.prefix()) || !command.is_ascii() {
        return Err(io::Error::other("日志行内验收只接受本轮 ASCII 隔离种子").into());
    }

    let copied = verify_copy_with_clipboard_restore(cx, window, &entry_id, &command)?;
    if copied {
        checkpoint(
            cx,
            window,
            capture,
            records,
            "logs-copy",
            "rendered_copy_button_wrote_seed_and_restored_original_clipboard",
        )
        .await?;
    } else {
        checkpoint(
            cx,
            window,
            capture,
            records,
            "logs-copy-not-tested",
            "clipboard_formats_cannot_be_restored_without_loss_copy_not_tested",
        )
        .await?;
    }

    let host_id = &capture.fixture_host_id;
    let original_line = poll_root(
        cx,
        root,
        window,
        "读取 Logs 原终端光标行",
        |root, _, cx| root.right_rail_acceptance_logs_terminal_cursor_line(host_id, run_id, cx),
    )
    .await?;
    let tail = &command[command.len().saturating_sub(24)..];
    click_selector(cx, window, &format!("rr-logs-fill-{entry_id}"))?;
    poll_root(
        cx,
        root,
        window,
        "等待历史填入原终端未提交光标行",
        |root, _, cx| {
            root.right_rail_acceptance_logs_terminal_cursor_line(host_id, run_id, cx)
                .map(|line| line.filter(|line| line.ends_with(tail) && line != &original_line))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "验证历史填入返回原终端焦点",
        |root, window, cx| {
            root.right_rail_acceptance_logs_terminal_focused(host_id, run_id, window, cx)
                .map(|focused| focused.then_some(()))
        },
    )
    .await?;
    let evidence = checkpoint(
        cx,
        window,
        capture,
        records,
        "logs-fill-pending",
        "rendered_fill_button_left_seed_at_original_terminal_cursor_without_enter",
    )
    .await;

    let filled = read_snapshot(cx, root, window, host_id, run_id).await?;
    if !filled.fill_confirmed() {
        return Err(io::Error::other("历史填入成功提示在被动布局后丢失").into());
    }
    verify_history_geometry(cx, window, &filled)?;

    // 后续验收复用同一隔离 pane；只在原 binding 仍持有焦点时清除未提交输入。
    window.update(cx, |_, window, app| {
        let focused = root
            .update(app, |root, cx| {
                root.right_rail_acceptance_logs_terminal_focused(host_id, run_id, window, cx)
            })
            .map_err(io::Error::other)?;
        if !focused {
            return Err(io::Error::other("原 fixture 终端焦点已变化，拒绝清除输入"));
        }
        let keystroke = Keystroke::parse("ctrl-u")
            .map_err(|error| io::Error::other(format!("无法构造 Ctrl+U: {error}")))?;
        let _ = window.dispatch_event(
            KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), app);
        Ok::<_, io::Error>(())
    })??;
    poll_root(
        cx,
        root,
        window,
        "确认原 fixture 输入已清除",
        |root, _, cx| {
            root.right_rail_acceptance_logs_terminal_cursor_line(host_id, run_id, cx)
                .map(|line| line.filter(|line| line == &original_line))
        },
    )
    .await?;
    evidence?;
    Ok(())
}

/// 查询完成后再等两个连续帧的分页投影一致，避免首次布局调整容量后仍拿旧行数检查新画面。
///
/// @author kongweiguang
async fn read_snapshot(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    run_id: &str,
) -> Result<RightRailAcceptanceLogsSnapshot, Box<dyn Error>> {
    let mut previous: Option<RightRailAcceptanceLogsSnapshot> = None;
    loop {
        let snapshot = poll_root(
            cx,
            root,
            window,
            "等待 Logs 生产查询与布局回读",
            |root, _, cx| {
                if !root.right_rail_acceptance_logs_ready(host_id, run_id, cx)? {
                    return Ok(None);
                }
                root.right_rail_acceptance_logs_snapshot(host_id, run_id, cx)
                    .map(Some)
            },
        )
        .await?;
        if previous.as_ref().is_some_and(|previous| {
            previous.page() == snapshot.page()
                && previous.page_size() == snapshot.page_size()
                && previous.total_entries() == snapshot.total_entries()
                && previous.query() == snapshot.query()
                && previous.source() == snapshot.source()
                && previous
                    .entries()
                    .iter()
                    .map(|entry| entry.id())
                    .eq(snapshot.entries().iter().map(|entry| entry.id()))
        }) {
            return Ok(snapshot);
        }
        previous = Some(snapshot);
    }
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

/// 用当前帧的真实 bounds 检查完整行、列表利用率和分页中线，防止快照条数正确但实际裁切或错位。
///
/// @author kongweiguang
fn verify_history_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    snapshot: &RightRailAcceptanceLogsSnapshot,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, _| {
        let bounds = |selector: &str| {
            window
                .debug_selector_bounds(selector)
                .ok_or_else(|| io::Error::other(format!("缺少历史布局 selector {selector}")))
        };
        let list = bounds("rr-logs-list")?;
        let card = bounds("rr-logs-card")?;
        let rounding = gpui_kit::px(1.);
        for entry in snapshot.entries() {
            let row = bounds(&format!("rr-logs-row-{}", entry.id()))?;
            if row.top() < list.top() - rounding || row.bottom() > list.bottom() + rounding {
                return Err(io::Error::other("历史分页含未完整显示的命令行"));
            }
        }
        if let Some(first) = snapshot.entries().first() {
            let row = bounds(&format!("rr-logs-row-{}", first.id()))?;
            if snapshot.page_count() > 1
                && list.size.height - row.size.height * snapshot.page_size() as f32
                    >= row.size.height + rounding
            {
                return Err(io::Error::other("历史列表仍空留一整行以上的可用高度"));
            }
        }
        if snapshot.page_count() > 1 {
            let footer = bounds("rr-logs-pagination")?;
            let previous = bounds("rr-logs-previous")?;
            let label = bounds("rr-logs-page-label")?;
            let next = bounds("rr-logs-next")?;
            if (previous.center().y - label.center().y).abs() > rounding
                || (next.center().y - label.center().y).abs() > rounding
                || (footer.bottom() - card.bottom()).abs() > gpui_kit::px(2.)
                || (footer.left() - list.left()).abs() > rounding
                || (footer.right() - list.right()).abs() > rounding
            {
                return Err(io::Error::other("历史分页未与列表边缘及按钮中线对齐"));
            }
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 合并所有实际页面验证种子集合与来源，确保动态容量不会漏页或重复展示记录。
///
/// @author kongweiguang
fn verify_seed_sources(
    first_page: &RightRailAcceptanceLogsSnapshot,
    remaining_pages: &[RightRailAcceptanceLogsSnapshot],
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
    for entry in first_page
        .entries()
        .iter()
        .chain(remaining_pages.iter().flat_map(|page| page.entries()))
    {
        actual.insert(entry.command().to_owned(), entry.source());
        ids.insert(entry.id().to_owned());
    }
    if actual != expected || ids.len() != SEEDED_ENTRIES {
        return Err(io::Error::other(format!(
            "Logs seed {run_id} 未回读到完整且唯一的 User 与 Tool 种子记录"
        )));
    }
    Ok(())
}
