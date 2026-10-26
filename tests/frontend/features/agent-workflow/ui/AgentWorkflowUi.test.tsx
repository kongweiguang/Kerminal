// @author kongweiguang
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import {
  AgentWorkflowHistoryList,
  AgentWorkflowSendPreviewPanel,
  AgentWorkflowStatusBadge,
} from "../../../../../src/features/agent-workflow/ui";

describe("Agent Workflow production UI", () => {
  it("渲染可访问状态 badge", () => {
    render(<AgentWorkflowStatusBadge status="failed" />);
    expect(
      screen.getByRole("status", { name: "Agent 状态：失败" }),
    ).toHaveAttribute("data-status", "failed");
  });

  it("只在用户明确确认后调用发送 callback", async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const onConfirm = vi.fn();
    render(
      <AgentWorkflowSendPreviewPanel
        now={() => new Date("2026-07-11T00:00:30.000Z")}
        onCancel={onCancel}
        onConfirm={onConfirm}
        preview={{
          byteLength: 18,
          createdAt: "2026-07-11T00:00:00.000Z",
          expiresAt: "2026-07-11T00:01:00.000Z",
          id: "preview-1",
          kind: "artifact",
          redacted: false,
          sessionId: "session-1",
          text: "仅在预览中显示",
          truncated: false,
        }}
      />,
    );

    expect(onConfirm).not.toHaveBeenCalled();
    expect(screen.getByLabelText("待发送正文")).toHaveTextContent(
      "仅在预览中显示",
    );
    await user.click(screen.getByRole("button", { name: "确认发送" }));
    expect(onConfirm).toHaveBeenCalledWith("preview-1");
    expect(onCancel).not.toHaveBeenCalled();
  });

  it("过期 preview 禁止确认但允许取消", async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const onConfirm = vi.fn();
    render(
      <AgentWorkflowSendPreviewPanel
        now={() => new Date("2026-07-11T00:02:00.000Z")}
        onCancel={onCancel}
        onConfirm={onConfirm}
        preview={{
          byteLength: 4,
          createdAt: "2026-07-11T00:00:00.000Z",
          expiresAt: "2026-07-11T00:01:00.000Z",
          id: "preview-expired",
          kind: "diagnostic",
          redacted: true,
          sessionId: "session-1",
          text: "安全",
          truncated: false,
        }}
      />,
    );

    expect(screen.getByRole("button", { name: "确认发送" })).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent("预览已过期");
    await user.click(screen.getByRole("button", { name: "取消" }));
    expect(onCancel).toHaveBeenCalledWith("preview-expired");
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("历史列表只展示 metadata", () => {
    render(
      <AgentWorkflowHistoryList
        items={[
          {
            action: "selection",
            createdAt: "2026-07-11T00:00:00.000Z",
            id: "history-1",
            outcome: "queued",
            sessionId: "session-1",
            submit: false,
            textBytes: 24,
          },
        ]}
      />,
    );

    expect(
      screen.getByRole("list", { name: "Agent 操作历史" }),
    ).toHaveTextContent("终端选区");
    expect(screen.getByText("24 B")).toBeVisible();
    expect(screen.queryByLabelText("待发送正文")).not.toBeInTheDocument();
  });

});
