//! 原生视图只在内存中合并插件数据；草稿和可见分页独立于异步请求。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use serde_json::{Map, Value};

const MAX_PATCH_BYTES: usize = 1024 * 1024;
const MAX_FETCH_ROWS: usize = 500;
const MAX_CACHED_ROWS: usize = 4096;

/// 绑定只允许对象键链，避免把数组位置误认为跨更新稳定的字段身份。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BindingPath(Vec<String>);

impl BindingPath {
    /// 双向与只读绑定共用路径模型；可选的 `$` 不进入对象键。
    pub fn parse(path: &str) -> Result<Self, ViewStateError> {
        let path = path.strip_prefix('$').unwrap_or(path);
        let segments = path.split('.').map(str::to_owned).collect::<Vec<_>>();
        if path.len() > 256
            || segments.iter().any(|segment| {
                segment.is_empty()
                    || !segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
        {
            return Err(ViewStateError::InvalidPath);
        }
        Ok(Self(segments))
    }

    /// 暴露拆分后的键用于控件和错误定位，避免各调用方重复解释路径语法。
    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

/// 状态错误不携带插件数据，调用方可映射为固定的本地化反馈。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewStateError {
    InvalidPath,
    PathThroughNonObject,
    PatchTooLarge,
    Closed,
    Busy,
    Stale,
    InvalidRange,
    InvalidPage,
    SequenceExhausted,
}

/// 请求票据固定按钮和代次，迟到回执不能解除新请求的忙碌状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionTicket {
    node: String,
    generation: u64,
    sequence: u64,
}

/// 页请求身份同时包含集合修订和实例代次，避免查询变化后旧行重新出现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageTicket {
    node: String,
    generation: u64,
    revision: u64,
    sequence: u64,
    range: Range<usize>,
}

impl PageTicket {
    /// 范围采用左闭右开坐标，与 RPC 中 `[start, end]` 的语义一致。
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// 集合身份来自经过校验的静态节点，不能由行序号或显示文案推断。
    pub fn node(&self) -> &str {
        &self.node
    }

    /// 让运行时把请求固定到发起时的进程代次。
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// 显示行保留源坐标，刷新期间即使展示旧页也不会伪装成新位置的数据。
#[derive(Clone, Debug, PartialEq)]
pub struct PageRow {
    index: usize,
    value: Value,
}

impl PageRow {
    /// 列表排序和选择使用源坐标，不从当前显示顺序猜测。
    pub fn index(&self) -> usize {
        self.index
    }

    /// 返回原始行而不展开未知字段，交由静态视图声明选择展示内容。
    pub fn value(&self) -> &Value {
        &self.value
    }
}

struct CachedRow {
    sequence: u64,
    value: Value,
}

#[derive(Default)]
struct PageState {
    revision: u64,
    desired: Option<Range<usize>>,
    queued: Vec<Range<usize>>,
    pending: BTreeMap<u64, Range<usize>>,
    rows: BTreeMap<usize, CachedRow>,
    total: Option<usize>,
    previous_total: Option<usize>,
    total_sequence: u64,
    displayed: Vec<PageRow>,
}

/// 每个视图实体独占状态；服务端快照与展示值分开，以便保护未提交输入。
pub struct ViewState {
    remote: Value,
    data: Value,
    dirty: BTreeSet<BindingPath>,
    pending_restores: BTreeSet<BindingPath>,
    new_values: BTreeSet<BindingPath>,
    generation: u64,
    sequence: u64,
    closed: bool,
    actions: BTreeMap<String, u64>,
    pages: BTreeMap<String, PageState>,
}

impl ViewState {
    /// 原始数据只复制一次；实例重启保留这份展示数据及用户草稿。
    pub fn new(data: Value, generation: u64) -> Self {
        Self {
            remote: data.clone(),
            data,
            dirty: BTreeSet::new(),
            pending_restores: BTreeSet::new(),
            new_values: BTreeSet::new(),
            generation,
            sequence: 0,
            closed: false,
            actions: BTreeMap::new(),
            pages: BTreeMap::new(),
        }
    }

    /// 动作快照使用用户看到的数据，但秘密字段必须由调用方排除出此模型。
    pub fn data(&self) -> &Value {
        &self.data
    }

    /// 控件只沿对象路径读取，缺失字段和中间非对象都明确返回空值。
    pub fn value(&self, path: &BindingPath) -> Option<&Value> {
        value_at(&self.data, path.segments())
    }

    /// 独立读取最近的服务端值，让冲突处理不必从展示值反向推断。
    pub fn remote_value(&self, path: &BindingPath) -> Option<&Value> {
        value_at(&self.remote, path.segments())
    }

    /// 局部标记只属于被补丁命中的脏字段，不污染其他字段的状态。
    pub fn has_new_value(&self, path: &BindingPath) -> bool {
        self.new_values.contains(path)
    }

    /// 未提交状态按绑定路径记录，允许父对象和子字段分别保留各自意图。
    pub fn is_dirty(&self, path: &BindingPath) -> bool {
        self.dirty.contains(path)
    }

    /// 宿主关闭页面前必须识别任何未确认绑定，避免跳过草稿确认。
    pub fn has_drafts(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// 只导出脏路径的值，宿主还须按当前控件白名单过滤，不能把整份远端数据落盘。
    pub(super) fn draft_values(&self) -> Vec<(BindingPath, Value)> {
        self.dirty
            .iter()
            .filter_map(|path| self.value(path).cloned().map(|value| (path.clone(), value)))
            .collect()
    }

    /// 静态默认值不能确认恢复草稿；单独保留待权威基线标记，避免无关编辑清掉脏保护。
    pub(super) fn restore_draft(
        &mut self,
        path: &BindingPath,
        value: Value,
    ) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        validate_write_path(&self.data, path.segments())?;
        set_value(&mut self.data, path.segments(), value);
        self.dirty.insert(path.clone());
        self.pending_restores.insert(path.clone());
        Ok(())
    }

    /// 隔离验收只读取脏绑定身份，不能借此修改用户草稿。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn acceptance_dirty_paths(&self) -> Vec<String> {
        self.dirty
            .iter()
            .map(|path| path.segments().join("."))
            .collect()
    }

    /// 用户输入先更新展示数据；中间存在非对象时拒绝覆盖已有结构。
    pub fn edit(&mut self, path: &BindingPath, value: Value) -> Result<bool, ViewStateError> {
        self.ensure_open()?;
        if self.value(path) == Some(&value) {
            return Ok(false);
        }
        validate_write_path(&self.data, path.segments())?;
        set_value(&mut self.data, path.segments(), value);
        self.dirty.insert(path.clone());
        self.clear_equal_drafts();
        Ok(true)
    }

    /// 接受远端值只解除目标及子字段的恢复保护，不能让无关草稿向静态默认值自动收敛。
    pub fn accept_remote(&mut self, path: &BindingPath) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        let value = self.remote_value(path).cloned();
        restore_value(&mut self.data, path.segments(), value);
        self.dirty.retain(|entry| !is_prefix(path, entry));
        self.pending_restores
            .retain(|entry| !is_prefix(path, entry));
        self.new_values.retain(|entry| !is_prefix(path, entry));
        let remaining = self
            .dirty
            .iter()
            .map(|entry| (entry.clone(), self.value(entry).cloned()))
            .collect::<Vec<_>>();
        self.data = self.remote.clone();
        for (entry, value) in remaining {
            restore_value(&mut self.data, entry.segments(), value);
        }
        self.clear_equal_drafts();
        Ok(())
    }

    /// 只有目标已提交回执才能解除对应恢复保护，无关字段仍等待自身权威基线。
    pub fn confirm(&mut self, path: &BindingPath) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        let value = self.value(path).cloned();
        restore_value(&mut self.remote, path.segments(), value);
        self.dirty.retain(|entry| !is_prefix(path, entry));
        self.pending_restores
            .retain(|entry| !is_prefix(path, entry));
        self.new_values.retain(|entry| !is_prefix(path, entry));
        self.clear_equal_drafts();
        Ok(())
    }

    /// 标记可以在用户查看后消除，草稿本身仍保留到确认或明确接受远端值。
    pub fn acknowledge_new_value(&mut self, path: &BindingPath) {
        self.new_values.remove(path);
    }

    /// 补丁只确认其实际命中的恢复路径；无关补丁不能把静态默认值当作已知远端基线。
    pub fn apply_patch(&mut self, patch: &Value) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        let size = serde_json::to_vec(patch)
            .map_err(|_| ViewStateError::PatchTooLarge)?
            .len();
        if size > MAX_PATCH_BYTES {
            return Err(ViewStateError::PatchTooLarge);
        }
        let protected = self
            .dirty
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    self.value(path).cloned(),
                    patch_touches(Some(&self.remote), patch, path.segments()),
                )
            })
            .collect::<Vec<_>>();
        merge_patch(&mut self.remote, patch);
        merge_patch(&mut self.data, patch);
        for (path, value, touched) in protected {
            if touched {
                self.pending_restores.remove(&path);
                self.new_values.insert(path.clone());
            }
            restore_value(&mut self.data, path.segments(), value);
        }
        self.clear_equal_drafts();
        Ok(())
    }

    /// 完整快照建立所有路径的权威基线后才允许等值草稿收敛，同时保留与服务端不同的草稿。
    pub fn replace_remote(&mut self, data: Value) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        let drafts = self
            .dirty
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    self.value(path).cloned(),
                    self.remote_value(path) != value_at(&data, path.segments()),
                )
            })
            .collect::<Vec<_>>();
        self.remote = data.clone();
        self.data = data;
        self.pending_restores.clear();
        for (path, value, changed) in drafts {
            if changed {
                self.new_values.insert(path.clone());
            }
            restore_value(&mut self.data, path.segments(), value);
        }
        self.clear_equal_drafts();
        Ok(())
    }

    /// 同一按钮只允许一个请求，其他按钮的动作可以独立进行。
    pub fn begin_action(&mut self, node: &str) -> Result<ActionTicket, ViewStateError> {
        self.ensure_open()?;
        if self.actions.contains_key(node) {
            return Err(ViewStateError::Busy);
        }
        let sequence = self.next_sequence()?;
        self.actions.insert(node.to_owned(), sequence);
        Ok(ActionTicket {
            node: node.to_owned(),
            generation: self.generation,
            sequence,
        })
    }

    /// 只有匹配按钮、代次和序号的回执才能解除忙碌。
    pub fn finish_action(&mut self, ticket: &ActionTicket) -> Result<(), ViewStateError> {
        self.ensure_current(ticket.generation)?;
        if self.actions.get(&ticket.node) != Some(&ticket.sequence) {
            return Err(ViewStateError::Stale);
        }
        self.actions.remove(&ticket.node);
        Ok(())
    }

    /// 忙碌归属于原按钮实例，不以同名动作推断其他按钮的状态。
    pub fn is_action_busy(&self, node: &str) -> bool {
        self.actions.contains_key(node)
    }

    /// 合并同一轮布局产生的邻接区间，并保留最后一次请求的可见范围。
    pub fn queue_page(&mut self, node: &str, range: Range<usize>) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        if node.is_empty() || range.start >= range.end || range.end - range.start > MAX_FETCH_ROWS {
            return Err(ViewStateError::InvalidRange);
        }
        let page = self.pages.entry(node.to_owned()).or_default();
        page.desired = Some(range.clone());
        page.queued.push(range);
        merge_ranges(&mut page.queued);
        refresh_display(page);
        Ok(())
    }

    /// 缓存和在途请求都参与去重；合并后的长区间拆为协议可接受的小请求。
    pub fn take_page_requests(&mut self, node: &str) -> Result<Vec<PageTicket>, ViewStateError> {
        self.ensure_open()?;
        let Some(page) = self.pages.get(node) else {
            return Ok(Vec::new());
        };
        let ranges = missing_ranges(page);
        let requests = ranges.iter().try_fold(0_u64, |count, range| {
            let length = range.end - range.start;
            let chunks = length / MAX_FETCH_ROWS + usize::from(length % MAX_FETCH_ROWS != 0);
            count
                .checked_add(u64::try_from(chunks).map_err(|_| ViewStateError::SequenceExhausted)?)
                .ok_or(ViewStateError::SequenceExhausted)
        })?;
        self.sequence
            .checked_add(requests)
            .ok_or(ViewStateError::SequenceExhausted)?;
        let mut tickets = Vec::new();
        for range in ranges {
            let mut start = range.start;
            while start < range.end {
                let end = start.saturating_add(MAX_FETCH_ROWS).min(range.end);
                let sequence = self.next_sequence()?;
                let page = self.pages.get_mut(node).ok_or(ViewStateError::Stale)?;
                page.pending.insert(sequence, start..end);
                tickets.push(PageTicket {
                    node: node.to_owned(),
                    generation: self.generation,
                    revision: page.revision,
                    sequence,
                    range: start..end,
                });
                start = end;
            }
        }
        if let Some(page) = self.pages.get_mut(node) {
            page.queued.clear();
        }
        Ok(tickets)
    }

    /// 完整校验行数后再提交；较旧回执既不能回退总数，也不能覆盖新行。
    pub fn finish_page(
        &mut self,
        ticket: &PageTicket,
        total: usize,
        rows: Vec<Value>,
    ) -> Result<(), ViewStateError> {
        self.ensure_page_ticket(ticket)?;
        let expected = ticket
            .range
            .end
            .min(total)
            .saturating_sub(ticket.range.start);
        let page = self
            .pages
            .get_mut(&ticket.node)
            .ok_or(ViewStateError::Stale)?;
        page.pending.remove(&ticket.sequence);
        if rows.len() != expected {
            return Err(ViewStateError::InvalidPage);
        }
        if ticket.sequence > page.total_sequence {
            page.total = Some(total);
            page.total_sequence = ticket.sequence;
            page.rows.retain(|index, _| *index < total);
        }
        let current_total = page.total.unwrap_or(total);
        for (offset, value) in rows.into_iter().enumerate() {
            let index = ticket.range.start + offset;
            if index >= current_total {
                break;
            }
            if page
                .rows
                .get(&index)
                .is_none_or(|row| row.sequence < ticket.sequence)
            {
                page.rows.insert(
                    index,
                    CachedRow {
                        sequence: ticket.sequence,
                        value,
                    },
                );
            }
        }
        trim_cache(page);
        refresh_display(page);
        Ok(())
    }

    /// 失败只撤销对应在途标记，先前完整显示的内容留给用户继续查看。
    pub fn fail_page(&mut self, ticket: &PageTicket) -> Result<(), ViewStateError> {
        self.ensure_page_ticket(ticket)?;
        let page = self
            .pages
            .get_mut(&ticket.node)
            .ok_or(ViewStateError::Stale)?;
        page.pending.remove(&ticket.sequence);
        Ok(())
    }

    /// 只有新的可见区间完整返回后才替换旧行，避免滚动时出现占位闪烁。
    pub fn page_rows(&self, node: &str) -> &[PageRow] {
        self.pages
            .get(node)
            .map_or(&[], |page| page.displayed.as_slice())
    }

    /// 刷新期间沿用旧总数，直到对应范围的数据能够整体切换。
    pub fn page_total(&self, node: &str) -> Option<usize> {
        self.pages
            .get(node)
            .and_then(|page| page.previous_total.or(page.total))
    }

    /// 失效后立即拒绝旧回执，但保留旧行与总数直到新结果完整可见。
    pub fn invalidate_page(&mut self, node: &str) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        let page = self.pages.entry(node.to_owned()).or_default();
        invalidate_page(page)
    }

    /// 进程重启时作废所有在途票据，同时保留用户草稿和上次显示的数据。
    pub fn advance_generation(&mut self, generation: u64) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        if generation <= self.generation {
            return Err(ViewStateError::Stale);
        }
        if self.pages.values().any(|page| page.revision == u64::MAX) {
            return Err(ViewStateError::SequenceExhausted);
        }
        self.generation = generation;
        self.actions.clear();
        for page in self.pages.values_mut() {
            invalidate_page(page)?;
        }
        Ok(())
    }

    /// 关闭后不再接收新工作和迟到结果，重复关闭保持幂等。
    pub fn close(&mut self) {
        self.closed = true;
        self.actions.clear();
        for page in self.pages.values_mut() {
            page.queued.clear();
            page.pending.clear();
        }
    }

    /// 仅已确认远端基线的等值草稿可以消除，重启恢复路径不能与静态默认值比较后提前失去保护。
    fn clear_equal_drafts(&mut self) {
        let equal = self
            .dirty
            .iter()
            .filter(|path| {
                !self.pending_restores.contains(*path)
                    && value_at(&self.data, path.segments())
                        == value_at(&self.remote, path.segments())
            })
            .cloned()
            .collect::<Vec<_>>();
        for path in equal {
            self.dirty.remove(&path);
            self.new_values.remove(&path);
        }
    }

    /// 序号不允许回绕，避免极端情况下旧票据重新变成有效票据。
    fn next_sequence(&mut self) -> Result<u64, ViewStateError> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(ViewStateError::SequenceExhausted)?;
        Ok(self.sequence)
    }

    /// 统一关闭检查，使所有会修改状态的异步入口遵守相同生命周期。
    fn ensure_open(&self) -> Result<(), ViewStateError> {
        if self.closed {
            Err(ViewStateError::Closed)
        } else {
            Ok(())
        }
    }

    /// 代次由宿主分配，进程重启后的旧结果不能写回当前实例。
    fn ensure_current(&self, generation: u64) -> Result<(), ViewStateError> {
        self.ensure_open()?;
        if self.generation == generation {
            Ok(())
        } else {
            Err(ViewStateError::Stale)
        }
    }

    /// 区间和序号都必须与仍在途的请求一致，防止重复或伪造的回执生效。
    fn ensure_page_ticket(&self, ticket: &PageTicket) -> Result<(), ViewStateError> {
        self.ensure_current(ticket.generation)?;
        let page = self.pages.get(&ticket.node).ok_or(ViewStateError::Stale)?;
        if page.revision != ticket.revision
            || page.pending.get(&ticket.sequence) != Some(&ticket.range)
        {
            return Err(ViewStateError::Stale);
        }
        Ok(())
    }
}

/// 路径缺失与显式 null 分开表示，草稿恢复时才能正确执行删除。
fn value_at<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter()
        .try_fold(root, |value, segment| value.as_object()?.get(segment))
}

/// 写入前检查整个已有前缀，失败时不改变任何祖先或兄弟字段。
fn validate_write_path(root: &Value, path: &[String]) -> Result<(), ViewStateError> {
    let mut value = root;
    for segment in path.iter().take(path.len().saturating_sub(1)) {
        let object = value
            .as_object()
            .ok_or(ViewStateError::PathThroughNonObject)?;
        let Some(next) = object.get(segment) else {
            return Ok(());
        };
        value = next;
    }
    if value.is_object() {
        Ok(())
    } else {
        Err(ViewStateError::PathThroughNonObject)
    }
}

/// 调用方先验证写路径；恢复操作复用相同的对象创建逻辑。
fn set_value(root: &mut Value, path: &[String], value: Value) {
    restore_value(root, path, Some(value));
}

/// 恢复草稿时允许重建被远端替换的祖先，但删除缺失值不会凭空创建对象。
fn restore_value(root: &mut Value, path: &[String], value: Option<Value>) {
    let Some((first, remaining)) = path.split_first() else {
        if let Some(value) = value {
            *root = value;
        }
        return;
    };
    if value.is_none() {
        if let Some(object) = root.as_object_mut() {
            if remaining.is_empty() {
                object.remove(first);
            } else if let Some(child) = object.get_mut(first) {
                restore_value(child, remaining, None);
            }
        }
        return;
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    if let Some(object) = root.as_object_mut() {
        if remaining.is_empty() {
            if let Some(value) = value {
                object.insert(first.clone(), value);
            }
        } else {
            let child = object
                .entry(first.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            restore_value(child, remaining, value);
        }
    }
}

/// 遵循 RFC 7386：对象递归合并，null 删除成员，其他值整体替换。
fn merge_patch(target: &mut Value, patch: &Value) {
    let Some(patch) = patch.as_object() else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Some(object) = target.as_object_mut() {
        for (key, value) in patch {
            if value.is_null() {
                object.remove(key);
            } else {
                merge_patch(object.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

/// 祖先被删除或替换时命中后代；仅更新兄弟字段不会制造冲突标记。
fn patch_touches(target: Option<&Value>, patch: &Value, path: &[String]) -> bool {
    if path.is_empty() || !patch.is_object() {
        return true;
    }
    if target.is_some_and(|value| !value.is_object()) {
        return true;
    }
    let Some((first, remaining)) = path.split_first() else {
        return true;
    };
    let Some(next_patch) = patch.as_object().and_then(|object| object.get(first)) else {
        return false;
    };
    let next_target = target
        .and_then(Value::as_object)
        .and_then(|object| object.get(first));
    patch_touches(next_target, next_patch, remaining)
}

/// 采用分段前缀比较，避免 `form.a` 误匹配 `form.ab`。
fn is_prefix(prefix: &BindingPath, path: &BindingPath) -> bool {
    path.segments().starts_with(prefix.segments())
}

/// 先合并重叠和邻接区间，减少同一帧内重复的 RPC 请求。
fn merge_ranges(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

/// 从待发区间减去已有缓存和在途请求，避免布局阶段反复请求同一行。
fn missing_ranges(page: &PageState) -> Vec<Range<usize>> {
    let mut covered = page.pending.values().cloned().collect::<Vec<_>>();
    covered.extend(
        page.rows
            .keys()
            .filter_map(|index| index.checked_add(1).map(|end| *index..end)),
    );
    merge_ranges(&mut covered);
    let mut missing = Vec::new();
    for range in &page.queued {
        let end = page.total.map_or(range.end, |total| range.end.min(total));
        let mut cursor = range.start;
        if cursor >= end {
            continue;
        }
        for existing in &covered {
            if existing.end <= cursor {
                continue;
            }
            if existing.start >= end {
                break;
            }
            if existing.start > cursor {
                missing.push(cursor..existing.start.min(end));
            }
            cursor = cursor.max(existing.end);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            missing.push(cursor..end);
        }
    }
    merge_ranges(&mut missing);
    missing
}

/// 仅在目标范围全部有数据时切换显示，失败和部分返回不制造空行。
fn refresh_display(page: &mut PageState) {
    let (Some(range), Some(total)) = (&page.desired, page.total) else {
        return;
    };
    let end = range.end.min(total);
    let mut displayed = Vec::with_capacity(end.saturating_sub(range.start));
    for index in range.start..end {
        let Some(row) = page.rows.get(&index) else {
            return;
        };
        displayed.push(PageRow {
            index,
            value: row.value.clone(),
        });
    }
    page.displayed = displayed;
    page.previous_total = Some(total);
}

/// 缓存有上限；优先丢弃离当前可见区间最远的行而不影响已显示副本。
fn trim_cache(page: &mut PageState) {
    let excess = page.rows.len().saturating_sub(MAX_CACHED_ROWS);
    if excess == 0 {
        return;
    }
    let desired = page.desired.clone().unwrap_or(0..0);
    let mut candidates = page.rows.keys().copied().collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|index| {
        std::cmp::Reverse(if *index < desired.start {
            desired.start - *index
        } else {
            index.saturating_sub(desired.end.saturating_sub(1))
        })
    });
    for index in candidates.into_iter().take(excess) {
        page.rows.remove(&index);
    }
}

/// 只清除请求缓存；旧显示值与总数保留到新的完整范围到达。
fn invalidate_page(page: &mut PageState) -> Result<(), ViewStateError> {
    page.revision = page
        .revision
        .checked_add(1)
        .ok_or(ViewStateError::SequenceExhausted)?;
    page.previous_total = page.previous_total.or(page.total);
    page.total = None;
    page.total_sequence = 0;
    page.rows.clear();
    page.pending.clear();
    page.queued.clear();
    if let Some(range) = &page.desired {
        page.queued.push(range.clone());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/external_native_state.rs"]
mod tests;
