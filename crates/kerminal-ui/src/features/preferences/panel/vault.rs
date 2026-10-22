//! Vault key 导入、恢复、导出与轮换的安全 UI 用例协调。
//!
//! @author kongweiguang

use std::path::{Path, PathBuf};

use gpui_kit::{Context, PathPromptOptions};
use kerminal_application::{
    AppError, PublicErrorCode,
    connections::credentials::{
        VaultKeyAvailability, VaultKeyExportMode, VaultKeyManagementService,
        VaultKeyOperationReceipt, VaultRecoveryReason,
    },
};

use super::PreferencesPanel;

#[derive(Clone)]
enum VaultCommand {
    Create,
    Export(PathBuf),
    ImportPreview(PathBuf),
    ImportCommit(PathBuf),
    RestorePreview(PathBuf, PathBuf),
    RestoreCommit(PathBuf, PathBuf),
    RotatePreview,
    RotateCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VaultAction {
    Ensure,
    Create,
    Import,
    Export,
    Restore,
    Rotate,
}

/// 只允许与最新已读取 readiness 匹配的动作。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::features::preferences) struct VaultActionPolicy {
    pub(in crate::features::preferences) ensure: bool,
    pub(in crate::features::preferences) create: bool,
    pub(in crate::features::preferences) import: bool,
    pub(in crate::features::preferences) export: bool,
    pub(in crate::features::preferences) restore: bool,
    pub(in crate::features::preferences) rotate: bool,
}

/// 将 typed readiness 投影为 UI 和事件回调共用的操作门禁。
pub(in crate::features::preferences) fn vault_action_policy(
    availability: Option<&VaultKeyAvailability>,
    read_only: bool,
) -> VaultActionPolicy {
    let mut policy = VaultActionPolicy::default();
    match availability {
        Some(VaultKeyAvailability::NotInitialized) => {
            policy.ensure = true;
            policy.create = true;
        }
        Some(VaultKeyAvailability::Ready { .. } | VaultKeyAvailability::Created { .. }) => {
            policy.ensure = true;
            policy.import = true;
            policy.export = true;
            policy.rotate = true;
        }
        Some(VaultKeyAvailability::MissingForExistingVault) => {
            policy.import = true;
            policy.restore = true;
        }
        Some(VaultKeyAvailability::RecoveryRequired { .. }) => policy.restore = true,
        None => {}
    }

    // OutcomeUnknown 时 availability 会被清空；显式刷新后按新的 typed readiness
    // 开放当前状态合法的入口，不复用此前的候选或文件选择结果。
    if read_only {
        policy = VaultActionPolicy::default();
    }
    policy
}

/// 测试与异步回调都可用同一映射判断具体命令是否仍被当前状态允许。
fn command_action(command: &VaultCommand) -> VaultAction {
    match command {
        VaultCommand::Create => VaultAction::Create,
        VaultCommand::Export(_) => VaultAction::Export,
        VaultCommand::ImportPreview(_) | VaultCommand::ImportCommit(_) => VaultAction::Import,
        VaultCommand::RestorePreview(_, _) | VaultCommand::RestoreCommit(_, _) => {
            VaultAction::Restore
        }
        VaultCommand::RotatePreview | VaultCommand::RotateCommit => VaultAction::Rotate,
    }
}

/// 事件到达时再次检查当前门禁，避免迟到的文件选择结果使用旧 readiness。
fn command_allowed(policy: VaultActionPolicy, action: VaultAction) -> bool {
    match action {
        VaultAction::Ensure => policy.ensure,
        VaultAction::Create => policy.create,
        VaultAction::Import => policy.import,
        VaultAction::Export => policy.export,
        VaultAction::Restore => policy.restore,
        VaultAction::Rotate => policy.rotate,
    }
}

impl PreferencesPanel {
    /// 读取 typed vault readiness；只检查状态，不在设置面板打开时自动创建密钥。
    pub(in crate::features::preferences) fn refresh_vault_availability(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.vault_loading || self.vault_busy {
            return;
        }
        let Some(service) = self.vault_keys.clone() else {
            self.vault_error = Some(vault_text(
                "Vault 密钥服务未由应用装配。",
                "The Vault key service is unavailable.",
            ));
            cx.notify();
            return;
        };

        self.vault_loading = true;
        self.vault_error = None;
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|_| async move { service.availability().await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.vault_loading = false;
                match result {
                    Ok(availability) => {
                        let ready = matches!(
                            availability,
                            VaultKeyAvailability::Ready { .. }
                                | VaultKeyAvailability::Created { .. }
                        );
                        panel.vault_availability = Some(availability);
                        panel.vault_error = None;
                        if ready {
                            panel.vault_outcome_unknown = false;
                            panel.vault_status = None;
                        }
                        panel.clear_vault_candidates_not_allowed();
                    }
                    Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                        panel.mark_vault_outcome_unknown(&error);
                    }
                    Err(error) => panel.vault_error = Some(super::localized_error(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 使用系统文件选择器选取 key 文档，导入前先验证且不改动现有文件。
    pub(in crate::features::preferences) fn choose_vault_import(&mut self, cx: &mut Context<Self>) {
        if self.vault_busy || self.vault_loading || !self.vault_command_allowed(VaultAction::Import)
        {
            return;
        }
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::locale::text(
                "选择要验证的 Vault key 文件",
                "Choose the Vault key file to validate",
            )),
        });
        cx.spawn(async move |this, cx| {
            let selected = selected.await;
            let _ = this.update(cx, |panel, cx| match selected {
                Ok(Ok(Some(mut paths))) => {
                    if let Some(path) = paths.pop() {
                        panel.run_vault_command(VaultCommand::ImportPreview(path), cx);
                    } else {
                        panel.vault_error = Some(vault_text(
                            "文件选择器未返回路径。",
                            "The file picker returned no path.",
                        ));
                        cx.notify();
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(_)) => {
                    panel.vault_error = Some(vault_text(
                        "系统文件选择器暂不可用。",
                        "The system file picker is unavailable.",
                    ));
                    cx.notify();
                }
                Err(_) => {
                    panel.vault_error = Some(vault_text(
                        "文件选择器未返回结果，请重试。",
                        "The file picker returned no result. Try again.",
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 导出只通过 application 原子写入，并要求系统对话框返回新建目标路径。
    pub(in crate::features::preferences) fn choose_vault_export(&mut self, cx: &mut Context<Self>) {
        if self.vault_busy || self.vault_loading || !self.vault_command_allowed(VaultAction::Export)
        {
            return;
        }
        let selected = cx.prompt_for_new_path(Path::new("."), Some("vault-key.toml"));
        cx.spawn(async move |this, cx| {
            let selected = selected.await;
            let _ = this.update(cx, |panel, cx| match selected {
                Ok(Ok(Some(path))) => panel.run_vault_command(VaultCommand::Export(path), cx),
                Ok(Ok(None)) => {}
                Ok(Err(_)) => {
                    panel.vault_error = Some(vault_text(
                        "系统保存对话框暂不可用。",
                        "The system save dialog is unavailable.",
                    ));
                    cx.notify();
                }
                Err(_) => {
                    panel.vault_error = Some(vault_text(
                        "保存位置选择器未返回结果，请重试。",
                        "The save dialog returned no location. Try again.",
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 恢复 pair 先分别选择 key 与密文文件，随后执行 dry-run 再等待用户显式提交。
    pub(in crate::features::preferences) fn choose_vault_restore_pair(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.vault_busy
            || self.vault_loading
            || !self.vault_command_allowed(VaultAction::Restore)
        {
            return;
        }
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::locale::text(
                "选择 Vault key 备份文件",
                "Choose the Vault key backup file",
            )),
        });
        cx.spawn(async move |this, cx| {
            let selected = selected.await;
            let key_path = match selected {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                Ok(Ok(None)) => None,
                _ => {
                    let _ = this.update(cx, |panel, cx| {
                        panel.vault_error = Some(vault_text(
                            "无法读取所选 key 文件路径。",
                            "Could not read the selected key file path.",
                        ));
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(key_path) = key_path else { return };
            let _ = this.update(cx, |panel, cx| {
                panel.choose_vault_restore_document(key_path, cx)
            });
        })
        .detach();
    }

    /// 在同一用户流程中选择成对密文文件，保持 key 与 vault 文件来源明确。
    fn choose_vault_restore_document(&mut self, key_path: PathBuf, cx: &mut Context<Self>) {
        if self.vault_busy
            || self.vault_loading
            || !self.vault_command_allowed(VaultAction::Restore)
        {
            return;
        }
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::locale::text(
                "选择与 key 配套的加密 Vault 文件",
                "Choose the encrypted Vault file matching this key",
            )),
        });
        cx.spawn(async move |this, cx| {
            let selected = selected.await;
            let _ = this.update(cx, |panel, cx| match selected {
                Ok(Ok(Some(mut paths))) => {
                    if let Some(vault_path) = paths.pop() {
                        panel.run_vault_command(
                            VaultCommand::RestorePreview(key_path, vault_path),
                            cx,
                        );
                    } else {
                        panel.vault_error = Some(vault_text(
                            "文件选择器未返回路径。",
                            "The file picker returned no path.",
                        ));
                        cx.notify();
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(_)) => {
                    panel.vault_error = Some(vault_text(
                        "系统文件选择器暂不可用。",
                        "The system file picker is unavailable.",
                    ));
                    cx.notify();
                }
                Err(_) => {
                    panel.vault_error = Some(vault_text(
                        "文件选择器未返回结果，请重试。",
                        "The file picker returned no result. Try again.",
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// dry-run 通过后，用户必须再次明确确认导入、恢复或轮换才会提交文件变更。
    pub(in crate::features::preferences) fn confirm_vault_import(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.vault_import_candidate.clone() {
            self.run_vault_command(VaultCommand::ImportCommit(path), cx);
        }
    }

    /// 只有配对校验完成后才提交恢复事务，避免选错一半文件覆盖当前可用密钥。
    pub(in crate::features::preferences) fn confirm_vault_restore(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if let Some((key_path, vault_path)) = self.vault_restore_candidate.clone() {
            self.run_vault_command(VaultCommand::RestoreCommit(key_path, vault_path), cx);
        }
    }

    /// vault rotation 先 dry-run 显示 entry 规模，用户二次确认后才重新加密所有条目。
    pub(in crate::features::preferences) fn preview_vault_rotation(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.run_vault_command(VaultCommand::RotatePreview, cx);
    }

    /// 用户查看 dry-run 回执后显式开始真实 key rotation。
    pub(in crate::features::preferences) fn confirm_vault_rotation(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.vault_rotation_verified {
            self.run_vault_command(VaultCommand::RotateCommit, cx);
        }
    }

    /// 对同一组应用用例操作集中设置 busy 门禁，防止窗口重复提交密钥事务。
    fn run_vault_command(&mut self, command: VaultCommand, cx: &mut Context<Self>) {
        if self.vault_busy || self.vault_loading {
            return;
        }
        if !self.vault_command_allowed(command_action(&command)) {
            self.vault_error = Some(vault_text(
                "Vault 状态已变化；请先检查当前状态，再重新验证操作。",
                "Vault readiness changed. Check the current status and validate the operation again.",
            ));
            cx.notify();
            return;
        }
        let Some(service) = self.vault_keys.clone() else {
            self.vault_error = Some(vault_text(
                "Vault 密钥服务未由应用装配。",
                "The Vault key service is unavailable.",
            ));
            cx.notify();
            return;
        };

        self.vault_busy = true;
        self.vault_error = None;
        self.vault_status = None;
        let executor = self.executor.clone();
        let completed_command = command.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { execute_vault_command(service, command).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.vault_busy = false;
                match result {
                    Ok(receipt) => panel.finish_vault_command(completed_command, receipt, cx),
                    Err(error) => {
                        if error.public_code() == PublicErrorCode::OutcomeUnknown {
                            panel.mark_vault_outcome_unknown(&error);
                        } else {
                            panel.vault_error = Some(super::localized_error(&error));
                            panel.clear_vault_candidates();
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 根据已执行命令更新确认状态；真实写入后重新读取 typed readiness。
    fn finish_vault_command(
        &mut self,
        command: VaultCommand,
        receipt: VaultKeyOperationReceipt,
        cx: &mut Context<Self>,
    ) {
        let result_text = crate::locale::text(
            format!(
                "影响 {} 个凭据条目{}。",
                receipt.entry_count,
                if receipt.backup_created {
                    "；已保留可恢复备份"
                } else {
                    ""
                }
            ),
            format!(
                "Affected {} credential entries{}.",
                receipt.entry_count,
                if receipt.backup_created {
                    "; a recoverable backup was kept"
                } else {
                    ""
                }
            ),
        );
        if matches!(
            &command,
            VaultCommand::Create
                | VaultCommand::ImportCommit(_)
                | VaultCommand::RestoreCommit(_, _)
                | VaultCommand::RotateCommit
        ) {
            self.vault_outcome_unknown = false;
        }
        match command {
            VaultCommand::Create => {
                self.vault_status = Some(
                    crate::locale::text(
                        format!("密钥已创建（{}）。{result_text}", receipt.key_id),
                        format!("Key created ({}). {result_text}", receipt.key_id),
                    )
                    .to_string(),
                );
                self.refresh_vault_availability(cx);
            }
            VaultCommand::Export(_) => {
                self.vault_status = Some(vault_text(
                    "密钥文件已安全导出到所选新路径。",
                    "The key backup was exported to the selected new path.",
                ));
            }
            VaultCommand::ImportPreview(path) => {
                self.vault_import_candidate = Some(path);
                self.vault_status = Some(
                    crate::locale::text(
                        format!("密钥验证通过，尚未写入。{result_text}"),
                        format!("Key validation passed; nothing was written. {result_text}"),
                    )
                    .to_string(),
                );
            }
            VaultCommand::ImportCommit(_) => {
                self.vault_import_candidate = None;
                self.vault_status = Some(
                    crate::locale::text(
                        format!("密钥导入完成。{result_text}"),
                        format!("Key import completed. {result_text}"),
                    )
                    .to_string(),
                );
                self.refresh_vault_availability(cx);
            }
            VaultCommand::RestorePreview(key, vault) => {
                self.vault_restore_candidate = Some((key, vault));
                self.vault_status = Some(crate::locale::text(
                    format!("密钥与加密 Vault 配对验证通过，尚未写入。{result_text}"),
                    format!("Key and encrypted Vault pair validated; nothing was written. {result_text}"),
                ).to_string());
            }
            VaultCommand::RestoreCommit(_, _) => {
                self.vault_restore_candidate = None;
                self.vault_status = Some(
                    crate::locale::text(
                        format!("Vault 配对恢复完成。{result_text}"),
                        format!("Vault pair restore completed. {result_text}"),
                    )
                    .to_string(),
                );
                self.refresh_vault_availability(cx);
            }
            VaultCommand::RotatePreview => {
                self.vault_rotation_verified = true;
                self.vault_status = Some(
                    crate::locale::text(
                        format!("轮换预检通过，尚未更改密钥。{result_text}"),
                        format!("Rotation preview passed; no key was changed. {result_text}"),
                    )
                    .to_string(),
                );
            }
            VaultCommand::RotateCommit => {
                self.vault_rotation_verified = false;
                self.vault_status = Some(
                    crate::locale::text(
                        format!("密钥轮换完成。{result_text}"),
                        format!("Key rotation completed. {result_text}"),
                    )
                    .to_string(),
                );
                self.refresh_vault_availability(cx);
            }
        }
        cx.notify();
    }

    /// 执行器只调用应用服务；path 本身由用户系统对话框选出，UI 不读写密钥文件。
    pub(in crate::features::preferences) fn ensure_vault_key(&mut self, cx: &mut Context<Self>) {
        if self.vault_busy || self.vault_loading || !self.vault_command_allowed(VaultAction::Ensure)
        {
            self.vault_error = Some(vault_text(
                "当前 Vault 状态不允许确保密钥；请先刷新状态。",
                "Current Vault readiness does not allow this action. Refresh the status first.",
            ));
            cx.notify();
            return;
        }
        let Some(service) = self.vault_keys.clone() else {
            self.vault_error = Some(vault_text(
                "Vault 密钥服务未由应用装配。",
                "The Vault key service is unavailable.",
            ));
            cx.notify();
            return;
        };
        self.vault_busy = true;
        self.vault_error = None;
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|_| async move { service.ensure().await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.vault_busy = false;
                match result {
                    Ok(availability) => {
                        panel.vault_status = Some(availability_label(&availability));
                        if matches!(
                            availability,
                            VaultKeyAvailability::Ready { .. }
                                | VaultKeyAvailability::Created { .. }
                        ) {
                            panel.vault_outcome_unknown = false;
                        }
                        panel.vault_availability = Some(availability);
                        panel.clear_vault_candidates_not_allowed();
                    }
                    Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                        panel.mark_vault_outcome_unknown(&error);
                    }
                    Err(error) => panel.vault_error = Some(super::localized_error(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 显式创建只适用于空工作区的首个密钥，冲突由 application 返回并呈现在页面。
    pub(in crate::features::preferences) fn create_vault_key(&mut self, cx: &mut Context<Self>) {
        self.run_vault_command(VaultCommand::Create, cx);
    }

    /// 后端回报未知提交时丢弃旧预检和状态，防止 UI 用过期回执重放写操作。
    ///
    /// @author kongweiguang
    fn mark_vault_outcome_unknown(&mut self, error: &AppError) {
        self.vault_outcome_unknown = true;
        self.vault_availability = None;
        self.vault_status = None;
        self.vault_error = Some(super::localized_error(error));
        self.clear_vault_candidates();
    }

    /// 清理所有依赖旧 readiness 的 dry-run 候选，未知结果后必须重新验证。
    fn clear_vault_candidates(&mut self) {
        self.vault_import_candidate = None;
        self.vault_restore_candidate = None;
        self.vault_rotation_verified = false;
    }

    /// readiness 改变后撤销不再适用的预检确认，防止跨状态复用旧候选。
    fn clear_vault_candidates_not_allowed(&mut self) {
        let policy = vault_action_policy(
            self.vault_availability.as_ref(),
            self.recovery.mode == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
        );
        if !policy.import {
            self.vault_import_candidate = None;
        }
        if !policy.restore {
            self.vault_restore_candidate = None;
        }
        if !policy.rotate {
            self.vault_rotation_verified = false;
        }
    }

    /// 只依据当前已读取状态和配置恢复模式判定入口，迟到事件也执行相同检查。
    fn vault_command_allowed(&self, action: VaultAction) -> bool {
        command_allowed(
            vault_action_policy(
                self.vault_availability.as_ref(),
                self.recovery.mode
                    == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
            ),
            action,
        )
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_vault_policy.rs"]
mod tests;

/// 将一次 UI 操作映射到 VaultKeyManagementService，不把明文 key 或文件内容带入视图。
async fn execute_vault_command(
    service: VaultKeyManagementService,
    command: VaultCommand,
) -> Result<VaultKeyOperationReceipt, AppError> {
    let result = match command {
        VaultCommand::Create => service.create().await,
        VaultCommand::Export(path) => {
            service
                .export_to_path(path, VaultKeyExportMode::CreateNew)
                .await
        }
        VaultCommand::ImportPreview(path) => service.import_from_path(path, true).await,
        VaultCommand::ImportCommit(path) => service.import_from_path(path, false).await,
        VaultCommand::RestorePreview(key, vault) => {
            service.restore_pair_from_paths(key, vault, true).await
        }
        VaultCommand::RestoreCommit(key, vault) => {
            service.restore_pair_from_paths(key, vault, false).await
        }
        VaultCommand::RotatePreview => service.rotate(true).await,
        VaultCommand::RotateCommit => service.rotate(false).await,
    }?;
    Ok(result)
}

/// 把有限 readiness enum 映射为不含配置路径和密钥正文的用户说明。
pub(in crate::features::preferences) fn availability_label(
    availability: &VaultKeyAvailability,
) -> String {
    match availability {
        VaultKeyAvailability::NotInitialized => {
            crate::locale::text("Vault 尚未初始化。", "Vault is not initialized.").to_string()
        }
        VaultKeyAvailability::Ready { key_id } => crate::locale::text(
            format!("Vault 可用，密钥标识 {key_id}。"),
            format!("Vault is ready; key ID {key_id}."),
        )
        .to_string(),
        VaultKeyAvailability::Created { key_id } => crate::locale::text(
            format!("Vault 密钥已创建，标识 {key_id}。"),
            format!("Vault key created; ID {key_id}."),
        )
        .to_string(),
        VaultKeyAvailability::MissingForExistingVault => crate::locale::text(
            "检测到加密凭据，但缺少可用密钥；请导入原密钥或恢复已验证的文件配对。",
            "Encrypted credentials exist but the key is missing. Import the original key or restore a validated file pair.",
        )
        .to_string(),
        VaultKeyAvailability::RecoveryRequired { reason } => crate::locale::text(
            format!("Vault 需要显式恢复：{}。", recovery_reason_label(*reason, true)),
            format!("Vault recovery is required: {}.", recovery_reason_label(*reason, false)),
        )
        .to_string(),
    }
}

/// 只翻译稳定错误枚举，避免向界面泄露 vault 文件正文或系统绝对路径。
pub(super) fn recovery_reason_label(reason: VaultRecoveryReason, chinese: bool) -> &'static str {
    let (zh, en) = match reason {
        VaultRecoveryReason::InvalidKey => ("密钥文件格式无效", "invalid key file format"),
        VaultRecoveryReason::InvalidVault => ("加密数据文件无效", "invalid encrypted data file"),
        VaultRecoveryReason::KeyMismatch => ("密钥与数据不匹配", "key does not match the data"),
        VaultRecoveryReason::PairTransaction => {
            ("配对事务需要恢复", "pair transaction needs recovery")
        }
        VaultRecoveryReason::UnsafePath => ("所选路径不安全", "selected path is unsafe"),
        VaultRecoveryReason::StorageUnavailable => ("存储暂不可用", "storage is unavailable"),
    };
    if chinese { zh } else { en }
}

/// 将 UI 自己生成的稳定消息交给全窗口 locale 选择，application 错误仍显示脱敏原文。
fn vault_text(zh_cn: &str, en_us: &str) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}
