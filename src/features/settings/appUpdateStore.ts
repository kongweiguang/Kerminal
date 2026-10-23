// @author kongweiguang
import { create } from "zustand";
import {
  currentDesktopNotificationVisibility,
  sendDesktopNotification,
} from "../../lib/desktopNotificationApi";
import {
  checkForAppUpdate,
  installPendingAppUpdate,
  relaunchApp,
  type AppUpdateCheckResult,
  type AppUpdateInstallResult,
  type AppUpdateProgress,
} from "../../lib/updaterApi";
import {
  buildUserFacingError,
  type UserFacingMessage,
} from "../../lib/userFacingMessage";
import type { DesktopNotificationSettings } from "./settingsModel";

export type UpdateCheckState =
  | "idle"
  | "checking"
  | "available"
  | "up-to-date"
  | "unavailable"
  | "installing"
  | "ready-to-restart"
  | "restarting"
  | "error";

interface AppUpdateStoreState {
  checkState: UpdateCheckState;
  error: UserFacingMessage | null;
  installResult: AppUpdateInstallResult | null;
  progress: AppUpdateProgress | null;
  updateResult: AppUpdateCheckResult | null;
}

interface AppUpdateStore extends AppUpdateStoreState {
  checkForUpdate: (desktopNotifications: DesktopNotificationSettings) => Promise<void>;
  ensureChecked: (desktopNotifications: DesktopNotificationSettings) => Promise<void>;
  installUpdate: () => Promise<void>;
  restartApp: () => Promise<void>;
}

/** 测试和设置宿主共享同一初始快照，更新状态仅属于当前应用进程。 */
export const appUpdateInitialState: AppUpdateStoreState = {
  checkState: "idle",
  error: null,
  installResult: null,
  progress: null,
  updateResult: null,
};

/** 通知节流快照随应用存活，避免定时检查对同一版本反复打扰用户。 */
const updateNotificationSentAtByKey: Record<string, number | undefined> = {};

/**
 * 将桌面更新状态和 updater 句柄的所有权留在应用级 store，确保导航卸载设置页后仍能安装已发现的版本。
 */
export const useAppUpdateStore = create<AppUpdateStore>()((set, get) => ({
  ...appUpdateInitialState,

  /**
   * 通过同一运行态 store 执行检查；检查期间或已开始安装/重启时拒绝竞争请求，避免替换待安装句柄。
   */
  checkForUpdate: async (desktopNotifications) => {
    const current = get();
    if (
      current.checkState === "checking" ||
      current.checkState === "installing" ||
      current.checkState === "restarting" ||
      current.checkState === "ready-to-restart"
    ) {
      return;
    }

    const preserveAvailableUpdate = current.updateResult?.kind === "available";
    set({
      checkState: "checking",
      error: null,
      installResult: null,
      progress: null,
    });

    try {
      const result = await checkForAppUpdate();
      set({
        checkState: result.kind,
        error: null,
        installResult: null,
        progress: null,
        updateResult: result,
      });

      if (result.kind === "available") {
        void sendDesktopNotification({
          event: {
            currentVersion: result.currentVersion,
            kind: "updater.available",
            version: result.version,
          },
          lastSentAtByKey: updateNotificationSentAtByKey,
          settings: desktopNotifications,
          visibility: currentDesktopNotificationVisibility(),
        });
      }
    } catch (nextError) {
      set({
        checkState: preserveAvailableUpdate ? "available" : "error",
        error: buildUserFacingError(nextError, {
          recoveryAction: "请检查网络连接后重试。",
          title: "检查更新失败",
        }),
      });
    }
  },

  /**
   * 启动入口只在尚未检查时发起一次请求；StrictMode 重挂载或重复宿主不会并行检查。
   */
  ensureChecked: async (desktopNotifications) => {
    if (get().checkState !== "idle") {
      return;
    }
    await get().checkForUpdate(desktopNotifications);
  },

  /**
   * 只安装当前共享快照对应的 Update；失败时保留 available 与错误提示，让用户可以直接重试。
   */
  installUpdate: async () => {
    const current = get();
    if (
      current.checkState !== "available" ||
      current.updateResult?.kind !== "available"
    ) {
      return;
    }

    set({ checkState: "installing", error: null, installResult: null, progress: null });
    try {
      const result = await installPendingAppUpdate((progress) => set({ progress }));
      set({ checkState: "ready-to-restart", error: null, installResult: result });
    } catch (nextError) {
      set({
        checkState: "available",
        error: buildUserFacingError(nextError, {
          recoveryAction: "请稍后直接重试安装。",
          title: "安装更新失败",
        }),
      });
    }
  },

  /**
   * 安装完成后再请求重启；失败时保留可重试的 ready-to-restart 状态。
   */
  restartApp: async () => {
    if (get().checkState !== "ready-to-restart") {
      return;
    }

    set({ checkState: "restarting", error: null });
    try {
      await relaunchApp();
    } catch (nextError) {
      set({
        checkState: "ready-to-restart",
        error: buildUserFacingError(nextError, {
          recoveryAction: "请手动重新启动 Kerminal。",
          title: "自动重启失败",
        }),
      });
    }
  },
}));
