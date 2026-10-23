// @author kongweiguang

import { StrictMode, useState } from "react";
import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { updaterApiMock } from "../../support/settings/SettingsToolContent.testHarness";
import { MachineSidebar } from "../../../../src/features/machine-sidebar/MachineSidebar";
import { useAppUpdateCheck } from "../../../../src/app/useAppUpdateCheck";
import { useAppUpdateStore } from "../../../../src/features/settings/appUpdateStore";
import { SettingsToolContent } from "../../../../src/features/settings/SettingsToolContent";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import type {
  AppUpdateCheckResult,
  AppUpdateInstallResult,
} from "../../../../src/lib/updaterApi";
import { localSidebarGroups } from "../../support/machine-sidebar/MachineSidebar.testSupport";

type EntrySettingsSection = "settings-appearance" | "settings-about";

/** A small deferred keeps discovery and installation pending while the shared entry is exercised. */
function createDeferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });

  return { promise, reject, resolve };
}

/** The startup hook models the app shell while the rest of the harness keeps the real sidebar and settings content. */
function AppUpdateEntryHarness() {
  const [settingsOpen, setSettingsOpen] = useState(true);
  const [settingsSection, setSettingsSection] =
    useState<EntrySettingsSection>("settings-appearance");

  useAppUpdateCheck(defaultAppSettings.desktopNotifications);

  /** The update shortcut and ordinary settings affordance share the About destination. */
  const openSettingsAbout = () => {
    setSettingsSection("settings-about");
    setSettingsOpen(true);
  };

  return (
    <>
      <button onClick={() => setSettingsOpen(false)} type="button">
        关闭设置内容
      </button>
      <MachineSidebar
        groups={localSidebarGroups}
        onOpenSettings={openSettingsAbout}
        onOpenUpdates={openSettingsAbout}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />
      {settingsOpen ? (
        <SettingsToolContent
          key={settingsSection}
          initialSectionId={settingsSection}
          onSettingsChange={vi.fn()}
          settings={defaultAppSettings}
        />
      ) : null}
    </>
  );
}

describe("AppUpdateButton entry", () => {
  // The updater check is held open so the test can prove the icon is absent before shared discovery finishes.
  it("opens About, installs the already discovered update, and keeps the shortcut through close, failure, and retry", async () => {
    const user = userEvent.setup();
    const updateCheck = createDeferred<AppUpdateCheckResult>();
    const firstInstall = createDeferred<AppUpdateInstallResult>();
    const availableUpdate: AppUpdateCheckResult = {
      currentVersion: "0.3.46",
      kind: "available",
      version: "0.3.47",
    };
    updaterApiMock.checkForAppUpdate.mockReturnValueOnce(updateCheck.promise);
    updaterApiMock.installPendingAppUpdate
      .mockReturnValueOnce(firstInstall.promise)
      .mockResolvedValueOnce({ downloadedBytes: 1024, version: "0.3.47" });

    render(
      <StrictMode>
        <AppUpdateEntryHarness />
      </StrictMode>,
    );

    expect(
      screen.queryByRole("button", { name: "有新版本，打开更新" }),
    ).not.toBeInTheDocument();
    expect(updaterApiMock.checkForAppUpdate).toHaveBeenCalledTimes(1);

    await act(async () => {
      updateCheck.resolve(availableUpdate);
      await updateCheck.promise;
    });
    const updateEntry = await screen.findByRole("button", {
      name: "有新版本，打开更新",
    });
    expect(updateEntry).toBeVisible();

    await user.click(updateEntry);
    expect(await screen.findByText("关于 Kerminal")).toBeInTheDocument();
    expect(await screen.findByText(/发现 v0\.3\.47/)).toBeInTheDocument();
    expect(updaterApiMock.checkForAppUpdate).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole("button", { name: "安装" }));
    expect(await screen.findByText("正在下载更新。")).toBeInTheDocument();
    expect(updaterApiMock.installPendingAppUpdate).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole("button", { name: "关闭设置内容" }));
    const installingEntry = screen.getByRole("button", {
      name: "查看更新进度",
    });
    expect(installingEntry).toBeVisible();
    await user.click(installingEntry);
    expect(await screen.findByText("关于 Kerminal")).toBeInTheDocument();
    expect(updaterApiMock.checkForAppUpdate).toHaveBeenCalledTimes(1);

    await act(async () => {
      firstInstall.reject(new Error("download interrupted"));
      await firstInstall.promise.catch(() => undefined);
    });
    expect(await screen.findByText("安装更新失败")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "安装" })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "关闭设置内容" }));
    await user.click(
      screen.getByRole("button", { name: "有新版本，打开更新" }),
    );
    expect(await screen.findByText("安装更新失败")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "安装" }));

    expect(await screen.findByText("等待重启")).toBeInTheDocument();
    expect(updaterApiMock.installPendingAppUpdate).toHaveBeenCalledTimes(2);
    expect(updaterApiMock.checkForAppUpdate).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole("button", { name: "关闭设置内容" }));
    const restartEntry = screen.getByRole("button", {
      name: "重启以完成更新",
    });
    expect(restartEntry).toBeVisible();
    await user.click(restartEntry);
    expect(await screen.findByText("等待重启")).toBeInTheDocument();
    expect(updaterApiMock.checkForAppUpdate).toHaveBeenCalledTimes(1);
  });

  // The narrow sidebar keeps the same discovered-update callback without adding a second checking path.
  it("renders and invokes the update entry in the collapsed sidebar", async () => {
    const user = userEvent.setup();
    const onOpenUpdates = vi.fn();
    useAppUpdateStore.setState({
      checkState: "available",
      updateResult: {
        currentVersion: "0.3.46",
        kind: "available",
        version: "0.3.47",
      },
    });

    render(
      <MachineSidebar
        collapsed
        groups={localSidebarGroups}
        onOpenUpdates={onOpenUpdates}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    const settingsEntry = screen.getByRole("button", { name: "打开设置" });
    const updateEntry = screen.getByRole("button", {
      name: "有新版本，打开更新",
    });
    expect(settingsEntry.compareDocumentPosition(updateEntry)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
    await user.click(updateEntry);
    expect(onOpenUpdates).toHaveBeenCalledTimes(1);
  });
});
