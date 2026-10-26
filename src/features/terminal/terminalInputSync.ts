// @author kongweiguang
import { writeTerminal } from "../../lib/terminalApi";
import type { PaneSessionRecord } from "./terminalSessionRegistry";

export type TerminalInputSyncBinding = Pick<
  PaneSessionRecord,
  "sessionId" | "connectionGeneration" | "tabId"
>;

export interface TerminalInputSyncInput {
  data: string;
  /** 人工粘贴正文已归一化为 CR，不包含终端转义包装。 */
  pasteText?: string;
}

export interface TerminalInputSyncEndpoint {
  readBinding(): TerminalInputSyncBinding | null;
  prepareMirroredInput(input: TerminalInputSyncInput): string;
  observeMirroredInput(data: string): void;
  onWriteError(): void;
}

export interface TerminalInputSyncSnapshot {
  enabled: boolean;
  disabledReason: string | null;
  notice: { level: "status" | "error"; message: string } | null;
}

export interface TerminalInputSyncController {
  configure(tabId: string | null, selectedPaneIds: readonly string[]): void;
  register(paneId: string, endpoint: TerminalInputSyncEndpoint): () => void;
  setEnabled(enabled: boolean): void;
  refresh(): void;
  stopForPane(paneId: string): void;
  writeUserInput(paneId: string, input: TerminalInputSyncInput): Promise<void>;
  getSnapshot(): TerminalInputSyncSnapshot;
  subscribe(listener: () => void): () => void;
}

interface EndpointRegistration {
  endpoint: TerminalInputSyncEndpoint;
}

interface BoundEndpoint {
  paneId: string;
  registration: EndpointRegistration;
  binding: TerminalInputSyncBinding;
}

interface InputSyncGroup {
  tabId: string;
  members: Map<string, BoundEndpoint>;
}

interface InputWrite {
  target: BoundEndpoint;
  data: string;
  group: InputSyncGroup | null;
  mirrored: boolean;
}

const TARGET_COUNT_REASON = "请至少选择两个终端分屏";
const CONNECTION_REASON = "所选分屏尚未全部连接";
const TARGET_CHANGED_NOTICE = "目标已变更，同步已关闭。";
const CONNECTION_CHANGED_NOTICE =
  "分屏连接已变化，同步已关闭；请确认各端输入后重新开启。";
const WRITE_ERROR_NOTICE =
  "同步已关闭：部分输入可能未送达，请检查各分屏后重新开启。";
const DIRECTED_INPUT_NOTICE = "检测到定向输入，同步已关闭。";

/**
 * 创建不持久化的同步控制器；组对象自身就是开启代次，不复用已关闭的组。
 * 来源模型由调用方先更新，镜像模型在本次同步入队阶段更新，不等待 PTY 回执。
 */
export function createTerminalInputSyncController(): TerminalInputSyncController {
  const endpoints = new Map<string, EndpointRegistration>();
  const queues = new Map<string, Promise<void>>();
  const listeners = new Set<() => void>();
  let tabId: string | null = null;
  let selectedPaneIds = new Set<string>();
  let group: InputSyncGroup | null = null;
  let notice: TerminalInputSyncSnapshot["notice"] = null;
  let snapshot: TerminalInputSyncSnapshot = {
    enabled: false,
    disabledReason: TARGET_COUNT_REASON,
    notice: null,
  };

  /** 只发布语义变化，避免逐次输入引起 React 重渲染；订阅失败不破坏输入队列。 */
  function publish(disabledReason: string | null): void {
    if (
      snapshot.enabled === (group !== null) &&
      snapshot.disabledReason === disabledReason &&
      snapshot.notice?.level === notice?.level &&
      snapshot.notice?.message === notice?.message
    ) {
      return;
    }
    snapshot = { enabled: group !== null, disabledReason, notice };
    for (const listener of listeners) {
      try {
        listener();
      } catch {
        // 单个订阅者异常不能阻断其它分屏的写入或产生未处理的 Promise。
      }
    }
  }

  /** 拍下 binding 值而非保存外部引用，旧任务不能跟随 runtime 的可变对象换连接。 */
  function readEndpoint(paneId: string): BoundEndpoint | null {
    const registration = endpoints.get(paneId);
    if (!registration) {
      return null;
    }
    try {
      const binding = registration.endpoint.readBinding();
      if (
        !binding?.sessionId ||
        !Number.isSafeInteger(binding.connectionGeneration) ||
        binding.connectionGeneration < 1 ||
        endpoints.get(paneId) !== registration
      ) {
        return null;
      }
      return {
        paneId,
        registration,
        binding: {
          sessionId: binding.sessionId,
          connectionGeneration: binding.connectionGeneration,
          tabId: binding.tabId,
        },
      };
    } catch {
      return null;
    }
  }

  /** 目标资格是全有或全无，绝不跳过未连接成员后把剩余成员当成同步组。 */
  function readEligibleMembers(): Map<string, BoundEndpoint> | null {
    if (tabId === null || selectedPaneIds.size < 2) {
      return null;
    }
    const members = new Map<string, BoundEndpoint>();
    for (const paneId of selectedPaneIds) {
      const target = readEndpoint(paneId);
      if (!target || target.binding.tabId !== tabId) {
        return null;
      }
      members.set(paneId, target);
    }
    return members;
  }

  /** 同时检查注册身份、会话、连接代次和所属标签，不只判断 sessionId 是否存在。 */
  function sameEndpoint(left: BoundEndpoint, right: BoundEndpoint): boolean {
    return (
      left.registration === right.registration &&
      left.binding.sessionId === right.binding.sessionId &&
      left.binding.connectionGeneration === right.binding.connectionGeneration &&
      left.binding.tabId === right.binding.tabId
    );
  }

  /** 已开启组固定全部成员的 binding，任何一员变化都关闭整组且不自动恢复。 */
  function groupMatches(members: Map<string, BoundEndpoint> | null): boolean {
    if (
      !group ||
      group.tabId !== tabId ||
      !members ||
      members.size !== group.members.size
    ) {
      return false;
    }
    for (const [paneId, expected] of group.members) {
      const current = members.get(paneId);
      if (!current || !sameEndpoint(expected, current)) {
        return false;
      }
    }
    return true;
  }

  /** 每次连接通知、入队和出队共用同一门禁，关闭后的旧镜像不会重新变得有效。 */
  function refreshState(): Map<string, BoundEndpoint> | null {
    const members = readEligibleMembers();
    if (group && !groupMatches(members)) {
      group = null;
      notice = { level: "status", message: CONNECTION_CHANGED_NOTICE };
    }
    publish(
      selectedPaneIds.size < 2
        ? TARGET_COUNT_REASON
        : members
          ? null
          : CONNECTION_REASON,
    );
    return members;
  }

  /** 迟到失败不能通知替代 runtime；读取异常与注销同样视为旧 binding 失效。 */
  function isCurrentEndpoint(expected: BoundEndpoint): boolean {
    const current = readEndpoint(expected.paneId);
    return current !== null && sameEndpoint(expected, current);
  }

  /** 当前组失败使未发镜像失效；本地或旧组失败不关闭新组，不重试也不记录输入。 */
  function handleWriteError(write: InputWrite): void {
    refreshState();
    if (write.group && group !== write.group) {
      return;
    }
    if (write.group) {
      group = null;
      notice = { level: "error", message: WRITE_ERROR_NOTICE };
    }
    if (isCurrentEndpoint(write.target)) {
      try {
        write.target.registration.endpoint.onWriteError();
      } catch {
        // 错误提示回调本身失败也不能让写入 Promise 泄漏 rejection。
      }
    }
    refreshState();
  }

  /**
   * 先为所有 pane 占据 FIFO 位置，再同步启动空队列；不同 pane 不互相等待。
   * 预占位置也覆盖同步 PTY 回调重入，旧任务只能按自身 Promise 身份清理队尾。
   */
  function enqueueWrites(writes: InputWrite[]): Promise<void> {
    const pending = writes.map((write) => {
      const paneId = write.target.paneId;
      const previous = queues.get(paneId);
      let finish!: () => void;
      const completion = new Promise<void>((resolve) => {
        finish = resolve;
      });
      queues.set(paneId, completion);

      /** 出队复核全组及自身；来源本地输入只依赖自身 binding，不随组关闭丢弃。 */
      const run = async () => {
        try {
          refreshState();
          if (write.mirrored && group !== write.group) {
            return;
          }
          if (!isCurrentEndpoint(write.target)) {
            return;
          }
          await writeTerminal(write.target.binding.sessionId, write.data);
        } catch {
          handleWriteError(write);
        }
      };

      /** 无论同步抛错还是异步拒绝都释放本任务，不删除后续任务持有的队尾。 */
      const complete = () => {
        if (queues.get(paneId) === completion) {
          queues.delete(paneId);
        }
        finish();
      };

      return {
        completion,
        start: () => {
          const operation = previous ? previous.then(run) : run();
          void operation.then(complete, complete);
        },
      };
    });
    for (const entry of pending) {
      entry.start();
    }
    // 调用方只等待来源自己的 FIFO，慢接收端不会反向阻塞来源继续输入。
    return pending[0].completion;
  }

  /** 集合比较忽略布局顺序和重复 ID；换标签或 owner 清空时不携带旧组提示。 */
  function configure(nextTabId: string | null, nextPaneIds: readonly string[]): void {
    const nextSelected = new Set(nextPaneIds);
    const tabChanged = tabId !== nextTabId;
    const membersChanged =
      nextSelected.size !== selectedPaneIds.size ||
      [...nextSelected].some((paneId) => !selectedPaneIds.has(paneId));
    if (tabChanged || membersChanged) {
      if (tabChanged || nextTabId === null) {
        notice = null;
      } else if (group) {
        notice = { level: "status", message: TARGET_CHANGED_NOTICE };
      }
      group = null;
      tabId = nextTabId;
      selectedPaneIds = nextSelected;
    }
    refreshState();
  }

  /** 每次注册都有独立身份，旧 disposer 不能注销同一 pane 后注册的 endpoint。 */
  function register(paneId: string, endpoint: TerminalInputSyncEndpoint): () => void {
    const registration = { endpoint };
    endpoints.set(paneId, registration);
    refreshState();
    return () => {
      if (endpoints.get(paneId) !== registration) {
        return;
      }
      endpoints.delete(paneId);
      refreshState();
    };
  }

  /** 只在显式开启时固定新组，连接恢复、目标恢复或 refresh 均不自动开启。 */
  function setEnabled(enabled: boolean): void {
    const members = refreshState();
    if (enabled) {
      if (group || !members || tabId === null) {
        return;
      }
      group = { tabId, members };
    } else {
      group = null;
    }
    notice = null;
    refreshState();
  }

  /** 程序化定向输入只关闭包含该 pane 的已开启组，不干扰其它独立终端。 */
  function stopForPane(paneId: string): void {
    if (!group?.members.has(paneId)) {
      return;
    }
    group = null;
    notice = { level: "status", message: DIRECTED_INPUT_NOTICE };
    refreshState();
  }

  /**
   * 先完成全部镜像编码和模型更新，再启动任何 PTY 写入；来源模型不在此重复更新。
   * 来源不必被选中或属于当前标签，其有效本地输入始终使用同一 pane FIFO。
   */
  function writeUserInput(paneId: string, input: TerminalInputSyncInput): Promise<void> {
    if (input.data === "" && !input.pasteText) {
      return Promise.resolve();
    }
    const members = refreshState();
    const source = members?.get(paneId) ?? readEndpoint(paneId);
    if (!source) {
      return Promise.resolve();
    }
    const inputGroup = group?.members.has(paneId) ? group : null;
    const sourceWrite: InputWrite = {
      target: source,
      data: input.data,
      group: inputGroup,
      mirrored: false,
    };
    const writes = [sourceWrite];
    if (inputGroup) {
      const mirrored: InputWrite[] = [];
      let preparing: InputWrite | null = null;
      try {
        for (const [targetPaneId, target] of inputGroup.members) {
          if (targetPaneId === paneId) {
            continue;
          }
          preparing = { target, data: "", group: inputGroup, mirrored: true };
          preparing.data = target.registration.endpoint.prepareMirroredInput(input);
          mirrored.push(preparing);
        }
        for (const write of mirrored) {
          preparing = write;
          write.target.registration.endpoint.observeMirroredInput(write.data);
        }
        writes.push(...mirrored);
      } catch {
        if (preparing) {
          handleWriteError(preparing);
        }
      }
    }
    // 编码/观察回调可能改变连接；入队复核后仍保留有效来源，不补发失效镜像。
    refreshState();
    return enqueueWrites(writes);
  }

  /** 订阅只持有监听器，不持久化开关或用户输入。 */
  function subscribe(listener: () => void): () => void {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }

  return {
    configure,
    register,
    setEnabled,
    refresh: () => {
      refreshState();
    },
    stopForPane,
    writeUserInput,
    getSnapshot: () => snapshot,
    subscribe,
  };
}

export const terminalInputSync: TerminalInputSyncController =
  createTerminalInputSyncController();
