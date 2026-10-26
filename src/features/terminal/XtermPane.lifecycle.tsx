// @author kongweiguang
import { useCallback, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type MouseEvent } from "react";
import type { FitAddon } from "@xterm/addon-fit";
import type { SearchAddon } from "@xterm/addon-search";
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import {
  startTerminalLog,
  stopTerminalLog,
  writeTerminal,
  type TerminalSessionLogState,
} from "../../lib/terminalApi";
import { writeDesktopClipboardText } from "../../lib/desktopClipboardApi";
import type { TerminalCommandBlockAction } from "./TerminalCommandBlockRail";
import {
  splitDirectionForMenuAction,
  type TerminalContextMenuAction,
} from "./TerminalContextMenu";
import {
  buildTerminalCommandBlockViews,
  commandBlockViewsEqual,
  copyTerminalCommandBlockAsImage,
  terminalCommandBlockPlainText,
  type TerminalCommandBlock,
  type TerminalCommandBlockView,
} from "./terminalCommandBlocks";
import { useXtermPanePromptBridge } from "./XtermPane.promptSourceRegistry";
import {
  clearTerminalCommandBlocks,
  syncTerminalCommandPromptBlocks,
} from "./terminalCommandBlockLifecycle";
import {
  createTerminalInputModelState,
  type TerminalInputModelState,
} from "./terminalInputModel";
import {
  applyTerminalCommandBlockFolding,
  errorMessage,
  formatLogPath,
  pasteIntoTerminal,
  resolveTerminalContentBottomLine,
  resolveTerminalPromptLine,
  resolveTerminalRowHeight,
  type ConnectionState,
  type TerminalGhostSuggestion,
} from "./XtermPane.helpers";
import { installXtermPaneRuntime } from "./XtermPane.runtime";
import { type XtermPaneActivityRuntime } from "./XtermPane.activityRuntime";
import {
  EMPTY_TERMINAL_PANE_CHROME_SNAPSHOT,
  terminalChromeRuntimeStore,
} from "./terminalChromeRuntimeStore";
import {
  createTerminalPaneRuntimeLifecycleRuntime,
  type TerminalPaneRuntimeLifecycleRuntime,
} from "./terminalPaneRuntimeLifecycleRuntime";
import type { TerminalRendererController } from "./terminalRenderer";
import { terminalRendererRegistry } from "./terminalRendererRegistry";
import { resolveSafeTerminalRendererType } from "./terminalRendererPlatform";
import { terminalSuggestionProbeScheduler } from "./terminalSuggestionProbeScheduler";
import { useTransientTerminalNotice } from "./useTransientTerminalNotice";
import { useXtermPaneSuggestionMenu } from "./useXtermPaneSuggestionMenu";
import {
  useTerminalKeywordHighlightHotUpdate,
  useTerminalWebLinkDecorationHotUpdate,
  useXtermPaneAppearanceRuntime,
  useXtermPaneTerminalOptionsHotUpdate,
} from "./useXtermPaneAppearanceRuntime";
import { useXtermPaneSearch } from "./XtermPane.search";
import { requestAgentSend } from "../agent-workflow/state/index";
import {
  updateTerminalPaneRuntimeContext,
  updateTerminalPaneSessionTabId,
} from "./terminalSessionRegistry";
import {
  XtermPaneView,
  type XtermPaneContextMenuState,
} from "./XtermPane.view";
import type { XtermPaneProps } from "./XtermPane.types";

const TERMINAL_CLEAR_SCREEN_INPUT = "\x0c";
const TERMINAL_FRONTEND_CLEAR_SCREEN_SEQUENCE = "\x1b[H\x1b[2J\x1b[3J";
/**
 * 管理单个 xterm 会话及其渲染生命周期；背景图仅改变 canvas 底色透明度，
 * 不能重建 PTY 或终端实例，否则切换外观会丢失正在运行的会话。
 */
export function XtermPane({
  args,
  backgroundImageVisible = false,
  currentCwd,
  cwd,
  enableAgentSendActions = true,
  env,
  focusRequestToken,
  focused,
  inputCompatibilityMode = "shell",
  inputRequest,
  paneId,
  profileId,
  remoteCommand,
  remoteHostId,
  resolvedTheme,
  shell,
  shellAssistEnabled = true,
  startupMessage,
  tabId,
  terminalAppearance,
  terminalColorSchemeOverride,
  target,
  title,
  transientStartupMessage = false,
  visible = true,
  onAgentSignal,
  onCurrentCwdChange,
  onConnectionStateChange,
  onOpenLogs,
  onOutputHistoryChange,
  onSessionFinished,
  onSplitPane,
  onTerminalDimensionsChange,
  outputHistory,
  resolveInitialOutputHistory,
}: XtermPaneProps) {
  const commandBlockCounterRef = useRef(0);
  const commandBlocksRef = useRef<TerminalCommandBlock[]>([]);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const disconnectSessionRef = useRef<(() => Promise<void>) | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const focusedRef = useRef(focused);
  const inputBufferRef = useRef("");
  const inputModelRef = useRef<TerminalInputModelState>(
    createTerminalInputModelState(),
  );
  const cwdTrackingBufferRef = useRef("");
  const currentCwdRef = useRef(currentCwd ?? cwd);
  const ghostSuggestionRef = useRef<TerminalGhostSuggestion | null>(null);
  const onAgentSignalRef = useRef(onAgentSignal);
  const onCurrentCwdChangeRef = useRef(onCurrentCwdChange);
  const onConnectionStateChangeRef = useRef(onConnectionStateChange);
  const onOutputHistoryChangeRef = useRef(onOutputHistoryChange);
  const onSessionFinishedRef = useRef(onSessionFinished);
  const onTerminalDimensionsChangeRef = useRef(onTerminalDimensionsChange);
  const outputHistoryRef = useRef(
    outputHistory ?? resolveInitialOutputHistory?.(),
  );
  const promptLineRef = useRef<number | undefined>(undefined);
  const manualClearSyncFrameRef = useRef<number | null>(null);
  const suppressCommandBlockSyncRef = useRef(false);
  const shellIntegrationCommandBlockProtocolRef = useRef(false);
  const reconnectSessionRef = useRef<(() => Promise<void>) | null>(null);
  const searchAddonRef = useRef<SearchAddon | null>(null);
  const sessionIdRef = useRef<string | null>(null);
  const terminalAppearanceRef = useRef(terminalAppearance);
  const tabIdRef = useRef(tabId);
  const terminalRef = useRef<XtermTerminal | null>(null);
  const activityRuntimeRef = useRef<XtermPaneActivityRuntime | null>(null);
  const terminalRendererControllerRef =
    useRef<TerminalRendererController | null>(null);
  const terminalRuntimeLifecycleControllerRef =
    useRef<TerminalPaneRuntimeLifecycleRuntime | null>(null);
  const terminalSurfaceCoordinatorRef =
    useRef<((invalidate?: boolean) => void) | null>(null);
  const visibleRef = useRef(visible);
  const effectiveRendererType = resolveSafeTerminalRendererType(terminalAppearance.rendererType);
  terminalRuntimeLifecycleControllerRef.current ??=
    createTerminalPaneRuntimeLifecycleRuntime({
      activeTab: visible,
      focused,
      rendererType: effectiveRendererType,
      visible,
    });
  const terminalRuntimeLifecycleRef =
    terminalRuntimeLifecycleControllerRef.current.decisionRef;
  const [commandBlockNotice, setCommandBlockNotice] =
    useTransientTerminalNotice();
  const [terminalNotice, setTerminalNotice] = useTransientTerminalNotice();
  const [commandBlockViews, setCommandBlockViews] = useState<
    TerminalCommandBlockView[]
  >([]);
  const [inlineTuiActive, setInlineTuiActive] = useState(false);
  const [contextMenu, setContextMenu] =
    useState<XtermPaneContextMenuState | null>(null);
  const [connectionState, setConnectionState] =
    useState<ConnectionState>("connecting");
  const [logState, setLogState] = useState<TerminalSessionLogState>({
    active: false,
    bytesWritten: 0,
  });
  const [logNotice, setLogNotice] = useState<string | null>(null);
  const [ghostSuggestion, setGhostSuggestion] =
    useState<TerminalGhostSuggestion | null>(null);
  const suggestionMenuRuntime = useXtermPaneSuggestionMenu();
  const search = useXtermPaneSearch({ searchAddonRef, terminalRef });
  const { setSuggestionMenu, suggestionMenuIntentRef } = suggestionMenuRuntime.runtimeParams;
  const openSearch = search.openSearch;
  const setSearchResults = search.setResults;
  const subscribePaneActivity = useCallback(
    (listener: () => void) =>
      terminalChromeRuntimeStore.subscribe(paneId, listener),
    [paneId],
  );
  const readPaneActivity = useCallback(
    () => terminalChromeRuntimeStore.getSnapshot(paneId),
    [paneId],
  );
  const paneActivity = useSyncExternalStore(
    subscribePaneActivity,
    readPaneActivity,
    () => EMPTY_TERMINAL_PANE_CHROME_SNAPSHOT,
  );
  const {
    argsDependencyKey,
    envDependencyKey,
    runtimeInstallParamsRef,
    targetDependencyKey,
    terminalFontWeight,
    terminalKeywordHighlightControllerRef,
    terminalTheme,
    terminalThemeRef,
    terminalWebLinkDecorationControllerRef,
  } = useXtermPaneAppearanceRuntime({
    args,
    backgroundImageVisible,
    env,
    resolvedTheme,
    target,
    terminalAppearance,
    terminalColorSchemeOverride,
  });
  const syncCommandBlockViews = useCallback(() => {
    if (!shellAssistEnabled) {
      setCommandBlockViews((current) => (current.length === 0 ? current : []));
      return;
    }
    if (suppressCommandBlockSyncRef.current) {
      return;
    }
    const terminal = terminalRef.current;
    if (!terminal) {
      setCommandBlockViews([]);
      return;
    }

    const activeBuffer = terminal.buffer.active;
    const useProtocolCommandBlocks =
      shellIntegrationCommandBlockProtocolRef.current;
    const promptLine = useProtocolCommandBlocks
      ? promptLineRef.current
      : resolveTerminalPromptLine(terminal, inputBufferRef.current);
    if (!useProtocolCommandBlocks) {
      promptLineRef.current = promptLine;
    }
    if (
      !useProtocolCommandBlocks &&
      typeof promptLine === "number" &&
      terminal.buffer.active.type === "normal"
    ) {
      syncTerminalCommandPromptBlocks({
        commandBlockCounterRef,
        commandBlocksRef,
        onEndMarkerDispose: syncCommandBlockViews,
        onStartMarkerDispose: syncCommandBlockViews,
        paneId,
        promptLine,
        terminal,
      });
    }
    const nextViews = buildTerminalCommandBlockViews(commandBlocksRef.current, {
      activeBufferType: activeBuffer.type,
      bufferLength: activeBuffer.length,
      cols: terminal.cols,
      contentBottomLine: resolveTerminalContentBottomLine(terminal),
      promptLine,
      rowHeight: resolveTerminalRowHeight(
        containerRef.current,
        terminalAppearanceRef.current,
        terminal,
      ),
      rows: terminal.rows,
      viewportY: activeBuffer.viewportY,
    });

    setCommandBlockViews((current) =>
      commandBlockViewsEqual(current, nextViews) ? current : nextViews,
    );
  }, [paneId, shellAssistEnabled]);

  const scheduleCommandBlockViewsSync = useCallback(() => {
    if (manualClearSyncFrameRef.current !== null) {
      return;
    }
    manualClearSyncFrameRef.current =
      typeof window.requestAnimationFrame === "function"
        ? window.requestAnimationFrame(() => {
            manualClearSyncFrameRef.current = null;
            syncCommandBlockViews();
          })
        : window.setTimeout(() => {
            manualClearSyncFrameRef.current = null;
            syncCommandBlockViews();
          }, 16);
  }, [syncCommandBlockViews]);

  useEffect(
    () => () => {
      const frameId = manualClearSyncFrameRef.current;
      if (frameId === null) {
        return;
      }
      if (typeof window.cancelAnimationFrame === "function") {
        window.cancelAnimationFrame(frameId);
      } else {
        window.clearTimeout(frameId);
      }
      manualClearSyncFrameRef.current = null;
    },
    [],
  );

  const clearCommandBlocks = useCallback(() => {
    suppressCommandBlockSyncRef.current = true;
    try {
      clearTerminalCommandBlocks(commandBlocksRef);
      setCommandBlockViews([]);
      setCommandBlockNotice(null);
    } finally {
      suppressCommandBlockSyncRef.current = false;
    }
  }, [setCommandBlockNotice]);

  useEffect(() => { currentCwdRef.current = currentCwd ?? cwd; }, [currentCwd, cwd]);
  useEffect(() => { onAgentSignalRef.current = onAgentSignal; }, [onAgentSignal]);
  useEffect(() => { onCurrentCwdChangeRef.current = onCurrentCwdChange; }, [onCurrentCwdChange]);
  useEffect(() => { onConnectionStateChangeRef.current = onConnectionStateChange; }, [onConnectionStateChange]);
  useEffect(() => { onConnectionStateChangeRef.current?.(connectionState); }, [connectionState]);
  useEffect(() => { onOutputHistoryChangeRef.current = onOutputHistoryChange; }, [onOutputHistoryChange]);
  useEffect(() => { onSessionFinishedRef.current = onSessionFinished; }, [onSessionFinished]);
  useEffect(() => { onTerminalDimensionsChangeRef.current = onTerminalDimensionsChange; }, [onTerminalDimensionsChange]);
  useEffect(() => {
    if (resolveInitialOutputHistory) {
      return;
    }
    outputHistoryRef.current = outputHistory;
  }, [outputHistory, resolveInitialOutputHistory]);
  useXtermPanePromptBridge({
    commandBlocksRef,
    connectionState,
    inputRequest,
    paneId,
    sessionIdRef,
    terminalRef,
  });
  useEffect(() => {
    ghostSuggestionRef.current = ghostSuggestion;
  }, [ghostSuggestion]);
  /**
   * pane 可以在不同 Tab 间移动而不重建 xterm；单独同步 tab 元数据，避免为了
   * 更新 Agent scope 让 PTY 断开重连，同时覆盖初次注册尚未完成的竞态。
   */
  useEffect(() => {
    tabIdRef.current = tabId;
    updateTerminalPaneSessionTabId(paneId, tabId);
  }, [paneId, tabId]);
  useEffect(
    () =>
      installXtermPaneRuntime({
        args: runtimeInstallParamsRef.current.args,
        activityRuntimeRef,
        commandBlockCounterRef,
        commandBlocksRef,
        containerRef,
        cwd,
        cwdTrackingBufferRef,
        currentCwdRef,
        disconnectSessionRef,
        env: runtimeInstallParamsRef.current.env,
        fitAddonRef,
        focusedRef,
        ghostSuggestionRef,
        inputBufferRef,
        inputModelRef,
        inputCompatibilityMode,
        onAgentSignalRef,
        onCurrentCwdChangeRef,
        onOutputHistoryChangeRef,
        onSessionFinishedRef,
        onTerminalDimensionsChangeRef,
        outputHistoryRef,
        paneId,
        profileId,
        promptLineRef,
        reconnectSessionRef,
        resolvedTheme: runtimeInstallParamsRef.current.resolvedTheme,
        remoteCommand,
        remoteHostId,
        searchAddonRef,
        sessionIdRef,
        setCommandBlockNotice,
        setCommandBlockViews,
        setConnectionState,
        setGhostSuggestion,
        setInlineTuiActive,
        shellAssistEnabled,
        setLogNotice,
        setLogState,
        setSearchResults,
        setTerminalNotice,
        shellIntegrationCommandBlockProtocolRef,
        shell,
        startupMessage,
        tabIdRef,
        setSuggestionMenu,
        suggestionMenuIntentRef,
        syncCommandBlockViews,
        target: runtimeInstallParamsRef.current.target,
        terminalAppearance: runtimeInstallParamsRef.current.terminalAppearance,
        terminalAppearanceRef,
        terminalFontWeight: runtimeInstallParamsRef.current.terminalFontWeight,
        terminalRef,
        terminalKeywordHighlightControllerRef,
        terminalRendererControllerRef,
        terminalRuntimeLifecycleControllerRef,
        terminalRuntimeLifecycleRef,
        terminalSurfaceCoordinatorRef,
        terminalTheme: runtimeInstallParamsRef.current.terminalTheme,
        terminalWebLinkDecorationControllerRef,
        transientStartupMessage,
        visibleRef,
      }),
    [
      argsDependencyKey,
      cwd,
      envDependencyKey,
      inputCompatibilityMode,
      paneId,
      profileId,
      remoteCommand,
      remoteHostId,
      runtimeInstallParamsRef,
      shell,
      shellAssistEnabled,
      setCommandBlockNotice,
      setInlineTuiActive,
      setSearchResults,
      setSuggestionMenu,
      setTerminalNotice,
      startupMessage,
      suggestionMenuIntentRef,
      syncCommandBlockViews,
      targetDependencyKey,
      terminalKeywordHighlightControllerRef,
      terminalWebLinkDecorationControllerRef,
      terminalRuntimeLifecycleRef,
      transientStartupMessage,
    ],
  );

  useEffect(() => {
    focusedRef.current = focused;
    terminalRuntimeLifecycleControllerRef.current?.markFocused(focused);
    terminalRendererRegistry.updatePaneFocus(paneId, focused);
    if (focused) {
      terminalRef.current?.focus();
    }
  }, [focused, paneId]);

  useEffect(() => {
    visibleRef.current = visible;
    activityRuntimeRef.current?.setVisible(visible);
    terminalRuntimeLifecycleControllerRef.current?.markVisible(visible);
    terminalSuggestionProbeScheduler.setOwnerDisabled(
      paneId,
      visible ? null : "hidden-pane",
    );
    if (!visible) {
      terminalRendererControllerRef.current?.suspend();
      terminalRendererRegistry.updatePaneVisibility(paneId, false);
      terminalSurfaceCoordinatorRef.current?.();
      return undefined;
    }

    terminalSurfaceCoordinatorRef.current?.(true);
    return undefined;
  }, [paneId, visible]);

  useEffect(() => {
    if (typeof focusRequestToken === "number") {
      terminalRef.current?.focus();
    }
  }, [focusRequestToken]);

  useXtermPaneTerminalOptionsHotUpdate({
    containerRef,
    effectiveRendererType,
    inputCompatibilityMode,
    terminalAppearance,
    terminalAppearanceRef,
    terminalFontWeight,
    terminalRef,
    terminalRuntimeLifecycleControllerRef,
    terminalSurfaceCoordinatorRef,
    terminalTheme,
    terminalThemeRef,
  });
  useTerminalKeywordHighlightHotUpdate(terminalKeywordHighlightControllerRef, resolvedTheme, terminalAppearance, visible);
  useTerminalWebLinkDecorationHotUpdate(
    terminalWebLinkDecorationControllerRef,
    resolvedTheme,
    terminalTheme,
    visible,
  );

  useEffect(() => {
    if (!contextMenu) {
      return undefined;
    }

    const closeMenu = () => setContextMenu(null);
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        closeMenu();
      }
    };

    window.addEventListener("click", closeMenu);
    window.addEventListener("resize", closeMenu);
    window.addEventListener("keydown", closeOnEscape);

    return () => {
      window.removeEventListener("click", closeMenu);
      window.removeEventListener("resize", closeMenu);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [contextMenu]);
  useLayoutEffect(() => {
    return applyTerminalCommandBlockFolding(
      containerRef.current,
      shellAssistEnabled ? commandBlockViews : [],
    );
  }, [commandBlockViews, shellAssistEnabled]);
  const startLogging = useCallback(async () => {
    const sessionId = sessionIdRef.current;
    if (!sessionId) {
      setLogNotice("会话尚未就绪，无法记录日志");
      return;
    }

    try {
      const nextState = await startTerminalLog(sessionId);
      setLogState(nextState);
      setLogNotice(
        nextState.path
          ? `正在记录日志：${formatLogPath(nextState.path)}`
          : "正在记录日志",
      );
    } catch (error: unknown) {
      setLogNotice(`日志启动失败：${errorMessage(error)}`);
    }
  }, []);

  const stopLogging = useCallback(async () => {
    const sessionId = sessionIdRef.current;
    if (!sessionId) {
      setLogNotice("会话尚未就绪，无法停止日志");
      return;
    }

    try {
      const nextState = await stopTerminalLog(sessionId);
      setLogState(nextState);
      setLogNotice(
        nextState.path
          ? `日志已停止：${formatLogPath(nextState.path)}`
          : "日志已停止",
      );
    } catch (error: unknown) {
      setLogNotice(`日志停止失败：${errorMessage(error)}`);
    }
  }, []);

  const disconnectTerminal = useCallback(async () => {
    await disconnectSessionRef.current?.();
  }, []);

  const reconnectTerminal = useCallback(async () => {
    await reconnectSessionRef.current?.();
  }, []);
  const executeContextMenuAction = useCallback(
    (action: TerminalContextMenuAction) => {
      setContextMenu(null);
      const terminal = terminalRef.current;
      const sessionId = sessionIdRef.current;

      if (action === "copy") {
        const selection = terminal?.getSelection?.() ?? "";
        if (selection) {
          void writeDesktopClipboardText(selection);
        }
      } else if (action === "copySessionId") {
        if (sessionId) {
          void writeDesktopClipboardText(sessionId);
        }
      } else if (action === "sendSelectionToAgent") {
        requestAgentSend({ paneId, source: "selection", tabId });
        setCommandBlockNotice("已将选中内容交给 Agent，等待发送预览");
      } else if (action === "sendContextToAgent") {
        requestAgentSend({ paneId, source: "context", tabId });
        setCommandBlockNotice("已将终端上下文交给 Agent，等待发送预览");
      } else if (action === "paste") {
        void pasteIntoTerminal(paneId, terminal, sessionId).catch(() => {
          setTerminalNotice("粘贴失败，请检查剪贴板权限或终端连接。");
        });
      } else if (action === "selectAll") {
        terminal?.selectAll?.();
      } else if (action === "clear") {
        clearCommandBlocks();
        if (sessionId) {
          void writeTerminal(sessionId, TERMINAL_CLEAR_SCREEN_INPUT);
          scheduleCommandBlockViewsSync();
        } else {
          terminal?.write?.(TERMINAL_FRONTEND_CLEAR_SCREEN_SEQUENCE, () => {
            scheduleCommandBlockViewsSync();
          });
          if (!terminal?.write) {
            scheduleCommandBlockViewsSync();
          }
        }
      } else if (action === "search") {
        openSearch();
      } else if (action === "startLog") {
        void startLogging();
      } else if (action === "stopLog") {
        void stopLogging();
      } else if (action === "disconnect") {
        void disconnectTerminal();
      } else if (action === "reconnect") {
        void reconnectTerminal();
      } else if (action === "openLogs") {
        onOpenLogs?.();
      } else {
        const direction = splitDirectionForMenuAction(action);
        if (direction) {
          onSplitPane?.(direction);
        }
      }

      terminal?.focus();
    },
    [
      onOpenLogs,
      onSplitPane,
      paneId,
      tabId,
      disconnectTerminal,
      reconnectTerminal,
      startLogging,
      stopLogging,
      clearCommandBlocks,
      scheduleCommandBlockViewsSync,
      openSearch,
      setCommandBlockNotice,
    ],
  );

  const openContextMenu = useCallback((event: MouseEvent) => {
    event.preventDefault();
    event.stopPropagation();
    const terminal = terminalRef.current;
    const sessionId = sessionIdRef.current;
    const rightClickBehavior = terminalAppearanceRef.current.rightClickBehavior;
    const openMenuWithModifier = event.shiftKey && enableAgentSendActions;
    if (rightClickBehavior === "none" && !openMenuWithModifier) {
      terminal?.focus();
      return;
    }
    if (rightClickBehavior === "paste" && !openMenuWithModifier) {
      void pasteIntoTerminal(paneId, terminal, sessionId).catch(() => {
        setTerminalNotice("粘贴失败，请检查剪贴板权限或终端连接。");
      });
      terminal?.focus();
      return;
    }
    const selection = terminal?.getSelection?.() ?? "";
    if (enableAgentSendActions && selection) {
      updateTerminalPaneRuntimeContext(paneId, {
        selectedText: selection,
      });
    }
    const menuState: XtermPaneContextMenuState = {
      canCopy: selection.length > 0,
      canCopySessionId: Boolean(sessionId),
      canSendSelectionToAgent:
        enableAgentSendActions && selection.length > 0,
      canSendToAgent: enableAgentSendActions,
      position: {
        x: event.clientX,
        y: event.clientY,
      },
    };
    setContextMenu(menuState);
    terminal?.focus();
  }, [enableAgentSendActions, paneId]);

  const executeCommandBlockAction = useCallback(
    (blockId: string, action: TerminalCommandBlockAction) => {
      const block = commandBlocksRef.current.find(
        (current) => current.id === blockId,
      );
      if (!block) {
        return;
      }

      if (action === "toggle") {
        block.collapsed = !block.collapsed;
        setCommandBlockNotice(
          block.collapsed ? "命令块已折叠" : "命令块已展开",
        );
        syncCommandBlockViews();
        terminalRef.current?.focus();
        return;
      }

      if (action === "copyText") {
        void writeDesktopClipboardText(terminalCommandBlockPlainText(block))
          .then((result) =>
            setCommandBlockNotice(
              result.ok ? "命令块文本已复制" : "复制命令块文本失败",
            ),
          )
          .catch(() => setCommandBlockNotice("复制命令块文本失败"));
        terminalRef.current?.focus();
        return;
      }

      if (action === "sendToAgent") {
        updateTerminalPaneRuntimeContext(paneId, {
          commandBlockText: terminalCommandBlockPlainText(block),
        });
        requestAgentSend({ paneId, source: "commandBlock", tabId });
        setCommandBlockNotice("已准备发送该命令块到 Agent");
        terminalRef.current?.focus();
        return;
      }

      void copyTerminalCommandBlockAsImage(block, resolvedTheme)
        .then((result) => {
          setCommandBlockNotice(
            result === "image"
              ? "命令块图片已复制"
              : "当前剪贴板不支持图片，已复制文本",
          );
        })
        .catch(() => {
          void writeDesktopClipboardText(terminalCommandBlockPlainText(block))
            .then((result) =>
              setCommandBlockNotice(
                result.ok ? "复制图片失败，已复制文本" : "复制命令块失败",
              ),
            )
            .catch(() => setCommandBlockNotice("复制命令块失败"));
        });
      terminalRef.current?.focus();
    },
    [paneId, resolvedTheme, setCommandBlockNotice, syncCommandBlockViews, tabId],
  );

  return (
    <XtermPaneView
      activityRuntimeRef={activityRuntimeRef}
      backgroundImageVisible={backgroundImageVisible}
      agentSendActionsEnabled={enableAgentSendActions}
      canSplit={Boolean(onSplitPane)}
      commandBlockNotice={commandBlockNotice}
      commandBlockViews={commandBlockViews}
      connectionState={connectionState}
      containerRef={containerRef}
      contextMenu={contextMenu}
      ghostSuggestion={ghostSuggestion}
      logActive={logState.active}
      logNotice={logNotice}
      logPath={logState.path}
      onCloseContextMenu={() => setContextMenu(null)}
      onCommandBlockAction={executeCommandBlockAction}
      onContextMenu={openContextMenu}
      onContextMenuAction={executeContextMenuAction}
      paneActivity={paneActivity}
      paneId={paneId}
      search={search}
      shellAssistEnabled={shellAssistEnabled && !inlineTuiActive}
      suggestionOverlay={suggestionMenuRuntime.overlay}
      terminalAppearance={terminalAppearance}
      terminalNotice={terminalNotice}
      terminalRef={terminalRef}
      title={title}
    />
  );
}
