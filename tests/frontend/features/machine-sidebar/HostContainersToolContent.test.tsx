import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  dockerContainerTarget,
  dockerContainerTargetCapabilities,
} from "../../../../src/lib/targetModel";
import type {
  DockerContainerListRequest,
  DockerContainerSummary,
} from "../../../../src/lib/dockerApi";
import type { Machine } from "../../../../src/features/workspace/types";
import { HostContainersToolContent } from "../../../../src/features/machine-sidebar/HostContainersToolContent";

vi.mock("../../../../src/features/sftp/MonacoTextEditor", () => ({
  MonacoTextEditor: () => <textarea aria-label="Compose YAML Monaco editor" />,
}));

const host: Machine = {
  description: "root@10.0.0.12:22",
  host: "10.0.0.12",
  id: "ubuntu-dev",
  kind: "ssh",
  name: "ubuntu-dev",
  port: 22,
  remoteGroupId: "group-dev",
  status: "offline",
  tags: ["ssh", "dev"],
  username: "root",
};

const localMachine: Machine = {
  description: "默认本地配置",
  id: "local-powershell",
  kind: "local",
  name: "PowerShell",
  status: "online",
  tags: ["local"],
};

const secondHost: Machine = {
  ...host,
  id: "ubuntu-prod",
  name: "ubuntu-prod",
};

function container(
  input: Partial<DockerContainerSummary> &
    Pick<DockerContainerSummary, "id" | "name">,
): DockerContainerSummary {
  const { id, name, ...overrides } = input;
  const hostId = input.hostId ?? host.id;
  const runtime = input.runtime ?? "docker";
  const target = dockerContainerTarget({
    containerId: id,
    containerName: name,
    hostId,
    runtime,
  });

  return {
    ...overrides,
    capabilities: dockerContainerTargetCapabilities,
    hostId,
    id,
    image: input.image ?? "kerminal/api:latest",
    name,
    ports: input.ports ?? [],
    runtime,
    shortId: input.shortId ?? id.slice(0, 12),
    state: input.state ?? input.status ?? "running",
    status: input.status ?? "running",
    statusText: input.statusText ?? "Up 12 minutes",
    target,
  };
}

function inspectorProps() {
  return {
    onFetchContainerStats: vi.fn().mockResolvedValue({
      blockIo: "0B / 0B",
      containerId: "c0ffee1234567890",
      cpuPercent: "0.42%",
      hostId: host.id,
      memoryPercent: "4.1%",
      memoryUsage: "42MiB / 1GiB",
      networkIo: "1kB / 2kB",
      pids: "7",
      raw: "{\"CPUPerc\":\"0.42%\"}",
      runtime: "docker",
    }),
    onInspectContainer: vi.fn().mockResolvedValue({
      command: ["serve"],
      containerId: "c0ffee1234567890",
      entrypoint: ["/entrypoint.sh"],
      hostId: host.id,
      id: "c0ffee1234567890",
      image: "kerminal/api:latest",
      labels: {},
      name: "api",
      networks: ["bridge"],
      ports: ["0.0.0.0:8080->80/tcp"],
      rawJson: "{}",
      running: true,
      runtime: "docker",
      status: "running",
    }),
    onOpenContainerLogs: vi.fn(),
  };
}

describe("HostContainersToolContent", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("shows a quiet empty state until an SSH host is selected", () => {
    const onListDockerContainers = vi.fn();

    render(
      <HostContainersToolContent
        onListDockerContainers={onListDockerContainers}
        selectedMachine={localMachine}
      />,
    );

    expect(screen.getByTestId("host-containers-tool-empty")).toHaveTextContent(
      "容器仅支持 SSH 主机上的 Docker 和 Podman。",
    );
    expect(onListDockerContainers).not.toHaveBeenCalled();
  });

  it("loads host containers and keeps terminal, logs, and pin actions in the right panel", async () => {
    const user = userEvent.setup();
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const props = inspectorProps();
    const onEnterContainer = vi.fn();
    const onListDockerContainers = vi.fn().mockResolvedValue([api]);
    const onPinContainer = vi.fn();

    render(
      <HostContainersToolContent
        {...props}
        onEnterContainer={onEnterContainer}
        onListDockerContainers={onListDockerContainers}
        onPinContainer={onPinContainer}
        selectedMachine={host}
      />,
    );

    expect(await screen.findByTestId("host-containers-tool-content")).toHaveTextContent(
      "ubuntu-dev",
    );
    expect(onListDockerContainers).toHaveBeenCalledWith({
      hostId: "ubuntu-dev",
      includeStopped: true,
      runtime: "docker",
    });

    await user.click(screen.getByRole("button", { name: "进入容器 api" }));
    expect(onEnterContainer).toHaveBeenCalledWith(api);

    await user.click(screen.getByRole("button", { name: "更多容器操作 api" }));
    await user.click(screen.getByRole("menuitem", { name: "日志" }));
    expect(props.onOpenContainerLogs).toHaveBeenCalledWith(api);

    await user.click(screen.getByRole("button", { name: "更多容器操作 api" }));
    await user.click(screen.getByRole("menuitem", { name: "固定" }));
    expect(onPinContainer).toHaveBeenCalledWith(api);
  });

  it("selects the initial container when opened from a pinned sidebar item", async () => {
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const worker = container({
      id: "f00d123456789abc",
      image: "kerminal/worker:latest",
      name: "worker",
      shortId: "f00d12345678",
    });
    const props = inspectorProps();

    render(
      <HostContainersToolContent
        {...props}
        initialContainerId={worker.id}
        onListDockerContainers={vi.fn().mockResolvedValue([api, worker])}
        selectedMachine={host}
      />,
    );

    expect(
      await screen.findByRole("button", { name: "进入容器 worker" }),
    ).toBeInTheDocument();
    expect(screen.getByText("worker", { selector: "h3" })).toBeInTheDocument();
    await waitFor(() => {
      expect(props.onInspectContainer).toHaveBeenCalledWith({
        containerId: worker.id,
        hostId: host.id,
        runtime: "docker",
      });
    });
  });

  it("preserves a pinned Podman reveal while switching the runtime", async () => {
    const dockerItem = container({ id: "docker-id", name: "docker-api" });
    const podmanItem = container({
      id: "podman-id",
      name: "podman-api",
      runtime: "podman",
    });
    const props = inspectorProps();
    const onListDockerContainers = vi.fn((request: DockerContainerListRequest) =>
      Promise.resolve(request.runtime === "podman" ? [podmanItem] : [dockerItem]),
    );
    const { rerender } = render(
      <HostContainersToolContent
        {...props}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    await waitFor(() => {
      expect(onListDockerContainers).toHaveBeenCalledWith({
        hostId: host.id,
        includeStopped: true,
        runtime: "docker",
      });
    });
    rerender(
      <HostContainersToolContent
        {...props}
        initialContainerId={podmanItem.id}
        initialRequestId={1}
        initialRuntime="podman"
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    expect(
      await screen.findByRole("button", { name: "进入容器 podman-api" }),
    ).toBeInTheDocument();
    await waitFor(() => {
      expect(props.onInspectContainer).toHaveBeenCalledWith({
        containerId: podmanItem.id,
        hostId: host.id,
        runtime: "podman",
      });
    });
    expect(onListDockerContainers).toHaveBeenLastCalledWith({
      hostId: host.id,
      includeStopped: true,
      runtime: "podman",
    });
  });

  it("does not replay a consumed pinned reveal after switching away and back", async () => {
    const user = userEvent.setup();
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const list = vi.fn().mockResolvedValue([api]);
    const { rerender } = render(
      <HostContainersToolContent
        initialContainerId={api.id}
        initialRequestId={7}
        onListDockerContainers={list}
        selectedMachine={host}
      />,
    );

    expect(await screen.findByText("api", { selector: "h3" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "返回容器列表" }));
    rerender(
      <HostContainersToolContent
        onListDockerContainers={list}
        selectedMachine={localMachine}
      />,
    );
    rerender(
      <HostContainersToolContent
        initialContainerId={api.id}
        initialRequestId={7}
        onListDockerContainers={list}
        selectedMachine={host}
      />,
    );

    expect(
      await screen.findByRole("button", { name: "进入容器 api" }),
    ).toBeInTheDocument();
    expect(screen.getByTestId("host-container-list-view")).not.toHaveAttribute(
      "aria-hidden",
      "true",
    );
    expect(screen.queryByRole("button", { name: "返回容器列表" })).not.toBeInTheDocument();
  });

  it("does not read while hidden and refreshes when activated", async () => {
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const onListDockerContainers = vi.fn().mockResolvedValue([api]);
    const { rerender } = render(
      <HostContainersToolContent
        active={false}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    expect(onListDockerContainers).not.toHaveBeenCalled();
    rerender(
      <HostContainersToolContent
        active
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    expect(
      await screen.findByRole("button", { name: "进入容器 api" }),
    ).toBeInTheDocument();
    expect(onListDockerContainers).toHaveBeenCalledTimes(1);
  });

  it("ignores a late container list from the previous host", async () => {
    let resolveOldList!: (items: DockerContainerSummary[]) => void;
    const oldList = new Promise<DockerContainerSummary[]>((resolve) => {
      resolveOldList = resolve;
    });
    const oldItem = container({ id: "old-id", name: "old-host-item" });
    const newItem = container({
      hostId: secondHost.id,
      id: "new-id",
      name: "new-host-item",
    });
    const onListDockerContainers = vi.fn((request: DockerContainerListRequest) =>
      request.hostId === host.id ? oldList : Promise.resolve([newItem]),
    );
    const { rerender } = render(
      <HostContainersToolContent
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    rerender(
      <HostContainersToolContent
        onListDockerContainers={onListDockerContainers}
        selectedMachine={secondHost}
      />,
    );
    expect(
      await screen.findByRole("button", { name: "进入容器 new-host-item" }),
    ).toBeInTheDocument();
    resolveOldList([oldItem]);

    expect(screen.queryByText("old-host-item")).not.toBeInTheDocument();
    expect(screen.getByText("ubuntu-prod", { selector: "div" })).toBeInTheDocument();
  });

  it("drops a lifecycle confirmation when its host changes", async () => {
    const user = userEvent.setup();
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const productionApi = container({
      hostId: secondHost.id,
      id: "prod-api-id",
      name: "production-api",
    });
    const onLifecycleContainer = vi.fn();
    const onListDockerContainers = vi.fn((request: DockerContainerListRequest) =>
      Promise.resolve(request.hostId === host.id ? [api] : [productionApi]),
    );
    const { rerender } = render(
      <HostContainersToolContent
        onLifecycleContainer={onLifecycleContainer}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    await user.click(
      await screen.findByRole("button", { name: "更多容器操作 api" }),
    );
    await user.click(screen.getByRole("menuitem", { name: "停止" }));
    expect(screen.getByRole("heading", { name: "停止容器" })).toBeInTheDocument();
    rerender(
      <HostContainersToolContent
        onLifecycleContainer={onLifecycleContainer}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={secondHost}
      />,
    );

    await waitFor(() => {
      expect(screen.queryByRole("heading", { name: "停止容器" })).not.toBeInTheDocument();
    });
    expect(onLifecycleContainer).not.toHaveBeenCalled();
  });

  it("blocks a duplicate lifecycle submission for the same target", async () => {
    const user = userEvent.setup();
    const stoppedApi = container({
      id: "c0ffee1234567890",
      name: "api",
      state: "exited",
      status: "exited",
    });
    let finishLifecycle!: () => void;
    const onListDockerContainers = vi.fn().mockResolvedValue([stoppedApi]);
    const onLifecycleContainer = vi.fn(
      () => new Promise<void>((resolve) => (finishLifecycle = resolve)),
    );

    render(
      <HostContainersToolContent
        onLifecycleContainer={onLifecycleContainer}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );
    await user.click(
      await screen.findByRole("button", { name: "启动容器 api" }),
    );
    await user.click(screen.getByRole("button", { name: "更多容器操作 api" }));
    await user.click(screen.getByRole("menuitem", { name: "启动" }));

    expect(onLifecycleContainer).toHaveBeenCalledTimes(1);
    finishLifecycle();
    await waitFor(() => expect(onListDockerContainers).toHaveBeenCalledTimes(2));
  });

  // The list stays mounted for scroll preservation, so wait for its visibility state before checking its contents.
  it("returns to the list when a selected container disappears after refresh", async () => {
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const onListDockerContainers = vi
      .fn()
      .mockResolvedValueOnce([api])
      .mockResolvedValueOnce([]);
    const { rerender } = render(
      <HostContainersToolContent
        initialContainerId={api.id}
        initialRequestId={1}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    expect(await screen.findByText("api", { selector: "h3" })).toBeInTheDocument();
    rerender(
      <HostContainersToolContent
        initialContainerId={api.id}
        initialRequestId={1}
        onListDockerContainers={onListDockerContainers}
        refreshRequestId={1}
        selectedMachine={host}
      />,
    );

    const listView = screen.getByTestId("host-container-list-view");
    await waitFor(() => expect(listView).toHaveAttribute("aria-hidden", "false"));
    expect(
      within(listView).getByText("当前主机没有可显示的容器。"),
    ).toBeInTheDocument();
  });

  it("prompts removal when a pinned container is absent without selecting a substitute", async () => {
    const replacement = container({ id: "replacement-id", name: "replacement" });
    const props = inspectorProps();
    render(
      <HostContainersToolContent
        {...props}
        initialContainerId="missing-pinned-id"
        initialRequestId={1}
        onListDockerContainers={vi.fn().mockResolvedValue([replacement])}
        selectedMachine={host}
      />,
    );

    expect(
      await screen.findByText("固定容器未在当前结果中找到"),
    ).toBeInTheDocument();
    expect(screen.getByTestId("host-container-list-view")).not.toHaveAttribute(
      "aria-hidden",
      "true",
    );
    expect(screen.queryByText("replacement", { selector: "h3" })).not.toBeInTheDocument();
    expect(props.onInspectContainer).not.toHaveBeenCalled();
  });

  it("clears container A inspector data before loading container B", async () => {
    const user = userEvent.setup();
    const api = container({ id: "api-id", name: "api" });
    const worker = container({ id: "worker-id", name: "worker" });
    const props = inspectorProps();
    let resolveWorkerInspect!: (summary: Awaited<ReturnType<typeof props.onInspectContainer>>) => void;
    const workerInspect = new Promise<Awaited<ReturnType<typeof props.onInspectContainer>>>(
      (resolve) => {
        resolveWorkerInspect = resolve;
      },
    );
    props.onInspectContainer.mockImplementation((request) =>
      request.containerId === api.id
        ? Promise.resolve({
            command: ["api-only"],
            containerId: api.id,
            entrypoint: [],
            hostId: host.id,
            id: api.id,
            image: "api-only-image",
            labels: {},
            name: api.name,
            networks: [],
            ports: [],
            rawJson: "{}",
            running: true,
            runtime: "docker",
            status: "running",
          })
        : workerInspect,
    );
    render(
      <HostContainersToolContent
        {...props}
        initialContainerId={api.id}
        initialRequestId={1}
        onListDockerContainers={vi.fn().mockResolvedValue([api, worker])}
        selectedMachine={host}
      />,
    );

    expect(await screen.findByText("api-only-image")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "返回容器列表" }));
    await user.click(screen.getByRole("option", { name: "容器 worker" }));

    expect(await screen.findByText("正在读取容器信息...")).toBeInTheDocument();
    expect(screen.queryByText("api-only-image")).not.toBeInTheDocument();
    resolveWorkerInspect({
      command: ["worker-only"],
      containerId: worker.id,
      entrypoint: [],
      hostId: host.id,
      id: worker.id,
      image: "worker-only-image",
      labels: {},
      name: worker.name,
      networks: [],
      ports: [],
      rawJson: "{}",
      running: true,
      runtime: "docker",
      status: "running",
    });
    expect(await screen.findByText("worker-only-image")).toBeInTheDocument();
  });

  it("retains search filters when returning from container details", async () => {
    const user = userEvent.setup();
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const worker = container({ id: "f00d123456789abc", name: "worker" });
    render(
      <HostContainersToolContent
        onListDockerContainers={vi.fn().mockResolvedValue([api, worker])}
        selectedMachine={host}
      />,
    );
    const search = screen.getByRole("textbox", { name: "搜索容器" });
    await user.type(search, "worker");
    await user.click(await screen.findByRole("option", { name: "容器 worker" }));
    expect(screen.getByText("worker", { selector: "h3" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "返回容器列表" }));

    expect(search).toHaveValue("worker");
    expect(screen.getByRole("option", { name: "容器 worker" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "容器 api" })).not.toBeInTheDocument();
  });

  it("opens Compose YAML from the container list in the central workspace tab", async () => {
    const user = userEvent.setup();
    const api = container({
      compose: {
        configFiles: ["compose.yaml"],
        project: "kerminal",
        service: "api",
        workingDir: "/srv/kerminal",
      },
      id: "c0ffee1234567890",
      name: "api",
    });
    const onOpenWorkspaceFileTab = vi.fn();

    render(
      <HostContainersToolContent
        {...inspectorProps()}
        onListDockerContainers={vi.fn().mockResolvedValue([api])}
        onOpenWorkspaceFileTab={onOpenWorkspaceFileTab}
        selectedMachine={host}
      />,
    );

    expect(
      screen.queryByRole("button", {
        name: "刷新 Compose 应用 kerminal",
      }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("/srv/kerminal")).not.toBeInTheDocument();

    await user.click(
      await screen.findByRole("button", {
        name: "打开 Compose YAML kerminal",
      }),
    );

    expect(onOpenWorkspaceFileTab).toHaveBeenCalledWith({
      access: "readonly",
      path: "/srv/kerminal/compose.yaml",
      rootPath: "/srv/kerminal",
      source: "composeYaml",
      target: { hostId: "ubuntu-dev", kind: "ssh" },
    });
  });

  it("keeps container actions in the row menu until details are opened", async () => {
    const user = userEvent.setup();
    const api = container({ id: "c0ffee1234567890", name: "api" });
    const props = inspectorProps();
    const onPinContainer = vi.fn();

    render(
      <HostContainersToolContent
        {...props}
        onListDockerContainers={vi.fn().mockResolvedValue([api])}
        onPinContainer={onPinContainer}
        selectedMachine={host}
      />,
    );

    expect(
      await screen.findByRole("button", { name: "进入容器 api" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByTestId("host-container-selection-summary"),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "概览" })).not.toBeInTheDocument();
    expect(props.onInspectContainer).not.toHaveBeenCalled();

    await user.click(
      screen.getByRole("button", { name: "更多容器操作 api" }),
    );

    expect(screen.getByRole("menuitem", { name: "日志" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "固定" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "详情" })).not.toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "监控" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("menuitem", { name: "日志" }));
    expect(props.onOpenContainerLogs).toHaveBeenCalledWith(api);

    await user.click(
      screen.getByRole("button", { name: "更多容器操作 api" }),
    );
    await user.click(screen.getByRole("menuitem", { name: "固定" }));
    expect(onPinContainer).toHaveBeenCalledWith(api);
  });

  it("uses one container summary and keeps sidebar rows focused on status and ports", async () => {
    const user = userEvent.setup();
    const onEnterContainer = vi.fn();
    const api = container({
      id: "c0ffee1234567890",
      name: "api",
      ports: ["0.0.0.0:8080->80/tcp"],
    });
    const db = container({
      id: "deadbeef98765432",
      image: "postgres:16",
      name: "postgres",
      state: "exited",
      status: "exited",
      statusText: "Exited (0) 2 hours ago",
    });

    render(
      <HostContainersToolContent
        {...inspectorProps()}
        onEnterContainer={onEnterContainer}
        onListDockerContainers={vi.fn().mockResolvedValue([api, db])}
        selectedMachine={host}
      />,
    );

    expect(await screen.findByTestId("host-container-summary")).toHaveTextContent(
      "1 运行 · 1 停止 · 0 Compose · 2 独立",
    );
    const apiRow = screen.getByRole("option", { name: "容器 api" });
    expect(within(apiRow).getByText("运行中")).toBeInTheDocument();
    expect(within(apiRow).getByText("0.0.0.0:8080->80/tcp")).toBeInTheDocument();
    expect(within(apiRow).queryByText("kerminal/api:latest")).not.toBeInTheDocument();
    expect(within(apiRow).queryByText("c0ffee123456")).not.toBeInTheDocument();
    expect(within(apiRow).queryByText("Up 12 minutes")).not.toBeInTheDocument();

    apiRow.focus();
    await user.keyboard("{Enter}");
    expect(onEnterContainer).toHaveBeenCalledWith(api);
  });

  it("starts a stopped container directly and refreshes the right panel list", async () => {
    const user = userEvent.setup();
    const db = container({
      id: "deadbeef98765432",
      image: "postgres:16",
      name: "postgres",
      state: "exited",
      status: "exited",
      statusText: "Exited (0) 2 hours ago",
    });
    const onLifecycleContainer = vi.fn().mockResolvedValue(undefined);
    const onListDockerContainers = vi.fn().mockResolvedValue([db]);

    render(
      <HostContainersToolContent
        {...inspectorProps()}
        onLifecycleContainer={onLifecycleContainer}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    await user.click(
      await screen.findByRole("button", { name: "启动容器 postgres" }),
    );

    expect(onLifecycleContainer).toHaveBeenCalledWith("start", db, undefined);
    await waitFor(() => {
      expect(onListDockerContainers).toHaveBeenCalledTimes(2);
    });
  });

  it("keeps raw container failures collapsed behind a recovery message", async () => {
    const user = userEvent.setup();
    const onListDockerContainers = vi
      .fn()
      .mockRejectedValue(
        new Error(
          "docker_runtime lease poisoned token=container-internal-secret",
        ),
      );

    render(
      <HostContainersToolContent
        {...inspectorProps()}
        onListDockerContainers={onListDockerContainers}
        selectedMachine={host}
      />,
    );

    expect(await screen.findByText("无法读取容器")).toBeVisible();
    expect(
      screen.getByText("请确认主机连接和容器运行时可用，然后重试。"),
    ).toBeVisible();
    const technicalDetail = screen.getByText(/docker_runtime lease poisoned/);
    expect(technicalDetail.closest("details")).not.toHaveAttribute("open");
    expect(
      screen.queryByText(/container-internal-secret/),
    ).not.toBeInTheDocument();

    await user.click(screen.getByText("技术详情"));

    expect(technicalDetail.closest("details")).toHaveAttribute("open");
  });
});
