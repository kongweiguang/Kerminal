// @author kongweiguang

import type { Dispatch, MutableRefObject, SetStateAction } from "react";
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import {
  listTerminalSuggestions,
  recordTerminalSuggestionFeedback,
} from "../../lib/terminalSuggestionApi";
import type { CommandHistoryTarget } from "../../lib/commandHistoryApi";
import type { AppSettings } from "../settings/contracts/index";
import type { TerminalPane } from "../workspace/contracts/index";
import { requestSnippetPanelOpen } from "../snippets/contracts/index";
import {
  terminalSuggestionEligibility,
  updateTerminalInputBufferKind,
  type TerminalInputModelState,
} from "./terminalInputModel";
import {
  resolveGhostSuggestionLayout,
  terminalGhostSuggestionEqual,
  terminalSuggestionProviders,
  type TerminalGhostSuggestion,
} from "./XtermPane.helpers";
import { resolveTerminalSuggestionProbePolicy } from "./terminalSuggestionProbePolicy";
import { createTerminalSuggestionController } from "./terminalSuggestionController";
import { createTerminalSuggestionRuntimeBridge } from "./terminalSuggestionRuntimeBridge";
import type {
  TerminalSuggestionLifecycle,
  TerminalSuggestionQuery,
} from "./terminalSuggestionModel";
import type { TerminalSuggestionKeyEvent } from "./terminalSuggestionKeyPolicy";
import {
  createTerminalSuggestionMenuState,
  reduceTerminalSuggestionMenuState,
  resolveTerminalSuggestionMenuKeyIntent,
  TERMINAL_SUGGESTION_MENU_MAX_ITEMS,
  type TerminalSuggestionMenuIntent,
  type TerminalSuggestionMenuState,
} from "./terminalSuggestionMenuModel";
import type {
  TerminalSuggestionMenuAnchor,
  TerminalSuggestionPaneSize,
} from "./terminalSuggestionMenuPosition";

export interface XtermPaneSuggestionMenuView {
  anchor: TerminalSuggestionMenuAnchor;
  paneSize: TerminalSuggestionPaneSize;
  state: TerminalSuggestionMenuState;
}

function resolveSuggestionTarget({
  remoteHostId,
  target,
}: {
  remoteHostId?: string;
  target: TerminalPane["target"];
}) {
  const containerHostId =
    target?.kind === "dockerContainer" ? target.hostId : undefined;
  const telnetHostId = target?.kind === "telnet" ? target.hostId : undefined;
  const serialHostId = target?.kind === "serial" ? target.hostId : undefined;
  const sshHostId = telnetHostId || serialHostId ? undefined : remoteHostId;
  const remoteHostTargetId =
    containerHostId ?? telnetHostId ?? serialHostId ?? sshHostId;
  const suggestionTarget: CommandHistoryTarget = containerHostId
    ? "dockerContainer"
    : telnetHostId
      ? "telnet"
      : serialHostId
        ? "serial"
        : sshHostId
          ? "ssh"
          : "local";

  return {
    remoteHostId: remoteHostTargetId,
    target: suggestionTarget,
  };
}

export function createXtermPaneGhostSuggestions({
  assistEnabled,
  canScheduleSuggestion = () => true,
  commitUserInput,
  container,
  currentCwdRef,
  cwd,
  ghostSuggestionRef,
  inputModelRef,
  inputCompatibilityMode,
  isDisposed,
  paneId,
  profileId,
  remoteHostId,
  scheduleCommandBlockViewSync,
  sessionIdRef,
  setGhostSuggestion,
  setSuggestionMenu,
  shell,
  target,
  terminal,
  terminalAppearanceRef,
}: {
  assistEnabled: boolean;
  canScheduleSuggestion?: () => boolean;
  commitUserInput: (data: string) => void;
  container: HTMLDivElement;
  currentCwdRef: MutableRefObject<string | undefined>;
  cwd?: string;
  ghostSuggestionRef: MutableRefObject<TerminalGhostSuggestion | null>;
  inputModelRef: MutableRefObject<TerminalInputModelState>;
  inputCompatibilityMode: "agentTui" | "shell";
  isDisposed: () => boolean;
  paneId: string;
  profileId?: string;
  remoteHostId?: string;
  scheduleCommandBlockViewSync: () => void;
  sessionIdRef: MutableRefObject<string | null>;
  setGhostSuggestion: Dispatch<SetStateAction<TerminalGhostSuggestion | null>>;
  setSuggestionMenu: Dispatch<
    SetStateAction<XtermPaneSuggestionMenuView | null>
  >;
  shell?: string;
  target: TerminalPane["target"];
  terminal: XtermTerminal;
  terminalAppearanceRef: MutableRefObject<AppSettings["terminal"]>;
}) {
  let suggestionRequestRun = 0;
  let suggestionTimer: number | null = null;
  let consecutiveSuggestionFailures = 0;
  let inputBurstCount = 0;
  let lastInputAt: number | undefined;
  let lastSuggestionDurationMs: number | undefined;
  let menuRequested = false;
  let menuState = createTerminalSuggestionMenuState();
  let latestLifecycle: TerminalSuggestionLifecycle = {
    alternateScreen: terminal.buffer.active.type === "alternate",
    enabled: assistEnabled,
    hidden: !canScheduleSuggestion(),
    imeComposing: false,
    inputCompatibilityMode,
    pasting: false,
    searchFocused: false,
    selectionActive: false,
    sessionOpen: Boolean(sessionIdRef.current),
  };

  const clearSuggestionTimer = () => {
    if (suggestionTimer !== null) {
      window.clearTimeout(suggestionTimer);
      suggestionTimer = null;
    }
  };

  const updateGhostSuggestion = (suggestion: TerminalGhostSuggestion) => {
    ghostSuggestionRef.current = suggestion;
    setGhostSuggestion((current) =>
      terminalGhostSuggestionEqual(current, suggestion) ? current : suggestion,
    );
  };

  const hideGhostSuggestion = () => {
    ghostSuggestionRef.current = null;
    setGhostSuggestion((current) => (current === null ? current : null));
  };

  const hideSuggestionMenu = () => {
    menuRequested = false;
    menuState = reduceTerminalSuggestionMenuState(menuState, { type: "close" });
    setSuggestionMenu(null);
  };

  const syncSuggestionMenu = () => {
    if (!menuRequested && !menuState.open) {
      return;
    }
    const snapshot = controller.getSnapshot();
    menuState = reduceTerminalSuggestionMenuState(menuState, {
      candidates: snapshot.candidates,
      stale: snapshot.stale,
      type: menuState.open ? "candidates" : "open",
    });
    if (!menuState.open) {
      return;
    }
    const layout = resolveGhostSuggestionLayout(
      container,
      terminal,
      terminalAppearanceRef.current,
      inputModelRef.current,
    );
    const frame = container.parentElement;
    if (!layout || !(frame instanceof HTMLElement)) {
      hideSuggestionMenu();
      return;
    }
    setSuggestionMenu({
      anchor: {
        height: layout.lineHeight,
        x: layout.left,
        y: layout.top,
      },
      paneSize: {
        height: frame.clientHeight,
        width: frame.clientWidth,
      },
      state: menuState,
    });
  };

  const suggestionTarget = resolveSuggestionTarget({ remoteHostId, target });
  const requestSuggestions = (
    query: TerminalSuggestionQuery,
    signal: AbortSignal,
  ) => {
    if (signal.aborted) {
      return Promise.resolve([]);
    }
    return listTerminalSuggestions(query.request).then((candidates) =>
      candidates.map((candidate) => ({
        ...candidate,
        acceptBoundaries: candidate.acceptBoundaries ?? [],
        allowedPresentations:
          candidate.allowedPresentations ??
          (candidate.sensitivity === "normal" ? ["inline", "menu"] : ["menu"]),
      })),
    );
  };
  const controller = createTerminalSuggestionController({
    debounceMs: 0,
    onFeedback: ({ candidate, input, kind }) => {
      const sessionId = sessionIdRef.current;
      void recordTerminalSuggestionFeedback({
        action: kind === "dismissed" ? "dismissed" : "accepted",
        cwd: currentCwdRef.current ?? cwd,
        input,
        paneId,
        profileId,
        provider: candidate.provider,
        remoteHostId: suggestionTarget.remoteHostId,
        replacementText: candidate.replacementText,
        sessionId: sessionId ?? undefined,
        shell,
        sourceId:
          candidate.provider === "snippet" ? candidate.id : candidate.sourceId,
        target: suggestionTarget.target,
      }).catch(() => undefined);
    },
    paneId,
    requestSuggestions,
  });
  const bridge = createTerminalSuggestionRuntimeBridge(controller);

  const syncGhostFromController = () => {
    const candidate = controller.getSnapshot().inlineCandidate;
    if (!candidate?.suffix) {
      hideGhostSuggestion();
      return;
    }
    const layout = resolveGhostSuggestionLayout(
      container,
      terminal,
      terminalAppearanceRef.current,
      inputModelRef.current,
    );
    if (!layout) {
      hideGhostSuggestion();
      return;
    }
    updateGhostSuggestion({ ...layout, candidate, suffix: candidate.suffix });
  };
  const syncFromController = () => {
    syncGhostFromController();
    syncSuggestionMenu();
  };
  const unsubscribeController = controller.subscribe(syncFromController);

  const clearGhostSuggestion = () => {
    clearSuggestionTimer();
    suggestionRequestRun += 1;
    controller.clear();
    hideGhostSuggestion();
    hideSuggestionMenu();
  };

  const refreshGhostSuggestionLayout = () => {
    syncGhostFromController();
  };

  const scheduleGhostSuggestion = () => {
    if (!assistEnabled) {
      clearGhostSuggestion();
      return;
    }
    const now = Date.now();
    inputBurstCount =
      typeof lastInputAt === "number" && now - lastInputAt <= 220
        ? inputBurstCount + 1
        : 1;
    lastInputAt = now;
    const policy = resolveTerminalSuggestionProbePolicy({
      consecutiveFailures: consecutiveSuggestionFailures,
      inputBurstCount,
      lastFailureAt: undefined,
      lastInputAt,
      lastProbeDurationMs: lastSuggestionDurationMs,
      lifecycleEnabled: canScheduleSuggestion(),
      lifecycleReason: "lifecycle-gate",
      now,
    });
    if (!policy.shouldSchedule) {
      clearGhostSuggestion();
      return;
    }
    clearSuggestionTimer();
    const inlineSuggestion = terminalAppearanceRef.current.inlineSuggestion;
    if (!inlineSuggestion.enabled) {
      clearGhostSuggestion();
      return;
    }
    inputModelRef.current = updateTerminalInputBufferKind(
      inputModelRef.current,
      terminal.buffer.active.type,
    );
    const eligibility = terminalSuggestionEligibility(inputModelRef.current);
    if (!eligibility.eligible) {
      clearGhostSuggestion();
      return;
    }
    suggestionTimer = window.setTimeout(() => {
      suggestionTimer = null;
      if (!canScheduleSuggestion()) {
        clearGhostSuggestion();
        return;
      }
      const requestRun = ++suggestionRequestRun;
      const requestStartedAt = Date.now();
      const model = inputModelRef.current;
      const layout = resolveGhostSuggestionLayout(
        container,
        terminal,
        terminalAppearanceRef.current,
        model,
      );
      if (!layout) {
        clearGhostSuggestion();
        return;
      }
      const sessionId = sessionIdRef.current;
      const suggestionProviders = terminalSuggestionProviders({
        hasSshRemote: Boolean(
          suggestionTarget.target === "ssh" && !target?.kind,
        ),
        inlineSuggestion: terminalAppearanceRef.current.inlineSuggestion,
      });
      if (suggestionProviders.length === 0) {
        clearGhostSuggestion();
        return;
      }
      latestLifecycle = {
        ...latestLifecycle,
        alternateScreen: model.bufferKind === "alternate",
        enabled: eligibility.eligible && inlineSuggestion.enabled,
        hidden: !canScheduleSuggestion(),
        imeComposing: model.imeComposing,
        pasting: model.hideReason === "paste",
        sessionOpen: Boolean(sessionId),
      };
      bridge.sync(
        {
          contextKey: [
            suggestionTarget.target,
            suggestionTarget.remoteHostId ?? "",
            currentCwdRef.current ?? cwd ?? "",
            shell ?? "",
          ].join("\u0000"),
          cursor: model.cursor,
          input: model.command,
          request: {
            cwd: currentCwdRef.current ?? cwd,
            limit: 1,
            profileId,
            providers: suggestionProviders,
            remoteHostId: suggestionTarget.remoteHostId,
            sessionId: sessionId ?? undefined,
            shell,
            target: suggestionTarget.target,
          },
        },
        latestLifecycle,
      );
      lastSuggestionDurationMs = Date.now() - requestStartedAt;
      consecutiveSuggestionFailures = 0;
      if (!isDisposed() && requestRun === suggestionRequestRun) {
        syncGhostFromController();
      }
    }, policy.delayMs);
  };

  const openSuggestionMenu = () => {
    const inlineSuggestion = terminalAppearanceRef.current.inlineSuggestion;
    if (
      !assistEnabled ||
      inlineSuggestion.presentation !== "inlineAndMenu" ||
      !canScheduleSuggestion()
    ) {
      return false;
    }
    inputModelRef.current = updateTerminalInputBufferKind(
      inputModelRef.current,
      terminal.buffer.active.type,
    );
    const model = inputModelRef.current;
    const eligibility = terminalSuggestionEligibility(model);
    const sessionId = sessionIdRef.current;
    if (!eligibility.eligible || !sessionId) {
      return false;
    }
    const providers = terminalSuggestionProviders({
      hasSshRemote: Boolean(
        suggestionTarget.target === "ssh" && !target?.kind,
      ),
      inlineSuggestion,
    });
    if (providers.length === 0) {
      return false;
    }
    menuRequested = true;
    bridge.sync(
      {
        contextKey: [
          suggestionTarget.target,
          suggestionTarget.remoteHostId ?? "",
          currentCwdRef.current ?? cwd ?? "",
          shell ?? "",
        ].join("\u0000"),
        cursor: model.cursor,
        input: model.command,
        mode: "menu",
        request: {
          cwd: currentCwdRef.current ?? cwd,
          limit: TERMINAL_SUGGESTION_MENU_MAX_ITEMS,
          profileId,
          providers,
          remoteHostId: suggestionTarget.remoteHostId,
          sessionId,
          shell,
          target: suggestionTarget.target,
        },
      },
      {
        ...latestLifecycle,
        alternateScreen: model.bufferKind === "alternate",
        enabled: true,
        imeComposing: model.imeComposing,
        sessionOpen: true,
      },
    );
    syncSuggestionMenu();
    return true;
  };

  const recordGhostSuggestionFeedback = (
    action: "accepted" | "dismissed",
    suggestion: TerminalGhostSuggestion,
    input: string,
  ) => {
    const sessionId = sessionIdRef.current;
    const suggestionTarget = resolveSuggestionTarget({ remoteHostId, target });
    const candidate = suggestion.candidate;
    void recordTerminalSuggestionFeedback({
      action,
      cwd: currentCwdRef.current ?? cwd,
      input,
      paneId,
      profileId,
      provider: candidate.provider,
      remoteHostId: suggestionTarget.remoteHostId,
      replacementText: candidate.replacementText,
      sessionId: sessionId ?? undefined,
      shell,
      sourceId: candidate.sourceId,
      target: suggestionTarget.target,
    }).catch(() => undefined);
  };

  let committingSuggestion = false;

  /** 接受控制器已推进候选；同步提交期间不让输入观察器重新清空剩余候选。 */
  const commitAcceptedInput = (data: string): void => {
    const previous = committingSuggestion;
    committingSuggestion = true;
    try {
      commitUserInput(data);
    } finally {
      committingSuggestion = previous;
    }
  };

  /** 接受候选统一提交实际插入字节，由输入事件边界更新模型并分发。 */
  const acceptGhostSuggestion = (_sessionId: string) => {
    if (terminalAppearanceRef.current.inlineSuggestion.acceptKey !== "rightArrow") {
      return false;
    }
    const acceptance = controller.accept("all");
    if (!acceptance) {
      return false;
    }
    commitAcceptedInput(acceptance.insertedText);
    clearGhostSuggestion();
    scheduleCommandBlockViewSync();
    return true;
  };

  /** 键盘接受候选不重复写 PTY，也不自行推进输入模型。 */
  const handleKeyEvent = (
    event: TerminalSuggestionKeyEvent,
    _sessionId: string,
  ) => {
    if (
      event.altKey &&
      !terminalAppearanceRef.current.inlineSuggestion.partialAccept
    ) {
      return false;
    }
    const result = bridge.handleKey(event);
    if (!result.handled || !result.acceptance) {
      return false;
    }
    commitAcceptedInput(result.acceptance.insertedText);
    hideGhostSuggestion();
    scheduleCommandBlockViewSync();
    return true;
  };

  /** 菜单接受沿用同一人工输入入口，保留候选选择与菜单生命周期。 */
  const handleMenuIntent = (
    intent: TerminalSuggestionMenuIntent,
    _sessionId: string,
  ) => {
    if (intent.type === "open") {
      return openSuggestionMenu();
    }
    if (intent.type === "close") {
      hideSuggestionMenu();
      return true;
    }
    if (intent.type === "move") {
      menuState = reduceTerminalSuggestionMenuState(menuState, {
        index: intent.index,
        type: "select",
      });
      syncSuggestionMenu();
      return true;
    }
    if (intent.type === "openSnippetPanel") {
      const snippetId = intent.candidate.sourceId;
      if (!snippetId) {
        hideSuggestionMenu();
        return true;
      }
      requestSnippetPanelOpen({ paneId, snippetId });
      hideSuggestionMenu();
      hideGhostSuggestion();
      return true;
    }
    const acceptance = controller.acceptCandidate(intent.candidate, "all");
    if (!acceptance) {
      return false;
    }
    commitAcceptedInput(acceptance.insertedText);
    hideSuggestionMenu();
    hideGhostSuggestion();
    scheduleCommandBlockViewSync();
    return true;
  };

  const handleMenuKeyEvent = (
    event: TerminalSuggestionKeyEvent,
    sessionId: string,
  ) => {
    const inlineSuggestion = terminalAppearanceRef.current.inlineSuggestion;
    if (
      event.key === "Tab" &&
      inlineSuggestion.tabOpensMenu &&
      !menuState.open
    ) {
      return openSuggestionMenu();
    }
    const intent = resolveTerminalSuggestionMenuKeyIntent(menuState, event);
    return intent ? handleMenuIntent(intent, sessionId) : false;
  };

  const handleRuntimeKeyEvent = (
    event: TerminalSuggestionKeyEvent,
    sessionId: string,
  ) =>
    handleMenuKeyEvent(event, sessionId) || handleKeyEvent(event, sessionId);

  const setLifecycle = (next: Partial<TerminalSuggestionLifecycle>) => {
    latestLifecycle = { ...latestLifecycle, ...next };
    bridge.setLifecycle(latestLifecycle);
    syncGhostFromController();
  };

  return {
    acceptGhostSuggestion,
    isCommittingSuggestion: () => committingSuggestion,
    clearGhostSuggestion,
    dispose: () => {
      clearSuggestionTimer();
      hideSuggestionMenu();
      unsubscribeController();
      bridge.dispose();
    },
    handleMenuIntent,
    handleRuntimeKeyEvent,
    handleKeyEvent,
    recordGhostSuggestionFeedback,
    refreshGhostSuggestionLayout,
    scheduleGhostSuggestion,
    setLifecycle,
  };
}
