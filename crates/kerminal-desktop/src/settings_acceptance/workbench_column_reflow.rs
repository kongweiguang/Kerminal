//! 隔离原生窗口中的真实 ConPTY 列重排验收。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::identity::SessionId;
use kerminal_terminal::snapshot::{RenderSnapshot, RenderSnapshotRequest};
use serde_json::{Value, json};

const COLUMN_REFLOW_ENV: &str = "KERMINAL_COLUMN_REFLOW_ACCEPTANCE";
const COLUMN_REFLOW_POWERSHELL: &str = "KERMINAL_COLUMN_REFLOW_POWERSHELL";
const REPORT_FILE: &str = "column-reflow-report.json";
const PROMPT: &str = "root@pkuai01:~# ";
const INPUT_LINE: &str = "root@pkuai01:~# sss";
const ERROR_PREFIX: &str = "Command 'sss' not found, but there are 17 similar ones.";

/// 专项仅由启动器显式打开，避免普通工作台验收向 shell 注入测试输出。
///
/// @author kongweiguang
pub(super) fn requested() -> bool {
    std::env::var_os(COLUMN_REFLOW_ENV).as_deref() == Some(OsStr::new("1"))
}

/// 真实 cmd/ConPTY 输出与 resize 一起验证，报告区分未形成折行的无效样本和真正通过的样本。
/// 只操作当前隔离会话与输出目录；SSH 截图的原始输出流不属于此次验证证据。
///
/// @author kongweiguang
pub(super) async fn capture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &WorkbenchContext,
    first_session: WorkbenchState,
) -> Result<(), Box<dyn Error>> {
    if !cfg!(windows) || context.width != 800 || context.height != 900 {
        return Err(
            io::Error::other("column reflow requires a Windows 800x900 initial window").into(),
        );
    }
    if std::env::var_os(WORKBENCH_POWERSHELL_ENV).is_some() {
        return Err(io::Error::other("column reflow requires the isolated cmd fixture").into());
    }
    let powershell = std::env::var_os(COLUMN_REFLOW_POWERSHELL)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("column reflow requires an explicit PowerShell 7 path"))?;
    if !powershell.is_absolute() || !powershell.is_file() {
        return Err(io::Error::other(
            "column reflow PowerShell path must be an existing absolute file",
        )
        .into());
    }
    let session = first_session
        .ready_session_ids
        .first()
        .ok_or_else(|| io::Error::other("column reflow has no ready local session"))?;
    let session_id = SessionId::parse(session)?;
    let output_directory = if context.output_directory.is_absolute() {
        context.output_directory.clone()
    } else {
        std::env::current_dir()?.join(&context.output_directory)
    };
    let mut checks = Vec::new();
    let mut all_passed = true;
    let mut last_wide_columns = None;

    for (theme, name, selector) in THEMES {
        let effective_theme =
            split_directory::change_theme(cx, root, window, services, theme, name, selector)
                .await?;
        split_directory::prepare_capture_surface(cx, window, 800, 900).await?;
        let initial = wait_snapshot(
            cx,
            window,
            services,
            &session_id,
            "cmd prompt",
            |snapshot| {
                snapshot.rows.iter().any(|row| row_text(row).ends_with('>'))
                    && last_wide_columns.is_none_or(|columns| snapshot.size.columns() < columns)
            },
        )
        .await?;
        let target_length = usize::from(initial.size.columns()) + 8;
        let tail_length = target_length.saturating_sub(ERROR_PREFIX.len() + 1);
        let error_line = format!("{ERROR_PREFIX} {}", "x".repeat(tail_length));
        let script = format!(
            "$e = [string][char]27\r\n[Console]::Write(\"`r`n\" * 100)\r\n[Console]::Write($e + '[H' + $e + '[2J' + $e + '[H')\r\n[Console]::Write('{INPUT_LINE}' + \"`r`n\" + '{}' + \"`r`n\" + '{PROMPT}' + \"`r`n\" + '{PROMPT}' + \"`r`n\" + '{PROMPT}')\r\n",
            error_line.replace('\'', "''"),
        );
        let script_path = output_directory.join(format!("column-reflow-{name}.ps1"));
        fs::write(&script_path, script)?;
        let command = format!(
            "\"{}\" -NoLogo -NoProfile -NonInteractive -File \"{}\"\r",
            cmd_path(&powershell)?,
            cmd_path(&script_path)?,
        );
        let sessions = services.sessions.clone();
        let target = session_id.clone();
        services
            .executor
            .clone()
            .run(move |_| async move { sessions.write(target, command.into_bytes()).await })
            .await?;
        let before = wait_snapshot(
            cx,
            window,
            services,
            &session_id,
            "synthetic output",
            |snapshot| {
                snapshot
                    .rows
                    .iter()
                    .map(row_text)
                    .collect::<String>()
                    .contains(ERROR_PREFIX)
                    && snapshot.rows.iter().any(|row| row_text(row).ends_with('>'))
            },
        )
        .await?;
        split_directory::prepare_capture_surface(cx, window, 1440, 900).await?;
        let after = wait_snapshot(
            cx,
            window,
            services,
            &session_id,
            "wider viewport",
            |snapshot| snapshot.size.columns() > before.size.columns(),
        )
        .await?;
        last_wide_columns = Some(after.size.columns());
        let frame = save_frame(
            cx,
            window,
            output_directory.join(format!("column-reflow-{name}.png")),
        )?;
        verify_capture_dimensions(cx, window, 1440, 900, std::slice::from_ref(&frame))?;

        let before_rows: Vec<_> = before.rows.iter().map(row_text).collect();
        let after_rows: Vec<_> = after.rows.iter().map(row_text).collect();
        let setup_valid = before.history_size > 0
            && before.scrollback_offset == 0
            && before_rows.first().is_some_and(|row| row == INPUT_LINE)
            && before.rows.iter().any(|row| row.wrapped)
            && before_rows.concat().contains(&error_line)
            && error_line.len() > usize::from(before.size.columns())
            && error_line.len() <= usize::from(after.size.columns());
        let passed = setup_valid
            && before.size.rows() == after.size.rows()
            && after.scrollback_offset == 0
            && after_rows.first().is_some_and(|row| row == INPUT_LINE)
            && after_rows.get(1).is_some_and(|row| row == &error_line);
        all_passed &= passed;
        checks.push(json!({
            "theme": name,
            "effectiveTheme": effective_theme,
            "setupValid": setup_valid,
            "passed": passed,
            "input": { "firstLine": INPUT_LINE, "errorLine": error_line, "prompt": PROMPT },
            "before": snapshot_evidence(&before),
            "after": snapshot_evidence(&after),
            "capture": frame,
        }));
    }

    let (visible, active) = window.update(cx, |_, window, _| {
        ensure_hidden_window(window)?;
        Ok::<_, io::Error>((window.is_visible(), window.is_window_active()))
    })??;
    let isolation = crate::right_rail_acceptance::isolation::capture()?;
    isolation.validate_window_state(visible, active)?;
    let report = json!({
        "schemaVersion": 1,
        "captureMode": "hidden-native-window",
        "scope": "Windows local cmd and ConPTY with synthetic output; not the original SSH stream",
        "nativeIsolation": isolation,
        "sessionId": session,
        "checks": checks,
        "passed": all_passed,
    });
    fs::write(
        output_directory.join(REPORT_FILE),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if !all_passed {
        return Err(io::Error::other("column reflow was not verified; inspect setupValid and snapshots in column-reflow-report.json").into());
    }
    Ok(())
}

/// 命令行路径只接受 cmd 可安全引用的绝对文本，避免测试路径被解释为 shell 语法。
///
/// @author kongweiguang
fn cmd_path(path: &Path) -> Result<&str, io::Error> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::other("column reflow path is not UTF-8"))?;
    if !path.is_absolute() || text.chars().any(|c| matches!(c, '"' | '%' | '\r' | '\n')) {
        return Err(io::Error::other(
            "column reflow path cannot be quoted safely for cmd",
        ));
    }
    Ok(text)
}

/// 仅裁剪网格行尾填充，保留首部与整行空白，防止证据转换掩盖顶部空行。
///
/// @author kongweiguang
fn row_text(row: &kerminal_terminal::snapshot::TerminalRow) -> String {
    row.runs
        .iter()
        .map(|run| run.text.as_ref())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// 显式保存尺寸、历史、光标与每一行，供失败样本和渲染重排的差异复核。
///
/// @author kongweiguang
fn snapshot_evidence(snapshot: &RenderSnapshot) -> Value {
    let rows: Vec<_> = snapshot.rows.iter().map(row_text).collect();
    json!({
        "columns": snapshot.size.columns(),
        "rows": snapshot.size.rows(),
        "historySize": snapshot.history_size,
        "scrollbackOffset": snapshot.scrollback_offset,
        "cursor": { "row": snapshot.cursor.row, "column": snapshot.cursor.column },
        "firstNonemptyRow": rows.iter().position(|row| !row.is_empty()),
        "wrappedRows": snapshot.rows.iter().filter(|row| row.wrapped).map(|row| row.viewport_row).collect::<Vec<_>>(),
        "visibleRows": rows,
    })
}

/// 等待共享会话服务的稳定快照，同时驱动隐藏窗口布局，避免将 resize 请求当作已生效。
///
/// @author kongweiguang
async fn wait_snapshot<F>(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    session_id: &SessionId,
    stage: &str,
    condition: F,
) -> Result<RenderSnapshot, Box<dyn Error>>
where
    F: Fn(&RenderSnapshot) -> bool,
{
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    let mut previous_revision = None;
    loop {
        window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(())
        })??;
        let sessions = services.sessions.clone();
        let target = session_id.clone();
        let snapshot = services
            .executor
            .clone()
            .run(move |_| async move {
                sessions
                    .render_snapshot(target, RenderSnapshotRequest::default())
                    .await
            })
            .await?;
        if condition(&snapshot) {
            if previous_revision == Some(snapshot.revision) {
                return Ok(snapshot);
            }
            previous_revision = Some(snapshot.revision);
        } else {
            previous_revision = None;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "column reflow timed out during {stage}: {}",
                snapshot_evidence(&snapshot)
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}
