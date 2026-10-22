//! 更新候选、异步检查与下载以及 SDK 安装生命周期。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use cargo_packager_updater::{Update, reqwest::Certificate};
use kerminal_application::updates::{UpdateAvailability, UpdateRecoveryId, UpdateRecoveryRecord};
use kerminal_application::{
    AppError, AppResult, DownloadedUpdate, DownloadedUpdateId, UpdateCandidate, UpdateCandidateId,
    UpdateInstallResult, UpdatePort, UpdateProgress,
};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use kerminal_application::UpdateInstallDisposition;

#[cfg(target_os = "linux")]
use std::{io, io::Read, os::unix::fs::PermissionsExt, path::Path};

#[cfg(windows)]
use cargo_packager_updater::UpdateFormat;

use super::{download::download_update, metadata, settings::UpdaterSettings};

#[derive(Clone, Copy, Eq, PartialEq)]
enum CandidatePhase {
    Available,
    Downloading,
}

struct CandidateRecord {
    update: Update,
    phase: CandidatePhase,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DownloadedPhase {
    Ready,
    Installing,
    OutcomeUnknown,
    Discarding,
}

struct DownloadedRecord {
    update: Update,
    recovery_id: UpdateRecoveryId,
    phase: DownloadedPhase,
}

struct InstallPlan {
    update: Update,
    recovery_id: UpdateRecoveryId,
}

struct InstallOutcome {
    result: UpdateInstallResult,
    installed_target_sha256: Option<String>,
}

#[derive(Default)]
struct StartupTargetSnapshot {
    installation_started: bool,
    digest_computed: bool,
    digest: Option<String>,
}

/// adapter 将同步 SDK 的状态保存在单一运行态目录，不把更新包暴露给 UI。
pub struct CargoPackagerUpdater {
    settings: Option<UpdaterSettings>,
    additional_trust_roots: Vec<Certificate>,
    recovery: super::recovery::RecoveryStore,
    current_target: Option<String>,
    current_version: String,
    availability: UpdateAvailability,
    startup_target_path: Option<PathBuf>,
    startup_target_snapshot: Arc<Mutex<StartupTargetSnapshot>>,
    candidates: Arc<Mutex<HashMap<UpdateCandidateId, CandidateRecord>>>,
    downloaded: Arc<Mutex<HashMap<DownloadedUpdateId, DownloadedRecord>>>,
    check_gate: AsyncMutex<()>,
    download_gate: AsyncMutex<()>,
}

/// async 下载 future 被丢弃时同步复位状态，避免候选永久卡在 Downloading。
struct DownloadResetGuard {
    candidates: Arc<Mutex<HashMap<UpdateCandidateId, CandidateRecord>>>,
    candidate_id: UpdateCandidateId,
    active: bool,
}

impl CargoPackagerUpdater {
    /// 即使缺少 feed 或当前安装形态不能更新，也创建同一适配器以读取和确认恢复记录。
    pub fn new(settings: Option<UpdaterSettings>, recovery_root: PathBuf) -> AppResult<Self> {
        Self::from_settings(settings, recovery_root, Vec::new())
    }

    /// 共享恢复状态初始化；私有信任根只供隔离 TLS 测试装配，不改变 URL 或安装目标策略。
    fn from_settings(
        settings: Option<UpdaterSettings>,
        recovery_root: PathBuf,
        additional_trust_roots: Vec<Certificate>,
    ) -> AppResult<Self> {
        let runtime_supported = UpdaterSettings::supports_current_runtime();
        let startup_target_path = current_runtime_target_path();
        let current_target = cargo_packager_updater::target();
        let current_version = settings
            .as_ref()
            .map(|configured| configured.current_version().to_string())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
        let max_recovery_bytes = settings
            .as_ref()
            .map(UpdaterSettings::max_update_bytes)
            .unwrap_or(super::settings::MAX_UPDATE_BYTES);
        let availability = match (&settings, runtime_supported, current_target.is_some()) {
            (None, _, _) => UpdateAvailability::Unavailable {
                reason: "此构建尚未配置更新源和签名公钥",
            },
            (Some(_), false, _) => UpdateAvailability::Unavailable {
                reason: "当前安装形态不支持应用内自动更新",
            },
            (Some(_), _, false) => UpdateAvailability::Unavailable {
                reason: "当前系统无法确定更新目标",
            },
            (Some(_), true, true) => UpdateAvailability::Configured,
        };
        let recovery = super::recovery::RecoveryStore::new(recovery_root, max_recovery_bytes)?;
        Ok(Self {
            settings,
            additional_trust_roots,
            recovery,
            current_target,
            current_version,
            availability,
            startup_target_path,
            startup_target_snapshot: Arc::new(Mutex::new(StartupTargetSnapshot::default())),
            candidates: Arc::new(Mutex::new(HashMap::new())),
            downloaded: Arc::new(Mutex::new(HashMap::new())),
            check_gate: AsyncMutex::new(()),
            download_gate: AsyncMutex::new(()),
        })
    }

    /// 恢复扫描在阻塞池执行摘要计算和目录同步，应用 task 收到取消时仍等待其落盘边界。
    async fn load_recoveries(&self) -> AppResult<Vec<UpdateRecoveryRecord>> {
        let recovery = self.recovery.clone();
        let current_version = self.current_version.clone();
        let current_target = self.current_target.clone();
        let startup_target_path = self.startup_target_path.clone();
        let startup_target_snapshot = Arc::clone(&self.startup_target_snapshot);
        tokio::task::spawn_blocking(move || {
            let target = current_target.as_deref().unwrap_or("");
            let initial = recovery.list(&current_version, target, None)?;
            if initial.is_empty() {
                return Ok(initial);
            }
            let mut snapshot = startup_target_snapshot
                .lock()
                .map_err(|_| AppError::infrastructure("更新目标回读状态不可用"))?;
            if !snapshot.digest_computed && !snapshot.installation_started {
                snapshot.digest = startup_target_path
                    .as_deref()
                    .and_then(|path| recovery.target_sha256(path).ok());
                snapshot.digest_computed = true;
            }
            recovery.list(&current_version, target, snapshot.digest.as_deref())
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("读取更新恢复状态的后台任务失败", error)
        })?
    }

    /// 确认没有遗留恢复包后才允许新下载，避免覆盖尚未人工处理的 Unknown 事务。
    async fn ensure_no_unresolved_recovery(&self) -> AppResult<()> {
        if !self.load_recoveries().await?.is_empty() {
            return Err(AppError::conflict("存在待人工核实的更新恢复材料"));
        }
        Ok(())
    }

    /// 删除显式放弃的 Pending 包；文件系统清理放入 blocking pool，避免堵住异步 worker。
    async fn remove_recovery(&self, recovery_id: UpdateRecoveryId) -> AppResult<()> {
        let recovery = self.recovery.clone();
        tokio::task::spawn_blocking(move || recovery.remove(recovery_id))
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("清理更新恢复材料的后台任务失败", error)
            })?
    }

    /// 更新源缺失或平台不支持时短路网络入口，同时保留 recovery store 的读写能力。
    fn require_configured_settings(&self) -> AppResult<&UpdaterSettings> {
        match self.availability {
            UpdateAvailability::Configured => self
                .settings
                .as_ref()
                .ok_or_else(|| AppError::unsupported("当前构建未配置应用内更新源")),
            UpdateAvailability::Unavailable { reason } => Err(AppError::unsupported(reason)),
        }
    }
}

#[async_trait]
impl UpdatePort for CargoPackagerUpdater {
    /// 静态能力查询让缺少 feed 的构建仍能在 UI 中展示恢复记录入口。
    fn availability(&self) -> UpdateAvailability {
        self.availability
    }

    /// 通过异步有界 metadata adapter 检查 SDK manifest，确保网络读取可取消且总时限明确。
    async fn check(&self, cancel: CancellationToken) -> AppResult<Option<UpdateCandidate>> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("检查更新已取消"));
        }
        let settings = self.require_configured_settings()?;
        let _check_guard = lock_or_cancel(&self.check_gate, &cancel, "检查更新已取消").await?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("检查更新已取消"));
        }
        self.candidates
            .lock()
            .map_err(|_| AppError::infrastructure("更新候选目录不可用"))?
            .retain(|_, record| record.phase == CandidatePhase::Downloading);

        let update =
            metadata::check(settings, cancel.clone(), &self.additional_trust_roots).await?;

        if cancel.is_cancelled() {
            return Err(AppError::cancelled("检查更新已取消"));
        }
        let Some(update) = update else {
            return Ok(None);
        };
        let candidate = UpdateCandidate {
            id: UpdateCandidateId::new(),
            version: update.version.clone(),
            notes: update.body.clone(),
            published_at: update.date.map(|date| date.to_string()),
        };
        self.candidates
            .lock()
            .map_err(|_| AppError::infrastructure("更新候选目录不可用"))?
            .insert(
                candidate.id,
                CandidateRecord {
                    update,
                    phase: CandidatePhase::Available,
                },
            );
        Ok(Some(candidate))
    }

    /// 下载 API 只返回验签通过的字节；取消、下载失败和过大包都会释放或复位状态。
    async fn download(
        &self,
        candidate_id: UpdateCandidateId,
        cancel: CancellationToken,
        progress: watch::Sender<UpdateProgress>,
    ) -> AppResult<DownloadedUpdate> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("下载更新已取消"));
        }
        let settings = self.require_configured_settings()?;
        let _download_guard =
            lock_or_cancel(&self.download_gate, &cancel, "下载更新已取消").await?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("下载更新已取消"));
        }
        self.ensure_no_unresolved_recovery().await?;
        if !self
            .downloaded
            .lock()
            .map_err(|_| AppError::infrastructure("已验签更新目录不可用"))?
            .is_empty()
        {
            return Err(AppError::conflict("已有更新包等待安装或释放"));
        }
        let update = {
            let mut candidates = self
                .candidates
                .lock()
                .map_err(|_| AppError::infrastructure("更新候选目录不可用"))?;
            let record = candidates
                .get_mut(&candidate_id)
                .ok_or_else(|| AppError::not_found("更新候选已失效"))?;
            if record.phase != CandidatePhase::Available {
                return Err(AppError::conflict("该更新候选正在下载"));
            }
            record.phase = CandidatePhase::Downloading;
            record.update.clone()
        };
        let mut reset_guard = DownloadResetGuard {
            candidates: self.candidates.clone(),
            candidate_id,
            active: true,
        };
        progress.send_replace(UpdateProgress::default());

        let bytes = download_update(
            &update,
            cancel.clone(),
            progress,
            settings.max_update_bytes(),
            &self.additional_trust_roots,
        )
        .await?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("下载更新已取消"));
        }
        if bytes.is_empty() {
            return Err(AppError::infrastructure("更新包为空"));
        }
        if bytes.len() as u64 > settings.max_update_bytes() {
            return Err(AppError::invalid("更新包超过允许的大小限制"));
        }

        let recovery_id = UpdateRecoveryId::new();
        let recovery = self.recovery.clone();
        let target = self
            .current_target
            .clone()
            .ok_or_else(|| AppError::unsupported("当前系统无法确定更新目标"))?;
        let version = update.version.clone();
        tokio::task::spawn_blocking(move || {
            recovery.persist_pending_bytes(recovery_id, &version, &target, &bytes)?;
            recovery.artifact_path(recovery_id)?;
            Ok::<(), AppError>(())
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("保存更新恢复材料的后台任务失败", error)
        })??;
        if cancel.is_cancelled() {
            self.remove_recovery(recovery_id).await?;
            return Err(AppError::cancelled("下载更新已取消"));
        }

        let mut downloaded = self
            .downloaded
            .lock()
            .map_err(|_| AppError::infrastructure("已验签更新目录不可用"))?;
        if !downloaded.is_empty() {
            return Err(AppError::conflict("已有更新包等待安装或释放"));
        }
        self.candidates
            .lock()
            .map_err(|_| AppError::infrastructure("更新候选目录不可用"))?
            .remove(&candidate_id)
            .ok_or_else(|| AppError::conflict("下载期间更新候选已失效"))?;
        let update_id = DownloadedUpdateId::new();
        let version = update.version.clone();
        downloaded.insert(
            update_id,
            DownloadedRecord {
                update,
                recovery_id,
                phase: DownloadedPhase::Ready,
            },
        );
        reset_guard.active = false;
        Ok(DownloadedUpdate {
            id: update_id,
            version,
        })
    }

    /// 安装不可中途取消；收据状态只随 SDK 结果推进，未知结果保留材料待人工核实。
    ///
    /// @author kongweiguang
    async fn install(&self, update_id: DownloadedUpdateId) -> UpdateInstallResult {
        let plan = {
            let downloaded = self.downloaded.lock().map_err(|_| {
                UpdateInstallResult::NotStarted(AppError::infrastructure("已验签更新目录不可用"))
            });
            let mut downloaded = match downloaded {
                Ok(downloaded) => downloaded,
                Err(result) => return result,
            };
            let record = downloaded.get_mut(&update_id).ok_or_else(|| {
                UpdateInstallResult::NotStarted(AppError::not_found("已验签更新包不存在"))
            });
            let record = match record {
                Ok(record) => record,
                Err(result) => return result,
            };
            match record.phase {
                DownloadedPhase::Ready => {}
                DownloadedPhase::Installing => {
                    return UpdateInstallResult::NotStarted(AppError::conflict("更新包正在安装"));
                }
                DownloadedPhase::OutcomeUnknown => {
                    return UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown(
                        "该更新包的安装结果待人工确认，不能自动重放",
                    ));
                }
                DownloadedPhase::Discarding => {
                    return UpdateInstallResult::NotStarted(AppError::conflict(
                        "更新包正在清理，不能开始安装",
                    ));
                }
            }
            record.phase = DownloadedPhase::Installing;
            InstallPlan {
                update: record.update.clone(),
                recovery_id: record.recovery_id,
            }
        };
        let recovery_id = plan.recovery_id;
        let downloaded = Arc::clone(&self.downloaded);
        let recovery = self.recovery.clone();
        let startup_target_snapshot = Arc::clone(&self.startup_target_snapshot);
        let install_task = tokio::task::spawn_blocking(move || {
            let mut outcome = match recovery.read_verified_artifact(recovery_id) {
                Err(error) => InstallOutcome {
                    result: UpdateInstallResult::NotStarted(error),
                    installed_target_sha256: None,
                },
                Ok(bytes) => {
                    let snapshot_started = startup_target_snapshot
                        .lock()
                        .map(|mut snapshot| snapshot.installation_started = true)
                        .map_err(|_| AppError::infrastructure("更新目标回读状态不可用"));
                    match snapshot_started {
                        Err(error) => InstallOutcome {
                            result: UpdateInstallResult::NotStarted(error),
                            installed_target_sha256: None,
                        },
                        Ok(()) => match recovery.mark_installing(recovery_id) {
                            Ok(()) => catch_unwind(AssertUnwindSafe(|| {
                                perform_install(plan, bytes, &recovery)
                            }))
                            .unwrap_or_else(|_| InstallOutcome {
                                result: UpdateInstallResult::OutcomeUnknown(
                                    AppError::outcome_unknown(
                                        "安装器执行异常，目标状态需要人工核实",
                                    ),
                                ),
                                installed_target_sha256: None,
                            }),
                            Err(error) => match recovery.restore_pending(recovery_id) {
                                Ok(()) => InstallOutcome {
                                    result: UpdateInstallResult::NotStarted(error),
                                    installed_target_sha256: None,
                                },
                                Err(restore_error) => InstallOutcome {
                                    result: UpdateInstallResult::OutcomeUnknown(
                                        AppError::outcome_unknown_with_source(
                                            "安装器未启动，但恢复收据状态需要人工核实",
                                            restore_error,
                                        ),
                                    ),
                                    installed_target_sha256: None,
                                },
                            },
                        },
                    }
                }
            };

            if matches!(&outcome.result, UpdateInstallResult::Installed(_))
                && let Err(error) = recovery.mark_awaiting_confirmation(
                    recovery_id,
                    outcome.installed_target_sha256.clone(),
                )
            {
                outcome.result =
                    UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                        "安装器已返回，但无法保存目标文件确认状态",
                        error,
                    ));
                outcome.installed_target_sha256 = None;
            } else if matches!(
                &outcome.result,
                UpdateInstallResult::NotStarted(_) | UpdateInstallResult::FailedNoEffect(_)
            ) && let Err(error) = recovery.restore_pending(recovery_id)
            {
                outcome.result =
                    UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                        "安装无副作用，但恢复收据状态无法回退",
                        error,
                    ));
            }
            finish_install_record(&downloaded, update_id, &outcome.result);
            outcome.result
        });
        match install_task.await {
            Ok(result) => result,
            Err(error) => {
                mark_install_unknown(&self.downloaded, update_id);
                UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                    "安装任务异常结束，目标状态需要人工核实",
                    error,
                ))
            }
        }
    }

    /// 只释放明确尚未提交的包；清理失败只恢复仍处于 Discarding 的记录。
    ///
    /// @author kongweiguang
    async fn discard(&self, update_id: DownloadedUpdateId) -> AppResult<()> {
        let recovery_id = {
            let mut downloaded = self
                .downloaded
                .lock()
                .map_err(|_| AppError::infrastructure("已验签更新目录不可用"))?;
            let Some(record) = downloaded.get_mut(&update_id) else {
                return Ok(());
            };
            match record.phase {
                DownloadedPhase::Installing => {
                    return Err(AppError::conflict("更新安装已开始，不能再丢弃"));
                }
                DownloadedPhase::OutcomeUnknown => {
                    return Err(AppError::outcome_unknown(
                        "更新安装结果待人工核实，暂存包已保留",
                    ));
                }
                DownloadedPhase::Discarding => {
                    return Err(AppError::conflict("更新恢复材料正在清理"));
                }
                DownloadedPhase::Ready => record.phase = DownloadedPhase::Discarding,
            }
            record.recovery_id
        };
        match self.remove_recovery(recovery_id).await {
            Ok(()) => {
                if let Ok(mut downloaded) = self.downloaded.lock() {
                    downloaded.remove(&update_id);
                }
                Ok(())
            }
            Err(error) => {
                if let Ok(mut downloaded) = self.downloaded.lock()
                    && let Some(record) = downloaded.get_mut(&update_id)
                    && record.phase == DownloadedPhase::Discarding
                {
                    record.phase = DownloadedPhase::Ready;
                }
                Err(error)
            }
        }
    }

    /// 启动查询只读；回读摘要在启动后缓存，安装已经开始的旧进程不会哈希替换目标。
    async fn recoveries(&self) -> AppResult<Vec<UpdateRecoveryRecord>> {
        self.load_recoveries().await
    }

    /// 人工确认只删除指定恢复目录；清理失败时按原阶段恢复且不覆盖并发状态。
    ///
    /// @author kongweiguang
    async fn acknowledge_recovery(&self, recovery_id: UpdateRecoveryId) -> AppResult<()> {
        let active_id = {
            let mut downloaded = self
                .downloaded
                .lock()
                .map_err(|_| AppError::infrastructure("已验签更新目录不可用"))?;
            let matching = downloaded
                .iter_mut()
                .find(|(_, record)| record.recovery_id == recovery_id)
                .map(|(update_id, record)| {
                    let prior_phase = record.phase;
                    match prior_phase {
                        DownloadedPhase::Installing => {
                            return Err(AppError::conflict("更新安装已开始，不能确认或清理"));
                        }
                        DownloadedPhase::Discarding => {
                            return Err(AppError::conflict("更新恢复材料正在清理"));
                        }
                        DownloadedPhase::Ready | DownloadedPhase::OutcomeUnknown => {
                            record.phase = DownloadedPhase::Discarding;
                        }
                    }
                    Ok((*update_id, prior_phase))
                });
            matching.transpose()?
        };
        match self.remove_recovery(recovery_id).await {
            Ok(()) => {
                if let (Some((update_id, _)), Ok(mut downloaded)) =
                    (active_id, self.downloaded.lock())
                {
                    downloaded.remove(&update_id);
                }
                Ok(())
            }
            Err(error) => {
                if let (Some((update_id, prior_phase)), Ok(mut downloaded)) =
                    (active_id, self.downloaded.lock())
                    && let Some(record) = downloaded.get_mut(&update_id)
                    && record.phase == DownloadedPhase::Discarding
                {
                    record.phase = prior_phase;
                }
                Err(error)
            }
        }
    }
}

impl Drop for DownloadResetGuard {
    /// 只在下载租约仍由本 guard 持有时回退状态；同步锁不跨异步边界。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut candidates) = self.candidates.lock()
            && let Some(record) = candidates.get_mut(&self.candidate_id)
            && record.phase == CandidatePhase::Downloading
        {
            record.phase = CandidatePhase::Available;
        }
    }
}

/// 只在 SDK 完成后迁移缓存阶段；未知结果保留安装包用于人工恢复，成功则释放它。
fn finish_install_record(
    downloaded: &Mutex<HashMap<DownloadedUpdateId, DownloadedRecord>>,
    update_id: DownloadedUpdateId,
    result: &UpdateInstallResult,
) {
    let Ok(mut downloaded) = downloaded.lock() else {
        return;
    };
    match result {
        UpdateInstallResult::Installed(_) => {
            downloaded.remove(&update_id);
        }
        UpdateInstallResult::NotStarted(_) | UpdateInstallResult::FailedNoEffect(_) => {
            if let Some(record) = downloaded.get_mut(&update_id) {
                record.phase = DownloadedPhase::Ready;
            }
        }
        UpdateInstallResult::OutcomeUnknown(_) => {
            if let Some(record) = downloaded.get_mut(&update_id) {
                record.phase = DownloadedPhase::OutcomeUnknown;
            }
        }
    }
}

/// Join 异常本身不足以证明是否触发了平台副作用，因此只锁定句柄并禁止盲目重装。
///
/// @author kongweiguang
fn mark_install_unknown(
    downloaded: &Mutex<HashMap<DownloadedUpdateId, DownloadedRecord>>,
    update_id: DownloadedUpdateId,
) {
    if let Ok(mut downloaded) = downloaded.lock()
        && let Some(record) = downloaded.get_mut(&update_id)
    {
        record.phase = DownloadedPhase::OutcomeUnknown;
    }
}

/// 保留 SDK 原生错误类型；panic 转成同一错误边界后再用文件快照判定实际替换结果。
fn perform_install(
    plan: InstallPlan,
    bytes: Vec<u8>,
    recovery: &super::recovery::RecoveryStore,
) -> InstallOutcome {
    let _ = recovery;
    #[cfg(target_os = "linux")]
    {
        let before = match appimage_snapshot(&plan.update.extract_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return InstallOutcome {
                    result: UpdateInstallResult::NotStarted(AppError::infrastructure_with_source(
                        "无法确认当前 AppImage，安装器未启动",
                        error,
                    )),
                    installed_target_sha256: None,
                };
            }
        };
        let sdk_result: cargo_packager_updater::Result<()> =
            match catch_unwind(AssertUnwindSafe(|| plan.update.install(bytes))) {
                Ok(result) => result,
                Err(_) => Err(io::Error::other("updater SDK panicked").into()),
            };
        return reconcile_appimage_install(sdk_result, &plan.update.extract_path, before);
    }

    #[cfg(windows)]
    {
        if !matches!(plan.update.format, UpdateFormat::Nsis) {
            return InstallOutcome {
                result: UpdateInstallResult::NotStarted(AppError::unsupported(
                    "当前只允许通过 NSIS 安装更新",
                )),
                installed_target_sha256: None,
            };
        }
        return match plan.update.install(bytes) {
            // SDK 0.2.3 的 NSIS 分支只在创建、写入暂存安装器时返回 Err；启动失败会 panic，
            // 成功启动后立即 process::exit。因此此处的 Err 可证明安装进程尚未启动。
            Err(error) => InstallOutcome {
                result: UpdateInstallResult::NotStarted(AppError::infrastructure_with_source(
                    "NSIS 安装器未启动",
                    error,
                )),
                installed_target_sha256: None,
            },
            // 固定 SDK 在成功启动后应退出进程；若未来返回，不能据此假定安装已完成。
            Ok(()) => InstallOutcome {
                result: UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown(
                    "NSIS 安装器已返回但应用仍在运行，安装状态需要确认",
                )),
                installed_target_sha256: None,
            },
        };
    }

    #[cfg(target_os = "macos")]
    {
        let installed_target = plan.update.extract_path.join("Contents/MacOS/kerminal");
        let before = recovery.target_sha256(&installed_target).ok();
        return match plan.update.install(bytes) {
            Ok(()) => {
                let after = recovery.target_sha256(&installed_target).ok();
                let digest = match (before.as_deref(), after.as_deref()) {
                    (Some(before), Some(after)) if before != after => Some(after.to_owned()),
                    _ => None,
                };
                InstallOutcome {
                    result: UpdateInstallResult::Installed(
                        UpdateInstallDisposition::RestartRequired,
                    ),
                    installed_target_sha256: digest,
                }
            }
            // SDK 会先解包并替换 .app；任何错误都不能证明备份回滚已经完整。
            Err(error) => InstallOutcome {
                result: UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                    "macOS 应用替换结果需要人工核实",
                    error,
                )),
                installed_target_sha256: None,
            },
        };
    }

    #[allow(unreachable_code)]
    InstallOutcome {
        result: UpdateInstallResult::NotStarted(AppError::unsupported("当前平台不支持应用内更新")),
        installed_target_sha256: None,
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AppImageSnapshot {
    sha256: [u8; 32],
    mode: u32,
}

/// 对目标文件计算 SHA-256 和执行权限，供 SDK 原位替换后的事实核对使用。
#[cfg(target_os = "linux")]
fn appimage_snapshot(path: &Path) -> io::Result<AppImageSnapshot> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AppImage target must be a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > 1024 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AppImage exceeds the update target size limit",
        ));
    }
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut read_total = 0_u64;
    loop {
        let received = reader.read(&mut buffer)?;
        if received == 0 {
            break;
        }
        read_total = read_total
            .checked_add(received as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "AppImage size overflow"))?;
        if read_total > 1024 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "AppImage exceeds the update target size limit",
            ));
        }
        hasher.update(&buffer[..received]);
    }
    Ok(AppImageSnapshot {
        sha256: hasher.finalize().into(),
        mode: metadata.permissions().mode() & 0o7777,
    })
}

/// 用 SDK 返回值与实际 AppImage 前后快照区分无副作用、文件已替换和未知状态。
#[cfg(target_os = "linux")]
fn reconcile_appimage_install<E>(
    sdk_result: Result<(), E>,
    target: &Path,
    before: AppImageSnapshot,
) -> InstallOutcome
where
    E: std::error::Error + Send + Sync + 'static,
{
    let sdk_error = sdk_result.err();
    let after = appimage_snapshot(target);
    match (sdk_error, after) {
        (None, Ok(after)) => {
            let digest = (after.sha256 != before.sha256 && after.mode & 0o111 != 0)
                .then(|| hex_sha256(&after.sha256));
            InstallOutcome {
                result: UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired),
                installed_target_sha256: digest,
            }
        }
        (Some(error), Ok(after)) if after == before => InstallOutcome {
            result: UpdateInstallResult::FailedNoEffect(AppError::infrastructure_with_source(
                "AppImage 更新失败，原文件完整保留",
                error,
            )),
            installed_target_sha256: None,
        },
        (Some(error), Ok(_)) => InstallOutcome {
            result: UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                "AppImage 替换结果需要人工核实",
                error,
            )),
            installed_target_sha256: None,
        },
        (None, Err(error)) => InstallOutcome {
            result: UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                "无法读取 AppImage 替换结果",
                error,
            )),
            installed_target_sha256: None,
        },
        (Some(error), Err(_read_error)) => InstallOutcome {
            result: UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
                "AppImage 替换结果需要人工核实",
                error,
            )),
            installed_target_sha256: None,
        },
    }
}

/// 将 SDK 提供的 SHA-256 字节摘要转成恢复收据要求的小写十六进制文本。
#[cfg(target_os = "linux")]
fn hex_sha256(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = String::with_capacity(64);
    for byte in bytes {
        digest.push(HEX[(byte >> 4) as usize] as char);
        digest.push(HEX[(byte & 0x0f) as usize] as char);
    }
    digest
}

/// 只在启动期定位可更新运行形态的真实目标文件，macOS 按 bundle 组件精确匹配。
fn current_runtime_target_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("APPIMAGE").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsStr;

        let executable = std::env::current_exe().ok()?;
        let macos_dir = executable.parent()?;
        let contents_dir = macos_dir.parent()?;
        if executable.file_name()? != OsStr::new("kerminal")
            || macos_dir.file_name()? != OsStr::new("MacOS")
            || contents_dir.file_name()? != OsStr::new("Contents")
        {
            return None;
        }
        return Some(executable);
    }
    #[cfg(windows)]
    {
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

/// 等待共享操作槽时也监听取消，不能让已取消请求排在长下载之后继续占队。
async fn lock_or_cancel<'a>(
    gate: &'a AsyncMutex<()>,
    cancellation: &CancellationToken,
    message: &'static str,
) -> AppResult<tokio::sync::MutexGuard<'a, ()>> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AppError::cancelled(message)),
        guard = gate.lock() => Ok(guard),
    }
}

#[cfg(test)]
#[path = "../../../tests/support/updater_runtime_unit.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/support/updater_acceptance_unit.rs"]
mod acceptance_tests;
