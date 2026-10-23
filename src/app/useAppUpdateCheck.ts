// @author kongweiguang

import { useEffect } from "react";
import { useAppUpdateStore } from "../features/settings/appUpdateStore";
import type { DesktopNotificationSettings } from "../features/settings/settingsModel";

const updateCheckIntervalMs = 6 * 60 * 60 * 1_000;

/** 检查归主窗口生命周期所有，设置页关闭后仍保留结果；低频轮询兼顾长期开机与请求成本。 */
export function useAppUpdateCheck(desktopNotifications: DesktopNotificationSettings) {
  useEffect(() => {
    void useAppUpdateStore.getState().ensureChecked(desktopNotifications);
    const interval = window.setInterval(() => {
      const updater = useAppUpdateStore.getState();
      if (updater.updateResult?.kind !== "available") {
        void updater.checkForUpdate(desktopNotifications);
      }
    }, updateCheckIntervalMs);
    return () => window.clearInterval(interval);
  }, [desktopNotifications]);
}
