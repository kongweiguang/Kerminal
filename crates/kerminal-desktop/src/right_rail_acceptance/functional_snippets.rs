//! 验证隔离 fixture 中片段导入持久化、生产确认、运行输出与未提交 Fill 回读。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_domain::identity::HostId;
use kerminal_ui::RootView;

use super::{
    RightRailCaptureContext, ToolRailToolId, activate_dialog_tool,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

const CANCELLED_SNIPPET: &str = "snippet.builtin.core.find_large_files";
const PING_SNIPPET: &str = "snippet.builtin.core.ping_host";
const PING_HOST: &str = "127.0.0.1";
const PING_OUTPUT_MARKER: &str = "packets transmitted";
const PING_COMMAND: &str = "ping -c 4 127.0.0.1";

/// 只对隔离 SSH fixture 操作 built-in；取消高风险确认后再运行只读 ping 并回读真实 PTY grid。
/// Fill 回读后调用生产关闭入口并等待 owner 释放；程序化选片段不承诺键盘焦点，键盘路径由专门的弹层捕获验收。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Snippets,
        host,
        "snippets functional",
    )
    .await?;

    import_snippet(cx, root, window, capture, records).await?;

    select_snippet(cx, root, window, host, CANCELLED_SNIPPET).await?;
    let terminal_revision = window
        .update(cx, |_, _, app| {
            root.update(app, |root, _| {
                root.right_rail_acceptance_terminal_revision(host)
            })
        })??
        .ok_or_else(|| io::Error::other("Snippets fixture terminal 尚无活动 render revision"))?;
    poll_root(
        cx,
        root,
        window,
        "填写取消样例路径",
        |root, window, cx| {
            root.set_right_rail_acceptance_snippet_variable(
                host,
                CANCELLED_SNIPPET,
                "path",
                "/tmp",
                window,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    poll_root(cx, root, window, "打开 Run 确认", |root, window, cx| {
        root.request_right_rail_acceptance_snippet_run(host, CANCELLED_SNIPPET, window, cx)
            .map(|started| started.then_some(()))
    })
    .await?;
    poll_root(
        cx,
        root,
        window,
        "等待 Run 确认显示",
        |root, _, cx| {
            root.right_rail_acceptance_snippet_confirmation_pending(host, CANCELLED_SNIPPET, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "snippets-run-confirmation-open",
        "high_io_builtin_opens_production_run_confirmation",
    )
    .await?;
    poll_root(cx, root, window, "取消 Run 确认", |root, window, cx| {
        root.cancel_right_rail_acceptance_snippet_confirmation(host, CANCELLED_SNIPPET, window, cx)
            .map(|closed| closed.then_some(()))
    })
    .await?;
    poll_root(cx, root, window, "Run 取消回读", |root, _, cx| {
        root.right_rail_acceptance_snippet_confirmation_cancelled(host, CANCELLED_SNIPPET, cx)
            .map(|cancelled| cancelled.then_some(()))
    })
    .await?;
    let revision_after_cancel = window
        .update(cx, |_, _, app| {
            root.update(app, |root, _| {
                root.right_rail_acceptance_terminal_revision(host)
            })
        })??
        .ok_or_else(|| io::Error::other("Run 取消后 fixture terminal render revision 不可用"))?;
    if revision_after_cancel != terminal_revision {
        return Err(io::Error::other(
            "取消 Snippet Run 确认后终端 revision 改变，无法证明命令未提交",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "snippets-run-confirmation-cancelled",
        "production_dialog_cancel_clears_pending_without_terminal_revision_change",
    )
    .await?;

    select_snippet(cx, root, window, host, PING_SNIPPET).await?;
    poll_root(
        cx,
        root,
        window,
        "填写 ping 变量",
        |root, window, cx| {
            root.set_right_rail_acceptance_snippet_variable(
                host,
                PING_SNIPPET,
                "host",
                PING_HOST,
                window,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "提交 ping Run 请求",
        |root, window, cx| {
            root.request_right_rail_acceptance_snippet_run(host, PING_SNIPPET, window, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    let output_result = poll_root(
        cx,
        root,
        window,
        "等待 ping 完整输出",
        |root, _, _| {
            root.right_rail_acceptance_terminal_contains(host, PING_OUTPUT_MARKER)
                .map(|contains| contains.then_some(()))
        },
    )
    .await;
    if let Err(error) = output_result {
        let text = window.update(cx, |_, _, app| {
            root.update(app, |root, _| {
                root.right_rail_acceptance_terminal_text(host)
            })
        })??;
        std::fs::write(
            capture
                .output_directory
                .join("snippet-run-failure-grid.txt"),
            text.unwrap_or_default(),
        )?;
        let _ = checkpoint(
            cx,
            window,
            capture,
            records,
            "snippets-run-failed",
            "failure_diagnostic_only",
        )
        .await;
        return Err(error);
    }
    poll_root(
        cx,
        root,
        window,
        "等待 ping writer 空闲",
        |root, _, cx| {
            root.right_rail_acceptance_snippet_action_idle(host, PING_SNIPPET, cx)
                .map(|idle| idle.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "snippets-run-output",
        "read_only_ping_run_produced_terminal_summary_on_fixture_ssh_pane",
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "Fill ping 到终端",
        |root, window, cx| {
            root.fill_right_rail_acceptance_snippet(host, PING_SNIPPET, window, cx)
                .map(|started| started.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "回读未提交 Fill 命令行",
        |root, _, _| {
            root.right_rail_acceptance_terminal_cursor_line_ends_with(host, PING_COMMAND)
                .map(|filled| filled.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "snippets-fill-pending",
        "fill_is_visible_at_live_cursor_without_submitting_enter",
    )
    .await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.close_right_rail_acceptance_snippet_editor(cx);
        });
    })?;
    super::wait_for_acceptance_dialog_closed(
        cx,
        root,
        window,
        "功能验收片段编辑器",
        |root, cx| root.right_rail_acceptance_snippet_editor_closed(cx),
    )
    .await?;
    Ok(())
}

/// 预览使用真实文件读取路径，确认只提交一次，再分别等待 UI 刷新与版本化存储回读。
///
/// @author kongweiguang
async fn import_snippet(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let sample = super::snippet_import::write_functional_sample(&capture.output_directory)?;
    let accepted = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.preview_right_rail_acceptance_snippet_import(host, sample.path.clone(), cx)
        })
    })??;
    if !accepted {
        return Err(io::Error::other("生产片段导入路径未受理隔离验收样本").into());
    }
    poll_root(cx, root, window, "片段导入预览", |root, _, cx| {
        root.right_rail_acceptance_snippet_import_ready(host, sample.file_name, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;

    // This is the only submit call; later waits only observe completion and never retry it.
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.confirm_right_rail_acceptance_snippet_import(
                host,
                sample.file_name,
                &sample.title,
                &sample.command,
                cx,
            )
        })
    })??;
    poll_root(
        cx,
        root,
        window,
        "等待导入弹框关闭和 Mine 目录刷新",
        |root, _, cx| {
            root.right_rail_acceptance_snippet_import_committed(host, sample.file_name, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;

    let readback = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.right_rail_acceptance_snippet_import_readback(
                host,
                sample.title,
                sample.command,
                cx,
            )
        })
    })??;
    if !readback.await.map_err(io::Error::other)? {
        return Err(io::Error::other("片段导入后的版本化存储回读与样本不一致").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "snippets-import-persisted",
        "production_import_closed_and_mine_refreshed_then_exported_storage_matched",
    )
    .await?;
    Ok(())
}

/// Library 查询、精确目录回读与详情读取分成独立等待点；已取消样例切到 ping 时按生产决策显式放弃变量。
///
/// @author kongweiguang
async fn select_snippet(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    snippet_id: &str,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "查询 built-in 片段",
        |root, window, cx| {
            root.prepare_right_rail_acceptance_snippet(host, snippet_id, window, cx)
                .map(|prepared| prepared.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "等待精确片段目录项",
        |root, _, cx| {
            root.right_rail_acceptance_snippet_catalog_ready(host, snippet_id, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "打开生产片段详情",
        |root, window, cx| {
            root.open_right_rail_acceptance_snippet(host, snippet_id, window, cx)
                .map(|opened| opened.then_some(()))
        },
    )
    .await?;
    if snippet_id == PING_SNIPPET {
        poll_root(
            cx,
            root,
            window,
            "放弃已取消样例变量并继续选择 ping",
            |root, window, cx| {
                root.discard_right_rail_acceptance_cancelled_snippet_transition_to_ping(
                    host, window, cx,
                )
                .map(|discarded| discarded.then_some(()))
            },
        )
        .await?;
    }
    poll_root(
        cx,
        root,
        window,
        "等待片段详情回读",
        |root, _, cx| {
            root.right_rail_acceptance_snippet_ready(host, snippet_id, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    Ok(())
}
