//! 工作台短过渡只改变绘制透明度，真实面板与终端布局仍使用最终状态。
//!
//! @author kongweiguang

use std::time::Instant;

use gpui_kit::{Context, IntoElement, Render, Window, div, prelude::*, px};

use crate::theme::UI_TRANSITION_DURATION as TRANSITION_DURATION;

/// 仅保存布局已决定的可见性，动画进度不能反向影响空间预算或面板归属。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MotionTargets {
    pub(super) sidebar: bool,
    pub(super) dock: bool,
    pub(super) host_drawer: bool,
    pub(super) tool_drawer: bool,
    pub(super) compact_width: bool,
    pub(super) compact_panels: bool,
}

/// 视觉进度可以中途反向，但不持有面板、焦点句柄或可交互内容。
#[derive(Clone, Copy, Debug)]
struct Transition {
    visible: bool,
    from: f32,
    started_at: Option<Instant>,
}

impl Transition {
    /// 初始状态直接落到实际面板，避免应用启动或首次布局时闪现。
    fn settled(visible: bool) -> Self {
        Self {
            visible,
            from: if visible { 1.0 } else { 0.0 },
            started_at: None,
        }
    }

    /// 用当前采样值反向，避免快速连续开关从旧端点重新开始。
    fn set_visible(&mut self, visible: bool, now: Instant, animate: bool) {
        if !animate {
            *self = Self::settled(visible);
        } else if self.visible != visible {
            self.from = self.value(now);
            self.visible = visible;
            self.started_at = Some(now);
        }
    }

    /// 平滑曲线只影响绘制；进度绝不作为可拖动尺寸或终端网格的输入。
    fn value(self, now: Instant) -> f32 {
        let target = if self.visible { 1.0 } else { 0.0 };
        let Some(started_at) = self.started_at else {
            return target;
        };
        let elapsed = now.saturating_duration_since(started_at).as_secs_f32();
        let linear = (elapsed / TRANSITION_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - linear).powi(3);
        self.from + (target - self.from) * eased
    }

    /// 完成后不再请求帧；系统关闭动画时也不会留下计时器。
    fn is_running(self, now: Instant) -> bool {
        self.started_at
            .is_some_and(|started| now.saturating_duration_since(started) < TRANSITION_DURATION)
    }
}

/// 每个通道只返回透明度，调用者使用实际可见性决定真实面板是否挂载。
#[derive(Clone, Copy, Debug)]
pub(super) struct MotionFrame {
    pub(super) sidebar: f32,
    pub(super) dock: f32,
    pub(super) host_drawer: f32,
    pub(super) tool_drawer: f32,
}

/// 逐帧通知归属于这个轻量 Entity，而不是把动画状态写回工作台布局。
#[derive(Default)]
pub(super) struct WorkbenchMotion {
    targets: Option<MotionTargets>,
    transitions: Option<[Transition; 4]>,
    allow_motion: bool,
}

impl WorkbenchMotion {
    /// 只在可见目标变化时读取 Windows 偏好，避免在连续帧中同步访问系统设置。
    pub(super) fn update_targets(
        &mut self,
        targets: MotionTargets,
        reduce_motion: bool,
    ) -> MotionFrame {
        if self.targets != Some(targets) {
            self.allow_motion = !reduce_motion && system_allows_motion();
        }
        self.update_at(targets, Instant::now(), self.allow_motion && !reduce_motion)
    }

    /// 响应式断点切换属于窗口布局变化，直接落到结果而不在拖动窗口时制造额外过渡。
    fn update_at(
        &mut self,
        targets: MotionTargets,
        now: Instant,
        allow_motion: bool,
    ) -> MotionFrame {
        let values = [
            targets.sidebar,
            targets.dock,
            targets.host_drawer,
            targets.tool_drawer,
        ];
        let responsive_change = self.targets.is_some_and(|previous| {
            previous.compact_width != targets.compact_width
                || previous.compact_panels != targets.compact_panels
        });
        let transitions = self
            .transitions
            .get_or_insert_with(|| values.map(Transition::settled));
        for (transition, visible) in transitions.iter_mut().zip(values) {
            transition.set_visible(visible, now, allow_motion && !responsive_change);
        }
        self.targets = Some(targets);
        MotionFrame {
            sidebar: transitions[0].value(now),
            dock: transitions[1].value(now),
            host_drawer: transitions[2].value(now),
            tool_drawer: transitions[3].value(now),
        }
    }
}

impl Render for WorkbenchMotion {
    /// 单独请求短暂的视觉帧；该节点不绘制内容，也不创建命中区域或改变布局。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !cx.reduce_motion()
            && self.transitions.is_some_and(|transitions| {
                transitions
                    .into_iter()
                    .any(|state| state.is_running(Instant::now()))
            })
        {
            window.request_animation_frame();
        }
        div().absolute().top_0().left_0().w(px(0.)).h(px(0.))
    }
}

/// Windows 未能读取偏好时保守地关闭动效；该读取只在目标变化时发生。
#[cfg(target_os = "windows")]
fn system_allows_motion() -> bool {
    const SPI_GETCLIENTAREAANIMATION: u32 = 4162;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(
            action: u32,
            parameter: u32,
            value: *mut core::ffi::c_void,
            flags: u32,
        ) -> i32;
    }
    let mut enabled: i32 = 0;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to this valid pointer and retains no reference.
    let success = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        )
    };
    success != 0 && enabled != 0
}

/// 其它平台继续服从 GPUI 的 reduce_motion 状态，等待平台提供系统偏好映射。
#[cfg(not(target_os = "windows"))]
fn system_allows_motion() -> bool {
    true
}

#[cfg(test)]
#[path = "../../tests/unit/workbench_motion.rs"]
mod tests;
