// @author kongweiguang
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import type { SshAuthPromptRequest } from "../../lib/sshAuthApi";

interface TerminalInlineSshAuthPromptState {
  prompt: SshAuthPromptRequest;
  resolve: (value: string | null) => void;
  value: string;
}

interface CreateTerminalInlineSshAuthPromptOptions {
  markUserInteraction?: () => void;
  terminal: Pick<XtermTerminal, "focus" | "write">;
}

/// 终端内 SSH 密码/密钥口令提示只处理临时输入，不把明文写入 React state 或历史。
export function createTerminalInlineSshAuthPrompt({
  markUserInteraction,
  terminal,
}: CreateTerminalInlineSshAuthPromptOptions) {
  let activePrompt: TerminalInlineSshAuthPromptState | null = null;

  const finish = (value: string | null) => {
    const prompt = activePrompt;
    if (!prompt) {
      return;
    }
    activePrompt = null;
    prompt.resolve(value);
  };

  const promptForSecret = (prompt: SshAuthPromptRequest) =>
    new Promise<string | null>((resolve) => {
      activePrompt?.resolve(null);
      activePrompt = { prompt, resolve, value: "" };
      terminal.write(formatTerminalSshAuthPrompt(prompt));
      terminal.focus();
    });

  const handleInput = (data: string) => {
    if (!activePrompt) {
      return false;
    }
    markUserInteraction?.();
    for (const character of data) {
      if (character === "\u0003" || character === "\u001b") {
        terminal.write("^C\r\n");
        finish(null);
        return true;
      }
      if (character === "\r" || character === "\n") {
        terminal.write("\r\n");
        finish(activePrompt.value);
        return true;
      }
      if (character === "\u007f" || character === "\b") {
        activePrompt.value = activePrompt.value.slice(0, -1);
        continue;
      }
      if (character >= " ") {
        activePrompt.value += character;
      }
    }
    return true;
  };

  return {
    finish,
    handleInput,
    // 只暴露认证门禁，不暴露临时凭据或通过试探输入改变认证状态。
    isActive: (): boolean => activePrompt !== null,
    promptForSecret,
  };
}

function formatTerminalSshAuthPrompt(prompt: SshAuthPromptRequest) {
  const target = `${prompt.username}@${prompt.host}:${prompt.port}`;
  if (prompt.secretKind === "keyPassphrase") {
    return `\r\nEnter passphrase for ${target}: `;
  }
  return `\r\n${target}'s password: `;
}
