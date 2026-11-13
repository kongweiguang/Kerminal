//! 验收构建下通过正式插件服务和工作区视图驱动性能负载。
//! @author kongweiguang

use gpui_kit::{AnyWindowHandle, App, AsyncApp, Entity, Window, component::ActiveTheme as _};
use kerminal_application::{AppError, AppResult, plugin_runtime::PluginRuntimeService};
use kerminal_domain::plugins::{PluginBootPhase, PluginId};
use kerminal_ui::{AppServices, QuitIntent, RootView};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type FixtureTelemetry = Arc<Mutex<Value>>;

/// 共同列表固定为 515 个采样槽；计划间隔由实际采样预算推导，并由脚本核对两组一致。
const HOST_LIST_SCHEDULE_SLOTS: u64 = 515;
/// 低于此间隔时，单次真实滚动等待就会挤占下一槽，无法形成可执行的固定节奏。
const HOST_LIST_MIN_INTERVAL_MS: u64 = 55;
/// 允许事件循环和可视范围读取的少量抖动；超过即视为迟到并失败关闭，不做补跑。
const HOST_LIST_LATE_TOLERANCE_MS: u64 = 20;
/// 3 个外部插件必须在共同列表采样前完成真实进程启动与就绪；该预留覆盖握手抖动。
const HOST_LIST_ACTIVATION_RESERVE_MS: u64 = 6_000;
/// 列表结束后洪泛的固定尾巴：脚本启动延迟外的 12 秒发送、最多 3 秒 ACK 结算和 1 秒余量。
const HOST_LIST_FLOOD_SETTLE_MS: u64 = 16_000;
/// 事件循环抖动、滚动等待和帧报告回读的余量。
const HOST_LIST_SCHEDULE_MARGIN_MS: u64 = 2_000;
/// 测量下限必须同时容纳两段 warmup、激活预留、515 槽最小间隔和洪泛结算，否则拒绝运行。
const HOST_LIST_MIN_MEASUREMENT_MS: u64 = 65_000;
/// 验收帧报告只保留 10,000 个 raw 样本；按 16ms 刷新节拍最长约 160 秒。
/// 上限取 120 秒，使约 7500 个刷新帧加上 515 个采样槽仍低于 raw 容量；
/// 超过后必然先丢 raw 帧，因此必须在预算检查中显式拒绝，而不是让容量
/// 问题伪装成“帧丢失”完整性失败。
const HOST_LIST_MAX_MEASUREMENT_MS: u64 = 120_000;

/// 必须显式指定隔离目录，且配置和报告都归属于同一个验收案例，不能落入默认用户数据根。
pub(crate) fn enabled_in_isolated_root(config_root: &Path) -> AppResult<bool> {
    let Some(report) = std::env::var_os("KERMINAL_PLUGIN_PERFORMANCE_REPORT") else {
        return Ok(false);
    };
    let explicit = std::env::var_os("KERMINAL_CONFIG_ROOT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| AppError::invalid("性能验收需要显式绝对配置根"))?;
    let isolation = std::env::var_os("KERMINAL_PLUGIN_PERFORMANCE_ISOLATION_ROOT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| AppError::invalid("性能验收需要显式隔离案例根"))?;
    let report = PathBuf::from(report);
    if !report.is_absolute() || !config_root.is_absolute() {
        return Err(AppError::invalid("性能验收路径必须为绝对路径"));
    }
    let isolation = std::fs::canonicalize(isolation)
        .map_err(|_| AppError::invalid("性能验收隔离案例根不可用"))?;
    let config = std::fs::canonicalize(config_root)
        .map_err(|_| AppError::invalid("性能验收配置根不可用"))?;
    let explicit =
        std::fs::canonicalize(explicit).map_err(|_| AppError::invalid("性能验收配置根不可用"))?;
    let expected = std::fs::canonicalize(isolation.join("config"))
        .map_err(|_| AppError::invalid("性能验收隔离配置缺失"))?;
    let report_parent = report
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or_else(|| AppError::invalid("性能验收报告目录不可用"))?;
    let metrics = std::env::var_os("KERMINAL_ACCEPTANCE_METRICS_OUTPUT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| AppError::invalid("性能帧报告路径无效"))?;
    let metrics_parent = metrics
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .ok_or_else(|| AppError::invalid("性能帧报告目录不可用"))?;
    let report_new = matches!(std::fs::symlink_metadata(&report), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
    let metrics_new = matches!(std::fs::symlink_metadata(&metrics), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
    if config != explicit
        || config != expected
        || !config.starts_with(&isolation)
        || report_parent != isolation
        || metrics_parent != isolation
        || report == metrics
        || !report_new
        || !metrics_new
    {
        return Err(AppError::invalid("性能验收配置或报告不属于隔离案例"));
    }
    let home = dirs::home_dir().ok_or_else(|| AppError::invalid("无法核对性能验收用户目录"))?;
    for name in [".kerminal", ".kerminal-gpui"] {
        let formal = home.join(name);
        if let Ok(formal) = std::fs::canonicalize(formal) {
            if isolation.starts_with(&formal) || formal.starts_with(&isolation) {
                return Err(AppError::invalid("性能验收不能使用用户数据目录"));
            }
        }
    }
    Ok(true)
}

/// 先显式提交生产启动唯一的 RegistryFrozen 阶段，再用同一启动快照追加宿主列表声明。
/// 首帧标记只接受 RegistryFrozen 作为前驱；验收分支若只构建目录而绕过这次提交，
/// 窗口首帧会因仍处于 Starting 而永远冲突。这里不调用 catalogs()，避免对同一阶段
/// 重复提交，也不跳过状态机；`catalogs_for_plugins_inner` 只负责附加验收声明。
/// 编译器拒绝将只能用于本验收功能的 `pub(crate)` 引用暴露到公共类型签名。
pub(crate) fn acceptance_catalogs(
    startup: &crate::plugins::startup::StartupPlugins,
    locale: &str,
) -> AppResult<crate::plugin_catalogs::PluginCatalogs> {
    startup
        .store
        .mark_boot_phase(&startup.snapshot.boot_id, PluginBootPhase::RegistryFrozen)
        .map_err(|source| AppError::infrastructure_with_source("保存插件冻结标记失败", source))?;
    let mut plugins = startup
        .snapshot
        .plugins
        .iter()
        .map(|plugin| (plugin.id.clone(), Arc::new(plugin.manifest.clone())))
        .collect::<std::collections::BTreeMap<_, _>>();
    plugins.extend(
        startup
            .enabled
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.clone())),
    );
    crate::plugin_catalogs::catalogs_for_plugins_inner(
        &plugins.into_values().collect::<Vec<_>>(),
        &startup.enabled,
        locale,
        // 验收窗口沿用生产启动的持久内置状态，避免验收目录与正常启动给出不同门禁答案。
        &startup.builtin_states()?,
        true,
    )
}

/// 在 Root 创建前完成与冻结目录相同的工厂校验，并复用组合根唯一执行器。
pub(crate) fn prepare_services(
    mut services: AppServices,
    enabled: bool,
) -> AppResult<(AppServices, Option<FixtureTelemetry>)> {
    if !enabled {
        return Ok((services, None));
    }
    let (factory, telemetry) = RootView::host_list_performance_factory(services.executor.clone())?;
    services.content_factories.push(factory);
    crate::composition::builtin_plugins::validate_content_factories(
        &services.plugins,
        &services.content_factories,
    )?;
    Ok((services, Some(telemetry)))
}

/// 返回单调时间配对的墙钟锚点；时钟不可用时不猜测采样窗。
fn unix_ms() -> AppResult<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AppError::invalid("系统时间无效"))?
            .as_millis(),
    )
    .map_err(|_| AppError::invalid("系统时间超限"))
}

/// 激活成功不能证明整个列表窗内进程仍存活；采样前后都重新核对同一进程代次，
/// 否则中途退出或重启会被之后的洪泛检查掩盖，列表帧也就不是在 3 插件负载下测得。
fn verify_plugin_generations(
    runtime: &Option<Arc<PluginRuntimeService>>,
    ready: &[(PluginId, u64)],
) -> AppResult<()> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    for (plugin, generation) in ready {
        if runtime.process_generation(plugin)? != Some(*generation) {
            return Err(AppError::conflict("性能验收期间插件进程已退出或重启"));
        }
    }
    Ok(())
}

/// 可视范围来自本帧实际绘制回调；空范围或旧帧覆盖都不能当作新的可见证据。
fn visible_range(snapshot: &Value) -> Option<(u64, u64)> {
    let collection = &snapshot["list"]["collection"];
    let start = collection["visible_start"].as_u64()?;
    let end = collection["visible_end"].as_u64()?;
    (end > start).then_some((start, end))
}

/// 轮询真实 renderer 构造的业务键；滚动等待还要求可视范围在请求后实际变化。
async fn wait_list(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: Option<usize>,
    baseline: Option<(u64, u64)>,
    deadline: Instant,
) -> AppResult<Value> {
    loop {
        let snapshot = window
            .update(cx, |_, _, app| {
                root.update(app, |root, cx| root.host_list_performance_snapshot(cx))
            })
            .map_err(|_| AppError::conflict("性能验收窗口已关闭"))?;
        let ready = match target {
            None => snapshot["workspaceReady"] == true && snapshot["settingsReady"] == true,
            Some(index) => {
                let range = visible_range(&snapshot);
                snapshot["list"]["visible"] == true
                    && snapshot["list"]["status"] == "Ready"
                    && snapshot["list"]["collection"]["total"] == 100_000
                    && range.is_some_and(|(start, end)| {
                        let target = index as u64;
                        target >= start
                            && target < end
                            && baseline.is_none_or(|previous| previous != (start, end))
                    })
                    && snapshot["list"]["collection"]["visible_rows"]
                        .as_array()
                        .is_some_and(|rows| {
                            rows.iter().any(|row| {
                                row["index"] == index && row["key"] == format!("record-{index}")
                            })
                        })
            }
        };
        if ready {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待宿主列表实际业务行"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(8))
            .await;
    }
}

/// 在发起滚动前固定当前实际可视范围，之后必须观察到不同范围才能算新一次滚动。
fn current_visible_range(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> AppResult<(u64, u64)> {
    window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| root.host_list_performance_snapshot(cx))
        })
        .map_err(|_| AppError::conflict("性能验收窗口已关闭"))
        .and_then(|snapshot| {
            visible_range(&snapshot).ok_or_else(|| AppError::conflict("宿主列表实际可视范围缺失"))
        })
}

/// 显隐通知由内容宿主在打开回执后异步发送；私有桌面可能报告窗口非活动，
/// 因此只要求本轮实例真实可见。焦点/活动状态记录但不当门禁，内容证据仍由
/// 列表 visible 与实际渲染目标行单独成立。
async fn wait_host_visibility(
    cx: &mut AsyncApp,
    telemetry: &FixtureTelemetry,
    deadline: Instant,
) -> AppResult<Value> {
    loop {
        let snapshot = telemetry
            .lock()
            .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?
            .clone();
        if snapshot["phase"] == "open" && snapshot["visibility"]["visible"] == true {
            return Ok(snapshot["visibility"].clone());
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待宿主列表真实显隐通知"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(8))
            .await;
    }
}

/// 两组运行同一目标序列；完成时保留每次实际构造行和单调时间，禁止补造缺失行。
async fn exercise_host_list(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    telemetry: &FixtureTelemetry,
    started: Instant,
    warmup: Duration,
    deadline: Instant,
    schedule_interval_ms: u64,
    schedule_late_tolerance_ms: u64,
    activation_elapsed_ms: u64,
) -> AppResult<Value> {
    window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.enable_right_rail_acceptance_activity(cx)
            })
        })
        .map_err(|_| AppError::conflict("性能验收窗口已关闭"))?;
    wait_list(cx, root, window, None, None, deadline).await?;
    window
        .update(cx, |_, window, app| {
            root.update(app, |root, cx| root.host_list_performance_open(window, cx))
        })
        .map_err(|_| AppError::conflict("性能验收窗口已关闭"))??;
    // 先取得内容宿主对本轮实例的真实显隐通知，再采信分页和滚动证据。
    wait_host_visibility(cx, telemetry, deadline).await?;
    let first = wait_list(cx, root, window, Some(0), None, deadline).await?;
    // 预暖结束在 20_000，确保采样首个目标 0 是真实位移，而不是原地空滚动。
    for index in [50_000, 20_000] {
        let baseline = current_visible_range(cx, root, window)?;
        window
            .update(cx, |_, _, app| {
                root.update(app, |root, cx| {
                    root.plugin_list_performance_scroll(index, cx)
                })
            })
            .map_err(|_| AppError::conflict("性能验收窗口已关闭"))??;
        wait_list(cx, root, window, Some(index), Some(baseline), deadline).await?;
    }
    let earliest = started + warmup + Duration::from_secs(1);
    if Instant::now() < earliest {
        cx.background_executor()
            .timer(earliest.saturating_duration_since(Instant::now()))
            .await;
    }
    let metadata = window
        .update(cx, |_, window, app| {
            let viewport = window.viewport_size();
            let theme = app.theme();
            let theme_mode = if theme.is_dark() { "dark" } else { "light" };
            json!({"viewport":[viewport.width.as_f32(),viewport.height.as_f32()],
            "scaleFactor":window.scale_factor(),"fontFamily":theme.font_family.to_string(),
            "fontSize":theme.font_size.as_f32(),"themeMode":theme_mode,
            "windowVisible":window.is_visible(),"windowActive":window.is_window_active()})
        })
        .map_err(|_| AppError::conflict("性能验收窗口已关闭"))?;
    let fetch_start = telemetry
        .lock()
        .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?["fetches"]
        .as_array()
        .ok_or_else(|| AppError::conflict("宿主列表分页遥测无效"))?
        .len();
    // 槽 0 的基线读取本身要占用一次事件循环往返；先读完基线再锚定计划起点，
    // 让首个槽拥有完整调度余量。否则首次快照稍慢就会越过迟到容差而被误判，
    // 与后续槽“先读基线、再等计划时刻”的节奏也不一致。墙钟锚点必须紧邻
    // 单调起点，否则基线读取会进入两者的对齐误差。
    let mut pending_baseline = Some(current_visible_range(cx, root, window)?);
    let origin_unix_ms = unix_ms()?;
    let origin = Instant::now();
    let mut observations = Vec::new();
    // 500 次大跨度扫掠：步长远大于任何窗口可视行数，目标不在当前视口内，
    // 因而每次都必须产生真实滚动范围变化，避免大 viewport 下“目标已可见”的空滚动。
    let sweep = (0..500).map(|step| ("sweep", (step * 4096) % 100_000));
    // 每步目标都与上一目标不同，避免把原地滚动伪装成一次真实位移；
    // 三个五步往返覆盖 0、中段和末段，并在轮次边界保持相邻目标不同。
    let roundtrips = [
        0, 49_999, 99_999, 49_999, 0, 49_999, 0, 99_999, 0, 49_999, 0, 99_999, 49_999, 0, 49_999,
    ]
    .into_iter()
    .map(|index| ("roundtrip", index));
    for (slot, (phase, index)) in sweep.chain(roundtrips).enumerate() {
        if Instant::now() >= deadline {
            return Err(AppError::timeout("宿主列表滚动采样"));
        }
        // 计划以采样起点为绝对锚点。先读取滚动前范围，再等到计划时刻发请求；
        // 这样基线读取耗时不会侵占下一个槽，也不会因为忙等后集中发请求形成追赶突发。
        // 槽 0 的基线已在锚点之前读取，后续槽在等待计划时刻前读取。
        let baseline = match pending_baseline.take() {
            Some(baseline) => baseline,
            None => current_visible_range(cx, root, window)?,
        };
        let planned = origin + Duration::from_millis(slot as u64 * schedule_interval_ms);
        let now = Instant::now();
        if now < planned {
            cx.background_executor()
                .timer(planned.saturating_duration_since(now))
                .await;
        }
        let requested = Instant::now();
        let late_ms = u64::try_from(requested.saturating_duration_since(planned).as_millis())
            .unwrap_or(u64::MAX);
        // 迟到时保留原始时间并由采样失败关闭，不能在等待后补发若干请求伪装固定速率。
        if late_ms > schedule_late_tolerance_ms {
            return Err(AppError::conflict("宿主列表固定节奏采样迟到超过容差"));
        }
        window
            .update(cx, |_, _, app| {
                root.update(app, |root, cx| {
                    root.plugin_list_performance_scroll(index, cx)
                })
            })
            .map_err(|_| AppError::conflict("性能验收窗口已关闭"))??;
        let snapshot = wait_list(cx, root, window, Some(index), Some(baseline), deadline).await?;
        let (visible_start, visible_end) = visible_range(&snapshot)
            .ok_or_else(|| AppError::conflict("宿主列表滚动后可视范围缺失"))?;
        observations.push(json!({"phase":phase,"index":index,"slot":slot,
            "plannedNs":(slot as u64) * schedule_interval_ms * 1_000_000,
            "lateMs":late_ms,
            "baselineVisibleStart":baseline.0,"baselineVisibleEnd":baseline.1,
            "visibleStart":visible_start,"visibleEnd":visible_end,
            "requestedNs":u64::try_from(requested.duration_since(origin).as_nanos()).unwrap_or(u64::MAX),
            "observedNs":u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX),
            "list":snapshot["list"].clone()}));
    }
    let elapsed = origin.elapsed();
    let ended_unix_ms = unix_ms()?;
    let elapsed_ms = u64::try_from(elapsed.as_millis())
        .map_err(|_| AppError::invalid("宿主列表采样时长超限"))?;
    if ended_unix_ms < origin_unix_ms || (ended_unix_ms - origin_unix_ms).abs_diff(elapsed_ms) > 10
    {
        return Err(AppError::conflict("宿主列表墙钟与单调时钟不一致"));
    }
    let fixture = telemetry
        .lock()
        .map_err(|_| AppError::conflict("宿主列表遥测不可用"))?
        .clone();
    Ok(
        json!({"fixture":fixture,"fixtureFetchStart":fetch_start,"firstScreen":first,"metadata":metadata,
        "activation":{"elapsedMs":activation_elapsed_ms,"reserveMs":HOST_LIST_ACTIVATION_RESERVE_MS,
            "scope":"real plugin activation completed before the host list window; the zero plugin case has no external process"},
        "startedUnixMs":origin_unix_ms,"finishedUnixMs":ended_unix_ms,"elapsedMs":elapsed_ms,
        "scheduleIntervalMs":schedule_interval_ms,"scheduleLateToleranceMs":schedule_late_tolerance_ms,
        "scheduleSlots":HOST_LIST_SCHEDULE_SLOTS,
        "scheduleScope":"absolute monotonic plan anchored at the sampling origin; late slots fail closed instead of catching up",
        "sweepTargets":500,"roundtripTargets":15,"observations":observations,
        "scope":"host-in-process data; production NativeRenderer and NativeCollection; no external plugin factory or IPC",
        "observationMeaning":"target row was constructed by the renderer in a visible range that changed after the scroll request; not physical display or a frame count",
        "windowActiveScope":"windowVisible and windowActive are sampled at the interval start; activity may be false and is not a gate",
        "windowStateScope":"windowVisible is recorded for the private desktop, while windowActive/focused may be false; content visibility and rendered target rows remain the hard evidence",
        "themeScope":"themeMode is sampled once at the list interval start; this is not a light/dark/system sweep",
        "comparisonScope":"single-case app observation; the 0/3 comparison is decided by the acceptance script"}),
    )
}

/// 只在显式隔离验收时运行；报告落盘与插件存活核对完成后，交给正式退出事务回收资源。
pub(crate) fn install(
    services: AppServices,
    root: Entity<RootView>,
    fixture: Option<FixtureTelemetry>,
    window: &mut Window,
    cx: &mut App,
) -> AppResult<bool> {
    let Some(output) = std::env::var_os("KERMINAL_PLUGIN_PERFORMANCE_REPORT") else {
        return Ok(false);
    };
    let output = PathBuf::from(output);
    let metrics_output = std::env::var_os("KERMINAL_ACCEPTANCE_METRICS_OUTPUT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| AppError::invalid("插件性能帧报告路径必须为绝对路径"))?;
    if !output.is_absolute() {
        return Err(AppError::invalid("插件性能报告路径必须为绝对路径"));
    }
    let milliseconds = |key: &str| {
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    };
    let fixture = fixture.ok_or_else(|| AppError::conflict("性能验收宿主列表未装配"))?;
    let measurement_ms = milliseconds("KERMINAL_ACCEPTANCE_METRICS_DURATION_MS")
        .filter(|duration| {
            *duration >= HOST_LIST_MIN_MEASUREMENT_MS
                && *duration <= HOST_LIST_MAX_MEASUREMENT_MS
        })
        .ok_or_else(|| {
            AppError::invalid(
                "插件性能采样时长必须在 65 到 120 秒之间；更长时间超过 raw 帧容量，会先丢帧再报完整性失败",
            )
        })?;
    let warmup_ms = milliseconds("KERMINAL_ACCEPTANCE_METRICS_WARMUP_MS").unwrap_or(5_000);
    let minimum_ms = measurement_ms
        .checked_add(warmup_ms)
        .filter(|duration| *duration <= 7_500_000)
        .ok_or_else(|| AppError::invalid("插件性能采样时长无效"))?;
    let timeout = Duration::from_millis(minimum_ms + 30_000);
    // 3 插件先于列表激活；预算必须同时容纳列表前的激活预留、列表内部及帧窗前置的
    // 两段 warmup、515 个槽和列表后的 13 秒洪泛加 ACK 结算。两组使用同一公式，
    // 脚本还会用同一常量核对，间隔不足时直接拒绝运行而不是靠迟到补跑。
    let schedule_available_ms = measurement_ms
        .saturating_sub(warmup_ms.saturating_mul(2))
        .saturating_sub(1_000)
        .saturating_sub(HOST_LIST_FLOOD_SETTLE_MS)
        .saturating_sub(HOST_LIST_ACTIVATION_RESERVE_MS)
        .saturating_sub(HOST_LIST_SCHEDULE_MARGIN_MS);
    let schedule_interval_ms = schedule_available_ms / HOST_LIST_SCHEDULE_SLOTS;
    if schedule_interval_ms < HOST_LIST_MIN_INTERVAL_MS {
        return Err(AppError::invalid(
            "插件性能采样时长不足以完成固定节奏的宿主列表负载",
        ));
    }
    let refresh_window = window.window_handle();
    // 两组使用相同重绘节奏；持续到真实报告落盘的等待预算，不提前停止造成尾段空闲。
    cx.spawn(async move |cx| {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if refresh_window
                .update(cx, |_, window, _| window.refresh())
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    let plugins = services
        .external_plugins
        .iter()
        .map(|plugin| plugin.id.clone())
        .collect::<Vec<_>>();
    let runtime = services.external_plugin_runtime.clone();
    // 由脚本逐案例声明期望数量；0 与 3 不能用“实际为零”互相顶替。
    let expected = std::env::var("KERMINAL_PLUGIN_PERFORMANCE_EXPECTED_PLUGINS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| matches!(*value, 0 | 3))
        .ok_or_else(|| AppError::invalid("性能验收缺少明确的插件数量期望"))?;
    if plugins.len() != expected
        || (expected == 3
            && (!plugins.iter().any(|plugin| plugin.as_str() == "acme.notes") || runtime.is_none()))
    {
        return Err(AppError::invalid(
            "实际外部插件数量或负载身份与验收期望不一致",
        ));
    }
    let executor = services.executor.clone();
    let task_executor = executor.clone();
    let window = window.window_handle();
    cx.spawn(async move |cx| {
        let outcome: AppResult<Value> = async {
            let started = Instant::now();
            let deadline = started + timeout;
            // 列表结束仍要留下插件洪泛尾巴；超时按同一预算失败关闭，不把迟到采样算作完整覆盖。
            let list_tail_ms = warmup_ms
                .saturating_add(HOST_LIST_FLOOD_SETTLE_MS)
                .saturating_add(HOST_LIST_SCHEDULE_MARGIN_MS);
            let list_deadline =
                started + Duration::from_millis(minimum_ms.saturating_sub(list_tail_ms));
            // 3 插件必须先真正启动并就绪，共同列表帧才是在三插件进程负载下采集的；
            // 激活失败直接失败关闭，0 组没有进程负载但仍走同一列表和同一时间预算。
            let activation_runtime = runtime.clone();
            let activation_plugins = plugins.clone();
            // 激活耗时是列表窗相对帧起点的不可避免偏移来源；先记录实际值，再与
            // 固定预留比较。超预算时明确失败关闭，不能把偏移误报成“采样窗不可比”。
            let activation_started = Instant::now();
            let ready = task_executor.run(move |_| async move {
                let mut ready = Vec::new();
                if let Some(runtime) = activation_runtime {
                    for plugin in activation_plugins {
                        let generation = runtime.activate_plugin(plugin.clone(), Instant::now() + Duration::from_secs(20)).await?;
                        if generation == 0 || runtime.process_generation(&plugin)? != Some(generation) {
                            return Err(AppError::conflict("性能验收插件没有就绪进程"));
                        }
                        ready.push((plugin, generation));
                    }
                }
                Ok(ready)
            }).await?;
            let activation_elapsed_ms = u64::try_from(activation_started.elapsed().as_millis())
                .unwrap_or(u64::MAX);
            if activation_elapsed_ms > HOST_LIST_ACTIVATION_RESERVE_MS {
                return Err(AppError::infrastructure_with_source(
                    "插件激活超预算",
                    std::io::Error::other(format!(
                        "激活 {} ms 超过预留 {} ms",
                        activation_elapsed_ms, HOST_LIST_ACTIVATION_RESERVE_MS
                    )),
                ));
            }
            // 采样前核对同代次，随后由脚本核对进程树采样与列表窗相交，二者缺一不可。
            verify_plugin_generations(&runtime, &ready)?;
            let liveness_before_unix_ms = unix_ms()?;
            let list = exercise_host_list(
                cx,
                &root,
                window,
                &fixture,
                started,
                Duration::from_millis(warmup_ms),
                list_deadline,
                schedule_interval_ms,
                HOST_LIST_LATE_TOLERANCE_MS,
                activation_elapsed_ms,
            )
            .await?;
            let liveness_after_unix_ms = unix_ms()?;
            verify_plugin_generations(&runtime, &ready)?;
            let notes_tab = if !plugins.is_empty() {
                Some(window.update(cx, |_, window, app| root.update(app, |root, cx| root.external_plugin_acceptance_open_notes(window, cx)))
                    .map_err(|_| AppError::conflict("性能验收窗口已关闭"))??.as_str().to_owned())
            } else { None };
            let mut renderer = None;
            let mut metrics_observed = false;
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                // done 后仍持续验证，覆盖采样窗后半段；失败不能由启动状态文件掩盖。
                if let Some(runtime) = &runtime {
                    for (plugin, generation) in &ready {
                        if runtime.process_generation(plugin)? != Some(*generation) {
                            return Err(AppError::conflict("性能验收期间插件进程已退出或重启"));
                        }
                    }
                }
                if !plugins.is_empty() && renderer.is_none() {
                    let snapshot = window.update(cx, |_, window, app| root.update(app, |root, cx| root.external_plugin_acceptance_snapshot(window, cx)))
                        .map_err(|_| AppError::conflict("性能验收窗口已关闭"))?;
                    let current = snapshot["tabs"].as_array().and_then(|tabs| tabs.iter().find(|tab| tab["tabId"].as_str() == notes_tab.as_deref())).map(|tab| &tab["renderer"]);
                    if let Some(current) = current && current["status"] == "Ready" && current["data"]["done"] == true {
                        if current["data"]["completed"] != true || current["data"]["failed"] != 0 {
                            return Err(AppError::conflict("插件性能负载未全部确认成功"));
                        }
                        renderer = Some(current.clone());
                    }
                }
                if !metrics_observed && started.elapsed() >= Duration::from_millis(minimum_ms) {
                    let path = metrics_output.clone();
                    let list_start = list["startedUnixMs"].as_u64().ok_or_else(|| AppError::conflict("宿主列表采样起点缺失"))?;
                    let list_end = list["finishedUnixMs"].as_u64().ok_or_else(|| AppError::conflict("宿主列表采样终点缺失"))?;
                    metrics_observed = task_executor.run(move |_| async move {
                        let bytes = match tokio::fs::read(path).await {
                            Ok(bytes) => bytes,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                            Err(error) => return Err(AppError::infrastructure_with_source("读取插件性能帧报告失败", error)),
                        };
                        let report: Value = serde_json::from_slice(&bytes).map_err(|error| AppError::infrastructure_with_source("解析插件性能帧报告失败", error))?;
                        if report["targetMeasurementDurationMs"].as_u64() != Some(measurement_ms) || !report["framePhases"]["rawFrames"]["samples"].as_array().is_some_and(|samples| !samples.is_empty()) {
                            return Err(AppError::conflict("插件性能帧报告没有本轮实际采样"));
                        }
                        let raw = &report["framePhases"]["rawFrames"];
                        let frame_start = raw["originUnixMs"].as_u64().ok_or_else(|| AppError::conflict("性能帧采样起点缺失"))?;
                        let frame_end = raw["measurementEndUnixMs"].as_u64().ok_or_else(|| AppError::conflict("性能帧采样终点缺失"))?;
                        if list_start < frame_start.saturating_add(10) || list_end > frame_end.saturating_sub(10) {
                            return Err(AppError::conflict("宿主列表滚动未完整落入实际帧采样窗"));
                        }
                        Ok(true)
                    }).await?;
                }
                if metrics_observed && (plugins.is_empty() || renderer.is_some()) {
                    return Ok(json!({"pluginsReady": ready.iter().map(|(plugin, generation)| json!({"plugin": plugin.as_str(), "generation": generation})).collect::<Vec<_>>(), "expectedExternalPlugins": expected,
                        "loadedExternalPlugins": plugins.iter().map(|plugin| plugin.as_str()).collect::<Vec<_>>(),
                        "pluginLiveness": {"beforeListUnixMs": liveness_before_unix_ms, "afterListUnixMs": liveness_after_unix_ms,
                            "scope":"process generation rechecked immediately before and after the host list sampling; intersecting process tree samples are gated by the acceptance script"},
                        "renderer": renderer, "notesTabId":notes_tab, "hostList":list, "metricsObserved": true,
                        "comparisonScope":"single-case app observation only; cross-case acceptance requires the script comparison",
                        "observedElapsedMs": started.elapsed().as_millis()}));
                }
                if Instant::now() >= deadline { return Err(AppError::timeout("插件性能视图、洪泛与实际帧报告")); }
            }
        }.await;
        let report = match outcome { Ok(data) => json!({"status":"observed", "data":data}), Err(error) => json!({"status":"failed", "message":error.public_message()}) };
        if let Err(error) = executor.run(move |_| async move {
            let bytes = serde_json::to_vec_pretty(&report).map_err(|error| AppError::infrastructure_with_source("序列化插件性能报告失败", error))?;
            tokio::fs::write(output, bytes).await.map_err(|error| AppError::infrastructure_with_source("写入插件性能报告失败", error))?;
            Ok(())
        }).await {
            tracing::error!(event = "PLUGIN_PERFORMANCE_REPORT_FAILED", error = %error);
        }
        // 不先发异步关标签，避免与正式退出的工作区 Busy 门禁竞争；脚本等待退出码和 Job 归零。
        if let Err(error) = window.update(cx, |_, _, app| root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))) {
            tracing::error!(event = "PLUGIN_PERFORMANCE_QUIT_FAILED", error = %error);
        }
    }).detach();
    Ok(true)
}
