// @author kongweiguang
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { Terminal as XtermTerminal } from "@xterm/xterm";
import { resizeTerminal } from "../../lib/terminalApi";
import {
  updateTerminalPaneSessionCwd,
} from "./terminalSessionRegistry";
import { createTerminalOutputWriter } from "./terminalOutputWriter";
import { createXtermPaneCommandBlockRuntime } from "./XtermPane.commandBlockRuntime";
import { createXtermPaneGhostSuggestions } from "./XtermPane.ghostSuggestions";
import { createTerminalInlineSshAuthPrompt } from "./XtermPane.inlineSshAuthPrompt";
import { createTerminalInputModelState } from "./terminalInputModel";
import { createTerminalRemoteSuggestionPrewarm } from "./terminalRemoteSuggestionPrewarm";
import { createTerminalOutputHistoryBuffer } from "./terminalOutputHistoryBuffer";
import { terminalRendererFallbackReasonFromState } from "./terminalRendererPolicy";
import { createTerminalRendererController } from "./terminalRenderer";
import { resolveRuntimeTerminalRendererFeatureGates } from "./terminalRendererFeatureGates";
import {
  createTerminalRendererHealthWatchdog,
  type TerminalRendererHealthWatchdog,
} from "./terminalRendererHealthWatchdog";
import { createTerminalRendererPerformanceTelemetry } from "./terminalRendererPerformanceTelemetry";
import { terminalRendererRegistry } from "./terminalRendererRegistry";
import { createTerminalPaneResizeController } from "./terminalPaneResizeController";
import {
  createTerminalRendererSurfaceCoordinator,
  type TerminalRendererSurfaceCoordinator,
} from "./terminalRendererSurfaceCoordinator";
import type { TerminalRendererFallbackReason } from "./terminalRendererPolicy";
import { createTerminalOutputInstrumentation, runTerminalOutputInstrumentationStep } from "./terminalOutputInstrumentation";
import { registerCommandBlockClearHandlers } from "./XtermPane.runtime.helpers";
import { installShellIntegrationOscHandlers, isClearScreenCommand } from "./XtermPane.shellIntegration";
import { KITTY_KEYBOARD_PROTOCOL_ENABLE, shouldEnableKittyKeyboardProtocol } from "./terminalKeyboardPolicy";
import { createTerminalShellIntegrationState, reduceTerminalShellIntegrationState } from "./terminalShellIntegrationModel";
import { registerTerminalRuntimeDiagnosticsPane } from "./terminalRuntimeDiagnosticsStore";
import { createXtermPaneActivityRuntime } from "./XtermPane.activityRuntime";
import { registerXtermPaneRuntimeEvents, type XtermPaneRuntimeEvents } from "./XtermPane.runtime.events";
import { createXtermPaneArtifactRuntime } from "./XtermPane.artifacts";
import { createTerminalSurfaceEventController } from "./terminalSurfaceEventController";
import {
  createXtermAddonDisposalErrorState,
  disposeXtermTerminal,
  wrapXtermAddonForDisposal,
} from "./terminalDisposalCompatibility";
import { createXtermPaneSessionRuntime } from "./XtermPane.sessionRuntime";
import type { InstallXtermPaneRuntimeParams } from "./XtermPane.runtime.types";
import { resolveSafeTerminalRendererType } from "./terminalRendererPlatform";
import { createTerminalKeywordHighlightController } from "./terminalKeywordHighlightController";
import { createTerminalWebLinksAddon } from "./terminalWebLinks";
import {
  createTerminalWebLinkDecorationController,
  terminalWebLinkDecorationColorForTheme,
} from "./terminalWebLinkDecorationController";
const ORIGIN_ERASE_BELOW_COMMAND_BLOCK_GRACE_MS = 1_000;
const TERMINAL_RENDERER_FEATURE_GATES = resolveRuntimeTerminalRendererFeatureGates();
/**
 * 安装 xterm 与 PTY 的长期运行资源。透明能力和 proposed decoration API 都必须
 * 在构造时开启：前者避免背景切换重建会话，后者承载关键词及 URL 的 marker 装饰；
 * 只在独立 controller 内消费 proposed API，缩小后续 xterm 升级的影响面。
 */
export function installXtermPaneRuntime(params: InstallXtermPaneRuntimeParams) {
  const {
    activityRuntimeRef,
    args,
    commandBlockCounterRef,
    commandBlocksRef,
    containerRef,
    cwd,
    cwdTrackingBufferRef,
    currentCwdRef,
    disconnectSessionRef,
    env,
    fitAddonRef,
    focusedRef,
    ghostSuggestionRef,
    inputBufferRef,
    inputModelRef,
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
    resolvedTheme,
    remoteCommand,
    remoteHostId,
    searchAddonRef,
    sessionIdRef,
    setCommandBlockNotice,
    setCommandBlockViews,
    setConnectionState,
    setGhostSuggestion,
    setInlineTuiActive,
    setLogNotice,
    setLogState,
    setSearchResults,
    setSuggestionMenu,
    setTerminalNotice,
    shellIntegrationCommandBlockProtocolRef,
    shell,
    shellAssistEnabled = true,
    startupMessage,
    tabIdRef,
    suggestionMenuIntentRef,
    syncCommandBlockViews,
    target,
    terminalAppearance,
    terminalAppearanceRef,
    terminalFontWeight,
    terminalRef,
    terminalKeywordHighlightControllerRef,
    terminalRendererControllerRef,
    terminalRuntimeLifecycleControllerRef,
    terminalRuntimeLifecycleRef,
    terminalSurfaceCoordinatorRef,
    terminalTheme,
    terminalWebLinkDecorationControllerRef,
    transientStartupMessage,
    visibleRef,
  } = params;
  const container = containerRef.current;
  if (!container) {
    return undefined;
  }
  let disposed = false;
  let shellIntegrationState = createTerminalShellIntegrationState();
  const assistEnabled = shellAssistEnabled !== false;
  const inputCompatibilityMode = params.inputCompatibilityMode === "agentTui" ? "agentTui" : "shell";
  commandBlocksRef.current = [];
  inputModelRef.current = createTerminalInputModelState();
  setCommandBlockViews([]);
  setCommandBlockNotice(null);
  setGhostSuggestion(null);
  setSuggestionMenu(null);
  setTerminalNotice(null);
  const terminal = new XtermTerminal({
    allowProposedApi: true,
    allowTransparency: true,
    cols: 80,
    cursorBlink: terminalAppearance.cursorBlink,
    cursorInactiveStyle: "outline",
    cursorStyle: terminalAppearance.cursorStyle,
    fontFamily: terminalAppearance.fontFamily,
    fontSize: terminalAppearance.fontSize,
    fontWeight: terminalFontWeight,
    fontWeightBold: 700,
    lineHeight: terminalAppearance.lineHeight,
    macOptionIsMeta: terminalAppearance.macOptionIsMeta,
    minimumContrastRatio: 4.5,
    rows: 24,
    scrollback: terminalAppearance.scrollback,
    theme: terminalTheme,
  });
  const fitAddon = new FitAddon();
  const searchAddon = new SearchAddon({ highlightLimit: 1000 });
  const webLinksAddon = createTerminalWebLinksAddon({
    onOpenError: () =>
      setTerminalNotice("链接打开失败，请检查系统默认浏览器设置"),
  });
  const addonDisposalErrors = createXtermAddonDisposalErrorState();
  fitAddonRef.current = fitAddon;
  searchAddonRef.current = searchAddon;
  const reduceShellIntegrationRuntimeState = (event: Parameters<typeof reduceTerminalShellIntegrationState>[1]) => {
    shellIntegrationState = reduceTerminalShellIntegrationState(shellIntegrationState, event);
    return shellIntegrationState;
  };
  const commandBlockRuntime = createXtermPaneCommandBlockRuntime({
    assistEnabled,
    commandBlockCounterRef,
    commandBlocksRef,
    isDisposed: () => disposed,
    paneId,
    promptLineRef,
    readCurrentCommand: () => inputModelRef.current.command,
    readShellIntegrationState: () => shellIntegrationState,
    reduceShellIntegrationState: reduceShellIntegrationRuntimeState,
    setCommandBlockNotice,
    setCommandBlockViews,
    setInlineTuiActive,
    shellIntegrationCommandBlockProtocolRef,
    syncCommandBlockViews,
    terminal,
  });
  const artifactRuntime = createXtermPaneArtifactRuntime({
    paneId,
    profileId,
    remoteHostId,
    target,
  });
  const canScheduleSuggestionProbe = () => terminalRuntimeLifecycleRef?.current?.shouldRunSuggestionProbe !== false;
  const ghostSuggestions = createXtermPaneGhostSuggestions({
    assistEnabled,
    canScheduleSuggestion: canScheduleSuggestionProbe,
    // 候选只提交实际插入文本，输入模型与同步分发由事件边界统一处理。
    commitUserInput: (data) => runtimeEvents.commitUserInput(data),
    container,
    currentCwdRef,
    cwd,
    ghostSuggestionRef,
    inputModelRef,
    inputCompatibilityMode,
    isDisposed: () => disposed,
    paneId,
    profileId,
    remoteHostId,
    scheduleCommandBlockViewSync: commandBlockRuntime.scheduleCommandBlockViewSync,
    sessionIdRef,
    setGhostSuggestion,
    setSuggestionMenu,
    shell,
    target,
    terminal,
    terminalAppearanceRef,
  });
  suggestionMenuIntentRef.current = (intent) => (sessionIdRef.current ? ghostSuggestions.handleMenuIntent(intent, sessionIdRef.current) : false);
  const shouldPreserveCommandBlockForOriginEraseBelow = () => {
    if (!assistEnabled) {
      return false;
    }
    const block = commandBlocksRef.current[commandBlocksRef.current.length - 1];
    if (!block || block.endMarker || isClearScreenCommand(block.command)) {
      return false;
    }
    return Date.now() - block.createdAt <= ORIGIN_ERASE_BELOW_COMMAND_BLOCK_GRACE_MS;
  };
  const remoteSuggestionPrewarm = createTerminalRemoteSuggestionPrewarm({
    canScheduleProbe: canScheduleSuggestionProbe,
    paneId,
    remoteHostId,
    target,
    terminalAppearanceRef,
  });
  const terminalOutputInstrumentation = createTerminalOutputInstrumentation({
    paneId,
  });
  const updateCurrentCwdFromTerminal = (nextCwd: string, options: { prewarmRemoteSuggestions: boolean }) => {
    if (nextCwd === currentCwdRef.current) {
      return;
    }
    currentCwdRef.current = nextCwd;
    updateTerminalPaneSessionCwd(paneId, nextCwd);
    onCurrentCwdChangeRef.current?.(nextCwd);
    if (!assistEnabled || !options.prewarmRemoteSuggestions) {
      return;
    }
    runTerminalOutputInstrumentationStep(terminalOutputInstrumentation, "remotePrewarmGit", nextCwd.length, () => remoteSuggestionPrewarm.scheduleGit(nextCwd));
    runTerminalOutputInstrumentationStep(terminalOutputInstrumentation, "remotePrewarmPath", nextCwd.length, () => remoteSuggestionPrewarm.scheduleRemotePath(nextCwd));
  };
  const shellIntegrationOscDisposables = installShellIntegrationOscHandlers(terminal, {
    reduceState: (event) => {
      shellIntegrationState = reduceTerminalShellIntegrationState(shellIntegrationState, event);
    },
    readState: () => shellIntegrationState,
    writeState: (nextState) => {
      shellIntegrationState = nextState;
    },
    onOsc133: (event) => commandBlockRuntime.handleShellIntegrationOsc133(event, "parser"),
    onCurrentCwd: (nextCwd) =>
      updateCurrentCwdFromTerminal(nextCwd, {
        prewarmRemoteSuggestions: false,
      }),
  });
  let rendererSurfaceCoordinator: TerminalRendererSurfaceCoordinator | null =
    null;
  let rendererHealthWatchdog: TerminalRendererHealthWatchdog | null = null;

  // xterm 的 DisposableStore 不隔离 addon 异常；包裹已知 addon 后仍由原生
  // store 负责顺序和 ownership，错误则延迟到顶层 disposal 统一重抛。
  terminal.loadAddon(wrapXtermAddonForDisposal(fitAddon, addonDisposalErrors));
  terminal.loadAddon(
    wrapXtermAddonForDisposal(searchAddon, addonDisposalErrors),
  );
  terminal.loadAddon(
    wrapXtermAddonForDisposal(webLinksAddon, addonDisposalErrors),
  );
  const terminalInlineSshAuthPrompt = createTerminalInlineSshAuthPrompt({
    markUserInteraction: () => terminalRuntimeLifecycleControllerRef?.current?.markUserInteraction(),
    terminal,
  });
  const compositionTarget = container.querySelector(".xterm") ?? container;
  terminal.open(container);
  // 会话创建需要首个 cols/rows；后续所有 surface 变化统一交给 coordinator。
  fitAddon.fit();
  const initialSurfaceDimensions = {
    cols: terminal.cols,
    rows: terminal.rows,
  };
  let reuseInitialSurfaceDimensions = true;
  const paneResizeController = createTerminalPaneResizeController({
    initialDimensions: initialSurfaceDimensions,
    onDimensionsChange: (dimensions) =>
      onTerminalDimensionsChangeRef.current?.(dimensions),
    onGhostSuggestionLayoutChange: () =>
      ghostSuggestions.refreshGhostSuggestionLayout(),
    resizeSession: resizeTerminal,
  });
  terminalRef.current = terminal;
  const terminalKeywordHighlightController =
    createTerminalKeywordHighlightController({
      resolvedTheme,
      settings: terminalAppearance.keywordHighlights,
      terminal,
      visible: visibleRef?.current ?? true,
    });
  terminalKeywordHighlightControllerRef.current =
    terminalKeywordHighlightController;
  const terminalWebLinkDecorationController =
    createTerminalWebLinkDecorationController({
      foregroundColor: terminalWebLinkDecorationColorForTheme(
        terminalTheme,
        resolvedTheme,
      ),
      terminal,
      visible: visibleRef?.current ?? true,
    });
  terminalWebLinkDecorationControllerRef.current =
    terminalWebLinkDecorationController;
  const activityRuntime = createXtermPaneActivityRuntime({
    connectionState: "connecting",
    container,
    paneId,
    terminal,
    visible: visibleRef?.current ?? true,
  });
  activityRuntimeRef.current = activityRuntime;
  let rendererBackend = "cpu";
  let lastRecordedRendererFallbackReason: TerminalRendererFallbackReason | undefined;
  const rendererTelemetry = createTerminalRendererPerformanceTelemetry();
  const effectiveRendererType = resolveSafeTerminalRendererType(
    terminalAppearance.rendererType,
  );
  const terminalRendererController = createTerminalRendererController({
    compatibilityGate: {
      forceContextLoss:
        TERMINAL_RENDERER_FEATURE_GATES.privateCleanupCompat,
      privateRendererCleanup:
        TERMINAL_RENDERER_FEATURE_GATES.privateCleanupCompat,
    },
    healthWatchdogEnabled: TERMINAL_RENDERER_FEATURE_GATES.healthWatchdog,
    onStateChange: (state) => {
      terminalRendererRegistry.updatePaneState(paneId, state);
      const fallbackReason = terminalRendererFallbackReasonFromState(state.fallbackReason);
      if (fallbackReason && fallbackReason !== lastRecordedRendererFallbackReason) {
        lastRecordedRendererFallbackReason = fallbackReason;
        terminalRendererRegistry.recordPaneFailure(paneId, fallbackReason);
      } else if (!fallbackReason) {
        lastRecordedRendererFallbackReason = undefined;
      }
      if (state.backend !== rendererBackend) {
        rendererBackend = state.backend;
        rendererSurfaceCoordinator?.invalidate();
      }
    },
    paneId,
    rendererType: effectiveRendererType,
    telemetry: TERMINAL_RENDERER_FEATURE_GATES.performanceTelemetry
      ? rendererTelemetry
      : undefined,
    terminal,
  });
  terminalRendererControllerRef.current = terminalRendererController;
  terminalRendererRegistry.updateMode(effectiveRendererType);
  const unregisterTerminalRenderer = terminalRendererRegistry.registerPane({
    controller: terminalRendererController,
    focused: focusedRef.current,
    paneId,
    visible: visibleRef?.current ?? true,
  });
  if (shouldEnableKittyKeyboardProtocol(inputCompatibilityMode)) {
    terminal.write(KITTY_KEYBOARD_PROTOCOL_ENABLE);
  }
  const outputWriter = createTerminalOutputWriter(terminal, {
    adaptive: TERMINAL_RENDERER_FEATURE_GATES.adaptiveOutputScheduler,
    callbackMode: "auto",
    cadence:
      visibleRef?.current === false
        ? "hidden"
        : focusedRef.current
          ? "focused"
          : "visible",
    telemetry: TERMINAL_RENDERER_FEATURE_GATES.performanceTelemetry
      ? rendererTelemetry
      : undefined,
  });
  const runtimeEvents: XtermPaneRuntimeEvents = registerXtermPaneRuntimeEvents({
    activityRuntimeRef,
    assistEnabled,
    commandBlockRuntime,
    compositionTarget,
    container,
    currentCwdRef,
    cwd,
    ghostSuggestions,
    ghostSuggestionRef,
    inputBufferRef,
    inputCompatibilityMode,
    inputModelRef,
    paneId,
    profileId,
    readShellIntegrationState: () => shellIntegrationState,
    remoteHostId,
    searchAddon,
    sessionIdRef,
    setSearchResults,
    shell,
    shellIntegrationCommandBlockProtocolRef,
    syncCommandBlockViews,
    target,
    terminal,
    terminalAppearanceRef,
    terminalInlineSshAuthPrompt,
    terminalRuntimeLifecycleControllerRef,
    writeShellIntegrationState: (nextState) => {
      shellIntegrationState = nextState;
    },
    onArtifactCommandBlock: (id, command) => artifactRuntime.queueCommandBlock(id, command),
    onArtifactInvalidate: (reason) => artifactRuntime.invalidate(reason),
    onInputWriteError: () => setTerminalNotice("终端输入发送失败，请检查连接。"),
  });
  const commandBlockClearHandlersDisposable = registerCommandBlockClearHandlers(
    terminal,
    () => {
      commandBlockRuntime.clearCommandBlocks();
      artifactRuntime.invalidate("clear");
    },
    {
      shouldPreserveOriginEraseBelow: shouldPreserveCommandBlockForOriginEraseBelow,
    },
  );
  outputWriter.writeNow(outputHistoryRef.current ?? "");
  const outputHistoryBuffer = createTerminalOutputHistoryBuffer({
    flushDelayMs: () => terminalRuntimeLifecycleRef?.current?.outputHistoryFlushIntervalMs ?? 100,
    onOutputHistoryChangeRef,
    outputHistoryRef,
  });
  rendererSurfaceCoordinator = createTerminalRendererSurfaceCoordinator({
    fit: () => {
      if (reuseInitialSurfaceDimensions) {
        return initialSurfaceDimensions;
      }
      fitAddon.fit();
      return { cols: terminal.cols, rows: terminal.rows };
    },
    measure: () => {
      const rect = container.getBoundingClientRect();
      const visible =
        visibleRef?.current !== false &&
        document.visibilityState !== "hidden" &&
        container.isConnected;
      return {
        dpr:
          typeof window.devicePixelRatio === "number"
            ? window.devicePixelRatio
            : 1,
        height: rect.height,
        minimized: !visible || rect.width <= 0 || rect.height <= 0,
        visible,
        width: rect.width,
      };
    },
    onDimensionsChange: paneResizeController.handleSurfaceDimensions,
    onStableSurface: () => {
      terminalRendererRegistry.updatePaneVisibility(paneId, true);
      terminalRendererController.resume();
      terminalRendererController.attach();
      terminalRuntimeLifecycleControllerRef?.current?.markVisibleRecoveryComplete();
      rendererHealthWatchdog?.check();
    },
  });
  rendererHealthWatchdog = TERMINAL_RENDERER_FEATURE_GATES.healthWatchdog
    ? createTerminalRendererHealthWatchdog({
        container,
        renderer: terminalRendererController,
        surfaceSnapshot: () => rendererSurfaceCoordinator?.getSnapshot(),
      })
    : null;
  if (terminalSurfaceCoordinatorRef) {
    terminalSurfaceCoordinatorRef.current = (invalidate = false) => {
      if (invalidate) {
        rendererSurfaceCoordinator?.invalidate();
        return;
      }
      rendererSurfaceCoordinator?.notify();
    };
  }
  const fitAndResize = () => rendererSurfaceCoordinator?.notify();
  const surfaceEvents = createTerminalSurfaceEventController({
    onDocumentVisibilityChange: (visibilityState) => {
      if (visibilityState === "hidden") {
        terminalRendererController.suspend();
        terminalRendererRegistry.updatePaneVisibility(paneId, false);
        rendererSurfaceCoordinator?.notify();
        return;
      }
      rendererSurfaceCoordinator?.invalidate();
    },
    onResize: fitAndResize,
    onSurfaceChange: () => rendererSurfaceCoordinator?.invalidate(),
    resizeTarget: container,
  });
  rendererSurfaceCoordinator.flush();
  reuseInitialSurfaceDimensions = false;
  const sessionRuntime = createXtermPaneSessionRuntime({
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
    instrumentation: terminalOutputInstrumentation,
    onAgentSignalRef,
    onCurrentCwd: (nextCwd) =>
      updateCurrentCwdFromTerminal(nextCwd, {
        prewarmRemoteSuggestions: true,
      }),
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
    refreshSurface: fitAndResize,
    resetInputState: () => {
      inputBufferRef.current = "";
      inputModelRef.current = createTerminalInputModelState();
      cwdTrackingBufferRef.current = "";
    },
    resetShellIntegration: () => {
      shellIntegrationState = createTerminalShellIntegrationState();
      shellIntegrationCommandBlockProtocolRef.current = false;
    },
    sessionIdRef,
    setConnectionState,
    setLogNotice,
    setLogState,
    setShellIntegrationTrusted: (trusted) => {
      shellIntegrationState = reduceTerminalShellIntegrationState(
        shellIntegrationState,
        { trusted, type: "session" },
      );
      shellIntegrationCommandBlockProtocolRef.current = assistEnabled && trusted;
    },
    shell,
    startupMessage,
    tabIdRef,
    target,
    terminal,
    terminalAppearanceRef,
    terminalInlineSshAuthPrompt,
    transientStartupMessage,
    visibleRef,
  });
  const unregisterRuntimeDiagnostics = registerTerminalRuntimeDiagnosticsPane({
    getSnapshot: () => {
      const sessionDiagnostics = sessionRuntime.diagnosticsSnapshot();
      return {
        focused: focusedRef.current,
        historyStats: outputHistoryBuffer.stats(),
        paneId,
        runtimeWorkMode:
          terminalRuntimeLifecycleRef?.current?.workMode ?? "full",
        sessionId: sessionIdRef.current ?? undefined,
        ...sessionDiagnostics,
        visible: visibleRef?.current ?? true,
        writerStats: outputWriter.stats(),
      };
    },
  });
  sessionRuntime.startInitial();
  fitAndResize();
  surfaceEvents.install();
  return () => {
    if (disposed) {
      return;
    }
    disposed = true;
    let hasFirstError = false;
    let firstError: unknown;
    /**
     * 卸载时每个 runtime 都是独立所有权；隔离单步异常可以保证 terminal core
     * 以及 renderer registry 仍会执行，最后再把首个错误交给 React 生命周期。
     */
    const runCleanup = (cleanup: () => void) => {
      try {
        cleanup();
      } catch (error) {
        if (!hasFirstError) {
          hasFirstError = true;
          firstError = error;
        }
      }
    };

    runCleanup(() => sessionRuntime.dispose());
    runCleanup(() => surfaceEvents.dispose());
    runCleanup(() => rendererHealthWatchdog?.dispose());
    rendererHealthWatchdog = null;
    runCleanup(() => rendererSurfaceCoordinator?.dispose());
    rendererSurfaceCoordinator = null;
    runCleanup(() => {
      if (terminalSurfaceCoordinatorRef?.current) {
        terminalSurfaceCoordinatorRef.current = null;
      }
    });
    runCleanup(() => ghostSuggestions.dispose());
    suggestionMenuIntentRef.current = null;
    runCleanup(() => commandBlockRuntime.clearCommandBlockViewSyncFrame());
    runCleanup(() => artifactRuntime.close());
    runCleanup(() => outputHistoryBuffer.dispose());
    runCleanup(() => unregisterRuntimeDiagnostics());
    runCleanup(() => runtimeEvents.dispose());
    for (const disposable of shellIntegrationOscDisposables) {
      runCleanup(() => disposable.dispose());
    }
    runCleanup(() => commandBlockClearHandlersDisposable.dispose());
    runCleanup(() => terminalInlineSshAuthPrompt.finish(null));
    runCleanup(() => paneResizeController.dispose());
    shellIntegrationCommandBlockProtocolRef.current = false;
    runCleanup(() => commandBlockRuntime.resetProtocolState());
    runCleanup(() => outputWriter.dispose());
    runCleanup(() => activityRuntime.dispose());
    if (activityRuntimeRef.current === activityRuntime) {
      activityRuntimeRef.current = null;
    }

    // 视觉 controller 持有 xterm 事件、marker 和 decoration，必须在 core 与
    // renderer 仍存活时完整释放，避免 terminal.dispose 后回调访问已销毁 buffer。
    runCleanup(() => terminalWebLinkDecorationController.dispose());
    if (
      terminalWebLinkDecorationControllerRef.current ===
      terminalWebLinkDecorationController
    ) {
      terminalWebLinkDecorationControllerRef.current = null;
    }
    runCleanup(() => terminalKeywordHighlightController.dispose());
    if (
      terminalKeywordHighlightControllerRef.current ===
      terminalKeywordHighlightController
    ) {
      terminalKeywordHighlightControllerRef.current = null;
    }

    // 先让 renderer controller 在 core 与 DOM 仍存活时释放 WebGL/context，
    // 再由 xterm 回收 core、listener 和剩余 addons，避免 WebView2 保留脱离 DOM
    // 的 GPU backing store。
    runCleanup(() =>
      disposeXtermTerminal(terminal, {
        unregisterRenderer: unregisterTerminalRenderer,
      }, addonDisposalErrors),
    );

    runCleanup(() => {
      if (
        terminalRendererControllerRef.current === terminalRendererController
      ) {
        terminalRendererControllerRef.current = null;
      }
      terminalRef.current = null;
      fitAddonRef.current = null;
      searchAddonRef.current = null;
      cwdTrackingBufferRef.current = "";
      ghostSuggestionRef.current = null;
      setGhostSuggestion(null);
      setLogState({ active: false, bytesWritten: 0 });
      setLogNotice(null);
    });

    if (hasFirstError) {
      throw firstError;
    }
  };
}
