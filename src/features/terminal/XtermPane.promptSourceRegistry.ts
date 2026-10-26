/**
 * @author kongweiguang
 */
import { useEffect, useRef, type MutableRefObject } from "react";
import type { Terminal } from "@xterm/xterm";
import type { TerminalCommandBlock } from "./terminalCommandBlocks";
import { terminalCommandBlockPlainText } from "./terminalCommandBlocks";
import { getTerminalPaneSessionRecord } from "./terminalSessionRegistry";
import { withTerminalInputOrigin } from "./terminalInputOrigin";
import { terminalInputSync } from "./terminalInputSync";
import type { XtermPaneInputRequest } from "./XtermPane.types";

/** 终端正文按需读取结果；调用方不得把该对象同步到全局状态或持久化。 */
export interface XtermPanePromptSourceSnapshot {
  commandBlockText?: string;
  paneId: string;
  selectedText?: string;
}

export interface XtermPanePromptSource {
  read(): XtermPanePromptSourceSnapshot;
}

const promptSources = new Map<string, XtermPanePromptSource>();

/** 注册 pane 的瞬时正文读取器；重复 pane 只允许最新挂载实例生效。 */
export function registerXtermPanePromptSource(
  paneId: string,
  source: XtermPanePromptSource,
): () => void {
  promptSources.set(paneId, source);
  return () => {
    if (promptSources.get(paneId) === source) {
      promptSources.delete(paneId);
    }
  };
}

/** 仅在用户点击发送入口时读取正文，读取失败或 pane 不存在时安全返回空。 */
export function readXtermPanePromptSource(
  paneId: string,
): XtermPanePromptSourceSnapshot | null {
  const source = promptSources.get(paneId);
  if (!source) {
    return null;
  }
  const snapshot = source.read();
  return snapshot.paneId === paneId ? snapshot : null;
}

interface UseXtermPanePromptBridgeInput {
  commandBlocksRef: MutableRefObject<TerminalCommandBlock[]>;
  connectionState: string;
  inputRequest?: XtermPaneInputRequest | null;
  paneId: string;
  sessionIdRef: MutableRefObject<string | null>;
  terminalRef: MutableRefObject<Terminal | null>;
}

/** 集中管理 prompt 正文的按需读取和一次性输入请求消费。 */
export function useXtermPanePromptBridge({
  commandBlocksRef,
  connectionState,
  inputRequest,
  paneId,
  sessionIdRef,
  terminalRef,
}: UseXtermPanePromptBridgeInput): void {
  const lastInputRequestIdRef = useRef<string | null>(null);
  useEffect(
    () =>
      registerXtermPanePromptSource(paneId, {
        read: () => {
          const latest =
            commandBlocksRef.current[commandBlocksRef.current.length - 1];
          const runtimeContext = getTerminalPaneSessionRecord(paneId);
          return {
            commandBlockText:
              runtimeContext?.commandBlockText ??
              (latest ? terminalCommandBlockPlainText(latest) : undefined),
            paneId,
            selectedText:
              runtimeContext?.selectedText ??
              terminalRef.current?.getSelection?.() ??
              undefined,
          };
        },
      }),
    [commandBlocksRef, paneId, terminalRef],
  );
  useEffect(() => {
    if (!inputRequest || lastInputRequestIdRef.current === inputRequest.id) return;
    const terminal = terminalRef.current;
    const sessionId = sessionIdRef.current;
    if (!terminal || !sessionId) return;
    // 定向输入先关闭所属同步组，避免粘贴引发的延迟输入被传播。
    terminalInputSync.stopForPane(paneId);
    lastInputRequestIdRef.current = inputRequest.id;
    if (inputRequest.text) {
      withTerminalInputOrigin(terminal, { kind: "programmatic" }, () => {
        terminal.paste(inputRequest.text);
      });
    }
    if (inputRequest.submit) {
      void terminalInputSync.writeUserInput(paneId, { data: "\r" }).catch(() => undefined);
    }
    terminal.focus();
  }, [connectionState, inputRequest, paneId, sessionIdRef, terminalRef]);
}
