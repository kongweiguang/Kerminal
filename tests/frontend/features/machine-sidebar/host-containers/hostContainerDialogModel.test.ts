import { describe, expect, it } from "vitest";
import {
  dockerContainerTarget,
  dockerContainerTargetCapabilities,
} from "../../../../../src/lib/targetModel";
import {
  buildHostContainerDialogViewModel,
  canEnterHostContainer,
  canRunHostContainerLifecycleAction,
  containerComposeService,
  containerProjectName,
  hostContainerLifecycleDialogCopy,
  hostContainerLifecycleDisabledReason,
  hostContainerStatusLabel,
  hostContainerStatusTone,
  type HostContainerMetadata,
} from "../../../../../src/features/machine-sidebar/host-containers/hostContainerDialogModel";

function container(
  input: Partial<HostContainerMetadata> &
    Pick<HostContainerMetadata, "id" | "name">,
): HostContainerMetadata {
  const { id, name, ...overrides } = input;
  const hostId = input.hostId ?? "host-lab";
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

describe("hostContainerDialogModel", () => {
  it("groups compose containers ahead of standalone containers", () => {
    const viewModel = buildHostContainerDialogViewModel(
      [
        container({ id: "standalone1111", name: "redis" }),
        container({
          id: "api1111111111",
          labels: {
            "com.docker.compose.project": "kerminal",
            "com.docker.compose.service": "api",
          },
          name: "api",
        }),
        container({
          id: "worker111111",
          labels: {
            "com.docker.compose.project": "kerminal",
            "com.docker.compose.service": "worker",
          },
          name: "worker",
          status: "exited",
          statusText: "Exited (0) 1 hour ago",
        }),
      ],
      { groupMode: "compose", query: "" },
    );

    expect(viewModel.totalCount).toBe(3);
    expect(viewModel.runningCount).toBe(2);
    expect(viewModel.groups.map((group) => group.title)).toEqual([
      "kerminal",
      "独立容器",
    ]);
    expect(viewModel.groups[0]!.runningCount).toBe(1);
    expect(containerProjectName(viewModel.groups[0]!.containers[0]!)).toBe(
      "kerminal",
    );
    expect(containerComposeService(viewModel.groups[0]!.containers[0]!)).toBe(
      "api",
    );
  });

  it("bridges typed compose metadata and searches config paths", () => {
    const viewModel = buildHostContainerDialogViewModel(
      [
        container({
          compose: {
            configPaths: ["/srv/kerminal/compose.prod.yaml"],
            project: "kerminal",
            service: "api",
            workingDir: "/srv/kerminal",
          },
          id: "api1111111111",
          name: "api",
        }),
        container({ id: "redis1111111", name: "redis" }),
      ],
      { groupMode: "compose", query: "compose.prod.yaml" },
    );

    expect(viewModel.containers.map((item) => item.name)).toEqual(["api"]);
    expect(containerProjectName(viewModel.containers[0]!)).toBe("kerminal");
    expect(containerComposeService(viewModel.containers[0]!)).toBe("api");
  });

  it("filters by container metadata and reports empty search", () => {
    const viewModel = buildHostContainerDialogViewModel(
      [
        container({
          id: "api1111111111",
          name: "api",
          ports: ["8080->80/tcp"],
        }),
        container({ id: "db11111111111", image: "postgres:16", name: "db" }),
      ],
      { groupMode: "flat", query: "postgres" },
    );

    expect(viewModel.containers.map((item) => item.name)).toEqual(["db"]);
    expect(viewModel.emptySearch).toBe(false);

    const emptyViewModel = buildHostContainerDialogViewModel(
      viewModel.containers,
      { groupMode: "flat", query: "no-match" },
    );
    expect(emptyViewModel.emptySearch).toBe(true);
  });

  it("builds status groups and action affordances", () => {
    const running = container({ id: "api1111111111", name: "api" });
    const paused = container({
      id: "jobs11111111",
      name: "jobs",
      status: "paused",
      statusText: "Paused",
    });
    const dead = container({
      id: "bad111111111",
      name: "bad",
      status: "dead",
      statusText: "Dead",
    });
    const viewModel = buildHostContainerDialogViewModel(
      [dead, paused, running],
      { groupMode: "status", query: "" },
      paused.id,
    );

    expect(viewModel.selectedContainer?.name).toBe("jobs");
    expect(viewModel.groups.map((group) => group.title)).toEqual([
      "运行中",
      "已暂停",
      "异常",
    ]);
    expect(hostContainerStatusLabel("restarting")).toBe("重启中");
    expect(hostContainerStatusTone("running")).toBe("running");
    expect(hostContainerStatusTone("paused")).toBe("attention");
    expect(hostContainerStatusTone("dead")).toBe("danger");
    expect(canEnterHostContainer(running)).toBe(true);
    expect(canEnterHostContainer(paused)).toBe(false);
  });

  it("derives lifecycle action availability and confirmation copy", () => {
    const running = container({ id: "api1111111111", name: "api" });
    const stopped = container({
      id: "db11111111111",
      name: "db",
      status: "exited",
      statusText: "Exited",
    });

    expect(canRunHostContainerLifecycleAction(stopped, "start")).toBe(true);
    expect(canRunHostContainerLifecycleAction(running, "start")).toBe(false);
    expect(canRunHostContainerLifecycleAction(running, "stop")).toBe(true);
    expect(canRunHostContainerLifecycleAction(stopped, "stop")).toBe(false);
    expect(canRunHostContainerLifecycleAction(running, "restart")).toBe(true);
    expect(canRunHostContainerLifecycleAction(running, "remove")).toBe(false);
    expect(canRunHostContainerLifecycleAction(stopped, "remove")).toBe(true);
    expect(hostContainerLifecycleDisabledReason(running, "remove")).toBe(
      "先停止容器再删除",
    );
    expect(hostContainerLifecycleDialogCopy("stop", running).helperText).toBe(
      "停止后可重新启动容器。",
    );
    expect(hostContainerLifecycleDialogCopy("remove", stopped)).toMatchObject({
      confirmLabel: "删除容器",
      inputLabel: "容器名",
      variant: "danger",
    });
  });
});
