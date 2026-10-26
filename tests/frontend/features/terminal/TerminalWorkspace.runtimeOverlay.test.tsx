// @author kongweiguang

import { useState, type ReactNode } from "react";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { movePaneInLayout } from "../../../../src/features/workspace/workspaceLayout";
import type { TerminalPane, TerminalTab } from "../../../../src/features/workspace/types";
import { mocks } from "../../support/terminal/XtermPane.testSupport.tsx";
import { TerminalWorkspace } from "../../../../src/features/terminal/TerminalWorkspace";
import {
  baseTerminalPane,
  workspaceProps,
} from "../../support/terminal/TerminalWorkspace.testSupport.ts";

const resizableMockState = vi.hoisted(() => ({
  groups: [] as Array<{
    defaultLayout?: Record<string, number>;
    id?: string;
    onLayoutChanged?: (layout: Record<string, number>) => void;
  }>,
}));

vi.mock("../../../../src/components/ui/resizable", () => ({
  ResizableHandle: ({ "aria-label": ariaLabel }: { "aria-label"?: string }) => (
    <div aria-label={ariaLabel} role="separator" />
  ),
  ResizablePanel: ({ children }: { children: ReactNode }) => (
    <div>{children}</div>
  ),
  ResizablePanelGroup: ({
    children,
    defaultLayout,
    id,
    onLayoutChanged,
  }: {
    children: ReactNode;
    defaultLayout?: Record<string, number>;
    id?: string;
    onLayoutChanged?: (layout: Record<string, number>) => void;
  }) => (
    resizableMockState.groups.push({ defaultLayout, id, onLayoutChanged }),
    <div>{children}</div>
  ),
}));

const runtimePanes: TerminalPane[] = [
  {
    ...baseTerminalPane,
    id: "pane-runtime-left",
    title: "左侧 runtime",
  },
  {
    ...baseTerminalPane,
    id: "pane-runtime-right",
    title: "右侧 runtime",
  },
];

const runtimeTab: TerminalTab = {
  id: "tab-runtime",
  layout: {
    children: [
      { paneId: "pane-runtime-left", type: "pane" },
      { paneId: "pane-runtime-right", type: "pane" },
    ],
    direction: "horizontal",
    id: "split-runtime",
    type: "split",
  },
  machineId: "local-powershell",
  title: "runtime 分屏",
};

const hiddenRuntimePane: TerminalPane = {
  ...baseTerminalPane,
  id: "pane-hidden-remote",
  title: "隐藏 172.16.41.60",
};

const hiddenRuntimeTab: TerminalTab = {
  id: "tab-hidden-runtime",
  layout: {
    paneId: "pane-hidden-remote",
    type: "pane",
  },
  machineId: "hidden-remote",
  title: "隐藏远程",
};

describe("TerminalWorkspace runtime overlay", () => {
  it("closing one of multiple same-host tabs retires only its runtime and keeps the active session interactive", async () => {
    let sessionIndex = 0;
    resizableMockState.groups = [];
    mocks.api.createSshTerminalSession.mockImplementation(
      async (request, onOutput) => {
        sessionIndex += 1;
        const sessionId = `ssh-session-tab-${sessionIndex}`;
        onOutput({
          data: `hello from ${sessionId}`,
          kind: "data",
          sessionId,
        });
        return {
          cols: request.cols,
          id: sessionId,
          rows: request.rows,
          shell: "ssh",
          status: "running",
        };
      },
    );

    const firstPane: TerminalPane = {
      ...baseTerminalPane,
      id: "pane-tab-first",
      machineId: "host-shared",
      mode: "ssh",
      remoteHostId: "host-shared",
      title: "同主机 SSH 一",
    };
    const secondPane: TerminalPane = {
      ...baseTerminalPane,
      id: "pane-tab-second",
      machineId: "host-shared",
      mode: "ssh",
      remoteHostId: "host-shared",
      title: "同主机 SSH 二",
    };
    const firstTab: TerminalTab = {
      id: "tab-first",
      layout: { paneId: firstPane.id, type: "pane" },
      machineId: firstPane.machineId,
      title: firstPane.title,
    };
    const secondTab: TerminalTab = {
      id: "tab-second",
      layout: { paneId: secondPane.id, type: "pane" },
      machineId: secondPane.machineId,
      title: secondPane.title,
    };

    function ControlledWorkspace() {
      const [panes, setPanes] = useState<TerminalPane[]>([
        firstPane,
        secondPane,
      ]);
      const [tabs, setTabs] = useState<TerminalTab[]>([firstTab, secondTab]);

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: secondTab.id,
            focusedPaneId: secondPane.id,
            onCloseTabs: (tabIds) => {
              const tabId = tabIds[0];
              const closedTab = tabs.find((tab) => tab.id === tabId);
              const closedPaneId =
                closedTab && "layout" in closedTab
                  ? closedTab.layout.type === "pane"
                    ? closedTab.layout.paneId
                    : undefined
                  : undefined;
              setTabs((current) => current.filter((tab) => tab.id !== tabId));
              if (closedPaneId) {
                setPanes((current) =>
                  current.filter((pane) => pane.id !== closedPaneId),
                );
              }
            },
            panes,
            tabs,
            terminalAppearance: {
              ...workspaceProps().terminalAppearance,
              confirmCloseTab: false,
            },
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(2);
    });
    mocks.api.createSshTerminalSession.mockClear();
    mocks.api.closeTerminal.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "关闭 同主机 SSH 一 tab" }));

    await waitFor(() => {
      expect(mocks.api.closeTerminal).toHaveBeenCalledTimes(1);
    });
    expect(mocks.api.closeTerminal).toHaveBeenCalledWith("ssh-session-tab-1");
    expect(mocks.api.createSshTerminalSession).not.toHaveBeenCalled();
    expect(mocks.api.closeTerminal).not.toHaveBeenCalledWith("ssh-session-tab-2");
    expect(
      document.querySelector('[data-terminal-pane-runtime-host="pane-tab-second"]'),
    ).not.toBeNull();

    mocks.terminalInstances[1].onDataCallback?.("echo surviving-tab\r");
    await waitFor(() => {
      expect(mocks.api.writeTerminal).toHaveBeenCalledWith(
        "ssh-session-tab-2",
        "echo surviving-tab\r",
      );
    });
  });

  it("keeps real XtermPane sessions alive when pane move only changes layout", async () => {
    await expectRuntimeSessionsSurvivePaneMove({
      expectedIndicatorText: "交换位置 · 右侧 runtime",
      pointerUp: { clientX: 620, clientY: 150 },
    });
  });

  it("keeps real XtermPane sessions alive when pane move changes split structure", async () => {
    await expectRuntimeSessionsSurvivePaneMove({
      expectedIndicatorText: "停靠到右侧整列",
      pointerUp: { clientX: 790, clientY: 150 },
    });
  });

  it("adds a split session without reconnecting the source SSH pane", async () => {
    let sessionIndex = 0;
    resizableMockState.groups = [];
    mocks.api.createSshTerminalSession.mockImplementation(
      async (request, onOutput) => {
        sessionIndex += 1;
        const sessionId = `ssh-session-split-${sessionIndex}`;
        onOutput({
          data: `hello from ${sessionId}`,
          kind: "data",
          sessionId,
        });
        return {
          cols: request.cols,
          id: sessionId,
          rows: request.rows,
          shell: "ssh",
          status: "running",
        };
      },
    );

    const sourcePane: TerminalPane = {
      ...baseTerminalPane,
      currentCwd: "/dev",
      cwd: "/bwy",
      id: "pane-ssh-source",
      machineId: "host-prod",
      mode: "ssh",
      prompt: "deploy@prod.internal:~$",
      remoteHostId: "host-prod",
      title: "源 SSH",
    };
    const sourceTab: TerminalTab = {
      id: "tab-ssh-source",
      layout: { paneId: sourcePane.id, type: "pane" },
      machineId: "host-prod",
      title: "源 SSH",
    };

    function ControlledWorkspace() {
      const [focusedPaneId, setFocusedPaneId] = useState(sourcePane.id);
      const [panes, setPanes] = useState<TerminalPane[]>([sourcePane]);
      const [tabs, setTabs] = useState<TerminalTab[]>([sourceTab]);

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: sourceTab.id,
            focusedPaneId,
            onFocusPane: setFocusedPaneId,
            onSplitPane: (direction) => {
              const inheritedCwd = sourcePane.currentCwd ?? sourcePane.cwd;
              const splitPane: TerminalPane = {
                ...sourcePane,
                currentCwd: inheritedCwd,
                cwd: inheritedCwd,
                id: "pane-ssh-split",
                lines: [],
                outputHistory: undefined,
                title: direction === "horizontal" ? "右侧分屏" : "下方分屏",
              };
              setPanes((currentPanes) =>
                currentPanes.some((pane) => pane.id === splitPane.id)
                  ? currentPanes
                  : [...currentPanes, splitPane],
              );
              setTabs((currentTabs) =>
                currentTabs.map((tab) =>
                  tab.id === sourceTab.id && "layout" in tab
                    ? {
                        ...tab,
                        layout: {
                          children: [
                            { paneId: sourcePane.id, type: "pane" },
                            { paneId: splitPane.id, type: "pane" },
                          ],
                          direction,
                          id: "split-added",
                          type: "split",
                        },
                      }
                    : tab,
                ),
              );
              setFocusedPaneId(splitPane.id);
            },
            panes,
            tabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });
    mocks.api.createSshTerminalSession.mockClear();
    mocks.api.closeTerminal.mockClear();

    fireEvent.click(
      screen.getByRole("button", { name: "源 SSH 左右分屏" }),
    );

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(1);
    });
    expect(mocks.api.closeTerminal).not.toHaveBeenCalled();
    expect(mocks.api.createSshTerminalSession).toHaveBeenCalledWith(
      expect.objectContaining({
        cwd: "/dev",
        hostId: "host-prod",
      }),
      expect.any(Function),
    );
    expect(
      Array.from(
        document.querySelectorAll<HTMLElement>("[data-terminal-pane-card]"),
      ).map((card) => card.dataset.terminalPaneCard),
    ).toEqual(["pane-ssh-source", "pane-ssh-split"]);
  });

  it("ignores hidden tab pane cards when resolving pane move targets", async () => {
    let sessionIndex = 0;
    resizableMockState.groups = [];
    mocks.api.createTerminalSession.mockImplementation(async (request, onOutput) => {
      sessionIndex += 1;
      const sessionId = `session-runtime-hidden-${sessionIndex}`;
      onOutput({
        data: `hello from ${sessionId}`,
        kind: "data",
        sessionId,
      });
      return {
        cols: request.cols,
        id: sessionId,
        rows: request.rows,
        shell: request.shell ?? "powershell.exe",
        status: "running",
      };
    });

    const movePane = vi.fn();

    function ControlledWorkspace() {
      const [focusedPaneId, setFocusedPaneId] = useState("pane-runtime-left");
      const [tabs, setTabs] = useState<TerminalTab[]>([
        hiddenRuntimeTab,
        runtimeTab,
      ]);

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-runtime",
            focusedPaneId,
            onFocusPane: setFocusedPaneId,
            onMovePane: (sourcePaneId, targetPaneId, placement, scope) => {
              movePane(sourcePaneId, targetPaneId, placement);
              setTabs((currentTabs) =>
                currentTabs.map((tab) =>
                  tab.id === runtimeTab.id && "layout" in tab
                    ? {
                        ...tab,
                        layout: movePaneInLayout(tab.layout, {
                          placement,
                          scope,
                          sourcePaneId,
                          splitId: "split-runtime-moved",
                          targetPaneId,
                        }),
                      }
                    : tab,
                ),
              );
              setFocusedPaneId(sourcePaneId);
            },
            panes: [...runtimePanes, hiddenRuntimePane],
            tabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await waitFor(() => {
      expect(mocks.api.createTerminalSession).toHaveBeenCalledTimes(3);
    });
    mocks.api.createTerminalSession.mockClear();
    mocks.api.closeTerminal.mockClear();

    const cards = Array.from(
      document.querySelectorAll<HTMLElement>("[data-terminal-pane-card]"),
    );
    const hiddenCard = cards.find(
      (card) => card.dataset.terminalPaneCard === "pane-hidden-remote",
    );
    const leftCard = cards.find(
      (card) => card.dataset.terminalPaneCard === "pane-runtime-left",
    );
    const rightCard = cards.find(
      (card) => card.dataset.terminalPaneCard === "pane-runtime-right",
    );

    expect(hiddenCard).toBeTruthy();
    expect(leftCard).toBeTruthy();
    expect(rightCard).toBeTruthy();

    const workspaceContent = document.querySelector<HTMLElement>(
      "[data-terminal-workspace-content]",
    );
    expect(workspaceContent).toBeTruthy();

    vi.spyOn(workspaceContent!, "getBoundingClientRect").mockReturnValue({
      bottom: 300,
      height: 300,
      left: 0,
      right: 800,
      top: 0,
      width: 800,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    });
    vi.spyOn(hiddenCard!, "getBoundingClientRect").mockReturnValue({
      bottom: 300,
      height: 300,
      left: 400,
      right: 800,
      top: 0,
      width: 400,
      x: 400,
      y: 0,
      toJSON: () => ({}),
    });
    vi.spyOn(leftCard!, "getBoundingClientRect").mockReturnValue({
      bottom: 300,
      height: 300,
      left: 0,
      right: 400,
      top: 0,
      width: 400,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    });
    vi.spyOn(rightCard!, "getBoundingClientRect").mockReturnValue({
      bottom: 300,
      height: 300,
      left: 400,
      right: 800,
      top: 0,
      width: 400,
      x: 400,
      y: 0,
      toJSON: () => ({}),
    });

    fireEvent.pointerDown(
      screen.getByRole("button", {
        name: "拖动 左侧 runtime 分屏调整位置",
      }),
      { clientX: 20, clientY: 20, pointerId: 23 },
    );
    await act(async () => {});
    fireEvent.pointerMove(window, { clientX: 620, clientY: 150, pointerId: 23 });

    expect(
      await screen.findByLabelText(
        "终端分屏移动目标：交换位置 · 右侧 runtime",
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByLabelText("正在拖动终端分屏：交换位置 · 右侧 runtime"),
    ).toBeInTheDocument();
    expect(
      screen.queryByLabelText(
        "终端分屏移动目标：交换位置 · 隐藏 172.16.41.60",
      ),
    ).toBeNull();

    fireEvent.pointerUp(window, { clientX: 620, clientY: 150, pointerId: 23 });

    expect(movePane).toHaveBeenCalledWith(
      "pane-runtime-left",
      "pane-runtime-right",
      "center",
    );
    expect(mocks.api.closeTerminal).not.toHaveBeenCalled();
    expect(mocks.api.createTerminalSession).not.toHaveBeenCalled();
  });

  it("keeps real XtermPane sessions alive when split resize only changes sizes", async () => {
    let sessionIndex = 0;
    resizableMockState.groups = [];
    mocks.api.createTerminalSession.mockImplementation(async (request, onOutput) => {
      sessionIndex += 1;
      const sessionId = `session-runtime-resize-${sessionIndex}`;
      onOutput({
        data: `hello from ${sessionId}`,
        kind: "data",
        sessionId,
      });
      return {
        cols: request.cols,
        id: sessionId,
        rows: request.rows,
        shell: request.shell ?? "powershell.exe",
        status: "running",
      };
    });

    function ControlledWorkspace() {
      const [tabs, setTabs] = useState<TerminalTab[]>([runtimeTab]);

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-runtime",
            focusedPaneId: "pane-runtime-left",
            onSplitLayoutSizesChange: (splitId, sizes) => {
              setTabs((currentTabs) =>
                currentTabs.map((tab) =>
                  tab.id === runtimeTab.id && "layout" in tab
                    ? {
                        ...tab,
                        layout:
                          tab.layout.type === "split" && tab.layout.id === splitId
                            ? { ...tab.layout, sizes }
                            : tab.layout,
                      }
                    : tab,
                ),
              );
            },
            panes: runtimePanes,
            tabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await waitFor(() => {
      expect(mocks.api.createTerminalSession).toHaveBeenCalledTimes(2);
    });
    mocks.api.createTerminalSession.mockClear();
    mocks.api.closeTerminal.mockClear();

    const rootGroup = resizableMockState.groups.find(
      (group) => group.id === "tab-runtime",
    );
    rootGroup?.onLayoutChanged?.({
      "pane-runtime-left": 35,
      "pane-runtime-right": 65,
    });

    await waitFor(() => {
      const latestGroup =
        resizableMockState.groups[resizableMockState.groups.length - 1];
      expect(
        latestGroup?.defaultLayout,
      ).toMatchObject({
        "pane-runtime-left": 35,
        "pane-runtime-right": 65,
      });
    });
    expect(mocks.api.closeTerminal).not.toHaveBeenCalled();
    expect(mocks.api.createTerminalSession).not.toHaveBeenCalled();
  });
});

async function expectRuntimeSessionsSurvivePaneMove({
  expectedIndicatorText,
  pointerUp,
}: {
  expectedIndicatorText: string;
  pointerUp: { clientX: number; clientY: number };
}) {
  resizableMockState.groups = [];
  let sessionIndex = 0;
  mocks.api.createTerminalSession.mockImplementation(async (request, onOutput) => {
    sessionIndex += 1;
    const sessionId = `session-runtime-${sessionIndex}`;
    onOutput({
      data: `hello from ${sessionId}`,
      kind: "data",
      sessionId,
    });
    return {
      cols: request.cols,
      id: sessionId,
      rows: request.rows,
      shell: request.shell ?? "powershell.exe",
      status: "running",
    };
  });

  function ControlledWorkspace() {
    const [focusedPaneId, setFocusedPaneId] = useState("pane-runtime-left");
    const [tabs, setTabs] = useState<TerminalTab[]>([runtimeTab]);

    return (
      <TerminalWorkspace
        {...workspaceProps({
          activeTabId: "tab-runtime",
          focusedPaneId,
          onFocusPane: setFocusedPaneId,
          onMovePane: (sourcePaneId, targetPaneId, placement, scope) => {
            setTabs((currentTabs) =>
              currentTabs.map((tab) =>
                tab.id === runtimeTab.id && "layout" in tab
                  ? {
                      ...tab,
                      layout: movePaneInLayout(tab.layout, {
                        placement,
                        scope,
                        sourcePaneId,
                        splitId: "split-runtime-moved",
                        targetPaneId,
                      }),
                    }
                  : tab,
              ),
            );
            setFocusedPaneId(sourcePaneId);
          },
          panes: runtimePanes,
          tabs,
        })}
      />
    );
  }

  render(<ControlledWorkspace />);

  await waitFor(() => {
    expect(mocks.api.createTerminalSession).toHaveBeenCalledTimes(2);
  });
  mocks.api.createTerminalSession.mockClear();
  mocks.api.closeTerminal.mockClear();

  const cards = Array.from(
    document.querySelectorAll<HTMLElement>("[data-terminal-pane-card]"),
  );
  const workspaceContent = document.querySelector<HTMLElement>(
    "[data-terminal-workspace-content]",
  );
  expect(workspaceContent).toBeTruthy();

  vi.spyOn(workspaceContent!, "getBoundingClientRect").mockReturnValue({
    bottom: 300,
    height: 300,
    left: 0,
    right: 800,
    top: 0,
    width: 800,
    x: 0,
    y: 0,
    toJSON: () => ({}),
  });
  vi.spyOn(cards[0], "getBoundingClientRect").mockReturnValue({
    bottom: 300,
    height: 300,
    left: 0,
    right: 400,
    top: 0,
    width: 400,
    x: 0,
    y: 0,
    toJSON: () => ({}),
  });
  vi.spyOn(cards[1], "getBoundingClientRect").mockReturnValue({
    bottom: 300,
    height: 300,
    left: 400,
    right: 800,
    top: 0,
    width: 400,
    x: 400,
    y: 0,
    toJSON: () => ({}),
  });

  fireEvent.pointerDown(
    screen.getByRole("button", {
      name: "拖动 左侧 runtime 分屏调整位置",
    }),
    { clientX: 20, clientY: 20, pointerId: 17 },
  );
  await act(async () => {});
  fireEvent.pointerMove(window, { ...pointerUp, pointerId: 17 });

  expect(
    await screen.findByLabelText(`终端分屏移动目标：${expectedIndicatorText}`),
  ).toBeInTheDocument();

  fireEvent.pointerUp(window, { ...pointerUp, pointerId: 17 });

  await waitFor(() => {
    expect(
      Array.from(
        document.querySelectorAll<HTMLElement>("[data-terminal-pane-card]"),
      ).map((card) => card.dataset.terminalPaneCard),
    ).toEqual(["pane-runtime-right", "pane-runtime-left"]);
  });
  expect(mocks.api.closeTerminal).not.toHaveBeenCalled();
  expect(mocks.api.createTerminalSession).not.toHaveBeenCalled();
}

