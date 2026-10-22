use std::ops::Range;

use gpui::{
    Along, App, Axis, Bounds, Context, ElementId, EventEmitter, IsZero, Pixels, Window, px,
};

mod panel;
mod resize_handle;
pub use panel::*;
#[doc(hidden)]
pub use resize_handle::*;

#[doc(hidden)]
pub const PANEL_MIN_SIZE: Pixels = px(100.);

/// Create a [`ResizablePanelGroup`] with horizontal resizing
pub fn h_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup {
    ResizablePanelGroup::new(id).axis(Axis::Horizontal)
}

/// Create a [`ResizablePanelGroup`] with vertical resizing
pub fn v_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup {
    ResizablePanelGroup::new(id).axis(Axis::Vertical)
}

/// Create a [`ResizablePanel`].
pub fn resizable_panel() -> ResizablePanel {
    ResizablePanel::new()
}

/// State for a [`ResizablePanel`]
#[derive(Debug, Clone)]
pub struct ResizableState {
    /// The `axis` will sync to actual axis of the ResizablePanelGroup in use.
    axis: Axis,
    panels: Vec<ResizablePanelState>,
    sizes: Vec<Pixels>,
    resizing_panel_ix: Option<usize>,
    /// 被固定像素尺寸的面板索引。
    preserve_panel_size_ix: Option<usize>,
    /// 独立保存的用户首选值，避免临时空间夹紧覆盖它。
    preferred_preserved_panel_size: Option<Pixels>,
    bounds: Bounds<Pixels>,
}

impl Default for ResizableState {
    /// 默认不钉住任何面板，只有组显式 opt-in 后才保留像素首选。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            axis: Axis::Horizontal,
            panels: vec![],
            sizes: vec![],
            resizing_panel_ix: None,
            preserve_panel_size_ix: None,
            preferred_preserved_panel_size: None,
            bounds: Bounds::default(),
        }
    }
}

impl ResizableState {
    /// Get the size of the panels.
    pub fn sizes(&self) -> &Vec<Pixels> {
        &self.sizes
    }

    /// Programmatically resize the panel at `ix` to `size`, redistributing
    /// space among siblings using the same logic as a drag.
    ///
    /// Sizes are clamped to the panel's `size_range` and to the container.
    /// Emits `ResizablePanelEvent::Resized` so subscribers (e.g. preference
    /// persistence) see the change just as if the user had dragged a handle.
    ///
    /// Out-of-range indices are a no-op. For the last panel, space is taken
    /// from the previous sibling (the last panel has no handle of its own).
    /// 程序化调整仍使用统一尺寸分配；固定槽会记录该明确请求作为新的首选宽度。
    ///
    /// 请求值仅按面板自身范围夹紧，不按当前容器空间夹紧；临时缩窄值应由调用方读取首选值恢复。
    ///
    /// @author kongweiguang
    pub fn resize_panel(
        &mut self,
        ix: usize,
        size: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.sizes.len() {
            return;
        }
        if ix + 1 < self.sizes.len() {
            self.resize_panel_at_handle(ix, size, window, cx, false);
        } else if ix > 0 {
            // Last panel: drive its size by resizing the previous sibling so
            // the freed space lands here.
            let delta = self.sizes[ix] - size;
            let prev = self.sizes[ix - 1];
            self.resize_panel_at_handle(ix - 1, prev + delta, window, cx, false);
        }
        if self.preserve_panel_size_ix == Some(ix) {
            let range = self.panel_size_range(ix);
            self.preferred_preserved_panel_size = Some(size.clamp(range.start, range.end));
        }
        self.done_resizing(cx);
    }

    /// 读取固定面板的独立首选尺寸，不以临时容器有效尺寸作为回退值。
    ///
    /// 折叠/恢复调用方可据此保存用户意图；尚未测量或未启用固定策略时返回 `None`。
    ///
    /// @author kongweiguang
    pub fn preferred_panel_size(&self, ix: usize) -> Option<Pixels> {
        if self.preserve_panel_size_ix == Some(ix) {
            self.preferred_preserved_panel_size
        } else {
            None
        }
    }

    /// Insert a panel state at `ix`, or append it when no index is supplied.
    ///
    /// Existing panel sizes are redistributed so their total remains equal to
    /// the current container size.
    /// 插入面板时随原目标移动索引，避免首选尺寸附着到新插入的槽。
    ///
    /// @author kongweiguang
    pub fn insert_panel(
        &mut self,
        size: Option<Pixels>,
        ix: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let panel_state = ResizablePanelState {
            size,
            ..Default::default()
        };

        let size = size.unwrap_or(PANEL_MIN_SIZE);

        // We make sure that the size always sums up to the container size
        // by reducing the size of all other panels first.
        let container_size = self.container_size().max(px(1.));
        let total_leftover_size = (container_size - size).max(px(1.));

        for (i, panel) in self.panels.iter_mut().enumerate() {
            let ratio = self.sizes[i] / container_size;
            self.sizes[i] = total_leftover_size * ratio;
            panel.size = Some(self.sizes[i]);
        }

        if let Some(ix) = ix {
            self.panels.insert(ix, panel_state);
            self.sizes.insert(ix, size);
            if self
                .preserve_panel_size_ix
                .is_some_and(|fixed_ix| ix <= fixed_ix)
            {
                self.preserve_panel_size_ix =
                    self.preserve_panel_size_ix.map(|fixed_ix| fixed_ix + 1);
            }
        } else {
            self.panels.push(panel_state);
            self.sizes.push(size);
        };

        cx.notify();
    }

    /// Adopt slot sizes decided by an owner that keeps its own record of the
    /// layout — the dock's pane tree does.
    ///
    /// Unlike [`Self::insert_panel`], nothing is redistributed: the caller has
    /// already decided how the space divides, and re-normalizing here would
    /// undo exactly that decision. Slots the caller left unconstrained keep
    /// whatever they had.
    pub(crate) fn adopt_sizes(&mut self, sizes: &[Option<Pixels>], cx: &mut Context<Self>) {
        let mut changed = false;
        for (ix, size) in sizes.iter().enumerate() {
            // The preference is mirrored exactly, `None` included. That is the
            // load-bearing half: `insert_panel` resolves every existing
            // panel's `None` into a concrete value as a side effect of
            // redistributing, so after inserting one slot the caller's "these
            // two are equally unconstrained" has quietly become "that one is
            // pinned, this one is the only flexible slot" — and the flexible
            // one then swallows whatever the pinned ones leave over.
            if let Some(panel) = self.panels.get_mut(ix) {
                if panel.size != *size {
                    panel.size = *size;
                    changed = true;
                }
            }

            // The measurement only moves when the tree names a size; an
            // unconstrained slot keeps whatever it was last laid out at until
            // the next pass recomputes it.
            let Some(size) = size else { continue };
            if let Some(slot) = self.sizes.get_mut(ix) {
                if *slot != *size {
                    *slot = *size;
                    changed = true;
                }
            }
        }

        if changed {
            cx.notify();
        }
    }

    /// 同步数量时清除已经不存在的固定目标，防止误固定另一面板。
    ///
    /// @author kongweiguang
    pub(crate) fn sync_panels_count(
        &mut self,
        axis: Axis,
        panels_count: usize,
        cx: &mut Context<Self>,
    ) {
        let mut changed = self.axis != axis;
        self.axis = axis;

        if panels_count > self.panels.len() {
            let diff = panels_count - self.panels.len();
            self.panels
                .extend(vec![ResizablePanelState::default(); diff]);
            self.sizes.extend(vec![PANEL_MIN_SIZE; diff]);
            changed = true;
        }

        if panels_count < self.panels.len() {
            self.panels.truncate(panels_count);
            self.sizes.truncate(panels_count);
            if self
                .preserve_panel_size_ix
                .is_some_and(|ix| ix >= panels_count)
            {
                self.preserve_panel_size_ix = None;
                self.preferred_preserved_panel_size = None;
            }
            changed = true;
        }

        if changed {
            // We need to make sure the total size is in line with the container size.
            self.adjust_to_container_size(cx);
        }
    }

    /// Whether [`Self::update_panel_size`] with these arguments would change
    /// anything. A panel reports its bounds as it prepaints, every frame it
    /// is drawn; updating the state for the same bounds would mark it, and
    /// every view that reads it, changed on every frame.
    pub(crate) fn panel_size_changes(
        &self,
        panel_ix: usize,
        bounds: Bounds<Pixels>,
        size_range: &Range<Pixels>,
    ) -> bool {
        let Some(panel) = self.panels.get(panel_ix) else {
            return true;
        };
        self.sizes.get(panel_ix).map(|size| size.as_f32()) == Some(PANEL_MIN_SIZE.as_f32())
            || panel.bounds != bounds
            || panel.size_range != *size_range
    }

    /// 首次实际测量时为无显式初始值的固定面板建立恢复宽度。
    ///
    /// @author kongweiguang
    pub(crate) fn update_panel_size(
        &mut self,
        panel_ix: usize,
        bounds: Bounds<Pixels>,
        size_range: Range<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let size = bounds.size.along(self.axis);
        // This check is only necessary to stop the very first panel from resizing on its own
        // it needs to be passed when the panel is freshly created so we get the initial size,
        // but its also fine when it sometimes passes later.
        if self.sizes[panel_ix].as_f32() == PANEL_MIN_SIZE.as_f32() {
            self.sizes[panel_ix] = size;
            self.panels[panel_ix].size = Some(size);
            if self.preserve_panel_size_ix == Some(panel_ix)
                && self.preferred_preserved_panel_size.is_none()
            {
                self.preferred_preserved_panel_size = Some(size);
            }
        }
        self.panels[panel_ix].bounds = bounds;
        self.panels[panel_ix].size_range = size_range;
        cx.notify();
    }

    /// Remove the panel at `panel_ix` and redistribute the remaining space.
    /// 删除面板后清除或平移固定索引，首选宽度不应落到错误面板。
    ///
    /// @author kongweiguang
    pub fn remove_panel(&mut self, panel_ix: usize, cx: &mut Context<Self>) {
        self.panels.remove(panel_ix);
        self.sizes.remove(panel_ix);
        match self.preserve_panel_size_ix {
            Some(ix) if ix == panel_ix => {
                self.preserve_panel_size_ix = None;
                self.preferred_preserved_panel_size = None;
            }
            Some(ix) if ix > panel_ix => self.preserve_panel_size_ix = Some(ix - 1),
            _ => {}
        }
        if let Some(resizing_panel_ix) = self.resizing_panel_ix {
            if resizing_panel_ix > panel_ix {
                self.resizing_panel_ix = Some(resizing_panel_ix - 1);
            }
        }
        self.adjust_to_container_size(cx);
    }

    /// Reset the panel at `panel_ix` while preserving its current size.
    pub fn reset_panel(&mut self, panel_ix: usize, cx: &mut Context<Self>) {
        let old_size = self.sizes[panel_ix];

        self.panels[panel_ix] = ResizablePanelState::default();
        self.sizes[panel_ix] = old_size;
        self.adjust_to_container_size(cx);
    }

    /// Remove all panel state.
    /// 清空面板时清除对应首选值，重建后的面板从新布局初始化。
    ///
    /// @author kongweiguang
    pub fn clear(&mut self) {
        self.panels.clear();
        self.sizes.clear();
        self.preserve_panel_size_ix = None;
        self.preferred_preserved_panel_size = None;
    }

    #[inline]
    /// Return the measured size of the group along its resize axis.
    pub fn container_size(&self) -> Pixels {
        self.bounds.size.along(self.axis)
    }

    pub(crate) fn done_resizing(&mut self, cx: &mut Context<Self>) {
        self.resizing_panel_ix = None;
        cx.emit(ResizablePanelEvent::Resized);
    }

    fn panel_size_range(&self, ix: usize) -> Range<Pixels> {
        let Some(panel) = self.panels.get(ix) else {
            return PANEL_MIN_SIZE..Pixels::MAX;
        };

        panel.size_range.clone()
    }

    fn sync_real_panel_sizes(&mut self, _: &App) {
        for (i, panel) in self.panels.iter().enumerate() {
            self.sizes[i] = panel.bounds.size.along(self.axis);
        }
    }

    /// Resize the panel at `ix` by treating `ix` as the drag-handle position
    /// (the handle that sits between panel `ix` and panel `ix + 1`). Returns
    /// early on the last panel since there is no handle below it.
    ///
    /// This is the worker behind drag interactions and the public
    /// [`Self::resize_panel`] API.
    /// 使用同一夹紧与兄弟分配规则处理拖动；仅真实拖动改变固定面板首选值。
    ///
    /// 程序化回填可能来自折叠恢复，不应因暂时受限的有效宽度改写用户意图。
    ///
    /// @author kongweiguang
    fn resize_panel_at_handle(
        &mut self,
        ix: usize,
        size: Pixels,
        _: &mut Window,
        cx: &mut Context<Self>,
        update_preserved_preference: bool,
    ) {
        let old_sizes = self.sizes.clone();

        let mut ix = ix;
        // Only resize the left panels.
        if ix >= old_sizes.len() - 1 {
            return;
        }
        let container_size = self.container_size();
        self.sync_real_panel_sizes(cx);

        let move_changed = size - old_sizes[ix];
        if move_changed == px(0.) {
            return;
        }

        let size_range = self.panel_size_range(ix);
        let new_size = size.clamp(size_range.start, size_range.end);
        let is_expand = move_changed > px(0.);

        let main_ix = ix;
        let mut new_sizes = old_sizes.clone();

        if is_expand {
            let mut changed = new_size - old_sizes[ix];
            new_sizes[ix] = new_size;

            while changed > px(0.) && ix < old_sizes.len() - 1 {
                ix += 1;
                let size_range = self.panel_size_range(ix);
                let available_size = (new_sizes[ix] - size_range.start).max(px(0.));
                let to_reduce = changed.min(available_size);
                new_sizes[ix] -= to_reduce;
                changed -= to_reduce;
            }
        } else {
            let mut changed = new_size - size;
            new_sizes[ix] = new_size;

            while changed > px(0.) && ix > 0 {
                ix -= 1;
                let size_range = self.panel_size_range(ix);
                let available_size = (new_sizes[ix] - size_range.start).max(px(0.));
                let to_reduce = changed.min(available_size);
                changed -= to_reduce;
                new_sizes[ix] -= to_reduce;
            }

            new_sizes[main_ix + 1] += old_sizes[main_ix] - size - changed;
        }

        // If total size exceeds container size, adjust the main panel
        let total_size: Pixels = new_sizes.iter().map(|s| s.as_f32()).sum::<f32>().into();
        if total_size > container_size {
            let overflow = total_size - container_size;
            new_sizes[main_ix] = (new_sizes[main_ix] - overflow).max(size_range.start);
        }

        for (i, _) in old_sizes.iter().enumerate() {
            let size = new_sizes[i];
            self.panels[i].size = Some(size);
        }
        self.sizes = new_sizes;
        if update_preserved_preference {
            if let Some(fixed_ix) = self.preserve_panel_size_ix {
                self.preferred_preserved_panel_size = self.sizes.get(fixed_ix).copied();
            }
        }
        cx.notify();
    }

    /// Adjust panel sizes according to the container size.
    ///
    /// When the container size changes, the panels should take up the same percentage as they did before.
    /// 容器变化时应用可选像素首选策略；未 opt-in 时维持既有比例算法。
    ///
    /// @author kongweiguang
    fn adjust_to_container_size(&mut self, cx: &mut Context<Self>) {
        if self.preserve_panel_size_ix.is_some() && self.preferred_preserved_panel_size.is_some() {
            self.adjust_preserved_panel_to_container_size(cx);
            return;
        }
        if self.container_size().is_zero() {
            return;
        }

        // A panel with no size preference is laid out by flex, and its entry
        // in `sizes` is a placeholder until something measures it. Rescaling
        // by a ratio computed from that placeholder drags the panels that
        // *do* have a preference along with it: a 200px sidebar beside one
        // flexible panel comes back 587px wide on the frame after the first,
        // which reads as the layout jumping once for no reason. Flex already
        // fits the container, so there is nothing here to adjust.
        if self.panels.iter().any(|panel| panel.size.is_none()) {
            return;
        }

        let container_size = self.container_size();
        let total = self.sizes.iter().map(|s| s.as_f32()).sum::<f32>();
        if !total.is_finite() || total <= 0. {
            return;
        }
        let total_size = px(total);

        for i in 0..self.panels.len() {
            let size = self.sizes[i];
            let ratio = size / total_size;
            let new_size = container_size * ratio;

            self.sizes[i] = new_size;
            self.panels[i].size = Some(new_size);
        }
        cx.notify();
    }

    /// 设置固定面板索引；首选宽度与当前可用空间夹出的尺寸分开保存。
    ///
    /// @author kongweiguang
    fn set_preserved_panel(
        &mut self,
        panel_ix: Option<usize>,
        initial_size: Option<Pixels>,
    ) -> bool {
        let policy_changed = self.preserve_panel_size_ix != panel_ix;
        if policy_changed {
            self.preserve_panel_size_ix = panel_ix;
            self.preferred_preserved_panel_size = None;
        }
        let Some(panel_ix) = panel_ix else {
            return policy_changed;
        };
        if self.preferred_preserved_panel_size.is_some() {
            return policy_changed;
        }
        let measured_size = self
            .panels
            .get(panel_ix)
            .filter(|panel| panel.size.is_some())
            .and_then(|_| self.sizes.get(panel_ix).copied());
        self.preferred_preserved_panel_size = initial_size.or(measured_size);
        policy_changed || self.preferred_preserved_panel_size.is_some()
    }

    /// 固定目标面板，其余面板在自身范围内吸收容器尺寸变化。
    ///
    /// 所有面板最小值超过容器时维持各自最小尺寸并允许溢出，首选尺寸仍单独保留。
    ///
    /// @author kongweiguang
    fn adjust_preserved_panel_to_container_size(&mut self, cx: &mut Context<Self>) {
        let container = self.container_size().as_f32();
        let Some(fixed_ix) = self.preserve_panel_size_ix else {
            return;
        };
        let Some(preferred) = self.preferred_preserved_panel_size else {
            return;
        };
        if container <= 0. || fixed_ix >= self.panels.len() {
            return;
        }
        if self.panels.iter().any(|panel| panel.size.is_none()) {
            return;
        }

        let ranges: Vec<_> = (0..self.panels.len())
            .map(|ix| self.panel_size_range(ix))
            .collect();
        let min_total: f32 = ranges.iter().map(|range| range.start.as_f32()).sum();
        let mut next = vec![px(0.); self.panels.len()];

        if container < min_total {
            for (size, range) in next.iter_mut().zip(&ranges) {
                *size = range.start;
            }
        } else {
            let other_min: f32 = ranges
                .iter()
                .enumerate()
                .filter(|(ix, _)| *ix != fixed_ix)
                .map(|(_, range)| range.start.as_f32())
                .sum();
            let desired = preferred.as_f32().clamp(
                ranges[fixed_ix].start.as_f32(),
                ranges[fixed_ix].end.as_f32(),
            );
            let fixed = desired.min((container - other_min).max(ranges[fixed_ix].start.as_f32()));
            next[fixed_ix] = px(fixed);

            let flexible: Vec<usize> = (0..self.panels.len())
                .filter(|ix| *ix != fixed_ix)
                .collect();
            let sizes: Vec<_> = flexible.iter().map(|ix| self.sizes[*ix]).collect();
            let flexible_ranges: Vec<_> = flexible.iter().map(|ix| ranges[*ix].clone()).collect();
            let siblings =
                distribute_panel_sizes(&sizes, &flexible_ranges, px((container - fixed).max(0.)));
            for (ix, size) in flexible.into_iter().zip(siblings) {
                next[ix] = size;
            }
        }

        if next != self.sizes {
            for (panel, size) in self.panels.iter_mut().zip(next.iter().copied()) {
                panel.size = Some(size);
            }
            self.sizes = next;
            // 容器尺寸变化不是用户拖动，不产生 Resized 回执或 on_resize 回调。
            cx.notify();
        }
    }
}

/// 在非固定面板间维持相对尺寸，并迭代夹紧各面板的上下限。
///
/// @author kongweiguang
fn distribute_panel_sizes(
    current: &[Pixels],
    ranges: &[Range<Pixels>],
    requested: Pixels,
) -> Vec<Pixels> {
    let mut values: Vec<f32> = current
        .iter()
        .zip(ranges)
        .map(|(size, range)| {
            size.as_f32()
                .clamp(range.start.as_f32(), range.end.as_f32())
        })
        .collect();
    if values.is_empty() {
        return Vec::new();
    }
    let min_total: f32 = ranges.iter().map(|range| range.start.as_f32()).sum();
    let max_total: f32 = ranges.iter().map(|range| range.end.as_f32()).sum();
    let target = requested.as_f32().clamp(min_total, max_total);

    for _ in 0..=values.len() {
        let total: f32 = values.iter().sum();
        let difference = target - total;
        if difference.abs() < 0.01 {
            break;
        }
        if difference > 0. {
            let weight_total: f32 = values
                .iter()
                .enumerate()
                .filter(|(ix, value)| **value + 0.01 < ranges[*ix].end.as_f32())
                .map(|(_, value)| value.max(1.))
                .sum();
            if weight_total <= 0. {
                break;
            }
            for (ix, value) in values.iter_mut().enumerate() {
                let max = ranges[ix].end.as_f32();
                if *value + 0.01 >= max {
                    continue;
                }
                let share = difference * ((*value).max(1.) / weight_total);
                *value = (*value + share).min(max);
            }
        } else {
            let capacity_total: f32 = values
                .iter()
                .enumerate()
                .map(|(ix, value)| (*value - ranges[ix].start.as_f32()).max(0.))
                .sum();
            if capacity_total <= 0. {
                break;
            }
            let reduction = -difference;
            for (ix, value) in values.iter_mut().enumerate() {
                let capacity = (*value - ranges[ix].start.as_f32()).max(0.);
                if capacity <= 0. {
                    continue;
                }
                let share = reduction * (capacity / capacity_total);
                *value = (*value - share).max(ranges[ix].start.as_f32());
            }
        }
    }
    values.into_iter().map(px).collect()
}

impl EventEmitter<ResizablePanelEvent> for ResizableState {}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResizablePanelState {
    pub size: Option<Pixels>,
    pub size_range: Range<Pixels>,
    bounds: Bounds<Pixels>,
}

#[cfg(test)]
mod tests {
    #[path = "../../../tests/unit/resizable_preserved_panel_size.rs"]
    mod preserved_panel_size;
    #[path = "../../../tests/unit/resizable_single_move_drag.rs"]
    mod single_move_drag;

    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use gpui::{
        App, AppContext as _, Context, InteractiveElement as _, IntoElement, Modifiers,
        MouseButton, ParentElement as _, Pixels, Render, Styled as _, TestAppContext,
        VisualTestContext, Window, div, point, prelude::FluentBuilder as _, px, size,
    };

    use super::{
        ResizableState, ResizeHandleContext, ResizeHandleState, h_resizable, resizable_panel,
    };

    struct MixedSizingHarness {
        width: Pixels,
    }

    impl Render for MixedSizingHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(self.width).h(px(100.)).child(
                h_resizable("mixed-sizing")
                    .child(
                        resizable_panel()
                            .size(px(240.))
                            .child(div().size_full().debug_selector(|| "fixed-sidebar".into())),
                    )
                    .child(
                        resizable_panel().child(
                            div()
                                .size_full()
                                .debug_selector(|| "flexible-content".into()),
                        ),
                    ),
            )
        }
    }

    #[gpui::test]
    fn mixed_sizing_is_stable_between_resize_and_followup_frame(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| MixedSizingHarness { width: px(800.) });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
        });
        let before = cx.debug_bounds("fixed-sidebar").unwrap().size.width;

        view.update(cx, |view, cx| {
            view.width = px(1200.);
            cx.notify();
        });
        cx.run_until_parked();
        let settled_frame = cx.debug_bounds("fixed-sidebar").unwrap().size.width;
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let followup_frame = cx.debug_bounds("fixed-sidebar").unwrap().size.width;

        // Resizable panels preserve their proportional sizing across a
        // container resize; the important invariant is that applying the
        // state on the follow-up frame does not move the divider again.
        assert_ne!(settled_frame, before);
        assert_eq!(followup_frame, settled_frame);
    }

    struct CallerStateHarness {
        width: Pixels,
        state: gpui::Entity<ResizableState>,
    }

    impl Render for CallerStateHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(self.width).h(px(100.)).child(
                h_resizable("caller-state")
                    .with_state(&self.state)
                    .child(
                        resizable_panel()
                            .size(px(240.))
                            .child(div().size_full().debug_selector(|| "cs-sidebar".into())),
                    )
                    .child(resizable_panel().child(div().size_full())),
            )
        }
    }

    /// A group whose state the caller owns (`with_state`, as the dock does)
    /// has no `use_keyed_state` observer behind it, so the settling frame has
    /// to be scheduled by the deferred notify rather than by that observer.
    #[gpui::test]
    fn caller_owned_state_settles_on_the_same_frame(cx: &mut TestAppContext) {
        let state = cx.update(|cx| cx.new(|_| ResizableState::default()));
        let (view, cx) = cx.add_window_view({
            let state = state.clone();
            move |_, _| CallerStateHarness {
                width: px(800.),
                state,
            }
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
        });

        view.update(cx, |view, cx| {
            view.width = px(1200.);
            cx.notify();
        });
        cx.run_until_parked();
        let settled = cx.debug_bounds("cs-sidebar").unwrap().size.width;
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let followup = cx.debug_bounds("cs-sidebar").unwrap().size.width;

        assert_eq!(followup, settled, "settling frame must not be pending");
    }

    struct ResizableHarness {
        state: gpui::Entity<ResizableState>,
        resizes: Rc<Cell<usize>>,
    }

    impl Render for ResizableHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(400.)).h(px(100.)).child(
                h_resizable("resizable")
                    .with_state(&self.state)
                    .on_resize({
                        let resizes = self.resizes.clone();
                        move |_, _, _| resizes.set(resizes.get() + 1)
                    })
                    .child(
                        resizable_panel()
                            .size(px(150.))
                            .child(div().size_full().debug_selector(|| "first-panel".into())),
                    )
                    .child(
                        resizable_panel()
                            .size(px(250.))
                            .child(div().size_full().debug_selector(|| "second-panel".into())),
                    ),
            )
        }
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (
        &mut VisualTestContext,
        gpui::Entity<ResizableState>,
        Rc<Cell<usize>>,
    ) {
        let state = cx.update(|cx| cx.new(|_| ResizableState::default()));
        let resizes = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            let resizes = resizes.clone();
            move |_, _| ResizableHarness { state, resizes }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, state, resizes)
    }

    /// A view reading a [`ResizableState`], counting its renders.
    #[cfg(feature = "gpui-fast")]
    struct StateReader {
        state: gpui::Entity<ResizableState>,
        renders: Rc<Cell<usize>>,
    }

    #[cfg(feature = "gpui-fast")]
    impl Render for StateReader {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            let _ = self.state.read(cx).sizes.len();
            div()
        }
    }

    /// Panels beside a [`StateReader`] of their state.
    #[cfg(feature = "gpui-fast")]
    struct ReaderBesidePanels {
        state: gpui::Entity<ResizableState>,
        reader: gpui::Entity<StateReader>,
    }

    #[cfg(feature = "gpui-fast")]
    impl Render for ReaderBesidePanels {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(800.))
                .h(px(100.))
                .child(self.reader.clone())
                .child(
                    h_resizable("reader-panels")
                        .with_state(&self.state)
                        .child(resizable_panel().size(px(240.)).child(div().size_full()))
                        .child(resizable_panel().child(div().size_full())),
                )
        }
    }

    /// Drawing panels again at the bounds they had changes nothing in their
    /// state: a view reading it, beside them in a view drawn again, is not
    /// built again for it.
    #[cfg(feature = "gpui-fast")]
    #[gpui::test]
    fn redrawing_panels_where_they_were_leaves_their_readers_retained(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let state = cx.update(|cx| cx.new(|_| ResizableState::default()));
        let (view, cx) = cx.add_window_view({
            let state = state.clone();
            let renders = renders.clone();
            move |_, cx| ReaderBesidePanels {
                state: state.clone(),
                reader: cx.new(|_| StateReader { state, renders }),
            }
        });
        for _ in 0..3 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        let before = renders.get();
        for _ in 0..3 {
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        assert_eq!(renders.get(), before, "the reader rendered again");
    }

    #[gpui::test]
    fn dynamic_panel_lifecycle_is_owned_by_resizable_state(cx: &mut TestAppContext) {
        let state = cx.update(|cx| cx.new(|_| ResizableState::default()));

        cx.update(|cx| {
            state.update(cx, |state, cx| {
                state.bounds.size = size(px(400.), px(100.));
                state.panels.push(Default::default());
                state.sizes.push(px(400.));
                state.insert_panel(Some(px(200.)), None, cx);
                assert_eq!(state.sizes(), &vec![px(200.), px(200.)]);

                state.reset_panel(0, cx);
                assert_eq!(state.sizes(), &vec![px(200.), px(200.)]);

                state.remove_panel(0, cx);
                assert_eq!(state.sizes(), &vec![px(400.)]);

                state.clear();
                assert!(state.sizes().is_empty());
            });
        });
    }

    #[gpui::test]
    fn group_measures_panels_and_programmatic_resize_uses_drag_rules(cx: &mut TestAppContext) {
        let (cx, state, _) = harness(cx);
        let first = cx.debug_bounds("first-panel").unwrap();
        let second = cx.debug_bounds("second-panel").unwrap();
        assert_eq!(first.size.width + second.size.width, px(400.));

        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.resize_panel(0, px(220.), window, cx);
            });
            window.draw(cx).clear(cx);
        });

        state.read_with(cx, |state, _| {
            assert_eq!(state.sizes(), &vec![px(220.), px(180.)]);
        });
    }

    #[gpui::test]
    fn dragging_the_handle_resizes_and_emits_once(cx: &mut TestAppContext) {
        let (cx, state, resizes) = harness(cx);
        let boundary = cx.debug_bounds("second-panel").unwrap().left();

        cx.simulate_mouse_down(
            point(boundary - px(2.), px(50.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(boundary + px(10.), px(50.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(220.), px(50.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(220.), px(50.)),
            MouseButton::Left,
            Modifiers::default(),
        );

        state.read_with(cx, |state, _| {
            assert_eq!(state.sizes(), &vec![px(220.), px(180.)]);
        });
        assert_eq!(resizes.get(), 1);
    }

    /// Reports every state its divider is rendered in, so a drag can be watched
    /// from outside the handle.
    ///
    /// `covered` lays an occluding overlay over the whole group after it, the
    /// way a sheet's backdrop or a toast would sit over a dock.
    struct HandleStateHarness {
        seen: Rc<RefCell<Vec<ResizeHandleState>>>,
        covered: bool,
    }

    impl Render for HandleStateHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let seen = self.seen.clone();
            div()
                .relative()
                .w(px(400.))
                .h(px(100.))
                .child(
                    h_resizable("handle-state")
                        .with_handle_appearance(Rc::new(
                            move |handle: &ResizeHandleContext, _: &mut Window, _: &mut App| {
                                let mut seen = seen.borrow_mut();
                                if seen.last() != Some(&handle.state()) {
                                    seen.push(handle.state());
                                }
                                // Nothing painted: this renderer is here to watch.
                                None
                            },
                        ))
                        .child(resizable_panel().size(px(150.)).child(div().size_full()))
                        .child(
                            resizable_panel()
                                .size(px(250.))
                                .child(div().size_full().debug_selector(|| "hs-second".into())),
                        ),
                )
                .when(self.covered, |this| {
                    this.child(div().absolute().inset_0().occlude())
                })
        }
    }

    /// A handle under something else does not answer the pointer.
    ///
    /// The listeners used to test the pointer against the handle's bounds,
    /// which read true through anything painted over it: a divider under a
    /// sheet's backdrop lit up as the pointer crossed where it lay, and a
    /// press there counted as a press on the handle. They ask the hitbox now,
    /// and an occluding element in front of it answers for it.
    #[gpui::test]
    fn a_covered_handle_stays_idle(cx: &mut TestAppContext) {
        let seen: Rc<RefCell<Vec<ResizeHandleState>>> = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let seen = seen.clone();
            move |_, _| HandleStateHarness {
                seen,
                covered: true,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let boundary = cx.debug_bounds("hs-second").unwrap().left();
        let on_handle = point(boundary - px(2.), px(50.));
        let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));

        cx.simulate_mouse_move(on_handle, None, Modifiers::default());
        draw(cx);
        cx.simulate_mouse_down(on_handle, MouseButton::Left, Modifiers::default());
        draw(cx);
        cx.simulate_mouse_up(on_handle, MouseButton::Left, Modifiers::default());
        draw(cx);

        assert_eq!(*seen.borrow(), vec![ResizeHandleState::Idle]);
    }

    #[gpui::test]
    fn a_handle_reports_the_press_and_the_drag_to_its_renderer(cx: &mut TestAppContext) {
        let seen: Rc<RefCell<Vec<ResizeHandleState>>> = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let seen = seen.clone();
            move |_, _| HandleStateHarness {
                seen,
                covered: false,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let boundary = cx.debug_bounds("hs-second").unwrap().left();
        let on_handle = point(boundary - px(2.), px(50.));
        let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));

        cx.simulate_mouse_move(on_handle, None, Modifiers::default());
        draw(cx);
        cx.simulate_mouse_down(on_handle, MouseButton::Left, Modifiers::default());
        draw(cx);
        cx.simulate_mouse_move(
            point(px(260.), px(50.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        draw(cx);
        cx.simulate_mouse_up(
            point(px(390.), px(90.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(cx);

        // Every one of these frames used to render `Idle`: the listeners wrote
        // their progress into a copy of the handle's state, so no renderer ever
        // saw a press, and `is_active` never once read true.
        assert_eq!(
            *seen.borrow(),
            vec![
                ResizeHandleState::Idle,
                ResizeHandleState::Hovered,
                ResizeHandleState::Pressed,
                ResizeHandleState::Dragging,
                ResizeHandleState::Idle,
            ]
        );
    }

    struct SizedGroupHarness;

    impl Render for SizedGroupHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(400.))
                .h(px(100.))
                .child(
                    h_resizable("sized-resizable").size(px(40.)).child(
                        resizable_panel()
                            .child(div().size_full().debug_selector(|| "sized-panel".into())),
                    ),
                )
        }
    }

    #[gpui::test]
    fn a_group_size_binds_the_cross_axis(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| SizedGroupHarness);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let panel = cx.debug_bounds("sized-panel").unwrap();
        assert_eq!(panel.size.width, px(400.));
        assert_eq!(panel.size.height, px(40.));
    }
}
