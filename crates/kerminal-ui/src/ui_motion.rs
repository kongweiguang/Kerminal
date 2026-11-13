//! 右栏数据区的局部短入场：只把已接纳的数据层从 94% 不透明度收敛到完整，
//! 不携带业务状态、不改变布局或命中区域。
//!
//! @author kongweiguang

use std::time::Instant;

use crate::theme::UI_TRANSITION_DURATION as TRANSITION_DURATION;

/// 固定起点保留正文的可读轮廓，避免目标切换时数据区整体闪没。
pub(crate) const DATA_ENTRANCE_OPACITY: f32 = 0.94;

/// 目标切换只登记代次、冻结当前采样并中断旧动画，首个成功结果才启动一次入场。
///
/// 这样普通刷新、持续更新和迟到的旧请求都不会重放动画；目标不可用时直接落位，
/// 不持有列表内容或任何业务缓存，视觉状态也不能反向决定数据是否展示。
#[derive(Clone, Copy, Debug)]
pub(crate) struct TargetDataEntrance {
    epoch: u64,
    from: f32,
    pending_from: f32,
    started_at: Option<Instant>,
    armed: bool,
    initialized: bool,
    allow_motion: bool,
}

impl Default for TargetDataEntrance {
    /// 未观察目标前保持完整画面；初始挂载不依赖一次额外帧才落位。
    fn default() -> Self {
        Self {
            epoch: 0,
            from: 1.0,
            pending_from: DATA_ENTRANCE_OPACITY,
            started_at: None,
            armed: false,
            initialized: false,
            allow_motion: false,
        }
    }
}

impl TargetDataEntrance {
    /// 首次有效结果作为初始挂载基线，启动期间的目标重绑不产生入场闪动；
    /// 之后只有目标身份变化才进入等待，数据未就绪时维持原有画面。
    ///
    /// 目标变化会立即中断在途动画，并把当时的采样留给新目标的首次结果；
    /// 加载占位不属于数据入场，不能继续淡入。
    pub(crate) fn target_changed(
        &mut self,
        has_target: bool,
        allow_motion: bool,
        now: Instant,
    ) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        self.allow_motion = allow_motion;
        if self.initialized && has_target {
            self.pending_from = if self.is_running(now) {
                self.opacity(now)
            } else {
                DATA_ENTRANCE_OPACITY
            };
            self.armed = true;
        } else {
            self.pending_from = DATA_ENTRANCE_OPACITY;
            self.armed = false;
        }
        self.settle();
        self.epoch
    }

    /// 只接纳当前目标代次的首个成功结果；旧请求即使迟到也不能消费新的入场机会。
    ///
    /// 当前代次的首个结果若尚无基线，只把它登记为初始挂载，不播放动画；
    /// 这里刻意不把键控 transition 当作入场：它首次采用目标值而不会自动播放，
    /// 显式的代次与 armed 才能把“数据首次就绪”与普通刷新区分开。状态只保留
    /// 一个代次和一次采样，不复制列表内容，也不形成无界缓存。
    ///
    /// `animate` 由调用方按面板可见性和局部动效偏好组合；目标变化时的缓存偏好
    /// 与就绪时的当前系统检查都通过才播放，Windows SPI 读取失败时保守落位。
    pub(crate) fn ready(&mut self, epoch: u64, now: Instant, animate: bool) -> bool {
        if epoch != self.epoch {
            return false;
        }
        if !self.armed {
            self.initialized = true;
            return false;
        }
        self.armed = false;
        if !animate || !self.allow_motion {
            self.settle();
            return false;
        }
        self.from = self.pending_from.clamp(DATA_ENTRANCE_OPACITY, 1.0);
        self.started_at = Some(now);
        true
    }

    /// 隐藏或减少动效时立即落到完整画面，但保留尚未消费的目标等待；
    /// 面板在数据就绪前被隐藏，重新显示后的首个有效结果仍可获得一次入场。
    pub(crate) fn settle(&mut self) {
        self.from = 1.0;
        self.started_at = None;
    }

    /// 请求发起时捕获的代次；目标变化后旧代次不能再触发动画。
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 读取目标变化时缓存的工作台动效偏好，避免逐帧访问系统设置。
    pub(crate) fn allows_motion(&self) -> bool {
        self.allow_motion
    }

    /// 是否正在等待某个新目标的首个有效结果；调用方只在这个窗口重查系统偏好。
    pub(crate) fn awaiting_result(&self) -> bool {
        self.armed
    }

    /// 与工作台相同的三次缓出；只改变透明度，不参与布局测量。
    pub(crate) fn opacity(&self, now: Instant) -> f32 {
        let Some(started_at) = self.started_at else {
            return self.from;
        };
        let elapsed = now.saturating_duration_since(started_at).as_secs_f32();
        let linear = (elapsed / TRANSITION_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - linear).powi(3);
        self.from + (1.0 - self.from) * eased
    }

    /// 完成后不再请求帧；隐藏与目标不可用也会让此条件立即为假。
    pub(crate) fn is_running(&self, now: Instant) -> bool {
        self.started_at
            .is_some_and(|started| now.saturating_duration_since(started) < TRANSITION_DURATION)
    }
}

/// 右栏与工作台共用同一系统动效偏好，GPUI 的 reduce_motion 始终优先。
pub(crate) fn motion_allowed(reduce_motion: bool) -> bool {
    !reduce_motion && system_allows_motion()
}

/// Windows 的系统动效偏好在目标变化和必要数据就绪时读取，不在逐帧刷新时查询；
/// 读取失败保守关闭动效，避免接口异常带来不可预期的动画。
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

/// 非 Windows 不额外探测系统偏好，直接返回 true，由调用方继续服从 GPUI 的
/// reduce_motion 优先门禁；gpui-base 在 macOS 启动时读取该偏好，运行期切换
/// 系统开关可能要到重启后才更新，Linux 则可跟随 portal。
#[cfg(not(target_os = "windows"))]
fn system_allows_motion() -> bool {
    true
}

/// 测试固定起点、迟到结果、快速切换和隐藏后停止帧请求的纯状态契约。
#[cfg(test)]
#[path = "../tests/unit/ui_motion.rs"]
mod tests;
