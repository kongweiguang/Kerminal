// @author kongweiguang

import {
  Bot,
  Box,
  Cpu,
  FileText,
  FolderOpen,
  History,
  Network,
  PanelsTopLeft,
  ScanSearch,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import type { ToolId, ToolSummary } from "../workspace/contracts/index";

/**
 * 右栏真正可以放入工具栏的稳定 id；设置页和内部保留工具不暴露在这里，
 * 避免用户排序时改变工具内容的生命周期或把不可渲染的入口放进 rail。
 */
const toolRailToolIds = [
  "context",
  "agentLauncher",
  "sftp",
  "snippets",
  "tmux",
  "ports",
  "system",
  "logs",
  "containers",
] as const satisfies readonly ToolId[];

export type ToolRailToolId = (typeof toolRailToolIds)[number];

const toolRailPanelPlacements = [
  "attached",
  "left",
  "bottom",
  "center",
] as const;

export type ToolRailPanelPlacement =
  (typeof toolRailPanelPlacements)[number];

export type ToolRailPanelPlacements = Record<
  ToolRailToolId,
  ToolRailPanelPlacement
>;

export type ResolvedOpenToolPanels = Partial<
  Record<ToolRailPanelPlacement, ToolId>
>;

export interface ToolRailSettings {
  order: ToolRailToolId[];
  hidden: ToolRailToolId[];
  bottom: ToolRailToolId[];
  panelPlacements: ToolRailPanelPlacements;
}

export type ToolRailSettingsInput = Omit<
  Partial<ToolRailSettings>,
  "panelPlacements"
> & {
  /** 早期单一下拉框字段只用于无损迁移，不再作为新的保存格式。 */
  panelPlacement?: unknown;
  panelPlacements?: Partial<Record<ToolRailToolId, unknown>>;
};

export interface ToolRailDefinition extends ToolSummary {
  Icon: LucideIcon;
}

/** 默认顺序保持现有右栏的用户认知，新增工具会在归一化时追加到末尾。 */
const defaultToolRailPanelPlacements: ToolRailPanelPlacements =
  Object.fromEntries(
    toolRailToolIds.map((toolId) => [toolId, "attached"] as const),
  ) as ToolRailPanelPlacements;

export const defaultToolRailSettings: ToolRailSettings = {
  bottom: ["logs"],
  hidden: [],
  order: [...toolRailToolIds],
  panelPlacements: { ...defaultToolRailPanelPlacements },
};

export const toolRailDefinitions: readonly ToolRailDefinition[] = [
  {
    description: "当前目标、终端、目录和运行状态",
    Icon: ScanSearch,
    id: "context",
    title: "当前上下文",
  },
  {
    description: "Codex、Claude、自定义 Agent",
    Icon: Bot,
    id: "agentLauncher",
    title: "Agent Launcher",
  },
  {
    description: "SSH/SFTP 与容器文件浏览",
    Icon: FolderOpen,
    id: "sftp",
    title: "文件",
  },
  {
    description: "可复用脚本索引",
    Icon: FileText,
    id: "snippets",
    title: "片段",
  },
  {
    description: "session、window 和 pane 管理",
    Icon: PanelsTopLeft,
    id: "tmux",
    title: "tmux",
  },
  {
    description: "SSH 端口转发",
    Icon: Network,
    id: "ports",
    title: "端口",
  },
  {
    description: "CPU、内存、网络和磁盘状态",
    Icon: Cpu,
    id: "system",
    title: "系统",
  },
  {
    description: "当前终端的最近命令",
    Icon: History,
    id: "logs",
    title: "命令历史",
  },
  {
    description: "当前 SSH 主机的 Docker、Podman 和 Compose 容器",
    Icon: Box,
    id: "containers",
    title: "容器",
  },
];

const toolRailDefinitionById = new Map(
  toolRailDefinitions.map((definition) => [definition.id, definition]),
);

/** 判断外部设置或运行时工具 id 是否属于可配置的右栏工具。 */
export function isToolRailToolId(value: unknown): value is ToolRailToolId {
  return (
    typeof value === "string" &&
    (toolRailToolIds as readonly string[]).includes(value)
  );
}

/** 只接受稳定的面板展示模式，避免损坏的外部配置改变 Shell 布局。 */
function isToolRailPanelPlacement(
  value: unknown,
): value is ToolRailPanelPlacement {
  return (
    typeof value === "string" &&
    (toolRailPanelPlacements as readonly string[]).includes(value)
  );
}

/** 返回工具的稳定显示定义；未知 id 不应该进入 rail 渲染。 */
export function toolRailDefinitionFor(
  toolId: ToolId | string,
): ToolRailDefinition | undefined {
  return toolRailDefinitionById.get(toolId as ToolRailToolId);
}

/**
 * 将文件配置、旧版本 payload 和浏览器预览数据收敛成完整的右栏偏好。
 * 顺序缺项自动补齐，隐藏全部时恢复第一项；旧的单一 panelPlacement 仅在
 * 缺少逐工具映射时扩展到全部工具，升级后不会静默丢失用户原来的选择。
 */
export function normalizeToolRailSettings(
  settings?: ToolRailSettingsInput | null,
): ToolRailSettings {
  const order: ToolRailToolId[] = [];
  const requestedOrder = Array.isArray(settings?.order) ? settings.order : [];
  for (const toolId of requestedOrder) {
    if (isToolRailToolId(toolId) && !order.includes(toolId)) {
      order.push(toolId);
    }
  }
  for (const toolId of toolRailToolIds) {
    if (!order.includes(toolId)) {
      order.push(toolId);
    }
  }

  const hidden: ToolRailToolId[] = [];
  const requestedHidden = Array.isArray(settings?.hidden) ? settings.hidden : [];
  for (const toolId of requestedHidden) {
    if (isToolRailToolId(toolId) && !hidden.includes(toolId)) {
      hidden.push(toolId);
    }
  }

  const requestedBottom = Array.isArray(settings?.bottom)
    ? settings.bottom
    : defaultToolRailSettings.bottom;
  const requestedBottomIds = new Set(
    requestedBottom.filter(isToolRailToolId),
  );
  // bottom 只表达成员关系；按全局 order 输出可消除等价配置的虚假 dirty 状态。
  const bottom = order.filter((toolId) => requestedBottomIds.has(toolId));
  const requestedPanelPlacements =
    settings?.panelPlacements && typeof settings.panelPlacements === "object"
      ? settings.panelPlacements
      : undefined;
  const legacyPanelPlacement =
    requestedPanelPlacements === undefined &&
    isToolRailPanelPlacement(settings?.panelPlacement)
      ? settings.panelPlacement
      : undefined;
  const panelPlacements = Object.fromEntries(
    toolRailToolIds.map((toolId) => {
      const requestedPlacement = requestedPanelPlacements?.[toolId];
      return [
        toolId,
        isToolRailPanelPlacement(requestedPlacement)
          ? requestedPlacement
          : (legacyPanelPlacement ?? defaultToolRailPanelPlacements[toolId]),
      ];
    }),
  ) as ToolRailPanelPlacements;
  if (hidden.length >= order.length) {
    // 全部隐藏时优先恢复当前排序第一项，保证自定义排序不会被默认数组位置反转。
    const firstOrderedTool = order[0];
    return {
      bottom,
      hidden: hidden.filter((toolId) => toolId !== firstOrderedTool),
      order,
      panelPlacements,
    };
  }

  return { bottom, hidden, order, panelPlacements };
}

/**
 * 当前工具独立决定内容宿主；设置和 containers 等非定制工具保持贴靠模式，
 * 防止内部流程被一个不存在的目录项改变布局。
 */
export function resolveToolRailPanelPlacement(
  settings: ToolRailSettingsInput | null | undefined,
  toolId: ToolId | null | undefined,
): ToolRailPanelPlacement {
  if (!toolId || !isToolRailToolId(toolId)) {
    return "attached";
  }
  return normalizeToolRailSettings(settings).panelPlacements[toolId];
}

/**
 * 使用最新逐工具设置把有序打开集合收敛为每个方向最多一个工具；后出现的工具
 * 代表最近操作并替换同方向旧项，不同方向保持并行。非 rail 内部工具不参与投影。
 */
export function normalizeOpenToolPanels(
  openTools: readonly ToolId[],
  settings?: ToolRailSettingsInput | null,
): ToolRailToolId[] {
  const normalizedSettings = normalizeToolRailSettings(settings);
  const normalized: ToolRailToolId[] = [];
  for (const toolId of openTools) {
    if (!isToolRailToolId(toolId)) {
      continue;
    }
    const placement = normalizedSettings.panelPlacements[toolId];
    const conflictingIndex = normalized.findIndex(
      (candidate) =>
        normalizedSettings.panelPlacements[candidate] === placement,
    );
    if (conflictingIndex >= 0) {
      normalized.splice(conflictingIndex, 1);
    }
    const duplicateIndex = normalized.indexOf(toolId);
    if (duplicateIndex >= 0) {
      normalized.splice(duplicateIndex, 1);
    }
    normalized.push(toolId);
  }
  return normalized;
}

/** 为布局层生成稳定的四方向槽位，避免各宿主分别决定冲突优先级。 */
export function resolveOpenToolPanels(
  openTools: readonly ToolId[],
  settings?: ToolRailSettingsInput | null,
): ResolvedOpenToolPanels {
  const normalizedSettings = normalizeToolRailSettings(settings);
  return Object.fromEntries(
    normalizeOpenToolPanels(openTools, normalizedSettings).map((toolId) => [
      normalizedSettings.panelPlacements[toolId],
      toolId,
    ]),
  ) as ResolvedOpenToolPanels;
}

/**
 * 打开工具时只替换相同方向的现有项，并把目标移到末尾作为最近活动工具；
 * 紧凑布局没有方向空间，因此由调用方传入 singlePanel 收敛成传统单抽屉。
 */
export function openToolPanel(
  openTools: readonly ToolId[],
  toolId: ToolRailToolId,
  settings?: ToolRailSettingsInput | null,
  singlePanel = false,
): ToolRailToolId[] {
  if (singlePanel) {
    return [toolId];
  }
  return normalizeOpenToolPanels([...openTools, toolId], settings);
}

export interface ResolvedToolRailSections {
  main: ToolSummary[];
  bottom: ToolSummary[];
}

/**
 * 将可见入口拆成可滚动主区和稳定贴底区；活动的隐藏工具仍回到自己的分区，
 * 既保留临时收起入口，也不让快捷键打开动作破坏用户排序。
 */
export function resolveToolRailSections(
  tools: readonly ToolSummary[],
  settings?: ToolRailSettingsInput | null,
  activeTools?: ToolId | readonly ToolId[] | null,
): ResolvedToolRailSections {
  const normalized = normalizeToolRailSettings(settings);
  const activeToolIds = new Set(
    Array.isArray(activeTools)
      ? activeTools
      : activeTools
        ? [activeTools as ToolId]
        : [],
  );
  const available = new Map(
    tools
      .filter((tool) => isToolRailToolId(tool.id))
      .map((tool) => [tool.id, tool]),
  );
  const visibleTools = normalized.order.flatMap((toolId) => {
    const tool = available.get(toolId);
    if (!tool) {
      return [];
    }
    if (!normalized.hidden.includes(toolId) || activeToolIds.has(toolId)) {
      return [tool];
    }
    return [];
  });
  const bottomIds = new Set(normalized.bottom);
  return {
    main: visibleTools.filter((tool) => !bottomIds.has(tool.id as ToolRailToolId)),
    bottom: visibleTools.filter((tool) => bottomIds.has(tool.id as ToolRailToolId)),
  };
}

/**
 * 根据全局偏好和当前活动工具解析实际 rail；隐藏工具被快捷键打开时临时保留，
 * 这样用户仍能看到选中态并再次点击收起，而内容组件本身不会被卸载。
 */
export function resolveToolRailTools(
  tools: readonly ToolSummary[],
  settings?: ToolRailSettingsInput | null,
  activeTools?: ToolId | readonly ToolId[] | null,
): ToolSummary[] {
  const sections = resolveToolRailSections(tools, settings, activeTools);
  return [...sections.main, ...sections.bottom];
}
