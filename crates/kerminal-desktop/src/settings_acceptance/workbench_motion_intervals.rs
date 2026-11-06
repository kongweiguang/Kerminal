//! 工作台动效验收的原始呈现时间线与可证明的保守区间。
//!
//! @author kongweiguang

use std::time::{Duration, Instant};

use gpui_kit::{
    App, Window, WindowId,
    profiler::{
        PresentTiming,
        journal::{
            ForegroundEvent, ForegroundJournalCollector, ForegroundJournalEntry, IntervalBoundary,
        },
    },
};
use serde::Serialize;

pub(super) const MOTION_DURATION: Duration = Duration::from_millis(140);

/// 观察视口和系统偏好，避免将布局变化或禁用动效误认作侧栏动画。
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MotionEnvironment {
    viewport_width: f32,
    viewport_height: f32,
    scale_factor: f32,
    gpui_reduce_motion: bool,
    system_allows_motion: Option<bool>,
}

impl MotionEnvironment {
    /// 只读现有窗口和系统偏好；无法读取系统配置时保留未知值而非假定动画开启。
    ///
    /// @author kongweiguang
    pub(super) fn capture(window: &Window, app: &App) -> Self {
        let viewport = window.viewport_size();
        Self {
            viewport_width: viewport.width.as_f32(),
            viewport_height: viewport.height.as_f32(),
            scale_factor: window.scale_factor(),
            gpui_reduce_motion: app.reduce_motion(),
            system_allows_motion: system_allows_motion(),
        }
    }

    /// 仅当观察到的窗口环境与基线一致且动效明确开启，才允许归属侧栏动画样本。
    ///
    /// @author kongweiguang
    fn supports_motion(self, baseline: Self) -> bool {
        self.viewport_width == baseline.viewport_width
            && self.viewport_height == baseline.viewport_height
            && self.scale_factor == baseline.scale_factor
            && !self.gpui_reduce_motion
            && self.system_allows_motion == Some(true)
    }
}

/// 测量 Windows 的同一系统偏好；此调用只读取一个 BOOL，不更改用户设置。
///
/// @author kongweiguang
#[cfg(windows)]
fn system_allows_motion() -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };

    let mut enabled = 0_i32;
    // SAFETY: SPI_GETCLIENTAREAANIMATION 只向有效的 BOOL 槽写入结果，不保留指针。
    let success = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        )
    };
    (success != 0).then_some(enabled != 0)
}

/// 没有平台偏好证据时保留未知状态，不把其他平台当作已启用。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn system_allows_motion() -> Option<bool> {
    None
}

/// 记录一次真实点击及目标渲染树的首次只读观察，不要求或制造新的帧。
pub(super) struct ToggleObservation {
    requested_at: Instant,
    expected_visible: bool,
    before: MotionEnvironment,
    observed_at: Option<Instant>,
    observed: Option<MotionEnvironment>,
    settled_at: Option<Instant>,
    settled: Option<MotionEnvironment>,
}

impl ToggleObservation {
    /// 点击时刻早于目标状态进入渲染树，用作真实动画开始时间的下界。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        requested_at: Instant,
        expected_visible: bool,
        before: MotionEnvironment,
    ) -> Self {
        Self {
            requested_at,
            expected_visible,
            before,
            observed_at: None,
            observed: None,
            settled_at: None,
            settled: None,
        }
    }

    /// 首次目标帧观察时刻是动画起点的保守上界，重复观察不会缩短已有证据。
    ///
    /// @author kongweiguang
    pub(super) fn observe(&mut self, at: Instant, environment: MotionEnvironment) {
        if self.observed_at.is_none() {
            self.observed_at = Some(at);
            self.observed = Some(environment);
        }
    }

    /// 记录等待结束时的环境，供本次动画与下一次点击之间的边界核验。
    ///
    /// @author kongweiguang
    pub(super) fn settle(&mut self, at: Instant, environment: MotionEnvironment) {
        self.settled_at = Some(at);
        self.settled = Some(environment);
    }

    /// 观察成功后不再继续短轮询，减少对被测窗口前台线程的扰动。
    ///
    /// @author kongweiguang
    pub(super) fn observed(&self) -> bool {
        self.observed_at.is_some()
    }
}

/// 保留原始呈现时间，而不是只保留筛选后的分布。
struct RawPresent {
    timing: PresentTiming,
    draw_start: Option<Instant>,
    draw_end: Option<Instant>,
}

/// 独立游标只读取本轮采样，不改变其他验收采集器或平台调度。
pub(super) struct MotionTimeline {
    collector: ForegroundJournalCollector,
    window_id: WindowId,
    origin: Instant,
    lost: u64,
    power_interrupted: bool,
    inconsistent_window_id: bool,
    last_drawn_present: Option<(Instant, Instant)>,
    presents: Vec<RawPresent>,
}

impl MotionTimeline {
    /// 在帧和输入基线所在的窗口更新中创建游标，避免把预热结果混入本轮。
    ///
    /// @author kongweiguang
    pub(super) fn new(window: &Window, app: &App, origin: Instant) -> Self {
        Self {
            collector: app.foreground_journal().collector(),
            window_id: window.window_handle().window_id(),
            origin,
            lost: 0,
            power_interrupted: false,
            inconsistent_window_id: false,
            last_drawn_present: None,
            presents: Vec::new(),
        }
    }

    /// 分段排空有界 journal；有丢失或电源中断时保留其证据并拒绝完整性结论。
    ///
    /// @author kongweiguang
    pub(super) fn drain(&mut self) {
        let drained = self.collector.collect_unseen();
        self.lost = self.lost.saturating_add(drained.lost);
        for entry in drained.entries {
            match entry {
                ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(presented))
                    if presented.frame.window_id == self.window_id
                        || presented.presentation.window_id == self.window_id =>
                {
                    if presented.frame.window_id != self.window_id
                        || presented.presentation.window_id != self.window_id
                    {
                        self.inconsistent_window_id = true;
                    } else {
                        self.last_drawn_present = Some((
                            presented.frame.draw_start,
                            presented.presentation.present_end,
                        ));
                    }
                    self.presents.push(RawPresent {
                        timing: presented.presentation,
                        draw_start: Some(presented.frame.draw_start),
                        draw_end: Some(presented.frame.draw_end),
                    });
                }
                ForegroundJournalEntry::Event(ForegroundEvent::Present(timing))
                    if timing.window_id == self.window_id =>
                {
                    self.presents.push(RawPresent {
                        timing,
                        draw_start: None,
                        draw_end: None,
                    });
                }
                ForegroundJournalEntry::Boundary(IntervalBoundary::PowerTransition { .. }) => {
                    self.power_interrupted = true;
                }
                _ => {}
            }
        }
    }

    /// 已见到点击后的新绘制呈现，才能用当前渲染树为本次目标状态提供上界。
    ///
    /// @author kongweiguang
    pub(super) fn has_drawn_present_since(&self, requested_at: Instant) -> bool {
        self.last_drawn_present
            .is_some_and(|(draw_start, present_end)| {
                draw_start >= requested_at && present_end >= draw_start
            })
    }

    /// 同时输出原始时间线和保守样本；任何缺失证据都只能得到 indeterminate。
    ///
    /// @author kongweiguang
    pub(super) fn finish(
        self,
        toggles: Vec<ToggleObservation>,
        baseline: MotionEnvironment,
        expected_toggles: usize,
        session_and_sidebar_preserved: bool,
        profiler_interval_count: u64,
        minimum_samples: u64,
        target_p95_ns: Option<u64>,
    ) -> MotionIntervalReport {
        let mut reasons = Vec::new();
        if self.lost != 0 {
            reasons.push("journal_entries_lost");
        }
        if self.power_interrupted {
            reasons.push("power_interrupted");
        }
        if self.inconsistent_window_id {
            reasons.push("inconsistent_window_id");
        }
        if toggles.len() != expected_toggles {
            reasons.push("incomplete_toggles");
        }
        if !session_and_sidebar_preserved {
            reasons.push("session_or_sidebar_state_changed");
        }
        if toggles
            .windows(2)
            .any(|pair| pair[0].expected_visible == pair[1].expected_visible)
        {
            reasons.push("toggle_sequence_not_alternating");
        }
        if !baseline.supports_motion(baseline) {
            reasons.push("baseline_motion_or_viewport_unverified");
        }

        let mut bounds = Vec::with_capacity(toggles.len());
        for (index, toggle) in toggles.iter().enumerate() {
            let deadline = toggle.requested_at.checked_add(MOTION_DURATION);
            let next_request = toggles.get(index + 1).map(|next| next.requested_at);
            let valid = match (
                toggle.observed_at,
                toggle.observed,
                toggle.settled_at,
                toggle.settled,
                deadline,
            ) {
                (
                    Some(observed_at),
                    Some(observed),
                    Some(settled_at),
                    Some(settled),
                    Some(deadline),
                ) => {
                    let nonoverlap_end = next_request.unwrap_or(settled_at);
                    toggle.before.supports_motion(baseline)
                        && observed.supports_motion(baseline)
                        && settled.supports_motion(baseline)
                        && toggle.requested_at <= observed_at
                        && observed_at < deadline
                        && observed_at <= settled_at
                        && observed_at
                            .checked_add(MOTION_DURATION)
                            .is_some_and(|end| end <= nonoverlap_end)
                        && settled_at <= nonoverlap_end
                }
                _ => false,
            };
            if !valid {
                reasons.push("toggle_window_unverified");
            }
            bounds.push((toggle.observed_at, deadline, valid));
        }

        let mut eligible = Vec::new();
        let mut eligible_per_toggle = vec![0_u64; toggles.len()];
        let mut raw_animation_count = 0_u64;
        let mut timestamp_unrepresentable = false;
        let mut raw_timeline = Vec::with_capacity(self.presents.len());
        for present in self.presents {
            let end = present.timing.present_end;
            let interval = present.timing.animation_interval;
            let start = interval.and_then(|duration| end.checked_sub(duration));
            if interval.is_some() {
                raw_animation_count = raw_animation_count.saturating_add(1);
            }
            let assigned = start.and_then(|start| {
                bounds.iter().position(|(observed, deadline, valid)| {
                    *valid
                        && observed.is_some_and(|at| start >= at)
                        && deadline.is_some_and(|at| end <= at)
                })
            });
            let duration_ns = interval.and_then(duration_ns);
            if let (Some(index), Some(duration)) = (assigned, duration_ns) {
                eligible.push(duration);
                eligible_per_toggle[index] = eligible_per_toggle[index].saturating_add(1);
            }
            let raw = RawPresentation {
                draw_start_ns: present.draw_start.and_then(|at| offset_ns(self.origin, at)),
                draw_end_ns: present.draw_end.and_then(|at| offset_ns(self.origin, at)),
                present_start_ns: offset_ns(self.origin, present.timing.present_start),
                present_end_ns: offset_ns(self.origin, end),
                interval_start_ns: start.and_then(|at| offset_ns(self.origin, at)),
                animation_interval_ns: duration_ns,
                eligible_toggle: assigned.map(|index| index + 1),
            };
            timestamp_unrepresentable |= raw.present_start_ns.is_none()
                || raw.present_end_ns.is_none()
                || (present.draw_start.is_some() && raw.draw_start_ns.is_none())
                || (present.draw_end.is_some() && raw.draw_end_ns.is_none())
                || (interval.is_some()
                    && (raw.interval_start_ns.is_none() || duration_ns.is_none()));
            raw_timeline.push(raw);
        }
        if timestamp_unrepresentable {
            reasons.push("timestamp_unrepresentable");
        }
        if raw_animation_count != profiler_interval_count {
            reasons.push("journal_profiler_interval_count_mismatch");
        }
        if eligible_per_toggle.contains(&0) {
            reasons.push("toggle_without_proven_interval");
        }
        if u64::try_from(eligible.len()).unwrap_or(u64::MAX) < minimum_samples {
            reasons.push("insufficient_proven_intervals");
        }
        if target_p95_ns.is_none() {
            reasons.push("unknown_refresh_interval");
        }
        reasons.sort_unstable();
        reasons.dedup();
        eligible.sort_unstable();
        let p95_ns = nearest_rank_p95(&eligible);
        let status = if !reasons.is_empty() || p95_ns.is_none() {
            "indeterminate"
        } else if p95_ns > target_p95_ns {
            "target_missed"
        } else {
            "passed"
        };
        let toggles = toggles
            .into_iter()
            .zip(eligible_per_toggle)
            .enumerate()
            .map(|(index, (toggle, eligible_count))| ToggleReport {
                index: index + 1,
                expected_visible: toggle.expected_visible,
                requested_ns: offset_ns(self.origin, toggle.requested_at),
                observed_ns: toggle.observed_at.and_then(|at| offset_ns(self.origin, at)),
                conservative_end_ns: toggle
                    .requested_at
                    .checked_add(MOTION_DURATION)
                    .and_then(|at| offset_ns(self.origin, at)),
                settled_ns: toggle.settled_at.and_then(|at| offset_ns(self.origin, at)),
                before: toggle.before,
                observed: toggle.observed,
                settled: toggle.settled,
                interval_window_verified: bounds[index].2,
                eligible_interval_count: eligible_count,
            })
            .collect();
        MotionIntervalReport {
            method: "same-toggle-conservative-window-from-click-and-observed-target-frame",
            timing_origin: "measurement-baseline",
            quantile_method: "nearest-rank",
            environment_observation: "baseline-before-observed-and-settled",
            observation_poll_interval_ms: super::TARGET_OBSERVATION_INTERVAL.as_millis() as u64,
            baseline_environment: baseline,
            window_id: format!("{:?}", self.window_id),
            duration_ms: MOTION_DURATION.as_millis() as u64,
            status,
            indeterminate_reasons: reasons,
            journal_entries_lost: self.lost,
            power_interrupted: self.power_interrupted,
            profiler_animation_interval_count: profiler_interval_count,
            raw_animation_interval_count: raw_animation_count,
            eligible_interval_count: u64::try_from(eligible.len()).unwrap_or(u64::MAX),
            minimum_interval_samples: minimum_samples,
            target_p95_ns,
            p95_ns,
            toggles,
            raw_timeline,
        }
    }
}

/// 所有时间均为相对同一单调基线的纳秒；负值表示前一呈现早于基线。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawPresentation {
    draw_start_ns: Option<i64>,
    draw_end_ns: Option<i64>,
    present_start_ns: Option<i64>,
    present_end_ns: Option<i64>,
    interval_start_ns: Option<i64>,
    animation_interval_ns: Option<u64>,
    eligible_toggle: Option<usize>,
}

/// 同时保留请求、观察和稳定后的环境，便于解释被排除的区间。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToggleReport {
    index: usize,
    expected_visible: bool,
    requested_ns: Option<i64>,
    observed_ns: Option<i64>,
    conservative_end_ns: Option<i64>,
    settled_ns: Option<i64>,
    before: MotionEnvironment,
    observed: Option<MotionEnvironment>,
    settled: Option<MotionEnvironment>,
    interval_window_verified: bool,
    eligible_interval_count: u64,
}

/// 不覆盖原始 profiler 状态；保守子区间的状态独立报告。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MotionIntervalReport {
    method: &'static str,
    timing_origin: &'static str,
    quantile_method: &'static str,
    environment_observation: &'static str,
    observation_poll_interval_ms: u64,
    baseline_environment: MotionEnvironment,
    window_id: String,
    duration_ms: u64,
    status: &'static str,
    indeterminate_reasons: Vec<&'static str>,
    journal_entries_lost: u64,
    power_interrupted: bool,
    profiler_animation_interval_count: u64,
    raw_animation_interval_count: u64,
    eligible_interval_count: u64,
    minimum_interval_samples: u64,
    target_p95_ns: Option<u64>,
    p95_ns: Option<u64>,
    toggles: Vec<ToggleReport>,
    raw_timeline: Vec<RawPresentation>,
}

/// 保留基线前时间的负偏移；不可表示时返回未知而不是截断。
///
/// @author kongweiguang
fn offset_ns(origin: Instant, at: Instant) -> Option<i64> {
    if let Some(after) = at.checked_duration_since(origin) {
        i64::try_from(after.as_nanos()).ok()
    } else {
        i64::try_from(origin.duration_since(at).as_nanos())
            .ok()
            .and_then(i64::checked_neg)
    }
}

/// 时间跨度超出报告整数范围时标记未知，不能把饱和值当作原始测量。
///
/// @author kongweiguang
fn duration_ns(duration: Duration) -> Option<u64> {
    u64::try_from(duration.as_nanos()).ok()
}

/// 精确的 nearest-rank 分位数基于全部保留样本，不对各次切换的分位数再次平均。
///
/// @author kongweiguang
fn nearest_rank_p95(sorted: &[u64]) -> Option<u64> {
    let rank = sorted.len().checked_mul(95)?.checked_add(99)? / 100;
    sorted.get(rank.checked_sub(1)?).copied()
}
