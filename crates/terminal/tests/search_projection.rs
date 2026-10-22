//! 回归搜索结果在后续快照中的绝对坐标映射与完整逻辑行证据校验。
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::snapshot::{RenderSnapshot, RenderSnapshotRequest, TerminalSearchRequest};

/// 小网格使历史滚动、软折行和裁剪能在固定的行坐标中直接断言。
///
/// @author kongweiguang
fn engine(columns: u16, rows: u16) -> TerminalEngine {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("positive terminal grid");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .expect("bounded terminal engine")
}

/// 显式保留正则和方向参数，避免投影回归把匹配逻辑与导航选择混在一起。
///
/// @author kongweiguang
fn request(query: &str, regex: bool, backwards: bool) -> TerminalSearchRequest {
    TerminalSearchRequest {
        query: query.to_owned(),
        case_sensitive: true,
        regex,
        backwards,
        max_matches: 512,
    }
}

/// 统一从 engine 当前 viewport 取完整行集，投影不读取 UI 保存的旧坐标。
///
/// @author kongweiguang
fn snapshot(engine: &mut TerminalEngine) -> RenderSnapshot {
    engine.render_snapshot(RenderSnapshotRequest::default())
}

/// 输出把旧命中移入 scrollback 后按 origin 平移；方向反转时仍保留原 match index。
///
/// @author kongweiguang
#[test]
fn output_and_scroll_project_cached_matches_with_original_indices() {
    let mut terminal = engine(16, 2);
    terminal.process_output(b"older-hit\r\nnewer-hit");
    let result = terminal
        .search(&request("hit", false, true))
        .expect("search both original rows");
    assert_eq!(result.selected_match, Some(0));
    assert_eq!(result.matches.len(), 2);
    assert_eq!(
        result
            .project_visible_matches(&snapshot(&mut terminal))
            .expect("same revision is valid")
            .len(),
        2
    );

    terminal.process_output(b"\r\nlatest-hit");
    let at_bottom = result
        .project_visible_matches(&snapshot(&mut terminal))
        .expect("same grid epoch can project after output");
    assert_eq!(at_bottom.len(), 1);
    assert_eq!(
        at_bottom[0].0, 0,
        "the original reverse-search index is retained"
    );
    assert_eq!(at_bottom[0].1.range.start.line, 0);

    terminal.scroll(1);
    let scrolled_up = result
        .project_visible_matches(&snapshot(&mut terminal))
        .expect("scrollback rows remain in the current grid");
    assert_eq!(scrolled_up.len(), 2);
    assert_eq!(scrolled_up[0].0, 0);
    assert_eq!(scrolled_up[0].1.range.start.line, 0);
    assert_eq!(scrolled_up[1].0, 1);
    assert_eq!(scrolled_up[1].1.range.start.line, -1);
    assert_eq!(
        result.matches.len(),
        2,
        "projection must not discover later output"
    );
}

/// history limit 裁掉旧命中后即使用户滚到历史顶部，也不把它 clamp 到新内容。
///
/// @author kongweiguang
#[test]
fn trimmed_search_hit_is_not_projected_into_replacement_history() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(2);
    terminal.process_output(b"needle\r\nrow-1\r\nrow-2\r\nrow-3");
    let result = terminal
        .search(&request("needle", false, false))
        .expect("search before trimming");
    assert_eq!(result.matches.len(), 1);

    terminal.process_output(b"\r\nrow-4\r\nrow-5\r\nrow-6");
    terminal.scroll(i32::MAX);
    let projected = result.project_visible_matches(&snapshot(&mut terminal));
    assert!(projected.is_none_or(|matches| matches.is_empty()));
}

/// 第 300 个常规结果仍保留可验证证据，避免投影预算小于 UI 的常用导航区间。
///
/// @author kongweiguang
#[test]
fn projection_budget_keeps_matches_beyond_the_first_256_lines() {
    let mut terminal = engine(16, 2);
    let output = "hit\r\n".repeat(320);
    terminal.process_output(output.as_bytes());
    let result = terminal
        .search(&request("hit", false, false))
        .expect("search all generated hit rows");
    assert_eq!(result.matches.len(), 320);

    let mut found = false;
    for _ in 0..32 {
        terminal.scroll(1);
        let projected = result
            .project_visible_matches(&snapshot(&mut terminal))
            .expect("same grid generation can project cached hits");
        if projected.iter().any(|(match_index, _)| *match_index == 300) {
            found = true;
            break;
        }
    }
    assert!(found, "a hit after index 256 remains projectable");
}

/// 中文宽字、组合字符和 ASCII 命中在后续 scroll origin 变化时保留真实 cell range。
///
/// @author kongweiguang
#[test]
fn unicode_search_range_survives_output_and_scroll() {
    let mut terminal = engine(16, 2);
    let needle = "界é-hit";
    terminal.process_output(format!("{needle}\r\nlater").as_bytes());
    let result = terminal
        .search(&request(needle, false, false))
        .expect("search wide and combining glyphs");
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].range.start.column, 0);
    assert_eq!(result.matches[0].range.end.column, 7);

    terminal.process_output(b"\r\nnew");
    terminal.scroll(1);
    let projected = result
        .project_visible_matches(&snapshot(&mut terminal))
        .expect("unchanged row evidence can follow scroll origin");
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].1.range.start.line, -1);
    assert_eq!(projected[0].1.range.end.line, -1);
    assert_eq!(projected[0].1.range.start.column, 0);
    assert_eq!(projected[0].1.range.end.column, 7);
}

/// 超过单条逻辑行证据上限时仍返回完整搜索；同 revision 可画，后续 revision 安全隐藏。
///
/// @author kongweiguang
#[test]
fn over_budget_soft_wrap_search_remains_complete_but_is_not_reprojected() {
    let mut terminal = engine(4, 3);
    let output = format!("{}needle", "x".repeat(252));
    terminal.process_output(output.as_bytes());
    let result = terminal
        .search(&request("needle", false, false))
        .expect("search a logical line wider than projection evidence allows");
    assert_eq!(result.matches.len(), 1);

    let same_snapshot = snapshot(&mut terminal);
    let same_revision = result
        .project_visible_matches(&same_snapshot)
        .expect("the exact search revision can still draw its match");
    assert_eq!(same_revision.len(), 1);

    terminal.process_output(b"\x1b[?25l");
    let later_snapshot = snapshot(&mut terminal);
    assert!(later_snapshot.revision > same_snapshot.revision);
    let later_revision = result
        .project_visible_matches(&later_snapshot)
        .expect("non-grid revision changes remain compatible");
    assert!(later_revision.is_empty());
}

/// 同一行被覆写时 origin 不变，因此必须靠 cell 与文本 witness 隐藏旧坐标。
///
/// @author kongweiguang
#[test]
fn overwritten_hit_is_hidden_even_without_scrollback_movement() {
    let mut terminal = engine(12, 2);
    terminal.process_output(b"needle");
    let result = terminal
        .search(&request("needle", false, false))
        .expect("search original row");
    terminal.process_output(b"\x1b[1;1Hxxxxxx");

    let projected = result
        .project_visible_matches(&snapshot(&mut terminal))
        .expect("coordinate topology is unchanged");
    assert!(projected.is_empty());
}

/// resize 和 alternate screen 都改变坐标拓扑，旧的 typed projection 必须整体失效。
///
/// @author kongweiguang
#[test]
fn resize_and_alternate_screen_reject_old_search_coordinates() {
    let mut resized = engine(12, 2);
    resized.process_output(b"needle");
    let before_resize = resized
        .search(&request("needle", false, false))
        .expect("search before resize");
    resized
        .resize(TerminalSize::new(10, 2, 0, 0).expect("valid resized grid"))
        .expect("resize terminal");
    assert!(
        before_resize
            .project_visible_matches(&snapshot(&mut resized))
            .is_none()
    );

    let mut alternate = engine(12, 2);
    alternate.process_output(b"needle");
    let before_alternate = alternate
        .search(&request("needle", false, false))
        .expect("search primary grid");
    alternate.process_output(b"\x1b[?1049h");
    assert!(
        before_alternate
            .project_visible_matches(&snapshot(&mut alternate))
            .is_none()
    );
}

/// regex 锚点依赖完整软换行逻辑行；续写同一行后，不能只因匹配子串仍在就保留高亮。
///
/// @author kongweiguang
#[test]
fn soft_wrap_anchor_requires_unchanged_complete_logical_line_context() {
    let mut terminal = engine(4, 3);
    terminal.process_output(b"abcdef");
    let result = terminal
        .search(&request("^ab.*ef *$", true, false))
        .expect("search anchored across soft wrap");
    assert_eq!(result.matches.len(), 1);
    let original = result
        .project_visible_matches(&snapshot(&mut terminal))
        .expect("same revision uses exact search coordinates");
    assert_eq!(original.len(), 1);
    assert_eq!(
        original[0].1.range.start.line + 1,
        original[0].1.range.end.line
    );

    terminal.process_output(b"g");
    let projected = result
        .project_visible_matches(&snapshot(&mut terminal))
        .expect("soft-wrap epoch remains compatible");
    assert!(projected.is_empty(), "the full anchor context changed");
}
