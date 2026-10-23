// @author kongweiguang
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  appUpdateInitialState,
  useAppUpdateStore,
} from "../../../../src/features/settings/appUpdateStore";
import type { AppUpdateCheckResult } from "../../../../src/lib/updaterApi";

const updateStoreMocks = vi.hoisted(() => ({
  desktopNotificationApi: {
    currentDesktopNotificationVisibility: vi.fn(),
    sendDesktopNotification: vi.fn(),
  },
  updaterApi: {
    checkForAppUpdate: vi.fn(),
    installPendingAppUpdate: vi.fn(),
    relaunchApp: vi.fn(),
  },
}));

vi.mock("../../../../src/lib/desktopNotificationApi", () =>
  updateStoreMocks.desktopNotificationApi,
);
vi.mock("../../../../src/lib/updaterApi", () => updateStoreMocks.updaterApi);

const availableUpdate: AppUpdateCheckResult = {
  currentVersion: "0.1.0",
  kind: "available",
  version: "0.2.0",
};

/** 恢复应用级快照和 mock 调用记录，防止单例状态跨用例泄漏。 */
function resetUpdateStore() {
  useAppUpdateStore.setState(appUpdateInitialState);
  updateStoreMocks.desktopNotificationApi.currentDesktopNotificationVisibility.mockReset();
  updateStoreMocks.desktopNotificationApi.currentDesktopNotificationVisibility.mockReturnValue(
    "hidden",
  );
  updateStoreMocks.desktopNotificationApi.sendDesktopNotification.mockReset();
  updateStoreMocks.desktopNotificationApi.sendDesktopNotification.mockResolvedValue(
    { reason: "disabled", requestedPermission: false, sent: false },
  );
  updateStoreMocks.updaterApi.checkForAppUpdate.mockReset();
  updateStoreMocks.updaterApi.checkForAppUpdate.mockResolvedValue({ kind: "up-to-date" });
  updateStoreMocks.updaterApi.installPendingAppUpdate.mockReset();
  updateStoreMocks.updaterApi.installPendingAppUpdate.mockResolvedValue({
    downloadedBytes: 128,
    version: "0.2.0",
  });
  updateStoreMocks.updaterApi.relaunchApp.mockReset();
  updateStoreMocks.updaterApi.relaunchApp.mockResolvedValue(undefined);
}

describe("app update store", () => {
  // 更新结果由应用进程级 store 持有，每条用例都显式清空共享状态。
  beforeEach(() => {
    resetUpdateStore();
  });

  // 启动宿主可能在开发 StrictMode 下重复挂载，共享 store 必须合并尚未完成的首检。
  it("runs only one startup check and shares the discovered update", async () => {
    let resolveCheck!: (result: AppUpdateCheckResult) => void;
    updateStoreMocks.updaterApi.checkForAppUpdate.mockReturnValueOnce(
      new Promise<AppUpdateCheckResult>((resolve) => {
        resolveCheck = resolve;
      }),
    );
    const settings = {
      backgroundOnly: true,
      enabled: true,
      importantOnly: false,
      minDurationMs: 10_000,
      throttleMs: 30_000,
    };

    const firstCheck = useAppUpdateStore.getState().ensureChecked(settings);
    const duplicateCheck = useAppUpdateStore.getState().ensureChecked(settings);

    expect(updateStoreMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(1);
    expect(useAppUpdateStore.getState().checkState).toBe("checking");
    resolveCheck(availableUpdate);
    await Promise.all([firstCheck, duplicateCheck]);

    expect(useAppUpdateStore.getState().checkState).toBe("available");
    expect(useAppUpdateStore.getState().updateResult).toEqual(availableUpdate);
    expect(
      updateStoreMocks.desktopNotificationApi.sendDesktopNotification,
    ).toHaveBeenCalledWith(
      expect.objectContaining({
        event: {
          currentVersion: "0.1.0",
          kind: "updater.available",
          version: "0.2.0",
        },
        settings,
        visibility: "hidden",
      }),
    );
  });

  // 下载失败时 updaterApi 仍持有待安装句柄，保留 available 才能原地重试而不重新检查。
  it("keeps a failed install retryable without another check", async () => {
    updateStoreMocks.updaterApi.checkForAppUpdate.mockResolvedValueOnce(
      availableUpdate,
    );
    updateStoreMocks.updaterApi.installPendingAppUpdate
      .mockRejectedValueOnce(new Error("download interrupted"))
      .mockResolvedValueOnce({ downloadedBytes: 128, version: "0.2.0" });

    await useAppUpdateStore.getState().checkForUpdate({
      backgroundOnly: true,
      enabled: false,
      importantOnly: false,
      minDurationMs: 10_000,
      throttleMs: 30_000,
    });
    await useAppUpdateStore.getState().installUpdate();

    expect(useAppUpdateStore.getState().checkState).toBe("available");
    expect(useAppUpdateStore.getState().error?.title).toBe("安装更新失败");
    expect(useAppUpdateStore.getState().updateResult).toEqual(availableUpdate);

    await useAppUpdateStore.getState().installUpdate();

    expect(updateStoreMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(1);
    expect(updateStoreMocks.updaterApi.installPendingAppUpdate).toHaveBeenCalledTimes(2);
    expect(useAppUpdateStore.getState().checkState).toBe("ready-to-restart");
    expect(useAppUpdateStore.getState().installResult).toEqual({
      downloadedBytes: 128,
      version: "0.2.0",
    });
  });

  // 安装中的 interval 以及安装完成待重启期间的调用不能抢占当前 updater 生命周期。
  it("does not check during installation or after installation succeeds", async () => {
    updateStoreMocks.updaterApi.checkForAppUpdate.mockResolvedValueOnce(
      availableUpdate,
    );
    let resolveInstall!: (result: { downloadedBytes: number; version: string }) => void;
    updateStoreMocks.updaterApi.installPendingAppUpdate.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveInstall = resolve;
      }),
    );
    const settings = {
      backgroundOnly: true,
      enabled: false,
      importantOnly: false,
      minDurationMs: 10_000,
      throttleMs: 30_000,
    };

    await useAppUpdateStore.getState().checkForUpdate(settings);
    const installOperation = useAppUpdateStore.getState().installUpdate();
    await useAppUpdateStore.getState().checkForUpdate(settings);
    expect(useAppUpdateStore.getState().checkState).toBe("installing");
    expect(updateStoreMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(1);

    resolveInstall({ downloadedBytes: 128, version: "0.2.0" });
    await installOperation;
    await useAppUpdateStore.getState().checkForUpdate(settings);

    expect(useAppUpdateStore.getState().checkState).toBe("ready-to-restart");
    expect(updateStoreMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(1);
    await useAppUpdateStore.getState().restartApp();
    expect(updateStoreMocks.updaterApi.relaunchApp).toHaveBeenCalledTimes(1);
    expect(useAppUpdateStore.getState().checkState).toBe("restarting");
  });
});
