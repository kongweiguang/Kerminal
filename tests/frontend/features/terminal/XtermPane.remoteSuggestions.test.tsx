// @author kongweiguang

import { act, render, screen, waitFor } from "@testing-library/react";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import { describe, expect, it } from "vitest";
import { mocks } from "../../support/terminal/XtermPane.testSupport.tsx";
import { terminalAppearanceWithInlineSuggestion } from "../../support/terminal/XtermPane.testUtilities";
import { XtermPane } from "../../../../src/features/terminal/XtermPane";

describe("XtermPane remote suggestions", () => {
  it("hides the ghost suggestion while viewing scrollback and restores it at the bottom", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-git-scroll",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-git-scroll",
        suffix: " status --short",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    expect(
      (await screen.findByLabelText("终端命令灰色提示")).textContent,
    ).toBe(" status --short");

    act(() => {
      terminal.buffer.active.baseY = 12;
      terminal.buffer.active.viewportY = 5;
      terminal.onScrollCallback?.(5);
    });
    expect(screen.queryByLabelText("终端命令灰色提示")).not.toBeInTheDocument();

    act(() => {
      terminal.buffer.active.viewportY = 12;
      terminal.onScrollCallback?.(12);
    });
    expect(screen.getByLabelText("终端命令灰色提示").textContent).toBe(
      " status --short",
    );
  });

  it("drops in-flight ghost suggestion responses after entering alternate buffer", async () => {
    let resolveFirstSuggestion: ((value: unknown) => void) | undefined;
    mocks.api.listTerminalSuggestions
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveFirstSuggestion = resolve;
          }),
      )
      .mockResolvedValueOnce([
        {
          description: "历史命令，匹配当前目录",
          displayText: "ls -la",
          id: "history-ls-1",
          provider: "history",
          replacementRange: { end: 2, start: 0 },
          replacementText: "ls -la",
          score: 0.9,
          sensitivity: "normal",
          sourceId: "history-ls-1",
          suffix: " -la",
        },
      ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "git",
        }),
      );
    });

    act(() => {
      terminal.buffer.active = terminal.buffer.alternate;
      terminal.onBufferChangeCallback?.();
    });

    await act(async () => {
      resolveFirstSuggestion?.([
        {
          description: "历史命令，匹配当前目录",
          displayText: "git status --short",
          id: "history-git-1",
          provider: "history",
          replacementRange: { end: 3, start: 0 },
          replacementText: "git status --short",
          score: 0.9,
          sensitivity: "normal",
          sourceId: "history-git-1",
          suffix: " status --short",
        },
      ]);
      await Promise.resolve();
    });

    expect(screen.queryByLabelText("终端命令灰色提示")).not.toBeInTheDocument();

    act(() => {
      terminal.buffer.active = terminal.buffer.normal;
      terminal.onBufferChangeCallback?.();
      terminal.buffer.active.cursorX = 2;
      terminal.onDataCallback?.("l");
      terminal.onDataCallback?.("s");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "ls",
        }),
      );
    });
    const ghostSuggestion = await screen.findByLabelText("终端命令灰色提示");
    expect(ghostSuggestion.textContent).toBe(" -la");
    expect(ghostSuggestion.textContent).not.toBe(" status --short");
  });

  it("suppresses pasted input, drops stale responses, and recovers on the next prompt", async () => {
    let resolveFirstSuggestion: ((value: unknown) => void) | undefined;
    mocks.api.listTerminalSuggestions
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveFirstSuggestion = resolve;
          }),
      )
      .mockResolvedValueOnce([
        {
          description: "历史命令，匹配当前目录",
          displayText: "ls -la",
          id: "history-ls-1",
          provider: "history",
          replacementRange: { end: 2, start: 0 },
          replacementText: "ls -la",
          score: 0.9,
          sensitivity: "normal",
          sourceId: "history-ls-1",
          suffix: " -la",
        },
      ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "git",
        }),
      );
    });

    mocks.api.listTerminalSuggestions.mockClear();
    act(() => {
      terminal.onDataCallback?.(" status\nls");
    });

    expect(mocks.api.writeTerminal).toHaveBeenCalledWith(
      "session-1",
      " status\nls",
    );
    expect(mocks.api.listTerminalSuggestions).not.toHaveBeenCalled();

    await act(async () => {
      resolveFirstSuggestion?.([
        {
          description: "历史命令，匹配当前目录",
          displayText: "git status --short",
          id: "history-git-1",
          provider: "history",
          replacementRange: { end: 3, start: 0 },
          replacementText: "git status --short",
          score: 0.9,
          sensitivity: "normal",
          sourceId: "history-git-1",
          suffix: " status --short",
        },
      ]);
      await Promise.resolve();
    });

    expect(screen.queryByLabelText("终端命令灰色提示")).not.toBeInTheDocument();

    mocks.api.writeTerminal.mockClear();
    mocks.api.recordTerminalSuggestionFeedback.mockClear();
    act(() => {
      terminal.triggerKeyData("\u001b[C");
    });

    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\u001b[C");
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      "session-1",
      " status --short",
    );
    expect(mocks.api.recordTerminalSuggestionFeedback).not.toHaveBeenCalledWith(
      expect.objectContaining({
        action: "accepted",
      }),
    );

    act(() => {
      terminal.onDataCallback?.("\r");
      terminal.buffer.active.cursorX = 2;
      terminal.onDataCallback?.("l");
      terminal.onDataCallback?.("s");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "ls",
        }),
      );
    });
    const ghostSuggestion = await screen.findByLabelText("终端命令灰色提示");
    expect(ghostSuggestion.textContent).toBe(" -la");
  });

  it("does not request ghost suggestions when inline suggestions are disabled", async () => {
    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={terminalAppearanceWithInlineSuggestion({
          enabled: false,
        })}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    expect(mocks.api.listTerminalSuggestions).not.toHaveBeenCalled();
    expect(screen.queryByLabelText("终端命令灰色提示")).not.toBeInTheDocument();
  });

  it("passes right arrow through when accepting ghost suggestions is disabled", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-1",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-1",
        suffix: " status --short",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={terminalAppearanceWithInlineSuggestion({
          acceptKey: "disabled",
        })}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    const ghostSuggestion = await screen.findByLabelText("终端命令灰色提示");
    expect(ghostSuggestion.textContent).toBe(" status --short");

    mocks.api.writeTerminal.mockClear();
    act(() => {
      terminal.triggerKeyData("\u001b[C");
    });

    expect(mocks.api.writeTerminal).toHaveBeenCalledWith(
      "session-1",
      "\u001b[C",
    );
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      "session-1",
      " status --short",
    );
  });

  it("uses remote path provider and prewarms the cwd for ssh terminals", async () => {
    render(
      <XtermPane
        currentCwd="/srv/app"
        focused
        paneId="pane-ssh"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="生产 SSH"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    await waitFor(() => {
      expect(
        mocks.api.refreshTerminalRemoteCommandSuggestions,
      ).toHaveBeenCalledWith({
        hostId: "host-prod",
        maxEntries: 1500,
        ttlSeconds: 300,
      });
    });
    await waitFor(() => {
      expect(mocks.api.refreshTerminalGitSuggestions).toHaveBeenCalledWith({
        cwd: "/srv/app",
        hostId: "host-prod",
        maxEntries: 500,
        ttlSeconds: 60,
      });
    });
    await waitFor(() => {
      expect(
        mocks.api.refreshTerminalRemoteHistorySuggestions,
      ).toHaveBeenCalledWith({
        hostId: "host-prod",
        maxEntries: 1000,
        ttlSeconds: 900,
      });
    });
    await waitFor(() => {
      expect(
        mocks.api.refreshTerminalRemotePathSuggestions,
      ).toHaveBeenCalledWith({
        hostId: "host-prod",
        maxEntries: 250,
        path: "/srv/app",
        ttlSeconds: 30,
      });
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 2;
    act(() => {
      terminal.onDataCallback?.("l");
      terminal.onDataCallback?.("s");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "ls",
          providers: [
            "history",
            "remotePath",
            "remoteCommand",
            "git",
            "spec",
            "snippet",
          ],
          remoteHostId: "host-prod",
          target: "ssh",
        }),
      );
    });
  });

  it("skips remote prewarm and remote providers when remote probing is disabled", async () => {
    render(
      <XtermPane
        currentCwd="/srv/app"
        focused
        paneId="pane-ssh"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={terminalAppearanceWithInlineSuggestion({
          remoteProbeEnabled: false,
        })}
        title="生产 SSH"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 850));
    });

    expect(mocks.api.refreshTerminalGitSuggestions).not.toHaveBeenCalled();
    expect(
      mocks.api.refreshTerminalRemoteCommandSuggestions,
    ).not.toHaveBeenCalled();
    expect(
      mocks.api.refreshTerminalRemoteHistorySuggestions,
    ).not.toHaveBeenCalled();
    expect(
      mocks.api.refreshTerminalRemotePathSuggestions,
    ).not.toHaveBeenCalled();
    expect(mocks.api.recordTerminalSuggestionAuditEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        decision: "skipped",
        eventKind: "remoteProbeSchedule",
        provider: "remoteCommand",
        reason: "remote-probe-disabled",
        remoteHostId: "host-prod",
        target: "ssh",
      }),
    );
    expect(mocks.api.recordTerminalSuggestionAuditEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        decision: "skipped",
        eventKind: "remoteProbeSchedule",
        provider: "history",
        reason: "remote-probe-disabled",
        remoteHostId: "host-prod",
        target: "ssh",
      }),
    );

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 2;
    act(() => {
      terminal.onDataCallback?.("l");
      terminal.onDataCallback?.("s");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "ls",
          providers: ["history", "spec", "snippet"],
          remoteHostId: "host-prod",
          target: "ssh",
        }),
      );
    });
  });

  it("allows remote prewarm and remote providers when remote probing is enabled", async () => {
    render(
      <XtermPane
        currentCwd="/srv/app"
        focused
        paneId="pane-ssh"
        remoteHostId="host-prod"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="SSH"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    await waitFor(() => {
      expect(
        mocks.api.refreshTerminalRemoteCommandSuggestions,
      ).toHaveBeenCalledWith({
        hostId: "host-prod",
        maxEntries: 1500,
        ttlSeconds: 300,
      });
    });
    await waitFor(() => {
      expect(
        mocks.api.refreshTerminalRemoteHistorySuggestions,
      ).toHaveBeenCalledWith({
        hostId: "host-prod",
        maxEntries: 1000,
        ttlSeconds: 900,
      });
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 2;
    act(() => {
      terminal.onDataCallback?.("l");
      terminal.onDataCallback?.("s");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "ls",
          providers: [
            "history",
            "remotePath",
            "remoteCommand",
            "git",
            "spec",
            "snippet",
          ],
          remoteHostId: "host-prod",
          target: "ssh",
        }),
      );
    });
  });

});
