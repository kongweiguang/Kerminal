// @author kongweiguang

import { vi } from "vitest";

const mocks = vi.hoisted(() => {
  class MockTerminal {
    buffer: {
      active: {
        baseY: number;
        getLine: ReturnType<typeof vi.fn>;
        getNullCell: ReturnType<typeof vi.fn>;
        length: number;
        type: "normal";
        viewportY: number;
      };
      alternate: {
        baseY: number;
        getLine: ReturnType<typeof vi.fn>;
        getNullCell: ReturnType<typeof vi.fn>;
        length: number;
        type: "alternate";
        viewportY: number;
      };
      normal: {
        baseY: number;
        getLine: ReturnType<typeof vi.fn>;
        getNullCell: ReturnType<typeof vi.fn>;
        length: number;
        type: "normal";
        viewportY: number;
      };
      onBufferChange: ReturnType<typeof vi.fn>;
    };
    attachCustomKeyEventHandler = vi.fn();
    cols = 80;
    clear = vi.fn();
    dispose = vi.fn();
    focus = vi.fn();
    getSelection = vi.fn(() => "");
    loadAddon = vi.fn();
    open = vi.fn();
    options: Record<string, unknown>;
    paste = vi.fn();
    rows = 24;
    selectAll = vi.fn();
    write = vi.fn();
    private nextMarkerId = 1;
    private nextMarkerLine = 0;

    constructor(options: Record<string, unknown>) {
      this.options = options;
      const normalBuffer = {
        baseY: 0,
        getLine: vi.fn(),
        getNullCell: vi.fn(),
        length: 30,
        type: "normal" as const,
        viewportY: 0,
      };
      const alternateBuffer = {
        baseY: 0,
        getLine: vi.fn(),
        getNullCell: vi.fn(),
        length: 30,
        type: "alternate" as const,
        viewportY: 0,
      };
      this.buffer = {
        active: normalBuffer,
        alternate: alternateBuffer,
        normal: normalBuffer,
        onBufferChange: vi.fn(() => ({ dispose: vi.fn() })),
      };
    }

    /** Shell 测试不发送按键，仅实现运行时所需的 xterm 公共订阅生命周期。 */
    onKey() {
      return { dispose: vi.fn() };
    }

    onData() {
      return { dispose: vi.fn() };
    }

    onScroll() {
      return { dispose: vi.fn() };
    }

    onSelectionChange() {
      return { dispose: vi.fn() };
    }

    onWriteParsed() {
      return { dispose: vi.fn() };
    }

    registerMarker() {
      const marker = {
        dispose: vi.fn(),
        id: this.nextMarkerId,
        line: this.nextMarkerLine,
        onDispose: vi.fn(() => ({ dispose: vi.fn() })),
      };
      this.nextMarkerId += 1;
      this.nextMarkerLine += 3;
      return marker;
    }
  }

  class MockFitAddon {
    fit = vi.fn();
    proposeDimensions = vi.fn(() => ({ cols: 100, rows: 30 }));
  }

  class MockSearchAddon {
    onDidChangeResults = vi.fn(() => ({ dispose: vi.fn() }));
  }

  return {
    appTitleBar: {
      renderCount: 0,
    },
    commandHistoryApi: {
      listCommandHistory: vi.fn(async (_request: unknown) => []),
      recordCommandHistory: vi.fn(),
    },
    connectionApi: {
      openRdpConnection: vi.fn(),
      openSavedRdpConnection: vi.fn(),
    },
    diagnosticsApi: {
      createDiagnosticsBundle: vi.fn(),
      getManagedSshRuntimeSnapshot: vi.fn(),
      getRuntimeHealthSnapshot: vi.fn(),
    },
    dockerApi: {
      fetchDockerContainerStats: vi.fn(),
      inspectDockerContainer: vi.fn(),
      listDockerContainers: vi.fn(),
      removeDockerContainer: vi.fn(),
      restartDockerContainer: vi.fn(),
      startDockerContainer: vi.fn(),
      stopDockerContainer: vi.fn(),
      tailDockerContainerLogs: vi.fn(),
    },
    MockFitAddon,
    MockSearchAddon,
    MockTerminal,
    nativeMenuApi: {
      listenNativeMenuActions: vi.fn(),
    },
    profileApi: {
      createProfile: vi.fn(),
      detectShells: vi.fn(),
      listProfiles: vi.fn(),
      updateProfile: vi.fn(),
    },
    remoteHostApi: {
      createRemoteHost: vi.fn(),
      createRemoteHostGroup: vi.fn(),
      deleteRemoteHost: vi.fn(),
      deleteRemoteHostGroup: vi.fn(),
      listRemoteHostTree: vi.fn(),
      updateRemoteHost: vi.fn(),
      updateRemoteHostGroup: vi.fn(),
    },
    remoteWorkspaceEditorTransport: {
      readRemoteWorkspaceTextFile: vi.fn(),
      writeRemoteWorkspaceTextFile: vi.fn(),
    },
    serverInfoApi: {
      getServerInfoSnapshot: vi.fn(),
    },
    settingsApi: {
      getSettings: vi.fn(),
      updateSettings: vi.fn(),
    },
    terminalApi: {
      closeTerminal: vi.fn(),
      createSshTerminalSession: vi.fn(),
      createTerminalSession: vi.fn(),
      getTerminalLogState: vi.fn(),
      listTerminalSessions: vi.fn(),
      reapOrphanTerminalSessions: vi.fn(),
      resizeTerminal: vi.fn(),
      startTerminalLog: vi.fn(),
      stopTerminalLog: vi.fn(),
      writeTerminal: vi.fn(),
    },
    workspaceSessionApi: {
      loadWorkspaceSessionFile: vi.fn(),
      saveWorkspaceSessionFile: vi.fn(),
    },
  };
});

export function getKerminalShellTestMocks() {
  return mocks;
}

vi.mock("../../../../src/app/AppTitleBar", () => ({
  AppTitleBar: ({
    className,
    leftPanelCollapsed = false,
    onLeftPanelCollapsedChange,
  }: {
    className?: string;
    leftPanelCollapsed?: boolean;
    onLeftPanelCollapsedChange?: (collapsed: boolean) => void;
  }) => {
    mocks.appTitleBar.renderCount += 1;
    return (
      <header className={className} data-tauri-drag-region>
        {onLeftPanelCollapsedChange ? (
          <button
            aria-label={
              leftPanelCollapsed ? "展开主机侧边栏" : "折叠主机侧边栏"
            }
            aria-pressed={leftPanelCollapsed}
            onClick={() => onLeftPanelCollapsedChange(!leftPanelCollapsed)}
            type="button"
          />
        ) : null}
      </header>
    );
  },
}));

vi.mock("@xterm/xterm", () => ({
  Terminal: mocks.MockTerminal,
}));

vi.mock("@xterm/addon-fit", () => ({
  FitAddon: mocks.MockFitAddon,
}));

vi.mock("@xterm/addon-search", () => ({
  SearchAddon: mocks.MockSearchAddon,
}));

vi.mock("../../../../src/lib/commandHistoryApi", () => ({
  listCommandHistory: (request: unknown) =>
    mocks.commandHistoryApi.listCommandHistory(request),
  recordCommandHistory: (...args: unknown[]) =>
    mocks.commandHistoryApi.recordCommandHistory(...args),
}));

vi.mock("../../../../src/lib/connectionApi", () => ({
  openRdpConnection: (...args: unknown[]) =>
    mocks.connectionApi.openRdpConnection(...args),
  openSavedRdpConnection: (...args: unknown[]) =>
    mocks.connectionApi.openSavedRdpConnection(...args),
}));

vi.mock("../../../../src/lib/diagnosticsApi", () => ({
  createDiagnosticsBundle: (...args: unknown[]) =>
    mocks.diagnosticsApi.createDiagnosticsBundle(...args),
  getManagedSshRuntimeSnapshot: (...args: unknown[]) =>
    mocks.diagnosticsApi.getManagedSshRuntimeSnapshot(...args),
  getRuntimeHealthSnapshot: (...args: unknown[]) =>
    mocks.diagnosticsApi.getRuntimeHealthSnapshot(...args),
}));

vi.mock("../../../../src/lib/dockerApi", () => ({
  fetchDockerContainerStats: (...args: unknown[]) =>
    mocks.dockerApi.fetchDockerContainerStats(...args),
  inspectDockerContainer: (...args: unknown[]) =>
    mocks.dockerApi.inspectDockerContainer(...args),
  listDockerContainers: (...args: unknown[]) =>
    mocks.dockerApi.listDockerContainers(...args),
  removeDockerContainer: (...args: unknown[]) =>
    mocks.dockerApi.removeDockerContainer(...args),
  restartDockerContainer: (...args: unknown[]) =>
    mocks.dockerApi.restartDockerContainer(...args),
  startDockerContainer: (...args: unknown[]) =>
    mocks.dockerApi.startDockerContainer(...args),
  stopDockerContainer: (...args: unknown[]) =>
    mocks.dockerApi.stopDockerContainer(...args),
  tailDockerContainerLogs: (...args: unknown[]) =>
    mocks.dockerApi.tailDockerContainerLogs(...args),
}));

vi.mock("../../../../src/lib/nativeMenuApi", () => ({
  listenNativeMenuActions: (...args: unknown[]) =>
    mocks.nativeMenuApi.listenNativeMenuActions(...args),
}));

vi.mock("../../../../src/lib/profileApi", () => ({
  browserPreviewProfiles: [
    {
      args: [],
      createdAt: "test",
      env: {},
      id: "profile-test",
      isDefault: true,
      name: "Test Shell",
      shell: "test-shell",
      sortOrder: 10,
      updatedAt: "test",
    },
  ],
  createProfile: (...args: unknown[]) =>
    mocks.profileApi.createProfile(...args),
  detectShells: (...args: unknown[]) => mocks.profileApi.detectShells(...args),
  listProfiles: (...args: unknown[]) => mocks.profileApi.listProfiles(...args),
  updateProfile: (...args: unknown[]) =>
    mocks.profileApi.updateProfile(...args),
}));

vi.mock("../../../../src/lib/remoteHostApi", () => ({
  UNGROUPED_REMOTE_HOST_GROUP_ID: "__ungrouped__",
  createDefaultSshOptions: () => ({
    jumpHosts: [],
    proxy: { protocol: "none" },
    terminal: {
      altModifier: "8bit",
      backspaceKey: "ascii-delete",
      connectTimeoutSeconds: 30,
      deleteKey: "delete-sequence",
      encoding: "UTF-8",
      environment: "",
      keepaliveSeconds: 60,
      keyboardProfile: "default",
      loginScript: "",
      startupCommand: "",
      terminalType: "xterm-256color",
    },
    transfer: {
      enabled: true,
      followSymlinks: false,
      localStartDirectory: "",
      maxConcurrentTransfers: 4,
      preserveTimestamps: true,
      remoteStartDirectory: "",
    },
    tunnels: [],
  }),
  createRemoteHost: (...args: unknown[]) =>
    mocks.remoteHostApi.createRemoteHost(...args),
  createRemoteHostGroup: (...args: unknown[]) =>
    mocks.remoteHostApi.createRemoteHostGroup(...args),
  deleteRemoteHost: (...args: unknown[]) =>
    mocks.remoteHostApi.deleteRemoteHost(...args),
  deleteRemoteHostGroup: (...args: unknown[]) =>
    mocks.remoteHostApi.deleteRemoteHostGroup(...args),
  listRemoteHostTree: (...args: unknown[]) =>
    mocks.remoteHostApi.listRemoteHostTree(...args),
  updateRemoteHost: (...args: unknown[]) =>
    mocks.remoteHostApi.updateRemoteHost(...args),
  updateRemoteHostGroup: (...args: unknown[]) =>
    mocks.remoteHostApi.updateRemoteHostGroup(...args),
}));

vi.mock("../../../../src/lib/serverInfoApi", () => ({
  getServerInfoSnapshot: (...args: unknown[]) =>
    mocks.serverInfoApi.getServerInfoSnapshot(...args),
}));

vi.mock("../../../../src/features/settings/settingsApi", () => ({
  getSettings: (...args: unknown[]) => mocks.settingsApi.getSettings(...args),
  updateSettings: (...args: unknown[]) =>
    mocks.settingsApi.updateSettings(...args),
}));

vi.mock("../../../../src/lib/terminalApi", () => ({
  closeTerminal: (...args: unknown[]) =>
    mocks.terminalApi.closeTerminal(...args),
  createSshTerminalSession: (...args: unknown[]) =>
    mocks.terminalApi.createSshTerminalSession(...args),
  createTerminalSession: (...args: unknown[]) =>
    mocks.terminalApi.createTerminalSession(...args),
  getTerminalLogState: (...args: unknown[]) =>
    mocks.terminalApi.getTerminalLogState(...args),
  listTerminalSessions: (...args: unknown[]) =>
    mocks.terminalApi.listTerminalSessions(...args),
  reapOrphanTerminalSessions: (...args: unknown[]) =>
    mocks.terminalApi.reapOrphanTerminalSessions(...args),
  resizeTerminal: (...args: unknown[]) =>
    mocks.terminalApi.resizeTerminal(...args),
  startTerminalLog: (...args: unknown[]) =>
    mocks.terminalApi.startTerminalLog(...args),
  stopTerminalLog: (...args: unknown[]) =>
    mocks.terminalApi.stopTerminalLog(...args),
  writeTerminal: (...args: unknown[]) =>
    mocks.terminalApi.writeTerminal(...args),
}));

vi.mock("../../../../src/features/workspace/workspaceSessionApi", () => ({
  loadWorkspaceSessionFile: (...args: unknown[]) =>
    mocks.workspaceSessionApi.loadWorkspaceSessionFile(...args),
  saveWorkspaceSessionFile: (...args: unknown[]) =>
    mocks.workspaceSessionApi.saveWorkspaceSessionFile(...args),
}));

vi.mock("../../../../src/features/sftp/MonacoTextEditor", () => ({
  MonacoTextEditor: ({
    beforeMount,
    onChange,
    onMount,
    value,
  }: {
    beforeMount?: (monaco: unknown) => void;
    onChange?: (value: string) => void;
    onMount?: (editor: unknown, monaco: unknown) => void;
    value?: string;
  }) => {
    const editor = {
      addCommand: vi.fn(),
      focus: vi.fn(),
      getAction: vi.fn(() => ({ run: vi.fn() })),
      hasTextFocus: vi.fn(() => true),
    };
    const monaco = {
      KeyCode: {
        Insert: 52,
        KeyA: 31,
        KeyC: 33,
        KeyF: 36,
        KeyH: 38,
        KeyS: 49,
        KeyV: 55,
        KeyX: 56,
        KeyY: 57,
        KeyZ: 58,
      },
      KeyMod: {
        CtrlCmd: 2048,
        Shift: 1024,
      },
      editor: { defineTheme: vi.fn() },
    };
    beforeMount?.(monaco);
    onMount?.(editor, monaco);
    return (
      <textarea
        aria-label="Compose YAML Monaco editor"
        onChange={(event) => onChange?.(event.target.value)}
        value={value ?? ""}
      />
    );
  },
}));

vi.mock("../../../../src/features/sftp/remoteWorkspaceEditorTransport", () => ({
  readRemoteWorkspaceTextFile: (...args: unknown[]) =>
    mocks.remoteWorkspaceEditorTransport.readRemoteWorkspaceTextFile(...args),
  writeRemoteWorkspaceTextFile: (...args: unknown[]) =>
    mocks.remoteWorkspaceEditorTransport.writeRemoteWorkspaceTextFile(...args),
}));

vi.mock("../../../../src/features/sftp/SftpToolContent", () => ({
  SftpToolContent: ({
    selectedMachine,
  }: {
    selectedMachine?: { id: string; name: string };
  }) => (
    <div aria-label="SFTP 工具内容">
      SFTP:{selectedMachine?.id ?? "none"}:{selectedMachine?.name ?? "none"}
    </div>
  ),
}));

vi.mock("../../../../src/features/sftp/LazySftpTransferWorkbench", () => ({
  LazySftpTransferWorkbench: ({
    createdHostTarget,
    initialRightHostId,
    lockedLeftHostId,
    onCreateSshHost,
    workspaceTabId,
  }: {
    createdHostTarget?: {
      hostId: string;
      sequence: number;
      side: "left" | "right";
      workspaceTabId?: string;
    };
    initialRightHostId?: string;
    lockedLeftHostId?: string;
    onCreateSshHost?: (request: {
      side: "left" | "right";
      workspaceTabId?: string;
    }) => void;
    workspaceTabId?: string;
  }) => (
    <div aria-label="SFTP 传输工作台">
      right:{initialRightHostId ?? "none"} locked:
      {lockedLeftHostId ?? "none"}
      <span>
        created:{createdHostTarget?.workspaceTabId ?? "none"}:
        {createdHostTarget?.side ?? "none"}:
        {createdHostTarget?.hostId ?? "none"}
      </span>
      <button
        onClick={() => onCreateSshHost?.({ side: "left", workspaceTabId })}
        type="button"
      >
        从左侧新建 SSH 主机
      </button>
      <button
        onClick={() => onCreateSshHost?.({ side: "right", workspaceTabId })}
        type="button"
      >
        从右侧新建 SSH 主机
      </button>
    </div>
  ),
}));

export const testSshOptions = {
  jumpHosts: [
    {
      authType: "agent" as const,
      host: "jump.internal",
      name: "jump",
      port: 22,
      username: "proxy",
    },
  ],
  proxy: { protocol: "none" as const },
  terminal: {
    altModifier: "8bit",
    backspaceKey: "ascii-delete",
    connectTimeoutSeconds: 15,
    deleteKey: "delete-sequence",
    encoding: "UTF-8",
    environment: "LANG=zh_CN.UTF-8",
    keepaliveSeconds: 45,
    keyboardProfile: "default",
    loginScript: "",
    startupCommand: "whoami",
    terminalType: "xterm-256color",
  },
  transfer: {
    enabled: true,
    followSymlinks: false,
    localStartDirectory: "",
    maxConcurrentTransfers: 4,
    preserveTimestamps: true,
    remoteStartDirectory: "/home/ubuntu",
  },
  tunnels: [],
};

export const remoteHostTree = [
  {
    createdAt: "2026-06-18 03:35:44",
    hosts: [
      {
        authType: "agent",
        createdAt: "2026-06-18 06:51:59",
        credentialRef: undefined,
        groupId: "30fbc381-2884-4b75-9f88-0e28f31ca8b0",
        host: "172.16.41.60",
        id: "db980b17-2ed0-44e5-b72a-6ecadf788439",
        name: "172.16.41.60",
        port: 22,
        sshOptions: testSshOptions,
        sortOrder: 10,
        tags: ["ssh", "bbb"],
        updatedAt: "2026-06-18 08:42:40",
        username: "ubuntu",
      },
    ],
    id: "30fbc381-2884-4b75-9f88-0e28f31ca8b0",
    name: "bwy",
    sortOrder: 30,
    updatedAt: "2026-06-18 03:35:44",
  },
];

export const remoteHostTreeWithTargetGroup = [
  ...remoteHostTree,
  {
    createdAt: "2026-06-18 03:35:44",
    hosts: [],
    id: "group-tools",
    name: "工具",
    sortOrder: 40,
    updatedAt: "2026-06-18 03:35:44",
  },
];

export const remoteHostTreeWithPinnedTargetGroup = [
  ...remoteHostTree,
  {
    createdAt: "2026-06-18 03:35:44",
    hosts: [],
    id: "group-tools",
    name: "工具",
    sortOrder: -10,
    updatedAt: "2026-06-18 03:35:44",
  },
];

export const rdpRemoteHostTree = [
  {
    createdAt: "2026-06-19 10:00:00",
    hosts: [
      {
        authType: "password",
        createdAt: "2026-06-19 10:00:00",
        credentialRef: "credential:rdp/rdp-office/password",
        groupId: "group-office",
        host: "rdp.internal",
        id: "rdp-office",
        name: "office-rdp",
        port: 3389,
        sortOrder: 10,
        tags: ["rdp"],
        updatedAt: "2026-06-19 10:00:00",
        username: "administrator",
      },
    ],
    id: "group-office",
    name: "办公主机",
    sortOrder: 10,
    updatedAt: "2026-06-19 10:00:00",
  },
];

export function mockElementFromPoint(element: Element) {
  const documentWithElementFromPoint = document as Document & {
    elementFromPoint?: Document["elementFromPoint"];
  };
  const originalElementFromPoint =
    documentWithElementFromPoint.elementFromPoint;
  Object.defineProperty(document, "elementFromPoint", {
    configurable: true,
    value: vi.fn(() => element),
  });

  return () => {
    if (originalElementFromPoint) {
      Object.defineProperty(document, "elementFromPoint", {
        configurable: true,
        value: originalElementFromPoint,
      });
      return;
    }
    Reflect.deleteProperty(documentWithElementFromPoint, "elementFromPoint");
  };
}
