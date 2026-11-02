//! 绑定 transport 的 Shell 编辑快照和一次性追加，不从屏幕内容推断命令行。
//!
//! @author kongweiguang

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use kerminal_application::sessions::{
    SHELL_EDITING_INPUT_TOO_LONG_CONTEXT, SHELL_EDITING_MAX_INPUT_CHARACTERS,
    SHELL_EDITING_MAX_SUFFIX_BYTES, ShellEditingAvailability, ShellEditingShell,
    ShellEditingSnapshot, ShellEditingUnavailableReason, TerminalCommandContextVersion,
    UiInputProjection, UiSessionBinding,
};
use kerminal_domain::sessions::{SessionLifecycle, SessionOwner};
use kerminal_terminal::shell_editing::{EditingMessage, EditingShell, parse_editing_message};
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{
    AppError, AppResult, ManagedSession, PendingTerminalInputWrite, SessionChange,
    SessionChangeKind, SessionState, ShellInputOrigin, ShellIntegrationState,
    UnwrittenSuggestionRecovery, finish_terminal_input_write_with_recovery, mark_shell_input,
    write_input_bytes_locked,
};

const PROBE: &[u8] = b"\x1b[24;8~";
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// 唯一在途请求与 transport nonce 一起持有，迟到响应不能转交给新的探测。
pub(super) struct EditingRuntime {
    generation: u64,
    nonce: Option<String>,
    status: EditingStatus,
    revision: u64,
    next_probe: u64,
    pending: Option<PendingProbe>,
    settled: watch::Sender<u64>,
}

enum EditingStatus {
    Pending,
    Installed {
        shell: EditingShell,
        version: String,
    },
    Unavailable(ShellEditingUnavailableReason),
}

struct PendingProbe {
    id: u64,
    binding: UiSessionBinding,
    context: TerminalCommandContextVersion,
    deadline: tokio::time::Instant,
    reply: Option<oneshot::Sender<AppResult<ShellEditingSnapshot>>>,
}

impl EditingRuntime {
    /// 每次连接独立初始化能力，旧 nonce 和请求不能跨重连复用。
    ///
    /// @author kongweiguang
    pub(super) fn new(generation: u64, nonce: Option<String>) -> Self {
        let status = if nonce.is_some() {
            EditingStatus::Pending
        } else {
            EditingStatus::Unavailable(ShellEditingUnavailableReason::Unsupported)
        };
        let (settled, _) = watch::channel(0);
        Self {
            generation,
            nonce,
            status,
            revision: 0,
            next_probe: 0,
            pending: None,
            settled,
        }
    }

    /// 状态变化时唤醒等待槽位的请求；watch 避免检查和订阅之间丢失通知。
    ///
    /// @author kongweiguang
    fn notify_settled(&self) {
        self.settled
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// 永久禁用当前连接的探测并唤醒等待者；超时后无法安全匹配迟到帧。
    ///
    /// @author kongweiguang
    fn invalidate(&mut self, reason: ShellEditingUnavailableReason) {
        self.status = EditingStatus::Unavailable(reason);
        self.pending = None;
        self.notify_settled();
    }

    /// 无请求 ID 的响应只能消费当前认证探测，超时后继续隔离迟到帧。
    ///
    /// @author kongweiguang
    fn take_authenticated_probe(
        &mut self,
        nonce: &str,
        shell: EditingShell,
        version: &str,
    ) -> Option<PendingProbe> {
        if self.nonce.as_deref() != Some(nonce)
            || !matches!(&self.status, EditingStatus::Installed { shell: expected_shell, version: expected_version }
                if *expected_shell == shell && expected_version == version)
        {
            return None;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| tokio::time::Instant::now() >= pending.deadline)
        {
            self.invalidate(ShellEditingUnavailableReason::ProbeTimedOut);
            return None;
        }
        let pending = self.pending.take()?;
        self.notify_settled();
        Some(pending)
    }
}

/// 输入版本按现有命令生命周期生成，不把无正文探测记成一次用户编辑。
///
/// @author kongweiguang
fn context_version(shell: &ShellIntegrationState) -> TerminalCommandContextVersion {
    TerminalCommandContextVersion {
        session_generation: shell.session_generation,
        prompt_epoch: shell.prompt_epoch,
        input_generation: shell.input_generation,
    }
}

/// 编辑能力只面向当前 UI 的真实 prompt；旧投影失效不妨碍从 Shell 重新读取。
///
/// @author kongweiguang
fn eligible(
    state: &SessionState,
    shell: &ShellIntegrationState,
    binding: &UiSessionBinding,
) -> bool {
    matches!(&binding.owner, SessionOwner::Ui { .. })
        && state.info.owner == binding.owner
        && state.info.session_id == binding.session_id
        && state.info.lifecycle == SessionLifecycle::Running
        && shell.session_generation == binding.session_generation
        && shell.observed
        && shell.prompt_ready
        && !shell.prompt_submission_sent
        && !shell.external_input_since_prompt
        && shell.pending_input_write.is_none()
        && shell
            .active_command
            .as_ref()
            .is_none_or(|active| active.cancellation.is_cancelled())
        && !state.engine.input_modes().alternate_screen
}

/// 将协议提供的精确实现映射为 application 契约，不从通用方言猜测 Shell。
///
/// @author kongweiguang
fn application_shell(shell: EditingShell) -> ShellEditingShell {
    match shell {
        EditingShell::Bash => ShellEditingShell::Bash,
        EditingShell::Zsh => ShellEditingShell::Zsh,
        EditingShell::Fish => ShellEditingShell::Fish,
        EditingShell::PowerShell => ShellEditingShell::PowerShell,
    }
}

/// 查询单一连接中的握手与 prompt 状态；不发送任何终端输入。
///
/// @author kongweiguang
pub(super) async fn availability(
    session: &ManagedSession,
    binding: &UiSessionBinding,
) -> ShellEditingAvailability {
    let state = session.state.lock().await;
    let shell = session.shell_integration.lock().await;
    if !matches!(&binding.owner, SessionOwner::Ui { .. })
        || state.info.owner != binding.owner
        || state.info.session_id != binding.session_id
        || shell.session_generation != binding.session_generation
    {
        return ShellEditingAvailability::Unavailable(
            ShellEditingUnavailableReason::SessionChanged,
        );
    }
    let editing = session
        .shell_editing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &editing.status {
        EditingStatus::Pending => ShellEditingAvailability::Pending,
        EditingStatus::Unavailable(reason) => ShellEditingAvailability::Unavailable(*reason),
        EditingStatus::Installed { .. } if eligible(&state, &shell, binding) => {
            ShellEditingAvailability::Available
        }
        EditingStatus::Installed { .. } => {
            ShellEditingAvailability::Unavailable(ShellEditingUnavailableReason::PromptNotReady)
        }
    }
}

/// 过滤器超限说明本连接上的消息边界已不再可信，不能继续读取或追加。
///
/// @author kongweiguang
pub(super) fn invalidate(session: &ManagedSession, generation: u64) {
    let mut editing = session
        .shell_editing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if editing.generation == generation {
        editing.invalidate(ShellEditingUnavailableReason::SessionChanged);
    }
}

/// 在处理相邻可见输出之间接收帧，以原始 PTY 顺序复核 nonce、prompt 和输入版本。
///
/// @author kongweiguang
pub(super) async fn accept_frame(session: &ManagedSession, generation: u64, payload: &[u8]) {
    let Some(message) = parse_editing_message(payload) else {
        return;
    };
    let state = session.state.lock().await;
    let shell_state = session.shell_integration.lock().await;
    let mut editing = session
        .shell_editing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if editing.generation != generation
        || session.transport_generation.load(Ordering::Acquire) != generation
    {
        return;
    }
    let changed = match message {
        EditingMessage::Hello {
            nonce,
            shell,
            version,
            installed,
        } => {
            if editing.nonce.as_deref() != Some(nonce.as_str())
                || !matches!(editing.status, EditingStatus::Pending)
            {
                false
            } else {
                editing.status = if installed {
                    EditingStatus::Installed { shell, version }
                } else {
                    EditingStatus::Unavailable(ShellEditingUnavailableReason::KeyBindingConflict)
                };
                true
            }
        }
        EditingMessage::Snapshot {
            nonce,
            shell,
            version,
            cwd,
            input,
            cursor,
            prediction_visible,
        } => {
            let Some(pending) = editing.take_authenticated_probe(&nonce, shell, &version) else {
                return;
            };
            let Some(reply) = pending.reply else {
                return;
            };
            if !eligible(&state, &shell_state, &pending.binding)
                || pending.context != context_version(&shell_state)
            {
                return;
            }
            editing.revision = editing.revision.wrapping_add(1);
            let snapshot = ShellEditingSnapshot {
                binding: pending.binding,
                context: pending.context,
                revision: editing.revision,
                input,
                cursor,
                cwd,
                shell: application_shell(shell),
                shell_version: (!version.is_empty()).then_some(version),
                shell_prediction_visible: prediction_visible,
            };
            let _ = reply.send(Ok(snapshot));
            true
        }
        EditingMessage::InputTooLong {
            nonce,
            shell,
            version,
        } => {
            let Some(pending) = editing.take_authenticated_probe(&nonce, shell, &version) else {
                return;
            };
            let Some(reply) = pending.reply else {
                return;
            };
            if !eligible(&state, &shell_state, &pending.binding)
                || pending.context != context_version(&shell_state)
            {
                return;
            }
            let _ = reply.send(Err(AppError::invalid(SHELL_EDITING_INPUT_TOO_LONG_CONTEXT)));
            true
        }
    };
    drop(editing);
    if changed {
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
    }
}

/// 无请求 ID 的已发探测保持唯一占位，直到响应被丢弃或超时后关闭能力。
struct ProbeGuard {
    editing: Arc<StdMutex<EditingRuntime>>,
    generation: u64,
    id: u64,
    bytes_written: usize,
    deadline: tokio::time::Instant,
    runtime: tokio::runtime::Handle,
    armed: bool,
}

impl Drop for ProbeGuard {
    /// 未发出时释放槽位；完整发送后排空迟到帧，部分发送或超时则保守禁用。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut editing = self
            .editing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if editing.generation != self.generation
            || editing
                .pending
                .as_ref()
                .is_none_or(|pending| pending.id != self.id)
        {
            return;
        }
        if self.bytes_written == 0 {
            editing.pending = None;
            editing.notify_settled();
        } else if self.bytes_written != PROBE.len() || tokio::time::Instant::now() >= self.deadline
        {
            editing.invalidate(ShellEditingUnavailableReason::ProbeTimedOut);
        } else {
            if let Some(pending) = editing.pending.as_mut() {
                pending.reply = None;
            }
            let mut settled = editing.settled.subscribe();
            let editing = Arc::clone(&self.editing);
            let generation = self.generation;
            let id = self.id;
            let deadline = self.deadline;
            self.runtime.spawn(async move {
                tokio::select! {
                    _ = settled.changed() => {}
                    _ = tokio::time::sleep_until(deadline) => {
                        let mut editing = editing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if editing.generation == generation
                            && editing.pending.as_ref().is_some_and(|pending| pending.id == id && pending.reply.is_none())
                        {
                            editing.invalidate(ShellEditingUnavailableReason::ProbeTimedOut);
                        }
                    }
                }
            });
        }
    }
}

/// 等待上一个探测的唯一响应被消费；新输入可取消等待，不能并发发送无 ID 探测。
///
/// @author kongweiguang
async fn wait_for_probe_slot(
    session: &ManagedSession,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    loop {
        let mut settled = {
            let editing = session
                .shell_editing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !matches!(editing.status, EditingStatus::Installed { .. }) {
                return Err(AppError::unsupported("当前连接不支持 Shell 编辑快照"));
            }
            if editing.pending.is_none() {
                return Ok(());
            }
            editing.settled.subscribe()
        };
        tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("Shell 编辑快照请求已取消")),
            _ = settled.changed() => {}
        }
    }
}

/// 在原 writer FIFO 发送一帧只读按键，并在完整版本仍匹配时返回唯一响应。
///
/// @author kongweiguang
pub(super) async fn request(
    session: Arc<ManagedSession>,
    binding: UiSessionBinding,
    cancellation: CancellationToken,
) -> AppResult<ShellEditingSnapshot> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("Shell 编辑快照请求已取消"));
    }
    wait_for_probe_slot(&session, &cancellation).await?;
    let input_gate = session
        .input_gate
        .try_lock()
        .map_err(|_| AppError::conflict("终端输入正在处理"))?;
    let mut writer = session
        .writer
        .try_lock()
        .map_err(|_| AppError::conflict("终端 writer 正忙"))?;
    let state = session.state.lock().await;
    let shell = session.shell_integration.lock().await;
    if !eligible(&state, &shell, &binding) {
        return Err(AppError::conflict("Shell 编辑快照的终端状态已变化"));
    }
    let generation = session.transport_generation.load(Ordering::Acquire);
    let (receiver, mut guard) = {
        let mut editing = session
            .shell_editing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if editing.generation != generation
            || !matches!(editing.status, EditingStatus::Installed { .. })
        {
            return Err(AppError::unsupported("当前连接不支持 Shell 编辑快照"));
        }
        if editing.pending.is_some() {
            return Err(AppError::conflict("Shell 编辑快照请求正在处理"));
        }
        editing.next_probe = editing.next_probe.wrapping_add(1);
        let id = editing.next_probe;
        let (reply, receiver) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
        editing.pending = Some(PendingProbe {
            id,
            binding: binding.clone(),
            context: context_version(&shell),
            deadline,
            reply: Some(reply),
        });
        (
            receiver,
            ProbeGuard {
                editing: Arc::clone(&session.shell_editing),
                generation,
                id,
                bytes_written: 0,
                deadline,
                runtime: tokio::runtime::Handle::current(),
                armed: true,
            },
        )
    };
    drop(shell);
    drop(state);
    drop(input_gate);
    let write = async {
        while guard.bytes_written < PROBE.len() {
            let written = writer
                .write(&PROBE[guard.bytes_written..])
                .await
                .map_err(super::io_transport_error)?;
            if written == 0 {
                return Err(AppError::infrastructure("发送 Shell 编辑快照请求失败"));
            }
            guard.bytes_written += written;
        }
        writer.flush().await.map_err(super::io_transport_error)
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(AppError::cancelled("Shell 编辑快照请求已取消")),
        result = tokio::time::timeout(PROBE_TIMEOUT, write) => result.map_err(|_| AppError::timeout("发送 Shell 编辑快照请求"))??,
    }
    drop(writer);
    let response = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(AppError::cancelled("Shell 编辑快照请求已取消")),
        result = tokio::time::timeout_at(guard.deadline, receiver) => result
            .map_err(|_| AppError::timeout("等待 Shell 编辑快照"))?
            .map_err(|_| AppError::conflict("Shell 编辑快照连接已失效"))?,
    };
    guard.armed = false;
    let snapshot = response?;
    let state = session.state.lock().await;
    let mut shell = session.shell_integration.lock().await;
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("Shell 编辑快照请求已取消"));
    }
    if session.transport_generation.load(Ordering::Acquire) != generation
        || !eligible(&state, &shell, &snapshot.binding)
        || context_version(&shell) != snapshot.context
    {
        return Err(AppError::conflict("Shell 编辑快照返回时终端状态已变化"));
    }
    // 仅成功交付的同版本真实缓冲才能恢复上下文；不放宽外部输入或命令提交门禁。
    shell.ui_input_projection_valid = true;
    Ok(snapshot)
}

/// 确认探测所读内容完全一致；revision 必然变化，因此不参与原缓冲比较。
///
/// @author kongweiguang
fn same_buffer(before: &ShellEditingSnapshot, current: &ShellEditingSnapshot) -> bool {
    before.binding == current.binding
        && before.context == current.context
        && before.input == current.input
        && before.cursor == current.cursor
        && before.cwd == current.cwd
        && before.shell == current.shell
        && before.shell_version == current.shell_version
        && before.shell_prediction_visible == current.shell_prediction_visible
}

/// future 被丢弃时取消在途写入，并异步清理原 generation 的 pending，不重放正文。
struct PendingWriteGuard {
    session: Arc<ManagedSession>,
    generation: u64,
    cancellation: CancellationToken,
    recovery: Option<UnwrittenSuggestionRecovery>,
    runtime: tokio::runtime::Handle,
    armed: bool,
}

impl Drop for PendingWriteGuard {
    /// 只清理原写入票据；新的 prompt 与新 generation 不被旧任务回写。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancellation.cancel();
        let session = Arc::clone(&self.session);
        let generation = self.generation;
        let recovery = self.recovery.take();
        self.runtime.spawn(async move {
            finish_terminal_input_write_with_recovery(
                &session,
                generation,
                true,
                recovery.as_ref(),
            )
            .await;
        });
    }
}

/// 先重新读回并逐项比较，再按原 session FIFO 写一次后缀，最后验证 Shell 实际结果。
///
/// @author kongweiguang
pub(super) async fn append(
    session: Arc<ManagedSession>,
    snapshot: ShellEditingSnapshot,
    suffix: String,
    cancellation: CancellationToken,
) -> AppResult<ShellEditingSnapshot> {
    if suffix.is_empty()
        || suffix.len() > SHELL_EDITING_MAX_SUFFIX_BYTES
        || suffix.chars().any(char::is_control)
        || snapshot.input.chars().any(char::is_control)
        || snapshot.input.chars().count() + suffix.chars().count()
            > SHELL_EDITING_MAX_INPUT_CHARACTERS
        || !snapshot.cursor_at_end()
    {
        return Err(AppError::conflict("Shell 建议后缀不符合单行追加约束"));
    }
    let current = request(
        Arc::clone(&session),
        snapshot.binding.clone(),
        cancellation.clone(),
    )
    .await?;
    if !same_buffer(&snapshot, &current) {
        return Err(AppError::conflict("Shell 编辑缓冲已变化，未追加建议"));
    }
    let input_gate = session
        .input_gate
        .try_lock()
        .map_err(|_| AppError::conflict("终端输入正在处理"))?;
    let mut writer = session
        .writer
        .try_lock()
        .map_err(|_| AppError::conflict("终端 writer 正忙"))?;
    let state = session.state.lock().await;
    let mut shell = session.shell_integration.lock().await;
    if !eligible(&state, &shell, &snapshot.binding) || context_version(&shell) != snapshot.context {
        return Err(AppError::conflict("Shell 编辑缓冲已变化，未追加建议"));
    }
    let write_cancel = cancellation.child_token();
    let bytes_written = Arc::new(AtomicUsize::new(0));
    let session_generation = shell.session_generation;
    let prompt_epoch = shell.prompt_epoch;
    let recovery =
        shell
            .pending_command_submission
            .is_none()
            .then(|| UnwrittenSuggestionRecovery {
                binding: snapshot.binding.clone(),
                transport_generation: session.transport_generation.load(Ordering::Acquire),
                prompt_epoch,
                prompt_line_dirty: shell.prompt_line_dirty,
            });
    let generation = mark_shell_input(
        &mut shell,
        ShellInputOrigin::Ui {
            projection: UiInputProjection::Tracked,
            submitted: false,
        },
    );
    shell.pending_input_write = Some(PendingTerminalInputWrite {
        generation,
        cancellation: write_cancel.clone(),
        bytes_written: Arc::clone(&bytes_written),
        expected_bytes: suffix.len(),
        session_generation,
        prompt_epoch,
        block_id: None,
        command_finished: false,
        prompt_started_after_command: false,
        prompt_ended_after_command: false,
    });
    let mut pending = PendingWriteGuard {
        session: Arc::clone(&session),
        generation,
        cancellation: write_cancel.clone(),
        recovery,
        runtime: tokio::runtime::Handle::current(),
        armed: true,
    };
    drop(shell);
    drop(state);
    drop(input_gate);
    let result = write_input_bytes_locked(
        &mut writer,
        suffix.as_bytes(),
        &write_cancel,
        &bytes_written,
    )
    .await;
    drop(writer);
    // 仅取消导致的零字节 Conflict 可恢复；I/O 错误和成功后的未确认结果不取得该权限。
    if !matches!(&result, Err(AppError::Conflict { .. })) || !write_cancel.is_cancelled() {
        pending.recovery = None;
    }
    finish_terminal_input_write_with_recovery(
        &session,
        generation,
        result.is_err(),
        pending.recovery.as_ref(),
    )
    .await;
    pending.armed = false;
    result?;
    let after = request(Arc::clone(&session), snapshot.binding.clone(), cancellation)
        .await
        .map_err(|_| AppError::outcome_unknown("建议已写入，但 Shell 编辑状态无法回读"))?;
    let mut expected_input = snapshot.input.clone();
    expected_input.push_str(&suffix);
    if after.context.session_generation != snapshot.context.session_generation
        || after.context.prompt_epoch != snapshot.context.prompt_epoch
        || after.context.input_generation != snapshot.context.input_generation.wrapping_add(1)
        || after.input != expected_input
        || !after.cursor_at_end()
        || after.cwd != snapshot.cwd
        || after.shell != snapshot.shell
        || after.shell_version != snapshot.shell_version
    {
        return Err(AppError::outcome_unknown(
            "建议已写入，但 Shell 回读与预期不一致",
        ));
    }
    Ok(after)
}
