/**
 * SFTP 传输队列的紧凑任务行。
 *
 * @author kongweiguang
 */

import {
  ChevronDown,
  ChevronUp,
  Download,
  RefreshCw,
  Upload,
  X,
} from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { Button } from "../../components/ui/button";
import { cn } from "../../lib/cn";
import type { SftpTransferSummary } from "../../lib/sftpApi";
import {
  canCancelTransfer,
  formatTransferBytes,
  transferFailureMessage,
  transferInlineSpeedLabel,
  transferMethodLabel,
  transferPathSummary,
  transferPercentLabel,
  transferProgressPercent,
  transferCompactStatusLabel,
  isTransferWaiting,
  transferStatusClassName,
  transferStatusLabel,
  transferTitle,
} from "./sftpTransferModel";
import { resolveSftpTransferRetry } from "./sftpTransferRetryPolicy";

interface SftpTransferQueueRowProps {
  onCancel: (transferId: string) => void | Promise<void>;
  onRetry?: (transfer: SftpTransferSummary) => void | Promise<void>;
  transfer: SftpTransferSummary;
}

/**
 * 失败原因直接出现在任务行，避免用户展开详情才能定位；每项失败只保留一个恢复动作。
 *
 * 长路径、字节明细仍按需展开，避免正常队列因错误说明而持续增高；但网络失联不能藏在
 * 详情里，否则用户无法判断刷新图标是否会安全地从断点继续。
 */
export function SftpTransferQueueRow({
  onCancel,
  onRetry,
  transfer,
}: SftpTransferQueueRowProps) {
  const detailId = useId();
  const [detailExpanded, setDetailExpanded] = useState(false);
  const [retryPending, setRetryPending] = useState(false);
  const [cancelPending, setCancelPending] = useState(false);
  const retryInFlightRef = useRef(false);
  const cancelInFlightRef = useRef(false);
  const progress = transferProgressPercent(transfer);
  const canCancel = canCancelTransfer(transfer) && !cancelPending;
  const retryDecision =
    transfer.status === "failed" || transfer.status === "canceled"
      ? resolveSftpTransferRetry(transfer)
      : null;
  const canShowCancelButton =
    (transfer.status === "queued" || transfer.status === "running") &&
    transfer.cancelable !== false;
  const showRetryUnavailable =
    retryDecision &&
    !retryDecision.canRetry &&
    transfer.transportMode === "singleHostSftp";
  const DirectionIcon = transfer.direction === "upload" ? Upload : Download;
  const DetailIcon = detailExpanded ? ChevronUp : ChevronDown;
  const directionLabel = transfer.direction === "upload" ? "上传" : "下载";
  const title = transferTitle(transfer);
  const isInlineFailure =
    transfer.status === "failed" ||
    transfer.failureKind === "idleTimeout" ||
    transfer.failureKind === "commitUnknown";
  const failureMessage = transferFailureMessage(transfer) ||
    (transfer.status === "failed" ? "传输失败，未收到详细错误信息。" : null);
  const isWaiting = isTransferWaiting(transfer);
  const normalizedPhase = transfer.phase?.toLowerCase();
  const hasExplicitLifecyclePhase = [
    "recovering",
    "reconnecting",
    "retrying",
    "waiting",
    "waitingresponse",
    "awaitingresponse",
    "verifying",
    "committing",
    "connecting",
    "opening",
    "establishingconnection",
    "connectingsftp",
  ].includes(normalizedPhase ?? "");
  const visiblePhase =
    cancelPending && (transfer.status === "queued" || transfer.status === "running")
      ? "canceling"
      : isWaiting && !hasExplicitLifecyclePhase
        ? "waiting"
        : transfer.phase;
  const isActiveCanceling =
    (transfer.status === "queued" || transfer.status === "running") &&
    (cancelPending ||
      transfer.cancelRequested ||
      normalizedPhase === "canceling" ||
      normalizedPhase === "cancelling");
  const statusLabel = transferStatusLabel(
    transfer.status,
    visiblePhase,
    transfer.cancelRequested,
    transfer.recoveryAttempt,
  );
  const compactStatusLabel = transferCompactStatusLabel(
    transfer.status,
    visiblePhase,
    transfer.cancelRequested,
    transfer.recoveryAttempt,
  );
  const hasSuccessor = Boolean(transfer.successorId);
  const visibleStatusLabel = hasSuccessor ? "已重新排队" : statusLabel;
  const visibleCompactStatusLabel = hasSuccessor
    ? "已排队"
    : compactStatusLabel;

  // 无事件期间也要及时归零速度，不能依赖最长十秒一次的安全轮询触发渲染。
  const [, setClock] = useState(0);
  useEffect(() => {
    if (transfer.status !== "running") return;
    const timer = window.setInterval(() => setClock((value) => value + 1), 1000);
    return () => window.clearInterval(timer);
  }, [transfer.status]);

  /**
   * ref 在同一帧内立即关闸；React 尚未提交 disabled 属性时也只能发送一次取消。
   * 请求返回后由权威摘要保留正在取消状态。
   */
  async function requestCancel() {
    if (!canCancel || cancelInFlightRef.current) return;
    cancelInFlightRef.current = true;
    setCancelPending(true);
    try {
      await onCancel(transfer.id);
    } finally {
      cancelInFlightRef.current = false;
      setCancelPending(false);
    }
  }

  /** 同帧双击只发一个恢复请求；后继任务出现后由 successorId 永久关闭旧入口。 */
  async function requestRetry() {
    if (retryPending || retryInFlightRef.current || !onRetry) return;
    retryInFlightRef.current = true;
    setRetryPending(true);
    try {
      await onRetry(transfer);
    } finally {
      retryInFlightRef.current = false;
      setRetryPending(false);
    }
  }

  return (
    <div
      aria-label={`SFTP ${directionLabel} ${title}`}
      className={cn(
        "kerminal-muted-surface min-w-0 overflow-hidden rounded-lg border px-2.5 py-1.5 transition-opacity",
        transfer.status === "succeeded" &&
          "opacity-70 hover:opacity-100 focus-within:opacity-100",
      )}
      role="group"
      aria-busy={retryPending || cancelPending || isActiveCanceling || undefined}
    >
      <div className="flex min-w-0 items-center gap-2 max-[380px]:gap-1.5">
        <span
          className={cn(
            "flex h-6 w-6 shrink-0 items-center justify-center rounded-md",
            transfer.status === "failed"
              ? "bg-rose-500/10 text-rose-500 dark:text-rose-300"
              : transfer.status === "succeeded"
                ? "bg-emerald-500/10 text-emerald-500 dark:text-emerald-300"
                : "bg-sky-500/10 text-sky-500 dark:text-sky-300",
          )}
          title={directionLabel}
        >
          <DirectionIcon aria-hidden="true" className="h-3.5 w-3.5" />
        </span>
        <span
          className="min-w-0 flex-1 truncate text-xs font-medium text-zinc-900 dark:text-zinc-100"
          data-sftp-transfer-title="true"
          title={title}
        >
          {title}
        </span>
        <span
          aria-label={visibleStatusLabel}
          className={cn(
            "shrink-0 rounded-md border px-1.5 py-0.5 text-[10px] max-[380px]:px-1",
            transferStatusClassName(
              transfer.status,
              visiblePhase,
              transfer.cancelRequested,
            ),
          )}
          title={
            hasSuccessor
              ? "该任务已创建后继任务，后继任务将显示在队列中"
              : statusLabel
          }
        >
          <span className="max-[380px]:hidden">{visibleStatusLabel}</span>
          <span className="hidden max-[380px]:inline">
            {visibleCompactStatusLabel}
          </span>
        </span>
        <span className="w-12 shrink-0 text-right font-mono text-[11px] text-zinc-600 dark:text-zinc-300 max-[380px]:w-10">
          {transferPercentLabel(transfer)}
        </span>
        <span
          aria-label={`传输速度 ${transferInlineSpeedLabel(transfer)}`}
          className="w-14 shrink-0 truncate text-right font-mono text-[10px] text-zinc-500 dark:text-zinc-400 max-[380px]:w-10 max-[380px]:text-[9px]"
          title={transferInlineSpeedLabel(transfer)}
        >
          {transferInlineSpeedLabel(transfer)}
        </span>
        <Button
          aria-controls={detailId}
          aria-expanded={detailExpanded}
          aria-label={`${detailExpanded ? "收起" : "查看"}传输详情 ${title}`}
          className="kerminal-muted-surface h-6 w-6 shrink-0 rounded-md border px-0 text-zinc-600 hover:bg-[var(--surface-hover)] dark:text-zinc-300 max-[380px]:h-5 max-[380px]:w-5"
          onClick={() => setDetailExpanded((current) => !current)}
          size="sm"
          title={detailExpanded ? "收起详情" : "查看详情"}
          type="button"
          variant="ghost"
        >
          <DetailIcon aria-hidden="true" className="h-3 w-3" />
        </Button>
        {retryDecision?.canRetry && onRetry && !isInlineFailure ? (
          <Button
            aria-label={`${retryDecision.actionLabel} ${title}`}
          className="kerminal-muted-surface h-6 w-6 shrink-0 rounded-md border px-0 text-zinc-600 hover:bg-[var(--surface-hover)] dark:text-zinc-300 max-[380px]:h-5 max-[380px]:w-5"
            disabled={retryPending}
            onClick={requestRetry}
            size="sm"
            title={retryDecision.statusMessage}
            type="button"
            variant="ghost"
          >
            <RefreshCw aria-hidden="true" className="h-3 w-3" />
          </Button>
        ) : null}
        {canShowCancelButton ? (
          <Button
            aria-label={`取消传输 ${title}`}
          className="kerminal-muted-surface h-6 w-6 shrink-0 rounded-md border px-0 text-zinc-600 hover:bg-[var(--surface-hover)] dark:text-zinc-300 max-[380px]:h-5 max-[380px]:w-5"
            disabled={!canCancel}
            onClick={requestCancel}
            size="sm"
            title={canCancel ? "取消传输" : "正在取消"}
            type="button"
            variant="ghost"
          >
            <X aria-hidden="true" className="h-3 w-3" />
          </Button>
        ) : null}
      </div>
      <div className="mt-1.5 flex min-w-0 items-center gap-2 max-[380px]:gap-1.5">
        <div
          aria-label={`传输进度 ${title}`}
          aria-valuemax={100}
          aria-valuemin={0}
          aria-valuenow={Math.round(progress)}
          className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-[var(--surface-muted)]"
          role="progressbar"
        >
          <div
            className={cn(
              "h-full rounded-full transition-all",
              transfer.status === "failed"
                ? "bg-rose-500"
                : transfer.status === "canceled"
                  ? "bg-zinc-400"
                  : transfer.status === "succeeded"
                    ? "bg-emerald-500"
                    : "bg-sky-500",
            )}
            style={{ width: `${progress}%` }}
          />
        </div>
      </div>
      {isInlineFailure && failureMessage && !hasSuccessor ? (
        <div className="mt-1.5 flex min-w-0 flex-wrap items-center justify-between gap-2 rounded-[var(--radius-control)] border border-rose-500/20 bg-rose-500/8 px-2 py-1 text-[11px] text-rose-700 dark:border-rose-300/20 dark:bg-rose-400/10 dark:text-rose-200">
          <span className="min-w-0 flex-1 break-words [overflow-wrap:anywhere]" role="alert">
            {failureMessage}
          </span>
          {retryDecision?.canRetry && onRetry ? (
            <Button
              aria-label={`${retryDecision.actionLabel} ${title}`}
              className="h-6 shrink-0 gap-1 rounded-[var(--radius-control)] border border-current/25 bg-transparent px-2 text-[11px] text-current hover:bg-rose-500/10 dark:hover:bg-rose-300/10"
              disabled={retryPending}
              onClick={requestRetry}
              size="sm"
              type="button"
              variant="ghost"
            >
              <RefreshCw aria-hidden="true" className="h-3 w-3" />
              {retryDecision.actionLabel}
            </Button>
          ) : null}
        </div>
      ) : null}
      {detailExpanded ? (
        <div
          className="mt-2 grid gap-1 border-t border-[var(--border-subtle)] pt-2 text-[11px] text-zinc-500 dark:text-zinc-400"
          id={detailId}
        >
          <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
            <span>{transferMethodLabel(transfer)}</span>
            <span className="font-mono">{formatTransferBytes(transfer)}</span>
          </div>
          <div className="break-all font-mono" title={transferPathSummary(transfer)}>
            {transferPathSummary(transfer)}
          </div>
          {failureMessage && (!isInlineFailure || hasSuccessor) ? (
            <div className="break-words text-rose-600 dark:text-rose-300">
              {failureMessage}
            </div>
          ) : null}
          {hasSuccessor ? (
            <div className="break-words text-sky-700 dark:text-sky-200">
              已重新排队，后继任务正在队列中执行。
            </div>
          ) : null}
          {showRetryUnavailable && transfer.failureKind !== "commitUnknown" ? (
            <div className="break-words text-amber-700 dark:text-amber-200">
              不能安全重试：{retryDecision.statusMessage}
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
