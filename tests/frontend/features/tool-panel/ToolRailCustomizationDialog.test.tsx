// @author kongweiguang

import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ToolRailCustomizationDialog } from "../../../../src/features/tool-panel/ToolRailCustomizationDialog";
import {
  defaultToolRailSettings,
  type ToolRailSettings,
} from "../../../../src/features/tool-panel/toolRailModel";

/** 用真实弹框壳渲染编辑器，测试只替换持久化边界而保留主题化控件行为。 */
function renderDialog(
  overrides: Partial<{
    onClose: () => void;
    onSave: (settings: ToolRailSettings) => Promise<void>;
    settings: ToolRailSettings;
  }> = {},
) {
  return render(
    <ToolRailCustomizationDialog
      onClose={overrides.onClose ?? vi.fn()}
      onSave={overrides.onSave ?? vi.fn().mockResolvedValue(undefined)}
      open
      settings={overrides.settings ?? defaultToolRailSettings}
    />,
  );
}

describe("ToolRailCustomizationDialog", () => {
  /** 编辑器须读取归一化后的完整工具目录，避免 UI 数量与设置类型脱节。 */
  it("通过开关和上下按钮编辑草稿，保存后才提交", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    const onClose = vi.fn();
    renderDialog({ onClose, onSave });

    expect(screen.getAllByRole("combobox")).toHaveLength(9);

    await user.click(screen.getByRole("switch", { name: "隐藏 当前上下文" }));
    await user.click(screen.getByRole("button", { name: "上移 Agent Launcher" }));

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByText("已显示 8 / 9 个工具")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    const savedSettings = onSave.mock.calls[0]?.[0];
    expect(savedSettings?.hidden).toEqual(["context"]);
    expect(savedSettings?.order.slice(0, 2)).toEqual(["agentLauncher", "context"]);
    expect(savedSettings?.bottom).toEqual(["logs"]);
    expect(savedSettings?.panelPlacements.context).toBe("attached");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("可为单个工具选择左侧栏并把任意工具固定到底部", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    renderDialog({ onSave });

    const placementSelect = screen.getByRole("combobox", {
      name: "打开位置 当前上下文",
    });
    await user.click(placementSelect);
    const placementListbox = screen.getByRole("listbox");
    expect(placementSelect.tagName).toBe("BUTTON");
    expect(placementListbox).toHaveClass("kerminal-floating-surface");
    expect(placementListbox).toHaveAttribute("data-side", "bottom");
    expect(screen.getByRole("option", { name: "自由浮窗" })).toBeVisible();
    expect(screen.getByRole("option", { name: "底部面板" })).toBeVisible();
    await user.click(screen.getByRole("option", { name: "左侧栏" }));
    expect(placementSelect).toHaveAttribute("data-value", "left");
    await user.click(
      screen.getByRole("button", { name: "固定到底部 当前上下文" }),
    );
    expect(
      within(
        screen.getByRole("region", { name: "底部固定区" }),
      ).getByText("当前上下文"),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave.mock.calls[0]?.[0]).toMatchObject({
      bottom: ["logs", "context"],
      panelPlacements: {
        context: "left",
        logs: "attached",
      },
    });
  });

  it("可为单个工具选择底部面板且不改变其它工具位置", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    renderDialog({ onSave });

    const placementSelect = screen.getByRole("combobox", {
      name: "打开位置 系统",
    });
    await user.click(placementSelect);
    await user.click(screen.getByRole("option", { name: "底部面板" }));
    expect(placementSelect).toHaveAttribute("data-value", "bottom");

    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave.mock.calls[0]?.[0].panelPlacements).toMatchObject({
      context: "attached",
      logs: "attached",
      system: "bottom",
    });
  });

  it("恢复默认会恢复历史底部和贴靠位置", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    renderDialog({
      onSave,
      settings: {
        ...defaultToolRailSettings,
        bottom: ["context"],
        panelPlacements: {
          ...defaultToolRailSettings.panelPlacements,
          context: "center",
        },
      },
    });

    await user.click(screen.getByRole("button", { name: "恢复默认" }));
    expect(
      screen.getByRole("combobox", { name: "打开位置 当前上下文" }),
    ).toHaveAttribute("data-value", "attached");
    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave.mock.calls[0]?.[0]).toEqual(defaultToolRailSettings);
  });

  it("底部固定工具的主题下拉向上展开，避免被滚动边界裁切", async () => {
    const user = userEvent.setup();
    renderDialog();

    const placementSelect = screen.getByRole("combobox", {
      name: "打开位置 命令历史",
    });
    await user.click(placementSelect);

    const listbox = screen.getByRole("listbox");
    expect(listbox).toHaveClass(
      "kerminal-floating-surface",
      "kerminal-layer-popover",
    );
    expect(listbox).toHaveAttribute("data-side", "top");
    expect(
      within(listbox).getByRole("option", { name: "贴靠右栏" }),
    ).toHaveAttribute("aria-selected", "true");
  });

  it("取消会丢弃排序和隐藏草稿", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockResolvedValue(undefined);
    const onClose = vi.fn();
    renderDialog({ onClose, onSave });

    await user.click(screen.getByRole("switch", { name: "隐藏 当前上下文" }));
    await user.click(screen.getByRole("button", { name: "取消" }));

    expect(onSave).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("保存失败时保留弹窗和草稿", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockRejectedValue(new Error("设置文件只读"));
    const onClose = vi.fn();
    renderDialog({ onClose, onSave });

    await user.click(screen.getByRole("switch", { name: "隐藏 当前上下文" }));
    await user.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("设置文件只读");
    expect(screen.getByRole("dialog", { name: "自定义工具栏" })).toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("支持拖拽把工具移动到目标行之前", () => {
    const onSave = vi.fn().mockResolvedValue(undefined);
    renderDialog({ onSave });

    const source = screen.getByRole("button", { name: "拖动排序 命令历史" });
    const targetRow = document.querySelector('[data-tool-rail-item="context"]');
    expect(targetRow).toBeTruthy();

    const dataTransfer = {
      effectAllowed: "",
      setData: vi.fn(),
      dropEffect: "",
    } as unknown as DataTransfer;
    fireEvent.dragStart(source, { dataTransfer });
    fireEvent.drop(targetRow as HTMLElement, { dataTransfer });

    const rows = Array.from(
      document.querySelectorAll<HTMLElement>("[data-tool-rail-item]"),
    ).map((row) => row.dataset.toolRailItem);
    expect(rows[0]).toBe("logs");
    expect(
      document.querySelector('[data-tool-rail-editor-section="bottom"]'),
    ).not.toContainElement(
      document.querySelector('[data-tool-rail-item="logs"]'),
    );
  });
});
