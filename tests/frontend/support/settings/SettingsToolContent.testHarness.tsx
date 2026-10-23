// @author kongweiguang
import { render, screen } from "@testing-library/react";
import type { RenderResult } from "@testing-library/react";
import type userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, vi } from "vitest";
import { SettingsToolContent } from "../../../../src/features/settings/SettingsToolContent";
import {
  appUpdateInitialState,
  useAppUpdateStore,
} from "../../../../src/features/settings/appUpdateStore";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import type { AppSettings } from "../../../../src/features/settings/settingsModel";
import type {
  SettingsSaveState,
  SettingsSectionId,
} from "../../../../src/features/settings/settings-tool-content/types";

const settingsToolContentMocks = vi.hoisted(() => ({
  agentLauncherApiMock: {
    getExternalAgentWorkspaceStatus: vi.fn(),
  },
  clipboardMock: {
    writeText: vi.fn(),
  },
  desktopNotificationApiMock: {
    currentDesktopNotificationVisibility: vi.fn(),
    sendDesktopNotification: vi.fn(),
  },
  fileDialogMock: {
    openLocalDirectory: vi.fn(),
    selectLocalFile: vi.fn(),
  },
  openerApiMock: {
    openUrl: vi.fn(),
  },
  tauriCoreMock: {
    isTauri: vi.fn(),
  },
  terminalSuggestionApiMock: {
    cleanupTerminalSuggestionDiagnostics: vi.fn(),
    getTerminalSuggestionTelemetryExport: vi.fn(),
    getTerminalSuggestionTelemetrySummary: vi.fn(),
  },
  mcpServerApiMock: {
    getMcpHttpServerStatus: vi.fn(),
    startMcpHttpServer: vi.fn(),
    stopMcpHttpServer: vi.fn(),
  },
  updaterApiMock: {
    checkForAppUpdate: vi.fn(),
    installPendingAppUpdate: vi.fn(),
    relaunchApp: vi.fn(),
  },
}));

const {
  agentLauncherApiMock,
  clipboardMock,
  desktopNotificationApiMock,
  fileDialogMock,
  openerApiMock,
  tauriCoreMock,
  terminalSuggestionApiMock,
  mcpServerApiMock,
  updaterApiMock,
} = settingsToolContentMocks;

const AGENT_WORKSPACE_DIRECTORY = "C:\\Users\\dev\\.kerminal";
export const AGENT_MCP_ENDPOINT = "http://127.0.0.1:30456/mcp";

vi.mock(
  "../../../../src/lib/agentLauncherApi",
  () => settingsToolContentMocks.agentLauncherApiMock,
);
vi.mock(
  "../../../../src/lib/desktopNotificationApi",
  () => settingsToolContentMocks.desktopNotificationApiMock,
);
vi.mock("../../../../src/features/settings/settings-tool-content/clipboard", () => ({
  writeTextToClipboard: settingsToolContentMocks.clipboardMock.writeText,
}));

vi.mock("../../../../src/lib/fileDialogApi", () => ({
  openLocalDirectory: settingsToolContentMocks.fileDialogMock.openLocalDirectory,
  selectLocalFile: settingsToolContentMocks.fileDialogMock.selectLocalFile,
}));

vi.mock(
  "../../../../src/lib/mcpServerApi",
  () => settingsToolContentMocks.mcpServerApiMock,
);
vi.mock(
  "../../../../src/lib/terminalSuggestionApi",
  () => settingsToolContentMocks.terminalSuggestionApiMock,
);
vi.mock("../../../../src/lib/updaterApi", () => settingsToolContentMocks.updaterApiMock);
vi.mock("@tauri-apps/api/core", () => ({
  isTauri: settingsToolContentMocks.tauriCoreMock.isTauri,
}));
vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: settingsToolContentMocks.openerApiMock.openUrl,
}));

interface RenderSettingsToolContentOptions {
  initialSectionId?: SettingsSectionId;
  onConfirmedSettingsChange?: (settings: AppSettings) => Promise<AppSettings>;
  onSettingsChange?: (settings: AppSettings) => void;
  resolvedTheme?: "light" | "dark";
  saveError?: string | null;
  saveState?: SettingsSaveState;
  settings?: AppSettings;
}

export async function chooseSelectOption(
  user: ReturnType<typeof userEvent.setup>,
  label: string,
  optionName: string,
) {
  await user.click(screen.getByRole("combobox", { name: label }));
  await user.click(
    screen.getByRole("option", { name: new RegExp(`^${optionName}`) }),
  );
}

function installClipboardMock() {
  const clipboard = {
    writeText: clipboardMock.writeText,
  };
  Object.defineProperty(window.navigator, "clipboard", {
    configurable: true,
    value: clipboard,
  });
  Object.defineProperty(globalThis.navigator, "clipboard", {
    configurable: true,
    value: clipboard,
  });
}

export function renderSettingsToolContent({
  initialSectionId,
  onConfirmedSettingsChange,
  onSettingsChange = vi.fn(),
  resolvedTheme,
  saveError,
  saveState,
  settings = defaultAppSettings,
}: RenderSettingsToolContentOptions = {}): RenderResult {
  return render(
    <SettingsToolContent
      initialSectionId={initialSectionId}
      onConfirmedSettingsChange={onConfirmedSettingsChange}
      onSettingsChange={onSettingsChange}
      resolvedTheme={resolvedTheme}
      saveError={saveError}
      saveState={saveState}
      settings={settings}
    />,
  );
}

export function renderControlledSettings({
  initialSectionId,
  onConfirmedSettingsChange,
  onSettingsChange,
  settings: initialSettings = defaultAppSettings,
}: {
  initialSectionId?: SettingsSectionId;
  onConfirmedSettingsChange?: (settings: AppSettings) => Promise<AppSettings>;
  onSettingsChange: (settings: AppSettings) => void;
  settings?: AppSettings;
}): RenderResult {
  function ControlledSettings() {
    const [settings, setSettings] = useState(initialSettings);

    return (
      <SettingsToolContent
        initialSectionId={initialSectionId}
        onConfirmedSettingsChange={async (nextSettings) => {
          const storedSettings = onConfirmedSettingsChange
            ? await onConfirmedSettingsChange(nextSettings)
            : nextSettings;
          setSettings(storedSettings);
          return storedSettings;
        }}
        onSettingsChange={(nextSettings) => {
          setSettings(nextSettings);
          onSettingsChange(nextSettings);
        }}
        settings={settings}
      />
    );
  }

  return render(<ControlledSettings />);
}

/** 设置内容与更新 store 都是模块级状态，统一恢复可避免前一用例泄漏到下一用例。 */
function resetSettingsToolContentMocks() {
  agentLauncherApiMock.getExternalAgentWorkspaceStatus.mockReset();
  agentLauncherApiMock.getExternalAgentWorkspaceStatus.mockResolvedValue({
    agents: {
      claude: {
        cliCommand: "claude",
        configPath: `${AGENT_WORKSPACE_DIRECTORY}\\.mcp.json`,
        configReady: true,
        id: "claude",
        installed: true,
        statusDetail: "Claude CLI detected.",
        title: "Claude",
      },
      codex: {
        cliCommand: "codex",
        configPath: `${AGENT_WORKSPACE_DIRECTORY}\\.codex\\config.toml`,
        configReady: true,
        id: "codex",
        installed: true,
        statusDetail: "Codex CLI detected.",
        title: "Codex",
      },
      custom: {
        cliCommand: "",
        configPath: "",
        configReady: false,
        id: "custom",
        installed: false,
        statusDetail: "Custom Agent is not initialized by default.",
        title: "Custom",
      },
    },
    mcpEndpoint: AGENT_MCP_ENDPOINT,
    mcpServerRunning: true,
    validator: {
      available: true,
      command:
        'node "C:\\\\dev\\\\rust\\\\kerminal\\\\.codex\\\\skills\\\\bwy-kerminal-config-files\\\\scripts\\\\validate-kerminal-config.mjs" --root "C:\\\\Users\\\\dev\\\\.kerminal"',
      detail: "Run this after editing Kerminal configuration files.",
      status: "available",
    },
    workspaceDir: AGENT_WORKSPACE_DIRECTORY,
  });

  fileDialogMock.openLocalDirectory.mockReset();
  fileDialogMock.openLocalDirectory.mockResolvedValue(undefined);
  fileDialogMock.selectLocalFile.mockReset();

  openerApiMock.openUrl.mockReset();
  openerApiMock.openUrl.mockResolvedValue(undefined);
  tauriCoreMock.isTauri.mockReset();
  tauriCoreMock.isTauri.mockReturnValue(true);

  mcpServerApiMock.getMcpHttpServerStatus.mockReset();
  mcpServerApiMock.getMcpHttpServerStatus.mockResolvedValue({
    bindAddress: "127.0.0.1",
    endpoint: "http://127.0.0.1:30456/mcp",
    localOnly: true,
    port: 30456,
    running: true,
  });
  mcpServerApiMock.startMcpHttpServer.mockReset();
  mcpServerApiMock.startMcpHttpServer.mockResolvedValue({
    bindAddress: "127.0.0.1",
    endpoint: "http://127.0.0.1:30456/mcp",
    localOnly: true,
    port: 30456,
    running: true,
  });
  mcpServerApiMock.stopMcpHttpServer.mockReset();
  mcpServerApiMock.stopMcpHttpServer.mockResolvedValue({
    bindAddress: "127.0.0.1",
    endpoint: null,
    localOnly: true,
    port: null,
    running: false,
  });

  clipboardMock.writeText.mockReset();
  clipboardMock.writeText.mockResolvedValue(undefined);
  installClipboardMock();

  desktopNotificationApiMock.currentDesktopNotificationVisibility.mockReset();
  desktopNotificationApiMock.currentDesktopNotificationVisibility.mockReturnValue(
    "hidden",
  );
  desktopNotificationApiMock.sendDesktopNotification.mockReset();
  desktopNotificationApiMock.sendDesktopNotification.mockResolvedValue({
    reason: "will-send",
    requestedPermission: false,
    sent: true,
  });

  terminalSuggestionApiMock.getTerminalSuggestionTelemetryExport.mockReset();
  terminalSuggestionApiMock.getTerminalSuggestionTelemetryExport.mockResolvedValue(
    {
      auditEvents: [],
      generatedAtUnixMs: 1760000000200,
      persisted: {
        generatedAtUnixMs: 1760000000200,
        providers: [],
        startedAtUnixMs: 1760000000000,
        totalCandidateCount: 6,
        totalQueryCount: 4,
      },
      runtime: {
        generatedAtUnixMs: 1760000000200,
        providers: [],
        startedAtUnixMs: 1760000000100,
        totalCandidateCount: 6,
        totalQueryCount: 4,
      },
    },
  );
  terminalSuggestionApiMock.cleanupTerminalSuggestionDiagnostics.mockReset();
  terminalSuggestionApiMock.cleanupTerminalSuggestionDiagnostics.mockResolvedValue(
    {
      auditEventsDeleted: 2,
      feedbackDeleted: 1,
      generatedAtUnixMs: 1760000000300,
      providerCacheDeleted: 3,
      telemetryRowsDeleted: 0,
    },
  );
  terminalSuggestionApiMock.getTerminalSuggestionTelemetrySummary.mockReset();
  terminalSuggestionApiMock.getTerminalSuggestionTelemetrySummary.mockResolvedValue(
    {
      generatedAtUnixMs: 1760000000100,
      providers: [
        {
          averageElapsedMs: 1.5,
          cacheHitCount: 5,
          cacheMissCount: 1,
          candidateCount: 6,
          feedbackAcceptedCount: 2,
          feedbackDismissedCount: 1,
          feedbackSkippedCount: 0,
          lastEventUnixMs: 1760000000100,
          provider: "remoteCommand",
          queryCount: 4,
          refreshFailureCount: 0,
          refreshSuccessCount: 1,
          totalElapsedMs: 6,
        },
      ],
      startedAtUnixMs: 1760000000000,
      totalCandidateCount: 6,
      totalQueryCount: 4,
    },
  );

  updaterApiMock.checkForAppUpdate.mockReset();
  updaterApiMock.checkForAppUpdate.mockResolvedValue({ kind: "up-to-date" });
  updaterApiMock.installPendingAppUpdate.mockReset();
  updaterApiMock.installPendingAppUpdate.mockResolvedValue(undefined);
  updaterApiMock.relaunchApp.mockReset();
  updaterApiMock.relaunchApp.mockResolvedValue(undefined);
  useAppUpdateStore.setState(appUpdateInitialState);
}

beforeEach(() => {
  resetSettingsToolContentMocks();
});

export {
  clipboardMock,
  defaultAppSettings,
  desktopNotificationApiMock,
  fileDialogMock,
  mcpServerApiMock,
  openerApiMock,
  terminalSuggestionApiMock,
  updaterApiMock,
};
