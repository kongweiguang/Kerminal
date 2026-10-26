// @author kongweiguang

import { Suspense, type ComponentProps, type CSSProperties, type RefObject } from "react";
import {
  CloseTabsConfirmationDialog,
  CloseWorkspaceFileTabsConfirmationDialog,
} from "../features/terminal/terminalTabChrome";
import type { ResolvedTheme } from "../features/settings/settingsModel";
import type { DesktopPlatform } from "../lib/desktopPlatform";
import type { WindowFrameState } from "../lib/useTauriWindowFrameState";
import type { ToolId } from "../features/workspace/types";
import { cn } from "../lib/cn";
import {
  DeleteConfirmationDialog,
  DialogLazyFallback,
  ShellResizeSeparator,
} from "./KerminalShell.helpers";
import {
  KerminalShellNotices,
  ShellResponsiveToolPanels,
  ShellToolRail,
  ShellWindowChrome,
} from "./KerminalShell.view";
import {
  LazyExternalLaunchHost,
  LazyRemoteHostCreateDialog,
  LazyRemoteHostGroupCreateDialog,
  LazySettingsDialog,
  LazySshAuthPromptHost,
  LazySshHostKeyPromptHost,
} from "./KerminalShell.static";
import {
  MachineSidebarStoreBridge,
  ToolPanelStoreBridge,
  WorkspaceTerminalSurface,
} from "./KerminalShell.workspaceBridge";
import { KerminalShellContextWorkspaceStoreBridge } from "./KerminalShell.contextWorkspace";
import { ToolRailCustomizationDialog } from "../features/tool-panel/ToolRailCustomizationDialog";
import type { ResolvedOpenToolPanels } from "../features/tool-panel";

interface ShellFrameProps {
  backgroundStyle: CSSProperties;
  density: string;
  desktopPlatform: DesktopPlatform;
  gridTemplateColumns: string;
  gridTemplateRows: string;
  language: string;
  lang: string;
  resolvedTheme: ResolvedTheme;
  windowFrameState: WindowFrameState;
  workspaceFrameRef: RefObject<HTMLDivElement | null>;
}

export interface KerminalShellLayoutProps {
  activeTool: ToolId | null;
  activeTools: readonly ToolId[];
  bottomSeparatorProps: ComponentProps<typeof ShellResizeSeparator>;
  compactShell: boolean;
  contextWorkspaceProps: ComponentProps<
    typeof KerminalShellContextWorkspaceStoreBridge
  >;
  deleteDialogProps: ComponentProps<typeof DeleteConfirmationDialog>;
  frame: ShellFrameProps;
  leftSeparatorProps: ComponentProps<typeof ShellResizeSeparator>;
  leftToolSeparatorProps: ComponentProps<typeof ShellResizeSeparator>;
  machineSidebarProps: ComponentProps<typeof MachineSidebarStoreBridge> | null;
  noticesProps: ComponentProps<typeof KerminalShellNotices>;
  remoteGroupDialogProps:
    | ComponentProps<typeof LazyRemoteHostGroupCreateDialog>
    | null;
  remoteHostDialogProps:
    | ComponentProps<typeof LazyRemoteHostCreateDialog>
    | null;
  rightSeparatorProps: ComponentProps<typeof ShellResizeSeparator>;
  settingsDialogProps: ComponentProps<typeof LazySettingsDialog> | null;
  shellWindowChromeProps: ComponentProps<typeof ShellWindowChrome>;
  tabsConfirmationProps: ComponentProps<typeof CloseTabsConfirmationDialog>;
  toolRailCustomizationProps: ComponentProps<typeof ToolRailCustomizationDialog>;
  openToolPanels: ResolvedOpenToolPanels;
  toolPanelProps: ComponentProps<typeof ToolPanelStoreBridge>;
  workspaceFileConfirmationProps: ComponentProps<
    typeof CloseWorkspaceFileTabsConfirmationDialog
  >;
  workspaceTerminalProps: ComponentProps<typeof WorkspaceTerminalSurface>;
  onActiveToolChange: ComponentProps<
    typeof ShellToolRail
  >["onActiveToolChange"];
  onOpenToolRailCustomization: () => void;
  onOpenTool: (toolId: ToolId) => void;
  onCloseToolPanel: (toolId: ToolId) => void;
}

/** 主 Shell 的纯布局层；状态、副作用和业务编排仍由 KerminalShell 持有。 */
export function KerminalShellLayout({
  activeTool,
  activeTools,
  bottomSeparatorProps,
  compactShell,
  contextWorkspaceProps,
  deleteDialogProps,
  frame,
  leftSeparatorProps,
  leftToolSeparatorProps,
  machineSidebarProps,
  noticesProps,
  onActiveToolChange,
  onOpenToolRailCustomization,
  onOpenTool,
  onCloseToolPanel,
  remoteGroupDialogProps,
  remoteHostDialogProps,
  rightSeparatorProps,
  settingsDialogProps,
  shellWindowChromeProps,
  tabsConfirmationProps,
  toolRailCustomizationProps,
  openToolPanels,
  toolPanelProps,
  workspaceFileConfirmationProps,
  workspaceTerminalProps,
}: KerminalShellLayoutProps) {
  return (
    <div
      ref={frame.workspaceFrameRef}
      className={cn(
        "relative grid h-screen overflow-hidden text-[var(--text-primary)]",
        frame.resolvedTheme === "dark" && "dark",
      )}
      data-desktop-platform={frame.desktopPlatform}
      data-density={frame.density}
      data-background-image-visible={
        workspaceTerminalProps.backgroundImageVisible || undefined
      }
      data-language={frame.language}
      data-theme={frame.resolvedTheme}
      data-window-frame={frame.windowFrameState}
      lang={frame.lang}
      style={{
        ...frame.backgroundStyle,
        gridTemplateColumns: frame.gridTemplateColumns,
        gridTemplateRows: frame.gridTemplateRows,
      }}
    >
      <ShellWindowChrome {...shellWindowChromeProps} />
      {machineSidebarProps ? (
        <div className="col-[1/2] row-[2/5] h-full overflow-hidden">
          <MachineSidebarStoreBridge {...machineSidebarProps} />
        </div>
      ) : null}
      <ShellResizeSeparator {...leftSeparatorProps} />
      <ShellResizeSeparator {...leftToolSeparatorProps} />
      <div
        className="relative z-0 h-full min-w-0 flex-1 overflow-hidden"
        style={{ gridColumn: "3 / 8", gridRow: "1 / 3" }}
      >
        <WorkspaceTerminalSurface {...workspaceTerminalProps} />
      </div>
      <ShellResizeSeparator {...rightSeparatorProps} />
      <ShellResizeSeparator {...bottomSeparatorProps} />
      <ShellResponsiveToolPanels
        activeTool={activeTool}
        activeTools={activeTools}
        compact={compactShell}
        onClose={onCloseToolPanel}
        openPanels={openToolPanels}
        rail={
          <ShellToolRail
            activeTool={activeTool}
            activeTools={activeTools}
            interfaceDensity={toolPanelProps.settings.interfaceDensity}
            onActiveToolChange={onActiveToolChange}
            onOpenToolRailCustomization={onOpenToolRailCustomization}
            onOpenTool={onOpenTool}
            toolRailSettings={toolPanelProps.settings.toolRail}
          />
        }
        renderPanel={(toolId) => (
          <ToolPanelStoreBridge
            {...toolPanelProps}
            activeTool={toolId}
            activeTools={activeTools}
            visible={activeTools.includes(toolId) && (
              !compactShell || activeTool === toolId
            )}
            showRail={false}
          />
        )}
      />
      <ToolRailCustomizationDialog {...toolRailCustomizationProps} />
      {settingsDialogProps ? (
        <Suspense fallback={<DialogLazyFallback />}>
          <LazySettingsDialog {...settingsDialogProps} />
        </Suspense>
      ) : null}
      {remoteHostDialogProps ? (
        <Suspense fallback={<DialogLazyFallback />}>
          <LazyRemoteHostCreateDialog {...remoteHostDialogProps} />
        </Suspense>
      ) : null}
      {remoteGroupDialogProps ? (
        <Suspense fallback={<DialogLazyFallback />}>
          <LazyRemoteHostGroupCreateDialog {...remoteGroupDialogProps} />
        </Suspense>
      ) : null}
      <DeleteConfirmationDialog {...deleteDialogProps} />
      <CloseTabsConfirmationDialog {...tabsConfirmationProps} />
      <CloseWorkspaceFileTabsConfirmationDialog
        {...workspaceFileConfirmationProps}
      />
      <KerminalShellNotices {...noticesProps} />
      <KerminalShellContextWorkspaceStoreBridge {...contextWorkspaceProps} />
      <Suspense fallback={null}>
        <LazySshAuthPromptHost />
        <LazySshHostKeyPromptHost />
        <LazyExternalLaunchHost />
      </Suspense>
    </div>
  );
}
