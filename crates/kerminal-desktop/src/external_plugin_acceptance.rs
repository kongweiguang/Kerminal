//! 正式外部插件的两进程原生验收；平台输入与截图复用已有隔离工具，不注册静态测试插件。
//! @author kongweiguang

use crate::{
    bootstrap,
    composition::DesktopComposition,
    plugin_acceptance::{
        capture_frame, checked_isolation, click_selector, draw, edit_input, viewport_evidence,
        wait_selector,
    },
    plugins::{arguments::PluginStartupOptions, startup::StartupPlugins},
    runtime::ApplicationRuntime,
};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, Keystroke, TitlebarOptions,
    WindowBackgroundAppearance, WindowOptions, component::TitleBar,
};
use gpui_pre::{Bounds, WindowBounds, px, size};
use kerminal_application::{
    external_contributions::PreparedPlugin,
    preferences::ThemeMode,
    workspace::{WorkspaceSessionLoad, WorkspaceSessionTab},
};
use kerminal_domain::{identity::TabId, plugins::PluginId};
use kerminal_plugin_protocol::PluginRuntime;
use kerminal_ui::{AppServices, QuitIntent, RootView, RootViewEvent};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    ffi::OsString,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const CONTENT: &str = "acme.notes/notes-editor";
const DRAFT: &str = "独立插件草稿-A";
const SAVED: &str = "独立插件保存-B";
const RESTART_DRAFT: &str = "独立插件重启草稿-A";
const TITLE: &str = "Kerminal External Plugin Acceptance";
const TIMEOUT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(120);

mod window;
use window::run_window;

#[derive(Clone)]
struct Options {
    output: PathBuf,
    plugin: PathBuf,
    phase: String,
    theme: String,
    width: u32,
    height: u32,
}
struct UiResult {
    report: Value,
    deadline: Instant,
}

/// 先取得独立根的真实单实例身份，再走启动快照、冻结目录和正式运行服务装配。
/// 成功报告只在界面退出、运行服务关闭和磁盘回读全部完成后提交；原生宿主草稿与插件 opaque 状态分别核对。
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_options(std::env::args_os().skip(2))?;
    let prepare = options.phase == "prepare";
    if prepare {
        fs::create_dir(&options.output)?;
        fs::create_dir(options.output.join("config"))?;
    }
    let output = options.output.canonicalize()?;
    let config = output.join("config").canonicalize()?;
    if !config.starts_with(&output) {
        return Err(io::Error::other("验收配置根越界").into());
    }
    let candidate_hash = digest(&fs::read(std::env::current_exe()?)?);
    let runtime = ApplicationRuntime::new()?;
    let primary =
        match crate::acquire_single_instance(&runtime, &crate::single_instance_id(&config)?)? {
            single_application::SingleApplication::PrimaryInstance(primary) => primary,
            single_application::SingleApplication::SecondaryInstance(_) => {
                return Err(io::Error::other("隔离配置根已有运行进程").into());
            }
        };
    let startup = Arc::new(StartupPlugins::load(
        &config,
        PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(options.plugin.canonicalize()?),
        },
    )?);
    let prepared = startup
        .enabled
        .iter()
        .find(|plugin| plugin.id.as_str() == "acme.notes")
        .cloned()
        .ok_or_else(|| io::Error::other("开发目录未提供 acme.notes"))?;
    if startup.enabled.len() != 1 {
        return Err(io::Error::other("验收只允许一个明确开发插件").into());
    }
    let plugin_hash = frozen_digest(&prepared)?;
    if !prepare {
        let receipt: Value =
            serde_json::from_slice(&fs::read(output.join("prepare-report.json"))?)?;
        if receipt["candidateSha256"] != candidate_hash
            || receipt["pluginSha256"] != plugin_hash
            || receipt["processId"] == std::process::id()
            || receipt["shutdownComplete"] != true
            || receipt["theme"] != options.theme
        {
            return Err(io::Error::other("重启验收候选、插件或准备回执不一致").into());
        }
    }
    let catalogs = startup.catalogs("zh-CN")?;
    let migration = bootstrap::migration_service(&config, None, catalogs.1.clone());
    bootstrap::run_after_migration(&migration, false, |_| ())
        .inspect_err(report_migration_io_error)?;
    let mut composition = runtime.block_on(DesktopComposition::build_with_plugin_catalogs(
        runtime.handle(),
        &config,
        None,
        catalogs.0,
        catalogs.1,
    ))?;
    let install = runtime.block_on(composition.install_external_plugins(startup.clone()));
    if let Err(error) = install {
        let _ = runtime.block_on(composition.shutdown_until(tokio::time::Instant::now() + TIMEOUT));
        drop(primary);
        runtime.shutdown();
        return Err(error.into());
    }
    let services = composition.app_services();
    let ui = catch_unwind(AssertUnwindSafe(|| {
        run_window(services.clone(), startup, &options, &output)
    }))
    .unwrap_or_else(|_| Err(io::Error::other("外部插件验收事件循环异常").into()));
    let disk = runtime.block_on(read_disk(&services, &prepared));
    let shutdown_deadline = ui
        .as_ref()
        .map(|result| result.deadline)
        .unwrap_or_else(|_| Instant::now() + Duration::from_secs(12));
    let shutdown = runtime.block_on(composition.shutdown_until(shutdown_deadline.into()));
    drop(primary);
    runtime.shutdown();
    let mut ui = ui?;
    shutdown?;
    let disk = disk?;
    if disk["state"]["text"] != DRAFT
        || disk["settings"]["showPreview"] != false
        || !disk["tabs"].as_array().is_some_and(|tabs| {
            tabs.iter().any(|tab| {
                tab["stateVersion"] == 0
                    && tab["state"]["format"] == "kerminal.native-content"
                    && tab["state"]["schema"] == 1
                    && tab["state"]["view"] == "notes.editor"
                    && tab["state"]["drafts"].as_array().is_some_and(|drafts| {
                        drafts
                            .iter()
                            .any(|draft| draft["path"] == "text" && draft["value"] == RESTART_DRAFT)
                    })
            })
        })
    {
        return Err(
            io::Error::other(format!("插件磁盘回读未确认保存、设置和重启草稿：{disk}")).into(),
        );
    }
    ui.report["candidateSha256"] = json!(candidate_hash);
    ui.report["pluginSha256"] = json!(plugin_hash);
    ui.report["pluginVersion"] = json!(prepared.version.to_string());
    ui.report["configDirectory"] = json!(config);
    ui.report["disk"] = disk;
    ui.report["shutdownComplete"] = json!(true);
    fs::write(
        output.join(format!("{}-report.json", options.phase)),
        serde_json::to_vec_pretty(&ui.report)?,
    )?;
    Ok(())
}

/// 启动迁移早于日志初始化，仅在验收 stderr 输出错误链中的系统分类和错误码。
/// 不展开路径或数据，遍历设上限避免异常错误链拖住验收；原错误与恢复语义保持不变。
fn report_migration_io_error(error: &kerminal_application::AppError) {
    let mut source: Option<&(dyn Error + 'static)> = Some(error);
    for depth in 0..16 {
        let Some(current) = source else { break };
        if let Some(error) = current.downcast_ref::<io::Error>() {
            eprintln!(
                "外部插件验收迁移系统错误：source_depth={depth} kind={:?} raw_os_error={:?}",
                error.kind(),
                error.raw_os_error()
            );
        }
        source = current.source();
    }
}

/// 只接受已知且不重复的参数，输出根必须由调用方明确给出，不能回退用户目录。
fn parse_options(arguments: impl IntoIterator<Item = OsString>) -> Result<Options, io::Error> {
    let mut values = std::collections::BTreeMap::new();
    let mut arguments = arguments.into_iter();
    while let Some(key) = arguments.next() {
        let key = key
            .into_string()
            .map_err(|_| io::Error::other("验收选项必须是UTF-8"))?;
        if ![
            "--output-dir",
            "--dev-plugin",
            "--phase",
            "--theme",
            "--window-width",
            "--window-height",
        ]
        .contains(&key.as_str())
        {
            return Err(io::Error::other("未知外部插件验收选项"));
        }
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other("验收选项缺少值"))?;
        if values.insert(key, value).is_some() {
            return Err(io::Error::other("验收选项不能重复"));
        }
    }
    let output = values
        .remove("--output-dir")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("缺少输出根"))?;
    let plugin = values
        .remove("--dev-plugin")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("缺少开发插件目录"))?;
    let phase = values
        .remove("--phase")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    let theme = values
        .remove("--theme")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "system".into());
    if !output.is_absolute()
        || !plugin.is_absolute()
        || !["prepare", "verify"].contains(&phase.as_str())
        || !["light", "dark", "system"].contains(&theme.as_str())
    {
        return Err(io::Error::other("验收路径、阶段或主题无效"));
    }
    let width = dimension(values.remove("--window-width"), 1440, 800, 3840)?;
    let height = dimension(values.remove("--window-height"), 900, 600, 2160)?;
    Ok(Options {
        output,
        plugin,
        phase,
        theme,
        width,
        height,
    })
}

/// 尺寸使用与私有桌面启动器一致的逻辑像素范围，拒绝无效值而不静默截断。
fn dimension(value: Option<OsString>, default: u32, min: u32, max: u32) -> Result<u32, io::Error> {
    let value = match value {
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::other("窗口尺寸无效"))?,
        None => default,
    };
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(io::Error::other("窗口尺寸超限"))
    }
}

/// 摘要绑定真实字节，使两个进程使用同一候选的证明不依赖文件名或版本文案。
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 冻结资源与实际脚本一起绑定，源码变化不能让重启验收跨过不同插件候选。
fn frozen_digest(prepared: &PreparedPlugin) -> Result<String, Box<dyn Error>> {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&prepared.package.manifest)?);
    for (path, bytes) in &prepared.resources {
        digest.update((path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update(bytes.as_ref());
    }
    if let PluginRuntime::Script { main, .. } = &prepared.package.manifest.runtime {
        digest.update(fs::read(prepared.directory.join(main))?);
    } else {
        return Err(io::Error::other("笔记验收只支持真实脚本宿主").into());
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// 读取真实应用端口，并与生产设置页共用已校验 schema；schema 不属于网页资源表。
/// 不从插件界面或验收期望值构造落盘结果，原保存、设置和草稿断言保持独立回读。
async fn read_disk(
    services: &AppServices,
    prepared: &PreparedPlugin,
) -> Result<Value, Box<dyn Error>> {
    let service = services
        .external_plugin_settings
        .as_ref()
        .ok_or_else(|| io::Error::other("插件设置服务未装配"))?;
    let state = service
        .state_get(&prepared.id, CancellationToken::new())
        .await?;
    let contribution = prepared
        .package
        .manifest
        .contributes
        .settings
        .as_ref()
        .ok_or_else(|| io::Error::other("笔记设置贡献缺失"))?;
    let schema = prepared
        .package
        .configuration_schemas
        .get(&contribution.schema)
        .cloned()
        .ok_or_else(|| io::Error::other("已校验设置schema缺失"))?;
    let settings = service
        .load(&prepared.id, &schema, CancellationToken::new())
        .await?;
    let workspace = services
        .workspace_sessions
        .as_ref()
        .ok_or_else(|| io::Error::other("工作区服务未装配"))?
        .load(CancellationToken::new())
        .await?;
    let WorkspaceSessionLoad::Loaded { document, .. } = workspace else {
        return Err(io::Error::other("工作区未持久化").into());
    };
    let tabs = document
        .workspace_snapshot()
        .map_err(|reason| io::Error::other(format!("工作区快照无效：{reason:?}")))?
        .terminal_tabs
        .into_iter()
        .filter_map(|tab| match tab {
            WorkspaceSessionTab::Extension {
                id,
                content_id,
                state_version,
                state,
                ..
            } if content_id == CONTENT => {
                Some(json!({"id":id.as_str(),"stateVersion":state_version,"state":state}))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    Ok(json!({"state":state,"settings":settings.values,"tabs":tabs}))
}
