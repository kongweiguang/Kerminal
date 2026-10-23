/**
 * SFTP 后台传输队列的纯展示模型。
 *
 * @author kongweiguang
 */

import type { SftpTransferEndpoint, SftpTransferSummary } from "../../lib/sftpApi";
import { fileNameFromPath, formatFileSize } from "./sftpFileUtils";

const SFTP_TRANSFER_WAITING_GRACE_MS = 5_000;

/**
 * 按用户关注优先级排序后台传输任务。
 */
export function sortTransfers(transfers: SftpTransferSummary[]) {
  return [...transfers].sort((left, right) => {
    const statusRank =
      transferStatusRank(left.status) - transferStatusRank(right.status);
    if (statusRank !== 0) {
      return statusRank;
    }
    return right.createdAt - left.createdAt;
  });
}

/**
 * 按传输 ID 替换或追加单个任务快照。
 */
export function upsertTransfer(
  transfers: SftpTransferSummary[],
  summary: SftpTransferSummary,
) {
  const currentTransfer = transfers.find(
    (transfer) => transfer.id === summary.id,
  );
  if (currentTransfer && shouldKeepCurrentSnapshot(currentTransfer, summary)) {
    return [...transfers];
  }
  const nextTransfers = transfers.filter(
    (transfer) => transfer.id !== summary.id,
  );
  nextTransfers.push(summary);
  return nextTransfers;
}

/**
 * 合并后端或事件返回的单条任务快照，并保持队列统一排序。
 */
export function mergeTransferSnapshot(
  transfers: SftpTransferSummary[],
  summary: SftpTransferSummary,
) {
  return sortTransfers(upsertTransfer(transfers, summary));
}

/**
 * 替换轮询快照时保留本地已确认的终态和取消乐观状态，避免迟到列表让按钮闪回可用。
 * 完整列表仍以服务端任务集合为准，因此只保护同 ID 的快照，不把已清理历史重新带回队列。
 *
 * @author kongweiguang
 */
export function replaceTransferQueue(
  transfers: SftpTransferSummary[],
  previousTransfers: SftpTransferSummary[] = [],
) {
  const previousById = new Map(
    previousTransfers.map((transfer) => [transfer.id, transfer]),
  );
  return sortTransfers(
    transfers.map((summary) => {
      const currentTransfer = previousById.get(summary.id);
      return currentTransfer && shouldKeepCurrentSnapshot(currentTransfer, summary)
        ? currentTransfer
        : summary;
    }),
  );
}

/**
 * 过滤迟到的事件或列表快照；终态一旦落地不可被非终态覆盖，取消请求也不能闪回。
 * 时间戳只作为活动快照的兜底排序依据，避免相同秒级时间戳阻挡正常进度事件。
 *
 * @author kongweiguang
 */
function shouldKeepCurrentSnapshot(
  currentTransfer: SftpTransferSummary,
  incomingTransfer: SftpTransferSummary,
) {
  if (isFinishedTransfer(currentTransfer)) {
    return !isFinishedTransfer(incomingTransfer) ||
      incomingTransfer.status !== currentTransfer.status ||
      Boolean(currentTransfer.successorId && !incomingTransfer.successorId) ||
      currentTransfer.updatedAt > incomingTransfer.updatedAt;
  }
  if (isFinishedTransfer(incomingTransfer)) {
    return false;
  }
  if (
    currentTransfer.cancelRequested &&
    !isFinishedTransfer(incomingTransfer)
  ) {
    return true;
  }
  return currentTransfer.updatedAt > incomingTransfer.updatedAt;
}

function transferStatusRank(status: SftpTransferSummary["status"]) {
  if (status === "running") {
    return 0;
  }
  if (status === "queued") {
    return 1;
  }
  if (status === "failed") {
    return 2;
  }
  if (status === "canceled") {
    return 3;
  }
  return 4;
}

/**
 * 判断传输任务是否已经进入终态。
 */
export function isFinishedTransfer(transfer: SftpTransferSummary) {
  return (
    transfer.status === "succeeded" ||
    transfer.status === "failed" ||
    transfer.status === "canceled"
  );
}

/**
 * 统计仍会占用队列或用户注意力的传输任务。
 */
export function activeTransferCount(transfers: SftpTransferSummary[]) {
  return transfers.filter(
    (transfer) => transfer.status === "running" || transfer.status === "queued",
  ).length;
}

/**
 * 容器直传没有后台取消通道；其它任务只在活动且未请求取消时提供操作。
 */
export function canCancelTransfer(transfer: SftpTransferSummary) {
  return (
    (transfer.status === "queued" || transfer.status === "running") &&
    !transfer.cancelRequested &&
    transfer.cancelable !== false
  );
}

/**
 * 判断当前队列是否包含后端 clearCompleted 可以移除的终态任务。
 */
export function canClearFinishedTransfers(transfers: SftpTransferSummary[]) {
  return transfers.some(isFinishedTransfer);
}

/**
 * 仅成功终态才表示远端提交完成并允许显示 100%；传输字节到齐仍可能卡在校验或提交，
 * 因此活动、失败和取消任务最多显示 99%，把数据进度与文件落盘结果区分开。
 * 未知总大小的运行任务仍保留可见进度。
 *
 * @author kongweiguang
 */
export function transferProgressPercent(transfer: SftpTransferSummary) {
  if (transfer.status === "succeeded") {
    return 100;
  }
  if (isArchiveWritePhase(transfer)) {
    return transfer.bytesTransferred > 0 || (transfer.totalBytes ?? 0) > 0 ? 96 : 12;
  }
  const totalBytes = transfer.totalBytes ?? 0;
  if (totalBytes <= 0) {
    return transfer.status === "running" ? 8 : 0;
  }
  return Math.min(
    99,
    Math.max(0, (transfer.bytesTransferred / totalBytes) * 100),
  );
}

/**
 * 获取传输任务的主标题。
 */
export function transferTitle(transfer: SftpTransferSummary) {
  const path = transfer.source.path;
  return fileNameFromPath(
    path,
    transfer.kind === "directory" ? "folder" : "file",
  );
}

/** 百分比文本复用进度模型，确保只有远端提交成功后才显示 100%。 */
export function transferPercentLabel(transfer: SftpTransferSummary) {
  if (transfer.status === "succeeded") {
    return "100%";
  }
  if (isArchiveWritePhase(transfer)) {
    return "压缩中";
  }
  const totalBytes = transfer.totalBytes ?? 0;
  if (totalBytes <= 0) {
    return transfer.status === "running" ? "..." : "0%";
  }
  return `${Math.round(transferProgressPercent(transfer))}%`;
}

/**
 * 获取上传或下载方向摘要。
 */
export function transferPathSummary(transfer: SftpTransferSummary) {
  return `${transferEndpointLabel(transfer.source)} -> ${transferEndpointLabel(transfer.target)}`;
}

/**
 * 获取传输方式的用户可见标签。
 */
export function transferMethodLabel(transfer: SftpTransferSummary) {
  if (transfer.operation === "remoteCopy") {
    if (transfer.transportMode === "clientBridge") {
      return "本机桥接";
    }
    if (transfer.transportMode === "localStage") {
      return "本机中转";
    }
    if (transfer.transportMode === "singleHostSftp") {
      return "远端复制";
    }
  }

  if (transfer.operation === "archiveDownload") {
    return "打包下载";
  }
  if (transfer.operation === "archiveUpload") {
    return "打包上传";
  }
  if (transfer.operation === "clipboardDownload") {
    return "剪贴板下载";
  }
  if (transfer.operation === "upload" || transfer.direction === "upload") {
    return "上传";
  }
  return "下载";
}

/**
 * 将后端阶段投影成低噪声状态文案；取消请求优先于运行阶段，避免用户重复点击。
 *
 * @author kongweiguang
 */
export function transferStatusLabel(
  status: SftpTransferSummary["status"],
  phase?: SftpTransferSummary["phase"],
  cancelRequested = false,
  recoveryAttempt?: number,
) {
  const normalizedPhase = phase?.toLowerCase();
  // 终态优先于迟到的取消标记，避免已取消任务在下一次轮询中闪回“正在取消”。
  if (status === "succeeded") {
    return "完成";
  }
  if (status === "failed") {
    return "失败";
  }
  if (status === "canceled") {
    return "已取消";
  }
  if (
    cancelRequested ||
    normalizedPhase === "canceling" ||
    normalizedPhase === "cancelling"
  ) {
    return "正在取消";
  }
  if (status === "queued") {
    return "排队";
  }
  if (status === "running") {
    if (
      normalizedPhase === "recovering" ||
      normalizedPhase === "reconnecting" ||
      normalizedPhase === "retrying"
    ) {
      const attempt = Math.max(1, recoveryAttempt ?? 1);
      return `正在恢复连接（${attempt}/1）`;
    }
    if (
      normalizedPhase === "waiting" ||
      normalizedPhase === "waitingresponse" ||
      normalizedPhase === "awaitingresponse"
    ) {
      return "等待响应";
    }
    if (
      normalizedPhase === "connecting" ||
      normalizedPhase === "opening" ||
      normalizedPhase === "establishingconnection" ||
      normalizedPhase === "connectingsftp"
    ) {
      return "连接中";
    }
    if (normalizedPhase === "verifying") {
      return "校验断点";
    }
    if (normalizedPhase === "committing") {
      return "提交文件";
    }
    if (normalizedPhase === "archiving") {
      return "压缩中";
    }
    if (normalizedPhase === "downloading") {
      return "下载中";
    }
    if (normalizedPhase === "uploading") {
      return "上传中";
    }
    return "传输中";
  }
  return "已取消";
}

/**
 * 为 320px 级窄面板提供短状态文案，完整含义仍通过 aria-label/title 暴露。
 * 这样文件名保留可辨识前缀；等待响应仍需与排队等待区分，不能只剩“等待”。
 *
 * @author kongweiguang
 */
export function transferCompactStatusLabel(
  status: SftpTransferSummary["status"],
  phase?: SftpTransferSummary["phase"],
  cancelRequested = false,
  recoveryAttempt?: number,
) {
  const fullLabel = transferStatusLabel(
    status,
    phase,
    cancelRequested,
    recoveryAttempt,
  );
  if (fullLabel === "正在恢复连接（1/1）") {
    return "恢复 1/1";
  }
  if (fullLabel === "正在恢复连接") {
    return "恢复中";
  }
  if (fullLabel === "等待响应") {
    return "待响应";
  }
  return fullLabel;
}

/**
 * 获取传输队列折叠态摘要。
 */
export function transferStatusSummary({
  activeCount,
  completedCount,
  failedCount,
  totalCount,
  transfer,
}: {
  activeCount: number;
  completedCount: number;
  failedCount: number;
  totalCount: number;
  transfer: SftpTransferSummary;
}) {
  if (activeCount > 0) {
    const finishedText =
      completedCount > 0 ? `，${completedCount} 项已结束` : "";
    return `后台传输 ${activeCount} 项${finishedText}`;
  }
  if (failedCount > 0) {
    return `${failedCount} 项传输失败，可从任务记录重试或清理`;
  }
  if (completedCount > 0) {
    return `${completedCount} 项传输完成`;
  }
  if (totalCount > 1) {
    return `${totalCount} 项传输任务`;
  }
  return transferPathSummary(transfer);
}

function transferEndpointLabel(endpoint: SftpTransferEndpoint) {
  if (endpoint.kind === "local") {
    return endpoint.path;
  }
  const hostLabel =
    readableEndpointPart(endpoint.hostLabel) ??
    readableEndpointPart(endpoint.hostId) ??
    "远端";
  return `${hostLabel}:${endpoint.path}`;
}

function readableEndpointPart(value: string | undefined) {
  const text = value?.trim();
  if (!text || text === "undefined" || text === "null") {
    return undefined;
  }
  return text;
}

function isArchiveWritePhase(transfer: SftpTransferSummary) {
  return transfer.status === "running" && transfer.phase === "archiving";
}

/**
 * 为运行阶段提供主题安全的 badge 颜色；恢复和等待不使用失败红色，减少误报焦虑。
 *
 * @author kongweiguang
 */
export function transferStatusClassName(
  status: SftpTransferSummary["status"],
  phase?: SftpTransferSummary["phase"],
  cancelRequested = false,
) {
  const normalizedPhase = phase?.toLowerCase();
  // 颜色与文案共享同一终态优先级，避免已结束任务继续呈现取消中的琥珀色。
  if (status === "succeeded") {
    return "border-emerald-300/35 bg-emerald-500/10 text-emerald-700 dark:text-emerald-100";
  }
  if (status === "failed") {
    return "border-rose-300/35 bg-rose-500/10 text-rose-700 dark:text-rose-100";
  }
  if (status === "canceled") {
    return "border-zinc-300/40 bg-zinc-500/10 text-zinc-600 dark:border-zinc-600 dark:text-zinc-300";
  }
  if (
    cancelRequested ||
    normalizedPhase === "canceling" ||
    normalizedPhase === "cancelling"
  ) {
    return "border-amber-300/40 bg-amber-500/10 text-amber-700 dark:text-amber-100";
  }
  if (
    normalizedPhase === "recovering" ||
    normalizedPhase === "reconnecting" ||
    normalizedPhase === "retrying" ||
    normalizedPhase === "waiting" ||
    normalizedPhase === "waitingresponse" ||
    normalizedPhase === "awaitingresponse" ||
    normalizedPhase === "verifying" ||
    normalizedPhase === "committing" ||
    normalizedPhase === "connecting" ||
    normalizedPhase === "opening" ||
    normalizedPhase === "establishingconnection" ||
    normalizedPhase === "connectingsftp"
  ) {
    return "border-violet-300/35 bg-violet-500/10 text-violet-700 dark:text-violet-100";
  }
  if (status === "running") {
    return "border-sky-300/35 bg-sky-500/10 text-sky-700 dark:text-sky-100";
  }
  if (status === "queued") {
    return "border-amber-300/35 bg-amber-500/10 text-amber-700 dark:text-amber-100";
  }
  return "border-zinc-300/40 bg-zinc-500/10 text-zinc-600 dark:border-zinc-600 dark:text-zinc-300";
}

/**
 * 获取已传输字节和总字节展示文本。
 */
export function formatTransferBytes(transfer: SftpTransferSummary) {
  const totalBytes = transfer.totalBytes ?? 0;
  const bytesLabel =
    totalBytes <= 0
      ? `${formatFileSize(transfer.bytesTransferred)} / -`
      : `${formatFileSize(transfer.bytesTransferred)} / ${formatFileSize(totalBytes)}`;
  const speedLabel = transferSpeedLabel(transfer);
  return speedLabel ? `${bytesLabel} · ${speedLabel}` : bytesLabel;
}

/**
 * 速度只有在最近确认仍然有效时显示，停滞期间归零以免用户误以为仍在发送数据。
 *
 * @author kongweiguang
 */
function transferSpeedLabel(transfer: SftpTransferSummary) {
  const speedBytesPerSecond = transfer.speedBytesPerSecond ?? 0;
  if (
    transfer.status !== "running" ||
    speedBytesPerSecond <= 0 ||
    isTransferWaiting(transfer)
  ) {
    return null;
  }
  return `${formatFileSize(speedBytesPerSecond)}/s`;
}

/**
 * 判断最近一次已确认字节是否超过 5 秒未变化；连接恢复和取消阶段不显示为网络停滞。
 * 时间戳同时兼容 Unix 秒和毫秒，避免旧任务或测试 fixture 造成错误等待提示。
 *
 * @author kongweiguang
 */
export function isTransferWaiting(
  transfer: SftpTransferSummary,
  now = Date.now(),
) {
  if (transfer.status !== "running" || transfer.cancelRequested) {
    return false;
  }
  const normalizedPhase = transfer.phase?.toLowerCase();
  if (
    normalizedPhase === "canceling" ||
    normalizedPhase === "cancelling" ||
    normalizedPhase === "recovering" ||
    normalizedPhase === "reconnecting" ||
    normalizedPhase === "retrying" ||
    normalizedPhase === "waiting" ||
    normalizedPhase === "waitingresponse" ||
    normalizedPhase === "awaitingresponse" ||
    normalizedPhase === "verifying" ||
    normalizedPhase === "committing" ||
    normalizedPhase === "connecting" ||
    normalizedPhase === "opening" ||
    normalizedPhase === "establishingconnection" ||
    normalizedPhase === "connectingsftp"
  ) {
    return true;
  }
  if (transfer.lastProgressAt === undefined || transfer.lastProgressAt === null) {
    return false;
  }
  const timestamp = Number(transfer.lastProgressAt);
  if (!Number.isFinite(timestamp) || timestamp <= 0) {
    return false;
  }
  const timestampMs = timestamp < 100_000_000_000 ? timestamp * 1000 : timestamp;
  return now - timestampMs >= SFTP_TRANSFER_WAITING_GRACE_MS;
}

/**
 * 将结构化无进度及提交不明失败投影为稳定文案；提交结果不明时先核对远端状态再决定，
 * 避免提示用户盲目重试并覆盖可能已经提交的文件。
 *
 * @author kongweiguang
 */
export function transferFailureMessage(transfer: SftpTransferSummary) {
  if (transfer.failureKind === "commitUnknown") {
    return "提交结果未确认。请刷新目录核对目标文件后，再决定是否重新传输。";
  }
  if (transfer.failureKind !== "idleTimeout") {
    return transfer.error ?? null;
  }
  const seconds = transfer.idleTimeoutSeconds ?? 180;
  return seconds % 60 === 0
    ? `网络连续 ${seconds / 60} 分钟无响应`
    : `网络连续 ${seconds} 秒无响应`;
}

/**
 * 为紧凑队列行提供固定宽度的实时速度文本；未运行时保持短占位避免布局跳动。
 *
 * @author kongweiguang
 */
export function transferInlineSpeedLabel(transfer: SftpTransferSummary) {
  if (isTransferWaiting(transfer)) {
    return "0 B/s";
  }
  return transferSpeedLabel(transfer) ?? "-";
}
