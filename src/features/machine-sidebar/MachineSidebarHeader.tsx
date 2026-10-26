// @author kongweiguang

import {
  ArrowLeftRight,
  ChevronsDownUp,
  ChevronsUpDown,
  Search,
} from "lucide-react";
import { Button } from "../../components/ui/button";
import { cn } from "../../lib/cn";

const accentIconButtonClassName =
  "kerminal-pressable h-8 w-8 rounded-lg text-sky-600 hover:bg-[var(--surface-hover)] dark:text-sky-300";
const iconButtonClassName =
  "kerminal-pressable h-8 w-8 rounded-lg text-zinc-500 hover:bg-[var(--surface-hover)] hover:text-zinc-950 dark:text-zinc-400 dark:hover:text-zinc-50";
const searchInputClassName =
  "kerminal-sidebar-search kerminal-field-surface w-full rounded-xl border pl-9 pr-3 text-sm text-zinc-950 placeholder:text-zinc-400 dark:text-zinc-100 dark:placeholder:text-zinc-600";

/** 单行搜索保留列表空间，并固定常用操作区宽度，避免窄侧栏挤压按钮。 */
export function MachineSidebarHeader({
  allGroupsCollapsed,
  groupCount,
  groupToggleLabel,
  onOpenTransferWorkbench,
  onSearchChange,
  onToggleAllGroups,
  search,
}: {
  allGroupsCollapsed: boolean;
  groupCount: number;
  groupToggleLabel: string;
  onOpenTransferWorkbench?: () => void;
  onSearchChange: (query: string) => void;
  onToggleAllGroups: () => void;
  search: string;
}) {
  const GroupToggleIcon = allGroupsCollapsed ? ChevronsUpDown : ChevronsDownUp;
  return (
    <div
      className="kerminal-sidebar-header flex items-center gap-2"
      data-tauri-drag-region
    >
      <label className="relative min-w-0 flex-1">
        <span className="sr-only">搜索主机</span>
        <Search
          aria-hidden="true"
          className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-zinc-500"
        />
        <input
          aria-label="搜索主机"
          className={cn(searchInputClassName, "h-8 outline-none")}
          disabled={groupCount === 0}
          onChange={(event) => onSearchChange(event.currentTarget.value)}
          placeholder="搜索"
          value={search}
        />
      </label>
      <div className="flex w-[68px] shrink-0 items-center gap-1">
        <Button
          aria-label="打开 SFTP 传输工作台"
          className={accentIconButtonClassName}
          disabled={!onOpenTransferWorkbench}
          onClick={onOpenTransferWorkbench}
          size="icon"
          title="SFTP 传输"
          type="button"
          variant="ghost"
        >
          <ArrowLeftRight className="h-4 w-4" />
        </Button>
        <Button
          aria-label={groupToggleLabel}
          aria-pressed={allGroupsCollapsed}
          className={iconButtonClassName}
          disabled={groupCount === 0}
          onClick={onToggleAllGroups}
          size="icon"
          title={groupToggleLabel}
          type="button"
          variant="ghost"
        >
          <GroupToggleIcon className="h-4 w-4" />
        </Button>
      </div>
    </div>
  );
}
