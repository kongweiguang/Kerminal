/**
 * @author kongweiguang
 */

import {
  ArrowLeft,
  ArrowRight,
  Check,
  ChevronUp,
  CornerDownRight,
  Download,
  Eye,
  EyeOff,
  Ellipsis,
  FolderPlus,
  FolderTree,
  List,
  PanelRight,
  RefreshCw,
  Settings2,
  Terminal,
  Upload,
} from "lucide-react";
import {
  useEffect,
  useId,
  useRef,
  useState,
  type Dispatch,
  type KeyboardEvent,
  type ReactNode,
  type RefObject,
  type SetStateAction,
} from "react";
import { Button } from "../../../components/ui/button";
import { cn } from "../../../lib/cn";
import { SftpBookmarkControl } from "../sftp-bookmark/SftpBookmarkControl";
import { ToolbarButton } from "./ToolbarButton";
import type { SftpBrowserMode } from "./sftpBrowserModeModel";
import type {
  RemoteDirectoryListing,
  SftpFileTarget,
  SftpStatus,
  SftpTransferTarget,
} from "./types";

const dividerClassName =
  "mx-1 hidden h-5 w-px bg-[var(--border-subtle)] @[360px]:block";

interface SftpBrowserHeaderProps {
  actions: {
    downloadSelectedEntries: () => Promise<void>;
    openNewDirectoryDialog: () => void;
    setOperationStatus: Dispatch<SetStateAction<SftpStatus | null>>;
    setShowHiddenFiles: Dispatch<SetStateAction<boolean>>;
    setUploadMenuOpen: Dispatch<SetStateAction<boolean>>;
    showHiddenFiles: boolean;
    showLocalTransferActions: boolean;
    transferableSelectionCount: number;
    transferSelectedEntriesToTarget: () => Promise<void>;
    transferTarget: SftpTransferTarget | undefined;
    uploadMenuOpen: boolean;
    uploadMenuRef: RefObject<HTMLDivElement | null>;
  };
  chrome: {
    compact: boolean;
    headerPaddingClass: string;
    pathSurfaceClass: string;
  };
  follow: {
    busy: boolean;
    enabled: boolean;
    normalizedPath: string | undefined;
    setEnabled: Dispatch<SetStateAction<boolean>>;
    setup: () => Promise<void>;
    supported: boolean;
    visible: boolean;
  };
  navigation: {
    currentPath: string;
    fileTarget: SftpFileTarget;
    listing: RemoteDirectoryListing | null;
    loadDirectory: (path: string) => Promise<void>;
    loading: boolean;
    pathDraft: string;
    pathInputId: string;
    setPathDraft: Dispatch<SetStateAction<string>>;
    submitPathDraft: () => void;
  };
  summary: {
    browserMode: SftpBrowserMode;
    entryCount: number;
    selectedCount: number;
    setBrowserMode: Dispatch<SetStateAction<SftpBrowserMode>>;
    visibleEntryCount: number;
  };
}

/** 路径与操作保持独立行；视图菜单和可截断计数让窄侧栏不再增加工具栏高度。 */
export function SftpBrowserHeader({
  actions,
  chrome,
  follow,
  navigation,
  summary,
}: SftpBrowserHeaderProps) {
  const summaryLabel = navigation.loading
    ? "刷新中"
    : `${summary.visibleEntryCount} / ${summary.entryCount} 项${
        summary.selectedCount > 0 ? ` / 已选 ${summary.selectedCount}` : ""
      }`;

  return (
    <header
      className={cn(
        "kerminal-material-nav @container relative z-30 shrink-0 border-b",
        chrome.headerPaddingClass,
      )}
    >
      <div
        className={cn("kerminal-solid-surface border", chrome.pathSurfaceClass)}
      >
        <form
          className="flex min-w-0 items-center gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            navigation.submitPathDraft();
          }}
        >
          <span className="shrink-0 font-mono text-[11px] text-zinc-500 dark:text-zinc-400">
            {navigation.fileTarget.protocol}
          </span>
          <label className="sr-only" htmlFor={navigation.pathInputId}>
            当前远程路径
          </label>
          <input
            className={cn(
              "kerminal-field-surface min-w-0 flex-1 rounded-lg border font-mono text-zinc-900 placeholder:text-zinc-400 dark:text-zinc-50 dark:placeholder:text-zinc-600",
              chrome.compact ? "px-1.5 py-1 text-[13px]" : "px-2 py-1 text-sm",
            )}
            id={navigation.pathInputId}
            onChange={(event) => navigation.setPathDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                navigation.setPathDraft(navigation.currentPath);
              }
            }}
            spellCheck={false}
            value={navigation.pathDraft}
          />
          <SftpBookmarkControl
            currentPath={navigation.currentPath}
            disabled={navigation.loading}
            fileTarget={navigation.fileTarget}
            loadDirectory={navigation.loadDirectory}
          />
          <Button
            aria-label="跳转远程路径"
            className="kerminal-muted-surface h-8 w-8 rounded-lg border px-0 text-zinc-600 hover:bg-[var(--surface-hover)] dark:text-zinc-300"
            disabled={navigation.loading}
            size="sm"
            title="跳转"
            type="submit"
            variant="ghost"
          >
            <CornerDownRight className="h-3.5 w-3.5" />
          </Button>
          {follow.visible ? <DirectoryFollowControls follow={follow} /> : null}
        </form>
        {!chrome.compact ? (
          <div className="mt-2 truncate font-mono text-xs text-zinc-500 dark:text-zinc-400">
            {navigation.fileTarget.summary}
          </div>
        ) : null}
      </div>

      <div
        className={cn(
          "flex min-w-0 items-center justify-between gap-0.5 @[360px]:gap-2",
          chrome.compact ? "mt-2" : "mt-3",
        )}
      >
        <div className="flex shrink-0 items-center gap-0.5 @[360px]:gap-1.5">
          <ToolbarButton
            ariaLabel="返回上级目录"
            disabled={!navigation.listing?.parentPath || navigation.loading}
            icon={<ChevronUp className="h-3.5 w-3.5" />}
            label="上级"
            onClick={() =>
              navigation.listing?.parentPath &&
              void navigation.loadDirectory(navigation.listing.parentPath)
            }
          />
          <ToolbarButton
            ariaLabel="刷新目录"
            disabled={navigation.loading}
            icon={
              <RefreshCw
                className={cn(
                  "h-3.5 w-3.5",
                  navigation.loading && "animate-spin",
                )}
              />
            }
            label="刷新"
            onClick={() => {
              actions.setOperationStatus(null);
              void navigation.loadDirectory(navigation.currentPath);
            }}
          />
          <ToolbarButton
            ariaLabel={actions.showHiddenFiles ? "隐藏隐藏文件" : "显示隐藏文件"}
            icon={
              actions.showHiddenFiles ? (
                <EyeOff className="h-3.5 w-3.5" />
              ) : (
                <Eye className="h-3.5 w-3.5" />
              )
            }
            label={actions.showHiddenFiles ? "隐藏隐藏文件" : "显示隐藏文件"}
            onClick={() => actions.setShowHiddenFiles((current) => !current)}
            pressed={actions.showHiddenFiles}
          />
          <div className={dividerClassName} />
          {actions.showLocalTransferActions ? (
            <div className="relative" ref={actions.uploadMenuRef}>
              <ToolbarButton
                ariaExpanded={actions.uploadMenuOpen}
                ariaHaspopup="menu"
                ariaLabel="上传"
                icon={<Upload className="h-3.5 w-3.5" />}
                label="上传"
                onClick={() => actions.setUploadMenuOpen((current) => !current)}
                pressed={actions.uploadMenuOpen}
              />
            </div>
          ) : null}
          <ToolbarButton
            ariaLabel="新建目录"
            icon={<FolderPlus className="h-3.5 w-3.5" />}
            label="新建"
            onClick={actions.openNewDirectoryDialog}
          />
          <TransferAction actions={actions} />
        </div>
        <div className="flex min-w-0 flex-1 items-center justify-end gap-0.5 @[360px]:gap-2">
          <div
            className="min-w-0 max-w-40 truncate font-mono text-xs text-zinc-500 dark:text-zinc-400"
            title={summaryLabel}
          >
            {summaryLabel}
          </div>
          <SftpBrowserModeMenu
            mode={summary.browserMode}
            onModeChange={summary.setBrowserMode}
          />
        </div>
      </div>
    </header>
  );
}

function DirectoryFollowControls({
  follow,
}: Pick<SftpBrowserHeaderProps, "follow">) {
  const followTitle = follow.enabled
    ? `关闭目录跟随${follow.normalizedPath ? "（已同步终端目录）" : "（等待终端目录）"}`
    : "开启目录跟随";

  return (
    <div
      aria-label="目录跟随控制"
      className="flex shrink-0 items-center gap-1"
      role="group"
    >
      <button
        aria-label="跟随终端目录"
        aria-pressed={follow.enabled}
        className={cn(
          "kerminal-focus-ring kerminal-pressable relative flex h-8 w-8 items-center justify-center rounded-md border transition",
          follow.enabled
            ? "border-emerald-400/45 bg-emerald-500/12 text-emerald-700 hover:bg-emerald-500/18 dark:border-emerald-300/35 dark:bg-emerald-300/12 dark:text-emerald-200 dark:hover:bg-emerald-300/18"
            : "kerminal-muted-surface text-zinc-600 hover:bg-[var(--surface-hover)] hover:text-zinc-950 dark:text-zinc-300 dark:hover:text-zinc-50",
        )}
        onClick={() => follow.setEnabled((current) => !current)}
        title={followTitle}
        type="button"
      >
        <Terminal className="h-3.5 w-3.5" />
        {follow.enabled ? (
          <span
            aria-hidden="true"
            className="absolute bottom-1 right-1 h-1.5 w-1.5 rounded-full border border-white/80 bg-emerald-500 dark:border-zinc-900"
          />
        ) : null}
      </button>
      {follow.supported ? (
        <Button
          aria-label="自动设置 SFTP 目录跟随"
          className="kerminal-focus-ring kerminal-pressable kerminal-muted-surface h-8 w-8 rounded-md border px-0 text-zinc-600 hover:bg-[var(--surface-hover)] hover:text-zinc-950 dark:text-zinc-300 dark:hover:text-zinc-50"
          disabled={follow.busy}
          onClick={() => void follow.setup()}
          size="sm"
          title="自动配置目录跟随"
          type="button"
          variant="ghost"
        >
          <Settings2
            className={cn("h-3.5 w-3.5", follow.busy && "animate-spin")}
          />
        </Button>
      ) : null}
    </div>
  );
}

function TransferAction({ actions }: Pick<SftpBrowserHeaderProps, "actions">) {
  if (actions.showLocalTransferActions) {
    return (
      <>
        <div className={dividerClassName} />
        <ToolbarButton
          ariaLabel="下载选中项目"
          disabled={actions.transferableSelectionCount === 0}
          icon={<Download className="h-3.5 w-3.5" />}
          label={
            actions.transferableSelectionCount > 1
              ? `下载 ${actions.transferableSelectionCount} 项`
              : "下载"
          }
          onClick={() => void actions.downloadSelectedEntries()}
        />
      </>
    );
  }
  if (!actions.transferTarget) return null;
  const right = actions.transferTarget.side === "right";
  return (
    <>
      <div className={dividerClassName} />
      <ToolbarButton
        ariaLabel={right ? "传到右侧" : "传到左侧"}
        disabled={actions.transferableSelectionCount === 0}
        icon={
          right ? (
            <ArrowRight className="h-3.5 w-3.5" />
          ) : (
            <ArrowLeft className="h-3.5 w-3.5" />
          )
        }
        label={right ? "传到右侧" : "传到左侧"}
        onClick={() => void actions.transferSelectedEntriesToTarget()}
      />
    </>
  );
}

/** 复用现有视图状态；菜单就地锚定，随面板缩放，退出时不抢走外部控件焦点。 */
function SftpBrowserModeMenu({
  mode,
  onModeChange,
}: {
  mode: SftpBrowserMode;
  onModeChange: Dispatch<SetStateAction<SftpBrowserMode>>;
}) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuId = useId();
  const items = [
    { icon: <List className="h-3.5 w-3.5" />, id: "list", label: "列表模式" },
    { icon: <FolderTree className="h-3.5 w-3.5" />, id: "tree", label: "树形模式" },
    {
      icon: <PanelRight className="h-3.5 w-3.5" />,
      id: "workspace",
      label: "工作区模式",
    },
  ] satisfies Array<{ icon: ReactNode; id: SftpBrowserMode; label: string }>;

  useEffect(() => {
    if (!open) return;
    rootRef.current?.querySelector<HTMLElement>('[aria-checked="true"]')?.focus();
    const closeOutside = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !rootRef.current?.contains(event.target)
      ) {
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", closeOutside);
    return () => document.removeEventListener("pointerdown", closeOutside);
  }, [open]);

  const closeMenu = () => {
    setOpen(false);
    triggerRef.current?.focus();
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (open) event.stopPropagation();
    if (event.key === "Escape" && open) {
      event.preventDefault();
      closeMenu();
      return;
    }
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    event.stopPropagation();
    if (!open) {
      setOpen(true);
      return;
    }
    const buttons = Array.from(
      rootRef.current?.querySelectorAll<HTMLElement>('[role="menuitemradio"]') ?? [],
    );
    const index = buttons.indexOf(document.activeElement as HTMLElement);
    const next =
      event.key === "Home"
        ? 0
        : event.key === "End"
          ? buttons.length - 1
          : (index + (event.key === "ArrowUp" ? -1 : 1) + buttons.length) %
            buttons.length;
    buttons[next]?.focus();
  };

  return (
    <div
      className="relative shrink-0"
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false);
      }}
      onKeyDown={handleKeyDown}
      ref={rootRef}
    >
      <button
        aria-controls={open ? menuId : undefined}
        aria-expanded={open}
        aria-haspopup="menu"
        aria-label="切换文件视图"
        className={cn(
          "kerminal-focus-ring kerminal-pressable flex h-8 w-8 items-center justify-center rounded-md border transition",
          open
            ? "border-sky-400/45 bg-[var(--surface-selected)] text-sky-700 dark:text-sky-100"
            : "kerminal-muted-surface text-zinc-600 hover:bg-[var(--surface-hover)] hover:text-zinc-950 dark:text-zinc-300 dark:hover:text-zinc-50",
        )}
        onClick={() => setOpen((current) => !current)}
        ref={triggerRef}
        title={`切换文件视图（当前：${items.find((item) => item.id === mode)?.label}）`}
        type="button"
      >
        <Ellipsis aria-hidden="true" className="h-4 w-4" />
      </button>
      {open ? (
        <div
          aria-label="文件视图"
          className="kerminal-floating-surface kerminal-layer-popover absolute right-0 top-[calc(100%+0.375rem)] w-40 rounded-[var(--radius-card)] border p-1"
          id={menuId}
          role="menu"
        >
          {items.map((item) => (
            <button
              aria-checked={mode === item.id}
              className={cn(
                "kerminal-focus-ring flex h-8 w-full items-center gap-2 rounded-[var(--radius-control)] px-2 text-left text-[13px] transition hover:bg-[var(--surface-hover)]",
                mode === item.id
                  ? "bg-[var(--surface-selected)] text-sky-700 dark:text-sky-100"
                  : "text-zinc-700 dark:text-zinc-200",
              )}
              key={item.id}
              onClick={() => {
                onModeChange(item.id);
                closeMenu();
              }}
              role="menuitemradio"
              tabIndex={-1}
              type="button"
            >
              {item.icon}
              <span className="flex-1">{item.label}</span>
              {mode === item.id ? (
                <Check aria-hidden="true" className="h-3.5 w-3.5" />
              ) : null}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}
