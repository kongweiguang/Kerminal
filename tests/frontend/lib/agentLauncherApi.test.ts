// @author kongweiguang
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
const isTauriMock = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => isTauriMock(),
}));

describe("agentLauncherApi", () => {
  beforeEach(() => {
    vi.resetModules();
    invokeMock.mockReset();
    isTauriMock.mockReset();
  });

  it("archives an agent session through Tauri", async () => {
    isTauriMock.mockReturnValue(true);
    invokeMock.mockResolvedValue({
      session: {
        agentSessionId: "ags-1",
        launch: { args: [], cwd: "C:/sessions/ags-1", shell: "codex" },
        status: "archived",
        title: "Codex",
      },
    });
    const { archiveAgentSession } = await import("../../../src/lib/agentLauncherApi");

    await expect(archiveAgentSession("ags-1")).resolves.toMatchObject({
      session: { agentSessionId: "ags-1", status: "archived" },
    });
    expect(invokeMock).toHaveBeenCalledWith("agent_session_archive", {
      agentSessionId: "ags-1",
    });
  });

  it("returns an archived browser preview record outside Tauri", async () => {
    isTauriMock.mockReturnValue(false);
    const { archiveAgentSession, agentSessionRecordStatus } = await import(
      "../../../src/lib/agentLauncherApi"
    );

    const record = await archiveAgentSession("ags-preview");

    expect(record.session).toMatchObject({
      agentSessionId: "ags-preview",
      status: "archived",
      title: "Archived Agent Session",
    });
    expect(agentSessionRecordStatus(record)).toBe("archived");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("mirrors Claude's provider resume command in browser preview", async () => {
    isTauriMock.mockReturnValue(false);
    const { prepareExternalAgentWorkspace } = await import(
      "../../../src/lib/agentLauncherApi"
    );

    await expect(
      prepareExternalAgentWorkspace({
        agentId: "claude",
        agentSessionId: "ags-preview-claude",
        resumeProviderSession: true,
      }),
    ).resolves.toMatchObject({
      args: ["--continue"],
      cwd: "~/.kerminal/agents/sessions/ags-preview-claude",
      shell: "claude",
    });
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("previews native PI status and appends its real continue flag", async () => {
    isTauriMock.mockReturnValue(false);
    const {
      getExternalAgentWorkspaceStatus,
      prepareExternalAgentWorkspace,
    } = await import("../../../src/lib/agentLauncherApi");

    await expect(getExternalAgentWorkspaceStatus()).resolves.toMatchObject({
      agents: {
        pi: {
          adapterAvailable: true,
          cliCommand: "pi --approve",
          configPath: "~/.kerminal/.pi/mcp.json",
          id: "pi",
          title: "PI Agent",
        },
      },
    });
    await expect(
      prepareExternalAgentWorkspace({
        agentId: "pi",
        agentSessionId: "ags-preview-pi",
        resumeProviderSession: true,
      }),
    ).resolves.toMatchObject({
      args: ["--approve", "--continue"],
      cwd: "~/.kerminal/agents/sessions/ags-preview-pi",
      shell: "pi",
      title: "PI Agent",
    });
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("updates an agent session title through Tauri", async () => {
    isTauriMock.mockReturnValue(true);
    invokeMock.mockResolvedValue({
      session: {
        agentSessionId: "ags-title",
        launch: { args: [], cwd: "C:/sessions/ags-title", shell: "codex" },
        status: "active",
        title: "发布检查",
      },
    });
    const { updateAgentSession } = await import(
      "../../../src/lib/agentLauncherApi"
    );

    await updateAgentSession("ags-title", { title: "发布检查" });

    expect(invokeMock).toHaveBeenCalledWith("agent_session_update", {
      agentSessionId: "ags-title",
      request: { title: "发布检查" },
    });
  });

  it("persists the effective Agent launch parameters through Tauri", async () => {
    isTauriMock.mockReturnValue(true);
    invokeMock.mockResolvedValue({
      session: {
        agentSessionId: "ags-yolo",
        launch: {
          args: ["--dangerously-bypass-approvals-and-sandbox", "resume"],
          commandLabel:
            "codex --dangerously-bypass-approvals-and-sandbox resume",
          cwd: "C:/sessions/ags-yolo",
          shell: "codex",
        },
        status: "active",
        title: "Codex",
      },
    });
    const { updateAgentSession } = await import(
      "../../../src/lib/agentLauncherApi"
    );
    const launch = {
      args: ["--dangerously-bypass-approvals-and-sandbox", "resume"],
      commandLabel:
        "codex --dangerously-bypass-approvals-and-sandbox resume",
      cwd: "C:/sessions/ags-yolo",
      shell: "codex",
    };

    await updateAgentSession("ags-yolo", { launch });

    expect(invokeMock).toHaveBeenCalledWith("agent_session_update", {
      agentSessionId: "ags-yolo",
      request: {
        launch: {
          args: launch.args,
          command_label: launch.commandLabel,
          cwd: launch.cwd,
          shell: launch.shell,
        },
      },
    });
  });

  it("normalizes missing record status as active for legacy records", async () => {
    const { agentSessionRecordStatus } = await import("../../../src/lib/agentLauncherApi");

    expect(
      agentSessionRecordStatus({
        session: {
          agentSessionId: "ags-legacy",
          launch: { args: [], cwd: "C:/sessions/ags-legacy", shell: "codex" },
          title: "Codex",
        },
      }),
    ).toBe("active");
  });

  it("sends explicit tab scope without deriving it from the focused pane", async () => {
    isTauriMock.mockReturnValue(true);
    invokeMock.mockResolvedValue({
      session: {
        agentSessionId: "ags-tab",
        launch: { args: [], cwd: "C:/sessions/ags-tab", shell: "codex" },
        scope: { kind: "tab", tabId: "tab-main" },
        status: "active",
        title: "Codex",
      },
    });
    const { createAgentSession, agentSessionRecordScope } = await import(
      "../../../src/lib/agentLauncherApi"
    );

    const record = await createAgentSession({
      agentId: "codex",
      launcherKey: "builtin:codex",
      scope: { kind: "tab", tabId: "tab-main" },
    });

    expect(invokeMock).toHaveBeenCalledWith("agent_session_create", {
      request: {
        agentId: "codex",
        launcherKey: "builtin:codex",
        scope: { kind: "tab", tabId: "tab-main" },
      },
    });
    expect(agentSessionRecordScope(record)).toEqual({
      kind: "tab",
      tabId: "tab-main",
    });
  });

  it("reads launcherKey and Custom launch snapshots across wire naming styles", async () => {
    const {
      agentSessionRecordLaunchCommand,
      agentSessionRecordLauncherKey,
    } = await import("../../../src/lib/agentLauncherApi");
    const record = {
      session: {
        agent_id: "custom" as const,
        agent_session_id: "ags-custom",
        launcher_key: "custom:11111111-1111-4111-8111-111111111111",
        launch: {
          args: ["--mcp-config", ".mcp.json"],
          command_label: "pi --mcp-config .mcp.json",
          cwd: "C:/sessions/ags-custom",
          shell: "pi",
        },
        title: "PI",
      },
    };

    expect(agentSessionRecordLauncherKey(record)).toBe(
      "custom:11111111-1111-4111-8111-111111111111",
    );
    expect(agentSessionRecordLaunchCommand(record)).toBe(
      "pi --mcp-config .mcp.json",
    );
  });

  it("maps legacy unbound records to the stable global scope", async () => {
    const { agentSessionRecordScope } = await import(
      "../../../src/lib/agentLauncherApi"
    );

    expect(
      agentSessionRecordScope({
        session: {
          agentSessionId: "ags-legacy-global",
          launch: { args: [], cwd: "C:/sessions/legacy", shell: "codex" },
          target: { liveStatus: "unbound" },
          title: "Codex",
        },
      }),
    ).toEqual({ kind: "global" });
  });

  it("maps legacy global records with a saved target tab back to that tab", async () => {
    const { agentSessionRecordScope } = await import(
      "../../../src/lib/agentLauncherApi"
    );

    expect(
      agentSessionRecordScope({
        session: {
          agentSessionId: "ags-legacy-tab",
          launch: { args: [], cwd: "C:/sessions/legacy-tab", shell: "codex" },
          scope: { kind: "global" },
          target: { tabId: "tab-original" },
          title: "Codex",
        },
      }),
    ).toEqual({ kind: "tab", tabId: "tab-original" });
  });
});
