//! 连接导航状态、异步用例与主机生命周期事件。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Window,
    component::input::InputState,
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor, ConfigurationChangeService, ProfileService, PublicErrorCode,
    configuration_changes::{ConfigurationChangeEvent, ConfigurationChangeMessage},
    connections::{
        HostKeyChallenge, HostKeyPeer, HostKeyRotationRequest, HostKeyStatus, HostKeyTrustRequest,
        RdpClientLifecycle, RdpLaunchId, RdpLaunchReceipt, RdpLaunchRequest, RdpLaunchService,
        RdpLaunchStatus, RdpLaunchStatusMessage, RdpLaunchStatusService, RemoteConnectionService,
        SerialPortDescriptor, SerialPortService,
        management::{
            CredentialCleanupStatus, HostAuthenticationMode, HostCredentialChange,
            HostCredentialSlot, HostDeletionReceipt, HostDraft, HostGroup,
            HostGroupDeletionReceipt, HostGroupDraft, HostGroupSnapshot, HostManagementService,
            HostSaveRecoveryToken, ManagedHost, RecoverableHostSaveOutcome,
        },
        probe::{
            ConnectionProbeOutcome, ConnectionProbeRequest, ConnectionProbeService,
            ProbeHostKeyConfirmation,
        },
    },
    platform::external_launch::{
        ExternalLaunchIntent, ExternalLaunchRequest, ExternalLaunchService, ExternalRoutedTarget,
        ExternalTargetClassification,
    },
};
use kerminal_domain::{
    connections::{CredentialRef, RemoteProtocol, SshAuthentication},
    files::FileEndpoint,
    identity::HostId,
    preferences::{ConfigRevision, ConfigurationMode, ConfigurationRecoveryState},
    sessions::{SessionTarget, SshSessionTarget, TelnetSessionTarget},
};
use tokio_util::sync::CancellationToken;

/// 主机保存成功后推进目录代次，使保存前启动的异步快照不能回写旧列表。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HostSnapshotReadGeneration(u64);

impl HostSnapshotReadGeneration {
    /// 开始读取时捕获当前代次；完成时只有仍为当前代次的数据可以进入 UI 快照。
    ///
    /// @author kongweiguang
    fn capture(&self) -> u64 {
        self.0
    }

    /// 仅在收到已确认保存回执后推进，阻止更早读取覆盖本地注入的提交结果。
    ///
    /// @author kongweiguang
    fn invalidate_after_confirmed_save(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// 定向读回是当前 HostId 的权威快照，保存前启动的整目录读取不得覆盖它。
    /// @author kongweiguang
    fn invalidate_after_authoritative_readback(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// 保存前开始的读取只允许丢弃并重读，不能更新主机、分组或其 revision。
    ///
    /// @author kongweiguang
    fn accepts(&self, read_generation: u64) -> bool {
        self.0 == read_generation
    }
}

/// 同一 HostId 的确认保存替换旧行，新建主机则追加到当前可见快照供后续预检使用。
///
/// @author kongweiguang
fn upsert_saved_host_row(rows: &mut Vec<ManagedHost>, saved_host: ManagedHost) {
    rows.retain(|row| row.id != saved_host.id);
    rows.push(saved_host);
}

/// 仅让当前读取提交完整主机/分组快照；迟到读取不得短暂覆盖保存注入的主机行。
///
/// @author kongweiguang
fn apply_host_snapshot_read_if_current(
    generation: &HostSnapshotReadGeneration,
    read_generation: u64,
    rows: &mut Vec<ManagedHost>,
    groups: &mut Vec<HostGroup>,
    group_revision: &mut Option<ConfigRevision>,
    loaded_rows: Vec<ManagedHost>,
    loaded_groups: HostGroupSnapshot,
) -> bool {
    if !generation.accepts(read_generation) {
        return false;
    }
    *rows = loaded_rows;
    *groups = loaded_groups.groups;
    *group_revision = loaded_groups.revision;
    true
}
use super::form::{
    HostEditor, HostEditorField, HostEditorSection, HostEditorValidationIssue, clear_textarea,
    create_input, set_input_placeholder,
};

/// 将领域校验字段映射回现有控件句柄；没有可聚焦控件时仍由锚点滚动展示错误。
/// @author kongweiguang
fn editor_validation_focus_handle(
    editor: &HostEditor,
    field: HostEditorField,
    cx: &gpui_kit::App,
) -> Option<FocusHandle> {
    let focus = match field {
        HostEditorField::Name => editor.inputs.name.read(cx).focus_handle(cx),
        HostEditorField::Group => editor.group_select.read(cx).focus_handle(cx),
        HostEditorField::Host => editor.inputs.host.read(cx).focus_handle(cx),
        HostEditorField::Port => editor.inputs.port.read(cx).focus_handle(cx),
        HostEditorField::Username => editor.inputs.username.read(cx).focus_handle(cx),
        HostEditorField::Authentication => match editor.authentication_mode {
            HostAuthenticationMode::Password => editor.inputs.secret.read(cx).focus_handle(cx),
            HostAuthenticationMode::PrivateKey => editor.inputs.key_path.read(cx).focus_handle(cx),
            HostAuthenticationMode::Agent => return None,
        },
        HostEditorField::PrivateKey => editor.inputs.private_key.read(cx).focus_handle(cx),
        HostEditorField::Note => editor.inputs.note.read(cx).focus_handle(cx),
        HostEditorField::SerialBaud => editor.inputs.serial_baud.read(cx).focus_handle(cx),
        HostEditorField::Network => editor.inputs.connect_timeout.read(cx).focus_handle(cx),
        HostEditorField::Terminal => editor.inputs.term.read(cx).focus_handle(cx),
        HostEditorField::Transfer => editor.inputs.sftp_concurrency.read(cx).focus_handle(cx),
        HostEditorField::Display => editor.inputs.rdp_width.read(cx).focus_handle(cx),
        HostEditorField::Proxy => editor.inputs.proxy_host.read(cx).focus_handle(cx),
        HostEditorField::JumpHosts => {
            return editor
                .jumps
                .first()
                .map(|jump| jump.host.read(cx).focus_handle(cx));
        }
        HostEditorField::Tags => editor.inputs.tags.read(cx).focus_handle(cx),
    };
    Some(focus)
}

/// 连接面板只发布带明确 host/target 的事件，Root 不得从当前 Tab 猜测连接目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionsEvent {
    PickHostFile {
        generation: u64,
        target: HostFilePickerTarget,
    },
    HostSelected {
        host_id: HostId,
    },
    HostSaved {
        host_id: HostId,
    },
    LocalProfileSaved {
        profile_id: String,
    },
    ConnectRequested {
        host_id: HostId,
        target: SessionTarget,
        intent_id: Option<String>,
    },
    ConnectLocalProfileRequested {
        profile_id: String,
    },
    SftpOpenStarted {
        request_id: String,
    },
    SftpHostCreationSaved {
        request_id: String,
        host_id: HostId,
        can_open_sftp: bool,
    },
    SftpHostCreationCancelled {
        request_id: String,
    },
    SftpTransferTabOpenStarted {
        request_id: String,
    },
    OpenSftpRequested {
        request_id: String,
        endpoint: FileEndpoint,
        initial_local_directory: Option<std::path::PathBuf>,
    },
    CreateSftpTransferTabRequested {
        request_id: String,
        endpoint: FileEndpoint,
        title: String,
        initial_local_directory: Option<PathBuf>,
    },
    RdpLaunchCompleted {
        host_id: HostId,
        receipt: RdpLaunchReceipt,
    },
    ExternalTargetReady {
        target: ExternalRoutedTarget,
    },
    GroupRenameCancelled,
    GroupRenameSaved,
}

/// 只有确认写入或明确采用读回时才发布导航选中意图，普通列表刷新不改变用户选择。
/// @author kongweiguang
pub(super) fn saved_selection_event(target: SavedSelectionTarget) -> ConnectionsEvent {
    match target {
        SavedSelectionTarget::Host(host_id) => ConnectionsEvent::HostSaved { host_id },
        SavedSelectionTarget::LocalProfile(profile_id) => {
            ConnectionsEvent::LocalProfileSaved { profile_id }
        }
    }
}

/// 保存完成后需要选择的稳定导航目标；远端 HostId 与本地 ProfileId 保持类型区分。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SavedSelectionTarget {
    Host(HostId),
    LocalProfile(String),
}

/// 文件选择的业务用途由编辑器声明，Root 只负责系统对话框，不自行猜测写回字段。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostFilePickerTarget {
    PrivateKey,
    LocalShell,
    LocalWorkingDirectory,
    SftpLocalDirectory,
}

/// 本地与远程共用一个编辑弹层，但草稿各自独立保存以免切换协议覆盖输入。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConnectionsEditorKind {
    Remote,
    Local,
}

/// 冲突重读只携带稳定实体 ID；不保存列表快照或表单实体的可变别名。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ConflictReloadTarget {
    Remote(HostId),
    Local(String),
}

/// 分组内联创建结束后恢复到原主机表单，分组弹层不取得或复制主机草稿所有权。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GroupEditorReturn {
    editor_kind: ConnectionsEditorKind,
}

/// 通过面板按钮发起的连接动作，在完成 SSH 信任预检前不进入 Root 或会话注册表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PendingAction {
    Terminal {
        host_id: HostId,
        target: SessionTarget,
        intent_id: Option<String>,
    },
    Sftp {
        endpoint: FileEndpoint,
        request_id: String,
    },
    SftpTransferTab {
        request_id: String,
        endpoint: FileEndpoint,
        title: String,
    },
    External(ExternalRoutedTarget),
}

/// SSH peer 尚未可信时暂存明确信任目标与后续连接意图。
#[derive(Clone)]
pub(super) struct PendingTrust {
    pub(super) challenge: HostKeyChallenge,
    pub(super) action: PendingAction,
    pub(super) display_host: Option<String>,
    pub(super) external_classification: Option<ExternalTargetClassification>,
    pub(super) second_confirmation: bool,
    pub(super) operation_busy: bool,
    pub(super) operation_cancellation: Option<CancellationToken>,
}

/// 仅保存安全、稳定的设备枚举反馈类别，不把底层错误路径透传到面板。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SerialPortScanIssue {
    ServiceUnavailable,
    Cancelled,
    EnumerationFailed,
}

/// 主机删除确认绑定原 revision，列表刷新或外部编辑后不会误删另一版本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingHostDeletion {
    pub(super) host_id: HostId,
    pub(super) revision: kerminal_domain::preferences::ConfigRevision,
}

/// 分组删除确认保留原 groups.toml revision；目标分组由用户在确认时显式选择。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingGroupDeletion {
    pub(super) group_id: String,
    pub(super) revision: kerminal_domain::preferences::ConfigRevision,
}

/// 未知分组写入只允许读回并由用户确认后再发起新的保存，不把刷新伪装成提交成功。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GroupSaveReconciliation {
    NeedsReadback,
    AwaitingUserDecision,
}

/// 只有成功读回才从“待核对”转入等待用户判断；任何读失败都维持写入封锁。
/// @author kongweiguang
fn group_save_reconciliation_after_readback(
    current: Option<GroupSaveReconciliation>,
    explicitly_requested_review: bool,
    readback_succeeded: bool,
) -> Option<GroupSaveReconciliation> {
    match (current, explicitly_requested_review, readback_succeeded) {
        (Some(GroupSaveReconciliation::NeedsReadback), true, true) => {
            Some(GroupSaveReconciliation::AwaitingUserDecision)
        }
        (state, _, _) => state,
    }
}

/// 只在编辑器仍显示读回结果时解除未知写入门禁，不把确认动作当作保存回执。
/// @author kongweiguang
fn group_save_reconciliation_after_acknowledgement(
    current: Option<GroupSaveReconciliation>,
    group_editor_active: bool,
) -> Option<GroupSaveReconciliation> {
    if group_editor_active && current == Some(GroupSaveReconciliation::AwaitingUserDecision) {
        None
    } else {
        current
    }
}

/// 未知主机保存只保留非明文候选与旧 revision；凭据动作保留槽位语义，不复制秘密。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingHostSaveRecovery {
    host_id: HostId,
    candidate: HostDraft,
    baseline_revision: Option<ConfigRevision>,
    credential_changes: Vec<(HostCredentialSlot, HostCredentialRecoveryAction)>,
    credential_cleanup: Option<HostSaveRecoveryToken>,
}

/// 读回只允许比较明确的替换/清除结果，未列出的凭据引用必须原样匹配候选。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostCredentialRecoveryAction {
    Replace,
    Clear,
}

/// 成功读回可能确认未提交、确认提交，或需要用户显式采用第三方/凭据状态。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostSaveReadbackDecision {
    ConfirmedUnsubmitted,
    ConfirmedCommitted,
    RequiresAdoption,
}

/// 入口替换门禁只由写入进行中或尚未核对的 typed recovery 驱动。
/// @author kongweiguang
fn host_editor_replacement_blocked(
    saving: bool,
    outcome_unknown: bool,
    recovery_present: bool,
) -> bool {
    saving || outcome_unknown || recovery_present
}

/// 重读按钮只在冲突有稳定目标且没有在途操作、确认弹层或未知结果时开放。
/// @author kongweiguang
fn conflict_editor_reload_enabled(has_conflict: bool, has_target: bool, busy: bool) -> bool {
    has_conflict && has_target && !busy
}

/// 重读确认或读回进行时锁定编辑操作，避免有效回执覆盖用户新输入。
/// @author kongweiguang
fn conflict_reload_blocks_editor_mutation(pending_confirmation: bool, in_progress: bool) -> bool {
    pending_confirmation || in_progress
}

/// 远程定向重读仅接受同一 HostId 和 generation 的当前请求，拒绝迟到读取回执。
/// @author kongweiguang
fn conflict_reload_response_matches(
    request_generation: u64,
    current_generation: u64,
    active_target: Option<&ConflictReloadTarget>,
    expected_target: &ConflictReloadTarget,
) -> bool {
    request_generation == current_generation && active_target == Some(expected_target)
}

/// 丢弃旧 HostId、旧代次或已解除的请求回执，避免迟到读取解锁另一份表单。
/// @author kongweiguang
fn host_save_readback_matches_pending(
    generation: u64,
    current_generation: u64,
    outcome_unknown: bool,
    recovery: Option<&PendingHostSaveRecovery>,
    host_id: &HostId,
) -> bool {
    generation == current_generation
        && outcome_unknown
        && recovery.is_some_and(|recovery| &recovery.host_id == host_id)
}

/// 只保存最新一次显式核对结果，包含缺失快照以区分新建尚未落盘和读取未完成。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostSaveReadback {
    generation: u64,
    host_id: HostId,
    host: Option<ManagedHost>,
    decision: HostSaveReadbackDecision,
}

/// 供用户审阅的读回摘要只包含连接标识和非秘密配置字段。
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HostSaveReadbackSummary {
    pub(super) host_id: String,
    pub(super) exists: bool,
    pub(super) name: Option<String>,
    pub(super) protocol: Option<RemoteProtocol>,
    pub(super) host: Option<String>,
    pub(super) port: Option<u16>,
    pub(super) username: Option<String>,
    pub(super) note: Option<String>,
}

/// 仅为当前显式核对代次生成只读摘要，避免渲染层接触候选秘密或凭据引用。
/// @author kongweiguang
fn host_save_readback_summary(
    readback: &HostSaveReadback,
    current_generation: u64,
    expected_host_id: &HostId,
) -> Option<HostSaveReadbackSummary> {
    if readback.generation != current_generation || &readback.host_id != expected_host_id {
        return None;
    }
    let fields = readback.host.as_ref().map(|host| {
        (
            host.name.clone(),
            host.protocol,
            host.endpoint.host.clone(),
            host.endpoint.port,
            host.endpoint.username.clone(),
            host.note.clone(),
        )
    });
    Some(HostSaveReadbackSummary {
        host_id: readback.host_id.to_string(),
        exists: fields.is_some(),
        name: fields.as_ref().map(|fields| fields.0.clone()),
        protocol: fields.as_ref().map(|fields| fields.1),
        host: fields.as_ref().map(|fields| fields.2.clone()),
        port: fields.as_ref().map(|fields| fields.3),
        username: fields.as_ref().map(|fields| fields.4.clone()),
        note: fields.and_then(|fields| fields.5),
    })
}

/// 将受控保存输入的动作转换为不含 SecretValue 的恢复意图。
/// @author kongweiguang
fn host_credential_recovery_actions(
    changes: &[HostCredentialChange],
) -> Vec<(HostCredentialSlot, HostCredentialRecoveryAction)> {
    changes
        .iter()
        .map(|change| match change {
            HostCredentialChange::Replace { slot, .. } => {
                (*slot, HostCredentialRecoveryAction::Replace)
            }
            HostCredentialChange::Clear { slot } => (*slot, HostCredentialRecoveryAction::Clear),
        })
        .collect()
}

/// 保存前按本次凭据动作判断有效私钥来源；新粘贴材料尚无 Vault 引用，不能被旧引用检查误拒。
/// 显式清除已有正文时仍需文件或新正文，避免把待删除引用当成可用密钥。
/// @author kongweiguang
fn private_key_save_has_material(
    authentication: Option<&SshAuthentication>,
    changes: &[HostCredentialChange],
) -> bool {
    let Some(SshAuthentication::PrivateKey { path, secret, .. }) = authentication else {
        return false;
    };
    let replacement = changes.iter().any(|change| {
        matches!(
            change,
            HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPrivateKey,
                ..
            }
        )
    });
    let cleared = changes.iter().any(|change| {
        matches!(
            change,
            HostCredentialChange::Clear {
                slot: HostCredentialSlot::TargetPrivateKey
            }
        )
    });
    path.is_some() || replacement || (secret.is_some() && !cleared)
}

/// 比较候选与同 HostId 的读回；replace ref 无法证明底层字节归属，因此转交显式采用。
/// baseline revision 未变或新建 ID 仍缺失时，安全地允许用户明确重试现有草稿。
/// @author kongweiguang
fn host_save_readback_decision(
    recovery: &PendingHostSaveRecovery,
    current: Option<&ManagedHost>,
) -> HostSaveReadbackDecision {
    if let Some(current) = current {
        if current.id != recovery.host_id {
            return HostSaveReadbackDecision::RequiresAdoption;
        }
        if recovery
            .baseline_revision
            .as_ref()
            .is_some_and(|revision| revision == &current.revision)
        {
            return HostSaveReadbackDecision::ConfirmedUnsubmitted;
        }
        let mut expected = recovery.candidate.clone();
        let actual = host_draft_from_managed_host(current);
        if recovery.credential_changes.iter().all(|(slot, action)| {
            reconcile_candidate_credential_reference(&mut expected, &actual, *slot, *action)
        }) && expected == actual
        {
            return if recovery
                .credential_changes
                .iter()
                .any(|(_, action)| *action == HostCredentialRecoveryAction::Replace)
            {
                HostSaveReadbackDecision::RequiresAdoption
            } else {
                HostSaveReadbackDecision::ConfirmedCommitted
            };
        }
        return HostSaveReadbackDecision::RequiresAdoption;
    }

    if recovery.baseline_revision.is_none() {
        HostSaveReadbackDecision::ConfirmedUnsubmitted
    } else {
        HostSaveReadbackDecision::RequiresAdoption
    }
}

/// 在恢复比较中只接受引用槽位与用户明确动作一致的变化，其它候选字段继续做全量相等比较。
/// @author kongweiguang
fn reconcile_candidate_credential_reference(
    expected: &mut HostDraft,
    actual: &HostDraft,
    slot: HostCredentialSlot,
    action: HostCredentialRecoveryAction,
) -> bool {
    let previous = draft_credential_reference(expected, slot).cloned();
    let current = draft_credential_reference(actual, slot).cloned();
    match action {
        HostCredentialRecoveryAction::Replace if current.is_some() && current != previous => {
            set_draft_credential_reference(expected, slot, current)
        }
        HostCredentialRecoveryAction::Clear if current.is_none() => {
            previous.is_none() || set_draft_credential_reference(expected, slot, None)
        }
        _ => false,
    }
}

/// 读回模型暂投影成同一草稿结构，使持久化结果与提交候选用一个明确相等契约比较。
/// @author kongweiguang
fn host_draft_from_managed_host(host: &ManagedHost) -> HostDraft {
    HostDraft {
        id: Some(host.id.clone()),
        group_id: host.group_id.clone(),
        name: host.name.clone(),
        note: host.note.clone(),
        protocol: host.protocol,
        endpoint: host.endpoint.clone(),
        authentication: host.authentication.clone(),
        authentication_mode: host.authentication_mode,
        rdp_password: host.rdp_password.clone(),
        route: host.route.clone(),
        serial: host.serial.clone(),
        tags: host.tags.clone(),
        is_favorite: host.is_favorite,
        sort_order: host.sort_order,
        connect_timeout_ms: host.connect_timeout_ms,
        keepalive_interval_ms: host.keepalive_interval_ms,
        terminal_options: host.terminal_options.clone(),
        sftp_options: host.sftp_options.clone(),
        rdp_display: host.rdp_display.clone(),
    }
}

/// 取某个凭据槽位的引用；秘密内容不进入恢复状态或相等比较。
/// @author kongweiguang
fn draft_credential_reference(
    draft: &HostDraft,
    slot: HostCredentialSlot,
) -> Option<&CredentialRef> {
    match slot {
        HostCredentialSlot::TargetPassword => match draft.authentication.as_ref() {
            Some(SshAuthentication::Password { secret }) => Some(secret),
            _ => None,
        },
        HostCredentialSlot::TargetPrivateKey => match draft.authentication.as_ref() {
            Some(SshAuthentication::PrivateKey { secret, .. }) => secret.as_ref(),
            _ => None,
        },
        HostCredentialSlot::TargetKeyPassphrase => match draft.authentication.as_ref() {
            Some(SshAuthentication::PrivateKey { passphrase, .. }) => passphrase.as_ref(),
            _ => None,
        },
        HostCredentialSlot::RdpPassword => draft.rdp_password.as_ref(),
        HostCredentialSlot::ProxyPassword => draft.route.proxy.as_ref()?.password.as_ref(),
        HostCredentialSlot::JumpPassword(index) => {
            let jump = draft.route.jump_hosts.get(index)?;
            match jump.authentication.as_ref()? {
                SshAuthentication::Password { secret } => Some(secret),
                _ => None,
            }
        }
        HostCredentialSlot::JumpPrivateKey(index) => {
            let jump = draft.route.jump_hosts.get(index)?;
            match jump.authentication.as_ref()? {
                SshAuthentication::PrivateKey { secret, .. } => secret.as_ref(),
                _ => None,
            }
        }
        HostCredentialSlot::JumpKeyPassphrase(index) => {
            let jump = draft.route.jump_hosts.get(index)?;
            match jump.authentication.as_ref()? {
                SshAuthentication::PrivateKey { passphrase, .. } => passphrase.as_ref(),
                _ => None,
            }
        }
    }
}

/// 将已核对的引用写入候选副本；结构不匹配时拒绝声称读回完整匹配。
/// @author kongweiguang
fn set_draft_credential_reference(
    draft: &mut HostDraft,
    slot: HostCredentialSlot,
    reference: Option<CredentialRef>,
) -> bool {
    match slot {
        HostCredentialSlot::TargetPassword => match draft.authentication.as_mut() {
            Some(SshAuthentication::Password { secret }) => {
                if let Some(reference) = reference {
                    *secret = reference;
                } else {
                    return false;
                }
                true
            }
            _ => false,
        },
        HostCredentialSlot::TargetPrivateKey => match draft.authentication.as_mut() {
            Some(SshAuthentication::PrivateKey { secret, .. }) => {
                *secret = reference;
                true
            }
            _ => false,
        },
        HostCredentialSlot::TargetKeyPassphrase => match draft.authentication.as_mut() {
            Some(SshAuthentication::PrivateKey { passphrase, .. }) => {
                *passphrase = reference;
                true
            }
            _ => false,
        },
        HostCredentialSlot::RdpPassword => {
            draft.rdp_password = reference;
            true
        }
        HostCredentialSlot::ProxyPassword => {
            let Some(proxy) = draft.route.proxy.as_mut() else {
                return false;
            };
            proxy.password = reference;
            true
        }
        HostCredentialSlot::JumpPassword(index) => {
            let Some(jump) = draft.route.jump_hosts.get_mut(index) else {
                return false;
            };
            let Some(SshAuthentication::Password { secret }) = jump.authentication.as_mut() else {
                return false;
            };
            if let Some(reference) = reference {
                *secret = reference;
            } else {
                return false;
            }
            true
        }
        HostCredentialSlot::JumpPrivateKey(index) => {
            let Some(jump) = draft.route.jump_hosts.get_mut(index) else {
                return false;
            };
            let Some(SshAuthentication::PrivateKey { secret, .. }) = jump.authentication.as_mut()
            else {
                return false;
            };
            *secret = reference;
            true
        }
        HostCredentialSlot::JumpKeyPassphrase(index) => {
            let Some(jump) = draft.route.jump_hosts.get_mut(index) else {
                return false;
            };
            let Some(SshAuthentication::PrivateKey { passphrase, .. }) =
                jump.authentication.as_mut()
            else {
                return false;
            };
            *passphrase = reference;
            true
        }
    }
}

/// 将核对事实限定到当前草稿，避免用无关分组列表暗示未知提交结果。
/// 创建按仓储实际的大小写不敏感重名规则匹配；编辑按稳定 ID 匹配，结果仅是当前事实。
/// @author kongweiguang
pub(super) fn group_save_review_facts(
    groups: &[HostGroup],
    draft_group_id: Option<&str>,
    draft_name: &str,
) -> String {
    let matching_groups = groups
        .iter()
        .filter(|group| match draft_group_id {
            Some(group_id) => group.id == group_id,
            None => group.name.eq_ignore_ascii_case(draft_name.trim()),
        })
        .collect::<Vec<_>>();

    if matching_groups.is_empty() {
        return match draft_group_id {
            Some(group_id) => crate::locale::text(
                format!("未找到原分组 ID {group_id}；草稿名称：{draft_name}"),
                format!("Original group ID {group_id} was not found; draft name: {draft_name}"),
            )
            .to_string(),
            None => crate::locale::text(
                format!("未找到与“{}”同名的分组。", draft_name.trim()),
                format!("No group named “{}” was found.", draft_name.trim()),
            )
            .to_string(),
        };
    }

    let mut facts = matching_groups
        .iter()
        .take(3)
        .map(|group| format!("{} · {}", group.id, group.name))
        .collect::<Vec<_>>();
    if matching_groups.len() > facts.len() {
        facts.push(
            crate::locale::text(
                format!("另有 {} 项", matching_groups.len() - 3),
                format!("{} more", matching_groups.len() - 3),
            )
            .to_string(),
        );
    }
    crate::locale::text(
        format!("当前配置匹配项（不代表上次保存成功）：{}", facts.join("；")),
        format!(
            "Current matching configuration (not proof of prior save success): {}",
            facts.join("; ")
        ),
    )
    .to_string()
}

/// 连接面板只区分主机详情与分组编辑，主机目录始终由独立左侧导航维护。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ConnectionsPresentation {
    #[default]
    HostDetails,
    GroupRename,
}

impl ConnectionsPresentation {
    /// 普通主机选择更新详情时不关闭分组编辑器，避免未提交名称随导航消失。
    ///
    /// @author kongweiguang
    fn show_host_details(&mut self) {
        if *self != Self::GroupRename {
            *self = Self::HostDetails;
        }
    }

    /// 创建与重命名共用全宽分组编辑器；退出后回到唯一的详情展示态。
    ///
    /// @author kongweiguang
    fn show_group_editor(&mut self) {
        *self = Self::GroupRename;
    }

    /// 关闭分组编辑器统一恢复详情态；Root generation 决定是否返回原中心工作区。
    ///
    /// @author kongweiguang
    fn finish_group_editor(&mut self) {
        if *self == Self::GroupRename {
            *self = Self::HostDetails;
        }
    }

    /// 专用重命名状态不绘制左侧目录，让名称编辑使用完整工作区宽度。
    ///
    /// @author kongweiguang
    pub(super) fn shows_group_editor(self) -> bool {
        self == Self::GroupRename
    }
}

/// 连接预检把面板取消和应用关闭两种来源合并到同一个底层令牌。
pub(super) struct ConnectionRequestCancellation {
    state: Arc<Mutex<ConnectionRequestCancellationState>>,
}

/// 保留任务令牌，使面板切换目标时能取消已经进入 application executor 的请求。
struct ConnectionRequestCancellationState {
    cancelled: bool,
    operation_token: Option<CancellationToken>,
}

impl Clone for ConnectionRequestCancellation {
    /// 异步回调持有同一取消状态，目标切换后可以中止正在运行的远端握手。
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl ConnectionRequestCancellation {
    /// 新请求从未取消状态开始，避免不同主机或重试共享旧连接令牌。
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnectionRequestCancellationState {
                cancelled: false,
                operation_token: None,
            })),
        }
    }

    /// 将面板请求令牌绑定为 executor 令牌的子项，同时保留两侧独立取消能力。
    pub(super) fn bind_to(&self, executor_token: &CancellationToken) -> CancellationToken {
        let operation_token = executor_token.child_token();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.operation_token = Some(operation_token.clone());
        if state.cancelled {
            operation_token.cancel();
        }
        operation_token
    }

    /// 取消具体预检任务；若任务尚未启动，后续绑定也会得到已取消令牌。
    fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        if let Some(operation_token) = &state.operation_token {
            operation_token.cancel();
        }
    }

    /// 检查面板或 executor 任一侧是否已取消该请求，以拒绝迟到结果。
    pub(super) fn is_cancelled(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled
            || state
                .operation_token
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
    }

    /// 区分面板换目标的取消与写入取消产生的 executor 子令牌取消，后者仍须接收提交回执。
    fn is_cancelled_by_request(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled
    }
}

/// 每次新目标或取消都会使旧结果失效，防止慢速 SSH 预检覆盖当前面板状态。
pub(super) struct ConnectionRequestEpoch {
    generation: u64,
    cancellation: ConnectionRequestCancellation,
}

impl ConnectionRequestEpoch {
    /// 初始状态没有在途任务，首个请求会创建独立 generation 和令牌。
    fn new() -> Self {
        Self {
            generation: 0,
            cancellation: ConnectionRequestCancellation::new(),
        }
    }

    /// 开始新请求前取消旧任务，并返回本次请求唯一的 generation/取消状态。
    fn begin(&mut self) -> (u64, ConnectionRequestCancellation) {
        self.cancellation.cancel();
        self.generation = self.generation.wrapping_add(1);
        self.cancellation = ConnectionRequestCancellation::new();
        (self.generation, self.cancellation.clone())
    }

    /// 使当前任务失效；即使其 future 尚未开始，后续也不会把旧结果交回面板。
    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cancellation.cancel();
    }

    /// 仅接受同一请求世代且两侧均未取消的完成结果。
    pub(super) fn is_current(
        &self,
        generation: u64,
        request: &ConnectionRequestCancellation,
    ) -> bool {
        self.generation == generation
            && Arc::ptr_eq(&self.cancellation.state, &request.state)
            && !request.is_cancelled()
    }

    /// 用户取消在途写入时允许原世代收取其完成/回读结果，但换目标仍会使回调失效。
    pub(super) fn is_current_after_user_cancel(
        &self,
        generation: u64,
        request: &ConnectionRequestCancellation,
    ) -> bool {
        self.generation == generation
            && Arc::ptr_eq(&self.cancellation.state, &request.state)
            && !request.is_cancelled_by_request()
    }

    /// 共享生命周期只读取当前请求世代与取消句柄，避免跨模块暴露计数器内部状态。
    pub(super) fn snapshot(&self) -> (u64, ConnectionRequestCancellation) {
        (self.generation, self.cancellation.clone())
    }
}

/// 主机目录和分组快照通过 application 读取；UI 状态只含可安全展示的类型数据。
pub struct ConnectionsPanel {
    pub(super) hosts: HostManagementService,
    pub(super) remote: Arc<dyn RemoteConnectionService>,
    pub(super) rdp: Option<Arc<RdpLaunchService>>,
    pub(super) rdp_status_host: Option<HostId>,
    pub(super) rdp_status_lifecycle: Option<RdpClientLifecycle>,
    pub(super) rdp_status_message: Option<RdpLaunchStatusMessage>,
    pub(super) rdp_status_error: Option<String>,
    pub(super) executor: ApplicationTaskExecutor,
    pub(super) recovery: ConfigurationRecoveryState,
    pub(super) serial_port_service: Option<Arc<SerialPortService>>,
    pub(super) serial_ports: Vec<SerialPortDescriptor>,
    pub(super) serial_ports_loaded: bool,
    pub(super) serial_ports_loading: bool,
    pub(super) serial_ports_issue: Option<SerialPortScanIssue>,
    pub(super) rows: Vec<ManagedHost>,
    pub(super) groups: Vec<HostGroup>,
    pub(super) group_revision: Option<kerminal_domain::preferences::ConfigRevision>,
    pub(super) selected_host: Option<HostId>,
    pub(super) presentation: ConnectionsPresentation,
    pub(super) group_name_input: Entity<InputState>,
    pub(super) editing_group_id: Option<String>,
    pub(super) group_save_reconciliation: Option<GroupSaveReconciliation>,
    pub(super) pending_host_deletion: Option<PendingHostDeletion>,
    pub(super) pending_group_deletion: Option<PendingGroupDeletion>,
    pub(super) configuration_mutation_busy: bool,
    status_after_reload: Option<String>,
    pub(super) editor: Option<HostEditor>,
    pub(super) local_hosts: super::local_hosts::LocalHostState,
    pub(super) active_editor_kind: Option<ConnectionsEditorKind>,
    group_editor_return: Option<GroupEditorReturn>,
    pub(super) pending_editor_discard: bool,
    pub(super) discard_focus: FocusHandle,
    pub(super) editor_fields_focus: FocusHandle,
    editor_discard_return_focus: Option<FocusHandle>,
    /// 仅 Files 选择器发起的 SFTP 新建流程可在明确保存回执后回投工作台。
    sftp_host_creation_flow_id: Option<String>,
    pub(super) save_outcome_unknown: bool,
    pending_host_save_recovery: Option<PendingHostSaveRecovery>,
    host_save_readback: Option<HostSaveReadback>,
    host_save_readback_generation: u64,
    host_save_readback_in_progress: bool,
    pending_new_host_id: Option<HostId>,
    pub(super) probe_service: Option<Arc<ConnectionProbeService>>,
    pub(super) probe_running: bool,
    pub(super) probe_outcome: Option<ConnectionProbeOutcome>,
    pub(super) probe_error: Option<String>,
    probe_challenge_draft: Option<kerminal_application::connections::management::HostDraft>,
    probe_request: ConnectionRequestEpoch,
    pub(super) pending_trust: Option<PendingTrust>,
    pub(super) connection_request: ConnectionRequestEpoch,
    sidebar_container_intent: Option<String>,
    sidebar_container_failure: Option<(String, String)>,
    pub(super) external_launch: Option<ExternalLaunchService>,
    pub(super) external_username_input: Entity<InputState>,
    pub(super) pending_external_username: Option<ExternalLaunchRequest>,
    pub(super) external_username_submitting: bool,
    pub(super) external_username_generation: u64,
    pub(super) external_password_input: Entity<InputState>,
    pub(super) pending_external_password: Option<ExternalRoutedTarget>,
    pub(super) external_authenticating: bool,
    pub(super) external_auth_generation: u64,
    pub(super) loading: bool,
    pub(super) saving: bool,
    pub(super) status: Option<String>,
    pub(super) configuration_changes: Option<ConfigurationChangeService>,
    rdp_status_service: Option<Arc<dyn RdpLaunchStatusService>>,
    configuration_watch_cancellation: CancellationToken,
    configuration_watch_started: bool,
    rdp_status_cancellation: CancellationToken,
    serial_port_scan_cancellation: ConnectionRequestCancellation,
    serial_port_scan_generation: u64,
    rdp_status_launch_id: Option<RdpLaunchId>,
    rdp_request_generation: u64,
    reload_pending: bool,
    host_snapshot_generation: HostSnapshotReadGeneration,
    external_conflict: bool,
    pending_conflict_reload_after_discard: Option<ConflictReloadTarget>,
    pub(super) conflict_reload_in_progress: Option<ConflictReloadTarget>,
    conflict_reload_generation: u64,
    editor_group_revision: Option<kerminal_domain::preferences::ConfigRevision>,
    host_file_picker_generation: u64,
    pending_host_file_picker: Option<(u64, HostFilePickerTarget)>,
}

impl EventEmitter<ConnectionsEvent> for ConnectionsPanel {}

impl ConnectionsPanel {
    /// 注入主机用例和应用 runtime；中央只承载所选详情与编辑器，不重复绘制目录。
    ///
    /// @author kongweiguang
    pub fn new(
        hosts: HostManagementService,
        remote: Arc<dyn RemoteConnectionService>,
        rdp: Option<Arc<RdpLaunchService>>,
        executor: ApplicationTaskExecutor,
        recovery: ConfigurationRecoveryState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let group_name_placeholder = crate::locale::text("新分组名称", "New group name");
        let group_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(group_name_placeholder));
        let discard_focus = cx.focus_handle().tab_stop(false);
        let editor_fields_focus = cx.focus_handle().tab_stop(false);
        let external_username_input = create_input(
            window,
            cx,
            crate::locale::text("外部 SSH/SFTP 用户名", "External SSH/SFTP username"),
            "",
            false,
        );
        let external_password_input = create_input(
            window,
            cx,
            crate::locale::text("外部连接密码", "External connection password"),
            "",
            true,
        );
        let mut panel = Self {
            hosts,
            remote,
            rdp,
            rdp_status_host: None,
            rdp_status_lifecycle: None,
            rdp_status_message: None,
            rdp_status_error: None,
            executor,
            recovery,
            serial_port_service: None,
            serial_ports: Vec::new(),
            serial_ports_loaded: false,
            serial_ports_loading: false,
            serial_ports_issue: None,
            rows: Vec::new(),
            groups: Vec::new(),
            group_revision: None,
            selected_host: None,
            presentation: ConnectionsPresentation::default(),
            group_name_input,
            editing_group_id: None,
            group_save_reconciliation: None,
            pending_host_deletion: None,
            pending_group_deletion: None,
            configuration_mutation_busy: false,
            status_after_reload: None,
            editor: None,
            local_hosts: super::local_hosts::LocalHostState::default(),
            active_editor_kind: None,
            group_editor_return: None,
            pending_editor_discard: false,
            discard_focus,
            editor_fields_focus,
            editor_discard_return_focus: None,
            sftp_host_creation_flow_id: None,
            save_outcome_unknown: false,
            pending_host_save_recovery: None,
            host_save_readback: None,
            host_save_readback_generation: 0,
            host_save_readback_in_progress: false,
            pending_new_host_id: None,
            probe_service: None,
            probe_running: false,
            probe_outcome: None,
            probe_error: None,
            probe_challenge_draft: None,
            probe_request: ConnectionRequestEpoch::new(),
            pending_trust: None,
            connection_request: ConnectionRequestEpoch::new(),
            sidebar_container_intent: None,
            sidebar_container_failure: None,
            external_launch: None,
            external_username_input,
            pending_external_username: None,
            external_username_submitting: false,
            external_username_generation: 0,
            external_password_input,
            pending_external_password: None,
            external_authenticating: false,
            external_auth_generation: 0,
            loading: false,
            saving: false,
            status: None,
            configuration_changes: None,
            rdp_status_service: None,
            configuration_watch_cancellation: CancellationToken::new(),
            configuration_watch_started: false,
            rdp_status_cancellation: CancellationToken::new(),
            serial_port_scan_cancellation: ConnectionRequestCancellation::new(),
            serial_port_scan_generation: 0,
            rdp_status_launch_id: None,
            rdp_request_generation: 0,
            reload_pending: false,
            host_snapshot_generation: HostSnapshotReadGeneration::default(),
            external_conflict: false,
            pending_conflict_reload_after_discard: None,
            conflict_reload_in_progress: None,
            conflict_reload_generation: 0,
            editor_group_revision: None,
            host_file_picker_generation: 0,
            pending_host_file_picker: None,
        };
        panel.reload(cx);
        panel
    }

    /// 刷新面板仍持有的编辑输入占位，不重建实体以免语言切换丢失焦点或草稿。
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        set_input_placeholder(
            &self.group_name_input,
            crate::locale::text("新分组名称", "New group name"),
            window,
            cx,
        );
        set_input_placeholder(
            &self.external_username_input,
            crate::locale::text("外部 SSH/SFTP 用户名", "External SSH/SFTP username"),
            window,
            cx,
        );
        set_input_placeholder(
            &self.external_password_input,
            crate::locale::text("外部连接密码", "External connection password"),
            window,
            cx,
        );
        if let Some(editor) = &self.editor {
            editor.refresh_locale(window, cx);
        }
        cx.notify();
    }

    /// 保存 desktop 注入的共享 watcher；订阅延迟至首次绘制，避免测试和非托管面板重复监听。
    pub fn with_configuration_changes(mut self, service: ConfigurationChangeService) -> Self {
        self.configuration_changes = Some(service);
        self
    }

    /// 持久本地主机与临时终端使用不同存储语义；这里只注入既有 Profile 用例并异步加载列表。
    /// @author kongweiguang
    pub fn with_profiles(mut self, profiles: ProfileService, cx: &mut Context<Self>) -> Self {
        self.local_hosts.set_service(profiles);
        self.reload_local_hosts(cx);
        self
    }

    /// 注入只读串口目录用例；面板本身不依赖平台驱动或枚举 crate。
    /// @author kongweiguang
    pub fn with_serial_port_service(mut self, service: Arc<SerialPortService>) -> Self {
        self.serial_port_service = Some(service);
        self
    }

    /// 注入未保存连接探测用例；缺少服务时不绘制无效的测试按钮。
    /// @author kongweiguang
    pub fn with_connection_probe_service(mut self, service: Arc<ConnectionProbeService>) -> Self {
        self.probe_service = Some(service);
        self
    }

    /// 供 Root 选择是否把同一面板实体投影为编辑弹层内容。
    /// @author kongweiguang
    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
            || self.local_hosts.editor.is_some()
            || self.local_hosts.editor_load_intent.is_some()
    }

    /// 外层 Root 焦点陷阱在嵌套确认或分组弹层打开期间应暂停，焦点由最上层表单拥有。
    /// @author kongweiguang
    pub fn nested_editor_open(&self) -> bool {
        self.pending_editor_discard || self.presentation.shows_group_editor()
    }

    /// 返回当前编辑器是否绑定已保存身份；复制和新建仍可切换协议，避免 UI 与提交策略分叉。
    /// @author kongweiguang
    pub(super) fn editor_protocol_locked(&self) -> bool {
        match self.active_editor_kind {
            Some(ConnectionsEditorKind::Remote) => self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.original.is_some()),
            Some(ConnectionsEditorKind::Local) => {
                self.local_hosts.editor.as_ref().is_some_and(|editor| {
                    editor.mode == super::local_hosts::LocalHostEditorMode::Edit
                })
            }
            None => self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.original.is_some()),
        }
    }

    /// 发起带 generation 的原生文件选择请求；Root 只处理当前活动编辑器声明的目标。
    /// @author kongweiguang
    pub(super) fn request_host_file_picker(
        &mut self,
        target: HostFilePickerTarget,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        if !self.host_file_target_is_available(target) {
            return None;
        }
        self.host_file_picker_generation = self.host_file_picker_generation.wrapping_add(1);
        let generation = self.host_file_picker_generation;
        self.pending_host_file_picker = Some((generation, target));
        cx.emit(ConnectionsEvent::PickHostFile { generation, target });
        Some(generation)
    }

    /// 仅接受仍绑定当前表单、协议和目标字段的原生选择回执；取消不会清空既有输入。
    /// @author kongweiguang
    pub(crate) fn set_host_file_path(
        &mut self,
        generation: u64,
        target: HostFilePickerTarget,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_pending_host_file_picker(generation, target) {
            return false;
        }
        self.pending_host_file_picker = None;
        let Some(path) = path else {
            cx.notify();
            return true;
        };
        let Some(path) = path.to_str() else {
            self.status = Some(
                crate::locale::text(
                    "所选路径无法编码为 UTF-8，请手动填写。",
                    "The selected path is not valid UTF-8. Enter it manually.",
                )
                .to_string(),
            );
            cx.notify();
            return true;
        };
        match target {
            HostFilePickerTarget::PrivateKey => {
                let Some(editor) = self.editor.as_mut() else {
                    return false;
                };
                editor
                    .inputs
                    .key_path
                    .update(cx, |input, cx| input.set_value(path.to_owned(), window, cx));
                editor.private_key_file_source = super::form::private_key_file_source_selected(
                    path,
                    editor.has_target_private_key(),
                );
                clear_textarea(&editor.inputs.private_key, window, cx);
                self.reset_connection_probe();
            }
            HostFilePickerTarget::SftpLocalDirectory => {
                let Some(editor) = self.editor.as_mut() else {
                    return false;
                };
                editor
                    .inputs
                    .sftp_local_directory
                    .update(cx, |input, cx| input.set_value(path.to_owned(), window, cx));
                self.reset_connection_probe();
            }
            HostFilePickerTarget::LocalShell | HostFilePickerTarget::LocalWorkingDirectory => {
                if !self.set_local_host_picker_path(target, path, window, cx) {
                    return false;
                }
            }
        }
        self.status = None;
        cx.notify();
        true
    }

    /// Root 在对话框失败时先询问当前 generation，过期错误不能覆盖新表单状态。
    /// @author kongweiguang
    pub(crate) fn set_host_file_picker_error(
        &mut self,
        generation: u64,
        target: HostFilePickerTarget,
        message: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_pending_host_file_picker(generation, target) {
            return false;
        }
        self.pending_host_file_picker = None;
        self.status = Some(message);
        cx.notify();
        true
    }

    /// 供 Root 的取消/错误回执检查使用；generation、目标和活动表单必须全部一致。
    /// @author kongweiguang
    pub(super) fn is_pending_host_file_picker(
        &self,
        generation: u64,
        target: HostFilePickerTarget,
    ) -> bool {
        self.pending_host_file_picker == Some((generation, target))
            && generation == self.host_file_picker_generation
            && self.host_file_target_is_available(target)
    }

    /// 校验 picker 意图与可见编辑器协议相符，异步完成不能写入已切换到的另一表单。
    /// @author kongweiguang
    fn host_file_target_is_available(&self, target: HostFilePickerTarget) -> bool {
        match (self.active_editor_kind, target) {
            (Some(ConnectionsEditorKind::Remote), HostFilePickerTarget::PrivateKey) => {
                self.editor.as_ref().is_some_and(|editor| {
                    matches!(editor.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
                        && editor.authentication_mode == HostAuthenticationMode::PrivateKey
                })
            }
            (Some(ConnectionsEditorKind::Remote), HostFilePickerTarget::SftpLocalDirectory) => {
                self.editor.as_ref().is_some_and(|editor| {
                    matches!(editor.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
                        && editor.sftp_enabled
                })
            }
            (Some(ConnectionsEditorKind::Local), HostFilePickerTarget::LocalShell)
            | (Some(ConnectionsEditorKind::Local), HostFilePickerTarget::LocalWorkingDirectory) => {
                self.local_hosts.editor.is_some()
            }
            _ => false,
        }
    }

    /// 关闭、替换或切换编辑器时使 native dialog 的迟到结果立即失效。
    /// @author kongweiguang
    pub(super) fn invalidate_host_file_picker(&mut self) {
        self.host_file_picker_generation = self.host_file_picker_generation.wrapping_add(1);
        self.pending_host_file_picker = None;
    }

    /// 仅聚焦当前分区实际可编辑的字段；无可输入项时留在分区容器，避免焦点落入隐藏表单。
    /// @author kongweiguang
    pub fn focus_editor_primary_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let local =
            self.active_editor_kind == Some(ConnectionsEditorKind::Local) || self.editor.is_none();
        let disabled = if local {
            self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
                || self.local_hosts.saving
                || self.local_hosts.save_outcome_unknown
                || self.conflicted_host_editor_reload_in_progress()
        } else {
            self.host_editor_fields_disabled()
        };
        if disabled {
            window.focus(&self.editor_fields_focus, cx);
            return;
        }
        let focus = if local {
            self.local_hosts
                .editor
                .as_ref()
                .map(|editor| match editor.section {
                    super::local_hosts::LocalHostEditorSection::Properties => {
                        editor.name.read(cx).focus_handle(cx)
                    }
                    super::local_hosts::LocalHostEditorSection::Terminal => {
                        editor.args.read(cx).focus_handle(cx)
                    }
                })
        } else if let Some(editor) = self.editor.as_ref() {
            let input = match editor.section {
                HostEditorSection::Properties | HostEditorSection::Authentication
                    if editor.protocol == RemoteProtocol::Serial =>
                {
                    Some(&editor.inputs.name)
                }
                HostEditorSection::Properties | HostEditorSection::Authentication => {
                    Some(&editor.inputs.host)
                }
                HostEditorSection::Proxy if editor.proxy_enabled => Some(&editor.inputs.proxy_host),
                HostEditorSection::Terminal if editor.protocol == RemoteProtocol::Serial => {
                    Some(&editor.inputs.serial_baud)
                }
                HostEditorSection::Terminal => Some(&editor.inputs.term),
                HostEditorSection::Network => Some(&editor.inputs.connect_timeout),
                HostEditorSection::Transfer if editor.sftp_enabled => {
                    Some(&editor.inputs.sftp_local_directory)
                }
                HostEditorSection::Display if !editor.rdp_fullscreen => {
                    Some(&editor.inputs.rdp_width)
                }
                HostEditorSection::JumpHosts => editor.jumps.first().map(|jump| &jump.host),
                _ => None,
            };
            input.map(|input| input.read(cx).focus_handle(cx))
        } else {
            None
        };
        window.focus(focus.as_ref().unwrap_or(&self.editor_fields_focus), cx);
    }

    /// 确认层夺取焦点前保留当前分区内的实际控件，连续关闭请求不能把确认层当成返回目标。
    /// @author kongweiguang
    pub(super) fn begin_editor_discard_confirmation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.pending_editor_discard {
            self.editor_discard_return_focus = window
                .focused(cx)
                .filter(|focus| self.editor_fields_focus.contains(focus, window));
        }
        self.pending_editor_discard = true;
        window.focus(&self.discard_focus, cx);
    }

    /// Root 的 Escape/遮罩关闭请求复用面板自己的脏草稿保护，不直接丢弃表单。
    /// @author kongweiguang
    pub fn request_editor_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.presentation.shows_group_editor() {
            self.cancel_group_rename(window, cx);
            return;
        }
        if self.pending_editor_discard {
            self.keep_editor_open(window, cx);
            return;
        }
        self.cancel_editor(window, cx);
    }

    /// 退出检查跨越当前可见表单，避免切换到另一协议后遗留的脏草稿或未知写入被窗口关闭丢弃。
    /// @author kongweiguang
    pub fn quit_block_reason(&self, cx: &gpui_kit::App) -> Option<String> {
        let unresolved_write = self.saving
            || self.save_outcome_unknown
            || self.local_hosts.saving
            || self.local_hosts.save_outcome_unknown
            || self.local_hosts.delete_checking
            || self.local_hosts.readback_in_progress
            || self.configuration_mutation_busy
            || self.pending_host_deletion.is_some()
            || self.pending_group_deletion.is_some()
            || self.local_hosts.pending_delete.is_some()
            || self.group_save_reconciliation.is_some();
        if unresolved_write {
            return Some(crate::locale::text(
                "连接配置仍有写入、删除或结果核对待处理；请先完成或取消该操作，再退出。",
                "A connection configuration write, deletion, or outcome review is still pending. Finish or cancel it before quitting.",
            ).to_string());
        }
        let remote_dirty = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_dirty(cx));
        let local_dirty = self.local_host_is_dirty(cx);
        let group_dirty = if self.presentation.shows_group_editor() {
            let name = self.group_name_input.read(cx).value().to_string();
            self.editing_group_id
                .as_deref()
                .and_then(|id| self.groups.iter().find(|group| group.id == id))
                .map_or_else(|| !name.trim().is_empty(), |group| group.name != name)
        } else {
            false
        };
        (remote_dirty || local_dirty || group_dirty || self.pending_editor_discard).then(|| {
            crate::locale::text(
                "连接编辑中仍有未保存内容；请先保存或明确放弃后再退出。",
                "Connection edits contain unsaved changes. Save or explicitly discard them before quitting.",
            ).to_string()
        })
    }

    /// 复用平台 launcher 的只读状态服务，面板只观察状态而不控制客户端或临时文件所有权。
    pub fn with_rdp_launch_status_service(
        mut self,
        service: Arc<dyn RdpLaunchStatusService>,
    ) -> Self {
        self.rdp_status_service = Some(service);
        self
    }

    /// 注入共享外部启动用例；密码提示只写入本次进程内目标，不进入 vault 或 host 文件。
    pub fn with_external_launch_service(mut self, service: ExternalLaunchService) -> Self {
        self.external_launch = Some(service);
        self
    }

    /// 首次绘制时订阅共享配置事件，借用面板取消令牌管理监听任务生命周期。
    pub(super) fn start_configuration_watch(&mut self, cx: &mut Context<Self>) {
        if self.configuration_watch_started {
            return;
        }
        let Some(service) = self.configuration_changes.clone() else {
            return;
        };
        self.configuration_watch_started = true;
        let mut subscription = service.subscribe();
        let cancellation = self.configuration_watch_cancellation.child_token();
        cx.spawn(async move |this, cx| {
            loop {
                match subscription.recv(cancellation.clone()).await {
                    ConfigurationChangeMessage::Changed(event) => {
                        let _ = this
                            .update(cx, |panel, cx| panel.apply_configuration_change(event, cx));
                    }
                    ConfigurationChangeMessage::RescanRequired => {
                        let recovery = service.recovery_state();
                        let _ = this.update(cx, |panel, cx| {
                            panel.apply_configuration_rescan(recovery, cx)
                        });
                    }
                    ConfigurationChangeMessage::Cancelled | ConfigurationChangeMessage::Closed => {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    /// 更新外部恢复门禁；主机文件变更时保留脏草稿并重读干净快照。
    fn apply_configuration_change(
        &mut self,
        event: ConfigurationChangeEvent,
        cx: &mut Context<Self>,
    ) {
        self.recovery = event.recovery_state;
        let hosts_changed =
            host_configuration_changed(&event.relative_paths, event.full_rescan_required);
        let profiles_changed =
            profile_configuration_changed(&event.relative_paths, event.full_rescan_required);
        if hosts_changed {
            self.refresh_for_external_change(cx);
        }
        if profiles_changed {
            self.reload_local_hosts(cx);
        }
        if !hosts_changed && !profiles_changed {
            cx.notify();
        }
    }

    /// 通道溢出时按共享仓储恢复态刷新完整主机快照，不猜测丢失的路径集合。
    fn apply_configuration_rescan(
        &mut self,
        recovery: ConfigurationRecoveryState,
        cx: &mut Context<Self>,
    ) {
        self.recovery = recovery;
        self.refresh_for_external_change(cx);
        self.reload_local_hosts(cx);
    }

    /// 外部改动保留脏输入和旧 revision，让下一次保存经 CAS 返回明确冲突。
    fn refresh_for_external_change(&mut self, cx: &mut Context<Self>) {
        if self.saving
            || self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.is_dirty(cx))
        {
            self.external_conflict = true;
            self.status = Some(crate::locale::text(
                "配置已被外部修改；当前草稿已保留，保存时将检查 revision 冲突。",
                "Configuration changed externally. Your draft is preserved; saving will check for a revision conflict.",
            ).to_string());
        } else {
            self.external_conflict = false;
        }
        self.reload(cx);
    }

    /// 向运维栏查询当前选择，返回副本以免外部保存可变借用或推断工作区当前 Tab。
    pub fn selected_host_id(&self) -> Option<HostId> {
        self.selected_host.clone()
    }

    /// 仅在相同 HostId 的详情中显示最近一次 RDP 结果，避免旧客户端事件覆盖其他操作状态。
    pub(super) fn rdp_status_for_host(&self, host_id: &HostId) -> Option<(String, bool)> {
        if self.rdp_status_host.as_ref() != Some(host_id) {
            return None;
        }
        if let Some(error) = &self.rdp_status_error {
            return Some((error.clone(), true));
        }
        let message = self.rdp_status_message?;
        let lifecycle = self.rdp_status_lifecycle?;
        Some(rdp_status_text(message.status, lifecycle))
    }

    /// 订阅当前启动身份；取消 watcher 只停止 UI 观察，不会关闭外部客户端或清理其文件。
    fn watch_rdp_status(
        &mut self,
        host_id: HostId,
        launch_id: RdpLaunchId,
        lifecycle: RdpClientLifecycle,
        cx: &mut Context<Self>,
    ) {
        self.rdp_status_cancellation.cancel();
        self.rdp_status_cancellation = CancellationToken::new();
        self.rdp_status_host = Some(host_id);
        self.rdp_status_launch_id = Some(launch_id);
        self.rdp_status_lifecycle = Some(lifecycle);
        self.rdp_status_message = None;
        self.rdp_status_error = None;

        let Some(service) = self.rdp_status_service.clone() else {
            cx.notify();
            return;
        };
        let subscription = match service.subscribe(launch_id) {
            Ok(subscription) => subscription,
            Err(error) => {
                self.rdp_status_error = Some(localized_error(&error));
                cx.notify();
                return;
            }
        };
        let cancellation = self.rdp_status_cancellation.child_token();
        cx.spawn(async move |this, cx| {
            loop {
                let result = subscription.recv(cancellation.clone()).await;
                match result {
                    Ok(message) => {
                        let terminal = message.status.is_terminal();
                        let _ = this.update(cx, |panel, cx| {
                            if panel.rdp_status_launch_id == Some(launch_id) {
                                panel.rdp_status_message = Some(message);
                                panel.rdp_status_error = None;
                                cx.notify();
                            }
                        });
                        if terminal {
                            break;
                        }
                    }
                    Err(error) => {
                        if cancellation.is_cancelled() {
                            break;
                        }
                        let message = localized_error(&error);
                        let _ = this.update(cx, |panel, cx| {
                            if panel.rdp_status_launch_id == Some(launch_id) {
                                panel.rdp_status_error = Some(message);
                                cx.notify();
                            }
                        });
                        break;
                    }
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// 外部快照仅重建干净表单并保留元信息区手动开关；脏表单保留旧 revision 交由 CAS 决定冲突。
    /// @author kongweiguang
    pub(super) fn refresh_clean_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.save_outcome_unknown {
            return;
        }
        let Some((host_id, revision)) = self
            .editor
            .as_ref()
            .and_then(|editor| editor.original.as_ref())
            .map(|host| (host.id.clone(), host.revision.clone()))
        else {
            return;
        };
        let latest = self.rows.iter().find(|host| host.id == host_id).cloned();
        let snapshot_changed = self.editor_group_revision != self.group_revision
            || latest.as_ref().is_none_or(|host| host.revision != revision);
        if !snapshot_changed
            || self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.is_dirty(cx))
        {
            return;
        }
        if let Some(host) = latest {
            let needs_serial_scan = host.protocol == RemoteProtocol::Serial;
            self.cancel_serial_port_scan();
            self.editor = Some(HostEditor::new(Some(host), &self.groups, window, cx));
            self.editor_group_revision = self.group_revision.clone();
            self.external_conflict = false;
            self.status = None;
            if needs_serial_scan {
                self.refresh_serial_ports(cx);
            }
        } else {
            self.cancel_serial_port_scan();
            if let Some(editor) = self.editor.take() {
                editor.clear_secrets(window, cx);
            }
            self.editor_group_revision = None;
            self.external_conflict = false;
            self.status = Some(
                crate::locale::text(
                    "主机已从外部配置中删除，已关闭干净的编辑表单。",
                    "This host was removed from configuration; the clean editor has been closed.",
                )
                .to_string(),
            );
        }
        cx.notify();
    }

    /// 仅把当前带外部冲突标记且仍有稳定身份的编辑表单作为重读目标。
    /// @author kongweiguang
    fn conflicted_editor_reload_target(&self) -> Option<ConflictReloadTarget> {
        match self.active_editor_kind {
            Some(ConnectionsEditorKind::Remote) if self.external_conflict => self
                .editor
                .as_ref()?
                .original
                .as_ref()
                .map(|host| ConflictReloadTarget::Remote(host.id.clone())),
            Some(ConnectionsEditorKind::Local) => {
                let editor = self.local_hosts.editor.as_ref()?;
                (editor.external_conflict
                    && !editor.profile_id.is_empty()
                    && self.local_hosts.service.is_some())
                .then(|| ConflictReloadTarget::Local(editor.profile_id.clone()))
            }
            _ => None,
        }
    }

    /// 重读入口在任何保存、探测、恢复、删除或配置事务中关闭，避免竞争写入和目标切换。
    /// @author kongweiguang
    pub(super) fn can_reload_conflicted_host_editor(&self) -> bool {
        let target = self.conflicted_editor_reload_target();
        let busy = self.saving
            || self.probe_running
            || self.save_outcome_unknown
            || self.pending_host_save_recovery.is_some()
            || self.local_hosts.saving
            || self.local_hosts.save_outcome_unknown
            || self.local_hosts.delete_checking
            || self.local_hosts.readback_in_progress
            || self.local_hosts.editor_load_pending
            || self.local_hosts.pending_delete.is_some()
            || self.configuration_mutation_busy
            || self.pending_host_deletion.is_some()
            || self.pending_group_deletion.is_some()
            || self.group_save_reconciliation.is_some()
            || self
                .pending_trust
                .as_ref()
                .is_some_and(|pending| pending.operation_busy)
            || self.presentation.shows_group_editor()
            || self.pending_host_file_picker.is_some()
            || self.pending_editor_discard
            || self.pending_conflict_reload_after_discard.is_some()
            || self.conflict_reload_in_progress.is_some();
        conflict_editor_reload_enabled(target.is_some(), target.is_some(), busy)
    }

    /// 脏表单先进入既有 discard 确认；确认后按稳定 ID 重读，失败则保留原 Entity 和秘密。
    /// @author kongweiguang
    pub(super) fn reload_conflicted_host_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_reload_conflicted_host_editor() {
            return;
        }
        let Some(target) = self.conflicted_editor_reload_target() else {
            return;
        };
        let dirty = match target {
            ConflictReloadTarget::Remote(_) => self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.is_dirty(cx)),
            ConflictReloadTarget::Local(_) => self.local_host_is_dirty(cx),
        };
        if dirty {
            if let ConflictReloadTarget::Local(profile_id) = &target {
                self.local_hosts.pending_action = Some(
                    super::local_hosts::LocalHostPendingAction::Edit(profile_id.clone()),
                );
            }
            self.pending_conflict_reload_after_discard = Some(target);
            self.begin_editor_discard_confirmation(window, cx);
            self.status = Some(crate::locale::text(
                "确认后将按原 ID 重新读取当前配置并放弃表单修改；取消可保留草稿。",
                "Confirming reloads the current configuration by its original ID and discards these edits. Cancel keeps the draft.",
            ).to_string());
            cx.notify();
            return;
        }
        match target {
            ConflictReloadTarget::Remote(host_id) => self.start_remote_conflict_reload(host_id, cx),
            ConflictReloadTarget::Local(profile_id) => {
                self.start_local_conflict_reload(profile_id, window, cx)
            }
        }
    }

    /// 远程读回期间只保留请求身份；表单替换必须等相同 HostId 的有效回执。
    /// @author kongweiguang
    pub(super) fn conflicted_host_editor_reload_in_progress(&self) -> bool {
        self.conflict_reload_in_progress.is_some()
    }

    /// 清除确认/读取意图并推进代次；关闭编辑器后旧回执不能重开或替换新目标。
    /// @author kongweiguang
    fn invalidate_conflicted_host_reload(&mut self) {
        self.conflict_reload_generation = self.conflict_reload_generation.wrapping_add(1);
        self.pending_conflict_reload_after_discard = None;
        self.conflict_reload_in_progress = None;
    }

    /// 仅在用户确认丢弃或当前表单干净后读取远程 HostId，错误时不改动旧编辑器。
    /// @author kongweiguang
    fn start_remote_conflict_reload(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        if !self.can_reload_conflicted_host_editor() {
            return;
        }
        let current_id = self
            .editor
            .as_ref()
            .and_then(|editor| editor.original.as_ref())
            .map(|host| host.id.clone());
        if self.active_editor_kind != Some(ConnectionsEditorKind::Remote)
            || current_id.as_ref() != Some(&host_id)
            || !self.external_conflict
        {
            self.status = Some(crate::locale::text(
                "冲突主机已变化；保留当前表单并重新选择目标。",
                "The conflicted host changed. The current form was preserved; select the intended target again.",
            ).to_string());
            cx.notify();
            return;
        }
        self.conflict_reload_generation = self.conflict_reload_generation.wrapping_add(1);
        let generation = self.conflict_reload_generation;
        let target = ConflictReloadTarget::Remote(host_id.clone());
        self.conflict_reload_in_progress = Some(target.clone());
        self.reset_connection_probe();
        self.status = Some(crate::locale::text(
            "正在按原主机 ID 重新读取配置；读取失败时当前草稿会保留。",
            "Reloading configuration by the original host ID. The current draft will remain if the read fails.",
        ).to_string());
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        let request_host_id = host_id.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { hosts.load_host(&request_host_id).await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_remote_conflict_reload(generation, target, host_id, result, window, cx)
            });
        })
        .detach();
        cx.notify();
    }

    /// 只有同 HostId/generation 的成功读取替换远程表单；NotFound 与错误保留草稿供处理。
    /// @author kongweiguang
    fn finish_remote_conflict_reload(
        &mut self,
        generation: u64,
        target: ConflictReloadTarget,
        host_id: HostId,
        result: Result<Option<ManagedHost>, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current_editor_id = self
            .editor
            .as_ref()
            .and_then(|editor| editor.original.as_ref())
            .map(|host| &host.id);
        if !conflict_reload_response_matches(
            generation,
            self.conflict_reload_generation,
            self.conflict_reload_in_progress.as_ref(),
            &target,
        ) || self.active_editor_kind != Some(ConnectionsEditorKind::Remote)
            || current_editor_id != Some(&host_id)
        {
            return;
        }
        self.conflict_reload_in_progress = None;
        match result {
            Ok(Some(host)) if host.id == host_id => {
                let needs_serial_scan = host.protocol == RemoteProtocol::Serial;
                self.host_snapshot_generation
                    .invalidate_after_authoritative_readback();
                self.cancel_serial_port_scan();
                self.invalidate_host_file_picker();
                self.reset_connection_probe();
                if let Some(editor) = self.editor.take() {
                    editor.clear_secrets(window, cx);
                }
                upsert_saved_host_row(&mut self.rows, host.clone());
                self.selected_host = Some(host_id);
                self.editor = Some(HostEditor::new(Some(host), &self.groups, window, cx));
                self.editor_group_revision = self.group_revision.clone();
                self.pending_new_host_id = None;
                self.external_conflict = false;
                self.status = Some(crate::locale::text(
                    "已重新读取当前主机配置；表单现在基于最新 revision。",
                    "The current host configuration was reloaded. The form now uses its latest revision.",
                ).to_string());
                if needs_serial_scan {
                    self.refresh_serial_ports(cx);
                }
            }
            Ok(Some(_)) => {
                self.status = Some(crate::locale::text(
                    "读取返回的主机 ID 与请求不一致；已保留原表单，请重试。",
                    "The read returned a different host ID. The original form was preserved; retry the reload.",
                ).to_string());
            }
            Ok(None) => {
                self.status = Some(crate::locale::text(
                    "该主机已不存在；原表单仍保留，可重试读取或取消编辑。",
                    "This host no longer exists. The original form remains available; retry the read or cancel editing.",
                ).to_string());
            }
            Err(error) => {
                self.status = Some(
                    crate::locale::text(
                        format!(
                            "重新读取失败，原表单已保留，可重试：{}",
                            localized_error(&error)
                        ),
                        format!(
                            "Reload failed and the original form was preserved. You can retry: {}",
                            localized_error(&error)
                        ),
                    )
                    .to_string(),
                );
            }
        }
        cx.notify();
    }

    /// Local 冲突读取复用 ProfileService 的 stable-ID 编辑加载路径，不提前丢弃原 Entity。
    /// @author kongweiguang
    fn start_local_conflict_reload(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_reload_conflicted_host_editor() {
            return;
        }
        let still_target =
            self.active_editor_kind == Some(ConnectionsEditorKind::Local)
                && self.local_hosts.editor.as_ref().is_some_and(|editor| {
                    editor.profile_id == profile_id && editor.external_conflict
                });
        if !still_target {
            self.local_hosts.pending_action = None;
            self.status = Some(crate::locale::text(
                "冲突本地主机已变化；保留当前表单并重新选择目标。",
                "The conflicted local host changed. The current form was preserved; select the intended target again.",
            ).to_string());
            cx.notify();
            return;
        }
        self.local_hosts.pending_action = None;
        self.cancel_local_host_background_requests();
        self.conflict_reload_in_progress = Some(ConflictReloadTarget::Local(profile_id.clone()));
        self.load_local_profile_editor(
            profile_id,
            super::local_hosts::LocalHostEditorMode::Edit,
            window,
            cx,
        );
    }

    /// 重读主机与分组完整快照；任一配置错误会显示恢复提示而不呈现部分成功列表。
    /// @author kongweiguang
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_with_group_save_review(false, cx);
    }

    /// 主机与分组共享一份读代次；已确认主机保存会使此前启动的读取失效并触发同意图重读。
    /// 仅明确发起的分组核对回执可推进未知分组保存状态，过期读取不承担确认语义。
    /// @author kongweiguang
    fn reload_with_group_save_review(
        &mut self,
        explicitly_requested_review: bool,
        cx: &mut Context<Self>,
    ) {
        if self.loading {
            if !explicitly_requested_review {
                self.reload_pending = true;
            }
            return;
        }
        self.loading = true;
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        let read_generation = self.host_snapshot_generation.capture();
        let review_group_save_on_success = explicitly_requested_review;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let rows = hosts.list_hosts().await?;
                    let groups = hosts.load_groups().await?;
                    Ok((rows, groups))
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    panel.loading = false;
                    match result {
                        Ok((rows, groups)) => {
                            if !apply_host_snapshot_read_if_current(
                                &panel.host_snapshot_generation,
                                read_generation,
                                &mut panel.rows,
                                &mut panel.groups,
                                &mut panel.group_revision,
                                rows,
                                groups,
                            ) {
                                panel.restart_stale_host_snapshot_read(
                                    review_group_save_on_success,
                                    cx,
                                );
                                return;
                            }
                            panel.group_save_reconciliation =
                                group_save_reconciliation_after_readback(
                                    panel.group_save_reconciliation,
                                    review_group_save_on_success,
                                    true,
                                );
                            if panel
                                .selected_host
                                .as_ref()
                                .is_some_and(|id| !panel.rows.iter().any(|row| &row.id == id))
                            {
                                panel.selected_host = None;
                            }
                            if let Some(status) = panel.status_after_reload.take() {
                                panel.status = Some(status);
                            } else if !panel.external_conflict && !panel.save_outcome_unknown {
                                panel.status = None;
                            }
                        }
                        Err(error) => {
                            if !panel.host_snapshot_generation.accepts(read_generation) {
                                panel.restart_stale_host_snapshot_read(
                                    review_group_save_on_success,
                                    cx,
                                );
                                return;
                            }
                            if review_group_save_on_success {
                                panel.status_after_reload = None;
                            }
                            panel.status = Some(localized_error(&error));
                        }
                    }
                    cx.notify();
                    if panel.reload_pending {
                        panel.reload_pending = false;
                        panel.reload(cx);
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 丢弃失效读取并立即重读；显式分组核对标记随重试保留，旧回执不参与核对推进。
    ///
    /// @author kongweiguang
    fn restart_stale_host_snapshot_read(
        &mut self,
        explicitly_requested_review: bool,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        // 这轮强制重读已覆盖在途期间排队的普通刷新请求，避免旧代次回执再启动第三轮。
        self.reload_pending = false;
        self.reload_with_group_save_review(explicitly_requested_review, cx);
    }

    /// 选择主机后向 Root 发布稳定 HostId，并保留现有终端会话与分屏状态。
    pub(super) fn select_host(&mut self, host_id: HostId, cx: &mut Context<Self>) {
        self.cancel_connection_request();
        self.status = None;
        self.selected_host = Some(host_id.clone());
        cx.emit(ConnectionsEvent::HostSelected { host_id });
        cx.notify();
    }

    /// 新预检独占新的请求世代，旧请求即使稍后返回也不能发布连接或信任结果。
    fn begin_connection_request(&mut self) -> (u64, ConnectionRequestCancellation) {
        self.pending_trust = None;
        self.connection_request.begin()
    }

    /// 选择、取消或替换目标时立刻取消 SSH 操作，并清掉属于旧请求的挑战。
    pub(super) fn cancel_connection_request(&mut self) {
        self.connection_request.cancel();
        self.pending_trust = None;
        self.sidebar_container_intent = None;
        self.sidebar_container_failure = None;
    }

    /// 显式查看详情时复用面板选择；普通目录单击不进入此入口，也不切换或新建会话。
    ///
    /// @author kongweiguang
    pub fn select_host_id(&mut self, host_id: &HostId, cx: &mut Context<Self>) {
        self.presentation.show_host_details();
        self.select_host(host_id.clone(), cx);
    }

    /// 由独立主机导航请求连接时复用当前协议映射、host-key 预检与二次信任流程。
    pub fn request_connect_host(&mut self, host_id: &HostId, cx: &mut Context<Self>) {
        self.select_host(host_id.clone(), cx);
        self.connect_selected(false, cx);
    }

    /// 保留侧栏容器请求的独立身份穿过多轮指纹预检，防止同主机的后续连接误消费回执。
    ///
    /// @author kongweiguang
    pub(crate) fn request_connect_host_for_container(
        &mut self,
        host_id: &HostId,
        intent_id: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if !host_rows_contain_protocol(
            self.rows.iter().map(|host| (&host.id, host.protocol)),
            host_id,
            RemoteProtocol::Ssh,
        ) {
            return false;
        }
        self.select_host(host_id.clone(), cx);
        self.connect_selected_with_destination(false, false, Some(intent_id), cx);
        true
    }

    /// 组合根只需知道原预检是否仍有效；取消或失败后立即释放续接意图，不给信任确认设置超时。
    /// @author kongweiguang
    pub(crate) fn sidebar_container_intent_is_current(&self, intent_id: &str) -> bool {
        self.sidebar_container_intent.as_deref() == Some(intent_id)
    }

    /// 错误只回投原侧栏请求；普通连接或后来请求不能借用旧失败，也不向 Root 暴露私有诊断。
    /// @author kongweiguang
    pub(crate) fn sidebar_container_failure(&self, intent_id: &str) -> Option<&str> {
        self.sidebar_container_failure
            .as_ref()
            .filter(|(id, _)| id == intent_id)
            .map(|(_, message)| message.as_str())
    }

    /// 使用菜单打开时捕获的协议校验当前面板快照，防止异步重载后同一 HostId 被解释为另一协议。
    /// 匹配后同步复用既有连接流程；目标缺失或协议变化时不修改选择，也不触发连接。
    /// @author kongweiguang
    pub fn request_connect_host_for_protocol(
        &mut self,
        host_id: &HostId,
        expected_protocol: RemoteProtocol,
        cx: &mut Context<Self>,
    ) -> bool {
        if !host_rows_contain_protocol(
            self.rows.iter().map(|host| (&host.id, host.protocol)),
            host_id,
            expected_protocol,
        ) {
            return false;
        }
        self.request_connect_host(host_id, cx);
        true
    }

    /// 由独立主机导航请求 SFTP 时仍经过现有 host-key 预检，且不从活动终端推断目标。
    pub fn request_open_sftp_host(&mut self, host_id: &HostId, cx: &mut Context<Self>) {
        self.select_host(host_id.clone(), cx);
        self.connect_selected(true, cx);
    }

    /// 创建 transfer tab 仍使用同一连接预检和信任事务，只有最终消费端不同于右栏文件面板。
    ///
    /// @author kongweiguang
    pub fn request_create_sftp_transfer_tab_host(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) {
        self.select_host(host_id.clone(), cx);
        self.connect_selected_for_transfer_tab(cx);
    }

    /// 从左栏事件切入详情，并让运维目标继续接收显式选择的 HostId。
    /// @author kongweiguang
    pub fn show_saved_host(&mut self, host_id: &HostId, cx: &mut Context<Self>) {
        self.pending_host_deletion = None;
        self.pending_group_deletion = None;
        self.presentation.show_host_details();
        self.select_host(host_id.clone(), cx);
    }

    /// 左栏编辑动作复用当前列表快照与同一 revision CAS 表单。
    /// @author kongweiguang
    pub fn edit_saved_host(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        self.show_saved_host(host_id, cx);
        self.edit_host(host_id, window, cx);
    }

    /// 复制只复用可公开编辑字段；表单立即断开源 aggregate 和所有 vault 引用。
    /// @author kongweiguang
    pub fn duplicate_saved_host(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_saved_host_with_protocol(host_id, None, window, cx);
    }

    /// SFTP 可显式派生 SSH 新草稿，复用地址/分组等字段但不继承源凭据或覆盖源主机。
    /// @author kongweiguang
    pub fn duplicate_saved_host_as_ssh(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_saved_host_with_protocol(host_id, Some(RemoteProtocol::Ssh), window, cx);
    }

    /// 两类复制共享草稿替换、只读与存在性门禁；协议转换在构造干净基线前完成。
    /// @author kongweiguang
    fn duplicate_saved_host_with_protocol(
        &mut self,
        host_id: &HostId,
        protocol: Option<RemoteProtocol>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，不能复制主机。",
                    "Configuration is read-only during recovery; hosts cannot be duplicated.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(mut host) = self.rows.iter().find(|row| &row.id == host_id).cloned() else {
            self.status = Some(
                crate::locale::text(
                    "主机已不存在，请刷新列表。",
                    "The host no longer exists. Refresh the list.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        };
        if let Some(protocol) = protocol {
            if host.protocol != RemoteProtocol::Sftp {
                self.status = Some(
                    crate::locale::text(
                        "源主机协议已变化，请刷新列表后重试。",
                        "The source protocol changed. Refresh the list and try again.",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
            host.protocol = protocol;
        }
        self.show_saved_host(host_id, cx);
        self.cancel_serial_port_scan();
        self.editor = Some(HostEditor::duplicate(&host, &self.groups, window, cx));
        self.pending_new_host_id = None;
        self.editor_group_revision = self.group_revision.clone();
        self.external_conflict = false;
        self.status = Some(crate::locale::text(
            "已创建未保存副本；凭据不会复制，请为新主机重新录入。",
            "A new unsaved copy is ready. Credentials were not copied; enter them for this host.",
        ).to_string());
        cx.notify();
    }

    /// 在指定真实分组中创建主机，预选值作为干净草稿初值而非持久化副作用。
    /// @author kongweiguang
    pub fn create_host_in_group(
        &mut self,
        group_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.groups.iter().any(|group| group.id == group_id) {
            self.status = Some(
                crate::locale::text(
                    "分组已不存在，请刷新后重试。",
                    "The group no longer exists. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        }
        self.create_host(window, cx);
        if let Some(editor) = self.editor.as_mut() {
            editor.preselect_group(Some(group_id.to_owned()), window, cx);
        }
        cx.notify();
    }

    /// 绑定主机删除确认到列表中的 revision；真正提交前仍由应用和配置仓储做 CAS。
    /// @author kongweiguang
    pub fn request_delete_host(&mut self, host_id: &HostId, cx: &mut Context<Self>) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，不能删除主机。",
                    "Configuration is read-only during recovery; hosts cannot be deleted.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(revision) = self
            .rows
            .iter()
            .find(|host| &host.id == host_id)
            .map(|host| host.revision.clone())
        else {
            self.status = Some(
                crate::locale::text(
                    "主机已不存在，请刷新后重试。",
                    "The host no longer exists. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        };
        self.show_saved_host(host_id, cx);
        self.pending_host_deletion = Some(PendingHostDeletion {
            host_id: host_id.clone(),
            revision,
        });
        self.status = None;
        cx.notify();
    }

    /// 撤销尚未提交的删除确认；无后台删除任务时不会触碰持久数据。
    /// @author kongweiguang
    pub fn cancel_host_deletion(&mut self, cx: &mut Context<Self>) {
        if !self.configuration_mutation_busy {
            self.pending_host_deletion = None;
            cx.notify();
        }
    }

    /// 用户确认后只提交一次原 revision 删除，后续密文清理由 application 安全检查。
    /// @author kongweiguang
    pub fn confirm_host_deletion(&mut self, cx: &mut Context<Self>) {
        if self.configuration_mutation_busy
            || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
        {
            return;
        }
        let Some(pending) = self.pending_host_deletion.clone() else {
            return;
        };
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        self.configuration_mutation_busy = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    hosts.delete_host(&pending.host_id, &pending.revision).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| panel.finish_host_deletion(result, cx));
            });
        })
        .detach();
        self.status = Some(
            crate::locale::text("正在删除主机配置…", "Deleting host configuration…").to_string(),
        );
        cx.notify();
    }

    /// 分组删除确认捕获当前文件 revision；迁移目标必须在随后选择，不依赖隐式默认。
    /// @author kongweiguang
    pub fn request_delete_group(&mut self, group_id: &str, cx: &mut Context<Self>) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，不能删除分组。",
                    "Configuration is read-only during recovery; groups cannot be deleted.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(revision) = self.group_revision.clone() else {
            self.status = Some(
                crate::locale::text(
                    "分组配置尚未就绪，请刷新后重试。",
                    "Group configuration is not ready. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        };
        if !self.groups.iter().any(|group| group.id == group_id) {
            self.status = Some(
                crate::locale::text(
                    "分组已不存在，请刷新后重试。",
                    "The group no longer exists. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        }
        self.pending_group_deletion = Some(PendingGroupDeletion {
            group_id: group_id.to_owned(),
            revision,
        });
        self.editing_group_id = None;
        self.status = None;
        cx.notify();
    }

    /// 取消分组删除弹层时保留原组和主机归属，不执行配置写入。
    /// @author kongweiguang
    pub fn cancel_group_deletion(&mut self, cx: &mut Context<Self>) {
        if !self.configuration_mutation_busy {
            self.pending_group_deletion = None;
            cx.notify();
        }
    }

    /// 按用户明选的目标分组执行 host/group 原子事务，None 明确表示迁移到未分组。
    /// @author kongweiguang
    pub fn confirm_group_deletion(
        &mut self,
        destination_group_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_mutation_busy
            || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
        {
            return;
        }
        let Some(pending) = self.pending_group_deletion.clone() else {
            return;
        };
        if destination_group_id.as_ref().is_some_and(|destination| {
            destination == &pending.group_id
                || !self.groups.iter().any(|group| &group.id == destination)
        }) {
            self.status = Some(
                crate::locale::text(
                    "迁移目标已失效，请重新选择。",
                    "The destination group changed. Choose again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        }
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        let destination_for_notice = destination_group_id.clone();
        let groups_for_notice = self.groups.clone();
        self.configuration_mutation_busy = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    hosts
                        .delete_group(pending.group_id, destination_group_id, pending.revision)
                        .await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    panel.finish_group_deletion(
                        result,
                        destination_for_notice,
                        groups_for_notice,
                        cx,
                    )
                });
            });
        })
        .detach();
        self.status = Some(
            crate::locale::text(
                "正在迁移主机并删除分组…",
                "Moving hosts and deleting group…",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 左栏的新建入口复用完整宽度编辑器；配置写入仍由已有 save_group/CAS 按钮执行。
    /// @author kongweiguang
    pub fn begin_create_group(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.begin_create_group_internal(false, window, cx)
    }

    /// 主机编辑器内创建分组仅暂借 presentation，原主机 Entity 和草稿始终留在 panel 中。
    /// @author kongweiguang
    pub(super) fn begin_create_group_for_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.group_editor_return.is_some() {
            self.status = Some(
                crate::locale::text(
                    "当前分组创建尚未结束，请先保存或取消。",
                    "The current group creation is still active. Save or cancel it first.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        let Some(editor_kind) = self.active_editor_kind.filter(|kind| match kind {
            ConnectionsEditorKind::Remote => self.editor.is_some(),
            ConnectionsEditorKind::Local => self.local_hosts.editor.is_some(),
        }) else {
            return false;
        };
        if self.begin_create_group_internal(true, window, cx) {
            self.group_editor_return = Some(GroupEditorReturn { editor_kind });
            true
        } else {
            false
        }
    }

    /// 统一普通入口与编辑器内入口的配置/删除/未知结果门禁，调用方只能选择是否保留主机表单。
    /// @author kongweiguang
    fn begin_create_group_internal(
        &mut self,
        keep_host_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.configuration_mutation_busy
            || (!keep_host_editor && (self.editor.is_some() || self.local_hosts.editor.is_some()))
        {
            self.status = Some(crate::locale::text(
                "请先完成当前配置操作或主机编辑，再创建分组。",
                "Finish the current configuration operation or host edit before creating a group.",
            ).to_string());
            cx.notify();
            return false;
        }
        if self.pending_group_deletion.is_some() || self.pending_host_deletion.is_some() {
            self.status = Some(
                crate::locale::text(
                    "请先完成或取消待确认的删除操作。",
                    "Finish or cancel the pending deletion before creating a group.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if self.group_save_reconciliation.is_some() {
            // 未知事务期间重入只恢复原草稿与核对界面，不替换名字或自动重放。
            self.presentation.show_group_editor();
            self.group_name_input
                .update(cx, |input, cx| input.focus(window, cx));
            self.status = Some(
                crate::locale::text(
                    "先核对上次分组保存结果；当前草稿已保留。",
                    "Review the previous group save outcome first. The current draft is preserved.",
                )
                .to_string(),
            );
        } else {
            if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
                self.status = Some(
                    crate::locale::text(
                        "配置处于只读恢复模式，不能创建分组。",
                        "Configuration is read-only during recovery; groups cannot be created.",
                    )
                    .to_string(),
                );
                cx.notify();
                return false;
            }
            if self.presentation.shows_group_editor() || self.editing_group_id.is_some() {
                self.status = Some(
                    crate::locale::text(
                        "已有分组编辑草稿，请先保存或取消。",
                        "A group editor is already open. Save or cancel it first.",
                    )
                    .to_string(),
                );
                cx.notify();
                return false;
            }
            self.presentation.show_group_editor();
            self.editing_group_id = None;
            self.group_name_input.update(cx, |input, cx| {
                input.set_value(String::new(), window, cx);
                input.focus(window, cx);
            });
            self.status = None;
        }
        cx.notify();
        true
    }

    /// 显式核对只读回当前分组快照；读回完成后仍需用户另行决定是否提交新请求。
    /// @author kongweiguang
    pub fn verify_group_save_outcome(&mut self, cx: &mut Context<Self>) {
        if self.configuration_mutation_busy
            || self.loading
            || !self.presentation.shows_group_editor()
            || self.group_save_reconciliation != Some(GroupSaveReconciliation::NeedsReadback)
        {
            return;
        }
        self.status_after_reload = Some(crate::locale::text(
            "分组配置已重新读取；以下是当前事实，不代表上次保存成功。",
            "Group configuration was re-read. The current facts below do not confirm that the prior save succeeded.",
        ).to_string());
        self.reload_with_group_save_review(true, cx);
    }

    /// 用户看过成功读回的配置后才解除重试封锁；这里只准许新点击，不自动重放旧事务。
    /// @author kongweiguang
    pub fn acknowledge_group_save_review(&mut self, cx: &mut Context<Self>) {
        if !self.presentation.shows_group_editor()
            || self.group_save_reconciliation != Some(GroupSaveReconciliation::AwaitingUserDecision)
        {
            return;
        }
        self.group_save_reconciliation = group_save_reconciliation_after_acknowledgement(
            self.group_save_reconciliation,
            self.presentation.shows_group_editor(),
        );
        self.status = Some(
            crate::locale::text(
                "核对已完成；如仍需保存，请再次点击保存以提交新请求。",
                "Review complete. Click Save again only if you still want to submit a new request.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 验证分组稳定 ID 后进入全宽重命名编辑；共享草稿只在取消或保存回执时结束。
    /// @author kongweiguang
    pub fn begin_rename_group(
        &mut self,
        group_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.configuration_mutation_busy {
            self.status = Some(
                crate::locale::text(
                    "正在处理其它配置修改，请稍后重试。",
                    "Another configuration change is in progress. Try again shortly.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if self.editor.is_some() {
            self.status = Some(
                crate::locale::text(
                    "请先完成或取消当前主机编辑，再重命名分组。",
                    "Finish or cancel the current host edit before renaming a group.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if self.presentation.shows_group_editor() && self.group_save_reconciliation.is_none() {
            self.status = Some(
                crate::locale::text(
                    "已有分组编辑草稿，请先保存或取消。",
                    "A group editor is already open. Save or cancel it first.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if self.group_save_reconciliation.is_some() {
            self.presentation.show_group_editor();
            self.group_name_input
                .update(cx, |input, cx| input.focus(window, cx));
            self.status = Some(crate::locale::text(
                "先核对上次分组保存结果；当前草稿不会被替换。",
                "Review the previous group save outcome first. The current draft was preserved.",
            ).to_string());
            cx.notify();
            return true;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，不能重命名分组。",
                    "Configuration is read-only during recovery; groups cannot be renamed.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if self.pending_group_deletion.is_some() || self.pending_host_deletion.is_some() {
            self.status = Some(
                crate::locale::text(
                    "请先完成或取消待确认的删除操作。",
                    "Finish or cancel the pending deletion before renaming a group.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        let Some(group) = self.groups.iter().find(|group| group.id == group_id) else {
            self.status = Some(
                crate::locale::text(
                    "分组已不存在，请刷新后重试。",
                    "The group no longer exists. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return false;
        };
        self.presentation.show_group_editor();
        self.group_name_input.update(cx, |input, cx| {
            input.set_value(group.name.clone(), window, cx);
            input.focus(window, cx);
        });
        self.editing_group_id = Some(group_id.to_owned());
        self.pending_group_deletion = None;
        self.status = None;
        cx.notify();
        true
    }

    /// 取消创建/重命名均不写配置；未知提交时保留草稿供用户离开后重新核对。
    /// @author kongweiguang
    pub fn cancel_group_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.configuration_mutation_busy || !self.presentation.shows_group_editor() {
            return;
        }
        if self.group_editor_return.is_some() && self.group_save_reconciliation.is_some() {
            self.status = Some(
                crate::locale::text(
                    "请先完成分组保存结果核对，再返回主机编辑。",
                    "Reconcile the group save outcome before returning to host editing.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let inline_return = self.group_editor_return.take();
        if self.group_save_reconciliation.is_none() {
            self.editing_group_id = None;
            self.group_name_input
                .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        }
        self.presentation.finish_group_editor();
        if inline_return.is_none() {
            cx.emit(ConnectionsEvent::GroupRenameCancelled);
        } else if let Some(inline_return) = inline_return {
            self.focus_host_group_control(inline_return.editor_kind, window, cx);
        }
        cx.notify();
    }

    /// 关闭内联分组层后把焦点交回原表单的分组选项，避免焦点落到已卸载的分组输入。
    /// @author kongweiguang
    fn focus_host_group_control(
        &mut self,
        editor_kind: ConnectionsEditorKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match editor_kind {
            ConnectionsEditorKind::Remote => {
                if let Some(editor) = self.editor.as_ref() {
                    editor
                        .group_select
                        .update(cx, |select, cx| select.focus(window, cx));
                }
            }
            ConnectionsEditorKind::Local => {
                if let Some(editor) = self.local_hosts.editor.as_ref() {
                    editor
                        .group_select
                        .update(cx, |select, cx| select.focus(window, cx));
                }
            }
        }
    }

    /// 删除回执区分已删除配置和凭据清理状态；未知提交刷新读取但不会自动重试。
    fn finish_host_deletion(
        &mut self,
        result: Result<HostDeletionReceipt, AppError>,
        cx: &mut Context<Self>,
    ) {
        self.configuration_mutation_busy = false;
        match result {
            Ok(receipt) => {
                self.pending_host_deletion = None;
                self.status_after_reload = Some(host_deletion_notice(receipt));
                self.external_conflict = false;
                self.reload(cx);
            }
            Err(error)
                if matches!(
                    error.public_code(),
                    PublicErrorCode::OutcomeUnknown | PublicErrorCode::Conflict
                ) =>
            {
                self.pending_host_deletion = None;
                self.status_after_reload = Some(
                    if error.public_code() == PublicErrorCode::OutcomeUnknown {
                        crate::locale::text("删除结果未知；已刷新列表，请核对主机是否仍存在后再手动决定，不会自动重试。", "The delete outcome is unknown. The list was refreshed; verify whether the host remains before deciding manually. No automatic retry was made.").to_string()
                    } else {
                        crate::locale::text("主机配置已变化；列表已刷新，请重新确认删除。", "Host configuration changed. The list was refreshed; confirm deletion again if needed.").to_string()
                    },
                );
                self.external_conflict = false;
                self.reload(cx);
            }
            Err(error) => self.status = Some(localized_error(&error)),
        }
        cx.notify();
    }

    /// 事务回执只更新分组快照和迁移提示；结果未知时由上层读取恢复，不补发事务。
    ///
    /// @author kongweiguang
    fn finish_group_deletion(
        &mut self,
        result: Result<HostGroupDeletionReceipt, AppError>,
        destination: Option<String>,
        groups_before: Vec<HostGroup>,
        cx: &mut Context<Self>,
    ) {
        self.configuration_mutation_busy = false;
        match result {
            Ok(receipt) => {
                self.pending_group_deletion = None;
                self.groups = receipt.groups.groups;
                self.group_revision = receipt.groups.revision;
                let destination_name = destination
                    .as_ref()
                    .and_then(|id| groups_before.iter().find(|group| &group.id == id))
                    .map(|group| group.name.as_str())
                    .unwrap_or("未分组");
                self.status_after_reload = Some(
                    crate::locale::text(
                        format!(
                            "分组已删除；{} 台主机已迁移到 {}。",
                            receipt.reassigned_hosts, destination_name
                        ),
                        format!(
                            "Group deleted; {} hosts moved to {}.",
                            receipt.reassigned_hosts,
                            if destination.is_some() {
                                destination_name
                            } else {
                                "ungrouped"
                            }
                        ),
                    )
                    .to_string(),
                );
                self.reload(cx);
            }
            Err(error)
                if matches!(
                    error.public_code(),
                    PublicErrorCode::OutcomeUnknown | PublicErrorCode::Conflict
                ) =>
            {
                self.pending_group_deletion = None;
                self.status_after_reload = Some(
                    if error.public_code() == PublicErrorCode::OutcomeUnknown {
                        crate::locale::text("分组事务结果未知；已刷新分组和主机列表，请核对归属后再手动处理，不会自动重试。", "The group transaction outcome is unknown. Groups and hosts were refreshed; verify membership before deciding manually. No automatic retry was made.").to_string()
                    } else {
                        crate::locale::text("分组或主机配置已变化；列表已刷新，请重新确认操作。", "Group or host configuration changed. The lists were refreshed; confirm the operation again if needed.").to_string()
                    },
                );
                self.external_conflict = false;
                self.reload(cx);
            }
            Err(error) => self.status = Some(localized_error(&error)),
        }
        cx.notify();
    }

    /// 主机默认目录只用于新工作区，禁用的 SFTP 在预检前拒绝；现存面板位置不在此重置。
    ///
    /// @author kongweiguang
    pub(super) fn connect_selected(&mut self, sftp: bool, cx: &mut Context<Self>) {
        self.connect_selected_with_destination(sftp, false, None, cx);
    }

    /// 保留现有协议和 host-key 检查，仅将 SSH 新传输意图记入待信任动作而非共享右栏面板。
    ///
    /// @author kongweiguang
    fn connect_selected_for_transfer_tab(&mut self, cx: &mut Context<Self>) {
        self.connect_selected_with_destination(true, true, None, cx);
    }

    /// 统一验证连接意图并在异步信任回执后恢复原消费端；SFTP 请求 ID 在首轮预检前发布，
    /// 后续信任等待与重试沿用该 ID，使 Root 能拒绝抢占新焦点的迟到成功回执。
    ///
    /// @author kongweiguang
    fn connect_selected_with_destination(
        &mut self,
        sftp: bool,
        create_transfer_tab: bool,
        intent_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.cancel_connection_request();
        let Some(host_id) = self.selected_host.clone() else {
            self.status =
                Some(crate::locale::text("请先选择一台主机。", "Select a host first.").to_string());
            cx.notify();
            return;
        };
        let Some(host) = self.rows.iter().find(|host| host.id == host_id).cloned() else {
            self.status = Some(
                crate::locale::text(
                    "所选主机已不存在，请刷新列表。",
                    "The selected host no longer exists. Refresh the list.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        };
        if (sftp || host.protocol == RemoteProtocol::Sftp)
            && !host_sftp_capable(host.protocol, host.sftp_options.enabled)
        {
            self.status = Some(
                if matches!(host.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
                    crate::locale::text(
                        "此主机已禁用 SFTP，可在文件传输设置中启用。",
                        "SFTP is disabled for this host. Enable it in file transfer settings.",
                    )
                } else {
                    crate::locale::text(
                        "此协议不支持 SFTP。",
                        "This protocol does not support SFTP.",
                    )
                }
                .to_string(),
            );
            cx.notify();
            return;
        }
        let action = match (host.protocol, sftp) {
            (RemoteProtocol::Ssh, true)
            | (RemoteProtocol::Sftp, true)
            | (RemoteProtocol::Sftp, false) => match FileEndpoint::remote(
                host.id.clone(),
                host.sftp_options
                    .remote_start_directory
                    .as_deref()
                    .unwrap_or("/"),
            ) {
                Ok(endpoint) if create_transfer_tab => PendingAction::SftpTransferTab {
                    endpoint,
                    title: crate::locale::text(
                        format!("传输 · {}", host.name),
                        format!("Transfer · {}", host.name),
                    )
                    .to_string(),
                    request_id: uuid::Uuid::new_v4().to_string(),
                },
                Ok(endpoint) => PendingAction::Sftp {
                    endpoint,
                    request_id: uuid::Uuid::new_v4().to_string(),
                },
                Err(_) => {
                    self.status = Some(
                        crate::locale::text(
                            "无法创建远端文件目标。",
                            "Could not create the remote file target.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                }
            },
            (RemoteProtocol::Ssh, false) => PendingAction::Terminal {
                host_id: host.id.clone(),
                intent_id: intent_id.clone(),
                target: SessionTarget::Ssh(SshSessionTarget {
                    host_id: host.id.clone(),
                    working_directory: None,
                    command: None,
                }),
            },
            (RemoteProtocol::Telnet, false) => PendingAction::Terminal {
                host_id: host.id.clone(),
                intent_id: intent_id.clone(),
                target: SessionTarget::Telnet(TelnetSessionTarget {
                    host: host.endpoint.host.clone(),
                    port: host.endpoint.port,
                }),
            },
            (RemoteProtocol::Serial, false) => match host.serial.clone() {
                Some(serial) => PendingAction::Terminal {
                    host_id: host.id.clone(),
                    intent_id: intent_id.clone(),
                    target: SessionTarget::Serial(serial),
                },
                None => {
                    self.status = Some(
                        crate::locale::text(
                            "串口连接缺少线制配置，请编辑主机。",
                            "Serial settings are missing. Edit the host configuration.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                }
            },
            (RemoteProtocol::Rdp, false) => {
                let Some(service) = self.rdp.clone() else {
                    self.status = Some(
                        crate::locale::text(
                            "当前平台没有可用的 RDP 启动服务。",
                            "No RDP launcher is available on this platform.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                };
                self.rdp_request_generation = self.rdp_request_generation.wrapping_add(1);
                let request_generation = self.rdp_request_generation;
                self.rdp_status_cancellation.cancel();
                self.rdp_status_cancellation = CancellationToken::new();
                self.rdp_status_host = None;
                self.rdp_status_launch_id = None;
                self.rdp_status_lifecycle = None;
                self.rdp_status_message = None;
                self.rdp_status_error = None;
                let executor = self.executor.clone();
                let host_id = host.id.clone();
                cx.spawn(async move |this, cx| {
                    let id = host_id.clone();
                    let result = executor
                        .run(move |cancel| async move {
                            service
                                .launch(
                                    RdpLaunchRequest {
                                        host_id: id.clone(),
                                    },
                                    cancel,
                                )
                                .await
                                .map(|receipt| (id, receipt))
                        })
                        .await;
                    cx.update(|app| {
                        let _ = this.update(app, |panel, cx| {
                            if panel.rdp_request_generation != request_generation {
                                return;
                            }
                            match result {
                                Ok((host_id, receipt)) => {
                                    let launch_id = receipt.launch_id;
                                    let lifecycle = receipt.lifecycle;
                                    panel.status = Some(match receipt.lifecycle {
                                        RdpClientLifecycle::ManagedProcess => crate::locale::text(
                                            "已启动受管 RDP 客户端。",
                                            "The managed RDP client was started.",
                                        ).to_string(),
                                        RdpClientLifecycle::DelegatedToSystem => crate::locale::text(
                                            "已请求系统打开 RDP 文件；登录状态由外部客户端处理。",
                                            "The system was asked to open the RDP file; sign-in is handled by the external client.",
                                        ).to_string(),
                                    });
                                    panel.watch_rdp_status(
                                        host_id.clone(),
                                        launch_id,
                                        lifecycle,
                                        cx,
                                    );
                                    cx.emit(ConnectionsEvent::RdpLaunchCompleted {
                                        host_id,
                                        receipt,
                                    });
                                }
                                Err(error) => {
                                    panel.status = Some(localized_error(&error))
                                }
                            }
                            cx.notify();
                        });
                    });
                })
                .detach();
                self.status = Some(
                    crate::locale::text("正在启动 RDP 客户端…", "Starting RDP client…").to_string(),
                );
                cx.notify();
                return;
            }
            (_, true) => {
                self.status = Some(
                    crate::locale::text(
                        "此协议不支持 SFTP。",
                        "This protocol does not support SFTP.",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
        };
        if let Some(event) = action.sftp_started_event() {
            cx.emit(event);
        }
        if matches!(host.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
            self.preflight_host_keys(host.id, action, cx);
        } else {
            self.emit_action(action, cx);
        }
    }

    /// 创建一个纯表单草稿；已有凭据只显示关联状态，不从加密 vault 回填。
    /// @author kongweiguang
    pub(super) fn create_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        self.cancel_sftp_host_creation_intent(cx);
        self.create_host_with_protocol(RemoteProtocol::Ssh, window, cx);
    }

    /// 复用通用表单构造，只允许明确的 picker 入口覆盖新主机协议默认值。
    ///
    /// @author kongweiguang
    fn create_host_with_protocol(
        &mut self,
        protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        self.cancel_local_host_background_requests();
        self.invalidate_host_file_picker();
        self.cancel_serial_port_scan();
        self.reset_connection_probe();
        self.pending_editor_discard = false;
        self.editor = Some(HostEditor::new_with_protocol(
            None,
            &self.groups,
            protocol,
            window,
            cx,
        ));
        self.active_editor_kind = Some(ConnectionsEditorKind::Remote);
        self.pending_new_host_id = None;
        self.editor_group_revision = self.group_revision.clone();
        self.external_conflict = false;
        self.status = None;
        cx.notify();
    }

    /// 为 Root 左栏底部的真实新增入口打开同一连接编辑器，避免暴露或复制草稿构造逻辑。
    /// @author kongweiguang
    pub fn begin_create_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_host(window, cx);
    }

    /// 从共享文件选择器创建 SFTP 主机；拒绝覆盖现有草稿并携带一次性 Root 回投身份。
    ///
    /// @author kongweiguang
    pub fn begin_create_sftp_host(
        &mut self,
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.refuse_host_editor_replacement(cx) || self.editor.is_some() || self.saving {
            return false;
        }
        self.create_host_with_protocol(RemoteProtocol::Sftp, window, cx);
        self.sftp_host_creation_flow_id = Some(request_id);
        true
    }

    /// 放弃专用创建草稿时通知 Root 撤销回投资格，迟到成功只能保留已保存主机。
    ///
    /// @author kongweiguang
    fn cancel_sftp_host_creation_intent(&mut self, cx: &mut Context<Self>) {
        if let Some(request_id) = self.sftp_host_creation_flow_id.take() {
            cx.emit(ConnectionsEvent::SftpHostCreationCancelled { request_id });
        }
    }

    /// 未知保存与冲突重读期间拒绝替换编辑目标，保持原表单和恢复请求身份稳定。
    /// @author kongweiguang
    pub(super) fn refuse_host_editor_replacement(&mut self, cx: &mut Context<Self>) -> bool {
        if self.refuse_conflict_reload_mutation(cx) {
            return true;
        }
        if !host_editor_replacement_blocked(
            self.saving,
            self.save_outcome_unknown,
            self.pending_host_save_recovery.is_some(),
        ) {
            return false;
        }
        self.status = Some(crate::locale::text(
            "主机保存仍在处理或结果待核对；请先完成明确读回，当前草稿不会被替换。",
            "A host save is active or unresolved. Complete its explicit readback before replacing the current draft.",
        ).to_string());
        cx.notify();
        true
    }

    /// 冲突重读确认或读回期间拒绝保存、探测及目标切换，防止有效回执覆盖当前操作。
    /// @author kongweiguang
    pub(super) fn refuse_conflict_reload_mutation(&mut self, cx: &mut Context<Self>) -> bool {
        if !conflict_reload_blocks_editor_mutation(
            self.pending_conflict_reload_after_discard.is_some(),
            self.conflict_reload_in_progress.is_some(),
        ) {
            return false;
        }
        self.status = Some(crate::locale::text(
            "主机冲突配置正在重新读取；读取完成或取消前不会保存、测试或切换目标。",
            "A conflicted host is being reloaded. Saving, testing, and target changes are paused until the read completes or is cancelled.",
        ).to_string());
        cx.notify();
        true
    }

    /// 取消编辑时先清空输入框中的短生命周期秘密，再丢弃尚未保存的草稿。
    /// @author kongweiguang
    pub(super) fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.local_hosts.saving {
            self.status = Some(
                crate::locale::text(
                    "主机或本地 Profile 保存正在进行；请等待明确结果后再关闭。",
                    "A host or local Profile save is still running. Wait for a confirmed result before closing.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.save_outcome_unknown || self.pending_host_save_recovery.is_some() {
            self.status = Some(crate::locale::text(
                "保存结果仍待明确读回；请先核对，或采用读回配置后再关闭。",
                "The save outcome still needs explicit readback. Review it or adopt the readback before closing.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.local_hosts.mutation_blocked() && !self.local_hosts.can_cancel_editor_load() {
            self.status = Some(crate::locale::text(
                "本地主机结果仍待核对；请先完成读回，再关闭编辑器。",
                "A local host operation is still being reconciled. Finish its readback before closing the editor.",
            ).to_string());
            cx.notify();
            return;
        }
        let remote_dirty = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_dirty(cx));
        let local_dirty = self.local_host_is_dirty(cx);
        if unified_editor_requires_discard(remote_dirty, local_dirty, self.save_outcome_unknown) {
            self.begin_editor_discard_confirmation(window, cx);
            cx.notify();
            return;
        }
        self.close_unified_editor(window, cx);
    }

    /// 优先继续显式冲突重读，其次才处理本地导航或整体关闭；普通丢弃只在确认后清秘密。
    /// @author kongweiguang
    pub(super) fn confirm_discard_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_editor_discard {
            self.editor_discard_return_focus = None;
            if let Some(target) = self.pending_conflict_reload_after_discard.take() {
                self.pending_editor_discard = false;
                match target {
                    ConflictReloadTarget::Remote(host_id) => {
                        self.start_remote_conflict_reload(host_id, cx);
                    }
                    ConflictReloadTarget::Local(profile_id) => {
                        self.local_hosts.pending_action = None;
                        self.start_local_conflict_reload(profile_id, window, cx);
                    }
                }
                return;
            }
            if should_continue_local_host_action_after_discard(
                self.active_editor_kind == Some(ConnectionsEditorKind::Local),
                self.has_pending_local_host_action(),
            ) {
                self.discard_local_host(window, cx);
                if self.active_editor_kind.is_none() && self.editor.is_some() {
                    self.active_editor_kind = Some(ConnectionsEditorKind::Remote);
                }
            } else {
                self.close_unified_editor(window, cx);
            }
        }
    }

    /// 继续编辑恢复仍挂载在当前分区的原焦点；目标失效时使用当前分区字段而非隐藏名称。
    /// @author kongweiguang
    pub(super) fn keep_editor_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancelled_reload = self.pending_conflict_reload_after_discard.is_some()
            || self.conflict_reload_in_progress.is_some();
        if matches!(
            self.conflict_reload_in_progress.as_ref(),
            Some(ConflictReloadTarget::Local(_))
        ) {
            self.local_hosts.cancel_editor_load();
        }
        self.invalidate_conflicted_host_reload();
        if self.active_editor_kind == Some(ConnectionsEditorKind::Local) {
            self.keep_local_host_editor();
        }
        self.pending_editor_discard = false;
        if cancelled_reload {
            self.status = Some(
                crate::locale::text(
                    "已保留当前表单，并取消冲突重读。",
                    "The current form was kept and the conflict reload was cancelled.",
                )
                .to_string(),
            );
            self.local_hosts.status = self.status.clone();
        }
        if let Some(focus) = self.editor_discard_return_focus.take().filter(|focus| {
            !self.host_editor_fields_disabled() && self.editor_fields_focus.contains(focus, window)
        }) {
            window.focus(&focus, cx);
        } else {
            self.focus_editor_primary_input(window, cx);
        }
        cx.notify();
    }

    /// 结束统一表单时使冲突重读失效并清理所有协议草稿与发现任务，避免迟到快照复活表单。
    /// @author kongweiguang
    pub(super) fn close_unified_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.save_outcome_unknown || self.pending_host_save_recovery.is_some() {
            self.status = Some(crate::locale::text(
                "保存结果仍待明确读回；编辑器和秘密保持在当前状态。",
                "The save outcome still needs explicit readback. The editor and its secrets remain available for reconciliation.",
            ).to_string());
            cx.notify();
            return;
        }
        self.invalidate_conflicted_host_reload();
        let outcome_unknown = self.save_outcome_unknown;
        self.cancel_sftp_host_creation_intent(cx);
        self.cancel_serial_port_scan();
        self.invalidate_host_file_picker();
        self.reset_connection_probe();
        if let Some(editor) = &self.editor {
            editor.clear_secrets(window, cx);
        }
        self.editor = None;
        self.clear_local_host_editor_for_unified_close(window, cx);
        self.pending_editor_discard = false;
        self.editor_discard_return_focus = None;
        self.editor_group_revision = None;
        self.pending_new_host_id = None;
        self.external_conflict = false;
        self.active_editor_kind = None;
        if !outcome_unknown {
            self.status = None;
        }
        cx.notify();
        if outcome_unknown {
            self.reload(cx);
        }
    }

    /// 编辑指定主机的当前 revision 快照；保存时仓储以该 revision 执行 CAS。
    /// @author kongweiguang
    pub(super) fn edit_host(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        let Some(host) = self.rows.iter().find(|host| &host.id == host_id).cloned() else {
            self.status = Some(
                crate::locale::text(
                    "主机已不存在，请刷新列表。",
                    "The host no longer exists. Refresh the list.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let needs_serial_scan = host.protocol == RemoteProtocol::Serial;
        self.cancel_local_host_background_requests();
        self.invalidate_host_file_picker();
        self.cancel_serial_port_scan();
        self.reset_connection_probe();
        self.pending_editor_discard = false;
        self.editor = Some(HostEditor::new(Some(host), &self.groups, window, cx));
        self.active_editor_kind = Some(ConnectionsEditorKind::Remote);
        self.pending_new_host_id = None;
        self.editor_group_revision = self.group_revision.clone();
        self.external_conflict = false;
        self.status = None;
        if needs_serial_scan {
            self.refresh_serial_ports(cx);
        }
        cx.notify();
    }

    /// 取消旧的设备快照请求并使它的迟到结果失效。
    /// @author kongweiguang
    fn cancel_serial_port_scan(&mut self) {
        self.serial_port_scan_generation = self.serial_port_scan_generation.wrapping_add(1);
        self.serial_port_scan_cancellation.cancel();
        self.serial_ports_loading = false;
    }

    /// 枚举当前串口快照；手工路径始终保留，取消或换表单后丢弃迟到结果。
    /// @author kongweiguang
    pub(super) fn refresh_serial_ports(&mut self, cx: &mut Context<Self>) {
        if !self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.protocol == RemoteProtocol::Serial)
        {
            return;
        }

        self.cancel_serial_port_scan();
        let generation = self.serial_port_scan_generation;
        self.serial_ports.clear();
        self.serial_ports_loaded = false;
        self.serial_ports_issue = None;
        let Some(service) = self.serial_port_service.clone() else {
            self.serial_ports_issue = Some(SerialPortScanIssue::ServiceUnavailable);
            cx.notify();
            return;
        };

        let request_cancellation = ConnectionRequestCancellation::new();
        self.serial_port_scan_cancellation = request_cancellation.clone();
        self.serial_ports_loading = true;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |executor_cancel| async move {
                    let operation_cancel = request_cancellation.bind_to(&executor_cancel);
                    service.list(operation_cancel).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    if panel.serial_port_scan_generation != generation
                        || !panel
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.protocol == RemoteProtocol::Serial)
                    {
                        return;
                    }
                    panel.serial_ports_loading = false;
                    panel.serial_ports_loaded = true;
                    match result {
                        Ok(ports) => panel.serial_ports = ports,
                        Err(error) => {
                            panel.serial_ports.clear();
                            panel.serial_ports_issue =
                                Some(if error.public_code() == PublicErrorCode::Cancelled {
                                    SerialPortScanIssue::Cancelled
                                } else {
                                    SerialPortScanIssue::EnumerationFailed
                                });
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 只选择当前枚举快照中的名称；设备路径输入仍允许直接编辑。
    /// @author kongweiguang
    pub(super) fn select_serial_port(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        if editor.protocol != RemoteProtocol::Serial
            || !self.serial_ports.iter().any(|port| port.name == name)
        {
            return;
        }
        editor
            .inputs
            .host
            .update(cx, |state, cx| state.set_value(name, window, cx));
        cx.notify();
    }

    /// 仅用当前未保存草稿执行短生命周期 probe，不创建主机记录、会话 Tab 或 vault 写入。
    /// @author kongweiguang
    pub(super) fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_conflict_reload_mutation(cx) {
            return;
        }
        self.run_connection_probe(None, window, cx);
    }

    /// 用户确认未知主机密钥时复用原 peer、endpoint 与指纹，信任只对第二次 probe 有效。
    /// @author kongweiguang
    pub(super) fn confirm_probe_host_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(challenge) = self
            .probe_outcome
            .as_ref()
            .and_then(|outcome| match outcome {
                ConnectionProbeOutcome::RequiresHostKeyTrust(challenge) => Some(challenge.clone()),
                _ => None,
            })
        else {
            return;
        };
        let Some(challenged_draft) = self.probe_challenge_draft.clone() else {
            return;
        };
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let current_draft = match editor.to_draft(&self.groups, cx) {
            Ok(draft) => draft,
            Err(error) => {
                self.probe_outcome = None;
                self.probe_challenge_draft = None;
                self.set_editor_validation_error(&error, window, cx);
                return;
            }
        };
        if current_draft != challenged_draft {
            self.probe_outcome = None;
            self.probe_challenge_draft = None;
            self.probe_error = Some(
                crate::locale::text(
                    "目标草稿已变化，请重新测试后再确认指纹。",
                    "The target draft changed. Test it again before confirming the fingerprint.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.run_connection_probe(
            Some(ProbeHostKeyConfirmation {
                peer: challenge.peer,
                endpoint: challenge.endpoint,
                fingerprint_sha256: challenge.fingerprint_sha256,
            }),
            window,
            cx,
        );
    }

    /// 取消当前 probe 世代并保留表单；迟到的网络结果因世代失配而被丢弃。
    /// @author kongweiguang
    pub(super) fn cancel_connection_probe(&mut self, cx: &mut Context<Self>) {
        if !self.probe_running {
            return;
        }
        self.probe_request.cancel();
        self.probe_running = false;
        self.probe_outcome = None;
        self.probe_challenge_draft = None;
        self.probe_error = Some(
            crate::locale::text(
                "连接测试已取消；草稿仍保留。",
                "Connection test cancelled; the draft is preserved.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 构建每次独立的一次性 probe 请求，并以 executor 令牌和本地世代共同约束结果回投。
    /// @author kongweiguang
    fn run_connection_probe(
        &mut self,
        host_key_confirmation: Option<ProbeHostKeyConfirmation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = self.probe_service.clone() else {
            return;
        };
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let draft = match editor.to_draft(&self.groups, cx) {
            Ok(draft) => draft,
            Err(error) => {
                self.probe_outcome = None;
                self.set_editor_validation_error(&error, window, cx);
                return;
            }
        };
        let challenge_draft = draft.clone();
        let credential_changes = editor.credential_changes(cx);
        let request = ConnectionProbeRequest {
            draft,
            credential_changes,
            timeout: Duration::from_secs(20),
            host_key_confirmation,
        };
        let executor = self.executor.clone();
        let (generation, request_cancellation) = self.probe_request.begin();
        let callback_cancellation = request_cancellation.clone();
        self.probe_running = true;
        self.probe_outcome = None;
        self.probe_error = None;
        self.probe_challenge_draft = None;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |executor_token| async move {
                    let cancellation = request_cancellation.bind_to(&executor_token);
                    service.probe(request, cancellation).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    if !panel
                        .probe_request
                        .is_current(generation, &callback_cancellation)
                        || panel.editor.is_none()
                    {
                        return;
                    }
                    panel.probe_running = false;
                    match result {
                        Ok(outcome) => {
                            if matches!(outcome, ConnectionProbeOutcome::RequiresHostKeyTrust(_)) {
                                panel.probe_challenge_draft = Some(challenge_draft);
                            }
                            panel.probe_outcome = Some(outcome);
                            panel.probe_error = None;
                        }
                        Err(error) => {
                            panel.probe_challenge_draft = None;
                            panel.probe_outcome = None;
                            panel.probe_error =
                                Some(if error.public_code() == PublicErrorCode::Cancelled {
                                    crate::locale::text(
                                        "连接测试已取消；草稿仍保留。",
                                        "Connection test cancelled; the draft is preserved.",
                                    )
                                    .to_string()
                                } else {
                                    localized_error(&error)
                                });
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 输入或目标变化时取消旧 probe，并清除它或旧校验对应的草稿反馈。
    /// @author kongweiguang
    pub(super) fn reset_connection_probe(&mut self) {
        self.probe_request.cancel();
        self.probe_running = false;
        self.probe_outcome = None;
        self.probe_error = None;
        self.probe_challenge_draft = None;
        let cleared_validation_issue = self
            .editor
            .as_mut()
            .is_some_and(|editor| editor.validation_issue.take().is_some());
        if cleared_validation_issue {
            self.status = None;
        }
    }

    /// 把稳定公开的输入错误放回对应控件；未知错误仍留在状态区，不做猜测。
    /// @author kongweiguang
    fn set_editor_validation_error(
        &mut self,
        error: &AppError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = localized_error(error);
        let issue = HostEditorValidationIssue::from_error(error, message.clone());
        if let Some(editor) = self.editor.as_mut() {
            if let Some(issue) = &issue {
                // A stale or cross-protocol error must remain visible in the summary instead of
                // selecting a section that the current protocol cannot render.
                editor.section = HostEditorSection::normalize(editor.protocol, issue.section);
            }
            editor.validation_issue = issue.clone();
        }
        self.status = issue.is_none().then_some(message);
        if issue.is_some() {
            self.focus_editor_validation(window, cx);
        }
        cx.notify();
    }

    /// 按字段错误挂载的同一 ScrollAnchor 先滚动到可见坐标，再聚焦对应控件。
    /// @author kongweiguang
    pub(super) fn focus_editor_validation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let Some(issue) = editor.validation_issue.as_ref() else {
            return;
        };
        let focus = editor_validation_focus_handle(editor, issue.field, cx);
        let anchor = editor.validation_anchor.clone();
        let protocol = editor.protocol;
        let editor_name_id = editor.inputs.name.entity_id();
        let issue = issue.clone();
        let panel_entity = cx.entity().clone();
        window.on_next_frame(move |window, cx| {
            panel_entity.update(cx, |panel, cx| {
                if panel.pending_editor_discard || panel.presentation.shows_group_editor() {
                    return;
                }
                let Some(editor) = panel.editor.as_ref() else {
                    return;
                };
                if editor.inputs.name.entity_id() != editor_name_id
                    || editor.protocol != protocol
                    || editor.validation_issue.as_ref() != Some(&issue)
                {
                    return;
                }
                anchor.scroll_to(window, cx);
                if let Some(focus) = focus {
                    window.focus(&focus, cx);
                }
            });
        });
    }

    /// 解析表单并由 application 编排凭据 vault、host CAS 和安全清理，不从 UI 访问配置文件。
    /// @author kongweiguang
    pub(super) fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_conflict_reload_mutation(cx) {
            return;
        }
        if self.local_hosts.saving || self.local_hosts.save_outcome_unknown {
            self.status = Some(crate::locale::text(
                "本地主机保存仍在处理或结果待核对，暂不能提交远程主机。",
                "A local host save is active or unresolved, so a remote host cannot be saved yet.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.save_outcome_unknown {
            self.status = Some(crate::locale::text(
                "上次保存结果尚未确认。请先按原主机 ID 定向读回；确认保存前状态后才可明确重试。",
                "The previous save outcome is still unknown. Read back the original host ID; retry is available only after the pre-save state is confirmed.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.probe_running {
            self.status = Some(
                crate::locale::text(
                    "连接测试仍在运行；请先取消或等待结果。",
                    "A connection test is still running. Cancel it or wait for its result first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，主机修改已禁用。",
                    "Configuration is in read-only recovery mode; host changes are disabled.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let mut draft = match editor.to_draft(&self.groups, cx) {
            Ok(draft) => draft,
            Err(error) => {
                self.set_editor_validation_error(&error, window, cx);
                cx.notify();
                return;
            }
        };
        if draft.id.is_none() {
            let host_id = self.pending_new_host_id.get_or_insert_with(HostId::new);
            draft.id = Some(host_id.clone());
        }
        let changes = editor.credential_changes(cx);
        if matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
            match draft.authentication_mode {
                HostAuthenticationMode::Password
                    if !matches!(
                        draft.authentication.as_ref(),
                        Some(SshAuthentication::Password { .. })
                    ) && !changes.iter().any(|change| {
                        matches!(
                            change,
                            HostCredentialChange::Replace {
                                slot: HostCredentialSlot::TargetPassword,
                                ..
                            }
                        )
                    }) =>
                {
                    self.set_editor_validation_error(
                        &AppError::invalid("密码认证需要输入新口令或保留已有 vault 凭据。"),
                        window,
                        cx,
                    );
                    cx.notify();
                    return;
                }
                HostAuthenticationMode::PrivateKey => {
                    let valid_key =
                        private_key_save_has_material(draft.authentication.as_ref(), &changes);
                    if !valid_key {
                        self.set_editor_validation_error(
                            &AppError::invalid(
                                "私钥认证需要选择私钥文件、粘贴私钥或保留已保存的私钥。",
                            ),
                            window,
                            cx,
                        );
                        cx.notify();
                        return;
                    }
                }
                _ => {}
            }
        }
        let revision = editor.original.as_ref().map(|host| host.revision.clone());
        let recovery = PendingHostSaveRecovery {
            host_id: draft
                .id
                .clone()
                .expect("remote host save always has a preallocated stable ID"),
            candidate: draft.clone(),
            baseline_revision: revision.clone(),
            credential_changes: host_credential_recovery_actions(&changes),
            credential_cleanup: None,
        };
        let executor = self.executor.clone();
        let hosts = self.hosts.clone();
        self.reset_connection_probe();
        self.saving = true;
        self.host_save_readback = None;
        self.host_save_readback_in_progress = false;
        self.status = Some(
            crate::locale::text(
                "正在安全保存主机配置…",
                "Saving host configuration securely…",
            )
            .to_string(),
        );
        cx.spawn(async move |this, cx| {
            let recovery = recovery.clone();
            let result = executor
                .run(move |cancel| async move {
                    hosts
                        .save_host_with_credentials_recoverable(draft, revision, changes, cancel)
                        .await
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_save(result, recovery, window, cx)
            });
        })
        .detach();
        cx.notify();
    }

    /// 只有已知 host CAS 成功后才关闭表单；先使旧目录读取失效并注入回执主机，避免自动回投读到旧行。
    /// Conflict 保留用户字段供刷新后重新应用。
    /// @author kongweiguang
    fn finish_save(
        &mut self,
        result: Result<RecoverableHostSaveOutcome, AppError>,
        mut recovery: PendingHostSaveRecovery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saving = false;
        let result = match result {
            Ok(RecoverableHostSaveOutcome::Saved(receipt)) => Ok(*receipt),
            Ok(RecoverableHostSaveOutcome::OutcomeUnknown {
                error,
                recovery: cleanup,
            }) => {
                recovery.credential_cleanup = Some(cleanup);
                Err(error)
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(receipt) => {
                let saved_host = receipt.host.clone();
                self.selected_host = Some(receipt.host.id.clone());
                self.save_outcome_unknown = false;
                self.pending_host_save_recovery = None;
                self.host_save_readback = None;
                self.host_save_readback_in_progress = false;
                self.host_save_readback_generation =
                    self.host_save_readback_generation.wrapping_add(1);
                self.pending_new_host_id = None;
                let success_status = match receipt.cleanup {
                    kerminal_application::connections::management::CredentialCleanupStatus::Complete => crate::locale::text("主机和凭据已保存。", "Host and credentials saved.").to_string(),
                    kerminal_application::connections::management::CredentialCleanupStatus::Deferred { remaining } => crate::locale::text(format!("主机已保存；{} 个旧凭据待安全清理。", remaining), format!("Host saved; {} old credentials still need secure cleanup.", remaining)).to_string(),
                };
                // 用已确认的保存回执短暂补齐当前行，供 Root 在重读完成前安全启动同一 HostId 预检。
                self.host_snapshot_generation
                    .invalidate_after_confirmed_save();
                upsert_saved_host_row(&mut self.rows, saved_host.clone());
                cx.emit(saved_selection_event(SavedSelectionTarget::Host(
                    saved_host.id.clone(),
                )));
                if let Some(event) = sftp_host_creation_saved_event(
                    self.sftp_host_creation_flow_id.take(),
                    saved_host.id.clone(),
                    saved_host.protocol,
                    saved_host.sftp_options.enabled,
                ) {
                    cx.emit(event);
                }
                self.close_unified_editor(window, cx);
                self.status = Some(success_status);
                self.reload(cx);
            }
            Err(error) => {
                let unknown = error.public_code() == PublicErrorCode::OutcomeUnknown;
                self.save_outcome_unknown = unknown;
                if unknown {
                    self.pending_host_save_recovery = Some(recovery);
                    self.host_save_readback = None;
                    self.host_save_readback_in_progress = false;
                    self.host_save_readback_generation =
                        self.host_save_readback_generation.wrapping_add(1);
                } else {
                    self.pending_host_save_recovery = None;
                    self.host_save_readback = None;
                    self.host_save_readback_in_progress = false;
                }
                if error.public_code() == PublicErrorCode::Conflict {
                    self.external_conflict = true;
                }
                if unknown {
                    self.status = Some(crate::locale::text("保存结果暂时无法确认。当前草稿和凭据仍保留，请执行定向读回；普通刷新不会解除门禁，也不会自动重试。", "The save outcome is unknown. The draft and credentials are preserved. Read back this host explicitly; ordinary refresh will not clear the gate or retry the save.").to_string());
                } else if error.public_code() == PublicErrorCode::Invalid {
                    self.set_editor_validation_error(&error, window, cx);
                } else {
                    self.status = Some(localized_error(&error));
                }
            }
        }
        cx.notify();
    }

    /// 只按本次保存捕获的 HostId 定向读回；普通列表刷新和旧回执都不能解除未知结果门禁。
    /// @author kongweiguang
    pub(super) fn review_unknown_host_save(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.save_outcome_unknown || self.host_save_readback_in_progress {
            return;
        }
        let Some(recovery) = self.pending_host_save_recovery.as_ref() else {
            self.status = Some(crate::locale::text(
                "缺少本次保存的核对目标；结果仍未知，请勿重试。",
                "The original save target is unavailable. Its outcome remains unknown; do not retry.",
            ).to_string());
            cx.notify();
            return;
        };
        let host_id = recovery.host_id.clone();
        let request_host_id = host_id.clone();
        let generation = self.host_save_readback_generation.wrapping_add(1);
        self.host_save_readback_generation = generation;
        self.host_save_readback_in_progress = true;
        self.host_save_readback = None;
        self.status = Some(
            crate::locale::text(
                "正在按原主机 ID 读取当前配置…",
                "Reading the current configuration by the original host ID…",
            )
            .to_string(),
        );
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { hosts.load_host(&request_host_id).await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_unknown_host_save_readback(generation, host_id, result, window, cx)
            });
        })
        .detach();
        cx.notify();
    }

    /// 渲染层只读核对进度，不访问或复制恢复快照。
    /// @author kongweiguang
    pub(super) fn unknown_host_save_review_in_progress(&self) -> bool {
        self.save_outcome_unknown && self.host_save_readback_in_progress
    }

    /// 只有本次恢复代次读到且仍绑定同一 HostId 的冲突快照才允许显式采用。
    /// @author kongweiguang
    pub(super) fn unknown_host_save_readback_available(&self) -> bool {
        let Some(recovery) = self.pending_host_save_recovery.as_ref() else {
            return false;
        };
        self.save_outcome_unknown
            && !self.host_save_readback_in_progress
            && self.host_save_readback.as_ref().is_some_and(|readback| {
                readback.generation == self.host_save_readback_generation
                    && readback.host_id == recovery.host_id
                    && readback.decision == HostSaveReadbackDecision::RequiresAdoption
            })
    }

    /// 只向渲染层提供当前恢复快照的非秘密摘要，且只允许审阅需要用户采用的读回。
    /// @author kongweiguang
    pub(super) fn unknown_host_save_readback_summary(&self) -> Option<HostSaveReadbackSummary> {
        if !self.unknown_host_save_readback_available() {
            return None;
        }
        let recovery = self.pending_host_save_recovery.as_ref()?;
        let readback = self.host_save_readback.as_ref()?;
        host_save_readback_summary(
            readback,
            self.host_save_readback_generation,
            &recovery.host_id,
        )
    }

    /// 用户明确采用当前读回快照后才丢弃旧草稿与秘密；缺失快照代表该 HostId 已不存在。
    /// @author kongweiguang
    pub(super) fn use_host_save_readback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.unknown_host_save_readback_available() {
            return;
        }
        let Some(readback) = self.host_save_readback.take() else {
            return;
        };
        let host_id = readback.host_id.clone();
        self.host_snapshot_generation
            .invalidate_after_authoritative_readback();
        let status = if let Some(host) = readback.host {
            self.selected_host = Some(host.id.clone());
            upsert_saved_host_row(&mut self.rows, host.clone());
            cx.emit(saved_selection_event(SavedSelectionTarget::Host(
                host.id.clone(),
            )));
            if let Some(event) = sftp_host_creation_saved_event(
                self.sftp_host_creation_flow_id.take(),
                host.id.clone(),
                host.protocol,
                host.sftp_options.enabled,
            ) {
                cx.emit(event);
            }
            crate::locale::text(
                "已采用明确读回的主机配置；原草稿和未确认秘密已丢弃。",
                "The explicitly read-back host configuration was adopted. The old draft and unconfirmed secrets were discarded.",
            ).to_string()
        } else {
            self.rows.retain(|host| host.id != host_id);
            if self.selected_host.as_ref() == Some(&host_id) {
                self.selected_host = None;
            }
            self.cancel_sftp_host_creation_intent(cx);
            crate::locale::text(
                "读回确认该主机 ID 当前不存在；已放弃原草稿。",
                "Readback confirms this host ID is currently absent. The old draft was discarded.",
            )
            .to_string()
        };
        self.save_outcome_unknown = false;
        self.pending_host_save_recovery = None;
        self.host_save_readback = None;
        self.host_save_readback_in_progress = false;
        self.host_save_readback_generation = self.host_save_readback_generation.wrapping_add(1);
        self.pending_new_host_id = None;
        self.external_conflict = false;
        self.close_unified_editor(window, cx);
        self.status_after_reload = Some(status);
        self.reload(cx);
    }

    /// Vault 清理回执绑定同一次定向读回；清理失败继续保留 token 和未知门禁供显式重试。
    /// @author kongweiguang
    fn finish_unknown_host_credential_cleanup(
        &mut self,
        generation: u64,
        host_id: HostId,
        host: Option<ManagedHost>,
        result: Result<CredentialCleanupStatus, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !host_save_readback_matches_pending(
            generation,
            self.host_save_readback_generation,
            self.save_outcome_unknown,
            self.pending_host_save_recovery.as_ref(),
            &host_id,
        ) {
            return;
        }
        self.host_save_readback_in_progress = false;
        match result {
            Ok(CredentialCleanupStatus::Complete) => {
                if let Some(recovery) = self.pending_host_save_recovery.as_mut() {
                    recovery.credential_cleanup = None;
                }
                self.finish_unknown_host_save_readback(generation, host_id, Ok(host), window, cx);
                return;
            }
            Ok(CredentialCleanupStatus::Deferred { remaining }) => {
                self.status = Some(crate::locale::text(
                    format!("配置已读回，但 {remaining} 项凭据仍待安全清理；门禁保持，请重试读回核对。"),
                    format!("Configuration was read back, but {remaining} credentials still require safe cleanup. The gate remains; retry readback."),
                ).to_string());
            }
            Err(error) => {
                self.status = Some(crate::locale::text(
                    format!("配置已读回，但凭据恢复失败；门禁保持，请解决问题后重试：{}", localized_error(&error)),
                    format!("Configuration was read back, but credential recovery failed. The gate remains; resolve the issue and retry: {}", localized_error(&error)),
                ).to_string());
            }
        }
        self.host_save_readback = None;
        cx.notify();
    }

    /// 丢弃迟到回执；只有仍处于同一未知保存和显式读回代次的结果可以影响恢复状态。
    /// 先让应用层保护已引用凭据并清理本次孤立引用，完成后才允许重试、确认提交或采用快照。
    /// @author kongweiguang
    fn finish_unknown_host_save_readback(
        &mut self,
        generation: u64,
        host_id: HostId,
        result: Result<Option<ManagedHost>, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_current = host_save_readback_matches_pending(
            generation,
            self.host_save_readback_generation,
            self.save_outcome_unknown,
            self.pending_host_save_recovery.as_ref(),
            &host_id,
        );
        if !is_current {
            return;
        }
        self.host_save_readback_in_progress = false;
        match result {
            Ok(host) => {
                if let Some(token) = self
                    .pending_host_save_recovery
                    .as_ref()
                    .and_then(|recovery| recovery.credential_cleanup.clone())
                {
                    self.host_save_readback_in_progress = true;
                    self.status = Some(crate::locale::text(
                        "配置已读回，正在核对凭据引用并清理本次未引用的凭据…",
                        "Configuration was read back. Checking references and safely cleaning unused credentials from this save…",
                    ).to_string());
                    let hosts = self.hosts.clone();
                    let executor = self.executor.clone();
                    cx.spawn(async move |this, cx| {
                        let result =
                            executor
                                .run(move |_| async move {
                                    hosts.recover_unknown_host_save(&token).await
                                })
                                .await;
                        let _ = this.update_in(cx, |panel, window, cx| {
                            panel.finish_unknown_host_credential_cleanup(
                                generation, host_id, host, result, window, cx,
                            );
                        });
                    })
                    .detach();
                    cx.notify();
                    return;
                }
                let decision = self
                    .pending_host_save_recovery
                    .as_ref()
                    .map(|recovery| host_save_readback_decision(recovery, host.as_ref()))
                    .unwrap_or(HostSaveReadbackDecision::RequiresAdoption);
                self.host_snapshot_generation
                    .invalidate_after_authoritative_readback();
                match decision {
                    HostSaveReadbackDecision::ConfirmedUnsubmitted => {
                        if let Some(host) = host {
                            upsert_saved_host_row(&mut self.rows, host);
                        } else {
                            self.rows.retain(|candidate| candidate.id != host_id);
                        }
                        let retry_id = self
                            .pending_host_save_recovery
                            .as_ref()
                            .filter(|recovery| recovery.baseline_revision.is_none())
                            .map(|recovery| recovery.host_id.clone());
                        self.save_outcome_unknown = false;
                        self.pending_host_save_recovery = None;
                        self.host_save_readback = None;
                        self.pending_new_host_id = retry_id;
                        self.status = Some(crate::locale::text(
                            "读回确认当前仍是保存前状态；草稿保留，现可明确重试保存。",
                            "Readback confirms the current configuration is still at its pre-save state. The draft is preserved and may now be explicitly retried.",
                        ).to_string());
                    }
                    HostSaveReadbackDecision::ConfirmedCommitted => {
                        let Some(host) = host else {
                            return;
                        };
                        self.selected_host = Some(host.id.clone());
                        upsert_saved_host_row(&mut self.rows, host.clone());
                        cx.emit(saved_selection_event(SavedSelectionTarget::Host(
                            host.id.clone(),
                        )));
                        if let Some(event) = sftp_host_creation_saved_event(
                            self.sftp_host_creation_flow_id.take(),
                            host.id.clone(),
                            host.protocol,
                            host.sftp_options.enabled,
                        ) {
                            cx.emit(event);
                        }
                        self.save_outcome_unknown = false;
                        self.pending_host_save_recovery = None;
                        self.host_save_readback = None;
                        self.pending_new_host_id = None;
                        self.close_unified_editor(window, cx);
                        self.status_after_reload = Some(crate::locale::text(
                            "定向读回确认主机配置已保存；本次核对只覆盖配置与凭据引用状态。",
                            "Targeted readback confirms the host configuration was saved. This review covers configuration and credential-reference state only.",
                        ).to_string());
                        self.reload(cx);
                    }
                    HostSaveReadbackDecision::RequiresAdoption => {
                        self.host_save_readback = Some(HostSaveReadback {
                            generation,
                            host_id,
                            host,
                            decision,
                        });
                        self.status = Some(crate::locale::text(
                            "读回与保存前状态不同，或包含无法归属到本次保存的凭据变化。门禁保持；请核对快照后明确采用读回配置。",
                            "Readback differs from the pre-save state or contains credential changes that cannot be attributed to this save. The gate remains; review the snapshot and explicitly adopt it to continue.",
                        ).to_string());
                    }
                }
            }
            Err(error) => {
                self.host_save_readback = None;
                self.status = Some(crate::locale::text(
                    format!("定向读回失败，保存结果仍未知，门禁保持。请先解决配置恢复问题后重试读回：{}", localized_error(&error)),
                    format!("Targeted readback failed; the save outcome remains unknown and the gate stays active. Resolve configuration recovery, then retry readback: {}", localized_error(&error)),
                ).to_string());
            }
        }
        cx.notify();
    }

    /// 通过二次确认构造端点绑定请求；共享 mutation 生命周期负责提交、取消与回读。
    /// @author kongweiguang
    pub(super) fn confirm_host_key(&mut self, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_trust.clone() else {
            return;
        };
        if pending.operation_busy {
            return;
        }
        if !host_key_trust_allowed(&pending.challenge.status, self.recovery.mode) {
            self.status = Some(match &pending.challenge.status {
                HostKeyStatus::Changed { .. } => crate::locale::text(
                    "主机指纹与已保存值不同，连接已阻止。请先通过独立可信渠道核实新指纹。",
                    "The host key differs from the saved key, so the connection is blocked. Verify the new key through an independent trusted channel.",
                )
                .to_string(),
                HostKeyStatus::Unknown | HostKeyStatus::Known => crate::locale::text(
                    "配置处于只读恢复模式，无法保存此主机指纹；连接不会继续。请先恢复配置后重新连接。",
                    "Configuration is in read-only recovery mode, so this host key cannot be saved and the connection will not continue. Recover configuration before reconnecting.",
                )
                .to_string(),
            });
            cx.notify();
            return;
        }
        if !pending.second_confirmation {
            if let Some(current) = self.pending_trust.as_mut() {
                current.second_confirmation = true;
            }
            self.status = Some(crate::locale::text(
                "已标记指纹已核对；再次点击“信任并继续”才会写入 known_hosts。",
                "Key marked as verified. Click “Trust key and continue” again to update known_hosts.",
            ).to_string());
            cx.notify();
            return;
        }

        let challenge = pending.challenge.clone();
        let request = HostKeyTrustRequest {
            host_id: challenge.host_id.clone(),
            peer: challenge.peer.clone(),
            endpoint: challenge.endpoint.clone(),
            expected_fingerprint_sha256: challenge.fingerprint_sha256.clone(),
        };
        pending.second_confirmation = true;
        super::host_key_mutation::start_host_key_mutation(
            self,
            pending,
            super::host_key_mutation::HostKeyMutationKind::Trust,
            "主机指纹信任已取消",
            move |remote, cancel| async move { remote.trust_host_key(request, cancel).await },
            cx,
        );
    }

    /// 轮换只构造绑定旧/新指纹的请求；提交时序由共享生命周期集中保障。
    /// @author kongweiguang
    pub(super) fn confirm_host_key_rotation(&mut self, cx: &mut Context<Self>) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，无法替换主机指纹。",
                    "Configuration is read-only during recovery; the host key cannot be replaced.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(mut pending) = self.pending_trust.clone() else {
            return;
        };
        if pending.operation_busy {
            return;
        }
        let HostKeyStatus::Changed {
            previous_fingerprint_sha256,
        } = &pending.challenge.status
        else {
            return;
        };
        if previous_fingerprint_sha256 == &pending.challenge.fingerprint_sha256 {
            self.status = Some(
                crate::locale::text(
                    "新指纹被标记为已撤销，不能替换或信任。",
                    "The observed key is revoked and cannot be replaced or trusted.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if !pending.second_confirmation {
            if let Some(current) = self.pending_trust.as_mut() {
                current.second_confirmation = true;
            }
            self.status = Some(crate::locale::text(
                "请再次确认：只替换当前主机与端口的旧指纹，并在保存成功后重试原连接。",
                "Confirm once more: replace only the old key for this host and port, then retry the original connection after the save succeeds.",
            ).to_string());
            cx.notify();
            return;
        }

        let challenge = pending.challenge.clone();
        let request = HostKeyRotationRequest {
            host_id: challenge.host_id.clone(),
            peer: challenge.peer.clone(),
            endpoint: challenge.endpoint.clone(),
            expected_previous_fingerprint_sha256: previous_fingerprint_sha256.clone(),
            expected_fingerprint_sha256: challenge.fingerprint_sha256.clone(),
        };
        pending.second_confirmation = true;
        super::host_key_mutation::start_host_key_mutation(
            self,
            pending,
            super::host_key_mutation::HostKeyMutationKind::Rotation,
            "主机密钥轮换已取消",
            move |remote, cancel| async move { remote.replace_host_key(request, cancel).await },
            cx,
        );
    }

    /// 写入前可直接取消连接；写入开始后只请求取消并等回执/回读，避免误报信任记录未变。
    /// @author kongweiguang
    pub(super) fn cancel_pending_trust(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending_trust.as_ref()
            && pending.operation_busy
        {
            if let Some(cancellation) = &pending.operation_cancellation {
                cancellation.cancel();
            }
            self.status = Some(crate::locale::text(
                "已请求取消；仍在等待信任写入的真实回执和端点回读，期间不会重复提交。",
                "Cancellation requested. Waiting for the trust write receipt and endpoint readback; no duplicate write will be started.",
            ).to_string());
            cx.notify();
            return;
        }
        self.cancel_connection_request();
        self.status = Some(
            crate::locale::text(
                "已取消连接请求，不会继续建立终端或文件连接。",
                "Connection request cancelled. No terminal or file connection will be started.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 逐个检查跳板和目标指纹；已知 peer 自动继续，未知或变化的指纹必须停在显式确认界面。
    pub(super) fn preflight_host_keys(
        &mut self,
        host_id: HostId,
        action: PendingAction,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.rows.iter().find(|host| host.id == host_id).cloned() else {
            self.status = Some(
                crate::locale::text(
                    "主机已不存在，请刷新列表后重试。",
                    "The host no longer exists. Refresh the list and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        };
        self.preflight_host_key_count(host_id, host.route.jump_hosts.len(), action, cx);
    }

    /// 对保存主机或外部临时目标复用逐跳检查，跳板数量只来自各自的显式路由快照。
    pub(super) fn preflight_host_key_count(
        &mut self,
        host_id: HostId,
        jump_count: usize,
        action: PendingAction,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_container_failure = None;
        self.sidebar_container_intent = match &action {
            PendingAction::Terminal { intent_id, .. } => intent_id.clone(),
            _ => None,
        };
        let (request_generation, request_cancellation) = self.begin_connection_request();
        let callback_cancellation = request_cancellation.clone();
        let mut peers = (0..jump_count)
            .map(|index| HostKeyPeer::Jump { index })
            .collect::<Vec<_>>();
        peers.push(HostKeyPeer::Target);
        let remote = self.remote.clone();
        let executor = self.executor.clone();
        let check_host_id = host_id.clone();
        let expected_host_key = action.expected_host_key().map(str::to_owned);
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |executor_cancel| async move {
                    let cancel = request_cancellation.bind_to(&executor_cancel);
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled("SSH 主机指纹预检已取消"));
                    }
                    for peer in peers {
                        if cancel.is_cancelled() {
                            return Err(AppError::cancelled("SSH 主机指纹预检已取消"));
                        }
                        let challenge = remote
                            .inspect_host_key(check_host_id.clone(), peer.clone(), cancel.clone())
                            .await?;
                        if matches!(peer, HostKeyPeer::Target)
                            && expected_host_key.as_deref().is_some_and(|expected| {
                                !super::external::expected_fingerprint_matches(
                                    expected,
                                    &challenge.fingerprint_sha256,
                                )
                            })
                        {
                            return Err(AppError::host_key_changed());
                        }
                        if challenge.status != HostKeyStatus::Known {
                            return Ok(Some(challenge));
                        }
                    }
                    Ok(None)
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    if !panel
                        .connection_request
                        .is_current(request_generation, &callback_cancellation)
                    {
                        return;
                    }
                    match result {
                        Ok(Some(challenge)) => {
                            let display_host = action.external_peer_label(&challenge.peer);
                            let external_classification = action.external_classification().cloned();
                            panel.pending_trust = Some(PendingTrust {
                                challenge,
                                action,
                                display_host,
                                external_classification,
                                second_confirmation: false,
                                operation_busy: false,
                                operation_cancellation: None,
                            });
                            panel.status = Some(
                                crate::locale::text(
                                    "请核对远端主机指纹，再决定是否继续连接。",
                                    "Verify the remote host key before continuing.",
                                )
                                .to_string(),
                            );
                            cx.notify();
                        }
                        Ok(None) => panel.emit_action(action, cx),
                        Err(error) => {
                            let message = localized_error(&error);
                            panel.sidebar_container_failure = panel
                                .sidebar_container_intent
                                .take()
                                .map(|id| (id, message.clone()));
                            panel.status = Some(message);
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
        self.status = Some(
            crate::locale::text(
                "正在逐跳检查 SSH 主机指纹…",
                "Checking SSH host keys along the route…",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 信任确认后按 action 中的路由重新预检；外部临时 ID 不从保存主机目录反查。
    pub(super) fn preflight_pending_action(
        &mut self,
        host_id: HostId,
        action: PendingAction,
        cx: &mut Context<Self>,
    ) {
        if let PendingAction::External(target) = &action {
            self.preflight_host_key_count(host_id, target.identity.jump_hosts.len(), action, cx);
        } else {
            self.preflight_host_keys(host_id, action, cx);
        }
    }

    /// 创建分组由 application 写入整份 groups.toml 并按 revision CAS，UI 不写配置文件。
    /// 返回结果只有在该编辑器仍可见且草稿未变时才结束视图；隐藏旧编辑器不得继续提交。
    /// @author kongweiguang
    pub(super) fn create_group(&mut self, cx: &mut Context<Self>) {
        if !self.presentation.shows_group_editor() {
            return;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，不能创建分组。",
                    "Configuration is in read-only recovery mode; groups cannot be created.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.configuration_mutation_busy {
            return;
        }
        if self.group_save_reconciliation.is_some() {
            self.status = Some(crate::locale::text(
                "请先核对上次分组保存结果；系统不会自动重试。",
                "Review the previous group save outcome first. No automatic retry will be made.",
            ).to_string());
            cx.notify();
            return;
        }
        let name = self
            .group_name_input
            .update(cx, |input, _| input.value().to_string());
        if name.trim().is_empty() {
            self.status = Some(
                crate::locale::text("请先填写分组名称。", "Enter a group name first.").to_string(),
            );
            cx.notify();
            return;
        }
        let editing_group_id = self.editing_group_id.clone();
        let submitted_name = name.clone();
        let saved_group_id = editing_group_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let existing_group = editing_group_id
            .as_ref()
            .and_then(|id| self.groups.iter().find(|group| &group.id == id));
        if editing_group_id.is_some() && existing_group.is_none() {
            self.status = Some(
                crate::locale::text(
                    "分组已不存在，请刷新后重试。",
                    "The group no longer exists. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        }
        let draft = HostGroupDraft {
            id: Some(saved_group_id.clone()),
            name,
            sort_order: existing_group.map_or_else(
                || {
                    self.groups
                        .iter()
                        .map(|group| group.sort_order)
                        .max()
                        .unwrap_or(-1)
                        + 1
                },
                |group| group.sort_order,
            ),
        };
        let expected = self.group_revision.clone();
        if editing_group_id.is_some() && expected.is_none() {
            self.status = Some(
                crate::locale::text(
                    "分组 revision 尚未就绪，请刷新后重试。",
                    "The group revision is not ready. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload(cx);
            return;
        }
        let hosts = self.hosts.clone();
        let executor = self.executor.clone();
        self.configuration_mutation_busy = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { hosts.save_group(draft, expected).await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                    panel.configuration_mutation_busy = false;
                    let current_name = panel
                        .group_name_input
                        .update(cx, |input, _| input.value().to_string());
                    let finish_group_editor = Self::should_finish_group_editor(
                        result.is_ok(),
                        panel.presentation.shows_group_editor(),
                        editing_group_id.as_deref(),
                        panel.editing_group_id.as_deref(),
                        submitted_name.as_str(),
                        current_name.as_str(),
                    );
                    match result {
                        Ok(snapshot) => {
                            panel.groups = snapshot.groups;
                            panel.group_revision = snapshot.revision;
                            if finish_group_editor {
                                panel.group_save_reconciliation = None;
                                panel.editing_group_id = None;
                                if let Some(inline_return) = panel.group_editor_return.take() {
                                    let created_group_id = panel
                                        .groups
                                        .iter()
                                        .find(|group| group.id == saved_group_id)
                                        .map(|group| group.id.clone());
                                    if let Some(group_id) = created_group_id {
                                        panel.presentation.finish_group_editor();
                                        match inline_return.editor_kind {
                                            ConnectionsEditorKind::Remote => {
                                                if let Some(editor) = panel.editor.as_mut() {
                                                    editor.select_group_after_inline_create(
                                                        &panel.groups,
                                                        group_id,
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }
                                            ConnectionsEditorKind::Local => {
                                                panel.select_local_host_group_after_inline_create(
                                                    group_id,
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }
                                    } else {
                                        panel.group_editor_return = Some(inline_return);
                                        panel.status = Some(crate::locale::text(
                                            "分组保存已确认，但回执中没有对应分组；请重新读取核对。",
                                            "The group save was confirmed, but its matching group was absent from the receipt. Reload and reconcile.",
                                        ).to_string());
                                    }
                                } else {
                                    panel.presentation.finish_group_editor();
                                    cx.emit(ConnectionsEvent::GroupRenameSaved);
                                }
                            }
                            if panel.status.is_none() || panel.group_editor_return.is_none() {
                                panel.status = Some(
                                    crate::locale::text("主机分组已保存。", "Host group saved.")
                                        .to_string(),
                                );
                            }
                            cx.notify();
                        }
                        Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                            panel.group_save_reconciliation =
                                Some(GroupSaveReconciliation::NeedsReadback);
                            panel.status_after_reload = None;
                            panel.presentation.show_group_editor();
                            panel.status = Some(crate::locale::text(
                                "分组保存结果未知；请先核对当前分组配置。草稿已保留，不会自动重试。",
                                "The group save outcome is unknown. Review the current group configuration first. The draft is preserved and will not be retried automatically.",
                            ).to_string());
                            cx.notify();
                        }
                        Err(error) => {
                            panel.status = Some(localized_error(&error));
                            cx.notify();
                        }
                    }
            });
        })
        .detach();
        self.status =
            Some(crate::locale::text("正在保存主机分组…", "Saving host group…").to_string());
        cx.notify();
    }

    /// 只让活跃编辑器中 ID 与名称均匹配的成功回执关闭视图；创建也不能仅凭 None ID 结束。
    ///
    /// @author kongweiguang
    fn should_finish_group_editor(
        save_succeeded: bool,
        group_editor_active: bool,
        saved_group_id: Option<&str>,
        current_group_id: Option<&str>,
        submitted_name: &str,
        current_name: &str,
    ) -> bool {
        save_succeeded
            && group_editor_active
            && saved_group_id == current_group_id
            && submitted_name == current_name
    }

    /// 协议草稿替换输入实体后立即交接焦点，避免按键仍派发给已卸载字段；重复选择保留焦点。
    /// @author kongweiguang
    pub(super) fn set_protocol(
        &mut self,
        protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_conflict_reload_mutation(cx) {
            return;
        }
        self.active_editor_kind = Some(ConnectionsEditorKind::Remote);
        if self.editor_protocol_locked() {
            self.status = Some(crate::locale::text(
                "已保存主机的协议固定；请在原协议中编辑连接字段。",
                "A saved host's protocol is fixed. Edit its connection fields under the original protocol.",
            ).to_string());
            cx.notify();
            return;
        }
        let was_serial = self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.protocol == RemoteProtocol::Serial);
        let mut changed = false;
        if let Some(editor) = self.editor.as_mut() {
            changed = editor.switch_new_protocol(protocol, window, cx);
            if changed {
                editor.refresh_locale(window, cx);
            }
        }
        if changed {
            self.invalidate_host_file_picker();
            self.status = None;
            self.focus_editor_primary_input(window, cx);
            cx.notify();
        }
        if !changed
            && self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.protocol == protocol)
        {
            return;
        }
        self.reset_connection_probe();
        if was_serial && protocol != RemoteProtocol::Serial {
            self.cancel_serial_port_scan();
            self.serial_ports.clear();
            self.serial_ports_loaded = false;
            self.serial_ports_issue = None;
        } else if protocol == RemoteProtocol::Serial {
            self.refresh_serial_ports(cx);
        }
    }

    /// 跨编辑器类型时恢复目标草稿的可见焦点与滚动起点，避免隐藏输入截断 Esc 冒泡。
    ///
    /// @author kongweiguang
    pub(super) fn activate_local_host_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_conflict_reload_mutation(cx) {
            return;
        }
        self.invalidate_host_file_picker();
        self.reset_connection_probe();
        if self.local_hosts.editor.is_some() {
            let returning_to_local = self.active_editor_kind != Some(ConnectionsEditorKind::Local);
            self.active_editor_kind = Some(ConnectionsEditorKind::Local);
            if returning_to_local {
                if let Some(editor) = self.local_hosts.editor.as_ref() {
                    editor.scroll_handle.set_offset(Default::default());
                }
                self.resume_local_host_default_target(cx);
                self.focus_editor_primary_input(window, cx);
            }
            if self.local_hosts.shell_candidates.is_empty() && !self.local_hosts.shell_loading {
                self.discover_local_shells(cx);
            }
            cx.notify();
        } else {
            let selected_group = self
                .editor
                .as_ref()
                .and_then(|editor| editor.selected_group.clone());
            self.begin_create_local_host(selected_group, window, cx);
            self.focus_editor_primary_input(window, cx);
        }
    }

    /// 切出 Local 保留默认目标意图，并将焦点交给远程可见字段；重复协议点击不打断浏览。
    /// @author kongweiguang
    pub(super) fn activate_remote_protocol(
        &mut self,
        protocol: RemoteProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_conflict_reload_mutation(cx) {
            return;
        }
        let returning_to_remote = self.active_editor_kind != Some(ConnectionsEditorKind::Remote);
        let pending_local_defaults = self.local_hosts.default_target_request.take();
        self.cancel_local_host_background_requests();
        self.invalidate_host_file_picker();
        if self.editor.is_none() {
            self.create_host(window, cx);
        }
        self.local_hosts.default_target_request = pending_local_defaults;
        self.active_editor_kind = Some(ConnectionsEditorKind::Remote);
        self.set_protocol(protocol, window, cx);
        if returning_to_remote {
            if let Some(editor) = self.editor.as_ref() {
                editor.scroll_handle.set_offset(Default::default());
            }
            self.focus_editor_primary_input(window, cx);
        }
        cx.notify();
    }

    /// 分区真实切换后同步滚动与可见焦点，保证 Esc、Tab 继续从当前表单派发；重复选择不变。
    /// @author kongweiguang
    pub(super) fn select_editor_section(
        &mut self,
        section: HostEditorSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.editor.as_mut() {
            let section = HostEditorSection::normalize(editor.protocol, section);
            if editor.section == section {
                return;
            }
            editor.scroll_handle.set_offset(Default::default());
            editor.section = section;
            self.focus_editor_primary_input(window, cx);
            cx.notify();
        }
    }

    /// 认证字段卸载前交接到新模式的输入；Agent 没有秘密字段时保留表单按键路径。
    /// @author kongweiguang
    pub(super) fn set_authentication_mode(
        &mut self,
        mode: HostAuthenticationMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.editor.as_mut() {
            if editor.protocol == RemoteProtocol::Rdp && mode == HostAuthenticationMode::PrivateKey
            {
                return;
            }
            if editor.authentication_mode == mode {
                return;
            }
            editor.authentication_mode = mode;
            let focus = match mode {
                HostAuthenticationMode::Password => editor.inputs.secret.read(cx).focus_handle(cx),
                HostAuthenticationMode::PrivateKey => {
                    editor.inputs.key_path.read(cx).focus_handle(cx)
                }
                HostAuthenticationMode::Agent => self.editor_fields_focus.clone(),
            };
            window.focus(&focus, cx);
            cx.notify();
        }
        self.reset_connection_probe();
    }

    /// 用户显式切换分组时只改变草稿归属，不立即写文件或覆盖另一个配置 revision。
    /// @author kongweiguang
    pub(super) fn select_editor_group(&mut self, group_id: Option<String>, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            editor.selected_group = group_id;
            cx.notify();
        }
        self.reset_connection_probe();
    }

    /// 添加/移除有序跳板会改变凭据作用域索引，只有点击保存才经 CAS 发布新路由。
    /// @author kongweiguang
    pub(super) fn add_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            if editor.jumps.len() >= 8 {
                self.status = Some(
                    crate::locale::text(
                        "一条 SSH 路由最多配置 8 个跳板。",
                        "An SSH route supports up to 8 jump hosts.",
                    )
                    .to_string(),
                );
            } else {
                editor
                    .jumps
                    .push(super::form::JumpInputs::empty(window, cx));
            }
            cx.notify();
        }
        self.reset_connection_probe();
    }

    /// 删除跳板后释放秘密并聚焦相邻路由，避免保留已卸载字段焦点或跳回列表首项。
    /// @author kongweiguang
    pub(super) fn remove_jump(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.editor.as_mut()
            && index < editor.jumps.len()
        {
            let jump = editor.jumps.remove(index);
            jump.clear_secrets(window, cx);
            let focus = editor
                .jumps
                .get(index)
                .or_else(|| editor.jumps.last())
                .map(|jump| jump.host.read(cx).focus_handle(cx))
                .unwrap_or_else(|| self.editor_fields_focus.clone());
            window.focus(&focus, cx);
            cx.notify();
        }
        self.reset_connection_probe();
    }

    /// 所有远程字段共享同一门禁，标题栏收藏不能绕过只读、测试或结果待核对状态。
    /// @author kongweiguang
    pub(super) fn host_editor_fields_disabled(&self) -> bool {
        self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
            || self.saving
            || self.conflicted_host_editor_reload_in_progress()
            || self.probe_running
            || self.save_outcome_unknown
    }

    /// 收藏遵循与其它字段相同的门禁；取消测试释放门禁后仍可修改草稿。
    /// @author kongweiguang
    pub(super) fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        if self.host_editor_fields_disabled() {
            return;
        }
        if let Some(editor) = self.editor.as_mut() {
            editor.favorite = !editor.favorite;
            cx.notify();
        }
        self.reset_connection_probe();
    }

    /// 把已验证的目标事件交给 Root，并随传输目标保留其 HostId；SFTP 工作台和传输标签共用首轮
    /// 预检前分配的请求 ID，本地起点仍只作为初始建议，不改现存目录或隐式终端目标。
    ///
    /// @author kongweiguang
    pub(super) fn emit_action(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::Terminal {
                host_id,
                target,
                intent_id,
            } => cx.emit(ConnectionsEvent::ConnectRequested {
                host_id,
                target,
                intent_id,
            }),
            PendingAction::Sftp {
                endpoint,
                request_id,
            } => {
                let initial_local_directory = endpoint
                    .host_id()
                    .and_then(|host_id| self.rows.iter().find(|host| &host.id == host_id))
                    .and_then(|host| host.sftp_options.local_start_directory.clone());
                cx.emit(ConnectionsEvent::OpenSftpRequested {
                    request_id,
                    endpoint,
                    initial_local_directory,
                })
            }
            PendingAction::SftpTransferTab {
                request_id,
                endpoint,
                title,
            } => {
                let initial_local_directory = endpoint
                    .host_id()
                    .and_then(|host_id| self.rows.iter().find(|host| &host.id == host_id))
                    .and_then(|host| host.sftp_options.local_start_directory.clone());
                cx.emit(ConnectionsEvent::CreateSftpTransferTabRequested {
                    request_id,
                    endpoint,
                    title,
                    initial_local_directory,
                })
            }
            PendingAction::External(target) => {
                cx.emit(ConnectionsEvent::ExternalTargetReady { target })
            }
        }
        cx.notify();
    }
}

impl PendingAction {
    /// 两种 SFTP 去向都在首次预检前发布稳定 ID，信任重试不得新建请求身份。
    /// @author kongweiguang
    fn sftp_started_event(&self) -> Option<ConnectionsEvent> {
        match self {
            Self::Sftp { request_id, .. } => Some(ConnectionsEvent::SftpOpenStarted {
                request_id: request_id.clone(),
            }),
            Self::SftpTransferTab { request_id, .. } => {
                Some(ConnectionsEvent::SftpTransferTabOpenStarted {
                    request_id: request_id.clone(),
                })
            }
            _ => None,
        }
    }

    /// SFTP 调用方提供的指纹作为本次目标的额外固定约束，密码和路径不会进入检查日志。
    fn expected_host_key(&self) -> Option<&str> {
        match self {
            Self::External(ExternalRoutedTarget {
                intent:
                    ExternalLaunchIntent::SftpTransfer {
                        expected_host_key, ..
                    },
                ..
            }) => expected_host_key.as_deref(),
            _ => None,
        }
    }

    /// 信任弹层优先显示外部请求中的真实 peer endpoint，避免只展示一次性 external ID。
    fn external_peer_label(&self, peer: &HostKeyPeer) -> Option<String> {
        let Self::External(target) = self else {
            return None;
        };
        let endpoint = match peer {
            HostKeyPeer::Target => Some(&target.identity.endpoint),
            HostKeyPeer::Jump { index } => target.identity.jump_hosts.get(*index),
        }?;
        Some(format!(
            "{}@{}:{}",
            endpoint.username, endpoint.host, endpoint.port
        ))
    }

    /// 保存外部目标的安全分类供确认弹层说明；分类仅用于展示，不复用命中主机的凭据。
    fn external_classification(&self) -> Option<&ExternalTargetClassification> {
        match self {
            Self::External(target) => Some(&target.classification),
            _ => None,
        }
    }
}

impl Drop for ConnectionsPanel {
    /// 关闭面板时取消观察、远端预检并使迟到的用户名续接结果失效。
    /// @author kongweiguang
    fn drop(&mut self) {
        self.configuration_watch_cancellation.cancel();
        self.rdp_status_cancellation.cancel();
        self.connection_request.cancel();
        self.probe_request.cancel();
        self.serial_port_scan_cancellation.cancel();
        self.external_username_generation = self.external_username_generation.wrapping_add(1);
        self.pending_external_username.take();
    }
}

/// 将只读 RDP 运行态翻译成准确提示；系统委派完成不表示外部客户端已启动或登录。
fn rdp_status_text(status: RdpLaunchStatus, lifecycle: RdpClientLifecycle) -> (String, bool) {
    match status {
        RdpLaunchStatus::Requested => match lifecycle {
            RdpClientLifecycle::ManagedProcess => (
                crate::locale::text(
                    "受管 RDP 客户端已启动，等待其退出状态。",
                    "The managed RDP client started; waiting for its exit status.",
                )
                .to_string(),
                false,
            ),
            RdpClientLifecycle::DelegatedToSystem => (
                crate::locale::text(
                    "已请求系统打开 RDP 文件；登录状态由外部客户端处理。",
                    "The system was asked to open the RDP file; sign-in is handled by the external client.",
                )
                .to_string(),
                false,
            ),
        },
        RdpLaunchStatus::DelegatedToSystem => (
            crate::locale::text(
                "系统已处理打开 RDP 文件的请求；客户端登录由外部客户端处理。",
                "The system finished handling the RDP file request; the external client handles sign-in.",
            )
            .to_string(),
            false,
        ),
        RdpLaunchStatus::Failed { code } => (
            crate::locale::text(
                format!("RDP 客户端操作失败（{}）。", code.as_str()),
                format!("The RDP client operation failed ({}).", code.as_str()),
            )
            .to_string(),
            true,
        ),
        RdpLaunchStatus::Closed => (
            crate::locale::text(
                "受管 RDP 客户端已关闭，临时资源已清理。",
                "The managed RDP client closed and its temporary resources were cleaned up.",
            )
            .to_string(),
            false,
        ),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/rdp_status.rs"]
mod rdp_status_tests;

/// 将受控错误文案按 Kit 当前语言映射；已知 TCP 失败提供可操作指引，不暴露底层路径、命令或 source。
/// @author kongweiguang
pub(super) fn localized_error(error: &AppError) -> String {
    let message = match error.public_message() {
        "SSH TCP connection failed" => "无法连接 SSH 主机，请检查网络、地址和端口后重试。",
        message => message,
    };
    let english = match message {
        "无法连接 SSH 主机，请检查网络、地址和端口后重试。" => {
            "Cannot connect to the SSH host. Check the network, address and port, then retry."
        }
        "主机名称必须为 1 到 128 个可见字符" => {
            "Host name must contain 1 to 128 visible characters"
        }
        "主机地址不能为空，网络协议需要非零端口" => {
            "Host address is required and network protocols need a non-zero port"
        }
        "此协议需要有效的用户名" => "This protocol requires a valid username",
        "RDP 用户名不能包含控制字符" => "RDP username cannot contain control characters",
        "所选主机分组已不存在，请刷新后重新选择" => {
            "The selected host group no longer exists; refresh and choose again"
        }
        "Windows 远程桌面客户端 mstsc.exe 不可用" => {
            "Windows Remote Desktop client mstsc.exe is unavailable; enable or install the client and retry"
        }
        "macOS LaunchServices open 客户端不可用" => {
            "macOS could not open the RDP file; install or enable a Remote Desktop client and retry"
        }
        "未找到 FreeRDP 图形客户端，请安装 xfreerdp 或 wlfreerdp" => {
            "No FreeRDP client was found; install xfreerdp or wlfreerdp and retry"
        }
        "连接端口必须为 1 到 65535" => "Connection port must be between 1 and 65535",
        "密码认证需要输入新口令或保留已有 vault 凭据。" => {
            "Password authentication needs a new password or an existing vault credential"
        }
        "私钥认证需要配置 key path 或已有私钥凭据。" => {
            "Private-key authentication needs a key path or a saved private-key credential"
        }
        "波特率必须为 300 到 4000000" => "Baud rate must be between 300 and 4000000",
        "串口波特率必须大于零" => "Serial baud rate must be greater than zero",
        "串口连接必须提供完整串口参数" => {
            "Serial connections require complete line settings"
        }
        "串口 endpoint 必须由设备路径投影，且不包含网络端口或用户名" => {
            "Serial target must use a device path without a network port or username"
        }
        "串口设备路径必须能编码为 UTF-8" => "Serial device path must be valid UTF-8",
        "SSH 超时必须为 1 到 600 秒" => "SSH timeout must be between 1 and 600 seconds",
        "SSH 心跳必须为 0 到 600 秒" => "SSH keepalive must be between 0 and 600 seconds",
        "SSH 超时和心跳间隔必须在 1 到 600 秒之间" => {
            "SSH timeout and keepalive must be between 1 and 600 seconds"
        }
        "SSH 终端类型或默认目录无效" => {
            "SSH terminal type or initial directory is invalid"
        }
        "SFTP 主机并发必须为 1 到 8" => "SFTP host concurrency must be between 1 and 8",
        "SFTP 起始目录或主机并发数无效" => {
            "SFTP start directory or host concurrency is invalid"
        }
        "RDP 窗口尺寸必须成对填写且在支持范围内" => {
            "RDP window width and height must be provided together and be in range"
        }
        "代理端口必须为 1 到 65535" => "Proxy port must be between 1 and 65535",
        "跳板端口必须为 1 到 65535" => "Jump host port must be between 1 and 65535",
        "启用代理时必须填写代理地址" => {
            "A proxy address is required when proxy routing is enabled"
        }
        "代理地址或凭据配置无效" => {
            "Proxy address or credential configuration is invalid"
        }
        "代理密码必须绑定有效的代理用户名" => {
            "A proxy password requires a valid proxy username"
        }
        "跳板名称、认证模式或连接地址无效" => {
            "Jump host name, authentication mode, or endpoint is invalid"
        }
        "主机标签不能包含控制字符" => "Host tags cannot contain control characters",
        "串口参数必须填写在串口设置中，不能作为主机标签" => {
            "Serial line settings belong in serial options, not host tags"
        }
        "主机认证模式与凭据字段不匹配" => {
            "Host authentication mode does not match its credential fields"
        }
        "RDP 认证不能使用 SSH 凭据字段" => {
            "RDP authentication cannot use SSH credential fields"
        }
        "此协议不接受保存的连接凭据" => {
            "This protocol does not accept saved credentials"
        }
        "私钥路径不能包含控制字符" => {
            "Private-key path cannot contain control characters"
        }
        "只有 SSH 与 SFTP 支持代理或跳板路由" => {
            "Only SSH and SFTP support proxy or jump-host routing"
        }
        "用户名不能为空" => "Username cannot be empty",
        _ => match error.public_code() {
            PublicErrorCode::Invalid => "The connection details are invalid",
            PublicErrorCode::NotFound => "The requested host or group was not found",
            PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
                "This connection does not support the requested operation"
            }
            PublicErrorCode::Conflict => {
                "Configuration changed; review the latest revision and retry"
            }
            PublicErrorCode::Cancelled => "The operation was cancelled",
            PublicErrorCode::Infrastructure => "The connection operation failed",
            PublicErrorCode::OutcomeUnknown => {
                "The save outcome is unknown; refresh before retrying"
            }
            PublicErrorCode::AuthenticationFailed => "Connection authentication failed",
            PublicErrorCode::HostKeyUnknown => "The host key has not been verified",
            PublicErrorCode::HostKeyChanged => "The host key changed; the connection was blocked",
            PublicErrorCode::HostKeyTrustMismatch => "Host key verification did not match",
            PublicErrorCode::Timeout => "The operation timed out",
            PublicErrorCode::RemoteExit => "The remote command exited",
            PublicErrorCode::ConfigurationCorrupt => {
                "Configuration is damaged and requires recovery"
            }
        },
    };
    crate::locale::text(message, english).to_string()
}

/// 只允许未知指纹在可写模式下进入显式 trust 流程，变化指纹必须保持 fail-closed。
/// @author kongweiguang
pub(super) fn host_key_trust_allowed(
    status: &HostKeyStatus,
    configuration_mode: ConfigurationMode,
) -> bool {
    configuration_mode == ConfigurationMode::ReadWrite && matches!(status, HostKeyStatus::Unknown)
}

/// 只刷新主机配置对应的目录文档；完整重扫事件始终刷新全部 host/group 快照。
fn host_configuration_changed(paths: &[String], full_rescan_required: bool) -> bool {
    full_rescan_required
        || paths.iter().any(|path| {
            let path = path.replace('\\', "/");
            path == "hosts/groups.toml" || (path.starts_with("hosts/") && path.ends_with(".toml"))
        })
}

/// 本地主机文件与 Host 使用同一外部配置 watcher；全量重扫必须同时刷新两类导航快照。
/// @author kongweiguang
fn profile_configuration_changed(paths: &[String], full_rescan_required: bool) -> bool {
    full_rescan_required
        || paths.iter().any(|path| {
            let path = path.replace('\\', "/");
            path.starts_with("profiles/") && path.ends_with(".toml")
        })
}

/// 主机文件删除成功与凭据清理结果分开呈现，避免清理延迟诱发重复删除。
/// @author kongweiguang
fn host_deletion_notice(receipt: HostDeletionReceipt) -> String {
    match receipt.credential_cleanup {
        CredentialCleanupStatus::Complete => crate::locale::text(
            "主机配置已删除，相关凭据清理完成。",
            "Host configuration was deleted and unreferenced credentials were cleaned up.",
        )
        .to_string(),
        CredentialCleanupStatus::Deferred { remaining } => crate::locale::text(
            format!(
                "主机配置已删除；{} 项凭据仍被引用或尚未确认清理，请勿重复删除主机。",
                remaining
            ),
            format!(
                "Host configuration was deleted; {} credential references remain in use or could not be confirmed as cleaned. Do not repeat the host deletion.",
                remaining
            ),
        )
        .to_string(),
    }
}

/// 只接受同一快照中 ID 与协议均匹配的行，避免主机配置异步重载造成目标身份漂移。
/// @author kongweiguang
pub(super) fn host_rows_contain_protocol<'a>(
    rows: impl IntoIterator<Item = (&'a HostId, RemoteProtocol)>,
    host_id: &HostId,
    expected_protocol: RemoteProtocol,
) -> bool {
    rows.into_iter()
        .any(|(row_id, protocol)| row_id == host_id && protocol == expected_protocol)
}

/// 统一编辑器只要任一缓存表单仍 dirty 或远程保存结果未知，就必须先经过丢弃确认。
/// @author kongweiguang
pub(super) fn unified_editor_requires_discard(
    remote_dirty: bool,
    local_dirty: bool,
    remote_outcome_unknown: bool,
) -> bool {
    remote_dirty || local_dirty || remote_outcome_unknown
}

/// 只有本地表单的脏导航确认继续原动作；普通取消确认必须结束整个统一表单。
/// @author kongweiguang
pub(super) fn should_continue_local_host_action_after_discard(
    local_editor_active: bool,
    pending_local_action: bool,
) -> bool {
    local_editor_active && pending_local_action
}

/// 只有已启用 SFTP 的 SSH 或 SFTP-only 主机能成为工作台/传输标签目标。
///
/// @author kongweiguang
pub(super) fn host_sftp_capable(protocol: RemoteProtocol, enabled: bool) -> bool {
    enabled && matches!(protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
}

/// 仅 picker 专用创建流程回传保存成功；通用侧栏新增主机不会隐式绑定文件工作台。
///
/// @author kongweiguang
pub(super) fn sftp_host_creation_saved_event(
    request_id: Option<String>,
    host_id: HostId,
    protocol: RemoteProtocol,
    sftp_enabled: bool,
) -> Option<ConnectionsEvent> {
    Some(ConnectionsEvent::SftpHostCreationSaved {
        request_id: request_id?,
        host_id,
        can_open_sftp: host_sftp_capable(protocol, sftp_enabled),
    })
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_watch.rs"]
mod connections_watch_tests;

#[cfg(test)]
#[path = "../../../tests/unit/connections_preflight.rs"]
mod connections_preflight_tests;

#[cfg(test)]
#[path = "../../../tests/unit/connections_guardrails.rs"]
mod connections_guardrails_tests;

#[cfg(test)]
#[path = "../../../tests/unit/connections_panel.rs"]
mod connections_panel_tests;

#[cfg(test)]
#[path = "../../../tests/unit/connections_editor_dirty.rs"]
mod connections_editor_dirty_tests;
