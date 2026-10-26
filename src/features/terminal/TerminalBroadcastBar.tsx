// @author kongweiguang

import { Send } from "lucide-react";
import type { CSSProperties, KeyboardEvent } from "react";
import { Button } from "../../components/ui/button";
import { Switch } from "../../components/ui/switch";
import { cn } from "../../lib/cn";
import {
  canBroadcastCommand,
  type BroadcastCommandAnalysis,
} from "./broadcastCommandPolicy";
import { TerminalBroadcastTargetSelector } from "./TerminalBroadcastTargetSelector";
import type {
  BroadcastTargetMode,
  BroadcastTargetOption,
} from "./terminalBroadcastTargets";
import type { TerminalInputSyncSnapshot } from "./terminalInputSync";

interface TerminalBroadcastBarProps {
  analysis: BroadcastCommandAnalysis;
  draft: string;
  error: string | null;
  focusedPaneId: string;
  inputSync: TerminalInputSyncSnapshot;
  onToggleInputSync: () => void;
  onDraftChange: (draft: string) => void;
  onRequestBroadcast: () => void;
  onTargetModeChange: (mode: BroadcastTargetMode) => void;
  onToggleCustomTarget: (paneId: string, selected: boolean) => void;
  selectedTargetPaneIds: string[];
  sending: boolean;
  status: string | null;
  style?: CSSProperties;
  targetMode: BroadcastTargetMode;
  targetOptions: BroadcastTargetOption[];
  toolbarPaddingClass: string;
}

/** 广播栏共享终端双侧 inset；同步复用标准开关并在鼠标操作时保留终端焦点。 */
export function TerminalBroadcastBar({
  analysis,
  draft,
  error,
  focusedPaneId,
  inputSync,
  onToggleInputSync,
  onDraftChange,
  onRequestBroadcast,
  onTargetModeChange,
  onToggleCustomTarget,
  selectedTargetPaneIds,
  sending,
  status,
  style,
  targetMode,
  targetOptions,
  toolbarPaddingClass,
}: TerminalBroadcastBarProps) {
  const sendLabel =
    analysis.targetCount > 0
      ? `发送到 ${analysis.targetCount} 个目标`
      : "没有可发送目标";
  const syncError = inputSync.notice?.level === "error" ? inputSync.notice.message : null;
  const feedbackError = Boolean(error || syncError);
  const feedbackStatus = inputSync.notice?.level === "status" ? inputSync.notice.message : status;
  const syncTitle = inputSync.disabledReason ??
    "同步所选分屏的输入（含粘贴、终端快捷键）；请确认各端命令行状态一致。";

  const handleInputKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key !== "Enter") {
      return;
    }
    event.preventDefault();
    onRequestBroadcast();
  };

  return (
    <>
      <div
        className={cn(
          "flex items-center gap-2 border-b border-[var(--border-subtle)] transition-[margin-left,margin-right] duration-200 ease-out",
          toolbarPaddingClass,
        )}
        data-terminal-workspace-inset
        style={style}
      >
        <TerminalBroadcastTargetSelector
          focusedPaneId={focusedPaneId}
          onTargetModeChange={onTargetModeChange}
          onToggleCustomTarget={onToggleCustomTarget}
          selectedTargetPaneIds={selectedTargetPaneIds}
          targetCount={analysis.targetCount}
          targetMode={targetMode}
          targetOptions={targetOptions}
        />
        <label className="sr-only" htmlFor="broadcast-command">
          批量命令
        </label>
        <input
          className="kerminal-field-surface h-9 min-w-0 flex-1 rounded-xl border px-3 font-mono text-sm text-zinc-950 placeholder:text-zinc-400 dark:text-zinc-100 dark:placeholder:text-zinc-600"
          id="broadcast-command"
          onChange={(event) => onDraftChange(event.currentTarget.value)}
          onKeyDown={handleInputKeyDown}
          placeholder="向所选目标发送命令..."
          value={draft}
        />
        <span className="hidden shrink-0 rounded-lg bg-[var(--surface-hover)] px-2 py-1 text-xs text-zinc-500 dark:text-zinc-400 xl:inline">
          {analysis.targetCount} 个目标
        </span>
        <span
          className="inline-flex h-9 shrink-0 items-center gap-2 text-xs text-[var(--text-secondary)]"
          title={syncTitle}
        >
          <span>同步</span>
          <Switch
            aria-label="同步终端输入"
            checked={inputSync.enabled}
            disabled={inputSync.disabledReason !== null}
            onCheckedChange={onToggleInputSync}
            onMouseDown={(event) => { if (event.button === 0) event.preventDefault(); }}
          />
        </span>
        <Button
          aria-label={sending ? "发送中" : sendLabel}
          disabled={!canBroadcastCommand(analysis) || sending}
          onClick={onRequestBroadcast}
          size="icon"
          title={sending ? "发送中" : sendLabel}
          variant="primary"
        >
          <Send className="h-4 w-4" />
        </Button>
      </div>

      {feedbackStatus || feedbackError ? (
        <div
          className={cn(
            "border-b border-[var(--border-subtle)] px-3 py-2 text-sm transition-[margin-left,margin-right] duration-200 ease-out",
            feedbackError
              ? "bg-rose-500/10 text-rose-700 dark:text-rose-100"
              : "bg-emerald-500/10 text-emerald-700 dark:text-emerald-100",
          )}
          role={feedbackError ? "alert" : "status"}
          style={style}
        >
          {feedbackError ? <>
            {error ? <span>{error}</span> : null}
            {syncError ? <span className={error ? "ml-2" : undefined}>{syncError}</span> : null}
          </> : feedbackStatus}
        </div>
      ) : null}
    </>
  );
}
