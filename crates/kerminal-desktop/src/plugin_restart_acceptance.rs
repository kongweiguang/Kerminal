//! 显式验收环境驱动普通 main 的三代进程，重启与资源回收始终由生产宿主拥有。
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::{AnyWindowHandle, App, AsyncApp, ElementId, Entity, Role, Window};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    extensions::{ExtensionsService, install_progress_channel},
    preferences::ThemeMode,
};
use kerminal_domain::plugins::{PendingPluginChange, PluginStoreState};
use kerminal_ui::{AppServices, QuitIntent, RootView};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{plugins::startup::StartupPlugins, right_rail_acceptance::isolation};

mod persistence;

const WAIT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(100);
const NOTES_FIELD: &str = "external-field-root.1";
const SAVE_BUTTON: &str = "external-button-root.2.0";
const CHECK_BUTTON: &str = "external-button-root.2.1";
const COPY_BUTTON: &str = "external-button-root.2.2";
const COUNT_BUTTON: &str = "external-button-root.2.3";

/// 启动阶段的插件目录/设置读取可能与权威状态锁重叠；短暂等待后仍通过正式服务回读，
/// 不绕过锁、不直接读文件，也不把持久错误伪装成成功。
async fn extension_state(service: &ExtensionsService) -> AppResult<PluginStoreState> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match service.state().await {
            Ok(state) => return Ok(state),
            Err(error) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = error;
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone)]
struct Options {
    output: PathBuf,
    config: PathBuf,
    initial_package: PathBuf,
    package: PathBuf,
    theme: ThemeMode,
}

/// 普通发行路径默认无动作；只读环境后调度，路径回读及所有文件操作均进入共享执行器。
pub(crate) fn install(
    services: AppServices,
    root: Entity<RootView>,
    window: &mut Window,
    cx: &mut App,
    startup: Arc<StartupPlugins>,
) -> AppResult<bool> {
    let Some(output) = std::env::var_os("KERMINAL_PLUGIN_RESTART_OUTPUT") else {
        return Ok(false);
    };
    let options = Options {
        output: output.into(),
        config: std::env::var_os("KERMINAL_CONFIG_ROOT")
            .map(PathBuf::from)
            .ok_or_else(|| AppError::invalid("重启验收缺少隔离配置根"))?,
        initial_package: std::env::var_os("KERMINAL_PLUGIN_RESTART_INITIAL_PACKAGE")
            .map(PathBuf::from)
            .ok_or_else(|| AppError::invalid("重启验收缺少本地初始包"))?,
        package: std::env::var_os("KERMINAL_PLUGIN_RESTART_PACKAGE")
            .map(PathBuf::from)
            .ok_or_else(|| AppError::invalid("重启验收缺少本地更新包"))?,
        theme: match std::env::var("KERMINAL_PLUGIN_RESTART_THEME").as_deref() {
            Ok("light") => ThemeMode::Light,
            Ok("dark") => ThemeMode::Dark,
            Ok("system") => ThemeMode::System,
            _ => return Err(AppError::invalid("重启验收主题无效")),
        },
    };
    if !options.output.is_absolute()
        || !options.config.is_absolute()
        || !options.initial_package.is_absolute()
        || !options.package.is_absolute()
        || startup.options.safe_mode
        || startup.options.dev_plugin.is_some()
    {
        return Err(AppError::invalid("重启验收必须使用本地包及显式绝对路径"));
    }
    let service = services
        .extensions
        .clone()
        .ok_or_else(|| AppError::invalid("缺少正式扩展服务"))?;
    let settings = services
        .external_plugin_settings
        .clone()
        .ok_or_else(|| AppError::invalid("缺少正式插件设置服务"))?;
    let executor = services.executor.clone();
    let window = window.window_handle();
    cx.spawn(async move |cx| {
        let result = exercise(
            cx,
            &root,
            window,
            &executor,
            service,
            settings,
            startup,
            &options,
        )
        .await;
        if let Err(error) = result {
            let path = options.output.join(format!("failure-{}.json", std::process::id()));
            // 公开 message 供脚本失败关闭；detail 只写入隔离验收目录，沿 source 链保留内部阶段。
            let report = json!({"status":"failed", "processId":std::process::id(), "message":error.public_message(), "detail":error_chain(&error)});
            if let Err(write_error) = executor.run(move |_| async move { write_json(path, report).await }).await {
                tracing::error!(event="PLUGIN_RESTART_FAILURE_RECEIPT_FAILED", error=%write_error);
            }
            tracing::error!(event="PLUGIN_RESTART_ACCEPTANCE_FAILED", error=%error);
            // 失败分支同样要求生产退出明确受理；false 和窗口错误都必须留下诊断，不能静默当作已退出。
            match window.update(cx, |_, _, app| root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))) {
                Ok(true) => {}
                Ok(false) => tracing::error!(event="PLUGIN_RESTART_FAILURE_QUIT_FAILED", accepted=false),
                Err(quit_error) => tracing::error!(event="PLUGIN_RESTART_FAILURE_QUIT_FAILED", error=%quit_error),
            }
        }
    }).detach();
    Ok(true)
}

/// 每代先让脚本持有真实退出句柄，再用正式视图和事件路径核对能力；成功只提交退出意图，不自行 spawn。
async fn exercise(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    service: Arc<ExtensionsService>,
    settings: kerminal_application::plugin_settings::PluginSettingsService,
    startup: Arc<StartupPlugins>,
    options: &Options,
) -> AppResult<()> {
    let native = serde_json::to_value(isolation::capture().map_err(io_error)?).map_err(io_error)?;
    if native["isolationProven"] != true {
        return Err(AppError::invalid("重启验收不在私有桌面"));
    }
    let setup = options.clone();
    let startup_service = service.clone();
    let native_startup = native.clone();
    let (phase, initial) = executor.run(move |_| async move {
        let config = tokio::fs::canonicalize(&setup.config).await.map_err(io_error)?;
        let expected = tokio::fs::canonicalize(setup.output.join("config")).await.map_err(io_error)?;
        if config != expected { return Err(AppError::invalid("重启验收配置根不属于本轮输出")); }
        // 主线单版本目录已移除人工回退，验收状态机固定为三代；发现旧四代残留时显式失败，
        // 避免把已不存在的回退代当成下一阶段继续授权，也不能静默沿用旧成功收据。
        if tokio::fs::try_exists(setup.output.join("observed-2.json")).await.map_err(io_error)? {
            return Err(AppError::invalid("重启验收仍是旧四代形态"));
        }
        let phase = if tokio::fs::try_exists(setup.output.join("observed-1.json")).await.map_err(io_error)? { 2 }
            else if tokio::fs::try_exists(setup.output.join("observed-0.json")).await.map_err(io_error)? { 1 } else { 0 };
        let version = expected_version(phase)?;
        let frozen = startup.enabled.iter().find(|plugin| plugin.id.as_str() == "acme.notes")
            .map(|plugin| plugin.package.manifest.version.as_str());
        if frozen != version { return Err(AppError::conflict("启动冻结版本与阶段不符")); }
        let state = extension_state(&startup_service).await?;
        if let Some(version) = version { validate_state(&state, version, None)?; }
        else { validate_bootstrap(&state, false)?; }
        let executable = std::env::current_exe().map_err(io_error)?;
        let hash = format!("{:x}", Sha256::digest(tokio::fs::read(&executable).await.map_err(io_error)?));
        let report = json!({"schemaVersion":1,"phase":phase,"processId":std::process::id(),"candidateSha256":hash,
            "executable":executable,"frozenVersion":version,"state":state,"nativeIsolation":native_startup,"argv":std::env::args_os().map(|arg|arg.to_string_lossy().into_owned()).collect::<Vec<_>>()});
        write_json(setup.output.join(format!("startup-{phase}.json")), report.clone()).await?;
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match tokio::fs::read(setup.output.join(format!("ack-{phase}.json"))).await {
                Ok(bytes) => {
                    let ack: Value = serde_json::from_slice(&bytes).map_err(io_error)?;
                    if ack["processId"] != std::process::id() || ack["phase"] != phase {
                        return Err(AppError::conflict("重启验收句柄握手身份不符"));
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
            if tokio::time::Instant::now() >= deadline { return Err(AppError::timeout("等待脚本持有本代退出句柄")); }
            tokio::time::sleep(POLL).await;
        }
        Ok((phase, report))
    }).await?;
    let ready = wait_view(cx, root, window, None)
        .await
        .map_err(|error| stage_error(error, "阶段 wait_view"))?;
    window
        .update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                // 重启验收只操作工作区标签和内容；右栏活动面板会挤窄标签栏，
                // 使已保存的首标签脱离可点击可视区，因此先沿生产开关关闭它。
                root.close_right_rail_acceptance_panels(window, cx);
                root.set_right_rail_acceptance_theme(options.theme, cx);
            })
        })
        .map_err(io_error)?;
    let version = expected_version(phase)?;
    let (renderer, persistence_evidence, new_capability) = if let Some(version) = version {
        persistence::exercise_generation(cx, root, window, executor, &settings, phase, version)
            .await
            .map_err(|error| stage_error(error, "阶段 persistence"))?
    } else {
        (
            ready,
            persistence::bootstrap_evidence(),
            json!({
                "viewSemantics": {"status":"not-applicable-bootstrap"},
                "behavior": {"status":"not-attempted-bootstrap"},
                "physicalScreenVisibility": "not-observed"
            }),
        )
    };
    // GPU读回留在所属窗口线程；这不是系统合成画面，编码和写盘在执行器中完成。
    let image = window
        .update(cx, |_, window, app| {
            crate::plugin_acceptance::draw(window, app);
            window.render_to_image()
        })
        .map_err(io_error)?
        .map_err(io_error)?;
    if image.width() < 100
        || image.height() < 100
        || !image.as_raw().chunks_exact(4).any(|pixel| pixel[3] != 0)
    {
        return Err(AppError::conflict("GPUI 离屏帧为空"));
    }
    let frame = options.output.join(format!(
        "native-{phase}-{}.png",
        version.unwrap_or("bootstrap")
    ));
    executor
        .run(move |_| async move {
            tokio::task::spawn_blocking(move || image.save(frame).map_err(io_error))
                .await
                .map_err(io_error)?
        })
        .await?;
    let package = if phase == 0 {
        options.initial_package.clone()
    } else {
        options.package.clone()
    };
    let changed = executor
        .run(move |cancellation| async move {
            if phase < 2 {
                let inspected = service
                    .inspect_file(package.clone(), cancellation.clone())
                    .await?;
                let package_version = if phase == 0 { "1.0.0" } else { "1.1.0" };
                if inspected.id.as_str() != "acme.notes"
                    || inspected.version.to_string() != package_version
                {
                    return Err(AppError::invalid("本地插件包身份不符"));
                }
                let (progress, _receiver) = install_progress_channel();
                let receipt = service
                    .install_file(package, inspected, true, progress, cancellation)
                    .await?;
                if !receipt.restart_required || receipt.cleanup_warning.is_some() {
                    return Err(AppError::conflict("本地安装未形成确定的待重启事务"));
                }
            }
            // 第三代只核对升级后的运行态并退出；主线单版本目录已固定拒绝回退，
            // 验收不能再制造人工回退事务，否则会把已移除的旧契约当成成功路径。
            let state = service.state().await?;
            if let Some(version) = version {
                validate_state(&state, version, if phase < 2 { Some(phase) } else { None })?;
            } else {
                validate_bootstrap(&state, true)?;
            }
            Ok(state)
        })
        .await
        .map_err(|error| stage_error(error, "阶段安装与状态回读"))?;
    // 前两代分别产生初始安装和本地更新事务，第三代只完成升级后核验并退出。
    let operation = match phase {
        0 => "local-file-initial-install",
        1 => "local-file-update",
        _ => "exit",
    };
    let requested_intent = if phase < 2 {
        "RestartForExtensions"
    } else {
        "Exit"
    };
    let report = json!({"schemaVersion":1,"status":"observed","phase":phase,"processId":std::process::id(),
        "startup":initial,"nativeRenderer":renderer,"persistence":persistence_evidence,
        "newCapability":new_capability,"stateAfterOperation":changed,"nativeIsolation":native,
        "frameEvidence":{"source":"gpui-render-to-image","validation":"dimensions-and-nonempty-alpha-only","featurePixelsChecked":false,"systemComposition":"not-observed"},
        "operation":operation,"requestedIntent":requested_intent,"versionMarker":version});
    let observed_path = options.output.join(format!("observed-{phase}.json"));
    let observed_digest = executor
        .run(move |_| async move { write_json_with_digest(observed_path, report).await })
        .await?;
    let intent = if phase < 2 {
        QuitIntent::RestartForExtensions
    } else {
        QuitIntent::Exit
    };
    let quit_accepted = window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| root.request_quit(intent, cx))
        })
        .map_err(io_error)?;
    if !quit_accepted {
        return Err(AppError::conflict("生产退出请求未被接受"));
    }
    // 完成收据在全部动作和退出请求之后发布；没有它时，缺 failure 或临时文件不能算成功。
    // Root 的 QuitReady 只在进入生产 HostCommand 后产生 HOST_QUIT/RESTART 收据，脚本另行核对。
    let completion = json!({
        "schemaVersion": 1,
        "status": "completed",
        "phase": phase,
        "processId": std::process::id(),
        "observed": format!("observed-{phase}.json"),
        "observedSha256": observed_digest,
        "operation": operation,
        "requestedIntent": requested_intent,
        "quitRequestAccepted": true,
        "acceptanceBoundary": "all-generation-workflows-finished-and-quit-requested"
    });
    let completion_path = options.output.join(format!("completion-{phase}.json"));
    executor
        .run(move |_| async move { write_json(completion_path, completion).await })
        .await?;
    Ok(())
}

/// 保留活动标签、实例和进程代次，避免异步验收把另一实例的更新误认为当前控件的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RendererIdentity {
    tab: String,
    instance: String,
    instance_generation: u64,
    process_generation: u64,
}

/// 只接受正式笔记 renderer 的活动标签及健康运行状态，不能以冻结包或预期数据代替实体。
fn active_renderer(snapshot: &Value) -> AppResult<(&str, &Value)> {
    let tab_id = snapshot["activeTabId"]
        .as_str()
        .ok_or_else(|| AppError::conflict("笔记活动标签缺失"))?;
    let tab = snapshot["tabs"]
        .as_array()
        .and_then(|tabs| tabs.iter().find(|tab| tab["tabId"] == tab_id))
        .ok_or_else(|| AppError::conflict("笔记活动标签快照缺失"))?;
    let renderer = &tab["renderer"];
    if renderer["status"] != "Ready"
        || renderer["error"] != Value::Null
        || renderer["plugin"] != "acme.notes"
        || renderer["view"] != "notes.editor"
    {
        return Err(AppError::conflict("活动笔记实体不可用或身份不符"));
    }
    Ok((tab_id, renderer))
}

/// 每项身份均来自真实 renderer 回执；脚本进程代次必须存在且不能使用静态插件的零代次。
fn renderer_identity(snapshot: &Value) -> AppResult<RendererIdentity> {
    let (tab, renderer) = active_renderer(snapshot)?;
    let instance = renderer["instance"]
        .as_str()
        .filter(|instance| !instance.is_empty())
        .ok_or_else(|| AppError::conflict("笔记实例身份缺失"))?;
    let instance_generation = renderer["instanceGeneration"]
        .as_u64()
        .ok_or_else(|| AppError::conflict("笔记实例代次缺失"))?;
    let process_generation = renderer["processGeneration"]
        .as_u64()
        .filter(|generation| *generation > 0)
        .ok_or_else(|| AppError::conflict("笔记插件进程代次缺失"))?;
    Ok(RendererIdentity {
        tab: tab.to_owned(),
        instance: instance.to_owned(),
        instance_generation,
        process_generation,
    })
}

/// 从真实 GPUI 控件最近一次绘制的语义快照取事实；布局可见性不代表物理屏幕可见。
fn control_evidence(window: &Window, selector: &str) -> AppResult<Value> {
    let id: ElementId = selector.to_owned().into();
    let matches = gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .filter(|element| element.path().last() == Some(&id))
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(AppError::conflict("笔记控件身份不唯一"));
    }
    let Some(element) = matches.first() else {
        return Ok(Value::Null);
    };
    let bounds = element.bounds();
    Ok(json!({
        "selector": selector,
        "isButton": element.role() == Some(Role::Button),
        "accessibleName": element.label(),
        "layoutVisible": element.visible(),
        "supportsClick": element.supports_click(),
        "accessibilityDisabled": element.disabled(),
        "bounds": {
            "x": bounds.origin.x.as_f32(), "y": bounds.origin.y.as_f32(),
            "width": bounds.size.width.as_f32(), "height": bounds.size.height.as_f32()
        }
    }))
}

/// 同时约束语义、非禁用标记和有限正尺寸；Click 只代表节点声明的能力，不代表物理点击成功。
fn valid_button(value: &Value, names: &[&str]) -> bool {
    value["isButton"] == true
        && value["layoutVisible"] == true
        && value["supportsClick"] == true
        && matches!(
            value.get("accessibilityDisabled"),
            Some(Value::Null | Value::Bool(false))
        )
        && value["accessibleName"]
            .as_str()
            .is_some_and(|name| names.contains(&name))
        && ["x", "y", "width", "height"].into_iter().all(|key| {
            value["bounds"][key].as_f64().is_some_and(|number| {
                number.is_finite() && (!matches!(key, "width" | "height") || number > 0.0)
            })
        })
}

/// 动作前保留旧结果且必须与新期望不同，避免已有值让未触发的点击也通过。
fn valid_count_before_click(before: &Value, previous: &Value, expected: u64) -> bool {
    before == previous && before != &json!(expected)
}

/// 更新必须对应当前输入且控件重新声明 Click，不能把旧值或动作中途补丁当作结果。
fn count_result_ready(renderer: &Value, count_button: &Value, input: &str, expected: u64) -> bool {
    valid_button(count_button, &["Count code points", "统计码点"])
        && renderer["data"]["text"] == input
        && renderer["data"]["codePointCount"] == expected
}

/// 每一代先确认已有按钮真实挂载，再比较复制和统计控件；空白视图不能冒充旧版缺失。
fn validate_view_semantics(controls: &Value, new_version: bool) -> AppResult<()> {
    if !valid_button(&controls["save"], &["Save", "保存"])
        || !valid_button(
            &controls["check"],
            &["Check example.com", "检查 example.com"],
        )
    {
        return Err(AppError::conflict("原有笔记控件语义未确认"));
    }
    if new_version {
        if !valid_button(&controls["copy"], &["Copy note", "复制笔记"])
            || !valid_button(&controls["count"], &["Count code points", "统计码点"])
        {
            return Err(AppError::conflict("新版笔记控件语义未确认"));
        }
    } else if !controls["copy"].is_null() || !controls["count"].is_null() {
        return Err(AppError::conflict("旧版笔记出现新版控件"));
    }
    Ok(())
}

/// 合成输入只经 GPUI 窗口事件分发，结果来自实际插件进程补丁，不访问系统剪贴板。
async fn inspect_capability(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    initial: &Value,
    version: &str,
) -> AppResult<Value> {
    let identity = renderer_identity(initial)?;
    let (observed, controls) =
        wait_view_semantics(cx, root, window, &identity, version == "1.1.0").await?;
    let new_version = version == "1.1.0";
    let (_, renderer) = active_renderer(&observed)?;
    if !new_version {
        if renderer["data"].get("codePointCount").is_some() {
            return Err(AppError::conflict("旧版笔记包含新版统计结果"));
        }
        return Ok(json!({
            "viewSemantics": {"status":"new-controls-absent", "source":"gpui-last-draw", "controls":controls},
            "behavior": {"status":"not-attempted-control-absent"},
            "physicalScreenVisibility":"not-observed"
        }));
    }
    if renderer["data"]["codePointCount"] != "" {
        return Err(AppError::conflict("新版笔记统计结果不是初始空值"));
    }
    let inputs = [
        format!("probe{}short", std::process::id()),
        format!("probe{}longer猫\u{1f642}", std::process::id()),
    ];
    let mut previous_count = json!("");
    let mut cases = Vec::new();
    for input in inputs {
        crate::plugin_acceptance::edit_input(cx, window, NOTES_FIELD, &input)
            .map_err(|error| io_error(std::io::Error::other(error.to_string())))?;
        let (before, control_before) =
            wait_capability_snapshot(cx, root, window, &identity, |renderer, count| {
                valid_button(count, &["Count code points", "统计码点"])
                    && renderer["data"]["text"] == input
                    && renderer["fields"].as_array().is_some_and(|fields| {
                        fields.iter().any(|field| {
                            field["binding"] == "text"
                                && field["kind"] == "textarea"
                                && field["value"] == input
                        })
                    })
            })
            .await?;
        let before_count = active_renderer(&before)?.1["data"]["codePointCount"].clone();
        let expected_count = u64::try_from(input.chars().count()).map_err(io_error)?;
        if !valid_count_before_click(&before_count, &previous_count, expected_count) {
            return Err(AppError::conflict("统计动作前的结果不符合前一状态"));
        }
        crate::plugin_acceptance::click_selector(cx, window, COUNT_BUTTON, false)
            .map_err(|error| io_error(std::io::Error::other(error.to_string())))?;
        let (after, control_after) =
            wait_capability_snapshot(cx, root, window, &identity, |renderer, count| {
                count_result_ready(renderer, count, &input, expected_count)
            })
            .await?;
        let actual_count = active_renderer(&after)?.1["data"]["codePointCount"].clone();
        cases.push(json!({
            "input":input, "countBeforeClick":before_count,
            "expectedCodePoints":expected_count, "observedCodePoints":actual_count,
            "controlBeforeClick":control_before, "controlAfterClick":control_after
        }));
        previous_count = json!(expected_count);
    }
    Ok(json!({
        "viewSemantics": {"status":"new-controls-present", "source":"gpui-last-draw", "controls":controls},
        "behavior": {
            "status":"confirmed", "inputAndClick":"gpui-window-event-dispatch",
            "resultSource":"native-renderer-plugin-data", "countControlClickAdvertisedAfterResult":true,
            "asynchronousSideEffects":"not-observed", "tabId":identity.tab,
            "instance":identity.instance, "instanceGeneration":identity.instance_generation,
            "processGeneration":identity.process_generation, "cases":cases,
            "clipboardCopyTested":false, "nativeImeTested":false
        },
        "physicalScreenVisibility":"not-observed"
    }))
}

/// 打开回执可能先于控件挂载；等待真实绘制完成，并在超时时保留末次语义事实。
async fn wait_view_semantics(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    identity: &RendererIdentity,
    new_version: bool,
) -> AppResult<(Value, Value)> {
    let deadline = Instant::now() + WAIT;
    loop {
        let (snapshot, controls) = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::checked_isolation(window).map_err(io_error)?;
                crate::plugin_acceptance::draw(window, app);
                let snapshot = root.update(app, |root, cx| {
                    root.external_plugin_acceptance_snapshot(window, cx)
                });
                let controls = json!({
                    "save": control_evidence(window, SAVE_BUTTON)?,
                    "check": control_evidence(window, CHECK_BUTTON)?,
                    "copy": control_evidence(window, COPY_BUTTON)?,
                    "count": control_evidence(window, COUNT_BUTTON)?
                });
                Ok::<_, AppError>((snapshot, controls))
            })
            .map_err(io_error)??;
        if renderer_identity(&snapshot)? != *identity {
            return Err(AppError::conflict("笔记实例在语义检查期间改变"));
        }
        if validate_view_semantics(&controls, new_version).is_ok() {
            return Ok((snapshot, controls));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待笔记控件的真实语义")
                .with_source(std::io::Error::other(controls.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 每轮重新检查隔离和实体身份；结果到达后仍须观察到控件声明 Click，不推断其他任务均已结束。
async fn wait_capability_snapshot(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    identity: &RendererIdentity,
    predicate: impl Fn(&Value, &Value) -> bool,
) -> AppResult<(Value, Value)> {
    let deadline = Instant::now() + WAIT;
    loop {
        let (snapshot, count) = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::checked_isolation(window).map_err(io_error)?;
                crate::plugin_acceptance::draw(window, app);
                let snapshot = root.update(app, |root, cx| {
                    root.external_plugin_acceptance_snapshot(window, cx)
                });
                Ok::<_, AppError>((snapshot, control_evidence(window, COUNT_BUTTON)?))
            })
            .map_err(io_error)??;
        if renderer_identity(&snapshot)? != *identity {
            return Err(AppError::conflict("笔记实例在动作完成前改变"));
        }
        if predicate(active_renderer(&snapshot)?.1, &count) {
            return Ok((snapshot, count));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待笔记控件动作的真实结果")
                .with_source(std::io::Error::other(snapshot.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 权威状态与当前运行版本分开校验，排队成功必须同时满足活动版本未热替换及确切pending目标。
/// 主线单版本目录不再接受回退排队，pending 只允许本代产生的 Update；出现其他形状即拒绝。
fn validate_state(
    state: &PluginStoreState,
    version: &str,
    pending_phase: Option<u8>,
) -> AppResult<()> {
    let plugin = state
        .plugins
        .iter()
        .find(|(id, _)| id.as_str() == "acme.notes")
        .map(|(_, plugin)| plugin)
        .ok_or_else(|| AppError::conflict("权威状态缺少笔记插件"))?;
    if plugin.active.to_string() != version || !plugin.enabled || !state.failures.is_empty() {
        return Err(AppError::conflict(
            "权威活动版本、启用状态或启动失败记录不符",
        ));
    }
    let valid_pending = match pending_phase {
        Some(1) => {
            matches!(state.pending.as_slice(), [PendingPluginChange::Update {id,to,..}] if id.as_str()=="acme.notes" && to.to_string()=="1.1.0")
        }
        None => state.pending.is_empty(),
        _ => false,
    };
    if !valid_pending {
        return Err(AppError::conflict("权威pending与本代操作不符"));
    }
    Ok(())
}

/// 初始代从普通迁移创建的空根开始，首次安装也必须回读正式installer产生的确切pending。
fn validate_bootstrap(state: &PluginStoreState, pending: bool) -> AppResult<()> {
    let pending_valid = if pending {
        matches!(state.pending.as_slice(), [PendingPluginChange::Install {id,to,..}] if id.as_str()=="acme.notes" && to.to_string()=="1.0.0")
    } else {
        state.pending.is_empty()
    };
    if !state.plugins.is_empty() || !state.failures.is_empty() || !pending_valid {
        return Err(AppError::conflict("空根初始安装状态与阶段不符"));
    }
    Ok(())
}

/// 初始安装尚无活动版本，随后只允许 1.0.0 和升级后的 1.1.0 两代；
/// 第四个阶段必须报错，防止旧四代脚本把已移除的回退代继续当作成功路径。
fn expected_version(phase: u8) -> AppResult<Option<&'static str>> {
    match phase {
        0 => Ok(None),
        1 => Ok(Some("1.0.0")),
        2 => Ok(Some("1.1.0")),
        _ => Err(AppError::invalid("重启验收阶段超出三代契约")),
    }
}

/// 观察正式工厂创建的活动NativeRenderer，并先完成真实绘制；只在工作区和设置完成恢复后打开内容。
async fn wait_view(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    marker: Option<&str>,
) -> AppResult<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::draw(window, app);
                root.update(app, |root, cx| {
                    root.external_plugin_acceptance_snapshot(window, cx)
                })
            })
            .map_err(io_error)?;
        let active = snapshot["tabs"].as_array().and_then(|tabs| {
            tabs.iter()
                .find(|tab| tab["tabId"] == snapshot["activeTabId"])
        });
        let ready = match marker {
            None => snapshot["workspaceReady"] == true && snapshot["settingsReady"] == true,
            Some(expected) => active.is_some_and(|tab| {
                tab["renderer"]["status"] == "Ready"
                    && tab["renderer"]["data"]["marker"] == expected
            }),
        };
        if ready {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待真实笔记视图及版本marker")
                .with_source(std::io::Error::other(snapshot.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 单次收据先完整写临时文件再发布；重复阶段拒绝覆盖，防止故障重跑被误计作新的进程代次。
async fn write_json(path: PathBuf, value: Value) -> AppResult<()> {
    write_json_with_digest(path, value).await.map(|_| ())
}

/// 发布时返回精确落盘字节的摘要，完成收据可据此绑定 observed，而不是只声明阶段成功。
async fn write_json_with_digest(path: PathBuf, value: Value) -> AppResult<String> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&value).map_err(io_error)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(io_error)?;
    file.write_all(&bytes).await.map_err(io_error)?;
    file.sync_all().await.map_err(io_error)?;
    drop(file);
    if tokio::fs::try_exists(&path).await.map_err(io_error)? {
        return Err(AppError::conflict("验收收据已经存在"));
    }
    tokio::fs::rename(temporary, path).await.map_err(io_error)?;
    Ok(digest)
}

/// 外层阶段名保留内部错误文案，避免 with_source 替换源链后丢失真实等待点。
fn stage_error(error: AppError, name: &'static str) -> AppError {
    let detail = error_chain(&error);
    error.with_source(std::io::Error::other(format!("{name}: {detail}")))
}

/// 固定错误分类保留原始source，避免将文件或GPU错误吞成验收成功。
fn io_error(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> AppError {
    AppError::infrastructure_with_source("插件重启验收失败", std::io::Error::other(error))
}

/// 验收失败详情沿 source 链展开；只写入隔离输出目录，不进入普通日志或远程通道。
pub(super) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(" <- ")
}

#[cfg(test)]
#[path = "../tests/support/plugin_restart_acceptance_unit.rs"]
mod tests;
