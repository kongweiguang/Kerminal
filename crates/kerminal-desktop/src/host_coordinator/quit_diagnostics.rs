//! 验收构建的有界退出采样；只读取编译期任务位置与持续时间，不读取业务状态或终端正文。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui_pre::profiler::{ActiveTiming, GuardedTaskTimings, THREAD_TIMINGS};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
static MAIN_THREAD_TIMINGS: OnceLock<Arc<GuardedTaskTimings>> = OnceLock::new();
static SAMPLING_STARTED: AtomicBool = AtomicBool::new(false);

/// 从真实 GPUI 入口捕获其线程局部 profiler，避免后台枚举误认线程或持有全局 registry 锁。
///
/// @author kongweiguang
pub(crate) fn register_main_thread() {
    let timings = THREAD_TIMINGS.with(|timings| Arc::clone(timings));
    let _ = MAIN_THREAD_TIMINGS.set(timings);
}

/// 仅在首次成功 Quit 回执后分离一个有限后台任务；正常退出不 join，runtime 停止即可丢弃它。
/// 两次各相隔一秒，既能看到不让出的一轮 poll，也不会创建长期监控或自唤醒日志循环。
///
/// @author kongweiguang
pub(crate) fn sample_after_successful_quit() {
    if MAIN_THREAD_TIMINGS.get().is_none() || SAMPLING_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    drop(tokio::spawn(async {
        let mut previous = None;
        for sequence in 1..=2 {
            tokio::time::sleep(SAMPLE_INTERVAL).await;
            previous = sample_main_thread(sequence, previous);
        }
    }));
}

/// 只尝试一次非阻塞锁并立即复制 running 元数据；空闲与锁忙均单独编码，不冒充正在卡住的任务。
/// phase 为 0=没有活动 poll、1=活动 poll、2=profiler 锁忙；只输出编译期 basename，不输出完整路径。
///
/// @author kongweiguang
fn sample_main_thread(sequence: u64, previous: Option<ActiveTiming>) -> Option<ActiveTiming> {
    let timings = MAIN_THREAD_TIMINGS.get()?;
    let Some(guard) = timings.try_lock() else {
        tracing::info!(event = "DESKTOP_QUIT_MAIN_TASK_SAMPLE", sequence, phase = 2);
        return None;
    };
    let running = guard.running;
    drop(guard);
    let Some(running) = running else {
        tracing::info!(event = "DESKTOP_QUIT_MAIN_TASK_SAMPLE", sequence, phase = 0);
        return None;
    };
    tracing::info!(
        event = "DESKTOP_QUIT_MAIN_TASK_SAMPLE",
        sequence,
        phase = 1,
        task_source = source_basename(running.location.file()),
        source_line = u64::from(running.location.line()),
        duration_ms = running.start.elapsed().as_millis(),
        same_poll = u64::from(is_same_poll(previous, &running)),
    );
    Some(running)
}

/// 精确比较 poll 起点和静态位置，避免把同一 spawn 行的后续短 poll 误判为一直不返回。
///
/// @author kongweiguang
fn is_same_poll(previous: Option<ActiveTiming>, current: &ActiveTiming) -> bool {
    previous.is_some_and(|previous| {
        previous.start == current.start && previous.location == current.location
    })
}

/// 编译器 Location 的路径可能来自任意构建平台；同时剥离两种分隔符，绝不向日志暴露构建根路径。
///
/// @author kongweiguang
fn source_basename(source: &'static str) -> &'static str {
    source.rsplit(['/', '\\']).next().unwrap_or("unknown.rs")
}

#[cfg(test)]
#[path = "../../tests/support/quit_diagnostics_unit.rs"]
mod tests;
