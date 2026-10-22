//! 关键词覆盖投影契约，验证 UTF-8、宽字符、软换行与首条规则优先级。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::preferences::{
    TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule,
    TerminalKeywordHighlightSettings, TerminalKeywordHighlightStyle,
};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::snapshot::{
    RenderSnapshot, TerminalAttributes, TerminalColor, TerminalCursor, TerminalCursorShape,
    TerminalModes, TerminalPalette, TerminalPoint, TerminalRow, TerminalRun, TerminalStyle,
    TerminalTextCellSpan, UnderlineStyle,
};
use kerminal_ui::features::terminal_tools::KeywordOverlayProjector;

/// 使用真实 cell span 构造单 run 行，测试不以 Unicode 字符数猜测终端列宽。
fn test_row(line: i64, wrapped: bool, text: &str, spans: Vec<TerminalTextCellSpan>) -> TerminalRow {
    let end_column = spans
        .iter()
        .map(|span| span.column_range.end)
        .max()
        .unwrap_or_default();
    test_row_with_runs(line, wrapped, vec![test_run(0, end_column, text, spans)])
}

/// 把多段样式 run 放入同一逻辑行，供匹配跨 run 边界测试复用。
fn test_row_with_runs(line: i64, wrapped: bool, runs: Vec<TerminalRun>) -> TerminalRow {
    TerminalRow {
        viewport_row: 0,
        absolute_line: line,
        wrapped,
        runs: Arc::from(runs),
    }
}

/// 创建带真实列边界的测试 run，测试样式切分不改变文本位置。
fn test_run(
    start_column: u16,
    end_column: u16,
    text: &str,
    spans: Vec<TerminalTextCellSpan>,
) -> TerminalRun {
    TerminalRun {
        start_column,
        end_column,
        text: Arc::from(text),
        cell_spans: Arc::from(spans),
        style: TerminalStyle {
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            underline_color: None,
            attributes: TerminalAttributes {
                underline: UnderlineStyle::None,
                ..TerminalAttributes::default()
            },
        },
        hyperlink: None,
    }
}

/// 构造无目录信号的最小快照；固定坐标世代，使关键词测试只覆盖 cache key 与 session 绑定。
///
/// @author kongweiguang
fn test_snapshot(rows: Vec<TerminalRow>) -> RenderSnapshot {
    RenderSnapshot {
        session_id: SessionId::parse("keyword-test-session").expect("fixture id is valid"),
        revision: 7,
        size: TerminalSize::new(80, 24, 0, 0).expect("fixture size is valid"),
        scrollback_offset: 0,
        scroll_origin: 0,
        grid_epoch: 0,
        history_size: 0,
        title: None,
        current_working_directory: None,
        cursor: TerminalCursor {
            row: 0,
            column: 0,
            shape: TerminalCursorShape::Block,
            blinking: false,
            visible: false,
            style_from_terminal: false,
        },
        cursor_cell: None,
        modes: TerminalModes::default(),
        palette: TerminalPalette::default(),
        rows: Arc::from(rows),
        dirty_rows: Arc::from([]),
        selection: None,
        search_matches: Arc::from([]),
    }
}

/// 建立可组合的单规则设置，保留应用层规则身份生成契约。
fn rule(pattern: &str, mode: TerminalKeywordHighlightMatchMode) -> TerminalKeywordHighlightRule {
    let mut rule = TerminalKeywordHighlightRule::new(pattern, "fixture");
    rule.match_mode = mode;
    rule
}

/// 宽字的三个 UTF-8 字节必须投影到引擎给出的两列区间。
#[test]
fn wide_character_uses_engine_cell_span() {
    let row = test_row(
        10,
        false,
        "界",
        vec![TerminalTextCellSpan {
            byte_range: 0..3,
            column_range: 3..5,
        }],
    );
    let snapshot = test_snapshot(vec![row]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    let mut keyword = rule("界", TerminalKeywordHighlightMatchMode::Literal);
    keyword.style = TerminalKeywordHighlightStyle::Blue;
    settings.rules.push(keyword);
    let mut projector = KeywordOverlayProjector::default();

    projector
        .update(settings, false)
        .expect("literal rule compiles");
    let matches = projector.project(&snapshot);

    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].range,
        TerminalPoint {
            line: 10,
            column: 3
        }..TerminalPoint {
            line: 10,
            column: 5
        }
    );
}

/// 相同设置不重编译、不失效缓存，避免 Root 每帧同步设置时产生额外绘制。
#[test]
fn identical_rule_update_reports_no_projection_change() {
    let snapshot = test_snapshot(vec![test_row(
        11,
        false,
        "ERROR",
        vec![TerminalTextCellSpan {
            byte_range: 0..5,
            column_range: 0..5,
        }],
    )]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    settings
        .rules
        .push(rule("ERROR", TerminalKeywordHighlightMatchMode::Literal));
    let mut projector = KeywordOverlayProjector::default();

    assert!(projector.update(settings.clone(), true).unwrap());
    let before = projector.project(&snapshot);
    assert!(!projector.update(settings, true).unwrap());
    let after = projector.project(&snapshot);

    assert_eq!(before.len(), 1);
    assert!(Arc::ptr_eq(&before, &after));
}

/// 编译失败不能覆盖有效规则或缓存；相同坏输入复用安全错误而不重编译。
#[test]
fn invalid_rule_keeps_last_valid_projection_and_cached_error() {
    let snapshot = test_snapshot(vec![test_row(
        12,
        false,
        "ERROR",
        vec![TerminalTextCellSpan {
            byte_range: 0..5,
            column_range: 0..5,
        }],
    )]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    settings
        .rules
        .push(rule("ERROR", TerminalKeywordHighlightMatchMode::Literal));
    let mut projector = KeywordOverlayProjector::default();
    assert!(projector.update(settings.clone(), true).unwrap());
    let before = projector.project(&snapshot);

    settings.rules[0].match_mode = TerminalKeywordHighlightMatchMode::Regex;
    settings.rules[0].pattern = "(".to_owned();
    let first_error = projector.update(settings.clone(), true).unwrap_err();
    let after_failure = projector.project(&snapshot);
    let second_error = projector.update(settings, true).unwrap_err();

    assert_eq!(first_error, "终端关键词规则无效");
    assert_eq!(second_error, first_error);
    assert!(Arc::ptr_eq(&before, &after_failure));
    assert!(Arc::ptr_eq(&after_failure, &projector.project(&snapshot)));
}

/// 匹配跨越软换行和样式 run 后，按两条绝对行分别返回半开 cell 区间。
#[test]
fn match_spans_soft_wrapped_rows() {
    let first = test_row(
        20,
        true,
        "ER",
        vec![
            TerminalTextCellSpan {
                byte_range: 0..1,
                column_range: 0..1,
            },
            TerminalTextCellSpan {
                byte_range: 1..2,
                column_range: 1..2,
            },
        ],
    );
    let second = test_row(
        21,
        false,
        "ROR界",
        vec![
            TerminalTextCellSpan {
                byte_range: 0..1,
                column_range: 0..1,
            },
            TerminalTextCellSpan {
                byte_range: 1..2,
                column_range: 1..2,
            },
            TerminalTextCellSpan {
                byte_range: 2..3,
                column_range: 2..3,
            },
            TerminalTextCellSpan {
                byte_range: 3..6,
                column_range: 3..5,
            },
        ],
    );
    let snapshot = test_snapshot(vec![first, second]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    settings
        .rules
        .push(rule("ERROR界", TerminalKeywordHighlightMatchMode::Literal));
    let mut projector = KeywordOverlayProjector::default();

    projector
        .update(settings, true)
        .expect("literal rule compiles");
    let matches = projector.project(&snapshot);

    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].range.start.line, 20);
    assert_eq!(matches[0].range.start.column, 0);
    assert_eq!(matches[0].range.end.column, 2);
    assert_eq!(matches[1].range.start.line, 21);
    assert_eq!(matches[1].range.start.column, 0);
    assert_eq!(matches[1].range.end.column, 5);
}

/// Unicode 字母和数字边界必须阻止部分词命中，仅输出独立的关键词单元格。
#[test]
fn whole_word_respects_unicode_letters_and_numbers() {
    let text = "x警告 警告2 警告";
    let spans = vec![
        TerminalTextCellSpan {
            byte_range: 0..1,
            column_range: 0..1,
        },
        TerminalTextCellSpan {
            byte_range: 1..4,
            column_range: 1..3,
        },
        TerminalTextCellSpan {
            byte_range: 4..7,
            column_range: 3..5,
        },
        TerminalTextCellSpan {
            byte_range: 7..8,
            column_range: 5..6,
        },
        TerminalTextCellSpan {
            byte_range: 8..11,
            column_range: 6..8,
        },
        TerminalTextCellSpan {
            byte_range: 11..14,
            column_range: 8..10,
        },
        TerminalTextCellSpan {
            byte_range: 14..15,
            column_range: 10..11,
        },
        TerminalTextCellSpan {
            byte_range: 15..16,
            column_range: 11..12,
        },
        TerminalTextCellSpan {
            byte_range: 16..19,
            column_range: 12..14,
        },
        TerminalTextCellSpan {
            byte_range: 19..22,
            column_range: 14..16,
        },
    ];
    let snapshot = test_snapshot(vec![test_row(30, false, text, spans)]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    settings
        .rules
        .push(rule("警告", TerminalKeywordHighlightMatchMode::WholeWord));
    let mut projector = KeywordOverlayProjector::default();

    projector
        .update(settings, true)
        .expect("whole-word rule compiles");
    let matches = projector.project(&snapshot);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].range.start.column, 12);
    assert_eq!(matches[0].range.end.column, 16);
}

/// 有序规则中首条命中占有对应字节，后续重叠关键词不能覆盖其颜色。
#[test]
fn first_rule_wins_overlapping_matches() {
    let row = test_row(
        40,
        false,
        "ERROR",
        vec![TerminalTextCellSpan {
            byte_range: 0..5,
            column_range: 0..5,
        }],
    );
    let snapshot = test_snapshot(vec![row]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    let mut first = rule("ERROR", TerminalKeywordHighlightMatchMode::Literal);
    first.style = TerminalKeywordHighlightStyle::Red;
    let mut overlapping = rule("RR", TerminalKeywordHighlightMatchMode::Literal);
    overlapping.style = TerminalKeywordHighlightStyle::Blue;
    settings.rules.extend([first, overlapping]);
    let mut projector = KeywordOverlayProjector::default();

    projector
        .update(settings, true)
        .expect("literal rules compile");
    let matches = projector.project(&snapshot);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].range.start.column, 0);
    assert_eq!(matches[0].range.end.column, 5);
    assert_eq!(
        matches[0].style.foreground,
        Some(kerminal_terminal::snapshot::TerminalRgb::new(
            0xfc, 0xa5, 0xa5
        ))
    );
}

/// 样式 run 边界不能中断一个词的查找或扩展到旁边未命中的 cell。
#[test]
fn match_spans_adjacent_style_runs() {
    let row = test_row_with_runs(
        50,
        false,
        vec![
            test_run(
                0,
                2,
                "ER",
                vec![TerminalTextCellSpan {
                    byte_range: 0..2,
                    column_range: 0..2,
                }],
            ),
            test_run(
                2,
                5,
                "ROR",
                vec![TerminalTextCellSpan {
                    byte_range: 0..3,
                    column_range: 2..5,
                }],
            ),
        ],
    );
    let snapshot = test_snapshot(vec![row]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    settings
        .rules
        .push(rule("ERROR", TerminalKeywordHighlightMatchMode::Literal));
    let mut projector = KeywordOverlayProjector::default();

    projector
        .update(settings, true)
        .expect("literal rule compiles");
    let matches = projector.project(&snapshot);

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].range.start.column, 0);
    assert_eq!(matches[0].range.end.column, 5);
}

/// 相同 revision 复用 Arc 结果，输出 revision 改变后重新投影以清除过期命中。
#[test]
fn cached_projection_is_invalidated_by_snapshot_revision() {
    let row = test_row(
        60,
        false,
        "ERROR",
        vec![TerminalTextCellSpan {
            byte_range: 0..5,
            column_range: 0..5,
        }],
    );
    let snapshot = test_snapshot(vec![row]);
    let mut settings = TerminalKeywordHighlightSettings::default();
    settings
        .rules
        .push(rule("ERROR", TerminalKeywordHighlightMatchMode::Literal));
    let mut projector = KeywordOverlayProjector::default();
    projector
        .update(settings, true)
        .expect("literal rule compiles");

    let first = projector.project(&snapshot);
    let cached = projector.project(&snapshot);
    let mut next_snapshot = snapshot.clone();
    next_snapshot.revision += 1;
    next_snapshot.rows = Arc::from([test_row(
        60,
        false,
        "clean",
        (0..5)
            .map(|column| TerminalTextCellSpan {
                byte_range: column..column + 1,
                column_range: column as u16..column as u16 + 1,
            })
            .collect(),
    )]);
    let updated = projector.project(&next_snapshot);

    assert!(Arc::ptr_eq(&first, &cached));
    assert_eq!(first.len(), 1);
    assert!(updated.is_empty());
}
