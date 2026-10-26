/**
 * Stateless host-container tool layout shared by the host-scoped controller.
 *
 * @author kongweiguang
 */

import { ArrowLeft, Play, Terminal } from "lucide-react";
import { Button } from "../../../components/ui/button";
import { PromptDialog } from "../../../components/ui/prompt-dialog";
import { UserFacingNotice } from "../../../components/ui/user-facing-notice";
import { cn } from "../../../lib/cn";
import type {
  DockerContainerInspectSummary,
  DockerContainerLifecycleAction,
  DockerContainerStatsResult,
} from "../../../lib/dockerApi";
import type { UserFacingMessage } from "../../../lib/userFacingMessage";
import type { Machine } from "../../workspace/contracts/index";
import type { OpenWorkspaceFileTabOptions } from "../../workspace/state/index";
import type { ContainerRuntime } from "../../../lib/targetModel";
import {
  ComposeProjectInspector,
  type ComposeProjectInspectorTab,
} from "./ComposeProjectInspector";
import { HostContainerActionsMenu } from "./HostContainerActionsMenu";
import { HostContainerInspector } from "./HostContainerInspector";
import { HostContainerList } from "./HostContainerList";
import type {
  ComposeProjectView,
  ComposeProjectViews,
} from "./composeProjectModel";
import type {
  HostContainerDialogViewModel,
  HostContainerGroupMode,
  HostContainerInspectorTab,
  HostContainerLifecycleDialogCopy,
  HostContainerMetadata,
} from "./hostContainerDialogModel";
import { hostContainerStatusLabel } from "./hostContainerDialogModel";
import {
  HostContainersStateMessage as StateMessage,
  HostContainersToolbar,
} from "./hostContainersPresenter";

export interface HostContainersToolViewState {
  actionError: UserFacingMessage | null;
  activeLifecycleAction: {
    action: DockerContainerLifecycleAction;
    containerId: string;
    targetKey: string;
  } | null;
  composeListViewModel: ComposeProjectViews;
  containerSummary: string;
  currentContainerCount: number;
  currentLoadError: UserFacingMessage | null;
  detailView: "list" | "container" | "project";
  groupMode: HostContainerGroupMode;
  host: Machine;
  includeStopped: boolean;
  inspectorError: UserFacingMessage | null;
  inspectorLoading: boolean;
  inspectorTab: HostContainerInspectorTab;
  inspectSummary: DockerContainerInspectSummary | null;
  lifecycleDialogCopy: HostContainerLifecycleDialogCopy | null;
  lifecycleConfirmValue: string;
  loading: boolean;
  missingPinnedContainer: boolean;
  pendingLifecycleAction: {
    action: DockerContainerLifecycleAction;
    container: HostContainerMetadata;
    targetKey: string;
  } | null;
  pinningContainerId: string | null;
  projectInspectorTab: ComposeProjectInspectorTab;
  query: string;
  resultMatchesTarget: boolean;
  runtime: ContainerRuntime;
  selectedActionBusy: boolean;
  selectedCanEnter: boolean;
  selectedCanStart: boolean;
  selectedContainer?: HostContainerMetadata;
  selectedContainerId?: string;
  selectedProject?: ComposeProjectView;
  selectedProjectId?: string;
  statsResult: DockerContainerStatsResult | null;
  viewModel: HostContainerDialogViewModel;
}

export interface HostContainersToolViewActions {
  onBackToList: () => void;
  onCloseLifecycleDialog: () => void;
  onConfirmLifecycleAction: () => void;
  onEnterContainer: (container?: HostContainerMetadata) => void;
  onGroupModeChange: (mode: HostContainerGroupMode) => void;
  onIncludeStoppedChange: (include: boolean) => void;
  onInspectAction: (
    tab: HostContainerInspectorTab,
    container: HostContainerMetadata,
  ) => void;
  onLifecycleAction: (
    action: DockerContainerLifecycleAction,
    container: HostContainerMetadata,
  ) => void;
  onLifecycleConfirmValueChange: (value: string) => void;
  onInspectorTabChange: (tab: HostContainerInspectorTab) => void;
  onOpenContainerLogs: (container: HostContainerMetadata) => void;
  onOpenProjectYaml: (projectId: string) => void;
  onOpenWorkspaceFileTab?: (options: OpenWorkspaceFileTabOptions) => void;
  onPinContainer: (container: HostContainerMetadata) => void;
  onProjectInspectorTabChange: (tab: ComposeProjectInspectorTab) => void;
  onProjectSelectContainer: (container: HostContainerMetadata) => void;
  onQueryChange: (query: string) => void;
  onRefreshContainers: () => void;
  onRefreshInspector: () => void;
  onRuntimeChange: (runtime: "docker" | "podman") => void;
  onSelectContainer: (containerId: string) => void;
  onSelectProject: (projectId: string) => void;
};

/** Renders current container state without owning host selection, requests, or navigation state. */
export function HostContainersToolView({
  actions,
  state,
}: {
  actions: HostContainersToolViewActions;
  state: HostContainersToolViewState;
}) {
  const { host } = state;
  return (
    <>
      <div
        className="grid h-full min-h-0 min-w-0 grid-rows-[auto_minmax(0,1fr)] gap-3 overflow-hidden p-3"
        data-testid="host-containers-tool-content"
      >
        <div className="grid min-w-0 gap-3">
          <HostContainersToolbar
            groupMode={state.groupMode}
            host={host}
            includeStopped={state.includeStopped}
            loading={state.loading}
            onGroupModeChange={actions.onGroupModeChange}
            onIncludeStoppedChange={actions.onIncludeStoppedChange}
            onQueryChange={actions.onQueryChange}
            onRefresh={actions.onRefreshContainers}
            onRuntimeChange={actions.onRuntimeChange}
            query={state.query}
            runtime={state.runtime}
            sidebar={false}
            summary={state.containerSummary}
          />
          {state.actionError ? (
            <UserFacingNotice compact message={state.actionError} />
          ) : null}
          {state.missingPinnedContainer ? (
            <UserFacingNotice
              compact
              message={missingPinnedContainerMessage}
            />
          ) : null}
        </div>

        <HostContainersToolListView actions={actions} state={state} />
        <HostContainersToolDetailView actions={actions} state={state} />
      </div>

      <HostContainersLifecycleConfirmation actions={actions} state={state} />
    </>
  );
}

/** Keeps list loading, filtering, and empty states in the same mounted tree so scroll position survives detail visits. */
function HostContainersToolListView({
  actions,
  state,
}: {
  actions: HostContainersToolViewActions;
  state: HostContainersToolViewState;
}) {
  return (
    <div
      aria-hidden={state.detailView !== "list"}
      className={cn(
        "min-h-0 min-w-0",
        state.detailView === "list" ? "flex flex-col" : "hidden",
      )}
      data-testid="host-container-list-view"
    >
      {state.currentLoadError && state.detailView === "list" ? (
        <div className="flex min-h-32 items-center">
          <UserFacingNotice
            className="w-full"
            compact
            message={state.currentLoadError}
          />
        </div>
      ) : state.loading && state.currentContainerCount === 0 ? (
        <StateMessage>正在读取容器...</StateMessage>
      ) : state.viewModel.emptySearch ? (
        <StateMessage>没有匹配的容器。</StateMessage>
      ) : state.viewModel.totalCount === 0 ? (
        <StateMessage>当前主机没有可显示的容器。</StateMessage>
      ) : (
        <HostContainerList
          groupMode={state.groupMode}
          groups={state.viewModel.groups}
          onEnterContainer={actions.onEnterContainer}
          onInspectAction={actions.onInspectAction}
          onLifecycleAction={actions.onLifecycleAction}
          onOpenLogs={actions.onOpenContainerLogs}
          onOpenProjectYaml={actions.onOpenProjectYaml}
          onPinContainer={actions.onPinContainer}
          onSelectContainer={actions.onSelectContainer}
          onSelectProject={actions.onSelectProject}
          pinningContainerId={state.pinningContainerId}
          presentation="sidebar"
          projects={state.composeListViewModel.projects}
          selectedContainerId={state.selectedContainerId}
          selectedProjectId={state.selectedProjectId}
          standaloneContainers={state.composeListViewModel.standaloneContainers}
        />
      )}
    </div>
  );
}

/** Renders either the Compose inspector or container inspector without owning their selection state. */
function HostContainersToolDetailView({
  actions,
  state,
}: {
  actions: HostContainersToolViewActions;
  state: HostContainersToolViewState;
}) {
  const { host } = state;
  const selectedContainer = state.selectedContainer;
  return (
    <div
      aria-hidden={state.detailView === "list"}
      className={cn(
        "min-h-0 min-w-0 flex-col overflow-hidden",
        state.detailView === "list" ? "hidden" : "flex",
      )}
      data-testid="host-container-detail-view"
    >
      <header className="mb-3 flex min-w-0 items-center gap-2 border-b border-[var(--border-subtle)] pb-2">
        <Button
          aria-label="返回容器列表"
          className="h-8 w-8 shrink-0 rounded-lg"
          onClick={actions.onBackToList}
          size="icon"
          title="返回容器列表"
          type="button"
          variant="ghost"
        >
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div className="min-w-0 flex-1">
          <h3 className="truncate text-sm font-semibold text-[var(--text-primary)]">
            {state.selectedProject?.project ?? selectedContainer?.name ?? "容器详情"}
          </h3>
          {selectedContainer ? (
            <p className="truncate text-xs text-[var(--text-muted)]">
              {hostContainerStatusLabel(selectedContainer.status)}
              {selectedContainer.ports.length > 0
                ? ` · ${selectedContainer.ports.join(", ")}`
                : ""}
            </p>
          ) : null}
        </div>
        {selectedContainer ? (
          <>
            <Button
              aria-label={state.selectedCanEnter
              ? `进入容器 ${selectedContainer.name}`
                : `启动容器 ${selectedContainer.name}`}
              className="h-8 w-8 shrink-0 rounded-lg"
              disabled={
                state.selectedCanEnter
                  ? false
                  : !state.selectedCanStart || state.selectedActionBusy
              }
              onClick={() => {
                if (state.selectedCanEnter) {
                  actions.onEnterContainer(selectedContainer);
                } else {
                  actions.onLifecycleAction("start", selectedContainer);
                }
              }}
              size="icon"
              title={state.selectedCanEnter ? "进入容器终端" : "启动容器"}
              type="button"
              variant="secondary"
            >
              {state.selectedCanEnter ? (
                <Terminal className="h-4 w-4" />
              ) : (
                <Play className="h-4 w-4" />
              )}
            </Button>
            <HostContainerActionsMenu
              compact
              container={selectedContainer}
              onAction={actions.onLifecycleAction}
              onInspectAction={actions.onInspectAction}
              onOpenLogs={actions.onOpenContainerLogs}
              onPinContainer={actions.onPinContainer}
              onSelectContainer={actions.onSelectContainer}
              pinning={state.pinningContainerId === selectedContainer.id}
              showInspectorItems={false}
            />
          </>
        ) : null}
      </header>
      <div className="min-h-0 min-w-0 flex-1 overflow-y-auto">
        {state.currentLoadError && state.detailView !== "list" ? (
          <UserFacingNotice compact message={state.currentLoadError} />
        ) : state.loading && state.currentContainerCount === 0 ? (
          <StateMessage>正在读取容器...</StateMessage>
        ) : state.selectedProject ? (
          <ComposeProjectInspector
            hostId={host.id}
            onEnterContainer={actions.onEnterContainer}
            onOpenContainerLogs={actions.onOpenContainerLogs}
            onOpenWorkspaceFileTab={actions.onOpenWorkspaceFileTab}
            onRefresh={actions.onRefreshContainers}
            onSelectContainer={actions.onProjectSelectContainer}
            onTabChange={actions.onProjectInspectorTabChange}
            project={state.selectedProject}
            tab={state.projectInspectorTab}
          />
        ) : selectedContainer ? (
          <>
            {state.inspectorError ? (
              <UserFacingNotice
                className="mb-3"
                compact
                message={state.inspectorError}
              />
            ) : null}
            <HostContainerInspector
              container={selectedContainer}
              embedded
              error={null}
              inspectSummary={state.inspectSummary}
              loading={state.inspectorLoading}
              onRefresh={actions.onRefreshInspector}
              onTabChange={actions.onInspectorTabChange}
              statsResult={state.statsResult}
              tab={state.inspectorTab}
            />
          </>
        ) : state.resultMatchesTarget ? (
          <StateMessage>所选容器已不存在，请返回列表刷新状态。</StateMessage>
        ) : (
          <StateMessage>正在读取容器...</StateMessage>
        )}
      </div>
    </div>
  );
}

/** Keeps the confirmation attached to the target snapshot supplied by the host-scoped controller. */
function HostContainersLifecycleConfirmation({
  actions,
  state,
}: {
  actions: HostContainersToolViewActions;
  state: HostContainersToolViewState;
}) {
  const copy = state.lifecycleDialogCopy;
  const pending = state.pendingLifecycleAction;
  if (!pending || !copy) {
    return null;
  }
  return (
    <PromptDialog
      busy={state.activeLifecycleAction?.targetKey === pending.targetKey}
      confirmLabel={copy.confirmLabel}
      confirmVariant={copy.variant}
      description={copy.description}
      helperText={copy.helperText}
      inputLabel={copy.inputLabel}
      onClose={actions.onCloseLifecycleDialog}
      onConfirm={actions.onConfirmLifecycleAction}
      onValueChange={actions.onLifecycleConfirmValueChange}
      open
      placeholder={copy.placeholder}
      title={copy.title}
      validate={
        pending.action === "remove"
          ? (value) =>
              value === pending.container.name
                ? null
                : "请输入完整容器名确认删除"
          : undefined
      }
      value={state.lifecycleConfirmValue}
    />
  );
}

const missingPinnedContainerMessage: UserFacingMessage = {
  detail: "该容器可能已删除，也可能被当前运行时或停止容器筛选隐藏。",
  recoveryAction: "检查 Docker/Podman 与停止容器筛选；确认不存在后，请从左栏移除此快捷项。",
  severity: "warning",
  title: "固定容器未在当前结果中找到",
};

