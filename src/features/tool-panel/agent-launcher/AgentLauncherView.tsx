// @author kongweiguang
import {
  useEffect,
  useRef,
  useState,
} from "react";
import { Info, Send } from "lucide-react";
import { Button } from "../../../components/ui/button";
import { UserFacingNotice } from "../../../components/ui/user-facing-notice";
import {
  type AgentWorkflowSnapshot,
} from "../../agent-workflow";
import { cn } from "../../../lib/cn";
import type {
  AgentSessionScope,
  AgentSessionTargetRequest,
} from "../../../lib/agentLauncherApi";
import type { UserFacingMessage } from "../../../lib/userFacingMessage";
import type { AgentSendRequest } from "../../agent-workflow/state/index";
import type { CustomAgentDefinition } from "../../settings/contracts/index";
import type { AgentLaunchPermissionMode } from "./agentLauncherModel";
import {
  AgentLaunchSplitButton,
  type AgentLaunchTargetMode,
} from "./AgentLaunchControls";
import {
  AgentSelector,
  type AgentSelectorOption,
} from "./AgentSelector";
import {
  CustomAgentManagerDialog,
  type CustomAgentDraft,
} from "./CustomAgentManagerDialog";
import { AgentConversationList } from "./AgentConversationList";

export type AgentLauncherLoadState =
  "idle" | "loading" | "refreshing" | "error";
export type AgentLauncherActionState = string | null;

export interface AgentLauncherViewProps {
  actionError: UserFacingMessage | null;
  actionState: AgentLauncherActionState;
  agentOptions: AgentSelectorOption[];
  currentAgentTarget?: AgentSessionTargetRequest;
  /** 缺省仅用于旧测试/嵌入调用方；主工具面板始终传入显式 scope。 */
  currentAgentScope?: AgentSessionScope;
  currentAgentTargetLabel: string;
  customAgentError: UserFacingMessage | null;
  customAgentMutationPending: boolean;
  customAgents: CustomAgentDefinition[];
  deletingSessionId: string | null;
  loadError: UserFacingMessage | null;
  loadState: AgentLauncherLoadState;
  pendingSendRequest: AgentSendRequest | null;
  statusAvailable: boolean;
  visible: boolean;
  selectedAgentKey: string;
  onAgentSelect: (key: string) => void;
  onCustomAgentDelete: (id: string) => Promise<boolean>;
  onCustomAgentSave: (draft: CustomAgentDraft) => Promise<boolean>;
  onLaunchSelected: (
    permissionMode?: AgentLaunchPermissionMode,
    targetMode?: AgentLaunchTargetMode,
  ) => void;
  onRetry: () => void;
  onWorkflowContinue: (sessionId: string) => void;
  onWorkflowDelete: (sessionId: string) => Promise<boolean>;
  onWorkflowRename: (sessionId: string, title: string) => Promise<boolean>;
  renamingSessionId: string | null;
  workflowSnapshot: AgentWorkflowSnapshot;
}

/**
 * Agent Launcher 的纯 UI 组合层。
 *
 * 会话创建、恢复、归档和终端信号仍由上层编排；这里只持有管理弹窗的短期状态，
 * 命令提示用原生 title，避免为了静态提示维护弹层状态。
 */
export function AgentLauncherView({
  actionError,
  actionState,
  agentOptions,
  currentAgentTarget,
  currentAgentScope,
  currentAgentTargetLabel,
  customAgentError,
  customAgentMutationPending,
  customAgents,
  deletingSessionId,
  loadError,
  loadState,
  pendingSendRequest,
  statusAvailable,
  visible,
  selectedAgentKey,
  onAgentSelect,
  onCustomAgentDelete,
  onCustomAgentSave,
  onLaunchSelected,
  onRetry,
  onWorkflowContinue,
  onWorkflowDelete,
  onWorkflowRename,
  renamingSessionId,
  workflowSnapshot,
}: AgentLauncherViewProps) {
  const [customAgentDialogOpen, setCustomAgentDialogOpen] = useState(false);
  const selectorButtonRef = useRef<HTMLButtonElement | null>(null);
  const launchButtonRef = useRef<HTMLButtonElement | null>(null);
  const customAgentDialogReturnFocusRef = useRef<HTMLElement | null>(null);
  const selectedAgent =
    agentOptions.find((option) => option.key === selectedAgentKey) ??
    agentOptions[0] ??
    null;

  useEffect(() => {
    if (!visible) {
      setCustomAgentDialogOpen(false);
    }
  }, [visible]);

  /** 普通关闭恢复到选择器；保存成功则前移到主“进入”按钮，形成连续键盘流程。 */
  const closeCustomAgentDialog = (focusLaunchButton = false) => {
    customAgentDialogReturnFocusRef.current = focusLaunchButton
      ? launchButtonRef.current
      : selectorButtonRef.current;
    setCustomAgentDialogOpen(false);
  };

  return (
    <div
      aria-hidden={!visible}
      inert={!visible}
      className={cn(
        "absolute inset-0 flex min-h-0 flex-col px-3 py-3 transition-opacity duration-150",
        visible ? "opacity-100" : "pointer-events-none select-none opacity-0",
      )}
    >
      <div className="scrollbar-none flex min-h-0 flex-1 overflow-y-auto">
        <div
          className="relative mx-auto my-auto w-full max-w-[320px] py-2"
          data-testid="agent-launcher-content"
        >
          <div className="mb-2 flex min-w-0 items-center gap-2 px-1">
            <div
              className="min-w-0 flex-1"
              data-testid="agent-current-target"
              title={
                pendingSendRequest
                  ? `${agentSendRequestLabel(pendingSendRequest)}；当前目标：${currentAgentTargetLabel}`
                  : currentAgentTargetLabel
              }
            >
              {pendingSendRequest ? (
                <h1 className="text-sm font-semibold text-zinc-950 dark:text-zinc-100">
                  发送到 Agent
                </h1>
              ) : null}
              {pendingSendRequest ? (
                <span
                  className="mt-0.5 flex h-4 min-w-0 items-center gap-1 text-[11px] text-sky-700 dark:text-sky-300"
                  data-testid="agent-launcher-pending-send"
                  role="status"
                >
                  <Send className="h-3 w-3 shrink-0" />
                  <span className="truncate">
                    {agentSendRequestLabel(pendingSendRequest)} · 选择 Agent
                    后预览
                  </span>
                </span>
              ) : (
                <span className="block h-4 truncate text-[11px] text-zinc-500 dark:text-zinc-400">
                  当前目标 ·{" "}
                  <span className="font-medium text-zinc-700 dark:text-zinc-300">
                    {currentAgentTargetLabel}
                  </span>
                </span>
              )}
            </div>
            <span
              aria-label="启动命令"
              className="kerminal-focus-ring grid h-6 w-6 shrink-0 cursor-help place-items-center rounded-md text-zinc-500 dark:text-zinc-400"
              role="img"
              tabIndex={0}
              title={`启动命令：${selectedAgent?.launchCommand || selectedAgent?.commandLabel || "未配置"}`}
            >
              <Info aria-hidden className="h-3.5 w-3.5" />
            </span>
          </div>
          <div
            aria-label="Agent 启动控件"
            className="flex min-w-0 items-stretch px-0.5"
            role="group"
          >
            <AgentSelector
              actionState={actionState}
              active={visible}
              disabled={customAgentMutationPending}
              onManageCustomAgents={() => {
                customAgentDialogReturnFocusRef.current = selectorButtonRef.current;
                setCustomAgentDialogOpen(true);
              }}
              onSelect={onAgentSelect}
              options={agentOptions}
              selectedKey={selectedAgentKey}
              triggerRef={selectorButtonRef}
            />
            <AgentLaunchSplitButton
              actionState={actionState}
              disabled={customAgentMutationPending}
              onLaunch={onLaunchSelected}
              option={selectedAgent}
              primaryButtonRef={launchButtonRef}
            />
          </div>
          <AgentConversationList
            actionDisabled={actionState !== null}
            currentScope={currentAgentScope}
            currentTarget={currentAgentTarget}
            deletingSessionId={deletingSessionId}
            historyMetadata={workflowSnapshot.historyMetadata}
            onContinue={onWorkflowContinue}
            onDelete={onWorkflowDelete}
            onRename={onWorkflowRename}
            renamingSessionId={renamingSessionId}
            sessions={workflowSnapshot.sessions}
          />
        </div>
      </div>

      {loadState === "error" && !statusAvailable ? (
        <UserFacingNotice
          className="mt-3"
          compact
          message={
            loadError ?? {
              recoveryAction: "请稍后重试。",
              severity: "error",
              title: "无法读取 Agent 状态",
            }
          }
        >
          <Button onClick={onRetry} size="sm">
            重试
          </Button>
        </UserFacingNotice>
      ) : null}
      {actionError ? (
        <UserFacingNotice className="mt-3" compact message={actionError} />
      ) : null}
      <CustomAgentManagerDialog
        customAgents={customAgents}
        error={customAgentError}
        mutationPending={customAgentMutationPending}
        onClose={() => closeCustomAgentDialog(false)}
        onDelete={onCustomAgentDelete}
        onSave={onCustomAgentSave}
        onSaved={() => closeCustomAgentDialog(true)}
        open={customAgentDialogOpen}
        returnFocusRef={customAgentDialogReturnFocusRef}
      />
    </div>
  );
}

/** 将待发送来源转成简短提示，避免在启动区提前展开终端内容。 */
function agentSendRequestLabel(request: AgentSendRequest): string {
  if (request.source === "selection") {
    return "选中内容待发送";
  }
  if (request.source === "commandBlock") {
    return "命令块待发送";
  }
  return "终端上下文待发送";
}
