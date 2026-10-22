//! Replay the bounded R38 ConPTY trace against the current terminal engine.
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::snapshot::{
    RenderSnapshot, RenderSnapshotRequest, TerminalPoint, TerminalSelection, TerminalSelectionKind,
};

const TRACE_BYTES: &[u8] = include_bytes!("fixtures/conpty_long_prompt_resize.r38bin");
const INITIAL_COLUMNS: u16 = 112;
const NARROW_COLUMNS: u16 = 75;
const ROWS: u16 = 33;
const MAX_TRACE_BYTES: usize = 256 * 1024;
const MAX_RECORDS: usize = 4096;

/// Keep the fixture parser's output limited to validated dimensions and stage-tagged byte chunks.
///
/// @author kongweiguang
struct ReplayTrace {
    initial_columns: u16,
    narrow_columns: u16,
    rows: u16,
    records: Vec<(u8, Vec<u8>)>,
}

/// Identify the captured phase without interpreting or logging terminal payload text.
///
/// @author kongweiguang
fn take_field<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    length: usize,
) -> Result<&'a [u8], &'static str> {
    let end = (*offset)
        .checked_add(length)
        .ok_or("fixture field offset overflowed")?;
    let field = bytes
        .get(*offset..end)
        .ok_or("fixture ended inside a field")?;
    *offset = end;
    Ok(field)
}

/// Parse only the bounded R38 envelope, rejecting malformed lengths, stage reordering, and trailing bytes.
///
/// @author kongweiguang
fn parse_trace(bytes: &[u8]) -> Result<ReplayTrace, &'static str> {
    if bytes.len() > MAX_TRACE_BYTES {
        return Err("fixture exceeded the replay byte budget");
    }

    let mut offset = 0;
    if take_field(bytes, &mut offset, 8)? != b"R38VTAB1" {
        return Err("fixture magic did not match R38 format");
    }
    let initial_columns = u16::from_le_bytes(
        take_field(bytes, &mut offset, 2)?
            .try_into()
            .map_err(|_| "fixture initial width was invalid")?,
    );
    let narrow_columns = u16::from_le_bytes(
        take_field(bytes, &mut offset, 2)?
            .try_into()
            .map_err(|_| "fixture narrow width was invalid")?,
    );
    let rows = u16::from_le_bytes(
        take_field(bytes, &mut offset, 2)?
            .try_into()
            .map_err(|_| "fixture row count was invalid")?,
    );
    if initial_columns == 0 || narrow_columns == 0 || rows == 0 {
        return Err("fixture dimensions must be nonzero");
    }

    let mut records = Vec::new();
    let mut seen_stages = [false; 3];
    let mut previous_stage = 0;
    let mut terminated = false;
    while records.len() < MAX_RECORDS {
        let stage = take_field(bytes, &mut offset, 1)?[0];
        let length = u32::from_le_bytes(
            take_field(bytes, &mut offset, 4)?
                .try_into()
                .map_err(|_| "fixture record length was invalid")?,
        ) as usize;
        if stage == 0 {
            if length != 0 || offset != bytes.len() {
                return Err("fixture terminator was malformed or had trailing bytes");
            }
            terminated = true;
            break;
        }
        if !(1..=3).contains(&stage) || stage < previous_stage {
            return Err("fixture replay stages were invalid or out of order");
        }
        if length == 0 || length > MAX_TRACE_BYTES {
            return Err("fixture record length was outside the replay budget");
        }
        let chunk = take_field(bytes, &mut offset, length)?.to_vec();
        seen_stages[usize::from(stage - 1)] = true;
        previous_stage = stage;
        records.push((stage, chunk));
    }

    if !terminated || !seen_stages.into_iter().all(|seen| seen) {
        return Err("fixture was incomplete or exceeded the record budget");
    }
    Ok(ReplayTrace {
        initial_columns,
        narrow_columns,
        rows,
        records,
    })
}

/// Create a bounded engine at the captured PTY grid size so replay uses the production model limits.
///
/// @author kongweiguang
fn engine(columns: u16, rows: u16) -> TerminalEngine {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("positive captured grid");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .expect("captured grid fits terminal limits")
}

/// Locate a visible non-space cell through its UTF-8-to-grid span map without exposing prompt text.
///
/// @author kongweiguang
fn first_non_space_point(snapshot: &RenderSnapshot) -> Option<TerminalPoint> {
    for row in snapshot.rows.iter() {
        for run in row.runs.iter() {
            for span in run.cell_spans.iter() {
                let glyph = run.text.as_bytes().get(span.byte_range.clone())?;
                if glyph.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    return Some(TerminalPoint {
                        line: row.absolute_line,
                        column: span.column_range.start,
                    });
                }
            }
        }
    }
    None
}

/// Check an ASCII cell at an exact viewport coordinate using the engine's authoritative span mapping.
///
/// @author kongweiguang
fn has_ascii_cell(snapshot: &RenderSnapshot, row_index: u16, column: u16, expected: u8) -> bool {
    let Some(row) = snapshot.rows.get(usize::from(row_index)) else {
        return false;
    };
    row.viewport_row == row_index
        && row.runs.iter().any(|run| {
            run.cell_spans.iter().any(|span| {
                span.column_range.start <= column
                    && column < span.column_range.end
                    && run
                        .text
                        .as_bytes()
                        .get(span.byte_range.clone())
                        .is_some_and(|glyph| glyph.len() == 1 && glyph[0] == expected)
            })
        })
}

/// Assert that a recorded cursor is inside the snapshot's actual grid after each resize boundary.
///
/// @author kongweiguang
fn assert_cursor_in_grid(snapshot: &RenderSnapshot) {
    assert!(snapshot.cursor.row < snapshot.size.rows());
    assert!(snapshot.cursor.column < snapshot.size.columns());
}

/// Reproduce the captured 112-to-75-column trace and keep cursor, grid, copied text, and selection state aligned.
///
/// The fixture records incoming ConPTY output only; the final phase is the shell's recorded echo after one
/// synthetic `p`, not an input write or proof that the real long-prompt UI resize issue is fixed.
///
/// @author kongweiguang
#[test]
fn captured_conpty_resize_replays_cursor_grid_and_selection_reflow() {
    assert_eq!(TRACE_BYTES.len(), 733, "R38 fixture size changed");
    let trace = parse_trace(TRACE_BYTES).expect("fixed R38 trace envelope is valid");
    assert_eq!(trace.initial_columns, INITIAL_COLUMNS);
    assert_eq!(trace.narrow_columns, NARROW_COLUMNS);
    assert_eq!(trace.rows, ROWS);

    let mut terminal = engine(trace.initial_columns, trace.rows);
    for (_, chunk) in trace.records.iter().filter(|(stage, _)| *stage == 1) {
        terminal.process_output(chunk);
    }
    let wide = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(wide.size.columns(), INITIAL_COLUMNS);
    assert_eq!(wide.size.rows(), ROWS);
    assert_eq!((wide.cursor.row, wide.cursor.column), (1, 58));
    assert_cursor_in_grid(&wide);

    let selected_prompt_point = first_non_space_point(&wide).expect("synthetic prompt is visible");
    terminal.set_selection(Some(TerminalSelection {
        start: selected_prompt_point,
        end: selected_prompt_point,
        kind: TerminalSelectionKind::Character,
    }));
    assert!(
        terminal
            .selected_text()
            .is_some_and(|selected| !selected.is_empty())
    );

    let narrow_size =
        TerminalSize::new(trace.narrow_columns, trace.rows, 0, 0).expect("positive narrow grid");
    terminal
        .resize(narrow_size)
        .expect("resize to captured narrow grid");
    let after_resize = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(after_resize.size.columns(), NARROW_COLUMNS);
    assert_eq!(after_resize.size.rows(), ROWS);
    assert_cursor_in_grid(&after_resize);
    assert_eq!(after_resize.selection, None);
    assert_eq!(terminal.selected_text(), None);

    for (_, chunk) in trace.records.iter().filter(|(stage, _)| *stage == 2) {
        terminal.process_output(chunk);
    }
    let narrow = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(narrow.size.columns(), NARROW_COLUMNS);
    assert_eq!((narrow.cursor.row, narrow.cursor.column), (2, 20));
    assert_cursor_in_grid(&narrow);

    for (_, chunk) in trace.records.iter().filter(|(stage, _)| *stage == 3) {
        terminal.process_output(chunk);
    }
    let after_known_p_echo = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(
        (
            after_known_p_echo.cursor.row,
            after_known_p_echo.cursor.column
        ),
        (2, 59)
    );
    assert!(has_ascii_cell(&after_known_p_echo, 2, 58, b'p'));
    assert_cursor_in_grid(&after_known_p_echo);

    let p_line = after_known_p_echo.rows[2].absolute_line;
    let p_point = TerminalPoint {
        line: p_line,
        column: 58,
    };
    terminal.set_selection(Some(TerminalSelection {
        start: p_point,
        end: p_point,
        kind: TerminalSelectionKind::Character,
    }));
    let selected_p = terminal.render_snapshot(RenderSnapshotRequest::default());
    let selection = selected_p
        .selection
        .expect("selected cell is visible as a highlight");
    assert_eq!(selection.kind, TerminalSelectionKind::Character);
    assert_eq!(selection.start, p_point);
    assert_eq!(selection.end, p_point);
    assert_eq!(terminal.selected_text().as_deref(), Some("p"));
}
