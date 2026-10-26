// @author kongweiguang

import { describe, expect, it } from "vitest";
import { tools } from "../../../../src/features/workspace/workspaceData";
import {
  defaultToolRailSettings,
  normalizeOpenToolPanels,
  normalizeToolRailSettings,
  openToolPanel,
  resolveOpenToolPanels,
  resolveToolRailPanelPlacement,
  resolveToolRailSections,
  resolveToolRailTools,
  type ToolRailToolId,
} from "../../../../src/features/tool-panel/toolRailModel";

describe("toolRailModel", () => {
  /** 归一化目录新增工具时，全部隐藏偏好也必须保留可见入口。 */
  it("补齐缺失顺序、去重并保留至少一个可见工具", () => {
    const settings = normalizeToolRailSettings({
      hidden: [...defaultToolRailSettings.order],
      order: ["logs", "logs"],
    });

    expect(settings.order[0]).toBe("logs");
    expect(settings.order).toHaveLength(9);
    expect(settings.hidden).not.toContain("logs");
    expect(settings.hidden).toHaveLength(8);
    expect(settings.bottom).toEqual(["logs"]);
    expect(settings.panelPlacements.logs).toBe("attached");
  });

  it("规范化底部成员与打开位置，同时保留显式空底部区", () => {
    const settings = normalizeToolRailSettings({
      bottom: ["context", "logs", "context"],
      order: ["logs", "context"],
      panelPlacements: {
        context: "left",
        logs: "detached",
      },
    });

    expect(settings.bottom).toEqual(["logs", "context"]);
    expect(settings.panelPlacements.context).toBe("left");
    expect(settings.panelPlacements.logs).toBe("attached");
    expect(settings.panelPlacements.system).toBe("attached");
    expect(resolveToolRailPanelPlacement(settings, "context")).toBe("left");
    expect(resolveToolRailPanelPlacement(settings, "logs")).toBe("attached");
    expect(normalizeToolRailSettings({ bottom: [] }).bottom).toEqual([]);
  });

  it("接受底部面板位置并让每个方向保留最近打开的一个工具", () => {
    const settings = normalizeToolRailSettings({
      panelPlacements: {
        agentLauncher: "attached",
        context: "left",
        logs: "attached",
        sftp: "center",
        system: "bottom",
      },
    });
    const openTools = normalizeOpenToolPanels(
      ["context", "logs", "system", "sftp", "agentLauncher"],
      settings,
    );

    expect(settings.panelPlacements.system).toBe("bottom");
    expect(openTools).toEqual([
      "context",
      "system",
      "sftp",
      "agentLauncher",
    ]);
    expect(resolveOpenToolPanels(openTools, settings)).toEqual({
      attached: "agentLauncher",
      bottom: "system",
      center: "sftp",
      left: "context",
    });
  });

  it("桌面打开只替换同方向工具，紧凑布局收敛为单抽屉", () => {
    const settings = normalizeToolRailSettings({
      panelPlacements: {
        context: "left",
        logs: "attached",
        system: "bottom",
      },
    });

    expect(openToolPanel(["context", "system"], "logs", settings)).toEqual([
      "context",
      "system",
      "logs",
    ]);
    expect(
      openToolPanel(
        ["context", "system", "logs"],
        "agentLauncher",
        settings,
      ),
    ).toEqual(["context", "system", "agentLauncher"]);
    expect(
      openToolPanel(["context", "system"], "logs", settings, true),
    ).toEqual(["logs"]);
  });

  /** 旧全局位置迁移要扩展到新增工具，显式逐工具配置仍有更高优先级。 */
  it("把早期单一打开位置迁移为逐工具映射且优先采用新格式", () => {
    const migrated = normalizeToolRailSettings({ panelPlacement: "center" });
    const explicit = normalizeToolRailSettings({
      panelPlacement: "center",
      panelPlacements: { context: "attached" },
    });

    expect(Object.values(migrated.panelPlacements)).toEqual(
      Array.from({ length: 9 }, () => "center"),
    );
    expect(explicit.panelPlacements.context).toBe("attached");
    expect(explicit.panelPlacements.system).toBe("attached");
  });

  /** 未知顺序保留在原顺序后，目录新增项只追加到末尾。 */
  it("将新工具追加到用户顺序末尾，并过滤非法 id", () => {
    const settings = normalizeToolRailSettings({
      order: ["system", "not-a-tool" as ToolRailToolId],
    });

    expect(settings.order).toEqual([
      "system",
      "context",
      "agentLauncher",
      "sftp",
      "snippets",
      "tmux",
      "ports",
      "logs",
      "containers",
    ]);
  });

  it("隐藏普通工具但保留快捷键打开的活动工具", () => {
    const settings = {
      ...defaultToolRailSettings,
      hidden: ["logs"] as ToolRailToolId[],
    };

    expect(resolveToolRailTools(tools, settings).map((tool) => tool.id)).not.toContain(
      "logs",
    );
    expect(
      resolveToolRailTools(tools, settings, "logs").map((tool) => tool.id),
    ).toContain("logs");
    expect(
      resolveToolRailSections(tools, settings, "logs").bottom.map(
        (tool) => tool.id,
      ),
    ).toEqual(["logs"]);
  });

  /** 主区与底部区共用顺序规则，新入口追加到主区末尾。 */
  it("按成员关系把任意工具放入底部且保持各分区顺序", () => {
    const sections = resolveToolRailSections(tools, {
      ...defaultToolRailSettings,
      bottom: ["system", "logs"],
      order: [
        "system",
        "context",
        "logs",
        "agentLauncher",
        "sftp",
        "snippets",
        "tmux",
        "ports",
      ],
    });

    expect(sections.main.map((tool) => tool.id)).toEqual([
      "context",
      "agentLauncher",
      "sftp",
      "snippets",
      "tmux",
      "ports",
      "containers",
    ]);
    expect(sections.bottom.map((tool) => tool.id)).toEqual(["system", "logs"]);
  });
});
