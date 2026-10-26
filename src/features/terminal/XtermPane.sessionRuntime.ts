// @author kongweiguang
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import {
  closeTerminal,
  getTerminalLogState,
  type TerminalAgentSignal,
  type TerminalSessionLogState,
} from "../../lib/terminalApi";
import { terminalSuggestionProbeScheduler } from "./terminalSuggestionProbeScheduler";
import {
  markTerminalPaneSessionDisconnected,
  markTerminalPaneSessionReconnected,
  registerTerminalPaneReconnectHandler,
  registerTerminalPaneSession,
  unregisterTerminalPaneReconnectHandler,
  unregisterTerminalPaneSession,
} from "./terminalSessionRegistry";
import type { TerminalOutputHistoryBuffer } from "./terminalOutputHistoryBuffer";
import type { TerminalOutputInstrumentation } from "./terminalOutputInstrumentation";
import type { TerminalOutputWriter } from "./terminalOutputWriter";
import type { TerminalPaneResizeController } from "./terminalPaneResizeController";
import { createTerminalReconnectRuntime } from "./terminalReconnectRuntime";
import { createTerminalSessionOutputController } from "./terminalSessionOutputController";
import { createTerminalSessionStatusPollController } from "./terminalSessionStatusPollController";
import { createSshTerminalFailureTracker, formatSshTerminalFailureMessage } from "./terminalSshFailurePolicy";
import type { XtermPaneActivityRuntime } from "./XtermPane.activityRuntime";
import type { XtermPaneArtifactRuntime } from "./XtermPane.artifacts";
import type { createXtermPaneCommandBlockRuntime } from "./XtermPane.commandBlockRuntime";
import type { createXtermPaneGhostSuggestions } from "./XtermPane.ghostSuggestions";
import { errorMessage, type ConnectionState } from "./XtermPane.helpers";
import { createInitialRemoteOutputGate } from "./terminalInitialRemoteOutputGate";
import type { createTerminalInlineSshAuthPrompt } from "./XtermPane.inlineSshAuthPrompt";
import { cancelSshHostKeyPromptsForOwner } from "../ssh-host-key/state/index";
import {
  terminalSessionFailureLabel,
  terminalSessionStartupNotice,
  terminalSessionTargetKind,
} from "./XtermPane.runtime.helpers";
import type { InstallXtermPaneRuntimeParams } from "./XtermPane.runtime.types";
import { createXtermPaneTerminalSession } from "./XtermPane.sessionFactory";
import { terminalInputSync } from "./terminalInputSync";

type SessionConfig = Pick<
  InstallXtermPaneRuntimeParams,
  | "args"
  | "cwd"
  | "currentCwdRef"
  | "cwdTrackingBufferRef"
  | "disconnectSessionRef"
  | "env"
  | "focusedRef"
  | "onAgentSignalRef"
  | "onSessionFinishedRef"
  | "paneId"
  | "profileId"
  | "reconnectSessionRef"
  | "remoteCommand"
  | "remoteHostId"
  | "sessionIdRef"
  | "setConnectionState"
  | "setLogNotice"
  | "setLogState"
  | "shell"
  | "startupMessage"
  | "tabIdRef"
  | "target"
  | "terminalAppearanceRef"
  | "transientStartupMessage"
  | "visibleRef"
>;

interface CreateXtermPaneSessionRuntimeOptions extends SessionConfig {
  activityRuntime: XtermPaneActivityRuntime;
  artifactRuntime: XtermPaneArtifactRuntime;
  assistEnabled: boolean;
  commandBlockRuntime: ReturnType<typeof createXtermPaneCommandBlockRuntime>;
  ghostSuggestions: ReturnType<typeof createXtermPaneGhostSuggestions>;
  instrumentation: TerminalOutputInstrumentation | null;
  onCurrentCwd(cwd: string): void;
  outputHistoryBuffer: TerminalOutputHistoryBuffer;
  outputWriter: TerminalOutputWriter;
  paneResizeController: TerminalPaneResizeController;
  remoteSuggestionPrewarm: {
    scheduleGit(cwd?: string): void;
    scheduleRemoteCommand(): void;
    scheduleRemoteHistory(): void;
    scheduleRemotePath(cwd?: string): void;
  };
  refreshSurface(): void;
  resetInputState(): void;
  resetShellIntegration(): void;
  setShellIntegrationTrusted(trusted: boolean): void;
  terminal: XtermTerminal;
  terminalInlineSshAuthPrompt: ReturnType<
    typeof createTerminalInlineSshAuthPrompt
  >;
}

interface XtermPaneSessionRuntimeDiagnostics {
  sshFailure: ReturnType<
    ReturnType<typeof createSshTerminalFailureTracker>["current"]
  >;
  sshReconnect: ReturnType<
    ReturnType<typeof createTerminalReconnectRuntime>["diagnosticsSnapshot"]
  >;
  sshTarget: boolean;
}

export interface XtermPaneSessionRuntime {
  diagnosticsSnapshot(): XtermPaneSessionRuntimeDiagnostics;
  dispose(): void;
  startInitial(): void;
}

/**
 * 持有单个 pane 的会话 generation、创建、关闭、轮询和重连生命周期。
 *
 * 父 runtime 只组合 controller；所有异步回调都在本 controller 内重新校验
 * generation，避免旧会话输出或关闭结果污染新会话。
 */
export function createXtermPaneSessionRuntime({
  activityRuntime,
  args,
  artifactRuntime,
  assistEnabled,
  commandBlockRuntime,
  currentCwdRef,
  cwd,
  cwdTrackingBufferRef,
  disconnectSessionRef,
  env,
  focusedRef,
  ghostSuggestions,
  instrumentation,
  onAgentSignalRef,
  onCurrentCwd,
  onSessionFinishedRef,
  outputHistoryBuffer,
  outputWriter,
  paneId,
  paneResizeController,
  profileId,
  reconnectSessionRef,
  remoteCommand,
  remoteHostId,
  remoteSuggestionPrewarm,
  refreshSurface,
  resetInputState,
  resetShellIntegration,
  sessionIdRef,
  setConnectionState: setReactConnectionState,
  setLogNotice,
  setLogState,
  setShellIntegrationTrusted,
  shell,
  startupMessage,
  tabIdRef,
  target,
  terminal,
  terminalAppearanceRef,
  terminalInlineSshAuthPrompt,
  transientStartupMessage,
  visibleRef,
}: CreateXtermPaneSessionRuntimeOptions): XtermPaneSessionRuntime {
  let disposed = false;
  let sessionRun = 0;
  // 活动 session 清空后仍保留最后绑定 id，pane 被关闭时才能注销 disconnected binding。
  let lastBoundSessionId: string | null = null;

  /** 先更新同步输入门禁，再通知 React；读失败保留 binding 时也立即停止整组。 */
  const setConnectionState = (state: ConnectionState): void => {
    activityRuntime.setConnectionState(state);
    setReactConnectionState(state);
    terminalInputSync.refresh();
  };
  const sshFailureTracker = createSshTerminalFailureTracker();
  const isSshTerminalTarget = () =>
    Boolean(
      remoteHostId &&
        target?.kind !== "dockerContainer" &&
        target?.kind !== "telnet" &&
        target?.kind !== "serial",
    );
  const hasRemoteTerminalTarget = () =>
    Boolean(
      remoteHostId ||
        target?.kind === "dockerContainer" ||
        target?.kind === "telnet" ||
        target?.kind === "serial",
    );

  const clearSessionState = (sessionId: string) => {
    if (sessionIdRef.current === sessionId) {
      sessionIdRef.current = null;
    }
    paneResizeController.clearSession(sessionId);
    unregisterTerminalPaneSession(paneId, sessionId, { preserveBinding: true });
    terminalInputSync.refresh();
    resetShellIntegration();
    commandBlockRuntime.resetProtocolState();
    setLogState({ active: false, bytesWritten: 0 });
  };

  const reconnectRuntime = createTerminalReconnectRuntime({
    isSshTerminalTarget,
    outputWriter,
    readAutoReconnect: () => terminalAppearanceRef.current.autoReconnect,
    sshFailureTracker,
    startSession: () => void startSession("reconnect"),
    window,
  });

  const closeActiveSession = async () => {
    const sessionId = sessionIdRef.current;
    if (!sessionId) {
      return true;
    }
    sessionStatusPoll.clear();
    markTerminalPaneSessionDisconnected(paneId, sessionId);
    clearSessionState(sessionId);
    try {
      await closeTerminal(sessionId);
      return true;
    } catch (error: unknown) {
      if (!disposed) {
        outputWriter.writeNow(
          `\r\n关闭终端会话失败：${errorMessage(error)}\r\n`,
        );
        setConnectionState("error");
      }
      return false;
    }
  };

  const finishSessionClosed = (
    sessionId: string,
    sessionStartedAtMs: number,
    currentRun: number,
    message = "\r\n会话已结束。\r\n",
  ) => {
    if (
      disposed ||
      sessionRun !== currentRun ||
      sessionIdRef.current !== sessionId
    ) {
      return;
    }
    sessionStatusPoll.clear();
    ghostSuggestions.clearGhostSuggestion();
    markTerminalPaneSessionDisconnected(paneId, sessionId);
    clearSessionState(sessionId);
    onSessionFinishedRef.current?.({
      durationMs: Math.max(0, Date.now() - sessionStartedAtMs),
      reason: "closed",
      sessionId,
    });
    outputWriter.writeNow(
      isSshTerminalTarget()
        ? formatSshTerminalFailureMessage(sshFailureTracker.current(), message)
        : message,
    );
    setConnectionState("closed");
    reconnectRuntime.scheduleReconnect();
  };

  const sessionStatusPoll = createTerminalSessionStatusPollController({
    isCurrent: ({ currentRun, sessionId }) =>
      !disposed &&
      sessionRun === currentRun &&
      sessionIdRef.current === sessionId,
    onSessionClosed: (
      { currentRun, sessionId, sessionStartedAtMs },
      message,
    ) =>
      finishSessionClosed(
        sessionId,
        sessionStartedAtMs,
        currentRun,
        message,
      ),
  });

  /**
   * 开启新的 session generation 前撤销本 pane 旧的主机身份确认，避免重连和关闭竞态把晚到结果接回新会话。
   */
  const startSession = async (reason: "initial" | "reconnect") => {
    const currentRun = ++sessionRun;
    cancelSshHostKeyPromptsForOwner(paneId);
    artifactRuntime.invalidate("restart");
    const closed = await closeActiveSession();
    if (!closed || disposed || sessionRun !== currentRun) {
      return;
    }

    setConnectionState(reason === "reconnect" ? "reconnecting" : "connecting");
    terminalSuggestionProbeScheduler.cancelOwner(paneId);
    setLogNotice(null);
    resetInputState();
    resetShellIntegration();
    commandBlockRuntime.resetProtocolState();
    ghostSuggestions.clearGhostSuggestion();
    sshFailureTracker.reset();
    const startupNotice = terminalSessionStartupNotice(
      reason,
      target,
      remoteHostId,
      startupMessage,
    );
    const transientStartupNoticeVisible =
      reason === "initial" &&
      startupNotice.trim().length > 0 &&
      (transientStartupMessage || hasRemoteTerminalTarget());
    outputWriter.writeNow(startupNotice);
    const sessionStartedAtMs = Date.now();
    const initialRemoteOutputGate =
      createInitialRemoteOutputGate(sessionStartedAtMs);
    const handleOutput = createTerminalSessionOutputController({
      activityRuntime,
      artifactRuntime,
      assistEnabled,
      commandBlockRuntime,
      cwdTrackingBufferRef,
      focusedRef,
      hasRemoteTerminalTarget: hasRemoteTerminalTarget(),
      initialRemoteOutputGate,
      instrumentation,
      isCurrent: () => !disposed && sessionRun === currentRun,
      isSshTerminalTarget: isSshTerminalTarget(),
      onAgentSignal: (signal: TerminalAgentSignal) => {
        const interactiveTuiActive = signal.status !== "exited";
        commandBlockRuntime.setInteractiveTuiActive(interactiveTuiActive);
        ghostSuggestions.clearGhostSuggestion();
        onAgentSignalRef.current?.(signal);
      },
      onCurrentCwd,
      onReadError: (event) => {
        ghostSuggestions.clearGhostSuggestion();
        sessionStatusPoll.clear();
        markTerminalPaneSessionDisconnected(paneId, event.sessionId);
        outputWriter.writeNow(
          `\r\n终端输出读取失败：${event.data}\r\n`,
        );
        setConnectionState("error");
        reconnectRuntime.scheduleReconnect();
      },
      onSessionClosed: (sessionId) =>
        finishSessionClosed(sessionId, sessionStartedAtMs, currentRun),
      outputHistoryBuffer,
      outputWriter,
      remoteCwdTracking: Boolean(
        remoteHostId || target?.kind === "dockerContainer",
      ),
      sshFailureTracker,
      transientStartupNoticeVisible,
      visibleRef,
    });

    try {
      const requestedDimensions = {
        cols: terminal.cols,
        rows: terminal.rows,
      };
      const session = await createXtermPaneTerminalSession({
        args,
        cols: requestedDimensions.cols,
        currentCwd: currentCwdRef.current,
        cwd,
        env,
        isPromptOwnerActive: () =>
          !disposed && sessionRun === currentRun,
        onOutput: handleOutput,
        promptOwnerId: paneId,
        promptForSecret: terminalInlineSshAuthPrompt.promptForSecret,
        remoteCommand,
        remoteHostId,
        rows: requestedDimensions.rows,
        shell,
        target,
      });
      if (disposed || sessionRun !== currentRun) {
        void closeTerminal(session.id);
        return;
      }
      const shellIntegrationTrusted =
        !hasRemoteTerminalTarget() &&
        session.shellIntegration?.status === "enabled";
      setShellIntegrationTrusted(shellIntegrationTrusted);
      sessionIdRef.current = session.id;
      paneResizeController.bindSession(session.id, requestedDimensions);
      registerTerminalPaneSession(paneId, session.id, {
        containerId:
          target?.kind === "dockerContainer" ? target.containerId : undefined,
        containerRuntime:
          target?.kind === "dockerContainer"
            ? (target.runtime ?? "docker")
            : undefined,
        cwd: currentCwdRef.current ?? cwd,
        profileId,
        remoteHostId:
          target?.kind === "dockerContainer" ||
          target?.kind === "telnet" ||
          target?.kind === "serial"
            ? target.hostId
            : remoteHostId,
        shell,
        tabId: tabIdRef.current,
        target: terminalSessionTargetKind(target, remoteHostId),
        targetRef: session.targetRef,
        targetToken: session.targetToken,
      });
      lastBoundSessionId = session.id;
      if (reason === "reconnect") {
        markTerminalPaneSessionReconnected(paneId, session.id);
      }
      setConnectionState("connected");
      reconnectRuntime.resetSshReconnectAttempts();
      sessionStatusPoll.schedule({
        currentRun,
        sessionId: session.id,
        sessionStartedAtMs,
      });
      if (assistEnabled) {
        remoteSuggestionPrewarm.scheduleGit(currentCwdRef.current ?? cwd);
        remoteSuggestionPrewarm.scheduleRemoteCommand();
        remoteSuggestionPrewarm.scheduleRemoteHistory();
        remoteSuggestionPrewarm.scheduleRemotePath(currentCwdRef.current ?? cwd);
      }
      void getTerminalLogState(session.id)
        .then((nextState: TerminalSessionLogState) => {
          if (
            !disposed &&
            sessionRun === currentRun &&
            sessionIdRef.current === session.id
          ) {
            setLogState(nextState);
          }
        })
        .catch(() => {
          if (
            !disposed &&
            sessionRun === currentRun &&
            sessionIdRef.current === session.id
          ) {
            setLogState({ active: false, bytesWritten: 0 });
          }
        });
      paneResizeController.requestCurrentDimensions();
      refreshSurface();
      if (focusedRef.current) {
        terminal.focus();
      }
    } catch (error: unknown) {
      if (disposed || sessionRun !== currentRun) {
        return;
      }
      outputWriter.writeNow(
        `\r\n${terminalSessionFailureLabel(target, remoteHostId)}：${errorMessage(error)}\r\n`,
      );
      setConnectionState("error");
    }
  };

  /**
   * 主动断开也必须撤销当前 pane 的 host-key 确认；否则用户取消连接后仍可能完成信任写入。
   */
  const disconnectSession = async () => {
    const currentRun = ++sessionRun;
    cancelSshHostKeyPromptsForOwner(paneId);
    reconnectRuntime.clearReconnectTimer();
    sessionStatusPoll.clear();
    terminalSuggestionProbeScheduler.cancelOwner(paneId);
    const sessionId = sessionIdRef.current;
    if (!sessionId) {
      setConnectionState("disconnected");
      outputWriter.writeNow(
        "\r\n当前没有活动会话，可通过右键菜单重新连接。\r\n",
      );
      return;
    }
    markTerminalPaneSessionDisconnected(paneId, sessionId);
    clearSessionState(sessionId);
    setLogNotice(null);
    resetInputState();
    ghostSuggestions.clearGhostSuggestion();
    try {
      await closeTerminal(sessionId);
      if (disposed || sessionRun !== currentRun) {
        return;
      }
      setConnectionState("disconnected");
      outputWriter.writeNow(
        "\r\n连接已断开，可通过右键菜单重新连接。\r\n",
      );
    } catch (error: unknown) {
      if (disposed || sessionRun !== currentRun) {
        return;
      }
      outputWriter.writeNow(
        `\r\n断开连接失败：${errorMessage(error)}\r\n`,
      );
      setConnectionState("error");
    }
  };

  reconnectSessionRef.current = async () => {
    await startSession("reconnect");
    if (!sessionIdRef.current) {
      throw new Error("终端重连未建立新的会话");
    }
  };
  registerTerminalPaneReconnectHandler(paneId, async () => {
    await reconnectSessionRef.current?.();
  });
  disconnectSessionRef.current = disconnectSession;

  return {
    diagnosticsSnapshot: () => ({
      sshFailure: isSshTerminalTarget()
        ? sshFailureTracker.current()
        : undefined,
      sshReconnect: reconnectRuntime.diagnosticsSnapshot(),
      sshTarget: isSshTerminalTarget(),
    }),
    /** 销毁 pane 时同步撤销 host-key 队列项，确保等待中的恢复 Promise 只能以取消结束。 */
    dispose() {
      disposed = true;
      sessionRun += 1;
      cancelSshHostKeyPromptsForOwner(paneId);
      reconnectRuntime.clearReconnectTimer();
      sessionStatusPoll.clear();
      terminalSuggestionProbeScheduler.cancelOwner(paneId);
      reconnectSessionRef.current = null;
      disconnectSessionRef.current = null;
      unregisterTerminalPaneReconnectHandler(paneId);
      const sessionId = sessionIdRef.current;
      const boundSessionId = sessionId ?? lastBoundSessionId;
      sessionIdRef.current = null;
      lastBoundSessionId = null;
      if (boundSessionId) {
        paneResizeController.clearSession(boundSessionId);
        unregisterTerminalPaneSession(paneId, boundSessionId);
      }
      terminalInputSync.refresh();
      if (sessionId) {
        void closeTerminal(sessionId);
      }
    },
    startInitial() {
      void startSession("initial");
    },
  };
}
