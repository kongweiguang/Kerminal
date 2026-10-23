/**
 * SFTP 后台传输展示模型测试。
 *
 * @author kongweiguang
 */

import { describe, expect, it } from "vitest";
import type { SftpTransferSummary } from "../../../../src/lib/sftpApi";
import { fileNameFromPath, formatFileSize } from "../../../../src/features/sftp/sftpFileUtils";
import {
  activeTransferCount,
  canCancelTransfer,
  canClearFinishedTransfers,
  formatTransferBytes,
  isTransferWaiting,
  isFinishedTransfer,
  mergeTransferSnapshot,
  replaceTransferQueue,
  sortTransfers,
  transferCompactStatusLabel,
  transferPathSummary,
  transferPercentLabel,
  transferProgressPercent,
  transferStatusLabel,
  transferStatusSummary,
  transferTitle,
  upsertTransfer,
} from "../../../../src/features/sftp/sftpTransferModel";

function transfer(
  overrides: Partial<SftpTransferSummary> = {},
): SftpTransferSummary {
  return {
    bytesTransferred: 0,
    cancelRequested: false,
    createdAt: 100,
    direction: "download",
    hostId: "host-1",
    id: "transfer-1",
    kind: "file",
    localPath: "C:\\Downloads\\archive.zip",
    operation: "download",
    remotePath: "/srv/archive.zip",
    source: {
      hostId: "host-1",
      hostLabel: "host-1",
      kind: "remote",
      path: "/srv/archive.zip",
    },
    status: "queued",
    target: {
      kind: "local",
      path: "C:\\Downloads\\archive.zip",
    },
    totalBytes: 100,
    transportMode: "singleHostSftp",
    updatedAt: 100,
    ...overrides,
  };
}

describe("sftpTransferModel", () => {
  it("sorts active transfers ahead of failed and completed items", () => {
    const sorted = sortTransfers([
      transfer({ createdAt: 1, id: "old-success", status: "succeeded" }),
      transfer({ createdAt: 3, id: "new-running", status: "running" }),
      transfer({ createdAt: 2, id: "failed", status: "failed" }),
      transfer({ createdAt: 4, id: "queued", status: "queued" }),
    ]);

    expect(sorted.map((item) => item.id)).toEqual([
      "new-running",
      "queued",
      "failed",
      "old-success",
    ]);
  });

  it("replaces an existing transfer snapshot by id", () => {
    const original = transfer({ bytesTransferred: 20, id: "same" });
    const replacement = transfer({
      bytesTransferred: 80,
      id: "same",
      status: "running",
    });

    expect(upsertTransfer([original], replacement)).toEqual([replacement]);
  });

  it("merges a transfer snapshot without mutating the existing queue", () => {
    const original = transfer({
      bytesTransferred: 20,
      createdAt: 3,
      id: "same",
      status: "queued",
    });
    const newerRunning = transfer({
      createdAt: 1,
      id: "newer-running",
      status: "running",
    });
    const replacement = transfer({
      bytesTransferred: 80,
      createdAt: 2,
      id: "same",
      status: "failed",
    });
    const queue = [original, newerRunning];

    const merged = mergeTransferSnapshot(queue, replacement);

    expect(queue).toEqual([original, newerRunning]);
    expect(merged.map((item) => item.id)).toEqual(["newer-running", "same"]);
    expect(merged[1]).toBe(replacement);
  });

  it("does not let a late active snapshot regress a terminal result", () => {
    const completed = transfer({
      id: "same",
      status: "canceled",
      cancelRequested: true,
      phase: "canceling",
      updatedAt: 10,
    });
    const lateRunning = transfer({
      id: "same",
      status: "running",
      cancelRequested: false,
      phase: "uploading",
      updatedAt: 11,
    });

    expect(mergeTransferSnapshot([completed], lateRunning)).toEqual([
      completed,
    ]);
  });

  it("keeps an optimistic cancel state until the backend reports a terminal result", () => {
    const canceling = transfer({
      id: "same",
      status: "running",
      cancelRequested: true,
      phase: "canceling",
      updatedAt: 20,
    });
    const lateQueued = transfer({
      id: "same",
      status: "queued",
      cancelRequested: false,
      updatedAt: 21,
    });

    expect(mergeTransferSnapshot([canceling], lateQueued)).toEqual([
      canceling,
    ]);
    expect(
      mergeTransferSnapshot(
        [canceling],
        transfer({
          id: "same",
          status: "canceled",
          cancelRequested: true,
          updatedAt: 22,
        }),
      )[0].status,
    ).toBe("canceled");
  });

  it("applies the same guard when replacing a polling list", () => {
    const canceling = transfer({
      id: "same",
      status: "running",
      cancelRequested: true,
      phase: "canceling",
      updatedAt: 20,
    });
    const lateList = transfer({
      id: "same",
      status: "running",
      cancelRequested: false,
      updatedAt: 21,
    });

    expect(replaceTransferQueue([lateList], [canceling])).toEqual([
      canceling,
    ]);
  });

  it("sorts replacement queue results without mutating the backend result", () => {
    const running = transfer({
      createdAt: 1,
      id: "running",
      status: "running",
    });
    const queued = transfer({
      createdAt: 5,
      id: "queued",
      status: "queued",
    });
    const backendQueue = [queued, running];

    const replaced = replaceTransferQueue(backendQueue);

    expect(backendQueue).toEqual([queued, running]);
    expect(replaced.map((item) => item.id)).toEqual(["running", "queued"]);
  });

  /** 字节计数可以到齐，但只有成功终态能表示远端提交完成。 */
  it("reserves 100 percent for committed transfers", () => {
    expect(
      transferProgressPercent(
        transfer({ bytesTransferred: 50, status: "running", totalBytes: 0 }),
      ),
    ).toBe(8);
    expect(
      transferProgressPercent(transfer({ bytesTransferred: 150, totalBytes: 100 })),
    ).toBe(99);
    const committingUpload = transfer({
      bytesTransferred: 100,
      direction: "upload",
      phase: "committing",
      status: "running",
      totalBytes: 100,
    });
    expect(transferProgressPercent(committingUpload)).toBe(99);
    expect(transferPercentLabel(committingUpload)).toBe("99%");
    expect(transferStatusLabel(committingUpload.status, committingUpload.phase)).toBe(
      "提交文件",
    );
    const failedCommit = transfer({
      bytesTransferred: 100,
      direction: "upload",
      phase: "committing",
      status: "failed",
      totalBytes: 100,
    });
    expect(transferProgressPercent(failedCommit)).toBe(99);
    expect(transferPercentLabel(failedCommit)).toBe("99%");
    expect(
      transferProgressPercent(
        transfer({ bytesTransferred: 0, status: "succeeded", totalBytes: 0 }),
      ),
    ).toBe(100);
  });

  it("keeps archive zip writing visible after the remote download reaches its byte total", () => {
    const archiving = transfer({
      bytesTransferred: 100,
      operation: "archiveDownload",
      phase: "archiving",
      status: "running",
      totalBytes: 100,
    });

    expect(transferProgressPercent(archiving)).toBe(96);
    expect(transferPercentLabel(archiving)).toBe("压缩中");
    expect(transferStatusLabel(archiving.status, archiving.phase)).toBe("压缩中");
  });

  it("formats status, title, path, percent, and byte summaries", () => {
    const running = transfer({
      bytesTransferred: 1536,
      direction: "upload",
      localPath: "C:\\logs\\app.log",
      remotePath: "/var/log/app.log",
      source: {
        kind: "local",
        path: "C:\\logs\\app.log",
      },
      speedBytesPerSecond: 2048,
      status: "running",
      target: {
        hostId: "host-1",
        hostLabel: "host-1",
        kind: "remote",
        path: "/var/log/app.log",
      },
      totalBytes: 3072,
    });

    expect(transferTitle(running)).toBe("app.log");
    expect(transferPercentLabel(running)).toBe("50%");
    expect(transferPathSummary(running)).toBe(
      "C:\\logs\\app.log -> host-1:/var/log/app.log",
    );
    expect(transferStatusLabel("running")).toBe("传输中");
    expect(formatTransferBytes(running)).toBe("1.5 KB / 3.0 KB · 2.0 KB/s");
    expect(
      formatTransferBytes(
        transfer({
          bytesTransferred: 1536,
          speedBytesPerSecond: 2048,
          status: "succeeded",
          totalBytes: 3072,
        }),
      ),
    ).toBe("1.5 KB / 3.0 KB");
  });

  /** 窄面板的短文案也必须区分网络等待与排队，避免用户误判是否该取消。 */
  it("projects stale progress as waiting without keeping a stale speed", () => {
    const now = 1_700_000_000_000;
    const waiting = transfer({
      lastProgressAt: (now - 6_000) / 1_000,
      phase: "uploading",
      speedBytesPerSecond: 64 * 1024,
      status: "running",
    });

    expect(isTransferWaiting(waiting, now)).toBe(true);
    expect(transferStatusLabel(waiting.status, "waiting")).toBe("等待响应");
    expect(transferCompactStatusLabel(waiting.status, "waiting")).toBe("待响应");
    expect(transferStatusLabel(waiting.status, "recovering")).toBe(
      "正在恢复连接（1/1）",
    );
    expect(transferStatusLabel(waiting.status, "canceling", true)).toBe("正在取消");
    expect(
      transferStatusLabel("canceled", "canceling", true),
    ).toBe("已取消");
    expect(transferStatusLabel("running", "connecting")).toBe("连接中");
    expect(transferStatusLabel("running", "verifying")).toBe("校验断点");
  });

  it("prefers structured source and target endpoints for cross-host tasks", () => {
    const remoteCopy = transfer({
      direction: "upload",
      localPath: "sftp://host-left/tmp/app.log",
      operation: "remoteCopy",
      remotePath: "/var/log/app.log",
      source: {
        hostId: "host-left",
        hostLabel: "left",
        kind: "remote",
        path: "/tmp/app.log",
      },
      target: {
        hostId: "host-right",
        hostLabel: "right",
        kind: "remote",
        path: "/var/log/app.log",
      },
      transportMode: "clientBridge",
    });

    expect(transferTitle(remoteCopy)).toBe("app.log");
    expect(transferPathSummary(remoteCopy)).toBe(
      "left:/tmp/app.log -> right:/var/log/app.log",
    );
  });

  it("falls back from placeholder endpoint labels before rendering paths", () => {
    const archiveDownload = transfer({
      operation: "archiveDownload",
      source: {
        hostLabel: "null",
        hostId: "source-host",
        kind: "remote",
        path: "/bwy/app/abc/.codex/jdk-21.0.2",
      },
      target: {
        kind: "local",
        path: "C:\\Users\\24052\\Downloads\\jdk-21.0.2.zip",
      },
    });

    expect(transferPathSummary(archiveDownload)).toBe(
      "source-host:/bwy/app/abc/.codex/jdk-21.0.2 -> C:\\Users\\24052\\Downloads\\jdk-21.0.2.zip",
    );
  });

  it("summarizes queue state by active, failed, completed, and fallback order", () => {
    const completed = transfer({ status: "succeeded" });

    expect(
      transferStatusSummary({
        activeCount: 2,
        completedCount: 1,
        failedCount: 0,
        totalCount: 3,
        transfer: completed,
      }),
    ).toBe("后台传输 2 项，1 项已结束");
    expect(
      transferStatusSummary({
        activeCount: 0,
        completedCount: 0,
        failedCount: 1,
        totalCount: 1,
        transfer: completed,
      }),
    ).toBe("1 项传输失败，可从任务记录重试或清理");
    expect(
      transferStatusSummary({
        activeCount: 0,
        completedCount: 1,
        failedCount: 0,
        totalCount: 1,
        transfer: completed,
      }),
    ).toBe("1 项传输完成");
  });

  it("classifies active and finished transfer counts", () => {
    const transfers = [
      transfer({ id: "queued", status: "queued" }),
      transfer({ id: "running", status: "running" }),
      transfer({ id: "failed", status: "failed" }),
      transfer({ id: "canceled", status: "canceled" }),
    ];

    expect(activeTransferCount(transfers)).toBe(2);
    expect(transfers.filter(isFinishedTransfer).map((item) => item.id)).toEqual([
      "failed",
      "canceled",
    ]);
  });

  it("allows cancel only for queued or running transfers without a pending cancel request", () => {
    expect(canCancelTransfer(transfer({ status: "queued" }))).toBe(true);
    expect(canCancelTransfer(transfer({ status: "running" }))).toBe(true);
    expect(canCancelTransfer(transfer({ cancelable: false, status: "running" }))).toBe(false);
    expect(
      canCancelTransfer(
        transfer({ cancelRequested: true, status: "queued" }),
      ),
    ).toBe(false);
    expect(
      canCancelTransfer(
        transfer({ cancelRequested: true, status: "running" }),
      ),
    ).toBe(false);
    expect(canCancelTransfer(transfer({ status: "succeeded" }))).toBe(false);
    expect(canCancelTransfer(transfer({ status: "failed" }))).toBe(false);
    expect(canCancelTransfer(transfer({ status: "canceled" }))).toBe(false);
  });

  it("allows clearing only when the transfer queue contains finished tasks", () => {
    expect(
      canClearFinishedTransfers([
        transfer({ id: "queued", status: "queued" }),
        transfer({ id: "running", status: "running" }),
      ]),
    ).toBe(false);
    expect(
      canClearFinishedTransfers([
        transfer({ id: "queued", status: "queued" }),
        transfer({ id: "succeeded", status: "succeeded" }),
      ]),
    ).toBe(true);
    expect(
      canClearFinishedTransfers([transfer({ id: "failed", status: "failed" })]),
    ).toBe(true);
    expect(
      canClearFinishedTransfers([
        transfer({ id: "canceled", status: "canceled" }),
      ]),
    ).toBe(true);
  });

  it("formats file names and sizes used by transfer labels", () => {
    expect(fileNameFromPath("C:\\Users\\kong\\Downloads\\report.zip")).toBe(
      "report.zip",
    );
    expect(fileNameFromPath("/tmp/", "folder")).toBe("folder");
    expect(formatFileSize(42)).toBe("42 B");
    expect(formatFileSize(2048)).toBe("2.0 KB");
    expect(formatFileSize(2 * 1024 * 1024)).toBe("2.0 MB");
  });
});
