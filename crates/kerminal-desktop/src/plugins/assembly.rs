//! 外部插件的桌面组合：冻结资源、持久端口与进程 owner 在这里接到同一应用执行器。
//! @author kongweiguang

use std::{
    ffi::OsString,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use kerminal_application::{
    AppError, AppResult,
    connections::{
        host_types::{ExternalHostProvider, HostConfigurationCatalog},
        management::HostManagementService,
    },
    executor::ApplicationTaskExecutor,
    plugin_host_api::{PluginEventSubscriptions, PluginHostApiService},
    plugin_ports::PluginProcessPort,
    plugin_runtime::{PluginRuntimeDescriptor, PluginRuntimeService},
    plugin_settings::PluginSettingsService,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore,
    plugins::{process::PluginProcessSpec, supervisor::ProcessSupervisor},
    vault::VaultStore,
};
use kerminal_plugin_protocol::{Activation, PluginRuntime as RuntimeKind};
use kerminal_ui::PluginUiBridge;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    dev_reload::DevPluginReloader,
    startup::{HOST_API, StartupPlugins, current_platform},
    view_runtime::DesktopViewRuntime,
};

/// 进程与订阅任务必须由组合根持有到实际回收，不能随着 UI 工厂被释放而脱管。
pub struct DesktopPlugins {
    pub runtime: Arc<PluginRuntimeService>,
    pub views: Arc<DesktopViewRuntime>,
    pub settings: PluginSettingsService,
    pub ui: Arc<PluginUiBridge>,
    pub events: Arc<PluginEventSubscriptions>,
    pub startup: Arc<StartupPlugins>,
    pub host_types: Arc<HostConfigurationCatalog>,
    pub vault: Arc<VaultStore>,
    process: Arc<dyn PluginProcessPort>,
    cancellation: CancellationToken,
    settings_task: Option<JoinHandle<AppResult<()>>>,
    reloader: Option<DevPluginReloader>,
}

impl DesktopPlugins {
    /// 基础设施已经使用同一配置根；冻结清单是权限、路径和版本的唯一来源。
    pub fn new(
        startup: Arc<StartupPlugins>,
        configuration: ConfigurationStore,
        vault: Arc<VaultStore>,
        hosts: HostManagementService,
        executor: ApplicationTaskExecutor,
        locale: String,
        theme: String,
    ) -> AppResult<Self> {
        let root = configuration.root().to_path_buf();
        let host_types = configuration.host_types().clone();
        let executable = std::env::current_exe()
            .map_err(|source| AppError::infrastructure_with_source("定位脚本宿主失败", source))?;
        let host = executable
            .parent()
            .ok_or_else(|| AppError::invalid("脚本宿主目录无效"))?
            .join(if cfg!(windows) {
                "kerminal-plugin-host.exe"
            } else {
                "kerminal-plugin-host"
            });
        let specs = process_specs(&startup, &root, &host, &locale, &theme)?;
        let process: Arc<dyn PluginProcessPort> =
            Arc::new(ProcessSupervisor::new(specs, executor.clone())?);
        let settings = PluginSettingsService::new(
            Arc::new(configuration.clone()),
            Arc::new(configuration),
            vault.clone(),
        );
        let ui = PluginUiBridge::new()?;
        let events = Arc::new(PluginEventSubscriptions::new(process.clone()));
        let api = Arc::new(PluginHostApiService::new(
            startup.enabled.clone(),
            settings.clone(),
            Arc::new(hosts),
            ui.clone(),
            events.clone(),
        )?);
        let runtime = PluginRuntimeService::new(
            startup
                .enabled
                .iter()
                .cloned()
                .map(PluginRuntimeDescriptor::from_prepared)
                .collect(),
            process.clone(),
            api,
            executor.clone(),
        )?;
        for plugin in &startup.enabled {
            for host in &plugin.package.manifest.contributes.host_types {
                let provider = ExternalHostProvider::new(
                    plugin.clone(),
                    Arc::downgrade(&runtime),
                    host.id.clone(),
                    locale.clone(),
                    vault.clone(),
                )?;
                host_types.bind_runtime_provider(
                    plugin.id.as_str(),
                    &format!("{}/{}", plugin.id.as_str(), host.id),
                    Arc::new(provider),
                )?;
            }
        }
        let reloader =
            DevPluginReloader::start(&startup, &root, runtime.clone(), executor.clone())?;
        let views = Arc::new(DesktopViewRuntime::new(
            runtime.clone(),
            process.clone(),
            vault.clone(),
        ));
        let cancellation = CancellationToken::new();
        // 保存成功事件只传脱敏投影；没有运行的插件等下次激活读取 settings.get。
        let mut changes = settings.subscribe();
        let worker_process = process.clone();
        let worker_runtime = runtime.clone();
        let worker_cancel = cancellation.clone();
        let worker_executor = executor.clone();
        let worker_settings = settings.clone();
        let worker_plugins = startup.enabled.clone();
        let settings_task = tokio::spawn(async move {
            worker_executor.run(move |shutdown| async move {
                loop {
                    let change = tokio::select! {
                        _ = worker_cancel.cancelled() => return Ok(()),
                        _ = shutdown.cancelled() => return Ok(()),
                        value = changes.recv() => value,
                    };
                    let updates = match change {
                        Ok(change) => vec![(change.plugin, change.snapshot.values)],
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let mut updates = Vec::new();
                            for plugin in &worker_plugins {
                                let schema = plugin.package.manifest.contributes.settings.as_ref()
                                    .and_then(|declaration| plugin.package.configuration_schemas.get(&declaration.schema));
                                let Some(schema) = schema else { continue; };
                                match worker_settings.load(&plugin.id, schema, worker_cancel.clone()).await {
                                    Ok(snapshot) => updates.push((plugin.id.clone(), snapshot.values)),
                                    Err(error) => tracing::warn!(event = "PLUGIN_SETTINGS_REFRESH_FAILED", plugin = %plugin.id, code = error.public_code().as_str()),
                                }
                            }
                            updates
                        }
                    };
                    for (plugin, values) in updates {
                        if let Some(generation) = worker_runtime.process_generation(&plugin)? {
                            if generation == 0 { continue; }
                            if let Err(error) = worker_process.notify(&plugin, generation, "settings.changed", json!({"settings": values}), Instant::now() + Duration::from_secs(5)).await {
                                tracing::warn!(event = "PLUGIN_SETTINGS_NOTIFY_FAILED", plugin = %plugin, code = error.public_code().as_str());
                            }
                        }
                    }
                }
            }).await
        });
        Ok(Self {
            runtime,
            views,
            settings,
            ui,
            events,
            startup,
            host_types,
            vault,
            process,
            cancellation,
            settings_task: Some(settings_task),
            reloader,
        })
    }

    /// 声明为 startup 的进程在所有宿主服务可用后启动；失败保留监督器的可重试状态。
    pub async fn activate_startup(&self) {
        for plugin in &self.startup.enabled {
            if matches!(
                plugin.package.manifest.runtime,
                RuntimeKind::Script {
                    activation: Activation::Startup,
                    ..
                } | RuntimeKind::Sidecar {
                    activation: Activation::Startup,
                    ..
                }
            ) && let Err(error) = self
                .runtime
                .activate_plugin(plugin.id.clone(), Instant::now() + Duration::from_secs(10))
                .await
            {
                tracing::warn!(event = "PLUGIN_STARTUP_FAILED", plugin = %plugin.id, code = error.public_code().as_str());
            }
        }
    }

    /// 与桌面共享绝对截止时间；任务或进程未回收时保留 owner 以便退出重试。
    pub async fn shutdown_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.cancellation.cancel();
        let mut first_error = None;
        if let Some(reloader) = self.reloader.as_mut() {
            match reloader.shutdown_until(deadline).await {
                Ok(()) => self.reloader = None,
                Err(error) => first_error = Some(error),
            }
        }
        // 监听器失败也必须向进程监督器传递同一截止时间，不能跳过进程树回收。
        if let Err(error) = self.process.shutdown_until(deadline).await {
            first_error.get_or_insert(error);
        }
        if let Some(task) = self.settings_task.as_mut() {
            match tokio::time::timeout_at(deadline.into(), task).await {
                Ok(result) => {
                    self.settings_task = None;
                    let result = result
                        .map_err(|source| {
                            AppError::infrastructure_with_source("插件设置订阅异常退出", source)
                        })
                        .and_then(|result| result);
                    if let Err(error) = result {
                        first_error.get_or_insert(error);
                    }
                }
                Err(_) => {
                    first_error.get_or_insert(AppError::timeout("插件设置订阅退出"));
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// 只解析锁定版本内的入口，Sidecar 不会意外回退到 PATH 或另一平台程序。
fn process_specs(
    startup: &StartupPlugins,
    root: &Path,
    host: &Path,
    locale: &str,
    theme: &str,
) -> AppResult<Vec<PluginProcessSpec>> {
    let platform = current_platform()?;
    let mut specs = Vec::new();
    for plugin in &startup.enabled {
        let (executable, arguments) = match &plugin.package.manifest.runtime {
            RuntimeKind::None { .. } => continue,
            RuntimeKind::Script { .. } => {
                let mut args = vec![plugin.directory.as_os_str().to_os_string()];
                if plugin.id.as_str().starts_with("kerminal.") {
                    args.push(OsString::from("--official"));
                }
                (host.to_path_buf(), args)
            }
            RuntimeKind::Sidecar { main, .. } => (
                plugin.directory.join(
                    main.get(&platform)
                        .ok_or_else(|| AppError::unsupported("插件没有当前平台程序"))?,
                ),
                Vec::new(),
            ),
        };
        specs.push(PluginProcessSpec {
            id: plugin.id.clone(),
            version: plugin.version.clone(),
            executable,
            arguments,
            data_directory: root.join("plugins/data").join(plugin.id.as_str()),
            log_directory: root.join("plugins/logs"),
            locale: locale.into(),
            theme: theme.into(),
            host_api: HOST_API.into(),
        });
    }
    Ok(specs)
}
