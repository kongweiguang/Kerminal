// @author kongweiguang

import { Settings } from "lucide-react";
import { Button } from "../../components/ui/button";
import { cn } from "../../lib/cn";
import { AppUpdateButton } from "../settings/updates";
import type { MachineSidebarProps } from "./MachineSidebar.shared";

type MachineSidebarSettingsButtonsProps = Pick<
  MachineSidebarProps,
  "onOpenSettings" | "onOpenUpdates" | "settingsSelected"
> & { compact?: boolean };

/** 两种侧栏共用设置和更新入口；更新按钮仅经 settings 的更新公开入口依赖，避免加载设置主视图。 */
export function MachineSidebarSettingsButtons({
  compact = false,
  onOpenSettings,
  onOpenUpdates,
  settingsSelected = false,
}: MachineSidebarSettingsButtonsProps) {
  return (
    <>
      <Button
        aria-label="打开设置"
        aria-pressed={settingsSelected}
        className={cn(
          !compact && "h-8 w-8 rounded-lg",
          settingsSelected && "bg-[var(--surface-selected)] text-sky-700 dark:text-sky-100",
        )}
        onClick={onOpenSettings}
        size="icon"
        title="设置"
        variant="ghost"
      >
        <Settings className="h-4 w-4" />
      </Button>
      <AppUpdateButton onOpenUpdates={onOpenUpdates} />
    </>
  );
}
