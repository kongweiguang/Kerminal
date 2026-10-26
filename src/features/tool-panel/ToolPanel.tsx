/**
 * @author kongweiguang
 */

import { lazy, Suspense, useEffect, useMemo, useState } from "react";
import { RenderErrorBoundary } from "../../components/RenderErrorBoundary";
import { cn } from "../../lib/cn";
import {
  defaultTerminalAppearance,
  type AppSettings,
  type ResolvedTheme,
  type TerminalAppearance,
} from "../settings/contracts/index";
import type { SettingsSectionId } from "../settings/view/index";
import type {
  Machine,
  MachineGroup,
  TerminalPane,
  TerminalTab,
  ToolId,
  ToolSummary,
  WorkspaceFileDirtyState,
  WorkspaceFileRevealRequest,
} from "../workspace/contracts/index";
import { isWorkspaceFileTab } from "../workspace/contracts/index";
import type {
  AddTerminalTabOptions,
  OpenWorkspaceFileTabOptions,
  TmuxAttachPlacement,
} from "../workspace/state/index";
import type { WorkspaceContextProjection } from "../workspace/context";
import type { TerminalArtifactActionRequest } from "../terminal/artifacts/public/index";
import type { TmuxAttachLaunch } from "../../lib/tmuxApi";
import { sftpSidebarTransferViewScope } from "../sftp/tool/scope/index";
import {
  claimAgentSendRequestAutoOpen,
  useAgentSendRequestSnapshot,
} from "../agent-workflow/state/index";
import {
  resolveToolPanelBinding,
  type HostContainerRevealRequest,
} from "./toolPanelContextModel";
import { ToolRail } from "./ToolRail";
import type { HostContainersToolContentProps } from "../machine-sidebar";

/** Shell 继续拥有业务操作，内容宿主只传递当前容器的动作，避免再造一套服务适配。 */
export type ContainerToolActions = Pick<
  HostContainersToolContentProps,
  | "onEnterContainer"
  | "onFetchContainerStats"
  | "onInspectContainer"
  | "onLifecycleContainer"
  | "onListDockerContainers"
  | "onOpenContainerLogs"
  | "onPinContainer"
>;

interface ToolPanelProps {
  activeTool: ToolId | null;
  activeTools?: readonly ToolId[];
  activeMachine?: Machine;
  machineGroups?: MachineGroup[];
  containerActions?: ContainerToolActions;
  containerRevealRequest?: HostContainerRevealRequest;
  activeTab?: TerminalTab;
  defaultRemoteGroupId?: string;
  defaultRemoteHostId?: string;
  focusedPane?: TerminalPane;
  selectedMachine?: Machine;
  terminalPanes?: TerminalPane[];
  terminalTabs?: TerminalTab[];
  workspaceFileDirtyState?: WorkspaceFileDirtyState;
  tools: ToolSummary[];
  settings?: AppSettings;
  showRail?: boolean;
  visible?: boolean;
  snippetConfigRevision?: number;
  sftpRevealRequest?: WorkspaceFileRevealRequest | null;
  resolvedTheme?: ResolvedTheme;
  terminalAppearance?: TerminalAppearance;
  workflowConfigRevision?: number;
  workspaceContext?: WorkspaceContextProjection;
  onArtifactActionRequest?: (request: TerminalArtifactActionRequest) => void;
  onActiveToolChange: (toolId: ToolId) => void;
  onOpenTool?: (toolId: ToolId) => void;
  onCreateTerminal?: (options?: AddTerminalTabOptions) => void;
  onFocusTab?: (tabId: string) => void;
  onOpenToolRailCustomization?: () => void;
  onClosePane?: (paneId: string) => void;
  onOpenSettingsSection?: (sectionId: SettingsSectionId) => void;
  onOpenSshTerminal?: (hostId: string) => void;
  onOpenWorkspaceFileTab?: (options: OpenWorkspaceFileTabOptions) => void;
  onOpenTmuxTerminal?: (
    launch: TmuxAttachLaunch,
    placement?: TmuxAttachPlacement,
  ) => void;
  onRemoteHostCreated?: () => void | Promise<void>;
  onConfirmedSettingsChange?: (
    settings: AppSettings,
  ) => Promise<AppSettings>;
  onSettingsChange?: (settings: AppSettings) => void;
  onSplitPane?: (direction: "horizontal" | "vertical") => void;
}

const SftpToolContent = lazy(async () => ({
  default: (await import("../sftp/tool/view/index")).SftpToolContent,
}));
const ServerInfoToolContent = lazy(async () => ({
  default: (await import("./ServerInfoToolContent")).ServerInfoToolContent,
}));
const PortForwardToolContent = lazy(async () => ({
  default: (await import("./PortForwardToolContent")).PortForwardToolContent,
}));
const SnippetToolContent = lazy(async () => ({
  default: (await import("../snippets/view/index")).SnippetToolContent,
}));
const LogToolContent = lazy(async () => ({
  default: (await import("../logs")).LogToolContent,
}));
const AgentLauncherToolContent = lazy(async () => ({
  default: (await import("./AgentLauncherToolContent"))
    .AgentLauncherToolContent,
}));
const TmuxToolContent = lazy(async () => ({
  default: (await import("./TmuxToolContent")).TmuxToolContent,
}));
const HostContainersToolContent = lazy(async () => ({
  default: (await import("../machine-sidebar")).HostContainersToolContent,
}));
const ContextInspectorToolContent = lazy(async () => ({
  default: (await import("./context-inspector")).ContextInspectorToolContent,
}));
const ContextInspectorTerminalArtifacts = lazy(async () => ({
  default: (await import("./context-inspector"))
    .ContextInspectorTerminalArtifacts,
}));

/**
 * 右栏内容宿主保持完整工具树，rail 的隐藏只改变入口展示，避免隐藏工具时卸载
 * Agent、SFTP 等带有长生命周期资源的内容组件。左侧与浮窗模式只把 rail 留在
 * Shell，同一个内容宿主不会因展示位置切换而重建。
 */
export function ToolPanel({
  activeTool,
  activeTools = activeTool ? [activeTool] : [],
  activeMachine,
  machineGroups,
  containerActions,
  containerRevealRequest,
  activeTab,
  focusedPane,
  selectedMachine,
  onClosePane,
  onActiveToolChange,
  onOpenTool,
  onArtifactActionRequest,
  onFocusTab,
  onOpenToolRailCustomization,
  onOpenWorkspaceFileTab,
  onOpenTmuxTerminal,
  onConfirmedSettingsChange,
  resolvedTheme = "dark",
  settings,
  showRail = true,
  visible = true,
  snippetConfigRevision,
  sftpRevealRequest,
  terminalPanes,
  terminalTabs,
  terminalAppearance,
  workspaceFileDirtyState,
  workspaceContext,
  tools,
}: ToolPanelProps) {
  const railTools = tools;
  const defaultContentToolId =
    tools.find((tool) => tool.id === "agentLauncher")?.id ??
    tools.find((tool) => tool.id !== "settings")?.id ??
    null;
  const contentTool =
    activeTool === null
      ? null
      : activeTool === "settings"
        ? defaultContentToolId
        : activeTool;
  const [mountedToolIds, setMountedToolIds] = useState<ToolId[]>(() =>
    contentTool ? [contentTool] : [],
  );
  const renderedToolIds = useMemo(() => {
    const availableToolIds = new Set(tools.map((tool) => tool.id));
    const nextToolIds = mountedToolIds.filter((toolId) =>
      availableToolIds.has(toolId),
    );
    if (contentTool && !nextToolIds.includes(contentTool)) {
      return [...nextToolIds, contentTool];
    }
    return nextToolIds;
  }, [contentTool, mountedToolIds, tools]);
  const renderedTools = useMemo(
    () =>
      renderedToolIds
        .map((toolId) => tools.find((tool) => tool.id === toolId))
        .filter((tool): tool is ToolSummary => Boolean(tool)),
    [renderedToolIds, tools],
  );
  const agentSendRequest = useAgentSendRequestSnapshot().request;
  const active = contentTool
    ? (tools.find((tool) => tool.id === contentTool) ?? railTools[0])
    : undefined;
  const drawerOpen = Boolean(contentTool && active);
  const interfaceDensity = settings?.interfaceDensity ?? "comfortable";
  const compactDensity = interfaceDensity === "compact";
  const spaciousDensity = interfaceDensity === "spacious";
  const drawerPaddingClass = compactDensity
    ? "scrollbar-none overflow-y-auto p-3"
    : spaciousDensity
      ? "scrollbar-none overflow-y-auto p-5"
      : "scrollbar-none overflow-y-auto p-4";
  useEffect(() => {
    if (!contentTool) {
      return;
    }
    setMountedToolIds((current) =>
      current.includes(contentTool) ? current : [...current, contentTool],
    );
  }, [contentTool]);

  useEffect(() => {
    if (
      !agentSendRequest ||
      !claimAgentSendRequestAutoOpen(agentSendRequest.id)
    ) {
      return;
    }
    if (
      activeTool !== "agentLauncher" &&
      tools.some((tool) => tool.id === "agentLauncher")
    ) {
      (onOpenTool ?? onActiveToolChange)("agentLauncher");
    }
  }, [activeTool, agentSendRequest, onActiveToolChange, onOpenTool, tools]);

  return (
    <aside
      aria-label="工具面板"
      aria-expanded={drawerOpen}
      className={cn(
        "kerminal-material-nav flex h-full w-full min-w-0",
        showRail && "border-l",
      )}
    >
      <div
        aria-hidden={!drawerOpen}
        className="relative min-w-0 flex-1 overflow-hidden"
        hidden={!drawerOpen}
      >
          {renderedTools.map((tool) => {
            const toolId = tool.id;
            const selected = toolId === contentTool;
            const binding = resolveToolPanelBinding(toolId, {
              machineGroups,
              activeMachine,
              activeTab,
              focusedPane,
              selectedMachine,
              workspaceRevision: workspaceContext?.revision,
            });
            const fullHeightTool =
              toolId === "agentLauncher" ||
              toolId === "sftp" ||
              toolId === "containers";
            const contentOwnsHeader =
              toolId === "system" || toolId === "ports" || toolId === "tmux";
            const containerReveal =
              containerRevealRequest &&
              containerRevealRequest.tabId === binding.activeTab?.id &&
              containerRevealRequest.hostId === binding.machine?.id
                ? containerRevealRequest
                : undefined;
            return (
              <div
                aria-hidden={!selected}
                className={cn(
                  "absolute inset-0 min-w-0",
                  fullHeightTool
                    ? "flex flex-col overflow-hidden"
                    : drawerPaddingClass,
                )}
                hidden={!selected}
                key={toolId}
              >
                {fullHeightTool || contentOwnsHeader ? null : (
                  <header className="mb-3">
                    <h2 className="min-w-0 text-lg font-semibold text-zinc-950 dark:text-zinc-50">
                      {tool.title}
                    </h2>
                  </header>
                )}

                <RenderErrorBoundary
                  fallback={() => (
                    <ToolContentCrashFallback title={tool.title} />
                  )}
                >
                  <Suspense
                    fallback={<ToolContentLoadingFallback title={tool.title} />}
                  >
                    {toolId === "containers" ? (
                      <HostContainersToolContent
                        {...containerActions}
                        active={selected && drawerOpen && visible}
                        initialContainerId={containerReveal?.containerId}
                        initialRuntime={containerReveal?.runtime ?? (
                          activeMachine?.kind === "dockerContainer"
                            ? activeMachine.runtime
                            : undefined
                        )}
                        initialRequestId={containerReveal?.requestId}
                        onOpenWorkspaceFileTab={onOpenWorkspaceFileTab}
                        selectedMachine={binding.machine}
                      />
                    ) : null}
                    {toolId === "agentLauncher" ? (
                      <AgentLauncherToolContent
                        activeTab={binding.activeTab}
                        desktopNotifications={settings?.desktopNotifications}
                        focusedPane={binding.focusedPane}
                        onConfirmedSettingsChange={onConfirmedSettingsChange}
                        resolvedTheme={resolvedTheme}
                        settings={settings}
                        terminalAppearance={
                          terminalAppearance ??
                          settings?.terminal ??
                          defaultTerminalAppearance
                        }
                        terminalPanes={terminalPanes}
                        terminalTabs={terminalTabs}
                      />
                    ) : null}
                    {toolId === "system" ? (
                      <ServerInfoToolContent
                        active={selected}
                        selectedMachine={binding.machine}
                      />
                    ) : null}
                    {toolId === "context" && workspaceContext ? (
                      <ContextInspectorToolContent
                        active={selected}
                        context={workspaceContext}
                        isNavigationAvailable={(navigationId) =>
                          Boolean(onFocusTab && navigationId.startsWith("tab:"))
                        }
                        onNavigate={(navigationId) => {
                          if (navigationId.startsWith("tab:")) {
                            onFocusTab?.(navigationId.slice(4));
                          }
                        }}
                      />
                    ) : null}
                    {toolId === "context" && workspaceContext?.focusedPaneId ? (
                      <ContextInspectorTerminalArtifacts
                        onActionRequest={onArtifactActionRequest}
                        paneId={workspaceContext.focusedPaneId}
                      />
                    ) : null}
                    {toolId === "sftp" ? (
                      <SftpToolContent
                        active={selected}
                        availableSessionScopeIds={terminalTabs?.map(
                          (tab) => tab.id,
                        )}
                        followedLocalPath={
                          binding.focusedPane?.mode === "local" &&
                          binding.focusedPane.machineId === binding.machine?.id
                            ? (binding.focusedPane.currentCwd ??
                              binding.focusedPane.cwd)
                            : undefined
                        }
                        followedRemotePath={
                          binding.focusedPane?.mode === "ssh" &&
                          binding.focusedPane.remoteHostId === binding.machine?.id
                            ? (binding.focusedPane.currentCwd ??
                              binding.focusedPane.cwd)
                            : binding.focusedPane?.mode === "container" &&
                                binding.focusedPane.machineId ===
                                  binding.machine?.id
                              ? binding.focusedPane.currentCwd
                              : undefined
                        }
                        interfaceDensity={interfaceDensity}
                        onOpenWorkspaceFileTab={onOpenWorkspaceFileTab}
                        selectedMachine={binding.machine}
                        sessionScopeId={binding.activeTab?.id}
                        sftpRevealRequest={sftpRevealRequest}
                        transferViewScope={sftpSidebarTransferViewScope({
                          hostId: binding.machine?.id,
                          tabId: binding.activeTab?.id,
                        })}
                        workspaceFileDirtyState={workspaceFileDirtyState}
                        workspaceFileTabs={terminalTabs?.filter(
                          isWorkspaceFileTab,
                        )}
                      />
                    ) : null}
                    {toolId === "ports" ? (
                      <PortForwardToolContent
                        active={selected}
                        focusedPane={binding.focusedPane}
                        selectedMachine={binding.machine}
                      />
                    ) : null}
                    {toolId === "tmux" ? (
                      <TmuxToolContent
                        active={selected}
                        activeMachine={
                          binding.source === "selectedMachine"
                            ? undefined
                            : binding.machine
                        }
                        activeTab={binding.activeTab}
                        focusedPane={binding.focusedPane}
                        onClosePane={onClosePane}
                        onFocusTab={onFocusTab}
                        onOpenTmuxTerminal={onOpenTmuxTerminal}
                        selectedMachine={
                          binding.source === "selectedMachine"
                            ? binding.machine
                            : undefined
                        }
                        terminalPanes={terminalPanes}
                        terminalTabs={terminalTabs}
                      />
                    ) : null}
                    {toolId === "snippets" ? (
                      <SnippetToolContent
                        activeTabId={binding.activeTab?.id}
                        configRevision={snippetConfigRevision}
                        focusedPane={binding.focusedPane}
                      />
                    ) : null}
                    {toolId === "logs" ? (
                      <LogToolContent
                        active={selected}
                        focusedPane={binding.focusedPane}
                      />
                    ) : null}
                  </Suspense>
                </RenderErrorBoundary>
              </div>
            );
          })}
      </div>
      {showRail ? (
        <ToolRail
          activeTool={activeTool}
          activeTools={activeTools}
          drawerOpen={drawerOpen}
          interfaceDensity={interfaceDensity}
          onActiveToolChange={onActiveToolChange}
          onOpenToolRailCustomization={onOpenToolRailCustomization}
          settings={settings?.toolRail}
          tools={railTools}
          variant="panel"
        />
      ) : null}
    </aside>
  );
}

function ToolContentLoadingFallback({ title }: { title?: string }) {
  return (
    <div
      aria-live="polite"
      className="kerminal-solid-surface rounded-lg border px-4 py-3 text-sm text-zinc-600 dark:text-zinc-300"
    >
      <div className="font-medium text-zinc-800 dark:text-zinc-100">
        正在加载{title ?? "工具"}
      </div>
      <div className="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
        请稍候...
      </div>
    </div>
  );
}

function ToolContentCrashFallback({ title }: { title?: string }) {
  return (
    <div className="rounded-lg border border-rose-300/25 bg-rose-500/10 px-4 py-3 text-sm text-rose-700 dark:text-rose-100">
      <div className="font-medium">{title ?? "工具"}加载失败</div>
      <div className="mt-1 text-xs opacity-80">
        收起右栏后重新打开可重试，详细信息已写入应用日志。
      </div>
    </div>
  );
}
