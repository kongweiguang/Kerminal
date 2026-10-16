/**
 * SFTP 紧凑传输队列行的恢复交互测试。
 *
 * @author kongweiguang
 */

import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SftpTransferQueueRow } from "../../../../src/features/sftp/SftpTransferQueueRow";
import { createSftpTransferSummary } from "../../support/sftp/SftpToolContent.testSupport";

describe("SftpTransferQueueRow", () => {
  /** 服务端拒绝的具体操作必须默认可见，展开详情也不能重复输出同一条错误。 */
  it("shows the concrete permission failure without expanding details", async () => {
    const message = "创建上传临时文件（OPEN）失败: Permission denied: Permission denied";
    render(<SftpTransferQueueRow
      onCancel={vi.fn()}
      onRetry={vi.fn()}
      transfer={createSftpTransferSummary({
        error: message,
        failureKind: "other",
        retryable: false,
        status: "failed",
      })}
    />);

    expect(screen.getByRole("alert")).toHaveTextContent(message);
    expect(screen.getByRole("alert")).toBeVisible();
    expect(screen.queryByRole("button", { name: /继续传输|重新传输/ })).not.toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole("button", { name: /查看传输详情/ }));
    expect(screen.getAllByText(message)).toHaveLength(1);
  });

  /** 普通失败沿用后端的重试资格，显示错误后也只提供一个可执行入口。 */
  it("keeps one inline retry action for a retryable ordinary failure", async () => {
    const onRetry = vi.fn();
    const transfer = createSftpTransferSummary({
      error: "打开上传断点文件（OPEN）失败: connection closed",
      failureKind: "other",
      resumable: true,
      retryable: true,
      status: "failed",
    });
    render(<SftpTransferQueueRow onCancel={vi.fn()} onRetry={onRetry} transfer={transfer} />);
    const actions = screen.getAllByRole("button", { name: /继续传输/ });
    expect(actions).toHaveLength(1);
    await userEvent.setup().click(actions[0]);
    expect(onRetry).toHaveBeenCalledWith(transfer);
  });

  /** 没有底层错误时承认信息缺失，不能留下一个只有失败 badge 的空状态。 */
  it("shows an explicit fallback when the failed task has no error details", () => {
    render(<SftpTransferQueueRow
      onCancel={vi.fn()}
      transfer={createSftpTransferSummary({ error: null, status: "failed" })}
    />);
    expect(screen.getByRole("alert")).toHaveTextContent("传输失败，未收到详细错误信息。");
  });

  it("keeps the idle-timeout recovery action singular and exposes its stable message", async () => {
    const user = userEvent.setup();
    const onRetry = vi.fn();
    const transfer = createSftpTransferSummary({
      bytesTransferred: 512,
      failureKind: "idleTimeout",
      idleTimeoutSeconds: 180,
      id: "long-transfer",
      localPath: "C:/Users/example/Downloads/a-very-long-artifact-name.tar.zst",
      remotePath: "/srv/releases/a-very-long-artifact-name.tar.zst",
      resumable: true,
      status: "failed",
      totalBytes: 1024,
    });

    render(
      <SftpTransferQueueRow
        onCancel={vi.fn()}
        onRetry={onRetry}
        transfer={transfer}
      />,
    );

    expect(
      screen.getByRole("button", { name: /继续传输.*a-very-long-artifact-name/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("网络连续 3 分钟无响应"),
    ).toBeVisible();
    expect(screen.getByText("a-very-long-artifact-name.tar.zst"))
      .toHaveAttribute("title", "a-very-long-artifact-name.tar.zst");
    expect(
      screen.getAllByRole("button", { name: /继续传输.*a-very-long-artifact-name/ }),
    ).toHaveLength(1);
    expect(screen.queryByRole("button", { name: /重试传输/ })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /查看传输详情/ }));
    expect(screen.getByText("网络连续 3 分钟无响应")).toBeVisible();

    await user.click(
      screen.getByRole("button", { name: /继续传输.*a-very-long-artifact-name/ }),
    );
    expect(onRetry).toHaveBeenCalledWith(transfer);
  });

  it("keeps terminal canceled text after a late cancel phase", () => {
    const transfer = createSftpTransferSummary({
      cancelRequested: true,
      phase: "canceling",
      status: "canceled",
    });

    render(
      <SftpTransferQueueRow
        onCancel={vi.fn()}
        onRetry={vi.fn()}
        transfer={transfer}
      />,
    );

    expect(screen.getByLabelText("已取消")).toBeVisible();
    expect(screen.queryByText("正在取消")).not.toBeInTheDocument();
  });

  it("shows recovery as the real lifecycle phase and keeps a compact narrow label", () => {
    const transfer = createSftpTransferSummary({
      phase: "recovering",
      recoveryAttempt: 1,
      status: "running",
    });

    render(
      <SftpTransferQueueRow
        onCancel={vi.fn()}
        transfer={transfer}
      />,
    );

    const status = screen.getByLabelText("正在恢复连接（1/1）");
    expect(status).toHaveAttribute("title", "正在恢复连接（1/1）");
    expect(status).toHaveTextContent("正在恢复连接（1/1）");
  });

  it("closes the old retry action when a successor task exists", async () => {
    const transfer = createSftpTransferSummary({
      failureKind: "idleTimeout",
      idleTimeoutSeconds: 180,
      status: "failed",
      successorId: "successor-transfer",
    });

    render(
      <SftpTransferQueueRow
        onCancel={vi.fn()}
        onRetry={vi.fn()}
        transfer={transfer}
      />,
    );

    expect(screen.getByLabelText("已重新排队")).toBeVisible();
    await userEvent.setup().click(
      screen.getByRole("button", { name: /查看传输详情/ }),
    );
    expect(screen.getByText(/已重新排队，后继任务正在队列中执行/)).toBeVisible();
    expect(screen.queryByRole("button", { name: /继续传输/ })).not.toBeInTheDocument();
  });

  /** 提交结果不明时把核对提示放在主行，并阻止 UI 诱导重复上传。 */
  it("shows uncertain commit without a retry action", () => {
    render(<SftpTransferQueueRow
      onCancel={vi.fn()}
      onRetry={vi.fn()}
      transfer={createSftpTransferSummary({
        failureKind: "commitUnknown",
        retryable: false,
        status: "failed",
      })}
    />);

    expect(screen.getByRole("alert")).toHaveTextContent(
      "提交结果未确认，请核对目标文件",
    );
    expect(screen.queryByRole("button", { name: /继续传输|重新传输/ }))
      .not.toBeInTheDocument();
  });
});
