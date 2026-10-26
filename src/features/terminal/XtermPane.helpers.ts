/**
 * @author kongweiguang
 */

import type { ISearchOptions } from "@xterm/addon-search";
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import type { TerminalCreateRequest } from "../../lib/terminalApi";
import { readTerminalClipboardText } from "../../lib/terminalApi";
import type {
  CommandSuggestionCandidate,
  CommandSuggestionProvider,
} from "../../lib/terminalSuggestionApi";
import {
  resolveRuntimeSnippetFeatureGates,
  snippetV2NavigationEnabled,
} from "../snippets/contracts/index";
import type { TerminalAppearance } from "../settings/contracts/index";
import type { TerminalCommandBlockView } from "./terminalCommandBlocks";
import {
  applyTerminalInputData,
  createTerminalInputModelState,
  type TerminalInputModelState,
} from "./terminalInputModel";
import { parseTerminalShellIntegrationCwd } from "./terminalShellIntegrationModel";
import { terminalChromeRuntimeStore } from "./terminalChromeRuntimeStore";
import { getTerminalPaneSessionRecord } from "./terminalSessionRegistry";
import { withTerminalInputOrigin } from "./terminalInputOrigin";

export type ConnectionState =
  | "connecting"
  | "reconnecting"
  | "connected"
  | "disconnected"
  | "closed"
  | "error";

export interface TerminalGhostSuggestion {
  candidate: CommandSuggestionCandidate;
  left: number;
  lineHeight: number;
  maxWidth: number;
  suffix: string;
  top: number;
}

const CURRENT_DIR_OSC_1337_PREFIX = "\u001b]1337;CurrentDir=";
const CURRENT_DIR_OSC_7_PREFIX = "\u001b]7;";
const OSC_BEL_TERMINATOR = "\u0007";
const OSC_ST_TERMINATOR = "\u001b\\";
const MAX_CWD_TRACKING_BUFFER_LENGTH = 4096;
const MAX_PROMPT_CWD_LINE_LENGTH = 512;
const MIN_TERMINAL_SESSION_COLS = 20;
const MIN_TERMINAL_SESSION_ROWS = 8;
const CONTROL_CHAR_RE = new RegExp(String.raw`[\u0000-\u001f\u007f]`);
const PROMPT_OSC_RE = new RegExp(
  String.raw`\u001b\][^\u0007]*(?:\u0007|\u001b\\)`,
  "g",
);
const PROMPT_CSI_RE = new RegExp(String.raw`\u001b\[[0-?]*[ -/]*[@-~]`, "g");
const PROMPT_ESCAPE_RE = new RegExp(String.raw`\u001b[ -/]*[@-~]`, "g");
const PROMPT_C0_CONTROL_RE = new RegExp(
  String.raw`[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]`,
  "g",
);

export function terminalSuggestionProviders({
  hasSshRemote,
  inlineSuggestion,
}: {
  hasSshRemote: boolean;
  inlineSuggestion: TerminalAppearance["inlineSuggestion"];
}): CommandSuggestionProvider[] {
  if (!inlineSuggestion.enabled) {
    return [];
  }
  const providers: CommandSuggestionProvider[] = [];
  if (inlineSuggestion.providers.history) {
    providers.push("history");
  }
  if (
    hasSshRemote && inlineSuggestion.remoteProbeEnabled
  ) {
    if (inlineSuggestion.providers.remotePath) {
      providers.push("remotePath");
    }
    if (inlineSuggestion.providers.remoteCommand) {
      providers.push("remoteCommand");
    }
    if (inlineSuggestion.providers.git) {
      providers.push("git");
    }
  }
  if (inlineSuggestion.providers.spec) {
    providers.push("spec");
  }
  if (snippetV2NavigationEnabled(resolveRuntimeSnippetFeatureGates())) {
    providers.push("snippet");
  }
  return providers;
}

export function terminalGhostSuggestionEqual(
  current: TerminalGhostSuggestion | null,
  next: TerminalGhostSuggestion | null,
) {
  if (current === next) {
    return true;
  }
  if (!current || !next) {
    return false;
  }
  return (
    current.suffix === next.suffix &&
    nearlyEqual(current.left, next.left) &&
    nearlyEqual(current.maxWidth, next.maxWidth) &&
    nearlyEqual(current.top, next.top) &&
    current.candidate.provider === next.candidate.provider &&
    current.candidate.sourceId === next.candidate.sourceId &&
    current.candidate.replacementText === next.candidate.replacementText &&
    current.candidate.description === next.candidate.description
  );
}

function nearlyEqual(left: number, right: number) {
  return Math.abs(left - right) <= 0.25;
}

export function resolveTerminalRowHeight(
  container: HTMLDivElement | null,
  terminalAppearance: TerminalAppearance,
  terminal?: Pick<XtermTerminal, "rows">,
) {
  const screenElement = container?.querySelector(".xterm-screen");
  if (screenElement instanceof HTMLElement && terminal && terminal.rows > 0) {
    const measuredHeight = screenElement.getBoundingClientRect().height;
    if (measuredHeight > 0) {
      return measuredHeight / terminal.rows;
    }
  }

  const rowElement = container?.querySelector(".xterm-rows > div");
  if (rowElement instanceof HTMLElement) {
    const measuredHeight = rowElement.getBoundingClientRect().height;
    if (measuredHeight > 0) {
      return measuredHeight;
    }
    const computedLineHeight = Number.parseFloat(
      window.getComputedStyle(rowElement).lineHeight,
    );
    if (Number.isFinite(computedLineHeight) && computedLineHeight > 0) {
      return computedLineHeight;
    }
  }

  return Math.max(
    12,
    terminalAppearance.fontSize * terminalAppearance.lineHeight,
  );
}

export function resolveGhostSuggestionLayout(
  container: HTMLDivElement,
  terminal: XtermTerminal,
  terminalAppearance: TerminalAppearance,
  inputModel: TerminalInputModelState,
): Pick<
  TerminalGhostSuggestion,
  "left" | "lineHeight" | "maxWidth" | "top"
> | null {
  const activeBuffer = terminal.buffer.active;
  if (
    activeBuffer.type === "alternate" ||
    activeBuffer.viewportY !== activeBuffer.baseY
  ) {
    return null;
  }

  const rowHeight = resolveTerminalRowHeight(
    container,
    terminalAppearance,
    terminal,
  );
  const cellWidth = resolveTerminalCellWidth(
    container,
    terminal,
    terminalAppearance,
  );
  if (rowHeight <= 0 || cellWidth <= 0) {
    return null;
  }

  const cursorX = resolveTerminalCursorX(terminal, inputModel);
  const cursorY = resolveTerminalCursorY(terminal);
  const frameElement =
    container.parentElement instanceof HTMLElement
      ? container.parentElement
      : null;
  const screenElement = container.querySelector(".xterm-screen");
  const rowsElement = container.querySelector(".xterm-rows");
  const frameRect = frameElement?.getBoundingClientRect();
  const screenRect =
    screenElement instanceof HTMLElement
      ? screenElement.getBoundingClientRect()
      : undefined;
  const rowsRect =
    rowsElement instanceof HTMLElement
      ? rowsElement.getBoundingClientRect()
      : undefined;
  const containerStyle = window.getComputedStyle(container);
  const paddingLeft = Number.parseFloat(containerStyle.paddingLeft) || 0;
  const paddingTop = Number.parseFloat(containerStyle.paddingTop) || 0;
  const originLeft =
    frameRect && rowsRect && rowsRect.width > 0
      ? rowsRect.left - frameRect.left
      : frameRect && screenRect && screenRect.width > 0
        ? screenRect.left - frameRect.left
        : container.offsetLeft +
          (rowsElement instanceof HTMLElement
            ? rowsElement.offsetLeft
            : paddingLeft);
  const originTop =
    frameRect && rowsRect && rowsRect.height > 0
      ? rowsRect.top - frameRect.top
      : frameRect && screenRect && screenRect.height > 0
        ? screenRect.top - frameRect.top
        : container.offsetTop +
          (rowsElement instanceof HTMLElement
            ? rowsElement.offsetTop
            : paddingTop);
  const left = originLeft + cursorX * cellWidth;
  const top = originTop + cursorY * rowHeight;
  const screenRight =
    frameRect && screenRect && screenRect.width > 0
      ? screenRect.right - frameRect.left
      : undefined;
  const availableWidth =
    typeof screenRight === "number"
      ? screenRight - left
      : (container.clientWidth || terminal.cols * cellWidth) -
        originLeft +
        container.offsetLeft -
        cursorX * cellWidth;

  if (availableWidth <= cellWidth) {
    return null;
  }

  return {
    left,
    lineHeight: rowHeight,
    maxWidth: Math.max(cellWidth, availableWidth),
    top,
  };
}

function resolveTerminalCellWidth(
  container: HTMLDivElement,
  terminal: XtermTerminal,
  terminalAppearance: TerminalAppearance,
) {
  const screenElement = container.querySelector(".xterm-screen");
  if (screenElement instanceof HTMLElement && terminal.cols > 0) {
    const measuredWidth = screenElement.getBoundingClientRect().width;
    if (measuredWidth > 0) {
      return measuredWidth / terminal.cols;
    }
  }

  const rowElement = container.querySelector(".xterm-rows > div");
  if (rowElement instanceof HTMLElement) {
    const measuredWidth = rowElement.getBoundingClientRect().width;
    if (measuredWidth > 0 && terminal.cols > 0) {
      return measuredWidth / terminal.cols;
    }
  }

  return Math.max(4, terminalAppearance.fontSize * 0.62);
}

function resolveTerminalCursorX(
  terminal: XtermTerminal,
  inputModel: TerminalInputModelState,
) {
  const activeBuffer = terminal.buffer.active as { cursorX?: number };
  if (typeof activeBuffer.cursorX === "number" && activeBuffer.cursorX >= 0) {
    return Math.min(activeBuffer.cursorX, Math.max(0, terminal.cols - 1));
  }
  return Math.min(inputModel.cursor, Math.max(0, terminal.cols - 1));
}

function resolveTerminalCursorY(terminal: XtermTerminal) {
  const activeBuffer = terminal.buffer.active as { cursorY?: number };
  if (typeof activeBuffer.cursorY === "number" && activeBuffer.cursorY >= 0) {
    return Math.min(activeBuffer.cursorY, Math.max(0, terminal.rows - 1));
  }
  return 0;
}

export function isRightArrowInput(data: string) {
  return data === "\u001b[C" || data === "\u001bOC";
}

export function resolveTerminalContentBottomLine(terminal: XtermTerminal) {
  const buffer = terminal.buffer.active;
  for (let line = buffer.length - 1; line >= 0; line -= 1) {
    const text = buffer.getLine(line)?.translateToString(true) ?? "";
    if (text.trim().length > 0) {
      return line;
    }
  }
  return undefined;
}

export function resolveTerminalPromptLine(
  terminal: XtermTerminal,
  pendingInput: string,
) {
  const buffer = terminal.buffer.active;
  const activeBuffer = buffer as typeof buffer & {
    baseY?: number;
    cursorY?: number;
  };
  const cursorLine =
    typeof activeBuffer.baseY === "number" &&
    typeof activeBuffer.cursorY === "number"
      ? activeBuffer.baseY + activeBuffer.cursorY
      : resolveTerminalContentBottomLine(terminal);
  if (typeof cursorLine !== "number") {
    return undefined;
  }

  const text = buffer.getLine(cursorLine)?.translateToString(true) ?? "";
  const promptCandidate =
    pendingInput.length > 0 && text.endsWith(pendingInput)
      ? text.slice(0, -pendingInput.length).trimEnd()
      : text;
  return isLikelyShellPrompt(promptCandidate) ? cursorLine : undefined;
}

export function isLikelyShellPrompt(text: string) {
  const trimmed = text.trim();
  if (!trimmed) {
    return false;
  }
  return /(?:[$#>%]|❯|➜)$/.test(trimmed);
}

export function applyTerminalCommandBlockFolding(
  container: HTMLDivElement | null,
  views: TerminalCommandBlockView[],
) {
  if (!container) {
    return undefined;
  }

  resetTerminalCommandBlockFolding(container);
  const collapsedViews = views.filter(
    (view) => view.collapsed && !view.muted && view.hiddenLineCount > 0,
  );
  if (collapsedViews.length === 0) {
    return () => resetTerminalCommandBlockFolding(container);
  }

  const rows = Array.from(
    container.querySelectorAll<HTMLElement>(".xterm-rows > div"),
  );
  if (rows.length === 0) {
    return () => resetTerminalCommandBlockFolding(container);
  }

  const viewportY = views[0]?.viewportY ?? 0;
  const rowHeight = views[0]?.rowHeight ?? 0;
  if (rowHeight <= 0) {
    return () => resetTerminalCommandBlockFolding(container);
  }

  rows.forEach((row, rowIndex) => {
    const line = viewportY + rowIndex;
    let hiddenLinesBefore = 0;
    let hiddenInsideFold = false;

    for (const view of collapsedViews) {
      if (line > view.visibleStartLine && line <= view.visibleEndLine) {
        hiddenInsideFold = true;
        break;
      }
      if (line > view.visibleEndLine) {
        hiddenLinesBefore += view.hiddenLineCount;
      }
    }

    if (!hiddenInsideFold && hiddenLinesBefore === 0) {
      return;
    }

    row.dataset.commandBlockFolded = "true";
    row.dataset.commandBlockFoldTransform = row.style.transform;
    row.dataset.commandBlockFoldVisibility = row.style.visibility;

    if (hiddenInsideFold) {
      row.style.visibility = "hidden";
      return;
    }

    const offset = hiddenLinesBefore * rowHeight;
    row.style.transform =
      `${row.dataset.commandBlockFoldTransform} translateY(-${offset}px)`.trim();
  });

  return () => resetTerminalCommandBlockFolding(container);
}

function resetTerminalCommandBlockFolding(container: HTMLDivElement) {
  const foldedRows = Array.from(
    container.querySelectorAll<HTMLElement>(
      ".xterm-rows > div[data-command-block-folded='true']",
    ),
  );
  foldedRows.forEach((row) => {
    row.style.transform = row.dataset.commandBlockFoldTransform ?? "";
    row.style.visibility = row.dataset.commandBlockFoldVisibility ?? "";
    delete row.dataset.commandBlockFolded;
    delete row.dataset.commandBlockFoldTransform;
    delete row.dataset.commandBlockFoldVisibility;
  });
}

export function terminalSearchOptions(caseSensitive: boolean): ISearchOptions {
  return {
    caseSensitive,
    decorations: {
      activeMatchBackground: "#38bdf8",
      activeMatchBorder: "#0ea5e9",
      activeMatchColorOverviewRuler: "#0ea5e9",
      matchBackground: "#fde68a",
      matchBorder: "#f59e0b",
      matchOverviewRuler: "#f59e0b",
    },
  };
}

export function collectSubmittedCommands(
  currentBuffer: string,
  data: string,
): { buffer: string; commands: string[] } {
  const update = applyTerminalInputData(
    createTerminalInputModelState({ command: currentBuffer }),
    data,
  );

  return { buffer: update.state.command, commands: update.commands };
}

export function collectCurrentDirOscSequences(
  currentBuffer: string,
  data: string,
): { buffer: string; paths: string[] } {
  const state = collectCurrentDirObservations(currentBuffer, data);
  return {
    buffer: state.buffer,
    paths: state.observations.map(({ path }) => path),
  };
}

export type TerminalCwdObservation = {
  path: string;
  source: "osc7" | "osc1337" | "prompt";
};

/** 按来源保留远端目录观测，供会话优先使用协议结果而非提示符猜测。 */
export function collectCurrentDirObservations(
  currentBuffer: string,
  data: string,
): { buffer: string; observations: TerminalCwdObservation[] } {
  const combinedBuffer = limitCwdTrackingBuffer(currentBuffer + data);
  const promptState = collectCurrentDirPromptPaths(combinedBuffer);
  const observations: TerminalCwdObservation[] = [];
  let buffer = combinedBuffer;

  while (buffer) {
    const nextOsc = findNextCurrentDirOsc(buffer);
    if (!nextOsc) {
      return {
        buffer: nextCwdTrackingBuffer(combinedBuffer, promptState.buffer),
        observations:
          observations.length > 0
            ? observations
            : promptState.paths.map((path) => ({ path, source: "prompt" })),
      };
    }

    if (nextOsc.index > 0) {
      buffer = buffer.slice(nextOsc.index);
    }

    const payloadStart = nextOsc.prefix.length;
    const terminator = findCurrentDirOscTerminator(buffer, payloadStart);
    if (!terminator) {
      return {
        buffer: limitCwdTrackingBuffer(buffer),
        observations,
      };
    }

    const payload = buffer.slice(payloadStart, terminator.index);
    const path =
      nextOsc.protocol === "osc7"
        ? sanitizeCurrentDirOscPath(
            parseTerminalShellIntegrationCwd(payload) ?? "",
          )
        : sanitizeCurrentDirOscPath(payload);
    if (path) {
      observations.push({ path, source: nextOsc.protocol });
    }
    buffer = buffer.slice(terminator.index + terminator.length);
  }

  return {
    buffer: promptState.buffer,
    observations:
      observations.length > 0
        ? observations
        : promptState.paths.map((path) => ({ path, source: "prompt" })),
  };
}

function findNextCurrentDirOsc(buffer: string): {
  index: number;
  prefix: string;
  protocol: "osc7" | "osc1337";
} | null {
  const candidates = [
    { prefix: CURRENT_DIR_OSC_1337_PREFIX, protocol: "osc1337" as const },
    { prefix: CURRENT_DIR_OSC_7_PREFIX, protocol: "osc7" as const },
  ];
  let next:
    { index: number; prefix: string; protocol: "osc7" | "osc1337" } | undefined;
  for (const candidate of candidates) {
    const index = buffer.indexOf(candidate.prefix);
    if (index >= 0 && (!next || index < next.index)) {
      next = { ...candidate, index };
    }
  }
  return next ?? null;
}

function findCurrentDirOscTerminator(
  buffer: string,
  fromIndex: number,
): { index: number; length: number } | null {
  const belIndex = buffer.indexOf(OSC_BEL_TERMINATOR, fromIndex);
  const stIndex = buffer.indexOf(OSC_ST_TERMINATOR, fromIndex);
  if (belIndex === -1 && stIndex === -1) {
    return null;
  }
  if (stIndex !== -1 && (belIndex === -1 || stIndex < belIndex)) {
    return { index: stIndex, length: OSC_ST_TERMINATOR.length };
  }
  return { index: belIndex, length: OSC_BEL_TERMINATOR.length };
}

function sanitizeCurrentDirOscPath(path: string): string | undefined {
  const trimmed = path.trim();
  if (!trimmed.startsWith("/") || CONTROL_CHAR_RE.test(trimmed)) {
    return undefined;
  }
  return trimmed.length > MAX_CWD_TRACKING_BUFFER_LENGTH ? undefined : trimmed;
}

function collectCurrentDirPromptPaths(buffer: string): {
  buffer: string;
  paths: string[];
} {
  const paths: string[] = [];
  const lines = stripTerminalControlsForPrompt(buffer)
    .split(/[\r\n]+/)
    .slice(-8);
  for (const line of lines) {
    const path = extractRemotePromptCwd(line);
    if (path) {
      paths.push(path);
    }
  }
  return {
    buffer: trailingPromptCwdBuffer(buffer),
    paths,
  };
}

function extractRemotePromptCwd(line: string): string | undefined {
  const trimmed = line.trimEnd();
  const userHostPrompt = trimmed.match(
    /^[^\s@]+@[^:\s]+:(\/.*?)(?:\s+(?:\([^)]*\)|\[[^\]]*\]))?\s*[#$]\s*$/,
  );
  const bracketPrompt = trimmed.match(
    /^\[[^\]\s@]+@[^\]\s]+\s+(\/[^\]]+?)\]\s*[#$]\s*$/,
  );
  return sanitizeCurrentDirOscPath(
    userHostPrompt?.[1] ?? bracketPrompt?.[1] ?? "",
  );
}

function trailingPromptCwdBuffer(buffer: string): string {
  const lineStart =
    Math.max(buffer.lastIndexOf("\n"), buffer.lastIndexOf("\r")) + 1;
  const tail = buffer.slice(lineStart);
  if (tail.length === 0 || tail.length > MAX_PROMPT_CWD_LINE_LENGTH) {
    return "";
  }
  const normalizedTail = stripTerminalControlsForPrompt(tail);
  if (extractRemotePromptCwd(normalizedTail)) {
    return "";
  }
  if (
    /^[^\s@]+@[^:\s]+:\/[^\r\n]*$/.test(normalizedTail.trimStart()) ||
    /^\[[^\]]*@?[^\]]+\s+\/[^\]\r\n]*$/.test(normalizedTail.trimStart())
  ) {
    return tail;
  }
  return "";
}

function stripTerminalControlsForPrompt(value: string): string {
  return value
    .replace(PROMPT_OSC_RE, "")
    .replace(PROMPT_CSI_RE, "")
    .replace(PROMPT_ESCAPE_RE, "")
    .replace(PROMPT_C0_CONTROL_RE, "");
}

function trailingPotentialCurrentDirOscPrefix(buffer: string): string {
  let longest = "";
  for (const prefix of [
    CURRENT_DIR_OSC_1337_PREFIX,
    CURRENT_DIR_OSC_7_PREFIX,
  ]) {
    const maxLength = Math.min(buffer.length, prefix.length - 1);
    for (let length = maxLength; length > longest.length; length -= 1) {
      const tail = buffer.slice(-length);
      if (prefix.startsWith(tail)) {
        longest = tail;
        break;
      }
    }
  }
  return longest;
}

function nextCwdTrackingBuffer(buffer: string, promptBuffer: string): string {
  const oscBuffer = trailingPotentialCurrentDirOscPrefix(buffer);
  return promptBuffer.length >= oscBuffer.length ? promptBuffer : oscBuffer;
}

function limitCwdTrackingBuffer(buffer: string): string {
  if (buffer.length <= MAX_CWD_TRACKING_BUFFER_LENGTH) {
    return buffer;
  }
  return buffer.slice(-MAX_CWD_TRACKING_BUFFER_LENGTH);
}

export function stateLabel(state: ConnectionState) {
  if (state === "connected") {
    return "已连接";
  }
  if (state === "closed") {
    return "已结束";
  }
  if (state === "disconnected") {
    return "已断开";
  }
  if (state === "error") {
    return "异常";
  }
  if (state === "reconnecting") {
    return "重新连接中";
  }
  return "连接中";
}

/** 为 effect 的结构化依赖生成键序稳定的 JSON。 */
export function stableJsonDependencyKey(value: unknown): string {
  return JSON.stringify(sortJsonValue(value));
}

function sortJsonValue(value: unknown): unknown {
  if (!value || typeof value !== "object") {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map(sortJsonValue);
  }
  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, entryValue]) => [key, sortJsonValue(entryValue)]),
  );
}

export function buildTerminalCreateRequest(
  request: TerminalCreateRequest,
): TerminalCreateRequest {
  const size = normalizeTerminalSessionSize(request);
  return {
    cols: size.cols,
    rows: size.rows,
    ...(request.shell ? { shell: request.shell } : {}),
    ...(request.args && request.args.length > 0 ? { args: request.args } : {}),
    ...(request.cwd ? { cwd: request.cwd } : {}),
    ...(request.env && Object.keys(request.env).length > 0
      ? { env: request.env }
      : {}),
  };
}

export function normalizeTerminalSessionSize({
  cols,
  rows,
}: {
  cols: number;
  rows: number;
}) {
  return {
    cols: normalizeTerminalSessionDimension(cols, MIN_TERMINAL_SESSION_COLS),
    rows: normalizeTerminalSessionDimension(rows, MIN_TERMINAL_SESSION_ROWS),
  };
}

function normalizeTerminalSessionDimension(value: number, minimum: number) {
  if (!Number.isFinite(value)) {
    return minimum;
  }
  return Math.max(minimum, Math.floor(value));
}

export function formatLogPath(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : path;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 剪贴板读取后重验当前连接，避免异步粘贴落入旧会话；空正文不触发输入。 */
export async function pasteIntoTerminal(
  paneId: string,
  terminal: XtermTerminal | null,
  sessionId: string | null,
): Promise<void> {
  const clipboardText = await readTerminalClipboardText();
  if (
    !terminal ||
    !sessionId ||
    getTerminalPaneSessionRecord(paneId)?.sessionId !== sessionId ||
    terminalChromeRuntimeStore.getSnapshot(paneId).connectionState !== "connected"
  ) {
    return;
  }
  const text = clipboardText.replace(/\r?\n/g, "\r");
  if (!text) return;
  withTerminalInputOrigin(terminal, { kind: "paste", text }, () => {
    terminal.paste(text);
  });
}
