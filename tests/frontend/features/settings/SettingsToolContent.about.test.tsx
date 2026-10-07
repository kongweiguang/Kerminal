// @author kongweiguang
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import packageJson from "../../../../package.json";
import {
  defaultAppSettings,
  desktopNotificationApiMock,
  openerApiMock,
  renderSettingsToolContent,
  updaterApiMock,
} from "../../support/settings/SettingsToolContent.testHarness";
import { normalizeAppSettings } from "../../../../src/features/settings/settingsModel";

describe("SettingsToolContent about section", () => {
  // 仓库仅调整名称大小写，关于页仍通过既有 opener 打开规范地址。
  it("shows only essential about information and opens GitHub", async () => {
    const user = userEvent.setup();

    renderSettingsToolContent();

    await user.click(screen.getByRole("button", { name: /关于/ }));

    expect(screen.getByText("关于 Kerminal")).toBeInTheDocument();
    expect(screen.getAllByText(`v${packageJson.version}`).length).toBeGreaterThan(
      0,
    );
    expect(screen.getByText(packageJson.license)).toBeInTheDocument();
    expect(screen.getByText("GitHub")).toBeInTheDocument();
    expect(screen.getByText("更新")).toBeInTheDocument();
    expect(screen.queryByText("能力更新")).not.toBeInTheDocument();
    expect(screen.queryByText("产品信息")).not.toBeInTheDocument();
    expect(screen.queryByText("GitHub Releases")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "打开 GitHub" }));
    await waitFor(() => {
      expect(openerApiMock.openUrl).toHaveBeenCalledWith(
        "https://github.com/kongweiguang/Kerminal",
      );
    });

    await user.click(screen.getByRole("button", { name: "检查" }));
    await waitFor(() => {
      expect(updaterApiMock.checkForAppUpdate).toHaveBeenCalledTimes(1);
    });
    expect(await screen.findByText("已是最新版本。")).toBeInTheDocument();
  });

  it("can install an available update from the about section", async () => {
    const user = userEvent.setup();
    updaterApiMock.checkForAppUpdate.mockResolvedValueOnce({
      body: "提升下载体验。",
      currentVersion: packageJson.version,
      kind: "available",
      version: "0.2.0",
    });
    updaterApiMock.installPendingAppUpdate.mockImplementationOnce(
      async (
        onProgress?: (progress: {
          contentLength?: number;
          downloadedBytes: number;
          percent?: number;
          phase: "starting" | "downloading" | "installing" | "finished";
        }) => void,
      ) => {
        onProgress?.({
          contentLength: 1024,
          downloadedBytes: 0,
          percent: 0,
          phase: "starting",
        });
        onProgress?.({
          contentLength: 1024,
          downloadedBytes: 512,
          percent: 50,
          phase: "downloading",
        });
        return {
          contentLength: 1024,
          downloadedBytes: 1024,
          version: "0.2.0",
        };
      },
    );

    const settingsWithNotifications = normalizeAppSettings({
      ...defaultAppSettings,
      desktopNotifications: {
        ...normalizeAppSettings(defaultAppSettings).desktopNotifications,
        enabled: true,
      },
    });

    renderSettingsToolContent({ settings: settingsWithNotifications });

    await user.click(screen.getByRole("button", { name: /关于/ }));
    await user.click(screen.getByRole("button", { name: "检查" }));

    expect(await screen.findByText("可更新")).toBeInTheDocument();
    expect(screen.getByText(/发现 v0\.2\.0/)).toBeInTheDocument();
    expect(screen.queryByText(/提升下载体验/)).not.toBeInTheDocument();
    expect(
      desktopNotificationApiMock.sendDesktopNotification,
    ).toHaveBeenCalledWith(
      expect.objectContaining({
        event: {
          currentVersion: packageJson.version,
          kind: "updater.available",
          version: "0.2.0",
        },
        settings: expect.objectContaining({ enabled: true }),
        visibility: "hidden",
      }),
    );

    await user.click(screen.getByRole("button", { name: "安装" }));

    await waitFor(() => {
      expect(updaterApiMock.installPendingAppUpdate).toHaveBeenCalledTimes(1);
    });
    expect(await screen.findByText("等待重启")).toBeInTheDocument();
    expect(screen.getByText("v0.2.0 已安装，重启后生效。")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "重启" }));
    await waitFor(() => {
      expect(updaterApiMock.relaunchApp).toHaveBeenCalledTimes(1);
    });
  });

  it("keeps updater runtime failures in collapsed technical details", async () => {
    const user = userEvent.setup();
    updaterApiMock.checkForAppUpdate.mockRejectedValueOnce(
      new Error(
        'updater runtime failed at C:\\private\\updater.json with "password": "secret-value"',
      ),
    );

    renderSettingsToolContent();
    await user.click(screen.getByRole("button", { name: /关于/ }));
    await user.click(screen.getByRole("button", { name: "检查" }));

    expect(await screen.findByText("检查更新失败")).toBeVisible();
    expect(screen.getByText("请检查网络连接后重试。")).toBeVisible();
    const detail = screen.getByText(/updater runtime failed/);
    expect(detail.closest("details")).not.toHaveAttribute("open");
    expect(detail).not.toHaveTextContent("secret-value");

    await user.click(screen.getByText("技术详情"));
    expect(detail.closest("details")).toHaveAttribute("open");
  });
});
