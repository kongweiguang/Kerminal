// @author kongweiguang

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AgentLauncherView } from "../../../../src/features/tool-panel/agent-launcher/AgentLauncherView";

const codexSelectorOption = {
  agentId: "codex" as const,
  key: "builtin:codex",
  name: "Codex",
  statusLabel: "可用",
  tone: "ready" as const,
};

const baseProps = {
  actionError: null,
  actionState: null,
  agentOptions: [codexSelectorOption],
  agentTechnicalDetail: "",
  currentAgentTargetLabel: "未绑定",
  customAgentError: null,
  customAgentMutationPending: false,
  customAgents: [],
  deletingSessionId: null,
  loadError: null,
  loadState: "idle" as const,
  pendingSendRequest: null,
  selectedAgentKey: "builtin:codex",
  onAgentSelect: vi.fn(),
  onCancelRestore: vi.fn(),
  onContinueRestore: vi.fn(),
  onCustomAgentDelete: vi.fn().mockResolvedValue(true),
  onCustomAgentSave: vi.fn().mockResolvedValue(true),
  onLaunchSelected: vi.fn(),
  onNewSession: vi.fn(),
  onRetry: vi.fn(),
  onWorkflowRename: vi.fn().mockResolvedValue(true),
  onWorkflowDelete: vi.fn().mockResolvedValue(true),
  renamingSessionId: null,
  restoreChoice: null,
  statusAvailable: true,
  visible: true,
};

describe("AgentLauncher workflow integration", () => {
  it("在固定标题区显示待发送状态，不插入会挤压 Agent 按钮的内容框", () => {
    const { rerender } = render(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        pendingSendRequest={{
          expiresAt: Date.now() + 60_000,
          id: 1,
          paneId: "pane-local",
          source: "selection",
          tabId: "tab-local",
        }}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [],
          stale: false,
        }}
      />,
    );

    expect(screen.getByTestId("agent-launcher-content")).toHaveClass("my-auto");
    expect(screen.getByRole("heading", { name: "发送到 Agent" })).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent(
      "选中内容待发送 · 选择 Agent 后预览",
    );
    expect(screen.getByTestId("agent-launcher-pending-send")).toHaveClass("h-4");
    expect(screen.queryByLabelText("待发送到 Agent")).not.toBeInTheDocument();

    rerender(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 2,
          sessions: [],
          stale: false,
        }}
      />,
    );

    expect(screen.getByRole("heading", { name: "新建对话" })).toBeInTheDocument();
    expect(screen.queryByTestId("agent-launcher-pending-send")).not.toBeInTheDocument();
  });

  it("保存自定义 Agent 成功后关闭管理弹窗并聚焦主进入按钮", async () => {
    const onCustomAgentSave = vi.fn().mockResolvedValue(true);
    render(
      <AgentLauncherView
        {...baseProps}
        onCustomAgentSave={onCustomAgentSave}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [],
          stale: false,
        }}
      />,
    );

    fireEvent.click(screen.getByRole("combobox", { name: "选择 Agent" }));
    fireEvent.click(
      screen.getByRole("button", { name: "添加自定义 Agent" }),
    );
    fireEvent.change(
      screen.getByRole("textbox", { name: "自定义 Agent 名称" }),
      { target: { value: "PI Agent" } },
    );
    fireEvent.change(
      screen.getByRole("textbox", { name: "自定义 Agent 启动命令" }),
      { target: { value: "pi" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "保存并选择" }));

    await waitFor(() => expect(onCustomAgentSave).toHaveBeenCalledTimes(1));
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "使用 Codex 进入" }),
      ).toHaveFocus(),
    );
    expect(
      screen.queryByRole("heading", { name: "管理自定义 Agent" }),
    ).not.toBeInTheDocument();
  });

  it("普通关闭自定义 Agent 管理弹窗后恢复选择器焦点", async () => {
    render(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [],
          stale: false,
        }}
      />,
    );

    const selector = screen.getByRole("combobox", { name: "选择 Agent" });
    fireEvent.click(selector);
    fireEvent.click(
      screen.getByRole("button", { name: "添加自定义 Agent" }),
    );
    fireEvent.click(screen.getByRole("button", { name: "关闭弹窗" }));

    await waitFor(() => expect(selector).toHaveFocus());
    expect(
      screen.queryByRole("heading", { name: "管理自定义 Agent" }),
    ).not.toBeInTheDocument();
  });

  it("旧 PI 会话缺少 launcherKey 时仍按内置 key 展示恢复中状态", () => {
    const { container } = render(
      <AgentLauncherView
        {...baseProps}
        actionState="builtin:pi"
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        restoreChoice={{
          agentId: "pi",
          permissionMode: "default",
          session: {
            agentSessionId: "ags-legacy-pi",
            scope: { kind: "tab", tabId: "tab-local" },
            tabId: "tab-local",
            title: "PI Agent",
          },
        }}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [],
          stale: false,
        }}
      />,
    );

    expect(
      screen.queryByRole("button", { name: "继续上次" }),
    ).not.toBeInTheDocument();
    expect(container.querySelector(".animate-spin")).toBeInTheDocument();
  });

  it("展示统一状态并只以上抛 session id 的方式执行继续和同 Agent 新会话", () => {
    const onWorkflowContinue = vi.fn();
    const onWorkflowNewSession = vi.fn();

    render(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={onWorkflowContinue}
        onWorkflowNewSession={onWorkflowNewSession}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [
            {
              agentId: "codex",
              agentSessionId: "ags-1",
              repositoryStatus: "active",
              runtimeStatus: "waitingForUser",
              statusSource: "terminalSignal",
              title: "Codex",
            },
          ],
          stale: false,
        }}
      />,
    );

    expect(screen.getByLabelText("Agent 状态：等待人工")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "继续对话" }));
    fireEvent.click(screen.getByRole("button", { name: "同 Agent 新会话" }));
    expect(onWorkflowContinue).toHaveBeenCalledWith("ags-1");
    expect(onWorkflowNewSession).toHaveBeenCalledWith("ags-1");
  });

  it("Pi 历史会话的身份标签与自定义标题独立显示", () => {
    render(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [
            {
              agentId: "pi",
              agentSessionId: "ags-pi",
              repositoryStatus: "active",
              runtimeStatus: "waitingForUser",
              statusSource: "terminalSignal",
              title: "服务器排查",
            },
          ],
          stale: false,
        }}
      />,
    );

    const card = screen.getByRole("heading", { name: "服务器排查" }).closest("article");
    expect(card).not.toBeNull();
    expect(within(card!).getByText("PI Agent")).toBeInTheDocument();
    expect(within(card!).queryByText("Codex")).not.toBeInTheDocument();
  });

  it("历史仅展示 metadata，不渲染 prompt 正文", () => {
    render(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [
            {
              action: "sent",
              createdAt: "2026-07-11T00:00:00.000Z",
              id: "history-1",
              outcome: "sent",
              sessionId: "ags-1",
              submit: true,
              textBytes: 128,
            },
          ],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [],
          stale: false,
        }}
      />,
    );

    expect(
      screen.getByRole("list", { name: "Agent 操作历史" }),
    ).toHaveTextContent("128 B");
    expect(screen.queryByText("secret prompt body")).not.toBeInTheDocument();
  });

  it("按当前目标筛选会话并允许修改 Kerminal 会话标题", async () => {
    const onWorkflowRename = vi.fn().mockResolvedValue(true);
    render(
      <AgentLauncherView
        {...baseProps}
        currentAgentTarget={{ targetRef: "ssh:prod" }}
        onWorkflowContinue={vi.fn()}
        onWorkflowNewSession={vi.fn()}
        onWorkflowRename={onWorkflowRename}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [
            {
              agentId: "codex",
              agentSessionId: "ags-current",
              repositoryStatus: "active",
              runtimeStatus: "running",
              statusSource: "repository",
              target: { targetRef: "ssh:prod" },
              title: "生产排障",
            },
            {
              agentId: "claude",
              agentSessionId: "ags-other",
              repositoryStatus: "active",
              runtimeStatus: "done",
              statusSource: "repository",
              target: { targetRef: "ssh:staging" },
              title: "测试环境",
            },
          ],
          stale: false,
        }}
      />,
    );

    expect(screen.getByText("生产排障")).toBeInTheDocument();
    expect(screen.queryByText("测试环境")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "重命名 生产排障" }));
    fireEvent.change(screen.getByRole("textbox", { name: "会话标题" }), {
      target: { value: "生产发布检查" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存标题" }));

    await waitFor(() =>
      expect(onWorkflowRename).toHaveBeenCalledWith(
        "ags-current",
        "生产发布检查",
      ),
    );
  });

  it("删除会话记录前确认，并隐藏已归档记录", async () => {
    const onWorkflowDelete = vi.fn().mockResolvedValue(true);
    render(
      <AgentLauncherView
        {...baseProps}
        onWorkflowContinue={vi.fn()}
        onWorkflowDelete={onWorkflowDelete}
        onWorkflowNewSession={vi.fn()}
        workflowSnapshot={{
          disposed: false,
          historyMetadata: [],
          loading: false,
          queueMetadata: [],
          revision: 1,
          sessions: [
            {
              agentId: "codex",
              agentSessionId: "ags-active",
              repositoryStatus: "active",
              runtimeStatus: "done",
              statusSource: "repository",
              title: "保留的会话",
            },
            {
              agentId: "claude",
              agentSessionId: "ags-archived",
              repositoryStatus: "archived",
              runtimeStatus: "done",
              statusSource: "repository",
              title: "已删除的会话",
            },
          ],
          stale: false,
        }}
      />,
    );

    expect(screen.queryByText("已删除的会话")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "删除 保留的会话" }));
    expect(
      screen.getByRole("dialog", { name: "删除会话记录？" }),
    ).toHaveTextContent("不会被删除");
    fireEvent.click(screen.getByRole("button", { name: "删除记录" }));

    await waitFor(() =>
      expect(onWorkflowDelete).toHaveBeenCalledWith("ags-active"),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "删除会话记录？" }),
      ).not.toBeInTheDocument(),
    );
  });
});
