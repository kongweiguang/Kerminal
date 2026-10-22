//! 终端链接扫描器的有限视口、坐标映射和缓存失效契约。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{SessionOwner, TerminalSize},
};
use kerminal_terminal::snapshot::{
    RenderSnapshot, TerminalAttributes, TerminalColor, TerminalCursor, TerminalCursorShape,
    TerminalHyperlink, TerminalModes, TerminalPalette, TerminalPoint, TerminalRow, TerminalRun,
    TerminalStyle, TerminalTextCellSpan,
};

use super::{TerminalLinkDiscoveryCache, TerminalLinkSource, link_at_point};

/// 生成 ASCII run 的逐字节 cell 映射，测试不依赖 GPUI、字体 shaping 或窗口。
///
/// @author kongweiguang
fn ascii_run(text: &str, start_column: u16, hyperlink: Option<TerminalHyperlink>) -> TerminalRun {
    let mut cell_spans = Vec::with_capacity(text.len());
    for (offset, _) in text.char_indices() {
        let column = start_column.saturating_add(offset.min(u16::MAX as usize) as u16);
        cell_spans.push(TerminalTextCellSpan {
            byte_range: offset..offset + 1,
            column_range: column..column.saturating_add(1),
        });
    }
    TerminalRun {
        start_column,
        end_column: start_column.saturating_add(text.len().min(u16::MAX as usize) as u16),
        text: Arc::from(text),
        cell_spans: cell_spans.into(),
        style: style(),
        hyperlink,
    }
}

/// 为宽字符、组合字符和 URL 建立与终端 cell 一致的混合映射。
///
/// @author kongweiguang
fn unicode_prefix_run() -> TerminalRun {
    let prefix = "界e\u{301} ";
    let url = "https://host.test/path";
    let mut spans = vec![
        TerminalTextCellSpan {
            byte_range: 0..3,
            column_range: 0..2,
        },
        TerminalTextCellSpan {
            byte_range: 3..6,
            column_range: 2..3,
        },
        TerminalTextCellSpan {
            byte_range: 6..7,
            column_range: 3..4,
        },
    ];
    let url_offset = prefix.len();
    for (offset, _) in url.char_indices() {
        let column = 4 + offset as u16;
        spans.push(TerminalTextCellSpan {
            byte_range: url_offset + offset..url_offset + offset + 1,
            column_range: column..column + 1,
        });
    }
    let text = format!("{prefix}{url}");
    TerminalRun {
        start_column: 0,
        end_column: 4 + url.len() as u16,
        text: text.into(),
        cell_spans: spans.into(),
        style: style(),
        hyperlink: None,
    }
}

/// 构造只有协议视觉属性的 run，集中保持测试快照与 engine 输出一致。
///
/// @author kongweiguang
fn style() -> TerminalStyle {
    TerminalStyle {
        foreground: TerminalColor::DefaultForeground,
        background: TerminalColor::DefaultBackground,
        underline_color: None,
        attributes: TerminalAttributes::default(),
    }
}

/// 构造有软换行关系的终端行，绝对行号与 viewport row 同步递增。
///
/// @author kongweiguang
fn row(
    viewport_row: u16,
    absolute_line: i64,
    wrapped: bool,
    runs: Vec<TerminalRun>,
) -> TerminalRow {
    TerminalRow {
        viewport_row,
        absolute_line,
        wrapped,
        runs: runs.into(),
    }
}

/// 给缓存测试建立与 session owner 一致的完整可见 binding。
///
/// @author kongweiguang
fn binding(session_id: SessionId, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: generation,
    }
}

/// 用当前快照字段构造纯数据 fixture，确保扫描不依赖 renderer 或 Root。
///
/// @author kongweiguang
/// 链接测试固定坐标世代且不提供目录信号，避免把 OSC7 元数据与文本链接发现混为同一路径。
///
/// @author kongweiguang
fn snapshot(
    session_id: SessionId,
    revision: u64,
    scrollback_offset: usize,
    rows: Vec<TerminalRow>,
    dirty_rows: Vec<u16>,
) -> RenderSnapshot {
    let row_count = rows.len().max(1).min(u16::MAX as usize) as u16;
    RenderSnapshot {
        session_id,
        revision,
        size: TerminalSize::new(120, row_count, 0, 0).expect("non-zero test size"),
        scrollback_offset,
        scroll_origin: 0,
        grid_epoch: 0,
        history_size: scrollback_offset,
        title: None,
        current_working_directory: None,
        cursor: TerminalCursor {
            row: 0,
            column: 0,
            shape: TerminalCursorShape::Block,
            style_from_terminal: false,
            blinking: false,
            visible: true,
        },
        cursor_cell: None,
        modes: TerminalModes::default(),
        palette: TerminalPalette::default(),
        rows: rows.into(),
        dirty_rows: dirty_rows.into(),
        selection: None,
        search_matches: Arc::from([]),
    }
}

/// 普通 URL 可跨越样式 run 和软换行，并映射成绝对半开终端坐标。
///
/// @author kongweiguang
#[test]
fn ordinary_url_crosses_soft_wrap_and_maps_cells() {
    let session_id = SessionId::new();
    let snapshot = snapshot(
        session_id,
        8,
        0,
        vec![
            row(0, 40, true, vec![ascii_run("https://exa", 0, None)]),
            row(1, 41, false, vec![ascii_run("mple.com/path", 0, None)]),
        ],
        vec![0, 1],
    );
    let mut cache = TerminalLinkDiscoveryCache::default();
    let projection = cache
        .update(&binding(snapshot.session_id.clone(), 3), &snapshot)
        .expect("matching binding");

    assert!(!projection.partial);
    assert_eq!(projection.candidates.len(), 1);
    let candidate = &projection.candidates[0];
    assert_eq!(candidate.source(), TerminalLinkSource::Text);
    assert_eq!(candidate.label(), "example.com");
    assert_eq!(
        candidate.range(),
        TerminalPoint {
            line: 40,
            column: 0
        }..TerminalPoint {
            line: 41,
            column: 13
        }
    );
    assert_eq!(candidate.safe_link().as_str(), "https://example.com/path");
    assert_eq!(
        link_at_point(
            &snapshot.rows,
            TerminalPoint {
                line: 41,
                column: 4
            }
        )
        .as_ref()
        .map(|link| link.as_str()),
        Some("https://example.com/path")
    );
}

/// UTF-8 字节不能被当作列数；宽字符、组合序列后的 URL 起点仍回映射到实际 cell。
///
/// @author kongweiguang
#[test]
fn unicode_cell_spans_preserve_url_hit_columns() {
    let rows = [row(0, 77, false, vec![unicode_prefix_run()])];
    let link = link_at_point(
        &rows,
        TerminalPoint {
            line: 77,
            column: 4,
        },
    )
    .expect("URL starts after two-column wide glyph and one combining cluster");
    assert_eq!(link.as_str(), "https://host.test/path");
    assert!(
        link_at_point(
            &rows,
            TerminalPoint {
                line: 77,
                column: 2
            }
        )
        .is_none()
    );
}

/// 直接核对生产命中入口的 OSC 8 宽字符、组合列、半开尾端和绝对行，避免只验证已退役 helper。
///
/// @author kongweiguang
#[test]
fn osc8_hit_uses_cell_columns_half_open_bounds_and_absolute_line() {
    let run = TerminalRun {
        start_column: 1,
        end_column: 4,
        text: Arc::from("界e\u{301}"),
        cell_spans: Arc::from([
            TerminalTextCellSpan {
                byte_range: 0..3,
                column_range: 1..3,
            },
            TerminalTextCellSpan {
                byte_range: 3..6,
                column_range: 3..4,
            },
        ]),
        style: style(),
        hyperlink: Some(TerminalHyperlink {
            id: Arc::from("link"),
            uri: Arc::from("https://example.com/"),
        }),
    };
    let rows = [row(3, 103, false, vec![run])];

    for column in [1, 2, 3] {
        assert_eq!(
            link_at_point(&rows, TerminalPoint { line: 103, column })
                .as_ref()
                .map(|link| link.as_str()),
            Some("https://example.com/")
        );
    }
    assert!(
        link_at_point(
            &rows,
            TerminalPoint {
                line: 103,
                column: 4
            }
        )
        .is_none()
    );
    assert!(link_at_point(&rows, TerminalPoint { line: 3, column: 2 }).is_none());
}

/// OSC 8 显式链接占据范围后不允许文本回退覆盖；带 userinfo 的链接始终因白名单拒绝。
///
/// @author kongweiguang
#[test]
fn osc8_has_priority_and_invalid_target_is_not_recovered_as_plain_text() {
    let session_id = SessionId::new();
    let invalid = TerminalHyperlink {
        id: Arc::from("invalid"),
        uri: Arc::from("https://user:secret@evil.test/path"),
    };
    let safe = TerminalHyperlink {
        id: Arc::from("safe"),
        uri: Arc::from("https://docs.test/guide?q=private"),
    };
    let snapshot = snapshot(
        session_id,
        1,
        0,
        vec![row(
            0,
            1,
            false,
            vec![
                ascii_run("https://evil.test/path", 0, Some(invalid)),
                ascii_run(" https://visible.test", 24, Some(safe)),
            ],
        )],
        vec![0],
    );
    let mut cache = TerminalLinkDiscoveryCache::default();
    let projection = cache
        .update(&binding(snapshot.session_id.clone(), 0), &snapshot)
        .expect("matching binding");

    assert_eq!(projection.candidates.len(), 1);
    assert_eq!(projection.candidates[0].source(), TerminalLinkSource::Osc8);
    assert_eq!(projection.candidates[0].label(), "docs.test");
    assert_eq!(
        projection.candidates[0].safe_link().as_str(),
        "https://docs.test/guide?q=private"
    );
}

/// dirty wrap split/merge 必须同时清除旧逻辑组坐标并发现合并后的完整 URL。
///
/// @author kongweiguang
#[test]
fn wrap_split_and_merge_replace_old_candidates() {
    let session_id = SessionId::new();
    let owner = binding(session_id.clone(), 5);
    let mut cache = TerminalLinkDiscoveryCache::default();
    let merged = snapshot(
        session_id.clone(),
        1,
        0,
        vec![
            row(0, 100, true, vec![ascii_run("https://", 0, None)]),
            row(1, 101, false, vec![ascii_run("example.com", 0, None)]),
        ],
        vec![0, 1],
    );
    let before = cache.update(&owner, &merged).expect("initial view");
    assert_eq!(before.candidates.len(), 1);
    assert_eq!(before.candidates[0].range().end.line, 101);

    let split = snapshot(
        session_id.clone(),
        2,
        0,
        vec![
            row(0, 100, false, vec![ascii_run("https://", 0, None)]),
            row(1, 101, false, vec![ascii_run("example.com", 0, None)]),
        ],
        vec![0],
    );
    let after_split = cache.update(&owner, &split).expect("split view");
    assert!(after_split.candidates.is_empty());

    let merged_again = snapshot(
        session_id,
        3,
        0,
        vec![
            row(0, 100, true, vec![ascii_run("https://", 0, None)]),
            row(1, 101, false, vec![ascii_run("example.com", 0, None)]),
        ],
        vec![0],
    );
    let after_merge = cache.update(&owner, &merged_again).expect("merged view");
    assert_eq!(after_merge.candidates.len(), 1);
    assert_eq!(after_merge.candidates[0].range().end.line, 101);
}

/// 空 dirty set 复用不可变索引；身份或 viewport 变化后旧 event identity 不再能解析。
///
/// @author kongweiguang
#[test]
fn cache_reuses_unchanged_revision_and_rejects_stale_identity() {
    let session_id = SessionId::new();
    let first_binding = binding(session_id.clone(), 7);
    let first = snapshot(
        session_id.clone(),
        4,
        0,
        vec![row(
            0,
            8,
            false,
            vec![ascii_run("https://host.test", 0, None)],
        )],
        vec![0],
    );
    let mut cache = TerminalLinkDiscoveryCache::default();
    let original = cache.update(&first_binding, &first).expect("initial view");
    let unchanged = snapshot(
        session_id.clone(),
        4,
        0,
        vec![row(
            0,
            8,
            false,
            vec![ascii_run("https://host.test", 0, None)],
        )],
        vec![],
    );
    let reused = cache
        .update(&first_binding, &unchanged)
        .expect("same revision");
    assert!(Arc::ptr_eq(&original, &reused));
    assert!(
        cache
            .resolve(&original.identity, original.candidates[0].id())
            .is_some()
    );

    let undamaged_revision = snapshot(
        session_id.clone(),
        5,
        0,
        vec![row(
            0,
            8,
            false,
            vec![ascii_run("https://host.test", 0, None)],
        )],
        vec![],
    );
    let advanced = cache
        .update(&first_binding, &undamaged_revision)
        .expect("revision with no row changes");
    assert_eq!(advanced.identity.revision, 5);
    assert_eq!(advanced.candidates.len(), 1);
    assert!(
        cache
            .resolve(&original.identity, original.candidates[0].id())
            .is_none()
    );

    let next_binding = binding(session_id.clone(), 8);
    let next = snapshot(
        session_id,
        6,
        1,
        vec![row(
            0,
            8,
            false,
            vec![ascii_run("https://host.test", 0, None)],
        )],
        vec![0],
    );
    let current = cache
        .update(&next_binding, &next)
        .expect("new generation and viewport");
    assert!(
        cache
            .resolve(&original.identity, original.candidates[0].id())
            .is_none()
    );
    assert!(
        cache
            .resolve(&current.identity, current.candidates[0].id())
            .is_some()
    );
}

/// 超过逻辑组预算时只返回明确 partial，不以未扫描作为“没有链接”。
///
/// @author kongweiguang
#[test]
fn oversized_soft_wrapped_group_is_reported_partial() {
    let session_id = SessionId::new();
    let text: Arc<str> = Arc::from("x".repeat(80));
    let mut rows = Vec::new();
    for index in 0..820u16 {
        let wrapped = index < 819;
        let run = TerminalRun {
            start_column: 0,
            end_column: 80,
            text: text.clone(),
            cell_spans: Arc::from([TerminalTextCellSpan {
                byte_range: 0..80,
                column_range: 0..80,
            }]),
            style: style(),
            hyperlink: None,
        };
        rows.push(row(index, i64::from(index), wrapped, vec![run]));
    }
    let snapshot = snapshot(session_id.clone(), 1, 0, rows, vec![0]);
    let mut cache = TerminalLinkDiscoveryCache::default();
    let projection = cache
        .update(&binding(session_id, 1), &snapshot)
        .expect("matching binding");
    assert!(projection.partial);
    assert!(projection.candidates.is_empty());
}
