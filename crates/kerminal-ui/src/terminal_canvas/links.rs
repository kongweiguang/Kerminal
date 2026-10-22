//! 为终端画布与 Context 共用的有限视口链接发现和安全目标索引。
//!
//! 仅依赖 RenderSnapshot 已提供的 UTF-8/cell 映射和出向软换行标记；候选 target
//! 不进入 Debug、展示 label 或跨边界事件，打开前仍由 SafeTerminalLink 白名单约束。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::snapshot::{RenderSnapshot, TerminalPoint, TerminalRow};
use linkify::{LinkFinder, LinkKind};
use url::Url;

use crate::features::terminal_tools::{SafeTerminalLink, validate_terminal_link};

const MAX_SCAN_BYTES: usize = 1024 * 1024;
const MAX_LOGICAL_GROUP_BYTES: usize = 64 * 1024;
const MAX_CANDIDATES: usize = 128;

/// 链接来自终端显式 OSC 8 标记，或由普通屏幕文本安全识别。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TerminalLinkSource {
    Osc8,
    Text,
}

/// Candidate id 在同一快照内以来源和绝对半开坐标稳定定位，不携带 URI。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TerminalLinkCandidateId {
    pub source: TerminalLinkSource,
    pub start: TerminalPoint,
    pub end: TerminalPoint,
}

/// Context 与画布共享的会话/视口版本；新 generation、尺寸或 scrollback 变化使旧候选失效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalLinkSnapshotIdentity {
    pub binding: UiSessionBinding,
    pub revision: u64,
    pub scrollback_offset: usize,
    pub size: TerminalSize,
}

/// 一条可点击候选只公开安全 label 和稳定坐标，安全 target 只能通过受控 getter 读取。
#[derive(Clone)]
pub(crate) struct TerminalLinkCandidate {
    id: TerminalLinkCandidateId,
    label: Arc<str>,
    link: SafeTerminalLink,
}

impl TerminalLinkCandidate {
    /// Context 只展示解析后的 hostname，不把路径、query、fragment 或凭据带入侧栏。
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Root 在身份和候选 id 重新核验后才可将不可伪造的 HTTP(S) target 交给打开器。
    pub(crate) fn safe_link(&self) -> &SafeTerminalLink {
        &self.link
    }

    /// 返回事件所用稳定候选身份，避免 Context 或画布自行生成另一种链接 id。
    pub(crate) fn id(&self) -> TerminalLinkCandidateId {
        self.id
    }

    /// 返回半开绝对终端范围；宽字符和组合字符由 cell span 映射而非 UTF-8 长度推算。
    pub(crate) fn range(&self) -> Range<TerminalPoint> {
        self.id.start..self.id.end
    }

    /// 返回 OSC 8 或普通文本来源，供 Context 的有限投影展示来源类型。
    pub(crate) fn source(&self) -> TerminalLinkSource {
        self.id.source
    }
}

/// 当前视口扫描结果；partial 为真时不能把未扫描范围解释为没有链接。
#[derive(Clone)]
pub(crate) struct TerminalLinkSnapshot {
    pub identity: TerminalLinkSnapshotIdentity,
    pub candidates: Arc<[TerminalLinkCandidate]>,
    pub partial: bool,
}

/// 每 pane 保留一份视口索引；变化行按软换行逻辑组失效，避免 render/mousemove 全量扫描。
#[derive(Default)]
pub(crate) struct TerminalLinkDiscoveryCache {
    snapshot: Option<Arc<TerminalLinkSnapshot>>,
    groups: BTreeMap<i64, CachedLogicalGroup>,
    row_to_group: BTreeMap<u16, i64>,
    layout: Vec<RowLayout>,
    partial: bool,
    requires_full_rebuild: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct RowLayout {
    viewport_row: u16,
    absolute_line: i64,
    wrapped: bool,
}

struct CachedLogicalGroup {
    candidates: Vec<TerminalLinkCandidate>,
    scanned_bytes: usize,
    partial: bool,
}

struct LogicalGroup<'a> {
    key: i64,
    rows: Vec<&'a TerminalRow>,
}

struct CellMap {
    bytes: Range<usize>,
    range: Range<TerminalPoint>,
}

struct GroupScan {
    candidates: Vec<TerminalLinkCandidate>,
    bytes_scanned: usize,
    partial: bool,
    requires_rescan: bool,
}

impl TerminalLinkDiscoveryCache {
    /// 只接受与完整 UI binding 对应的快照；同 revision 重用索引，dirty rows 更新受影响逻辑组。
    ///
    /// 软换行拓扑变化也加入失效集，以同时删除被拆开的旧候选和发现合并后的新候选。
    /// 视口身份改变、行号平移或先前受预算截断时全量重建，避免陈旧链接被保留。
    ///
    /// @author kongweiguang
    pub(crate) fn update(
        &mut self,
        binding: &UiSessionBinding,
        snapshot: &RenderSnapshot,
    ) -> Option<Arc<TerminalLinkSnapshot>> {
        if binding.session_id != snapshot.session_id {
            return None;
        }

        let identity = TerminalLinkSnapshotIdentity {
            binding: binding.clone(),
            revision: snapshot.revision,
            scrollback_offset: snapshot.scrollback_offset,
            size: snapshot.size,
        };
        let groups = logical_groups(&snapshot.rows);
        let row_to_group = row_group_map(&groups);
        let layout = row_layout(&snapshot.rows);

        let Some(current) = self.snapshot.clone() else {
            return Some(self.rebuild(identity, groups, row_to_group, layout));
        };

        if same_view(&current.identity, &identity) {
            if snapshot.revision < current.identity.revision
                && !(current.identity.revision > u64::MAX / 2 && snapshot.revision < u64::MAX / 2)
            {
                return None;
            }
            if snapshot.revision == current.identity.revision && self.layout == layout {
                return Some(current.clone());
            }
        }

        if !same_view(&current.identity, &identity)
            || snapshot.revision < current.identity.revision
            || row_positions_changed(&self.layout, &layout)
            || self.requires_full_rebuild
        {
            return Some(self.rebuild(identity, groups, row_to_group, layout));
        }

        let mut affected_rows = BTreeSet::new();
        for row in snapshot.dirty_rows.iter().copied() {
            add_neighbor_rows(row, &mut affected_rows);
        }
        for (old, new) in self.layout.iter().zip(&layout) {
            if old.wrapped != new.wrapped {
                add_neighbor_rows(new.viewport_row, &mut affected_rows);
            }
        }

        if affected_rows.is_empty() {
            self.snapshot = Some(Arc::new(TerminalLinkSnapshot {
                identity,
                candidates: current.candidates.clone(),
                partial: self.partial,
            }));
            return self.snapshot.clone();
        }

        if affected_rows
            .iter()
            .any(|row| !self.row_to_group.contains_key(row) && !row_to_group.contains_key(row))
        {
            return Some(self.rebuild(identity, groups, row_to_group, layout));
        }

        let mut affected_keys = BTreeSet::new();
        for row in &affected_rows {
            if let Some(key) = self.row_to_group.get(row) {
                affected_keys.insert(*key);
            }
            if let Some(key) = row_to_group.get(row) {
                affected_keys.insert(*key);
            }
        }

        if affected_keys.is_empty() {
            return Some(self.rebuild(identity, groups, row_to_group, layout));
        }

        for key in &affected_keys {
            self.groups.remove(key);
        }
        let mut bytes_remaining = MAX_SCAN_BYTES.saturating_sub(self.cached_bytes());
        let mut candidates_remaining = MAX_CANDIDATES.saturating_sub(self.cached_candidates());
        let mut partial = self.groups.values().any(|group| group.partial);
        let mut requires_full_rebuild = false;

        for group in groups {
            if !affected_keys.contains(&group.key) {
                continue;
            }
            let scan = scan_group(&group, bytes_remaining, candidates_remaining);
            bytes_remaining = bytes_remaining.saturating_sub(scan.bytes_scanned);
            candidates_remaining = candidates_remaining.saturating_sub(scan.candidates.len());
            self.groups.insert(
                group.key,
                CachedLogicalGroup {
                    candidates: scan.candidates,
                    scanned_bytes: scan.bytes_scanned,
                    partial: scan.partial,
                },
            );
            if scan.partial {
                partial = true;
            }
            if scan.requires_rescan {
                requires_full_rebuild = true;
                break;
            }
        }

        self.row_to_group = row_to_group;
        self.layout = layout;
        self.partial = partial;
        self.requires_full_rebuild = requires_full_rebuild;
        let next = self.make_snapshot(identity);
        self.snapshot = Some(next.clone());
        Some(next)
    }

    /// 仅用完全相同的 binding/视口 revision 解析候选，过期 Context 事件不会取得旧 URI。
    ///
    /// @author kongweiguang
    pub(crate) fn resolve(
        &self,
        identity: &TerminalLinkSnapshotIdentity,
        id: TerminalLinkCandidateId,
    ) -> Option<&SafeTerminalLink> {
        let snapshot = self.snapshot.as_ref()?;
        if &snapshot.identity != identity {
            return None;
        }
        snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.id() == id)
            .map(TerminalLinkCandidate::safe_link)
    }

    /// 返回最新不可变投影供 Root 同一 accepted snapshot 流程传给 Context。
    ///
    /// @author kongweiguang
    pub(crate) fn snapshot(&self) -> Option<&Arc<TerminalLinkSnapshot>> {
        self.snapshot.as_ref()
    }

    /// 用完整可见视口替换索引；全量路径只在初次、视口身份变化、行轴移动或 partial 后运行。
    ///
    /// @author kongweiguang
    fn rebuild(
        &mut self,
        identity: TerminalLinkSnapshotIdentity,
        groups: Vec<LogicalGroup<'_>>,
        row_to_group: BTreeMap<u16, i64>,
        layout: Vec<RowLayout>,
    ) -> Arc<TerminalLinkSnapshot> {
        self.groups.clear();
        self.partial = false;
        self.requires_full_rebuild = false;

        let mut bytes_remaining = MAX_SCAN_BYTES;
        let mut candidates_remaining = MAX_CANDIDATES;
        for group in groups {
            let scan = scan_group(&group, bytes_remaining, candidates_remaining);
            bytes_remaining = bytes_remaining.saturating_sub(scan.bytes_scanned);
            candidates_remaining = candidates_remaining.saturating_sub(scan.candidates.len());
            self.groups.insert(
                group.key,
                CachedLogicalGroup {
                    candidates: scan.candidates,
                    scanned_bytes: scan.bytes_scanned,
                    partial: scan.partial,
                },
            );
            if scan.partial {
                self.partial = true;
            }
            if scan.requires_rescan {
                self.requires_full_rebuild = true;
                break;
            }
        }

        self.row_to_group = row_to_group;
        self.layout = layout;
        let next = self.make_snapshot(identity);
        self.snapshot = Some(next.clone());
        next
    }

    /// 汇总已缓存字节；partial 缓存会在下一次变化时整体重建，避免把截断状态误当完整。
    ///
    /// @author kongweiguang
    fn cached_bytes(&self) -> usize {
        self.groups.values().map(|group| group.scanned_bytes).sum()
    }

    /// 汇总已缓存候选数，提供增量扫描剩余预算而不复制所有安全 target。
    ///
    /// @author kongweiguang
    fn cached_candidates(&self) -> usize {
        self.groups
            .values()
            .map(|group| group.candidates.len())
            .sum()
    }

    /// 生成按终端坐标排序的不可变投影；候选 target 仍被私有字段封装且不会进入 Debug。
    ///
    /// @author kongweiguang
    fn make_snapshot(&self, identity: TerminalLinkSnapshotIdentity) -> Arc<TerminalLinkSnapshot> {
        let mut candidates = self
            .groups
            .values()
            .flat_map(|group| group.candidates.iter().cloned())
            .collect::<Vec<_>>();
        candidates.sort_by_key(candidate_sort_key);
        Arc::new(TerminalLinkSnapshot {
            identity,
            candidates: candidates.into(),
            partial: self.partial,
        })
    }
}

/// 画布点击时只扫描命中的逻辑行组；Context 的全视口缓存仍由 Root 在快照接收点增量维护。
///
/// @author kongweiguang
pub(crate) fn link_at_point(
    rows: &[TerminalRow],
    point: TerminalPoint,
) -> Option<SafeTerminalLink> {
    let group = logical_groups(rows)
        .into_iter()
        .find(|group| group.rows.iter().any(|row| row.absolute_line == point.line))?;
    scan_group(&group, MAX_LOGICAL_GROUP_BYTES, MAX_CANDIDATES)
        .candidates
        .into_iter()
        .filter(|candidate| range_contains(&candidate.range(), point))
        .min_by_key(|candidate| {
            (
                match candidate.source() {
                    TerminalLinkSource::Osc8 => 0,
                    TerminalLinkSource::Text => 1,
                },
                candidate_sort_key(candidate),
            )
        })
        .map(|candidate| candidate.link)
}

/// 将当前 viewport 按前一物理行的 WRAPLINE 标记分组，硬换行之间绝不拼接 URL。
///
/// @author kongweiguang
fn logical_groups(rows: &[TerminalRow]) -> Vec<LogicalGroup<'_>> {
    let mut rows = rows.iter().collect::<Vec<_>>();
    rows.sort_by_key(|row| row.viewport_row);
    let mut groups = Vec::new();
    let mut current: Vec<&TerminalRow> = Vec::new();

    for row in rows {
        let joins_previous = current.last().is_some_and(|previous| {
            previous.wrapped
                && previous.viewport_row.saturating_add(1) == row.viewport_row
                && previous.absolute_line.saturating_add(1) == row.absolute_line
        });
        if !current.is_empty() && !joins_previous {
            groups.push(LogicalGroup {
                key: current[0].absolute_line,
                rows: std::mem::take(&mut current),
            });
        }
        current.push(row);
    }
    if !current.is_empty() {
        groups.push(LogicalGroup {
            key: current[0].absolute_line,
            rows: current,
        });
    }
    groups
}

/// 为每个 viewport row 记录它所属逻辑组，dirty-row 更新可同时清理合并前后的旧 key。
///
/// @author kongweiguang
fn row_group_map(groups: &[LogicalGroup<'_>]) -> BTreeMap<u16, i64> {
    groups
        .iter()
        .flat_map(|group| {
            group
                .rows
                .iter()
                .map(move |row| (row.viewport_row, group.key))
        })
        .collect()
}

/// 保存绝对行拓扑；相同 scrollback offset 下行轴变化需要重建，软换行变化则走局部失效。
///
/// @author kongweiguang
fn row_layout(rows: &[TerminalRow]) -> Vec<RowLayout> {
    let mut layout = rows
        .iter()
        .map(|row| RowLayout {
            viewport_row: row.viewport_row,
            absolute_line: row.absolute_line,
            wrapped: row.wrapped,
        })
        .collect::<Vec<_>>();
    layout.sort_by_key(|row| row.viewport_row);
    layout
}

/// binding 或视口参数变化时不能复用另一个 pane、generation、大小或 scrollback 的候选。
///
/// @author kongweiguang
fn same_view(left: &TerminalLinkSnapshotIdentity, right: &TerminalLinkSnapshotIdentity) -> bool {
    left.binding == right.binding
        && left.scrollback_offset == right.scrollback_offset
        && left.size == right.size
}

/// 行号变化意味着可见屏幕滚动或快照窗口已移动，不能按局部 dirty row 沿用坐标索引。
///
/// @author kongweiguang
fn row_positions_changed(old: &[RowLayout], new: &[RowLayout]) -> bool {
    old.len() != new.len()
        || old.iter().zip(new).any(|(left, right)| {
            left.viewport_row != right.viewport_row || left.absolute_line != right.absolute_line
        })
}

/// 一个 damage 可能改变被标记行的组和前后相邻边界，失效集向两侧各扩一物理行。
///
/// @author kongweiguang
fn add_neighbor_rows(row: u16, affected: &mut BTreeSet<u16>) {
    affected.insert(row);
    if row > 0 {
        affected.insert(row - 1);
    }
    if row < u16::MAX {
        affected.insert(row + 1);
    }
}

/// 扫描一组连续软换行行；超预算时仍保留可见 OSC 8，但普通文本候选明确标 partial。
///
/// @author kongweiguang
fn scan_group(group: &LogicalGroup<'_>, max_bytes: usize, max_candidates: usize) -> GroupScan {
    if max_candidates == 0 {
        return GroupScan {
            candidates: Vec::new(),
            bytes_scanned: 0,
            partial: true,
            requires_rescan: true,
        };
    }

    let text_bytes = group
        .rows
        .iter()
        .flat_map(|row| row.runs.iter())
        .map(|run| run.text.len())
        .fold(0usize, usize::saturating_add);
    let too_large = text_bytes > max_bytes || text_bytes > MAX_LOGICAL_GROUP_BYTES;
    let mut candidates = Vec::new();
    let mut osc_covered = Vec::new();
    let mut text_offset = 0usize;
    let mut partial = false;
    let mut requires_rescan = false;

    for row in &group.rows {
        for run in row.runs.iter() {
            let run_start = text_offset;
            text_offset = text_offset.saturating_add(run.text.len());
            if let Some(hyperlink) = run.hyperlink.as_ref() {
                if run_start < text_offset {
                    osc_covered.push(run_start..text_offset);
                }
                if let Ok(link) = validate_terminal_link(&hyperlink.uri)
                    && let Some(candidate) = candidate(
                        TerminalLinkSource::Osc8,
                        TerminalPoint {
                            line: row.absolute_line,
                            column: run.start_column,
                        },
                        TerminalPoint {
                            line: row.absolute_line,
                            column: run.end_column,
                        },
                        link,
                    )
                {
                    if candidates.len() < max_candidates {
                        candidates.push(candidate);
                    } else {
                        partial = true;
                        requires_rescan = true;
                    }
                }
            }
        }
    }

    if too_large {
        sort_candidates(&mut candidates);
        return GroupScan {
            candidates,
            bytes_scanned: max_bytes.min(text_bytes),
            partial: true,
            requires_rescan: true,
        };
    }

    let mut text = String::with_capacity(text_bytes);
    let mut cell_maps = Vec::new();
    let mut byte_offset = 0usize;
    for row in &group.rows {
        for run in row.runs.iter() {
            let run_offset = byte_offset;
            text.push_str(&run.text);
            byte_offset = byte_offset.saturating_add(run.text.len());
            for span in run.cell_spans.iter() {
                if span.byte_range.start >= span.byte_range.end
                    || span.byte_range.end > run.text.len()
                    || span.column_range.start >= span.column_range.end
                {
                    continue;
                }
                cell_maps.push(CellMap {
                    bytes: run_offset + span.byte_range.start..run_offset + span.byte_range.end,
                    range: TerminalPoint {
                        line: row.absolute_line,
                        column: span.column_range.start,
                    }..TerminalPoint {
                        line: row.absolute_line,
                        column: span.column_range.end,
                    },
                });
            }
        }
    }

    if !partial {
        let mut finder = LinkFinder::new();
        finder.kinds(&[LinkKind::Url]);
        finder.url_must_have_scheme(true);
        for found in finder.links(&text) {
            let start_byte = found.start();
            let end_byte = found.end();
            if start_byte >= end_byte
                || osc_covered
                    .iter()
                    .any(|range| start_byte < range.end && range.start < end_byte)
            {
                continue;
            }
            if candidates.len() >= max_candidates {
                partial = true;
                requires_rescan = true;
                break;
            }
            let (Some(start), Some(end)) = (
                point_for_start(&cell_maps, start_byte),
                point_after_byte(&cell_maps, end_byte),
            ) else {
                partial = true;
                requires_rescan = true;
                continue;
            };
            let Ok(link) = validate_terminal_link(found.as_str()) else {
                continue;
            };
            if let Some(candidate) = candidate(TerminalLinkSource::Text, start, end, link) {
                candidates.push(candidate);
            }
        }
    }

    // 视口末尾若明确带 WRAPLINE，紧贴结尾的 URL 可能尚未完整到达，不能伪装成完整候选。
    if let Some(last_row) = group.rows.last().filter(|row| row.wrapped) {
        let visible_end_column = last_row.runs.last().map_or(0, |run| run.end_column);
        candidates.retain(|candidate| {
            let range = candidate.range();
            candidate.source() == TerminalLinkSource::Osc8
                || range.end.line != last_row.absolute_line
                || range.end.column < visible_end_column
        });
        partial = true;
    }

    sort_candidates(&mut candidates);
    GroupScan {
        candidates,
        bytes_scanned: text_bytes,
        partial,
        requires_rescan,
    }
}

/// 映射 LinkFinder 字节起点到包含该 UTF-8 字节的 Alacritty cell，避免字宽估算。
///
/// @author kongweiguang
fn point_for_start(maps: &[CellMap], byte: usize) -> Option<TerminalPoint> {
    maps.iter()
        .find(|map| map.bytes.start <= byte && byte < map.bytes.end)
        .map(|map| map.range.start)
}

/// 把半开字节终点映射为最后一个字符 cell 的半开列终点。
///
/// @author kongweiguang
fn point_after_byte(maps: &[CellMap], byte_end: usize) -> Option<TerminalPoint> {
    let byte = byte_end.checked_sub(1)?;
    maps.iter()
        .find(|map| map.bytes.start <= byte && byte < map.bytes.end)
        .map(|map| map.range.end)
}

/// 从已验证 URI 提取不含路径和 query 的 hostname 展示值，安全 target 独立保存在私有字段。
///
/// @author kongweiguang
fn candidate(
    source: TerminalLinkSource,
    start: TerminalPoint,
    end: TerminalPoint,
    link: SafeTerminalLink,
) -> Option<TerminalLinkCandidate> {
    if point_key(start) >= point_key(end) {
        return None;
    }
    let parsed = Url::parse(link.as_str()).ok()?;
    let label: Arc<str> = Arc::from(parsed.host_str()?);
    Some(TerminalLinkCandidate {
        id: TerminalLinkCandidateId { source, start, end },
        label,
        link,
    })
}

/// 按绝对行和列进行半开范围判断，支持普通 URL 跨越多个软换行物理行。
///
/// @author kongweiguang
fn range_contains(range: &Range<TerminalPoint>, point: TerminalPoint) -> bool {
    point_key(range.start) <= point_key(point) && point_key(point) < point_key(range.end)
}

/// 将坐标转为稳定比较键；TerminalPoint 刻意不实现全序，避免其通用排序语义被误用。
///
/// @author kongweiguang
fn point_key(point: TerminalPoint) -> (i64, u16) {
    (point.line, point.column)
}

/// 先展示 OSC 8，其次按绝对坐标与来源排序，确保重绘和 Context 投影顺序稳定。
///
/// @author kongweiguang
fn sort_candidates(candidates: &mut [TerminalLinkCandidate]) {
    candidates.sort_by_key(candidate_sort_key);
}

/// 生成不含 URI 内容的稳定排序键。
///
/// @author kongweiguang
fn candidate_sort_key(candidate: &TerminalLinkCandidate) -> (i64, u16, u8, i64, u16) {
    (
        candidate.id.start.line,
        candidate.id.start.column,
        match candidate.source() {
            TerminalLinkSource::Osc8 => 0,
            TerminalLinkSource::Text => 1,
        },
        candidate.id.end.line,
        candidate.id.end.column,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/terminal_link_discovery.rs"]
mod tests;
