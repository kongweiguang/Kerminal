/**
 * @author kongweiguang
 */
import type { Terminal as XtermTerminal } from "@xterm/xterm";

export type TerminalInputOrigin =
  | { kind: "paste"; text: string }
  | { kind: "programmatic" };

const inputOrigins = new WeakMap<XtermTerminal, TerminalInputOrigin>();

/** 只标记同步调用范围；异常与嵌套调用均恢复原来源，不保留输入历史。 */
export function withTerminalInputOrigin(
  terminal: XtermTerminal,
  origin: TerminalInputOrigin,
  action: () => void,
): void {
  const previous = inputOrigins.get(terminal);
  inputOrigins.set(terminal, origin);
  try {
    action();
  } finally {
    if (previous) {
      inputOrigins.set(terminal, previous);
    } else {
      inputOrigins.delete(terminal);
    }
  }
}

/** 供 onData 在当前同步栈读取来源，跨事件不沿用标记。 */
export function getTerminalInputOrigin(
  terminal: XtermTerminal,
): TerminalInputOrigin | undefined {
  return inputOrigins.get(terminal);
}
