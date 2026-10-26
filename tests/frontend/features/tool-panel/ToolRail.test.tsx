// @author kongweiguang

import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { tools } from "../../../../src/features/workspace/workspaceData";
import { ToolRail } from "../../../../src/features/tool-panel/ToolRail";
import {
  defaultToolRailSettings,
  type ToolRailSettings,
} from "../../../../src/features/tool-panel/toolRailModel";

describe("ToolRail", () => {
  /** 主区顺序和固定底部分区共同决定 DOM 次序，新入口仍可发现。 */
  it("在两个入口使用相同顺序并响应右击编辑", () => {
    const onOpenCustomizer = vi.fn();
    const onActiveToolChange = vi.fn();
    const navSettings: ToolRailSettings = {
      ...defaultToolRailSettings,
      bottom: ["ports"],
      hidden: ["logs"],
      order: [
        "system",
        "context",
        "agentLauncher",
        "sftp",
        "snippets",
        "tmux",
        "ports",
        "logs",
      ],
    };

    render(
      <ToolRail
        activeTool={null}
        onActiveToolChange={onActiveToolChange}
        onOpenToolRailCustomization={onOpenCustomizer}
        settings={navSettings}
        tools={tools}
        variant="shell"
      />,
    );

    const nav = screen.getByRole("navigation", { name: "工具栏" });
    expect(
      within(nav)
        .getAllByRole("button")
        .map((button) => button.getAttribute("aria-label")),
    ).toEqual([
      "打开 系统",
      "打开 当前上下文",
      "打开 Agent Launcher",
      "打开 文件",
      "打开 片段",
      "打开 tmux",
      "打开 容器",
      "打开 端口",
    ]);
    expect(
      within(nav)
        .getByRole("group", { name: "底部工具" })
        .querySelector('[data-shell-tool-id="ports"]'),
    ).toBeInTheDocument();

    fireEvent.contextMenu(nav);
    fireEvent.keyDown(nav, { key: "ContextMenu" });
    fireEvent.keyDown(nav, { key: "F10", shiftKey: true });
    expect(onOpenCustomizer).toHaveBeenCalledTimes(3);
  });

  it("隐藏工具通过快捷键打开时保留活动按钮", () => {
    render(
      <ToolRail
        activeTool="logs"
        onActiveToolChange={vi.fn()}
        settings={{ ...defaultToolRailSettings, hidden: ["logs"] }}
        tools={tools}
        variant="panel"
      />,
    );

    expect(screen.getByRole("button", { name: "收起 命令历史" })).toBeInTheDocument();
  });

  it("不同方向并开时同时标记全部活动入口", () => {
    render(
      <ToolRail
        activeTool="system"
        activeTools={["context", "logs", "system"]}
        onActiveToolChange={vi.fn()}
        settings={defaultToolRailSettings}
        tools={tools}
        variant="shell"
      />,
    );

    for (const toolName of ["当前上下文", "命令历史", "系统"]) {
      expect(
        screen.getByRole("button", { name: `收起 ${toolName}` }),
      ).toHaveAttribute("aria-pressed", "true");
    }
    expect(screen.getByRole("button", { name: "打开 文件" })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
  });
});
