//! ZMODEM 独占同代 SSH 字节流，桌面只持有投影和确认入口。
//!
//! @author kongweiguang

use std::{collections::VecDeque, path::PathBuf, sync::atomic::Ordering, time::Duration};

use kerminal_application::{
    AppError, AppResult,
    sessions::{
        SessionChangeKind, UiSessionBinding, ZMODEM_INPUT_BUSY, ZmodemDecision, ZmodemDirection,
        ZmodemPhase, ZmodemStatus,
    },
};
use kerminal_domain::sessions::{SessionLifecycle, SessionOwner, SessionTarget};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::{
    ManagedSession,
    conpty_output_coalescer::ReaderEvent,
    zmodem_protocol::{
        CANCEL_SEQUENCE, DownloadFile, Handshake, ProtocolAction, TransferError, UploadFile,
        ZmodemProtocol,
    },
};

const SELECTION_WAIT: Duration = Duration::from_secs(120);
const WIRE_WAIT: Duration = Duration::from_secs(15);
const HANDSHAKE_RETRY: Duration = Duration::from_secs(5);
const RECOVERY_WAIT: Duration = Duration::from_secs(3);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_RECOVERY_BYTES: usize = 16 * 1024;
const MAX_UPLOAD_FILES: usize = 1024;
const PEER_CANCEL: &[u8] = &[0x18; 5];
const PROMPT_START: &[u8] = b"\x1b]133;A";
const COMMAND_END: &[u8] = b"\x1b]133;D";
const ZERO_ZFIN: &[u8] = b"**\x18B0800000000022d";

#[derive(Default)]
pub(super) struct Slot {
    sequence: u64,
    transfer: Option<TransferSlot>,
}

struct TransferSlot {
    generation: u64,
    status: ZmodemStatus,
    selection: Option<oneshot::Sender<Selection>>,
    cancel: CancellationToken,
}

enum Selection {
    Upload(Vec<PathBuf>),
    Download(PathBuf),
}

struct TransferRun<'a> {
    session: &'a ManagedSession,
    generation: u64,
    id: u64,
    lifetime: &'a CancellationToken,
    cancel: CancellationToken,
    status: ZmodemStatus,
    last_progress: Instant,
}

enum Stop {
    Cancelled,
    PeerAborted,
    Failed(&'static str),
    FinishUnverified,
    Transport(ReaderEvent),
}

struct TransferFiles {
    uploads: VecDeque<PathBuf>,
    upload: Option<UploadFile>,
    destination: Option<PathBuf>,
    download: Option<DownloadFile>,
}

type WireReader<'a> = &'a mut (dyn AsyncRead + Unpin + Send);

/// 状态订阅只投影准确绑定的桌面分屏，普通后台 SSH 始终走原路径。
pub(super) async fn enabled(session: &ManagedSession) -> bool {
    let state = session.state.lock().await;
    matches!(state.info.target, SessionTarget::Ssh(_))
        && matches!(state.info.owner, SessionOwner::Ui { .. })
}

/// 写入前检查相同代次的传输状态，重连后的旧握手不影响新 writer。
pub(super) async fn ensure_input(session: &ManagedSession) -> AppResult<()> {
    let generation = session.transport_generation.load(Ordering::Acquire);
    if session
        .zmodem
        .lock()
        .await
        .transfer
        .as_ref()
        .is_some_and(|transfer| {
            transfer.generation == generation && blocks_input(transfer.status.phase)
        })
    {
        return Err(AppError::conflict(ZMODEM_INPUT_BUSY));
    }
    Ok(())
}

/// 未确认的收尾继续阻止普通字节混入，分屏关闭和重连由原会话生命周期接管。
fn blocks_input(phase: ZmodemPhase) -> bool {
    matches!(
        phase,
        ZmodemPhase::WaitingForSelection
            | ZmodemPhase::Transferring
            | ZmodemPhase::Cancelling
            | ZmodemPhase::NeedsReconnect
    )
}

/// 会话归属和 Shell 代次一起核对，避免刚关闭或重连的选择器结果命中新 reader。
async fn binding_generation(session: &ManagedSession, binding: &UiSessionBinding) -> Option<u64> {
    let state = session.state.lock().await;
    let shell = session.shell_integration.lock().await;
    (state.info.owner == binding.owner
        && matches!(state.info.owner, SessionOwner::Ui { .. })
        && matches!(state.info.target, SessionTarget::Ssh(_))
        && matches!(
            state.info.lifecycle,
            SessionLifecycle::Running | SessionLifecycle::Exited | SessionLifecycle::Failed
        )
        && shell.session_generation == binding.session_generation)
        .then(|| session.transport_generation.load(Ordering::Acquire))
}

/// 返回 watch 对应的快照，旧代次留存的状态不穿透到新分屏。
pub(super) async fn status(
    session: &ManagedSession,
    binding: &UiSessionBinding,
) -> Option<ZmodemStatus> {
    let generation = binding_generation(session, binding).await?;
    session
        .zmodem
        .lock()
        .await
        .transfer
        .as_ref()
        .filter(|transfer| transfer.generation == generation)
        .map(|transfer| transfer.status.clone())
}

/// 选择只能提交一次；取消通过独立令牌抢占协议等待，不与选择排队。
pub(super) async fn respond(
    session: &ManagedSession,
    binding: &UiSessionBinding,
    id: u64,
    decision: ZmodemDecision,
) -> AppResult<()> {
    let stale = || AppError::conflict("ZMODEM 传输已结束或终端已重连");
    let generation = binding_generation(session, binding)
        .await
        .ok_or_else(stale)?;
    let mut slot = session.zmodem.lock().await;
    let transfer = slot
        .transfer
        .as_mut()
        .filter(|transfer| transfer.generation == generation && transfer.status.id == id)
        .ok_or_else(stale)?;
    let selection = match decision {
        ZmodemDecision::Cancel
            if blocks_input(transfer.status.phase)
                && transfer.status.phase != ZmodemPhase::NeedsReconnect =>
        {
            transfer.cancel.cancel();
            return Ok(());
        }
        ZmodemDecision::Dismiss if !blocks_input(transfer.status.phase) => {
            slot.transfer = None;
            notify(session);
            return Ok(());
        }
        ZmodemDecision::Upload(paths)
            if transfer.status.direction == ZmodemDirection::Upload
                && !paths.is_empty()
                && paths.len() <= MAX_UPLOAD_FILES =>
        {
            Selection::Upload(paths)
        }
        ZmodemDecision::Download(path)
            if transfer.status.direction == ZmodemDirection::Download =>
        {
            Selection::Download(path)
        }
        _ => return Err(AppError::invalid("当前传输无法应用此文件操作")),
    };
    if transfer.cancel.is_cancelled() {
        return Err(stale());
    }
    transfer
        .selection
        .take()
        .ok_or_else(stale)?
        .send(selection)
        .map_err(|_| stale())
}

/// 复用现有合并通知；进度更新无需额外订阅任务，也不会改变终端画布 revision。
fn notify(session: &ManagedSession) {
    session
        .changes
        .send_modify(|change| change.kind = SessionChangeKind::Output);
}

/// 先预约 reader 和写入准入，避免旧写入遇到背压时隐藏操作。真正写协议时再有界等待原 writer。
async fn begin<'a>(
    session: &'a ManagedSession,
    generation: u64,
    direction: ZmodemDirection,
    lifetime: &'a CancellationToken,
) -> Option<(TransferRun<'a>, oneshot::Receiver<Selection>)> {
    let mut slot = session.zmodem.lock().await;
    if session.transport_generation.load(Ordering::Acquire) != generation || lifetime.is_cancelled()
    {
        return None;
    }
    slot.sequence = slot.sequence.wrapping_add(1);
    let id = slot.sequence;
    let status = ZmodemStatus {
        id,
        direction,
        phase: ZmodemPhase::WaitingForSelection,
        file_name: None,
        bytes: 0,
        total: None,
        completed: 0,
        error: None,
        save_directory: None,
    };
    let cancel = lifetime.child_token();
    let (selection, receiver) = oneshot::channel();
    slot.transfer = Some(TransferSlot {
        generation,
        status: status.clone(),
        selection: Some(selection),
        cancel: cancel.clone(),
    });
    notify(session);
    Some((
        TransferRun {
            session,
            generation,
            id,
            lifetime,
            cancel,
            status,
            last_progress: Instant::now(),
        },
        receiver,
    ))
}

impl TransferRun<'_> {
    /// 只更新当前握手，避免迟到的文件任务投影覆盖重连后的新操作。
    async fn publish(&mut self, force: bool) {
        if !force && self.last_progress.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last_progress = Instant::now();
        let mut slot = self.session.zmodem.lock().await;
        if let Some(transfer) = slot.transfer.as_mut().filter(|transfer| {
            transfer.generation == self.generation && transfer.status.id == self.id
        }) {
            transfer.status = self.status.clone();
            if self.status.phase != ZmodemPhase::WaitingForSelection {
                transfer.selection = None;
            }
            notify(self.session);
        }
    }

    /// 上传和下载只在用户选定路径后开始，等待期间持续读远端以捕获断线或主动取消。
    async fn wait_for_selection(
        &self,
        mut selection: oneshot::Receiver<Selection>,
        reader: WireReader<'_>,
        pending: &mut Vec<u8>,
    ) -> Result<Selection, Stop> {
        let deadline = Instant::now() + SELECTION_WAIT;
        let mut buffer = [0; 8192];
        loop {
            if prompt_start(pending).is_some() || peer_cancelled(pending) {
                return Err(Stop::PeerAborted);
            }
            retain_recovery_tail(pending);
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(Stop::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Err(Stop::Failed("等待选择文件超时")),
                read = read_wire(reader, &mut buffer) => {
                    pending.extend_from_slice(&buffer[..read?]);
                }
                choice = &mut selection => return choice.map_err(|_| Stop::Cancelled),
            }
        }
    }

    /// 协议和普通输入共用原 writer。可取消和有界写入避免 SSH 背压卡住分屏关闭。
    async fn write_wire(
        &self,
        bytes: &[u8],
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Stop::Cancelled),
            result = timeout_at(deadline, async {
                let mut writer = self.session.writer.lock().await;
                writer.write_all(bytes).await?;
                writer.flush().await
            }) => match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(Stop::Transport(ReaderEvent::Failed(error))),
                Err(_) => Err(Stop::Failed("等待 SSH 写入超时")),
            }
        }
    }

    /// 提交终态前按既有 Shell 集成的提示符开始标记确认恢复，拒绝把残留协议数据当作普通输出。
    async fn recover(
        &self,
        reader: WireReader<'_>,
        pending: &mut Vec<u8>,
        deadline: Option<Instant>,
        mut require_exit_pair: bool,
    ) -> Result<bool, Stop> {
        let mut buffer = [0; 8192];
        loop {
            require_exit_pair &= !peer_cancelled(pending);
            if let Some(start) = exit_prompt(pending, require_exit_pair) {
                pending.drain(..start);
                return Ok(true);
            }
            retain_recovery_tail(pending);
            tokio::select! {
                biased;
                _ = self.lifetime.cancelled() => return Err(Stop::Cancelled),
                _ = async {
                    if let Some(deadline) = deadline {
                        tokio::time::sleep_until(deadline).await;
                    }
                }, if deadline.is_some() => return Ok(false),
                read = read_wire(reader, &mut buffer) => pending.extend_from_slice(&buffer[..read?]),
            }
        }
    }
}

/// 保留跨块分割的取消和提示符证据，连续垃圾流不会使恢复缓冲无限增长。
fn retain_recovery_tail(pending: &mut Vec<u8>) {
    if pending.len() >= MAX_RECOVERY_BYTES {
        pending.drain(..pending.len() - 1024);
    }
}

/// 连续 CAN 是协议取消，单个 ZDLE 与普通转义不能触发此分支。
fn peer_cancelled(bytes: &[u8]) -> bool {
    bytes
        .windows(PEER_CANCEL.len())
        .any(|window| window == PEER_CANCEL)
}

/// 只认可完整 OSC 133 A 起点，保留其后的真实终端控制符和 prompt。
fn prompt_start(bytes: &[u8]) -> Option<usize> {
    prompt_positions(bytes).next()
}

/// 逐个验证完整标记，允许跳过非退出配对的 A 后继续寻找真正的提示符。
fn prompt_positions(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    bytes
        .windows(PROMPT_START.len() + 1)
        .enumerate()
        .filter_map(|(start, window)| {
            if !window.starts_with(PROMPT_START) {
                return None;
            }
            (window[PROMPT_START.len()] == 7
                || bytes
                    .get(start + PROMPT_START.len()..)
                    .is_some_and(|rest| rest.starts_with(b"\x1b\\")))
            .then_some(start)
        })
}

/// Shell hook 可在 prompt A 前报告命令退出，保留这个状态及随后目录标记。
fn command_status_start(bytes: &[u8], prompt: usize) -> usize {
    bytes[..prompt]
        .windows(COMMAND_END.len())
        .rposition(|window| window == COMMAND_END)
        .filter(|&position| {
            let body = &bytes[position + COMMAND_END.len()..prompt];
            let delimiter = body.iter().position(|&byte| byte == 7 || byte == 0x1b);
            delimiter.is_some_and(|delimiter| {
                let status = &body[..delimiter];
                let terminated = body[delimiter] == 7 || body[delimiter..].starts_with(b"\x1b\\");
                terminated
                    && (status.is_empty()
                        || status.strip_prefix(b";").is_some_and(|code| {
                            (1..=3).contains(&code.len()) && code.iter().all(u8::is_ascii_digit)
                        }))
            })
        })
        .unwrap_or(prompt)
}

/// 在下载字节仍可能在途时，单个终端标记不能证明 Shell 已经重新接管输入。
fn exit_prompt(bytes: &[u8], require_exit_pair: bool) -> Option<usize> {
    prompt_positions(bytes).find_map(|prompt| {
        let start = command_status_start(bytes, prompt);
        (!require_exit_pair || (start < prompt && prompt - start < 1024)).then_some(start)
    })
}

/// 只把确定属于协议的前缀交给库，跨读块保留退出标记。下载要求 D/A 配对，文件中的单个 A 仍交给协议解码。
fn active_wire_prefix(bytes: &[u8], direction: ZmodemDirection) -> Result<usize, Stop> {
    let cancel = bytes
        .windows(PEER_CANCEL.len())
        .position(|window| window == PEER_CANCEL);
    let prompt = exit_prompt(bytes, direction == ZmodemDirection::Download);
    if let Some(exit) = cancel.into_iter().chain(prompt).min() {
        return if exit == 0 {
            Err(Stop::PeerAborted)
        } else {
            Ok(exit)
        };
    }

    let preserve_from = bytes.len().saturating_sub(1024);
    let end = &bytes[preserve_from..];
    let command = end
        .windows(COMMAND_END.len())
        .rposition(|window| window == COMMAND_END)
        .map(|offset| preserve_from + offset);
    let suffix = (1..=bytes.len().min(PROMPT_START.len() + 1))
        .rev()
        .find(|&length| {
            let suffix = &bytes[bytes.len() - length..];
            PEER_CANCEL.starts_with(suffix)
                || COMMAND_END.starts_with(suffix)
                || PROMPT_START.starts_with(suffix)
                || suffix == b"\x1b]133;A\x1b"
        })
        .map(|length| bytes.len() - length);
    Ok(command
        .into_iter()
        .chain(suffix)
        .min()
        .unwrap_or(bytes.len()))
}

/// 保持原有 reader 的 EOF/错误识别，协议层不抢占 SSH 重连裁决。
async fn read_wire(reader: WireReader<'_>, buffer: &mut [u8]) -> Result<usize, Stop> {
    match reader.read(buffer).await {
        Ok(0) => Err(Stop::Transport(ReaderEvent::End)),
        Ok(count) => Ok(count),
        Err(error) => Err(Stop::Transport(ReaderEvent::Failed(error))),
    }
}

/// 单独驱动握手和协议，任何终态都把 reader 和剩余普通终端字节还给原会话。
pub(super) async fn run(
    session: &ManagedSession,
    generation: u64,
    lifetime: &CancellationToken,
    handshake: Handshake,
    mut pending: Vec<u8>,
    reader: WireReader<'_>,
) -> (Vec<u8>, Option<ReaderEvent>) {
    let Some((mut transfer, selection)) =
        begin(session, generation, handshake.direction, lifetime).await
    else {
        return (Vec::new(), Some(ReaderEvent::Cancelled));
    };
    let result = match transfer
        .wait_for_selection(selection, reader, &mut pending)
        .await
    {
        Ok(selection) => {
            transfer
                .exchange(handshake, selection, reader, &mut pending)
                .await
        }
        Err(stop) => Err(stop),
    };
    if lifetime.is_cancelled() {
        return (Vec::new(), Some(ReaderEvent::Cancelled));
    }
    let end = match result {
        Ok(()) => {
            transfer.status.phase = if transfer.status.error.is_some() {
                ZmodemPhase::Failed
            } else {
                ZmodemPhase::Finished
            };
            None
        }
        Err(Stop::Transport(end)) => {
            transfer.status.phase = ZmodemPhase::Failed;
            transfer.status.error = Some("SSH 连接在传输期间中断");
            pending.clear();
            Some(end)
        }
        Err(stop) => {
            let require_exit_pair = transfer.status.direction == ZmodemDirection::Download
                && transfer.status.phase == ZmodemPhase::Transferring
                && !matches!(stop, Stop::PeerAborted)
                && !peer_cancelled(&pending);
            let (terminal_phase, error, send_cancel) = match stop {
                Stop::Cancelled => (ZmodemPhase::Cancelled, None, true),
                Stop::PeerAborted => (ZmodemPhase::Cancelled, Some("远端取消了传输"), false),
                Stop::Failed(message) => (ZmodemPhase::Failed, Some(message), true),
                Stop::FinishUnverified => (
                    ZmodemPhase::Failed,
                    Some("无法确认远端已结束 ZMODEM 传输"),
                    false,
                ),
                Stop::Transport(end) => return (Vec::new(), Some(end)),
            };
            transfer.status.phase = ZmodemPhase::Cancelling;
            transfer.status.error = error;
            transfer.publish(true).await;
            let deadline = Instant::now() + RECOVERY_WAIT;
            let recovered = if send_cancel && exit_prompt(&pending, require_exit_pair).is_none() {
                match transfer
                    .write_wire(&CANCEL_SEQUENCE, deadline, lifetime)
                    .await
                {
                    Ok(()) => {
                        transfer
                            .recover(reader, &mut pending, Some(deadline), require_exit_pair)
                            .await
                    }
                    Err(stop) => Err(stop),
                }
            } else {
                transfer
                    .recover(reader, &mut pending, Some(deadline), require_exit_pair)
                    .await
            };
            let recovered = match recovered {
                Ok(false) | Err(Stop::Failed(_)) => {
                    transfer.status.phase = ZmodemPhase::NeedsReconnect;
                    transfer.publish(true).await;
                    transfer
                        .recover(reader, &mut pending, None, require_exit_pair)
                        .await
                }
                other => other,
            };
            match recovered {
                Ok(true) => {
                    transfer.status.phase = terminal_phase;
                    None
                }
                Err(Stop::Transport(end)) => {
                    transfer.status.phase = ZmodemPhase::Failed;
                    transfer.status.error = Some("SSH 连接在传输期间中断");
                    pending.clear();
                    Some(end)
                }
                _ => return (Vec::new(), Some(ReaderEvent::Cancelled)),
            }
        }
    };
    transfer.publish(true).await;
    (pending, end)
}

impl TransferRun<'_> {
    /// 串行处理协议动作及文件边界，超出声明大小的块在写盘或确认前拒绝。
    async fn exchange(
        &mut self,
        handshake: Handshake,
        selection: Selection,
        reader: WireReader<'_>,
        pending: &mut Vec<u8>,
    ) -> Result<(), Stop> {
        let mut protocol = ZmodemProtocol::new(handshake.direction).map_err(map_error)?;
        let mut files = TransferFiles {
            uploads: VecDeque::new(),
            upload: None,
            destination: None,
            download: None,
        };
        match selection {
            Selection::Upload(paths) => files.uploads = paths.into(),
            Selection::Download(path) => {
                self.status.save_directory = Some(path.clone());
                files.destination = Some(path);
            }
        }
        let mut input = handshake.frame;
        input.append(pending);
        *pending = input;
        self.status.phase = ZmodemPhase::Transferring;
        self.publish(true).await;
        if handshake.direction == ZmodemDirection::Upload {
            self.next_upload(&mut protocol, &mut files).await?;
        }
        let mut buffer = [0; 8192];
        let mut complete = false;
        let mut skipped = false;
        let mut deadline = Instant::now() + WIRE_WAIT;
        let mut retry = Instant::now() + HANDSHAKE_RETRY;
        loop {
            if self.cancel.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Stop::Failed("ZMODEM 传输停滞"));
            }
            match protocol.poll() {
                ProtocolAction::WriteWire(bytes) => {
                    self.write_wire(&bytes, deadline, &self.cancel).await?;
                    protocol.wire_written(bytes.len());
                }
                ProtocolAction::ReadFile { offset, max_len } => {
                    let mut file = files
                        .upload
                        .take()
                        .ok_or(Stop::Failed("上传文件状态已失效"))?;
                    let (file, bytes) = file_operation(move || {
                        let bytes = file.read(offset, max_len)?;
                        Ok((file, bytes))
                    })
                    .await?;
                    protocol.submit_file(&bytes).map_err(map_error)?;
                    files.upload = Some(file);
                    let sent = u64::from(offset) + bytes.len() as u64;
                    if sent > self.status.bytes {
                        deadline = Instant::now() + WIRE_WAIT;
                    }
                    self.status.bytes = sent;
                    self.publish(false).await;
                }
                ProtocolAction::WriteFile(bytes) => {
                    let mut file = files
                        .download
                        .take()
                        .ok_or(Stop::Failed("下载文件状态已失效"))?;
                    let length = bytes.len();
                    let end = self.status.bytes + length as u64;
                    if end > u64::from(u32::MAX) {
                        return Err(Stop::Failed("文件超过 ZMODEM 文件大小限制"));
                    }
                    if self.status.total.is_some_and(|total| end > total) {
                        return Err(Stop::Failed("下载文件大小超过远端声明"));
                    }
                    files.download = Some(
                        file_operation(move || {
                            file.write(&bytes)?;
                            Ok(file)
                        })
                        .await?,
                    );
                    protocol.file_written(length).map_err(map_error)?;
                    self.status.bytes = end;
                    self.publish(false).await;
                    deadline = Instant::now() + WIRE_WAIT;
                }
                ProtocolAction::FileStarted { name, size } if files.destination.is_some() => {
                    let destination = files
                        .destination
                        .clone()
                        .ok_or(Stop::Failed("下载位置已失效"))?;
                    self.status.file_name = Some(String::from_utf8_lossy(&name).into_owned());
                    self.status.bytes = 0;
                    self.status.total = size.map(u64::from);
                    files.download = Some(
                        file_operation(move || DownloadFile::create(&destination, &name)).await?,
                    );
                    protocol.accept_file().map_err(map_error)?;
                    self.publish(true).await;
                    deadline = Instant::now() + WIRE_WAIT;
                }
                ProtocolAction::FileStarted { .. } => {}
                ProtocolAction::FileCompleted => {
                    if let Some(file) = files.download.take() {
                        if self
                            .status
                            .total
                            .is_some_and(|total| total != self.status.bytes)
                        {
                            return Err(Stop::Failed("下载文件大小与远端声明不一致"));
                        }
                        let cancellation = self.cancel.clone();
                        file_operation(move || file.commit(&cancellation)).await?;
                    }
                    self.status.completed = self.status.completed.saturating_add(1);
                    if files.upload.take().is_some() {
                        self.next_upload(&mut protocol, &mut files).await?;
                    }
                    self.publish(true).await;
                    deadline = Instant::now() + WIRE_WAIT;
                }
                ProtocolAction::FileSkipped => {
                    skipped = true;
                    if files.upload.take().is_none() {
                        return Err(Stop::Failed("上传文件状态已失效"));
                    }
                    self.next_upload(&mut protocol, &mut files).await?;
                    self.publish(true).await;
                    deadline = Instant::now() + WIRE_WAIT;
                }
                ProtocolAction::SessionCompleted => {
                    if files.download.is_some()
                        || files.upload.is_some()
                        || !files.uploads.is_empty()
                        || (self.status.completed == 0 && !skipped)
                    {
                        return Err(Stop::Failed("远端在文件完成前结束 ZMODEM 传输"));
                    }
                    complete = true;
                }
                ProtocolAction::Aborted => return Err(Stop::PeerAborted),
                ProtocolAction::Idle if complete => {
                    if handshake.direction == ZmodemDirection::Download {
                        self.consume_download_finish(reader, pending, deadline)
                            .await?;
                    }
                    if skipped {
                        self.status.error = Some("远端跳过了部分或全部上传文件");
                    }
                    return Ok(());
                }
                ProtocolAction::Idle => {
                    let prefix = active_wire_prefix(pending, handshake.direction)?;
                    if prefix > 0 {
                        let count = protocol.feed_wire(&pending[..prefix]).map_err(map_error)?;
                        if count > 0 {
                            pending.drain(..count);
                            continue;
                        }
                    }
                    let read = tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Err(Stop::Cancelled),
                        _ = tokio::time::sleep_until(deadline) => return Err(Stop::Failed("ZMODEM 传输停滞")),
                        _ = tokio::time::sleep_until(retry) => {
                            protocol.timeout().map_err(map_error)?;
                            retry = Instant::now() + HANDSHAKE_RETRY;
                            continue;
                        }
                        read = read_wire(reader, &mut buffer) => read?,
                    };
                    if pending.len() >= MAX_RECOVERY_BYTES {
                        return Err(Stop::Failed("ZMODEM 协议无法消费待接收数据"));
                    }
                    pending.extend_from_slice(&buffer[..read]);
                }
            }
        }
    }

    /// 文件只打开一次并保持原句柄，文件系统操作不阻塞 SSH runtime 或 GPUI 线程。
    async fn next_upload(
        &mut self,
        protocol: &mut ZmodemProtocol,
        files: &mut TransferFiles,
    ) -> Result<(), Stop> {
        if let Some(path) = files.uploads.pop_front() {
            let file = file_operation(move || UploadFile::open(&path)).await?;
            self.status.file_name = Some(String::from_utf8_lossy(&file.name).into_owned());
            self.status.bytes = 0;
            self.status.total = Some(u64::from(file.size));
            protocol
                .start_file(&file.name, file.size)
                .map_err(map_error)?;
            files.upload = Some(file);
        } else {
            protocol.finish().map_err(map_error)?;
        }
        Ok(())
    }

    /// ZFIN 已双向确认后，以 OO 或 Shell 的成功退出和新提示符结束下载，兼容 PTY 收尾丢失 OO 的 lrzsz。
    async fn consume_download_finish(
        &mut self,
        reader: WireReader<'_>,
        pending: &mut Vec<u8>,
        deadline: Instant,
    ) -> Result<(), Stop> {
        let mut buffer = [0; 8192];
        loop {
            let prefix = pending
                .iter()
                .position(|byte| !matches!(byte, b'\r' | b'\n' | 0x8d | 0x8a | 0x11 | 0x91))
                .unwrap_or(pending.len());
            pending.drain(..prefix);
            if pending.starts_with(b"OO") {
                pending.drain(..2);
                return Ok(());
            }
            if pending.starts_with(ZERO_ZFIN) {
                pending.drain(..ZERO_ZFIN.len());
                continue;
            }
            if let Some(start) = exit_prompt(pending, true) {
                pending.drain(..start);
                if !pending.starts_with(b"\x1b]133;D;0\x07")
                    && !pending.starts_with(b"\x1b]133;D;0\x1b\\")
                {
                    self.status.error = Some("远端未确认下载成功");
                }
                return Ok(());
            }
            retain_recovery_tail(pending);
            let read = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(Stop::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Err(Stop::FinishUnverified),
                read = read_wire(reader, &mut buffer) => read?,
            };
            pending.extend_from_slice(&buffer[..read]);
        }
    }
}

/// 等待文件任务收口，取消不会把仍可提交的阻塞文件操作遗留在后台。
async fn file_operation<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, TransferError> + Send + 'static,
) -> Result<T, Stop> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| Stop::Failed("ZMODEM 文件任务异常退出"))?
        .map_err(map_error)
}

/// 面向视图的错误使用固定文案，远端元数据和本地路径不进入诊断投影。
fn map_error(error: TransferError) -> Stop {
    match error {
        TransferError::Cancelled => Stop::Cancelled,
        TransferError::FileTooLarge => Stop::Failed("文件超过 ZMODEM 文件大小限制"),
        TransferError::InvalidName => Stop::Failed("文件名不安全或无法读取"),
        TransferError::Io(_) => Stop::Failed("无法读取或保存 ZMODEM 文件"),
        TransferError::CommitUnknown(_) => Stop::Failed("无法确认下载文件是否已保存"),
        _ => Stop::Failed("ZMODEM 协议错误"),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/zmodem_runtime.rs"]
mod tests;
