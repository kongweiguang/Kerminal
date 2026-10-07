// @author kongweiguang

import { useRef, useState, type ReactNode } from "react";
import {
  Download,
  ExternalLink,
  GitBranch,
  Hash,
  Info,
  RefreshCw,
  RotateCcw,
  Rocket,
  Scale,
  type LucideIcon,
} from "lucide-react";
import packageJson from "../../../../package.json";
import { UserFacingNotice } from "../../../components/ui/user-facing-notice";
import { cn } from "../../../lib/cn";
import {
  currentDesktopNotificationVisibility,
  sendDesktopNotification,
} from "../../../lib/desktopNotificationApi";
import {
  checkForAppUpdate,
  installPendingAppUpdate,
  relaunchApp,
  type AppUpdateCheckResult,
  type AppUpdateInstallResult,
  type AppUpdateProgress,
} from "../../../lib/updaterApi";
import {
  buildUserFacingError,
  type UserFacingMessage,
} from "../../../lib/userFacingMessage";
import type { DesktopNotificationSettings } from "../settingsModel";
import { desktopRuntime } from "../../../lib/desktopRuntimeApi";

const githubRepositoryUrl = "https://github.com/kongweiguang/Kerminal";
const appVersion = `v${packageJson.version}`;
const licenseName = packageJson.license ?? "GPL-3.0-or-later";
const aboutPanelClassName =
  "kerminal-solid-surface rounded-[var(--radius-panel)] border p-4";
const aboutListClassName =
  "mt-4 overflow-hidden rounded-[var(--radius-card)] border border-[var(--border-subtle)] bg-[var(--surface-content)]";
const aboutRowClassName =
  "flex flex-wrap items-center justify-between gap-3 border-b border-[var(--border-subtle)] px-3 py-2.5 last:border-b-0";
const aboutButtonClassName =
  "kerminal-focus-ring kerminal-pressable inline-flex h-8 items-center gap-1.5 rounded-[var(--radius-control)] border border-[var(--border-subtle)] bg-[var(--surface-content)] px-2.5 text-xs font-medium text-[var(--text-primary)] transition hover:bg-[var(--surface-hover)] disabled:cursor-not-allowed disabled:opacity-60";
const aboutPrimaryButtonClassName =
  "kerminal-focus-ring kerminal-pressable inline-flex h-8 items-center gap-1.5 rounded-[var(--radius-control)] border border-sky-400/25 bg-[var(--surface-selected)] px-2.5 text-xs font-medium text-sky-700 transition hover:brightness-105 disabled:cursor-not-allowed disabled:opacity-60 dark:text-sky-100";

type UpdateCheckState =
  | "idle"
  | "checking"
  | "available"
  | "up-to-date"
  | "unavailable"
  | "installing"
  | "ready-to-restart"
  | "restarting"
  | "error";

interface AboutSettingsSectionProps {
  desktopNotifications: DesktopNotificationSettings;
}

/** 关于页沿用 GitHub 仓库的规范名称，让展示地址与实际打开地址保持一致。 */
export function AboutSettingsSection({
  desktopNotifications,
}: AboutSettingsSectionProps) {
  const [checkState, setCheckState] = useState<UpdateCheckState>("idle");
  const [error, setError] = useState<UserFacingMessage | null>(null);
  const [installResult, setInstallResult] =
    useState<AppUpdateInstallResult | null>(null);
  const [linkError, setLinkError] = useState<UserFacingMessage | null>(null);
  const [progress, setProgress] = useState<AppUpdateProgress | null>(null);
  const [updateResult, setUpdateResult] = useState<AppUpdateCheckResult | null>(
    null,
  );
  const updateNotificationSentAtByKey = useRef<
    Record<string, number | undefined>
  >({});
  const checking = checkState === "checking";
  const installing = checkState === "installing";
  const restarting = checkState === "restarting";
  const canInstall =
    checkState === "available" && updateResult?.kind === "available";
  const canRestart = checkState === "ready-to-restart";

  const handleCheck = async () => {
    setCheckState("checking");
    setError(null);
    setInstallResult(null);
    setProgress(null);
    setUpdateResult(null);
    try {
      const result = await checkForAppUpdate();
      setUpdateResult(result);
      setCheckState(result.kind);
      if (result.kind === "available") {
        void sendDesktopNotification({
          event: {
            currentVersion: result.currentVersion,
            kind: "updater.available",
            version: result.version,
          },
          lastSentAtByKey: updateNotificationSentAtByKey.current,
          settings: desktopNotifications,
          visibility: currentDesktopNotificationVisibility(),
        });
      }
    } catch (nextError) {
      setError(
        buildUserFacingError(nextError, {
          recoveryAction: "请检查网络连接后重试。",
          title: "检查更新失败",
        }),
      );
      setCheckState("error");
    }
  };

  const handleInstall = async () => {
    setCheckState("installing");
    setError(null);
    setInstallResult(null);
    setProgress(null);
    try {
      const result = await installPendingAppUpdate(setProgress);
      setInstallResult(result);
      setCheckState("ready-to-restart");
    } catch (nextError) {
      setError(
        buildUserFacingError(nextError, {
          recoveryAction: "请稍后重新安装。",
          title: "安装更新失败",
        }),
      );
      setCheckState("error");
    }
  };

  const handleRestart = async () => {
    setCheckState("restarting");
    setError(null);
    try {
      await relaunchApp();
    } catch (nextError) {
      setError(
        buildUserFacingError(nextError, {
          recoveryAction: "请手动重新启动 Kerminal。",
          title: "自动重启失败",
        }),
      );
      setCheckState("ready-to-restart");
    }
  };

  const handleOpenGitHub = async () => {
    setLinkError(null);
    try {
      await openExternalUrl(githubRepositoryUrl);
    } catch (nextError) {
      setLinkError(
        buildUserFacingError(nextError, {
          recoveryAction: "请检查系统默认浏览器设置后重试。",
          title: "无法打开 GitHub",
        }),
      );
    }
  };

  return (
    <section className={aboutPanelClassName} id="settings-about-panel">
      <div className="flex items-center gap-2 text-sm font-semibold text-zinc-950 dark:text-zinc-50">
        <Info className="h-4 w-4 text-sky-500 dark:text-sky-300" />
        关于 Kerminal
      </div>

      <div className={aboutListClassName}>
        <AboutInfoRow icon={Hash} label="版本" value={appVersion} />
        <AboutInfoRow icon={Scale} label="协议" value={licenseName} />

        <div className={aboutRowClassName}>
          <AboutRowLabel
            error={linkError}
            icon={GitBranch}
            label="GitHub"
            value="github.com/kongweiguang/Kerminal"
          />
          <button
            aria-label="打开 GitHub"
            className={aboutButtonClassName}
            onClick={() => void handleOpenGitHub()}
            type="button"
          >
            打开
            <ExternalLink className="h-3.5 w-3.5" />
          </button>
        </div>

        <div className={aboutRowClassName}>
          <AboutRowLabel
            error={error}
            icon={RotateCcw}
            label="更新"
            status={
              <span className={updateBadgeClassName(checkState)}>
                {updateStateLabel(checkState)}
              </span>
            }
            value={updateStatusText(
              checkState,
              updateResult,
              progress,
              installResult,
            )}
          />
          <div className="flex shrink-0 flex-wrap items-center justify-end gap-2">
            <button
              className={aboutButtonClassName}
              disabled={checking || installing || restarting}
              onClick={() => void handleCheck()}
              type="button"
            >
              <RefreshCw
                className={cn("h-3.5 w-3.5", checking && "animate-spin")}
              />
              {checking ? "检查中" : "检查"}
            </button>
            {canInstall ? (
              <button
                className={aboutPrimaryButtonClassName}
                disabled={installing}
                onClick={() => void handleInstall()}
                type="button"
              >
                <Download className="h-3.5 w-3.5" />
                安装
              </button>
            ) : null}
            {canRestart ? (
              <button
                className={aboutPrimaryButtonClassName}
                disabled={restarting}
                onClick={() => void handleRestart()}
                type="button"
              >
                <Rocket className="h-3.5 w-3.5" />
                {restarting ? "重启中" : "重启"}
              </button>
            ) : null}
          </div>
        </div>
      </div>
    </section>
  );
}

function AboutInfoRow({
  icon,
  label,
  value,
}: {
  icon: LucideIcon;
  label: string;
  value: string;
}) {
  return (
    <div className={aboutRowClassName}>
      <AboutRowLabel icon={icon} label={label} value={value} />
    </div>
  );
}

function AboutRowLabel({
  error,
  icon: Icon,
  label,
  status,
  value,
}: {
  error?: UserFacingMessage | null;
  icon: LucideIcon;
  label: string;
  status?: ReactNode;
  value: string;
}) {
  return (
    <div className="flex min-w-0 flex-1 items-start gap-3">
      <Icon className="mt-0.5 h-4 w-4 shrink-0 text-zinc-400" />
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
            {label}
          </span>
          {status}
        </div>
        <p className="mt-1 break-words text-xs leading-5 text-zinc-500 dark:text-zinc-400">
          {value}
        </p>
        {error ? (
          <UserFacingNotice className="mt-2" compact message={error} />
        ) : null}
      </div>
    </div>
  );
}

function updateStateLabel(state: UpdateCheckState) {
  if (state === "checking") {
    return "检查中";
  }
  if (state === "available") {
    return "可更新";
  }
  if (state === "up-to-date") {
    return "已是最新";
  }
  if (state === "unavailable") {
    return "不可用";
  }
  if (state === "installing") {
    return "下载中";
  }
  if (state === "ready-to-restart") {
    return "等待重启";
  }
  if (state === "restarting") {
    return "重启中";
  }
  if (state === "error") {
    return "失败";
  }
  return "可检查";
}

function updateStatusText(
  state: UpdateCheckState,
  result: AppUpdateCheckResult | null,
  progress: AppUpdateProgress | null,
  installResult: AppUpdateInstallResult | null,
) {
  if (state === "checking") {
    return "正在检查 GitHub Releases。";
  }

  if (state === "available" && result?.kind === "available") {
    return `发现 ${versionLabel(result.version)}，当前 ${versionLabel(
      result.currentVersion,
    )}。`;
  }

  if (state === "up-to-date") {
    return "已是最新版本。";
  }

  if (state === "unavailable" && result?.kind === "unavailable") {
    return result.message;
  }

  if (state === "installing") {
    if (progress?.percent !== undefined) {
      return `${downloadPhaseText(progress.phase)} ${progress.percent}%`;
    }
    return "正在下载更新。";
  }

  if (state === "ready-to-restart") {
    const version =
      installResult?.version ??
      (result?.kind === "available" ? result.version : undefined);
    return `${version ? `${versionLabel(version)} 已安装，` : "更新已安装，"}重启后生效。`;
  }

  if (state === "restarting") {
    return "正在重启 Kerminal。";
  }

  if (state === "error") {
    return "更新检查失败。";
  }

  return "手动检查更新。";
}

function updateBadgeClassName(state: UpdateCheckState) {
  if (state === "error" || state === "unavailable") {
    return "rounded-full border border-rose-300/25 bg-rose-500/10 px-2 py-0.5 text-xs text-rose-700 dark:text-rose-100";
  }

  if (state === "available" || state === "installing") {
    return "rounded-full border border-sky-400/25 bg-sky-400/10 px-2 py-0.5 text-xs text-sky-700 dark:text-sky-100";
  }

  if (state === "ready-to-restart" || state === "restarting") {
    return "rounded-full border border-violet-400/25 bg-violet-500/10 px-2 py-0.5 text-xs text-violet-700 dark:text-violet-100";
  }

  return "rounded-full border border-emerald-400/25 bg-emerald-400/10 px-2 py-0.5 text-xs text-emerald-700 dark:text-emerald-100";
}

function downloadPhaseText(phase: AppUpdateProgress["phase"]) {
  if (phase === "starting") {
    return "准备下载";
  }
  if (phase === "downloading") {
    return "下载中";
  }
  if (phase === "installing") {
    return "安装中";
  }
  return "下载完成";
}

function versionLabel(version: string) {
  return version.startsWith("v") ? version : `v${version}`;
}

async function openExternalUrl(url: string) {
  await desktopRuntime.openUrl(url);
}
