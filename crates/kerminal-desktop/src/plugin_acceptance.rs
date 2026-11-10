//! 原生验收专用的静态插件；生产默认构建不注册或加载这些实现。
//!
//! @author kongweiguang

use std::{
    error::Error,
    ffi::OsStr,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gpui_kit::{
    AnyView, AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, Focusable,
    InputEvent, IntoElement, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Render, SharedString, Subscription, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowOptions,
    base::TestSupportExt as _,
    component::{
        TitleBar,
        input::{Input, InputEvent as ComponentInputEvent, InputState},
    },
    div,
    prelude::*,
};
use gpui_pre::{Bounds, ElementId, Pixels, Point, WindowBounds, px, size};
use kerminal_application::{
    AppError, AppResult,
    connections::host_types::{
        HostConfigurationCatalog, HostConfigurationCodec, HostConfigurationType, HostTypeProvider,
        PluginHost, PluginHostDraft, PluginHostOpenIntent,
    },
    plugins::{
        ContentDescriptor, ContentTypeId, HostTypeDescriptor, HostTypeId, PluginDescriptor,
        PluginId, PluginRegistry, PluginRegistryBuilder, ToolDescriptor, ToolId,
    },
};
use kerminal_domain::identity::{HostId, TabId};
use kerminal_infrastructure::configuration::host_configuration_catalog;
use kerminal_ui::plugins::{
    content::{
        ContentCloseFuture, ContentFactory, ContentInstance, ContentInstanceIdentity, ContentKind,
        ContentSavedState, ContentScope, ContentStateChanged,
    },
    host_forms::{HostFormFactory, HostFormInstance, HostFormSeed},
    tools::{ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPresentation, ToolScope},
};
use kerminal_ui::{AppServices, QuitIntent, RootView, RootViewEvent};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    bootstrap,
    composition::{DesktopComposition, PluginFactories},
    runtime::ApplicationRuntime,
};

const PROVIDER_ID: &str = "test.provider";
const HOST_TYPE_ID: &str = "test.host";
const CONTENT_ID: &str = "test.content";
const TOOL_ID: &str = "test.tool";
const HOST_NAME: &str = "Plugin acceptance host";
const INITIAL_TEXT: &str = "initial-plugin-value";
const HOST_EDITED_TEXT: &str = "host-edited-plugin-value";
const EDITED_TEXT: &str = "edited-plugin-value";
const READY_TIMEOUT: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);

#[cfg(target_os = "linux")]
mod x11_isolation;

#[derive(Clone, Default)]
struct FixtureObservation {
    rendered_text: Arc<Mutex<Option<String>>>,
}

/// 声明与实现一次性冻结，确保配置首次扫描已经能识别同一个验收插件。
///
/// @author kongweiguang
fn catalogs() -> Result<(Arc<PluginRegistry>, Arc<HostConfigurationCatalog>), Box<dyn Error>> {
    let provider = PluginId::new(PROVIDER_ID)?;
    let content = ContentTypeId::new(CONTENT_ID)?;
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins()?;
    builder.register_provider(PluginDescriptor::new(
        provider.clone(),
        "Test provider",
        true,
    ));
    builder.register_host_type(HostTypeDescriptor::new(
        HostTypeId::new(HOST_TYPE_ID)?,
        provider.clone(),
        HOST_TYPE_ID,
        "Test host",
        Some(content.clone()),
    ));
    builder.register_content_type(ContentDescriptor::new(
        content.clone(),
        provider.clone(),
        "Test content",
    ));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new(TOOL_ID)?,
        provider,
        "Test tool",
        Some(content),
    ));
    let hosts = host_configuration_catalog(vec![
        HostConfigurationType::new(PROVIDER_ID, HOST_TYPE_ID, Arc::new(FixtureCodec))?
            .with_provider(Arc::new(FixtureProvider)),
    ])?;
    Ok((Arc::new(builder.freeze()?), hosts))
}

struct FixtureCodec;

impl HostConfigurationCodec for FixtureCodec {
    /// 验收配置只接受有界文本和首版格式，确保读取不是无条件放行的占位 codec。
    ///
    /// @author kongweiguang
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        let text = configuration
            .as_table()
            .and_then(|table| table.get("text"))
            .and_then(toml::Value::as_str)
            .filter(|text| text.len() <= 1024 && !text.chars().any(char::is_control));
        if version != 1 || text.is_none() {
            return Err(AppError::invalid("验收主机配置无效"));
        }
        Ok(configuration.clone())
    }

    /// 编码和解码采用同一约束，避免测试数据绕过正式保存门禁。
    ///
    /// @author kongweiguang
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        self.decode(version, configuration)
    }
}

struct FixtureProvider;

#[async_trait]
impl HostTypeProvider for FixtureProvider {
    /// 验收提供方不创建外部连接，但仍核对取消及已验证配置。
    ///
    /// @author kongweiguang
    async fn test(&self, host: &PluginHost, cancel: CancellationToken) -> AppResult<()> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("验收主机测试已取消"));
        }
        let configuration = host
            .configuration
            .as_ref()
            .ok_or_else(|| AppError::invalid("验收主机配置不可用"))?;
        FixtureCodec.decode(host.configuration_version, configuration)?;
        Ok(())
    }

    /// 打开意图只携带无秘密的已验证状态，真正的标签仍由宿主创建和保存。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        host: &PluginHost,
        cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        self.test(host, cancel).await?;
        let text = host
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.get("text"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AppError::invalid("验收主机缺少文本"))?;
        Ok(PluginHostOpenIntent::Content {
            provider_id: PROVIDER_ID.into(),
            content_type_id: CONTENT_ID.into(),
            title: host.name.clone(),
            state_version: 1,
            state: json!({ "text": text }),
        })
    }
}

struct FixtureHostFormFactory;

impl HostFormFactory for FixtureHostFormFactory {
    /// 表单身份与冻结的 codec 归属一致，避免编辑器接管别的提供方。
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    /// 类型身份始终使用配置中的稳定标识，不依赖可见名称。
    fn type_id(&self) -> &str {
        HOST_TYPE_ID
    }

    /// 此名称只出现在显式开启的验收插件协议栏。
    fn display_name(&self) -> SharedString {
        "Test host".into()
    }

    /// 新建与编辑分别使用空草稿和真实存储投影，并在窗口内保留输入实体。
    fn create(
        &self,
        seed: &HostFormSeed,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn HostFormInstance>> {
        let initial = match &seed.original {
            Some(host) => host
                .configuration
                .as_ref()
                .and_then(|configuration| configuration.get("text"))
                .and_then(toml::Value::as_str)
                .ok_or_else(|| AppError::invalid("验收主机缺少已保存文本"))?
                .to_owned(),
            None => String::new(),
        };
        let original = seed.original.clone();
        let default_group = seed.default_group.clone();
        let view = cx.new(|cx| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(initial.clone()));
            let subscription = cx.subscribe(&input, |_, _, event: &ComponentInputEvent, cx| {
                if matches!(event, ComponentInputEvent::Change) {
                    cx.notify();
                }
            });
            FixtureHostFormView {
                input,
                enabled: true,
                _subscription: subscription,
            }
        });
        Ok(Box::new(FixtureHostForm {
            view,
            original,
            default_group,
            initial,
        }))
    }
}

struct FixtureHostFormView {
    input: Entity<InputState>,
    enabled: bool,
    _subscription: Subscription,
}

impl Render for FixtureHostFormView {
    /// 始终复用同一 Kit 输入实体，让冻结和异步保存不会替换用户草稿。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w_full().p_4().child(
            Input::new(&self.input)
                .id("plugin-acceptance-host-input")
                .disabled(!self.enabled)
                .w_full(),
        )
    }
}

struct FixtureHostForm {
    view: Entity<FixtureHostFormView>,
    original: Option<PluginHost>,
    default_group: Option<String>,
    initial: String,
}

impl HostFormInstance for FixtureHostForm {
    /// 宿主只持有可复用的视图句柄，不直接操作插件字段。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 编辑时保留原主机身份与公共字段，最终修订号仍由宿主负责比较。
    fn snapshot(&self, cx: &App) -> AppResult<PluginHostDraft> {
        let mut configuration = toml::map::Map::new();
        configuration.insert(
            "text".into(),
            toml::Value::String(self.view.read(cx).input.read(cx).value().to_string()),
        );
        Ok(PluginHostDraft {
            id: self.original.as_ref().map(|host| host.id.clone()),
            provider_id: PROVIDER_ID.into(),
            type_id: HOST_TYPE_ID.into(),
            name: self
                .original
                .as_ref()
                .map_or_else(|| HOST_NAME.into(), |host| host.name.clone()),
            group_id: self
                .original
                .as_ref()
                .map_or_else(|| self.default_group.clone(), |host| host.group_id.clone()),
            sort_order: self.original.as_ref().map_or(0, |host| host.sort_order),
            configuration_version: 1,
            configuration: toml::Value::Table(configuration),
        })
    }

    /// 与进入表单时的快照比较，确保未保存编辑参加宿主的关闭门禁。
    fn is_dirty(&self, cx: &App) -> bool {
        self.view.read(cx).input.read(cx).value().as_ref() != self.initial.as_str()
    }

    /// 保存期间通过控件禁用状态冻结输入，避免回执覆盖新的键盘编辑。
    fn set_enabled(&mut self, enabled: bool, _: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, cx| {
            view.enabled = enabled;
            cx.notify();
        });
    }

    /// 仅在宿主显式请求时恢复焦点，普通重绘不会干扰其他控件。
    fn focus(&mut self, window: &mut Window, cx: &mut App) {
        let focus = self.view.read(cx).input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }
}

struct FixtureContentFactory {
    observation: FixtureObservation,
}

impl ContentFactory for FixtureContentFactory {
    /// 身份与冻结声明一致，使冷启动恢复走正常工厂查找。
    ///
    /// @author kongweiguang
    fn content_id(&self) -> &str {
        CONTENT_ID
    }

    /// 样例只用于工作区标签，避免把可恢复状态误当成单例页面。
    ///
    /// @author kongweiguang
    fn supports(&self, kind: ContentKind) -> bool {
        kind == ContentKind::Tab
    }

    /// 新建真实输入实体并隔离渲染证据；恢复时拒绝不支持的状态版本。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        _: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        let text = state
            .filter(|saved| saved.version == 1)
            .and_then(|saved| saved.value.get("text"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::invalid("验收内容状态无效"))?
            .to_owned();
        let observation = self.observation.clone();
        *observation
            .rendered_text
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let view = cx.new(|cx| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
            let input_subscription = cx.subscribe_in(
                &input,
                window,
                |view: &mut FixtureContentView, _, event: &ComponentInputEvent, window, cx| {
                    if matches!(event, ComponentInputEvent::Change) {
                        if let Some(identity) = &view.identity {
                            window.dispatch_action(
                                Box::new(ContentStateChanged {
                                    identity: identity.clone(),
                                }),
                                cx,
                            );
                        }
                        cx.notify();
                    }
                },
            );
            FixtureContentView {
                input,
                observation,
                identity: None,
                _input_subscription: input_subscription,
            }
        });
        Ok(Box::new(FixtureContent { view }))
    }

    /// 使用宿主分配的代次上报变更，避免已关闭实例的事件覆盖后续标签。
    ///
    /// @author kongweiguang
    fn create_with_identity(
        &self,
        identity: &ContentInstanceIdentity,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        let instance = self.create(identity.scope(), state, window, cx)?;
        let view = instance
            .view()
            .downcast::<FixtureContentView>()
            .map_err(|_| AppError::invalid("验收内容视图类型无效"))?;
        view.update(cx, |view, _| view.identity = Some(identity.clone()));
        Ok(instance)
    }
}

struct FixtureContentView {
    input: Entity<InputState>,
    observation: FixtureObservation,
    identity: Option<ContentInstanceIdentity>,
    _input_subscription: Subscription,
}

impl Render for FixtureContentView {
    /// 使用 Kit 输入组件，以实际键盘编辑覆盖内容实体的状态快照。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        *self
            .observation
            .rendered_text
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(self.input.read(cx).value().to_string());
        div()
            .id("plugin-acceptance-content")
            .size_full()
            .p_4()
            .child(
                Input::new(&self.input)
                    .id("plugin-acceptance-input")
                    .w_full(),
            )
    }
}

struct FixtureContent {
    view: Entity<FixtureContentView>,
}

impl ContentInstance for FixtureContent {
    /// 保存稳定实体身份，避免切换标签时重建输入草稿。
    ///
    /// @author kongweiguang
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 状态由真实输入实体提供，宿主的工作区 CAS 负责持久化。
    ///
    /// @author kongweiguang
    fn snapshot(&self, cx: &App) -> AppResult<ContentSavedState> {
        let input = &self.view.read(cx).input;
        Ok(ContentSavedState {
            version: 1,
            value: json!({ "text": input.read(cx).value().to_string() }),
        })
    }

    /// 样例不拥有后台资源；关闭回执仍经过宿主生命周期门禁。
    ///
    /// @author kongweiguang
    fn begin_close(&mut self, _: Instant, _: &mut App) -> AppResult<ContentCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

struct FixtureToolFactory;

impl ToolFactory for FixtureToolFactory {
    /// 工具身份与静态声明对应，供宿主按用户配置定位。
    ///
    /// @author kongweiguang
    fn tool_id(&self) -> &str {
        TOOL_ID
    }

    /// 使用已安装的 Kit 图标和固定文案，不引入外部资源。
    ///
    /// @author kongweiguang
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new(
            "Test tool",
            "Plugin acceptance",
            gpui_kit::assets::IconName::Puzzle,
        )
    }

    /// 工具实例归属于窗口，切换标签时继续保持同一实体。
    ///
    /// @author kongweiguang
    fn scope(&self) -> ToolScope {
        ToolScope::Window
    }

    /// 构造实际可渲染视图，避免以元数据冒充已装配的工具。
    ///
    /// @author kongweiguang
    fn create(
        &self,
        _: &ToolContext,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        Ok(Box::new(FixtureTool {
            view: cx.new(|_| FixtureToolView),
        }))
    }
}

struct FixtureToolView;

impl Render for FixtureToolView {
    /// 可观察的真实内容使工具装配验收能够区分工厂与空白占位。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("plugin-acceptance-tool")
            .p_4()
            .child("Test tool")
            .test_support()
    }
}

struct FixtureTool {
    view: Entity<FixtureToolView>,
}

impl ToolInstance for FixtureTool {
    /// 同一窗口复用视图实体，生命周期由工具宿主管理。
    ///
    /// @author kongweiguang
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 样例没有外部资源，但仍以正式异步回执完成关闭。
    ///
    /// @author kongweiguang
    fn begin_close(&mut self, _: Instant, _: &mut App) -> AppResult<ToolCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Prepare,
    Verify,
}

struct Options {
    output: PathBuf,
    phase: Phase,
    width: u32,
    height: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    candidate_sha256: String,
    prepare_process_id: u32,
    host_id: String,
    initial_text: String,
    host_edited_text: String,
    edited_text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseReport {
    phase: &'static str,
    process_id: u32,
    candidate_sha256: String,
    host_id: String,
    host_form_created: bool,
    host_form_edited: bool,
    host_configuration_text: String,
    rendered_text: String,
    native_isolation: serde_json::Value,
    capture: String,
    commands_mounting: Vec<serde_json::Value>,
    requested_viewport: [u32; 2],
    viewport: [f32; 2],
    scale_factor: f32,
}

/// 验收只接受显式阶段和全新输出根，恢复阶段必须读取前一进程的已完成回执。
///
/// @author kongweiguang
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_options(std::env::args_os().skip(2))?;
    let output = resolve_output(&options.output, options.phase)?;
    let config = output.join("config");
    let candidate_sha256 = format!("{:x}", Sha256::digest(fs::read(std::env::current_exe()?)?));
    let expected_host = if options.phase == Phase::Prepare {
        fs::create_dir(&config)?;
        None
    } else {
        let receipt: Receipt = serde_json::from_slice(&fs::read(output.join("prepare.json"))?)?;
        if receipt.candidate_sha256 != candidate_sha256
            || receipt.prepare_process_id == std::process::id()
            || receipt.initial_text != INITIAL_TEXT
            || receipt.host_edited_text != HOST_EDITED_TEXT
            || receipt.edited_text != EDITED_TEXT
            || !fs::canonicalize(&config)?.starts_with(&output)
        {
            return Err(io::Error::other("插件验收准备回执与当前进程不匹配").into());
        }
        Some(HostId::parse(&receipt.host_id)?)
    };

    let (plugins, hosts) = catalogs()?;
    let migration = bootstrap::migration_service(&config, None, hosts.clone());
    bootstrap::run_after_migration(&migration, false, |_| ())?;
    let runtime = ApplicationRuntime::new()?;
    let observation = FixtureObservation::default();
    let composition = runtime.block_on(DesktopComposition::build_with_plugin_factories(
        runtime.handle(),
        &config,
        None,
        plugins,
        hosts,
        PluginFactories {
            content: vec![Arc::new(FixtureContentFactory {
                observation: observation.clone(),
            })],
            tools: vec![Arc::new(FixtureToolFactory)],
            host_forms: vec![Arc::new(FixtureHostFormFactory)],
        },
    ));
    let mut composition = match composition {
        Ok(composition) => composition,
        Err(error) => {
            runtime.shutdown();
            return Err(error.into());
        }
    };
    let services = composition.app_services();
    let setup = match expected_host.as_ref() {
        Some(host_id) => runtime.block_on(verify_persisted_host(&services, host_id)),
        None => Ok(()),
    };
    let ui_result = setup.and_then(|()| {
        catch_unwind(AssertUnwindSafe(|| {
            run_hidden_window(
                services,
                observation,
                options.phase,
                output.clone(),
                options.width,
                options.height,
                candidate_sha256.clone(),
            )
        }))
        .unwrap_or_else(|_| Err(io::Error::other("插件验收事件循环异常终止").into()))
    });
    let shutdown_deadline = ui_result
        .as_ref()
        .copied()
        .map(tokio::time::Instant::from_std)
        .unwrap_or_else(|_| tokio::time::Instant::now() + SHUTDOWN_TIMEOUT);
    let shutdown = runtime.block_on(composition.shutdown_until(shutdown_deadline));
    runtime.shutdown();
    ui_result?;
    shutdown?;
    if options.phase == Phase::Prepare {
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("prepare-report.json"))?)?;
        let host_id = report["hostId"]
            .as_str()
            .ok_or_else(|| io::Error::other("插件主机验收报告缺少身份"))?;
        HostId::parse(host_id)?;
        let receipt = Receipt {
            candidate_sha256,
            prepare_process_id: std::process::id(),
            host_id: host_id.into(),
            initial_text: INITIAL_TEXT.into(),
            host_edited_text: HOST_EDITED_TEXT.into(),
            edited_text: EDITED_TEXT.into(),
        };
        fs::write(
            output.join("prepare.json"),
            serde_json::to_vec_pretty(&receipt)?,
        )?;
    }
    Ok(())
}

/// 对重复、缺失和未知参数直接报错，避免拼错阶段时误用其它数据目录。
///
/// @author kongweiguang
fn parse_options(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Options, io::Error> {
    let mut arguments = arguments.into_iter();
    let mut output = None;
    let mut phase = None;
    let mut width = None;
    let mut height = None;
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other("插件验收参数缺少值"))?;
        if flag == OsStr::new("--output-dir") && output.is_none() {
            output = Some(PathBuf::from(value));
        } else if flag == OsStr::new("--phase") && phase.is_none() {
            phase = Some(if value == "prepare" {
                Phase::Prepare
            } else if value == "verify" {
                Phase::Verify
            } else {
                return Err(io::Error::other("插件验收阶段必须是 prepare 或 verify"));
            });
        } else if (flag == OsStr::new("--width") && width.is_none())
            || (flag == OsStr::new("--height") && height.is_none())
        {
            let dimension = value
                .to_str()
                .and_then(|text| text.parse::<u32>().ok())
                .ok_or_else(|| io::Error::other("插件验收窗口尺寸无效"))?;
            if flag == OsStr::new("--width") && (800..=3840).contains(&dimension) {
                width = Some(dimension);
            } else if flag == OsStr::new("--height") && (600..=2160).contains(&dimension) {
                height = Some(dimension);
            } else {
                return Err(io::Error::other("插件验收窗口尺寸超出允许范围"));
            }
        } else {
            return Err(io::Error::other("插件验收含重复或未知参数"));
        }
    }
    Ok(Options {
        output: output.ok_or_else(|| io::Error::other("插件验收缺少输出目录"))?,
        phase: phase.ok_or_else(|| io::Error::other("插件验收缺少阶段"))?,
        width: width.unwrap_or(1440),
        height: height.unwrap_or(900),
    })
}

/// 数据只允许放在仓库专用验收目录；prepare 不覆盖已有结果，verify 检查真实路径。
///
/// @author kongweiguang
fn resolve_output(requested: &Path, phase: Phase) -> Result<PathBuf, io::Error> {
    let base = std::env::current_dir()?
        .join("target")
        .join("acceptance")
        .join("plugin");
    fs::create_dir_all(&base)?;
    let base = fs::canonicalize(base)?;
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()?.join(requested)
    };
    if phase == Phase::Prepare {
        let parent = requested
            .parent()
            .ok_or_else(|| io::Error::other("插件验收输出缺少父目录"))?;
        let parent = fs::canonicalize(parent)?;
        if !parent.starts_with(&base) {
            return Err(io::Error::other(
                "插件验收输出必须位于 target/acceptance/plugin",
            ));
        }
        let name = requested
            .file_name()
            .ok_or_else(|| io::Error::other("插件验收输出缺少目录名"))?;
        let output = parent.join(name);
        fs::create_dir(&output)?;
        fs::canonicalize(output)
    } else {
        let output = fs::canonicalize(requested)?;
        if output == base || !output.starts_with(&base) || !output.is_dir() {
            return Err(io::Error::other("插件验收恢复目录不在专用输出根内"));
        }
        Ok(output)
    }
}

/// 恢复阶段按上一进程回执只读验证主机与运行提供方，不创建或修改配置。
///
/// @author kongweiguang
async fn verify_persisted_host(services: &AppServices, id: &HostId) -> Result<(), Box<dyn Error>> {
    let hosts = services
        .hosts
        .as_ref()
        .ok_or_else(|| io::Error::other("主机服务不可用"))?;
    let host = hosts
        .load_plugin_host(id)
        .await?
        .ok_or_else(|| io::Error::other("验收主机保存后不存在"))?;
    if !host.is_runnable()
        || host.provider_id != PROVIDER_ID
        || host.type_id != HOST_TYPE_ID
        || host.name != HOST_NAME
        || host
            .configuration
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(toml::Value::as_str)
            != Some(HOST_EDITED_TEXT)
    {
        return Err(io::Error::other("验收主机读回与保存值不一致").into());
    }
    services
        .plugin_hosts
        .as_ref()
        .ok_or_else(|| io::Error::other("插件主机服务未注入"))?
        .test(id, CancellationToken::new())
        .await?;
    Ok(())
}

/// 原生窗口的操作与回读共用 UI 线程，退出时返回 Root 分配的同一截止时间。
///
/// @author kongweiguang
fn run_hidden_window(
    services: AppServices,
    observation: FixtureObservation,
    phase: Phase,
    output: PathBuf,
    width: u32,
    height: u32,
    candidate_sha256: String,
) -> Result<Instant, Box<dyn Error>> {
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = failure.clone();
    let shutdown_deadline = Arc::new(Mutex::new(None::<Instant>));
    let deadline_for_app = shutdown_deadline.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let Some(display) = cx.primary_display() else {
                *failure_for_app
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some("没有可用显示设备".into());
                cx.quit();
                return;
            };
            let bounds = Bounds::centered_at(
                display.visible_bounds().center(),
                size(px(width as f32), px(height as f32)),
            );
            let options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(width as f32), px(height as f32))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal Plugin Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(options, cx, |window, cx| {
                window.resize(size(px(width as f32), px(height as f32)));
                let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
                cx.subscribe(&root, move |_, event: &RootViewEvent, _| {
                    if let RootViewEvent::QuitReady { deadline, .. } = event {
                        *deadline_for_app
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(*deadline);
                    }
                })
                .detach();
                let handle = window.window_handle();
                let root_for_task = root.clone();
                let failure_for_task = failure_for_app.clone();
                cx.spawn(async move |cx| {
                    let result = exercise_window(
                        cx,
                        &root_for_task,
                        handle,
                        &services,
                        &observation,
                        phase,
                        &output,
                        &candidate_sha256,
                        [width, height],
                    )
                    .await;
                    if let Err(error) = result {
                        let _ = capture_frame(cx, handle, &output.join("failure.png")).await;
                        *failure_for_task
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
                    }
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = result {
                *failure_for_app
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
                cx.quit();
            }
        });
    let failure = failure
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    let shutdown_deadline = shutdown_deadline
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    match failure {
        Some(message) => Err(io::Error::other(message).into()),
        None => {
            shutdown_deadline.ok_or_else(|| io::Error::other("未收到插件验收退出截止时间").into())
        }
    }
}

/// 使用真实控件打开、编辑、保存和关闭一次，再保留第二个标签验证冷启动恢复。
/// 先证明桌面隔离再投影活动态；私有桌面没有 OS 激活事件，其他生产门禁仍保持有效。
/// 工作区恢复后按请求尺寸初始化原生表面，不能把启动时恢复的最大化尺寸当作窄窗验收。
///
/// @author kongweiguang
async fn exercise_window(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &AppServices,
    observation: &FixtureObservation,
    phase: Phase,
    output: &Path,
    candidate_sha256: &str,
    requested_viewport: [u32; 2],
) -> Result<(), Box<dyn Error>> {
    let isolation = window.update(cx, |_, window, _| checked_isolation(window))??;
    eprintln!("Plugin acceptance isolation: {isolation}");
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.enable_right_rail_acceptance_activity(cx);
        });
    })?;
    wait_snapshot(cx, root, window, "工作区初始化", |snapshot| {
        snapshot["workspaceReady"] == true
    })
    .await?;
    #[cfg(windows)]
    {
        crate::settings_acceptance::prepare_capture_surface_for_title(
            cx,
            window,
            requested_viewport[0],
            requested_viewport[1],
            "Kerminal Plugin Acceptance",
        )
        .await?;
    }
    window.update(cx, |_, window, _| {
        viewport_evidence(window, requested_viewport)
    })??;
    let host = if phase == Phase::Prepare {
        exercise_host_forms(cx, window, services).await?
    } else {
        wait_fixture_host(cx, window, services, HOST_EDITED_TEXT).await?
    };
    if phase == Phase::Prepare {
        open_fixture_host(cx, window, &host.id).await?;
        let first = wait_content(cx, root, window, HOST_EDITED_TEXT).await?;
        edit_input(cx, window, "plugin-acceptance-input", EDITED_TEXT)?;
        wait_content(cx, root, window, EDITED_TEXT).await?;
        wait_persisted_content(cx, window, services, &first).await?;
        wait_selector(cx, window, "workbench-tool-plugin-test.tool").await?;
        click_selector(cx, window, "workbench-tool-plugin-test.tool", false)?;
        wait_selector(cx, window, "plugin-acceptance-tool").await?;
        click_selector(cx, window, &format!("workspace-tab-close-{first}"), false)?;
        wait_snapshot(cx, root, window, "关闭插件标签", |snapshot| {
            snapshot["tabs"]
                .as_array()
                .is_some_and(|tabs| !tabs.iter().any(|tab| tab["tabId"] == first))
                && snapshot["instanceCount"] == 0
        })
        .await?;
        open_fixture_host(cx, window, &host.id).await?;
        let second = wait_content(cx, root, window, HOST_EDITED_TEXT).await?;
        if first == second {
            return Err(io::Error::other("重开标签复用了已关闭的身份").into());
        }
        edit_input(cx, window, "plugin-acceptance-input", EDITED_TEXT)?;
        wait_content(cx, root, window, EDITED_TEXT).await?;
        wait_persisted_content(cx, window, services, &second).await?;
    } else {
        wait_content(cx, root, window, EDITED_TEXT).await?;
    }
    let commands_mounting = if phase == Phase::Prepare {
        exercise_commands_mounting(cx, root, window, output).await?
    } else {
        Vec::new()
    };
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            Ok::<_, io::Error>(())
        })??;
        let observed = observation
            .rendered_text
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if observed.as_deref() == Some(EDITED_TEXT) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("渲染视图未显示已编辑内容").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    let (name, capture) = if phase == Phase::Prepare {
        ("prepare", "prepare.png")
    } else {
        ("verify", "verify.png")
    };
    let (viewport, scale_factor) = window.update(cx, |_, window, _| {
        viewport_evidence(window, requested_viewport)
    })??;
    capture_frame(cx, window, &output.join(capture)).await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.request_quit(QuitIntent::Exit, cx);
        })
    })?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let ready = window
            .update(cx, |_, window, app| {
                draw(window, app);
                root.update(app, |root, _| {
                    root.transfer_workspace_acceptance_quit_ready()
                })
            })?
            .map_err(io::Error::other)?;
        if ready {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待工作区退出保存超时").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    let report = PhaseReport {
        phase: name,
        process_id: std::process::id(),
        candidate_sha256: candidate_sha256.into(),
        host_id: host.id.as_str().into(),
        host_form_created: phase == Phase::Prepare,
        host_form_edited: phase == Phase::Prepare,
        host_configuration_text: HOST_EDITED_TEXT.into(),
        rendered_text: EDITED_TEXT.into(),
        native_isolation: isolation,
        capture: capture.into(),
        commands_mounting,
        requested_viewport,
        viewport,
        scale_factor,
    };
    fs::write(
        output.join(format!("{name}-report.json")),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 几何证据必须对应请求的逻辑视口；DPI 单列记录，拒绝以屏幕或最大化窗口代替窄窗口。
pub(super) fn viewport_evidence(
    window: &Window,
    expected: [u32; 2],
) -> Result<([f32; 2], f32), io::Error> {
    let viewport = window.viewport_size();
    let actual = [viewport.width.as_f32(), viewport.height.as_f32()];
    let scale = window.scale_factor();
    if !scale.is_finite()
        || scale <= 0.0
        || actual
            .iter()
            .zip(expected)
            .any(|(value, expected)| !value.is_finite() || (*value - expected as f32).abs() > 32.0)
    {
        return Err(io::Error::other(format!(
            "插件验收视口不匹配：requested={expected:?}, actual={actual:?}, scale={scale}"
        )));
    }
    Ok((actual, scale))
}

/// 使用真实移动按钮验证同一片段实体的宿主切换，再回到原标签，避免验收专用路由掩盖交互遗漏。
/// 窄窗口抽屉遮住原工具轨道时使用抽屉自己的关闭按钮，不向被遮挡的背景控件发送点击。
///
/// @author kongweiguang
async fn exercise_commands_mounting(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    output: &Path,
) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
    let tab_id = window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| root.plugin_acceptance_snapshot(cx))["activeTabId"]
                .as_str()
                .map(str::to_owned)
        })?
        .ok_or_else(|| io::Error::other("片段验收缺少原标签"))?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_open_commands(window, cx)
        })
    })??;
    let first = wait_commands_mount(cx, root, window, true, None).await?;
    let entity_id = first["entityId"].clone();
    let mut states = vec![first];
    capture_frame(cx, window, &output.join("commands-center.png")).await?;
    for (name, mode) in [
        ("light", kerminal_application::preferences::ThemeMode::Light),
        ("dark", kerminal_application::preferences::ThemeMode::Dark),
        (
            "system",
            kerminal_application::preferences::ThemeMode::System,
        ),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            });
        })?;
        states.push(wait_commands_mount(cx, root, window, true, Some(&entity_id)).await?);
        capture_frame(
            cx,
            window,
            &output.join(format!("commands-center-{name}.png")),
        )
        .await?;
    }
    for _ in 0..3 {
        wait_selector(cx, window, "commands-move-to-rail").await?;
        click_selector(cx, window, "commands-move-to-rail", false)?;
        states.push(wait_commands_mount(cx, root, window, false, Some(&entity_id)).await?);
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.plugin_acceptance_open_commands(window, cx)
            })
        })??;
        states.push(wait_commands_mount(cx, root, window, false, Some(&entity_id)).await?);
        wait_selector(cx, window, "commands-move-to-center").await?;
        click_selector(cx, window, "commands-move-to-center", false)?;
        states.push(wait_commands_mount(cx, root, window, true, Some(&entity_id)).await?);
    }
    click_selector(cx, window, "commands-move-to-rail", false)?;
    states.push(wait_commands_mount(cx, root, window, false, Some(&entity_id)).await?);
    capture_frame(cx, window, &output.join("commands-rail.png")).await?;
    let close_selector = window.update(cx, |_, window, app| {
        draw(window, app);
        if selector_bounds(window, "workbench-close-compact-tool-drawer").is_some() {
            "workbench-close-compact-tool-drawer"
        } else {
            "workbench-tool-snippets"
        }
    })?;
    click_selector(cx, window, close_selector, false)?;
    wait_selector_absent(cx, window, "command-snippets-view").await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_open_commands(window, cx)
        })
    })??;
    states.push(wait_commands_mount(cx, root, window, true, Some(&entity_id)).await?);
    let tab_id = TabId::parse(&tab_id).map_err(|error| io::Error::other(error.to_string()))?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_tab_action(tab_id, false, window, cx)
        });
    })?;
    wait_content(cx, root, window, EDITED_TEXT).await?;
    Ok(states)
}

/// 绘制后的实体、宿主、布局、输入焦点和可见节点必须同时一致，避免抽屉换挂后焦点回落到 Root。
/// 双挂载或空白不能由快照字段单独掩盖；真实帧完成后才确认移动操作已交接输入。
///
/// @author kongweiguang
async fn wait_commands_mount(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    center: bool,
    entity_id: Option<&serde_json::Value>,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let (snapshot, visible) = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            let snapshot = root.update(app, |root, cx| {
                root.plugin_acceptance_commands_snapshot(window, cx)
            });
            Ok::<_, io::Error>((
                snapshot,
                selector_bounds(window, "command-snippets-view").is_some(),
            ))
        })??;
        if snapshot["centerMounted"] == true && snapshot["railMounted"] == true {
            return Err(io::Error::other(format!("片段发生重复挂载: {snapshot}")).into());
        }
        if entity_id.is_some_and(|expected| snapshot["entityId"] != *expected) {
            return Err(io::Error::other(format!("移动丢失原片段实例: {snapshot}")).into());
        }
        if visible
            && snapshot["centerMounted"] == center
            && snapshot["railMounted"] == !center
            && snapshot["compactLayout"] == !center
            && snapshot["searchFocused"] == true
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待片段挂载超时: {snapshot}")).into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 通过当前布局可见的新建入口创建并编辑，再比较同一主机的真实仓储回执。
/// 窄窗口空工作区使用“新连接”按钮，编辑已有主机再展开列表抽屉。
async fn exercise_host_forms(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &AppServices,
) -> Result<PluginHost, Box<dyn Error>> {
    let create_selector = window.update(cx, |_, window, app| {
        draw(window, app);
        if selector_bounds(window, "workbench-create-host").is_some() {
            "workbench-create-host"
        } else {
            "workbench-empty-terminal-create-host"
        }
    })?;
    wait_selector(cx, window, create_selector).await?;
    click_selector(cx, window, create_selector, false)?;
    wait_selector(cx, window, "connections-protocol-plugin-test.host").await?;
    click_selector(cx, window, "connections-protocol-plugin-test.host", false)?;
    wait_selector(cx, window, "plugin-acceptance-host-input").await?;
    edit_input(cx, window, "plugin-acceptance-host-input", INITIAL_TEXT)?;
    click_selector(cx, window, "connections-plugin-save", false)?;
    let created = wait_fixture_host(cx, window, services, INITIAL_TEXT).await?;
    wait_selector_absent(cx, window, "connections-plugin-save").await?;
    ensure_host_sidebar(cx, window).await?;
    let row = format!("host-navigation-plugin-{}", created.id.as_str());
    wait_selector(cx, window, &row).await?;
    click_selector_with_button(cx, window, &row, MouseButton::Right, false)?;
    click_menu_label(cx, window, &["编辑", "Edit"]).await?;
    wait_selector(cx, window, "plugin-acceptance-host-input").await?;
    edit_input(cx, window, "plugin-acceptance-host-input", HOST_EDITED_TEXT)?;
    click_selector(cx, window, "connections-plugin-save", false)?;
    let edited = wait_fixture_host(cx, window, services, HOST_EDITED_TEXT).await?;
    wait_selector_absent(cx, window, "connections-plugin-save").await?;
    if edited.id != created.id || edited.revision == created.revision {
        return Err(io::Error::other("编辑后的主机身份或修订号不正确").into());
    }
    let provider = services
        .plugin_hosts
        .clone()
        .ok_or_else(|| io::Error::other("插件主机服务未注入"))?;
    let host_id = edited.id.clone();
    services
        .executor
        .run(move |cancel| async move { provider.test(&host_id, cancel).await })
        .await?;
    Ok(edited)
}

/// 只通过共享执行器读取真实主机仓储，等待指定配置完成提交。
async fn wait_fixture_host(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &AppServices,
    expected_text: &str,
) -> Result<PluginHost, Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("主机服务不可用"))?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let service = hosts.clone();
        let matches: Vec<_> = services
            .executor
            .run(move |_| async move { service.list_plugin_hosts().await })
            .await?
            .into_iter()
            .filter(|host| {
                host.provider_id == PROVIDER_ID
                    && host.type_id == HOST_TYPE_ID
                    && host.name == HOST_NAME
            })
            .collect();
        if matches.len() > 1 {
            return Err(io::Error::other("验收主机出现重复记录").into());
        }
        if let Some(host) = matches.into_iter().next()
            && host.is_runnable()
            && host
                .configuration
                .as_ref()
                .and_then(|value| value.get("text"))
                .and_then(toml::Value::as_str)
                == Some(expected_text)
        {
            return Ok(host);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待读回主机配置 {expected_text} 超时")).into());
        }
        window.update(cx, |_, window, app| draw(window, app))?;
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 真实视图的活动标签和运行态快照必须同时达到预期，不能仅靠声明或截图通过。
///
/// @author kongweiguang
async fn wait_content(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    text: &str,
) -> Result<String, Box<dyn Error>> {
    let snapshot = wait_snapshot(cx, root, window, "插件内容", |snapshot| {
        snapshot["centralContent"] == true
            && snapshot["tabs"].as_array().is_some_and(|tabs| {
                tabs.iter().any(|tab| {
                    tab["contentId"] == CONTENT_ID
                        && tab["tabId"] == snapshot["activeTabId"]
                        && tab["phase"] == "Open"
                        && tab["liveState"]["text"] == text
                })
            })
    })
    .await?;
    snapshot["activeTabId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("插件标签缺少活动身份").into())
}

/// 只读回自动保存结果，不通过验收桥主动触发工作区写入。
///
/// @author kongweiguang
async fn wait_persisted_content(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &AppServices,
    tab_id: &str,
) -> Result<(), Box<dyn Error>> {
    use kerminal_application::workspace::{WorkspaceSessionLoad, WorkspaceSessionTab};
    let sessions = services
        .workspace_sessions
        .clone()
        .ok_or_else(|| io::Error::other("工作区保存服务不可用"))?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let service = sessions.clone();
        let loaded = services
            .executor
            .run(move |cancel| async move { service.load(cancel).await })
            .await?;
        if let WorkspaceSessionLoad::Loaded { document, .. } = loaded {
            let snapshot = document
                .workspace_snapshot()
                .map_err(|_| io::Error::other("工作区读回结构无效"))?;
            if snapshot.terminal_tabs.iter().any(|tab| matches!(tab,
                WorkspaceSessionTab::Extension { id, content_id, state_version: 1, state, .. }
                    if id.as_str() == tab_id && content_id == CONTENT_ID && state["text"] == EDITED_TEXT
            )) { return Ok(()); }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("插件标签保存读回超时").into());
        }
        window.update(cx, |_, window, app| draw(window, app))?;
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 复用产品主机行的双击手势，保证打开路径包含列表事件及异步提供方。
/// 窄窗口先通过标题栏展开主机抽屉，不能要求折叠内容仍然可点击。
///
/// @author kongweiguang
async fn open_fixture_host(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    host_id: &HostId,
) -> Result<(), Box<dyn Error>> {
    ensure_host_sidebar(cx, window).await?;
    let selector = format!("host-navigation-plugin-{}", host_id.as_str());
    wait_selector(cx, window, &selector).await?;
    click_selector(cx, window, &selector, true)
}

/// 主机栏默认随窗口预算折叠；验收也走真实展开按钮，不改生产响应式布局。
///
/// @author kongweiguang
async fn ensure_host_sidebar(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let visible = window.update(cx, |_, window, app| {
        draw(window, app);
        selector_bounds(window, "workbench-create-host").is_some()
            || selector_bounds(window, "workbench-close-compact-host-drawer").is_some()
    })?;
    if !visible {
        wait_selector(cx, window, "workbench-toggle-host-sidebar").await?;
        click_selector(cx, window, "workbench-toggle-host-sidebar", false)?;
        wait_selector(cx, window, "workbench-close-compact-host-drawer").await?;
    }
    Ok(())
}

/// 鼠标聚焦后发送实际按键，文本变化只能来自输入组件自身的编辑路径。
///
/// @author kongweiguang
pub(super) fn edit_input(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    text: &str,
) -> Result<(), Box<dyn Error>> {
    click_selector(cx, window, selector, false)?;
    window.update(cx, |_, window, app| {
        checked_isolation(window)?;
        draw(window, app);
        for key in ["secondary-a", "backspace"] {
            window.dispatch_keystroke(Keystroke::parse(key).map_err(io::Error::other)?, app);
        }
        for character in text.chars() {
            let key = character.to_string();
            window.dispatch_keystroke(
                Keystroke {
                    modifiers: Default::default(),
                    key: key.clone(),
                    key_char: Some(key),
                },
                app,
            );
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// Windows 核对私有桌面；Linux 核对专属 Xvfb 进程，避免 X11 的可见标记误报或影响用户显示。
///
/// @author kongweiguang
pub(super) fn checked_isolation(window: &Window) -> Result<serde_json::Value, io::Error> {
    #[cfg(target_os = "linux")]
    {
        let mut evidence = x11_isolation::capture()?;
        evidence["windowVisible"] = serde_json::json!(window.is_visible());
        evidence["windowActive"] = serde_json::json!(window.is_window_active());
        Ok(evidence)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let evidence = crate::right_rail_acceptance::isolation::capture()?;
        evidence.validate_plugin_window_state(window.is_visible(), window.is_window_active())?;
        let value = serde_json::to_value(evidence).map_err(io::Error::other)?;
        #[cfg(windows)]
        if value
            .get("isolationProven")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(io::Error::other("插件验收未证明私有桌面隔离"));
        }
        Ok(value)
    }
}

/// 下一帧回调和绘制一起推进，避免读取尚未挂载的控件或恢复状态。
///
/// @author kongweiguang
pub(super) fn draw(window: &mut Window, app: &mut App) {
    let frame = window.draw(app);
    frame.clear(app);
    if window.simulate_next_frame(app) > 0 {
        let frame = window.draw(app);
        frame.clear(app);
    }
}

/// 等待生产 Root 的只读状态，并在失败时附上末次状态而非伪报通过。
///
/// @author kongweiguang
async fn wait_snapshot(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    mut predicate: impl FnMut(&serde_json::Value) -> bool,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let snapshot = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            Ok::<_, io::Error>(root.update(app, |root, cx| root.plugin_acceptance_snapshot(cx)))
        })??;
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{stage}超时: {snapshot}")).into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 只接受已布局且在当前窗口可见视口内的控件，防止屏外坐标冒充真实操作。
///
/// @author kongweiguang
fn selector_bounds(window: &Window, selector: &str) -> Option<Bounds<gpui_pre::Pixels>> {
    let id: ElementId = selector.to_owned().into();
    let element = gpui_kit::base::test_support::find(window, &[], &id).or_else(|| {
        let matches: Vec<_> = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|element| {
                element.path().contains(&id)
                    && matches!(element.path().last(),
                        Some(ElementId::Name(name) | ElementId::NamedInteger(name, _))
                            if name.as_ref() == "input")
            })
            .collect();
        (matches.len() == 1).then(|| matches[0].clone())
    });
    let bounds = match element {
        Some(element) if element.visible() => element.bounds(),
        Some(_) => return None,
        None => window.debug_selector_bounds(selector)?,
    };
    let viewport = window.visual_viewport_bounds();
    let rounding = px(0.001);
    (!bounds.is_empty()
        && !viewport.is_empty()
        && bounds.origin.x + rounding >= viewport.origin.x
        && bounds.origin.y + rounding >= viewport.origin.y
        && bounds.origin.x + bounds.size.width
            <= viewport.origin.x + viewport.size.width + rounding
        && bounds.origin.y + bounds.size.height
            <= viewport.origin.y + viewport.size.height + rounding)
        .then_some(bounds)
}

/// 主机列表和工具面板异步加载，等待时持续绘制并保持隔离检查。
///
/// @author kongweiguang
pub(super) async fn wait_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let ready = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            Ok::<_, io::Error>(selector_bounds(window, selector).is_some())
        })??;
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待控件 {selector} 超时")).into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 持久化完成后仍需等待编辑器处理回执并关闭，避免菜单操作落在模态层后方。
async fn wait_selector_absent(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let absent = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            Ok::<_, io::Error>(selector_bounds(window, selector).is_none())
        })??;
        if absent {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待控件 {selector} 关闭超时")).into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 将真实布局中心交给 GPUI 鼠标分发；双击依次发送一次和两次点击计数。
///
/// @author kongweiguang
pub(super) fn click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    double: bool,
) -> Result<(), Box<dyn Error>> {
    click_selector_with_button(cx, window, selector, MouseButton::Left, double)
}

/// 导航右键与普通点击都从相同的可见布局位置进入 GPUI 分发路径。
fn click_selector_with_button(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    button: MouseButton,
    double: bool,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        checked_isolation(window)?;
        draw(window, app);
        let bounds = selector_bounds(window, selector)
            .ok_or_else(|| io::Error::other(format!("控件 {selector} 不可见")))?;
        dispatch_pointer_click(window, app, bounds.center(), button, double);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 菜单项使用无障碍名称定位，限定当前弹出菜单并拒绝重复匹配。
async fn click_menu_label(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    labels: &[&str],
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let clicked = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            let popup: ElementId = "popup-menu".into();
            let matches: Vec<_> = gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .filter(|item| {
                    item.visible()
                        && item.path().contains(&popup)
                        && item.label().is_some_and(|label| labels.contains(&label))
                })
                .collect();
            if matches.len() > 1 {
                return Err(io::Error::other("弹出菜单有重复的编辑项"));
            }
            if let Some(item) = matches.first() {
                dispatch_pointer_click(
                    window,
                    app,
                    item.bounds().center(),
                    MouseButton::Left,
                    false,
                );
                return Ok(true);
            }
            Ok(false)
        })??;
        if clicked {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待插件主机编辑菜单超时").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 与实际命中区域交互；双击按 GPUI 要求依次发送两种点击计数。
fn dispatch_pointer_click(
    window: &mut Window,
    app: &mut App,
    position: Point<Pixels>,
    button: MouseButton,
    double: bool,
) {
    let _ = window.dispatch_event(
        MouseMoveEvent {
            position,
            ..MouseMoveEvent::default()
        }
        .to_platform_input(),
        app,
    );
    for click_count in 1..=if double { 2 } else { 1 } {
        let _ = window.dispatch_event(
            MouseDownEvent {
                button,
                position,
                click_count,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(
            MouseUpEvent {
                button,
                position,
                click_count,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
            app,
        );
    }
}

/// Windows 核对 renderer 的非透明图像；Linux 从隔离 X11 的实际窗口读取像素。
/// 平台不支持离屏截图时不能将该错误误判为业务失败或跳过图像证据。
///
/// @author kongweiguang
pub(super) async fn capture_frame(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    path: &Path,
) -> Result<(), Box<dyn Error>> {
    // X11 提交 GPU 帧后需要让事件循环推进，避免截图拿到前一个页面的像素。
    window.update(cx, |_, window, app| draw(window, app))?;
    cx.background_executor().timer(POLL_INTERVAL).await;
    window.update(cx, |_, window, app| {
        checked_isolation(window)?;
        draw(window, app);
        #[cfg(target_os = "linux")]
        {
            let bounds = window.bounds();
            let viewport = window.viewport_size();
            let scale = window.scale_factor();
            if !scale.is_finite()
                || scale <= 0.0
                || bounds.origin.x.as_f32() < 0.0
                || bounds.origin.y.as_f32() < 0.0
            {
                return Err(io::Error::other("插件验收窗口坐标或缩放无效"));
            }
            return x11_isolation::save_frame(
                path,
                (
                    (bounds.origin.x.as_f32() * scale).round() as u32,
                    (bounds.origin.y.as_f32() * scale).round() as u32,
                ),
                (
                    (viewport.width.as_f32() * scale).round() as u32,
                    (viewport.height.as_f32() * scale).round() as u32,
                ),
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            let image = window.render_to_image().map_err(io::Error::other)?;
            let viewport = window.viewport_size();
            let scale = window.scale_factor();
            if !scale.is_finite()
                || scale <= 0.0
                || image.width() != (viewport.width.as_f32() * scale).round() as u32
                || image.height() != (viewport.height.as_f32() * scale).round() as u32
                || !image
                    .as_raw()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] != 0)
            {
                return Err(io::Error::other(format!(
                    "插件验收截图为空或与窗口尺寸不符：image={}x{}, viewport={}x{}, scale={scale}",
                    image.width(),
                    image.height(),
                    viewport.width.as_f32(),
                    viewport.height.as_f32()
                )));
            }
            image.save(path).map_err(io::Error::other)
        }
    })??;
    Ok(())
}
