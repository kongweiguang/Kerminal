// @author kongweiguang

import { StrictMode } from "react";
import { act, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useAppUpdateCheck } from "../../../src/app/useAppUpdateCheck";
import {
  appUpdateInitialState,
  useAppUpdateStore,
} from "../../../src/features/settings/appUpdateStore";
import type { AppUpdateCheckResult } from "../../../src/lib/updaterApi";
import { defaultAppSettings } from "../../../src/features/settings/settingsModel";

const updateCheckMocks = vi.hoisted(() => ({
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

vi.mock("../../../src/lib/desktopNotificationApi", () =>
  updateCheckMocks.desktopNotificationApi,
);
vi.mock("../../../src/lib/updaterApi", () => updateCheckMocks.updaterApi);

/** The probe follows the app-shell lifetime so closing settings cannot own or clear its polling timer. */
function AppUpdateCheckProbe() {
  useAppUpdateCheck(defaultAppSettings.desktopNotifications);
  return null;
}

/** Each case starts with a clean shared snapshot and inert notification mocks. */
function resetUpdateCheckState() {
  vi.useFakeTimers();
  useAppUpdateStore.setState(appUpdateInitialState);
  updateCheckMocks.desktopNotificationApi.currentDesktopNotificationVisibility.mockReset();
  updateCheckMocks.desktopNotificationApi.currentDesktopNotificationVisibility.mockReturnValue(
    "hidden",
  );
  updateCheckMocks.desktopNotificationApi.sendDesktopNotification.mockReset();
  updateCheckMocks.desktopNotificationApi.sendDesktopNotification.mockResolvedValue(
    { reason: "disabled", requestedPermission: false, sent: false },
  );
  updateCheckMocks.updaterApi.checkForAppUpdate.mockReset();
  updateCheckMocks.updaterApi.checkForAppUpdate.mockResolvedValue({
    kind: "up-to-date",
  });
  updateCheckMocks.updaterApi.installPendingAppUpdate.mockReset();
  updateCheckMocks.updaterApi.relaunchApp.mockReset();
}

describe("useAppUpdateCheck", () => {
  beforeEach(() => {
    resetUpdateCheckState();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  // StrictMode replays effects, so the store must collapse the startup request while the interval remains app-owned.
  it("checks once at startup, polls every six hours until an update is found, and clears the interval on unmount", async () => {
    const availableUpdate: AppUpdateCheckResult = {
      currentVersion: "0.3.46",
      kind: "available",
      version: "0.3.47",
    };
    updateCheckMocks.updaterApi.checkForAppUpdate
      .mockResolvedValueOnce({ kind: "up-to-date" })
      .mockResolvedValueOnce(availableUpdate);

    const { unmount } = render(
      <StrictMode>
        <AppUpdateCheckProbe />
      </StrictMode>,
    );

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(updateCheckMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(6 * 60 * 60 * 1_000 - 1);
    });
    expect(updateCheckMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
      await Promise.resolve();
    });
    expect(updateCheckMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(2);
    expect(useAppUpdateStore.getState().updateResult).toEqual(availableUpdate);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(6 * 60 * 60 * 1_000);
    });
    expect(updateCheckMocks.updaterApi.checkForAppUpdate).toHaveBeenCalledTimes(2);

    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });
});
