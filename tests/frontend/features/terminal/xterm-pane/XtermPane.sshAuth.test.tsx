// @author kongweiguang
import { act, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { defaultAppSettings } from "../../../../../src/features/settings/settingsModel";
import { mocks } from "../../../support/terminal/XtermPane.testSupport.tsx";
import { XtermPane } from "../../../../../src/features/terminal/XtermPane";

describe("XtermPane sessions and command blocks", () => {
  it("reads SSH password auth in the terminal and retries a managed terminal once", async () => {
    mocks.api.createSshTerminalSession
      .mockRejectedValueOnce({
        terminalError: {
          class: "sshAuthRequired",
          message: "SSH 认证需要用户输入",
          operation: "createSession",
          recovery: "userActionRequired",
          retryable: false,
          sshAuthPromptPlan: {
            prompts: [
              {
                host: "dev.internal",
                port: 22,
                promptId: "ssh-auth:target:deploy@dev.internal:22:password",
                reason: "passwordPrompt",
                role: "target",
                secretKind: "password",
                username: "deploy",
              },
            ],
          },
        },
      })
      .mockImplementationOnce(async (_request, onOutput) => {
        mocks.setLatestOutputHandler(
          onOutput as Parameters<typeof mocks.setLatestOutputHandler>[0],
        );
        mocks.getLatestOutputHandler()?.({
          data: "managed ssh ready",
          kind: "data",
          sessionId: "ssh-session-after-auth",
        });
        return {
          cols: 80,
          id: "ssh-session-after-auth",
          rows: 24,
          shell: "ssh",
          shellIntegration: {
            reason: "remote test after auth",
            status: "disabled",
          },
          status: "running",
        };
      });

    render(
      <XtermPane
        focused
        paneId="pane-ssh-auth-prompt"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="生产 SSH"
      />,
    );

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });

    expect(mocks.api.requestSshAuthPrompt).not.toHaveBeenCalled();
    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("deploy@dev.internal:22's password:"),
        ),
      ).toBe(true);
    });

    await act(async () => {
      mocks.terminalInstances[0].onDataCallback?.("terminal-secret\r");
    });

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(2);
    });
    expect(mocks.api.submitSshAuthPromptResponse).toHaveBeenCalledWith({
      promptId: "ssh-auth:target:deploy@dev.internal:22:password",
      secretKind: "password",
      value: "terminal-secret",
    });
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      expect.any(String),
      expect.stringContaining("terminal-secret"),
    );
    expect(mocks.api.recordCommandHistory).not.toHaveBeenCalled();
    expect(screen.getByText("已连接")).not.toBeVisible();
    expect(
      mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
        String(data).includes("managed ssh ready"),
      ),
    ).toBe(true);
  });

  it("reads jump password and target key passphrase prompts sequentially in the terminal", async () => {
    mocks.api.createSshTerminalSession
      .mockRejectedValueOnce({
        terminalError: {
          class: "sshAuthRequired",
          message: "SSH 认证需要用户输入",
          operation: "createSession",
          recovery: "userActionRequired",
          retryable: false,
          sshAuthPromptPlan: {
            prompts: [
              {
                host: "bastion.internal",
                port: 2222,
                promptId: "ssh-auth:jump:0:ops@bastion.internal:2222:password",
                reason: "passwordPrompt",
                role: { jump: { index: 0 } },
                secretKind: "password",
                username: "ops",
              },
              {
                host: "prod.internal",
                port: 22,
                promptId:
                  "ssh-auth:target:deploy@prod.internal:22:keyPassphrase",
                reason: "keyPassphrasePrompt",
                role: "target",
                secretKind: "keyPassphrase",
                username: "deploy",
              },
            ],
          },
        },
      })
      .mockImplementationOnce(async (_request, onOutput) => {
        mocks.setLatestOutputHandler(
          onOutput as Parameters<typeof mocks.setLatestOutputHandler>[0],
        );
        mocks.getLatestOutputHandler()?.({
          data: "managed bastion ssh ready",
          kind: "data",
          sessionId: "ssh-session-after-bastion-auth",
        });
        return {
          cols: 80,
          id: "ssh-session-after-bastion-auth",
          rows: 24,
          shell: "ssh",
          shellIntegration: {
            reason: "remote bastion test after auth",
            status: "disabled",
          },
          status: "running",
        };
      });

    render(
      <XtermPane
        focused
        paneId="pane-ssh-bastion-auth-prompt"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="堡垒机 SSH"
      />,
    );

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });
    expect(mocks.api.requestSshAuthPrompt).not.toHaveBeenCalled();
    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("ops@bastion.internal:2222's password:"),
        ),
      ).toBe(true);
    });

    await act(async () => {
      mocks.terminalInstances[0].onDataCallback?.("jump-secret\r");
    });

    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes(
            "Enter passphrase for deploy@prod.internal:22:",
          ),
        ),
      ).toBe(true);
    });

    await act(async () => {
      mocks.terminalInstances[0].onDataCallback?.("target-passphrase\r");
    });

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(2);
    });
    expect(mocks.api.requestSshAuthPrompt).not.toHaveBeenCalled();
    expect(mocks.api.submitSshAuthPromptResponse).toHaveBeenNthCalledWith(1, {
      promptId: "ssh-auth:jump:0:ops@bastion.internal:2222:password",
      secretKind: "password",
      value: "jump-secret",
    });
    expect(mocks.api.submitSshAuthPromptResponse).toHaveBeenNthCalledWith(2, {
      promptId: "ssh-auth:target:deploy@prod.internal:22:keyPassphrase",
      secretKind: "keyPassphrase",
      value: "target-passphrase",
    });
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      expect.any(String),
      expect.stringContaining("jump-secret"),
    );
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      expect.any(String),
      expect.stringContaining("target-passphrase"),
    );
    expect(screen.getByText("已连接")).not.toBeVisible();
  });

  it("cancels terminal SSH auth prompt without submitting or retrying", async () => {
    mocks.api.createSshTerminalSession
      .mockRejectedValueOnce({
        terminalError: {
          class: "sshAuthRequired",
          message: "SSH 认证需要用户输入",
          operation: "createSession",
          recovery: "userActionRequired",
          retryable: false,
          sshAuthPromptPlan: {
            prompts: [
              {
                host: "dev.internal",
                port: 22,
                promptId: "ssh-auth:target:deploy@dev.internal:22:password",
                reason: "passwordPrompt",
                role: "target",
                secretKind: "password",
                username: "deploy",
              },
            ],
          },
        },
      })
      .mockResolvedValueOnce({
        cols: 80,
        id: "ssh-session-should-not-open",
        rows: 24,
        shell: "ssh",
        shellIntegration: {
          reason: "unexpected retry",
          status: "disabled",
        },
        status: "running",
      });

    render(
      <XtermPane
        focused
        paneId="pane-ssh-auth-prompt-cancel"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="生产 SSH"
      />,
    );

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });
    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("deploy@dev.internal:22's password:"),
        ),
      ).toBe(true);
    });

    await act(async () => {
      mocks.terminalInstances[0].onDataCallback?.("\u0003");
    });

    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("SSH 认证已取消。"),
        ),
      ).toBe(true);
    });
    expect(
      mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
        String(data).includes("^C"),
      ),
    ).toBe(true);
    expect(mocks.api.requestSshAuthPrompt).not.toHaveBeenCalled();
    expect(mocks.api.submitSshAuthPromptResponse).not.toHaveBeenCalled();
    expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    expect(mocks.api.writeTerminal).not.toHaveBeenCalled();
  });

  it("cancels terminal SSH auth prompt with Escape without submitting or retrying", async () => {
    mocks.api.createSshTerminalSession
      .mockRejectedValueOnce({
        terminalError: {
          class: "sshAuthRequired",
          message: "SSH 认证需要用户输入",
          operation: "createSession",
          recovery: "userActionRequired",
          retryable: false,
          sshAuthPromptPlan: {
            prompts: [
              {
                host: "dev.internal",
                port: 22,
                promptId: "ssh-auth:target:deploy@dev.internal:22:password",
                reason: "passwordPrompt",
                role: "target",
                secretKind: "password",
                username: "deploy",
              },
            ],
          },
        },
      })
      .mockResolvedValueOnce({
        cols: 80,
        id: "ssh-session-should-not-open",
        rows: 24,
        shell: "ssh",
        shellIntegration: {
          reason: "unexpected retry",
          status: "disabled",
        },
        status: "running",
      });

    render(
      <XtermPane
        focused
        paneId="pane-ssh-auth-prompt-escape-cancel"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="生产 SSH"
      />,
    );

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });
    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("deploy@dev.internal:22's password:"),
        ),
      ).toBe(true);
    });

    await act(async () => {
      mocks.terminalInstances[0].onDataCallback?.("\u001b");
    });

    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("SSH 认证已取消。"),
        ),
      ).toBe(true);
    });
    expect(
      mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
        String(data).includes("^C"),
      ),
    ).toBe(true);
    expect(mocks.api.requestSshAuthPrompt).not.toHaveBeenCalled();
    expect(mocks.api.submitSshAuthPromptResponse).not.toHaveBeenCalled();
    expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    expect(mocks.api.writeTerminal).not.toHaveBeenCalled();
  });

  it("supports backspace while reading SSH password auth in the terminal", async () => {
    mocks.api.createSshTerminalSession
      .mockRejectedValueOnce({
        terminalError: {
          class: "sshAuthRequired",
          message: "SSH 认证需要用户输入",
          operation: "createSession",
          recovery: "userActionRequired",
          retryable: false,
          sshAuthPromptPlan: {
            prompts: [
              {
                host: "dev.internal",
                port: 22,
                promptId: "ssh-auth:target:deploy@dev.internal:22:password",
                reason: "passwordPrompt",
                role: "target",
                secretKind: "password",
                username: "deploy",
              },
            ],
          },
        },
      })
      .mockImplementationOnce(async (_request, onOutput) => {
        mocks.setLatestOutputHandler(
          onOutput as Parameters<typeof mocks.setLatestOutputHandler>[0],
        );
        mocks.getLatestOutputHandler()?.({
          data: "managed ssh ready after edited password",
          kind: "data",
          sessionId: "ssh-session-after-edited-auth",
        });
        return {
          cols: 80,
          id: "ssh-session-after-edited-auth",
          rows: 24,
          shell: "ssh",
          shellIntegration: {
            reason: "remote test after edited auth",
            status: "disabled",
          },
          status: "running",
        };
      });

    render(
      <XtermPane
        focused
        paneId="pane-ssh-auth-prompt-backspace"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="生产 SSH"
      />,
    );

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });
    await waitFor(() => {
      expect(
        mocks.terminalInstances[0].write.mock.calls.some(([data]) =>
          String(data).includes("deploy@dev.internal:22's password:"),
        ),
      ).toBe(true);
    });

    await act(async () => {
      mocks.terminalInstances[0].onDataCallback?.(
        "terminal-typo\b\b\b\bsecret\r",
      );
    });

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(2);
    });
    expect(mocks.api.requestSshAuthPrompt).not.toHaveBeenCalled();
    expect(mocks.api.submitSshAuthPromptResponse).toHaveBeenCalledWith({
      promptId: "ssh-auth:target:deploy@dev.internal:22:password",
      secretKind: "password",
      value: "terminal-secret",
    });
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      expect.any(String),
      expect.stringContaining("terminal-typo"),
    );
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      expect.any(String),
      expect.stringContaining("terminal-secret"),
    );
    expect(screen.getByText("已连接")).not.toBeVisible();
  });

});
