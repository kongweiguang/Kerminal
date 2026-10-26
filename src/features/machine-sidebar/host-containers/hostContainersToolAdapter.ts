// @author kongweiguang

import {
  removeDockerContainer,
  restartDockerContainer,
  startDockerContainer,
  stopDockerContainer,
  type DockerContainerInfoRequest,
  type DockerContainerInspectSummary,
  type DockerContainerLifecycleAction,
  type DockerContainerListRequest,
  type DockerContainerStatsRequest,
  type DockerContainerStatsResult,
  type DockerContainerSummary,
} from "../../../lib/dockerApi";
import type { Machine } from "../../workspace/contracts/index";
import type { OpenWorkspaceFileTabOptions } from "../../workspace/state/index";
import type { ContainerRuntime } from "../../../lib/targetModel";

export interface HostContainersToolContentProps {
  active?: boolean;
  initialContainerId?: string;
  initialRequestId?: number;
  initialRuntime?: ContainerRuntime;
  onEnterContainer?: (container: DockerContainerSummary) => void;
  onFetchContainerStats?: (request: DockerContainerStatsRequest) => Promise<DockerContainerStatsResult>;
  onInspectContainer?: (request: DockerContainerInfoRequest) => Promise<DockerContainerInspectSummary>;
  onLifecycleContainer?: (
    action: DockerContainerLifecycleAction,
    container: DockerContainerSummary,
    options?: { force?: boolean },
  ) => void | Promise<void>;
  onListDockerContainers?: (request: DockerContainerListRequest) => Promise<DockerContainerSummary[]>;
  onOpenContainerLogs?: (container: DockerContainerSummary) => void;
  onOpenWorkspaceFileTab?: (options: OpenWorkspaceFileTabOptions) => void;
  onPinContainer?: (container: DockerContainerSummary) => void | Promise<void>;
  refreshRequestId?: number;
  selectedMachine?: Machine;
}

/** 默认 adapter 只负责把 UI 生命周期命令翻译为 docker API 调用。 */
export async function runDefaultLifecycleAction(
  action: DockerContainerLifecycleAction,
  container: DockerContainerSummary,
  options?: { force?: boolean },
) {
  const request = {
    containerId: container.id,
    force: options?.force,
    hostId: container.hostId,
    runtime: container.runtime,
  };
  if (action === "start") await startDockerContainer(request);
  else if (action === "stop") await stopDockerContainer(request);
  else if (action === "restart") await restartDockerContainer(request);
  else await removeDockerContainer(request);
}

export function presentContainerSummary(input: {
  composeErrors: number;
  composeProjects: number;
  loadError: unknown;
  loading: boolean;
  running: number;
  standalone: number;
  total: number;
}) {
  if (input.loading) return "正在检查容器";
  if (input.loadError) return "容器读取失败";
  const stopped = Math.max(0, input.total - input.running);
  return [
    `${input.running} 运行`,
    `${stopped} 停止`,
    `${input.composeProjects} Compose`,
    `${input.standalone} 独立`,
    input.composeErrors > 0 ? `${input.composeErrors} 异常` : null,
  ].filter(Boolean).join(" · ");
}
