// @author kongweiguang

import "../../support/tool-panel/ToolPanel.testSupport";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { tools } from "../../../../src/features/workspace/workspaceData";
import { ToolPanel } from "../../../../src/features/tool-panel/ToolPanel";
import { publishXtermPaneArtifactSnapshot, removeXtermPaneArtifactSnapshot } from "../../../../src/features/terminal/XtermPane.artifactsRegistry";
import { requestAgentSend } from "../../../../src/features/agent-workflow/agentSendRequestStore";
import {
  contextWorkspaceProjection,
  focusedSshPane,
  localMachine,
  sshTerminalTab,
} from "../../support/tool-panel/ToolPanel.testSupport";

describe("ToolPanel core", () => {
it("renders only the rail when no tool drawer is active", () => {
  render(
    <ToolPanel
      activeTool={null}
      onActiveToolChange={vi.fn()}
      tools={tools}
    />,
  );

  expect(
    screen.getByRole("complementary", { name: "工具面板" }),
  ).toHaveAttribute("aria-expanded", "false");
  expect(
    screen.getByRole("button", { name: "打开 Agent Launcher" }),
  ).toBeInTheDocument();
  expect(
    screen.queryByRole("heading", { name: "Agent Launcher" }),
  ).not.toBeInTheDocument();
});

it("opens Agent Launcher when another tool is active and a send request arrives", async () => {
  const onActiveToolChange = vi.fn();
  render(
    <ToolPanel
      activeTool="context"
      onActiveToolChange={onActiveToolChange}
      tools={tools}
    />,
  );

  act(() => {
    requestAgentSend({ paneId: "pane-local", source: "selection" });
  });

  await waitFor(() => {
    expect(onActiveToolChange).toHaveBeenCalledWith("agentLauncher");
  });
});

it("Context 工具只启用真实导航并以只读模式展示终端产物", async () => {
  const user = userEvent.setup();
  const onFocusTab = vi.fn();
  publishXtermPaneArtifactSnapshot({
    artifacts: [
      {
        actions: [{ enabled: true, id: "copy", requiresConfirmation: false }],
        createdAt: 1,
        dedupeKey: "context-artifact",
        id: "context-artifact",
        kind: "url",
        label: "运行报告",
        paneId: focusedSshPane.id,
        pathStyle: "uri",
        revision: 1,
        sensitivity: "normal",
        source: "osc8",
        target: { id: "local", kind: "local" },
        value: "https://example.test/report",
      },
    ],
    degraded: false,
    disposed: false,
    evictions: 0,
    paneId: focusedSshPane.id,
    rejected: 0,
    revision: 1,
  });

  render(
    <ToolPanel
      activeTool="context"
      onActiveToolChange={vi.fn()}
      onFocusTab={onFocusTab}
      tools={tools}
      workspaceContext={contextWorkspaceProjection}
    />,
  );

  await user.click(
    await screen.findByRole(
      "button",
      { name: /活动页签/ },
      { timeout: 3_000 },
    ),
  );
  expect(onFocusTab).toHaveBeenCalledWith(sshTerminalTab.id);
  expect(screen.queryByRole("button", { name: /当前目录/ })).toBeNull();
  expect(screen.queryByRole("button", { name: /焦点窗格/ })).toBeNull();
  await user.click(screen.getByText("终端发现"));
  expect(screen.getByText("运行报告")).toBeVisible();
  expect(screen.queryByRole("button", { name: "复制" })).toBeNull();

  removeXtermPaneArtifactSnapshot(focusedSshPane.id);
});

it("renders the active Agent Launcher tool", async () => {
  render(
    <ToolPanel
      activeTool="agentLauncher"
      onActiveToolChange={vi.fn()}
      tools={tools}
    />,
  );

  expect(
    screen.getByRole("complementary", { name: "工具面板" }),
  ).toBeInTheDocument();
  expect(
    await screen.findByRole(
      "combobox",
      { name: "选择 Agent" },
      { timeout: 10000 },
    ),
  ).toHaveAttribute("aria-valuetext", "Codex");
  expect(
    screen.getByRole("button", { name: "使用 Codex 进入" }),
  ).toBeInTheDocument();
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(screen.queryByText("历史会话")).not.toBeInTheDocument();
  expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  expect(
    screen.queryByText(/Agent 栈：rig-core、rmcp/i),
  ).not.toBeInTheDocument();
}, 20000);

it("requests a tool switch from the rail", async () => {
  const user = userEvent.setup();
  const onActiveToolChange = vi.fn();

  render(
    <ToolPanel
      activeTool="agentLauncher"
      onActiveToolChange={onActiveToolChange}
      tools={tools}
    />,
  );

  await user.click(screen.getByRole("button", { name: "打开 文件" }));

  expect(onActiveToolChange).toHaveBeenCalledWith("sftp");
});

it("presents the logs tool as command history without diagnostics actions", async () => {
  render(
    <ToolPanel
      activeTool="logs"
      activeMachine={localMachine}
      onActiveToolChange={vi.fn()}
      tools={tools}
    />,
  );

  const logsTitle = screen.getByRole("heading", { name: "命令历史" });
  const header = logsTitle.closest("header");
  expect(header).toBeInTheDocument();
  expect(screen.queryByText("当前工具")).not.toBeInTheDocument();
  expect(
    screen.queryByText(
      tools.find((tool) => tool.id === "logs")?.description ?? "",
    ),
  ).not.toBeInTheDocument();
  expect(
    within(header as HTMLElement).queryByRole("button"),
  ).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "导出日志" })).not.toBeInTheDocument();
  expect(screen.queryByText("应用日志")).not.toBeInTheDocument();
});

it("keeps settings out of the rail without rendering settings content inside the right tool panel", async () => {
  const onActiveToolChange = vi.fn();

  render(
    <ToolPanel
      activeTool="settings"
      onActiveToolChange={onActiveToolChange}
      tools={tools}
    />,
  );

  expect(
    await screen.findByRole(
      "combobox",
      { name: "选择 Agent" },
      { timeout: 5000 },
    ),
  ).toHaveAttribute("aria-valuetext", "Codex");
  expect(screen.queryByText("终端外观")).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "收起 设置" }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "打开 设置" }),
  ).not.toBeInTheDocument();
  expect(onActiveToolChange).not.toHaveBeenCalled();
});

});
