/**
 * Host-scoped container workbench content for an SSH target.
 *
 * @author kongweiguang
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  fetchDockerContainerStats,
  inspectDockerContainer,
  listDockerContainers,
  type DockerContainerInspectSummary,
  type DockerContainerLifecycleAction,
  type DockerContainerStatsResult,
  type DockerContainerSummary,
} from "../../lib/dockerApi";
import type { ContainerRuntime } from "../../lib/targetModel";
import {
  buildUserFacingError,
  type UserFacingMessage,
} from "../../lib/userFacingMessage";
import { buildComposeProjectViews } from "./host-containers/composeProjectModel";
import type { ComposeProjectInspectorTab } from "./host-containers/ComposeProjectInspector";
import {
  buildHostContainerDialogViewModel,
  canEnterHostContainer,
  canRunHostContainerLifecycleAction,
  hostContainerLifecycleDialogCopy,
  type HostContainerGroupMode,
  type HostContainerInspectorTab,
  type HostContainerMetadata,
  type HostContainerSelection,
} from "./host-containers/hostContainerDialogModel";
import {
  HostContainersStateMessage as StateMessage,
  composeYamlRootPath,
  containerLifecycleActionText as lifecycleActionText,
} from "./host-containers/hostContainersPresenter";
import {
  presentContainerSummary,
  runDefaultLifecycleAction,
  type HostContainersToolContentProps,
} from "./host-containers/hostContainersToolAdapter";
import { HostContainersToolView } from "./host-containers/HostContainersToolView";

export type { HostContainersToolContentProps } from "./host-containers/hostContainersToolAdapter";

const emptyContainerList: DockerContainerSummary[] = [];

/** Keeps reads and actions scoped to the active SSH host/runtime while retaining the list view between detail visits. */
export function HostContainersToolContent({
  active = true,
  initialContainerId,
  initialRequestId,
  initialRuntime,
  onEnterContainer,
  onFetchContainerStats = fetchDockerContainerStats,
  onInspectContainer = inspectDockerContainer,
  onLifecycleContainer = runDefaultLifecycleAction,
  onListDockerContainers = listDockerContainers,
  onOpenContainerLogs,
  onOpenWorkspaceFileTab,
  onPinContainer,
  refreshRequestId,
  selectedMachine,
}: HostContainersToolContentProps) {
  const host = selectedMachine?.kind === "ssh" ? selectedMachine : undefined;
  const hostId = host?.id;
  const [runtime, setRuntime] = useState<ContainerRuntime>(
    () => initialRuntime ?? "docker",
  );
  const [includeStopped, setIncludeStopped] = useState(true);
  const [groupMode, setGroupMode] = useState<HostContainerGroupMode>("compose");
  const [query, setQuery] = useState("");
  const [containers, setContainers] = useState<DockerContainerSummary[]>([]);
  const [resultTargetKey, setResultTargetKey] = useState<string | null>(null);
  const [selection, setSelection] = useState<HostContainerSelection>(
    initialContainerId
      ? { kind: "container", containerId: initialContainerId }
      : null,
  );
  const [detailView, setDetailView] = useState<"list" | "container" | "project">(
    () => (initialContainerId ? "container" : "list"),
  );
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<UserFacingMessage | null>(null);
  const [actionError, setActionError] =
    useState<UserFacingMessage | null>(null);
  const [missingPinnedContainerId, setMissingPinnedContainerId] =
    useState<string | null>(null);
  const [pinningContainerId, setPinningContainerId] = useState<string | null>(
    null,
  );
  const [activeLifecycleAction, setActiveLifecycleAction] = useState<{
    action: DockerContainerLifecycleAction;
    containerId: string;
    targetKey: string;
  } | null>(null);
  const [pendingLifecycleAction, setPendingLifecycleAction] = useState<{
    action: DockerContainerLifecycleAction;
    container: HostContainerMetadata;
    targetKey: string;
  } | null>(null);
  const [lifecycleConfirmValue, setLifecycleConfirmValue] = useState("");
  const [inspectorTab, setInspectorTab] =
    useState<HostContainerInspectorTab>("details");
  const [projectInspectorTab, setProjectInspectorTab] =
    useState<ComposeProjectInspectorTab>("overview");
  const [inspectorLoading, setInspectorLoading] = useState(false);
  const [inspectorError, setInspectorError] =
    useState<UserFacingMessage | null>(null);
  const [inspectSummary, setInspectSummary] =
    useState<DockerContainerInspectSummary | null>(null);
  const [statsResult, setStatsResult] =
    useState<DockerContainerStatsResult | null>(null);
  const requestSequenceRef = useRef(0);
  const inspectorSequenceRef = useRef(0);
  const lifecycleOperationsRef = useRef(new Map<string, string>());
  const pinOperationsRef = useRef(new Set<string>());
  const currentTargetKey = host ? `${host.id}:${runtime}` : null;
  const currentTargetKeyRef = useRef(currentTargetKey);
  const activeRef = useRef(active);
  const initialRequestKeyRef = useRef<string | null>(null);
  const pendingInitialRuntimeRef = useRef<{
    containerId: string;
    hostId: string;
    runtime: ContainerRuntime;
  } | null>(null);
  currentTargetKeyRef.current = currentTargetKey;
  activeRef.current = active;

  const resultMatchesTarget = resultTargetKey === currentTargetKey;
  const currentContainers = resultMatchesTarget ? containers : emptyContainerList;
  const currentLoadError = resultMatchesTarget ? loadError : null;
  const selectedContainerId =
    selection?.kind === "container" ? selection.containerId : undefined;
  const selectedProjectId =
    selection?.kind === "project" ? selection.projectId : undefined;
  const viewModel = useMemo(
    () =>
      buildHostContainerDialogViewModel(
        currentContainers,
        { groupMode, query },
        selectedContainerId,
      ),
    [currentContainers, groupMode, query, selectedContainerId],
  );
  const composeViewModel = useMemo(
    () => buildComposeProjectViews(currentContainers),
    [currentContainers],
  );
  const composeListViewModel = useMemo(
    () => buildComposeProjectViews(viewModel.containers),
    [viewModel.containers],
  );
  const selectedContainer =
    selection?.kind === "container"
      ? currentContainers.find(
          (container) => container.id === selection.containerId,
        )
      : undefined;
  const selectedContainerHostId = selectedContainer?.hostId;
  const selectedContainerRuntime = selectedContainer?.runtime;
  const selectedProject =
    groupMode === "compose" && selection?.kind === "project"
      ? composeViewModel.projects.find(
          (project) => project.id === selection.projectId,
        )
      : undefined;
  /** Inspector data is tied to a selected container so switching rows never flashes the previous row's details. */
  useEffect(() => {
    inspectorSequenceRef.current += 1;
    setInspectorLoading(false);
    setInspectorError(null);
    setInspectSummary(null);
    setStatsResult(null);
  }, [selectedContainerId]);

  /** A list response is accepted only while its request sequence and host/runtime target are still current. */
  const loadContainers = useCallback(async () => {
    if (
      !activeRef.current ||
      !host ||
      currentTargetKeyRef.current !== currentTargetKey
    ) {
      return;
    }

    const pendingInitialRuntime = pendingInitialRuntimeRef.current;
    if (
      pendingInitialRuntime?.hostId === host.id &&
      pendingInitialRuntime.runtime !== runtime
    ) {
      return;
    }
    pendingInitialRuntimeRef.current = null;

    const requestId = requestSequenceRef.current + 1;
    requestSequenceRef.current = requestId;
    const requestTargetKey = `${host.id}:${runtime}`;
    setLoading(true);
    setLoadError(null);
    setActionError(null);
    setMissingPinnedContainerId(null);

    try {
      const nextContainers = await onListDockerContainers({
        hostId: host.id,
        includeStopped,
        runtime,
      });
      if (
        requestSequenceRef.current !== requestId ||
        currentTargetKeyRef.current !== requestTargetKey
      ) {
        return;
      }
      setContainers(nextContainers);
      setResultTargetKey(requestTargetKey);
    } catch (loadError: unknown) {
      if (
        requestSequenceRef.current !== requestId ||
        currentTargetKeyRef.current !== requestTargetKey
      ) {
        return;
      }
      setContainers([]);
      setResultTargetKey(requestTargetKey);
      setSelection(null);
      setLoadError(
        buildUserFacingError(loadError, {
          detail: "Kerminal 暂时无法获取这个主机的容器列表。",
          recoveryAction: "请确认主机连接和容器运行时可用，然后重试。",
          title: "无法读取容器",
        }),
      );
    } finally {
      if (requestSequenceRef.current === requestId) {
        setLoading(false);
      }
    }
  }, [
    currentTargetKey,
    host,
    includeStopped,
    onListDockerContainers,
    runtime,
  ]);

  /** Invalidate old reads on target changes, preserving a one-shot pinned-container reveal that caused the runtime switch. */
  useEffect(() => {
    requestSequenceRef.current += 1;
    inspectorSequenceRef.current += 1;
    const pendingInitialRuntime = pendingInitialRuntimeRef.current;
    const initialContainerToPreserve =
      pendingInitialRuntime &&
      pendingInitialRuntime.hostId === hostId &&
      pendingInitialRuntime.runtime === runtime
        ? pendingInitialRuntime.containerId
        : undefined;
    pendingInitialRuntimeRef.current = null;
    setQuery((current) => (hostId ? current : ""));
    setContainers([]);
    setResultTargetKey(null);
    setLoading(false);
    setSelection(
      initialContainerToPreserve
        ? { kind: "container", containerId: initialContainerToPreserve }
        : null,
    );
    setDetailView(initialContainerToPreserve ? "container" : "list");
    setActionError(null);
    setMissingPinnedContainerId(null);
    setActiveLifecycleAction(null);
    setPendingLifecycleAction(null);
    setLifecycleConfirmValue("");
    setPinningContainerId(null);
    setInspectorTab("details");
    setProjectInspectorTab("overview");
    setInspectorLoading(false);
    setInspectorError(null);
    setInspectSummary(null);
    setStatsResult(null);
  }, [hostId, runtime]);

  /** Each explicit reveal has a unique request key so reopening the same pinned item reselects its detail view. */
  useEffect(() => {
    if (!initialContainerId || !hostId) {
      pendingInitialRuntimeRef.current = null;
      return;
    }
    const requestKey = `${hostId}:${initialRequestId ?? "legacy"}:${initialContainerId}:${initialRuntime ?? ""}`;
    if (initialRequestKeyRef.current === requestKey) {
      return;
    }
    initialRequestKeyRef.current = requestKey;
    setMissingPinnedContainerId(null);
    if (initialRuntime && initialRuntime !== runtime) {
      pendingInitialRuntimeRef.current = {
        containerId: initialContainerId,
        hostId,
        runtime: initialRuntime,
      };
      setRuntime(initialRuntime);
    }
    setSelection({ kind: "container", containerId: initialContainerId });
    setDetailView("container");
    setInspectorTab("details");
  }, [hostId, initialContainerId, initialRequestId, initialRuntime, runtime]);

  /** Hidden tools start no new reads; active reads still finish under the host/runtime sequence guard. */
  useEffect(() => {
    void loadContainers();
  }, [active, loadContainers, refreshRequestId]);

  /** Compose project selection stays valid only while its project remains in the active host's full result set. */
  useEffect(() => {
    if (selection?.kind !== "project") {
      return;
    }
    if (
      groupMode !== "compose" ||
      !composeViewModel.projects.some(
        (project) => project.id === selection.projectId,
      )
    ) {
      setSelection(null);
      setDetailView("list");
    }
  }, [
    composeViewModel.projects,
    groupMode,
    selection,
    viewModel.containers,
  ]);

  /** A target-matched refresh is the only point at which a missing selection closes its detail view. */
  useEffect(() => {
    if (!resultMatchesTarget) {
      return;
    }
    if (
      initialContainerId &&
      currentContainers.some((container) => container.id === initialContainerId)
    ) {
      setMissingPinnedContainerId(null);
    }
    if (
      selection?.kind === "container" &&
      !currentContainers.some((container) => container.id === selection.containerId)
    ) {
      if (selection.containerId === initialContainerId) {
        setMissingPinnedContainerId(initialContainerId);
      }
      setSelection(null);
      setDetailView("list");
    } else if (
      selection?.kind === "project" &&
      !composeViewModel.projects.some((project) => project.id === selection.projectId)
    ) {
      setSelection(null);
      setDetailView("list");
    }
  }, [
    composeViewModel.projects,
    currentContainers,
    resultMatchesTarget,
    selection,
    initialContainerId,
  ]);

  /** Detail selection stays in the shared list model so its scroll position survives the view switch. */
  const selectContainer = useCallback((containerId: string) => {
    setMissingPinnedContainerId(null);
    setSelection({ kind: "container", containerId });
    setDetailView("container");
  }, []);

  /** Compose selection opens its existing project inspector instead of a second detail implementation. */
  const selectProject = useCallback((projectId: string) => {
    setMissingPinnedContainerId(null);
    setSelection({ kind: "project", projectId });
    setDetailView("project");
    setProjectInspectorTab("overview");
  }, []);

  /** YAML inspection reuses the central SSH workspace tab and preserves its read-only contract. */
  const openProjectYaml = useCallback(
    (projectId: string) => {
      setSelection({ kind: "project", projectId });
      setDetailView("project");
      setProjectInspectorTab("yaml");

      const project = composeViewModel.projects.find(
        (candidate) => candidate.id === projectId,
      );
      const path = project?.configPaths[0];
      if (!host || !project || !path || !onOpenWorkspaceFileTab) {
        return;
      }

      onOpenWorkspaceFileTab({
        access: "readonly",
        path,
        rootPath: composeYamlRootPath(project.workingDir, path),
        source: "composeYaml",
        target: { hostId: host.id, kind: "ssh" },
      });
    },
    [composeViewModel.projects, host, onOpenWorkspaceFileTab],
  );

  /** Entry is allowed only for a running container that still belongs to the visible host/runtime. */
  const enterContainer = useCallback(
    (container: HostContainerMetadata | undefined = selectedContainer) => {
      if (
        !container ||
        !activeRef.current ||
        currentTargetKeyRef.current !== `${container.hostId}:${container.runtime}` ||
        !canEnterHostContainer(container)
      ) {
        return;
      }
      onEnterContainer?.(container);
    },
    [onEnterContainer, selectedContainer],
  );

  /** Deduplicate pin writes per host/container and keep stale failures out of the newly selected host. */
  const pinContainer = useCallback(
    async (container: HostContainerMetadata) => {
      const targetKey = `${container.hostId}:${container.runtime}`;
      const operationKey = `${targetKey}:${container.id}`;
      if (!activeRef.current || currentTargetKeyRef.current !== targetKey) {
        return;
      }
      if (pinOperationsRef.current.has(operationKey)) {
        return;
      }
      pinOperationsRef.current.add(operationKey);
      selectContainer(container.id);
      setPinningContainerId(container.id);
      setActionError(null);
      try {
        await onPinContainer?.(container);
      } catch (pinError: unknown) {
        if (currentTargetKeyRef.current !== targetKey) {
          return;
        }
        setActionError(
          buildUserFacingError(pinError, {
            detail: `${container.name} 仍可从当前列表打开。`,
            recoveryAction: "请稍后重试固定操作。",
            title: "无法固定容器",
          }),
        );
      } finally {
        pinOperationsRef.current.delete(operationKey);
        if (currentTargetKeyRef.current === targetKey) {
          setPinningContainerId((current) =>
            current === container.id ? null : current,
          );
        }
      }
    },
    [onPinContainer, selectContainer],
  );

  /** Serialize lifecycle writes per host/runtime and scope their completion feedback to the submitted target. */
  const runLifecycleAction = useCallback(
    async (
      action: DockerContainerLifecycleAction,
      container: HostContainerMetadata,
      options?: { force?: boolean },
    ) => {
      const targetKey = `${container.hostId}:${container.runtime}`;
      if (!activeRef.current || currentTargetKeyRef.current !== targetKey) {
        return;
      }
      if (lifecycleOperationsRef.current.has(targetKey)) {
        return;
      }
      lifecycleOperationsRef.current.set(targetKey, container.id);
      selectContainer(container.id);
      setActiveLifecycleAction({ action, containerId: container.id, targetKey });
      setActionError(null);
      try {
        await onLifecycleContainer(action, container, options);
        await loadContainers();
      } catch (lifecycleError: unknown) {
        if (currentTargetKeyRef.current !== targetKey) {
          return;
        }
        setActionError(
          buildUserFacingError(lifecycleError, {
            detail: `${container.name} 未完成${lifecycleActionText(action)}操作。`,
            recoveryAction: "请刷新状态，确认容器运行时可用后重试。",
            title: `${lifecycleActionText(action)}容器失败`,
          }),
        );
      } finally {
        if (lifecycleOperationsRef.current.get(targetKey) === container.id) {
          lifecycleOperationsRef.current.delete(targetKey);
        }
        if (currentTargetKeyRef.current === targetKey) {
          setActiveLifecycleAction(null);
        }
      }
    },
    [loadContainers, onLifecycleContainer, selectContainer],
  );

  /** Refuse another lifecycle submission for this target while its prior write is pending. */
  const requestLifecycleAction = useCallback(
    (action: DockerContainerLifecycleAction, container: HostContainerMetadata) => {
      const targetKey = `${container.hostId}:${container.runtime}`;
      if (currentTargetKeyRef.current !== targetKey) {
        return;
      }
      if (lifecycleOperationsRef.current.has(targetKey)) {
        return;
      }
      selectContainer(container.id);
      setActionError(null);
      if (!canRunHostContainerLifecycleAction(container, action)) {
        return;
      }
      if (action === "start") {
        void runLifecycleAction(action, container);
        return;
      }
      setLifecycleConfirmValue("");
      setPendingLifecycleAction({ action, container, targetKey });
    },
    [runLifecycleAction, selectContainer],
  );

  /** Inspector reads use a separate sequence because list reads and detail tabs resolve independently. */
  const loadInspector = useCallback(
    async (tab: HostContainerInspectorTab, container: Pick<HostContainerMetadata, "id" | "hostId" | "runtime">) => {
      const targetKey = `${container.hostId}:${container.runtime}`;
      if (
        !activeRef.current ||
        currentTargetKeyRef.current !== targetKey
      ) {
        return;
      }
      const requestId = inspectorSequenceRef.current + 1;
      inspectorSequenceRef.current = requestId;
      const request = { containerId: container.id, hostId: container.hostId, runtime: container.runtime };
      setInspectorLoading(true);
      setInspectorError(null);
      try {
        if (tab === "details") {
          const summary = await onInspectContainer(request);
          if (
            inspectorSequenceRef.current === requestId &&
            currentTargetKeyRef.current === targetKey
          ) {
            setInspectSummary(summary);
          }
          return;
        }
        const stats = await onFetchContainerStats(request);
        if (
          inspectorSequenceRef.current === requestId &&
          currentTargetKeyRef.current === targetKey
        ) {
          setStatsResult(stats);
        }
      } catch (inspectorError: unknown) {
        if (
          inspectorSequenceRef.current === requestId &&
          currentTargetKeyRef.current === targetKey
        ) {
          setInspectorError(
            buildUserFacingError(inspectorError, {
              detail: "当前详情或监控数据暂时不可用。",
              recoveryAction: "请确认容器仍存在，然后点击刷新重试。",
              title: "无法读取容器信息",
            }),
          );
        }
      } finally {
        if (inspectorSequenceRef.current === requestId) {
          setInspectorLoading(false);
        }
      }
    },
    [onFetchContainerStats, onInspectContainer],
  );

  /** Selection/tab changes load through the effect; repeating the same menu action refreshes the unchanged view directly. */
  const openInspector = useCallback(
    (tab: HostContainerInspectorTab, container: HostContainerMetadata) => {
      selectContainer(container.id);
      setInspectorTab(tab);
      setDetailView("container");
      if (selectedContainer?.id === container.id && inspectorTab === tab) {
        void loadInspector(tab, {
          id: container.id,
          hostId: container.hostId,
          runtime: container.runtime,
        });
      }
    },
    [inspectorTab, loadInspector, selectContainer, selectedContainer?.id],
  );

  /** Keep detail selection synchronized before handing off to the existing logs flow. */
  const openContainerLogs = useCallback(
    (container: HostContainerMetadata) => {
      selectContainer(container.id);
      onOpenContainerLogs?.(container);
    },
    [onOpenContainerLogs, selectContainer],
  );

  /** Confirmation is bound to the dialog's captured target and is discarded after a host/runtime switch. */
  const confirmLifecycleAction = useCallback(async () => {
    if (!pendingLifecycleAction) {
      return;
    }
    const { action, container, targetKey } = pendingLifecycleAction;
    setPendingLifecycleAction(null);
    setLifecycleConfirmValue("");
    if (currentTargetKeyRef.current !== targetKey) {
      return;
    }
    await runLifecycleAction(action, container, { force: false });
  }, [pendingLifecycleAction, runLifecycleAction]);

  useEffect(() => {
    if (!active || detailView !== "container") {
      return;
    }
    if (
      !selectedContainerId ||
      !selectedContainerHostId ||
      !selectedContainerRuntime
    ) {
      return;
    }
    setInspectorError(null);
    void loadInspector(inspectorTab, {
      id: selectedContainerId,
      hostId: selectedContainerHostId,
      runtime: selectedContainerRuntime,
    });
  }, [
    inspectorTab,
    loadInspector,
    active,
    detailView,
    selectedContainerHostId,
    selectedContainerId,
    selectedContainerRuntime,
  ]);

  const containerSummary = presentContainerSummary({
    composeErrors: composeViewModel.errorCount,
    composeProjects: composeViewModel.projects.length,
    loadError,
    loading,
    running: viewModel.runningCount,
    standalone: composeViewModel.standaloneContainers.length,
    total: viewModel.totalCount,
  });

  const lifecycleDialogCopy = pendingLifecycleAction
    ? hostContainerLifecycleDialogCopy(
        pendingLifecycleAction.action,
        pendingLifecycleAction.container,
      )
    : null;
  const selectedCanEnter = selectedContainer
    ? canEnterHostContainer(selectedContainer)
    : false;
  const selectedCanStart = selectedContainer
    ? canRunHostContainerLifecycleAction(selectedContainer, "start")
    : false;
  const selectedActionBusy =
    selectedContainer &&
    activeLifecycleAction?.containerId === selectedContainer.id &&
    activeLifecycleAction.targetKey === currentTargetKey;
  if (!host) {
    return (
      <div className="grid h-full min-h-0 content-start gap-3 p-3" data-testid="host-containers-tool-empty">
        <StateMessage>容器仅支持 SSH 主机上的 Docker 和 Podman。</StateMessage>
      </div>
    );
  }

  return (
    <HostContainersToolView
      actions={{
        onBackToList: () => setDetailView("list"),
        onCloseLifecycleDialog: () => {
          setPendingLifecycleAction(null);
          setLifecycleConfirmValue("");
        },
        onConfirmLifecycleAction: () => {
          void confirmLifecycleAction();
        },
        onEnterContainer: enterContainer,
        onGroupModeChange: setGroupMode,
        onIncludeStoppedChange: setIncludeStopped,
        onInspectAction: openInspector,
        onLifecycleAction: requestLifecycleAction,
        onLifecycleConfirmValueChange: setLifecycleConfirmValue,
        onInspectorTabChange: setInspectorTab,
        onOpenContainerLogs: openContainerLogs,
        onOpenProjectYaml: openProjectYaml,
        onOpenWorkspaceFileTab,
        onPinContainer: (container) => void pinContainer(container),
        onProjectInspectorTabChange: setProjectInspectorTab,
        onProjectSelectContainer: (container) => {
          selectContainer(container.id);
          setInspectorTab("details");
          setDetailView("container");
        },
        onQueryChange: setQuery,
        onRefreshContainers: () => void loadContainers(),
        onRefreshInspector: () => {
          if (selectedContainer) {
            void loadInspector(inspectorTab, selectedContainer);
          }
        },
        onRuntimeChange: setRuntime,
        onSelectContainer: selectContainer,
        onSelectProject: selectProject,
      }}
      state={{
        actionError,
        activeLifecycleAction,
        composeListViewModel,
        containerSummary,
        currentContainerCount: currentContainers.length,
        currentLoadError,
        detailView,
        groupMode,
        host,
        includeStopped,
        inspectorError,
        inspectorLoading,
        inspectorTab,
        inspectSummary,
        lifecycleConfirmValue,
        lifecycleDialogCopy,
        loading,
        missingPinnedContainer: Boolean(missingPinnedContainerId),
        pendingLifecycleAction,
        pinningContainerId,
        projectInspectorTab,
        query,
        resultMatchesTarget,
        runtime,
        selectedActionBusy: Boolean(selectedActionBusy),
        selectedCanEnter,
        selectedCanStart,
        selectedContainer,
        selectedContainerId,
        selectedProject,
        selectedProjectId,
        statsResult,
        viewModel,
      }}
    />
  );
}
