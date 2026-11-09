//! 仅显式开发模式读取 CLI 的重载信号；正式安装目录仍在重启后生效。
//! @author kongweiguang

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use kerminal_application::{
    AppError, AppResult, executor::ApplicationTaskExecutor, external_contributions::PreparedPlugin,
    plugin_runtime::PluginRuntimeService,
};
use kerminal_domain::plugins::PluginSource;
use kerminal_infrastructure::plugins::boot::{BootPackageValidator, ProtocolBootValidator};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::startup::{HOST_API, StartupPlugins, current_platform};

const MAX_SIGNAL_BYTES: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(150);
const RELOAD_BUDGET: Duration = Duration::from_secs(10);

/// 任务所有权留在桌面组合根，退出时必须等待真实收尾，不能仅丢弃 JoinHandle。
pub struct DevPluginReloader {
    cancellation: CancellationToken,
    task: Option<JoinHandle<AppResult<()>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReloadSignal {
    generation: u64,
    plugin_dir: PathBuf,
}

#[derive(Default)]
struct ReloadCursor {
    payload: Option<Vec<u8>>,
    generation: u64,
}

impl DevPluginReloader {
    /// 开关和目标来自启动参数；没有 --dev-plugin 时连信号文件都不读取。
    pub fn start(
        startup: &StartupPlugins,
        root: &Path,
        runtime: Arc<PluginRuntimeService>,
        executor: ApplicationTaskExecutor,
    ) -> AppResult<Option<Self>> {
        let Some(directory) = &startup.options.dev_plugin else {
            return Ok(None);
        };
        let frozen = startup
            .enabled
            .iter()
            .find(|plugin| plugin.directory == *directory)
            .cloned()
            .ok_or_else(|| AppError::not_found("开发插件启动快照不存在"))?;
        let signal = root.join("plugins/dev-reload.json");
        let receipt = root.join("plugins/dev-reload-result.json");
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            executor
                .run_cancellable(worker_cancel, move |shutdown| async move {
                    watch(signal, receipt, frozen, runtime, shutdown).await
                })
                .await
        });
        Ok(Some(Self {
            cancellation,
            task: Some(task),
        }))
    }

    /// 与进程关闭共享绝对截止时间；超时保留句柄，下一轮退出可继续等待。
    pub async fn shutdown_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.cancellation.cancel();
        if let Some(task) = self.task.as_mut() {
            let result = tokio::time::timeout_at(deadline.into(), task)
                .await
                .map_err(|_| AppError::timeout("开发插件监听退出"))?;
            // JoinHandle 一旦返回就已消费，先清除 owner，错误路径也不能再次 poll。
            self.task = None;
            result.map_err(|source| {
                AppError::infrastructure_with_source("开发插件监听异常退出", source)
            })??;
        }
        Ok(())
    }
}

impl Drop for DevPluginReloader {
    /// 非正常退出仍通知受管任务收尾；正常路径由 shutdown_until 保留并验证 owner。
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// 每次只处理一个代次；构建较快时下一轮直接读取最新文件，不积压旧进程重启。
async fn watch(
    signal: PathBuf,
    receipt: PathBuf,
    frozen: Arc<PreparedPlugin>,
    runtime: Arc<PluginRuntimeService>,
    cancellation: CancellationToken,
) -> AppResult<()> {
    let mut cursor = ReloadCursor::default();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            _ = interval.tick() => {},
        }
        let payload = match read_signal(&signal).await {
            Ok(Some(payload)) => payload,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    event = "PLUGIN_DEV_SIGNAL_FAILED",
                    code = error.public_code().as_str()
                );
                return Err(error);
            }
        };
        if cursor.payload.as_ref() == Some(&payload) {
            continue;
        }
        cursor.payload = Some(payload.clone());
        let request = match decode_signal(&payload, cursor.generation) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(error) => {
                write_receipt(&receipt, None, Err(&error)).await?;
                continue;
            }
        };
        // 无效的新构建也消费该代次，防止对同一失败副作用自动重试。
        cursor.generation = request.generation;
        let generation = request.generation;
        let prepared = validate_reload(frozen.clone(), request, cancellation.clone()).await;
        let result = match prepared {
            Ok(prepared) => {
                runtime
                    .dev_reload(
                        Arc::new(prepared),
                        Instant::now() + RELOAD_BUDGET,
                        cancellation.clone(),
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }
        write_receipt(&receipt, Some(generation), result.as_ref().map(|_| ())).await?;
        match result {
            Ok(_) => tracing::info!(event = "PLUGIN_DEV_RELOADED", plugin = %frozen.id, generation),
            Err(error) => {
                tracing::warn!(event = "PLUGIN_DEV_RELOAD_FAILED", plugin = %frozen.id, generation, code = error.public_code().as_str())
            }
        }
    }
}

/// 固定小上限后再解析，开发信号不能借由错误文件占满内存。
async fn read_signal(path: &Path) -> AppResult<Option<Vec<u8>>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取开发插件重载信号失败",
                source,
            ));
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_SIGNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("读取开发插件重载信号失败", source)
        })?;
    if bytes.len() as u64 > MAX_SIGNAL_BYTES {
        return Err(AppError::invalid("开发插件重载信号过大"));
    }
    Ok(Some(bytes))
}

/// 代次单调递增；迟到的旧文件不会重复关闭已经恢复的插件。
fn decode_signal(bytes: &[u8], current: u64) -> AppResult<Option<ReloadSignal>> {
    let signal: ReloadSignal =
        serde_json::from_slice(bytes).map_err(|_| AppError::invalid("开发插件重载信号无效"))?;
    if signal.generation == 0 || !signal.plugin_dir.is_absolute() {
        return Err(AppError::invalid("开发插件重载身份无效"));
    }
    Ok((signal.generation > current).then_some(signal))
}

/// 包校验是阻塞磁盘工作；等待真实返回后才响应取消，避免后台验证脱管。
async fn validate_reload(
    frozen: Arc<PreparedPlugin>,
    signal: ReloadSignal,
    cancellation: CancellationToken,
) -> AppResult<PreparedPlugin> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("开发插件重载已取消"));
    }
    let result =
        tokio::task::spawn_blocking(move || {
            let expected = frozen.directory.canonicalize().map_err(|source| {
                AppError::infrastructure_with_source("开发插件目录不可用", source)
            })?;
            let requested = signal.plugin_dir.canonicalize().map_err(|source| {
                AppError::infrastructure_with_source("开发插件目录不可用", source)
            })?;
            if expected != requested {
                return Err(AppError::conflict("重载目标与开发插件不一致"));
            }
            let replacement = ProtocolBootValidator {
                host_api: HOST_API.into(),
                platform: current_platform()?,
            }
            .validate(
                &requested,
                &frozen.id,
                &frozen.version,
                PluginSource::Official,
            )
            .map_err(|_| AppError::invalid("开发插件包无效或清单已改变，请重新启动开发会话"))?;
            validate_frozen_contract(&frozen, &replacement)?;
            Ok(replacement)
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("开发插件校验异常退出", source))?;
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("开发插件重载已取消"));
    }
    result
}

/// 注册器、配置 codec 和授权在启动时冻结，热重载只替换逻辑及界面资源正文。
pub(super) fn validate_frozen_contract(
    frozen: &PreparedPlugin,
    replacement: &PreparedPlugin,
) -> AppResult<()> {
    if frozen.id != replacement.id
        || frozen.version != replacement.version
        || frozen.package.manifest != replacement.package.manifest
        || frozen.package.configuration_schemas != replacement.package.configuration_schemas
        || frozen.localizations != replacement.localizations
    {
        return Err(AppError::conflict(
            "插件清单、配置或贡献已改变，请重新启动开发会话",
        ));
    }
    Ok(())
}

/// 回执只含代次及受控错误；CLI/验收能区分已请求与已完成，不泄漏本机路径。
async fn write_receipt(
    path: &Path,
    generation: Option<u64>,
    result: Result<(), &AppError>,
) -> AppResult<()> {
    let value: Value = match result {
        Ok(()) => json!({"generation": generation, "status": "reloaded"}),
        Err(error) => {
            json!({"generation": generation, "status": "rejected", "message": error.public_message()})
        }
    };
    let bytes = serde_json::to_vec(&value).map_err(|source| {
        AppError::infrastructure_with_source("编码开发插件重载回执失败", source)
    })?;
    tokio::fs::write(path, bytes)
        .await
        .map_err(|source| AppError::infrastructure_with_source("保存开发插件重载回执失败", source))
}

#[cfg(test)]
#[path = "../../tests/support/plugin_dev_reload_unit.rs"]
mod tests;
