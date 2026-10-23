// @author kongweiguang

import { Download, LoaderCircle, RotateCcw } from "lucide-react";
import { Button } from "../../components/ui/button";
import { cn } from "../../lib/cn";
import { useAppUpdateStore } from "./appUpdateStore";

/** 入口只订阅已有更新，不在导航时重新检查，避免替换设置页正准备安装的句柄。 */
export function AppUpdateButton({ onOpenUpdates }: { onOpenUpdates?: () => void }) {
  const checkState = useAppUpdateStore((state) => state.checkState);
  const updateResult = useAppUpdateStore((state) => state.updateResult);
  if (updateResult?.kind !== "available" || !onOpenUpdates) {
    return null;
  }

  const readyToRestart =
    checkState === "ready-to-restart" || checkState === "restarting";
  const installing = checkState === "installing";
  const label = readyToRestart
    ? "重启以完成更新"
    : installing
      ? "查看更新进度"
      : "有新版本，打开更新";
  const Icon = readyToRestart ? RotateCcw : installing ? LoaderCircle : Download;

  return (
    <Button
      aria-label={label}
      className="h-8 w-8 rounded-lg text-sky-700 dark:text-sky-300"
      onClick={onOpenUpdates}
      size="icon"
      title={`${label}（v${updateResult.version.replace(/^v/, "")}）`}
      variant="ghost"
    >
      <Icon aria-hidden="true" className={cn("h-4 w-4", installing && "animate-spin")} />
    </Button>
  );
}
