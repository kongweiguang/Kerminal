/**
 * Host-scoped container inspector panel.
 *
 * @author kongweiguang
 */

import { RefreshCw } from "lucide-react";
import { Button } from "../../../components/ui/button";
import { cn } from "../../../lib/cn";
import type {
  DockerContainerInspectSummary,
  DockerContainerStatsResult,
} from "../../../lib/dockerApi";
import type {
  HostContainerInspectorTab,
  HostContainerMetadata,
} from "./hostContainerDialogModel";

const inspectorTabs: Array<{
  id: HostContainerInspectorTab;
  label: string;
}> = [
  { id: "details", label: "详情" },
  { id: "stats", label: "监控" },
];

const inspectorIconButtonClassName =
  "h-8 w-8 rounded-lg text-zinc-500 hover:text-zinc-950 dark:text-zinc-400 dark:hover:text-zinc-50";
const inspectorPanelClassName =
  "grid h-full min-h-0 min-w-0 grid-rows-[auto_minmax(0,1fr)] overflow-hidden rounded-[var(--radius-card)] border border-[var(--border-subtle)] bg-[var(--surface-content)] p-3";
const embeddedInspectorPanelClassName =
  "grid h-full min-h-0 min-w-0 grid-rows-[auto_minmax(0,1fr)] overflow-hidden";
const inspectorHeaderClassName =
  "grid h-[5.75rem] grid-rows-[minmax(0,2.25rem)_2rem] gap-3 border-b border-[var(--border-subtle)] pb-3";
const embeddedInspectorHeaderClassName =
  "flex min-h-9 min-w-0 items-center gap-2 border-b border-[var(--border-subtle)] pb-2";
const inspectorTabsClassName =
  "grid h-8 w-full max-w-[22rem] justify-self-end rounded-xl bg-black/5 p-0.5 dark:bg-white/10";
const inspectorBodyClassName =
  "min-h-0 overflow-y-auto pt-3 [scrollbar-gutter:stable]";

/** Embedded mode removes duplicate card chrome while the standalone dialog keeps its framed layout. */
export function HostContainerInspector({
  container,
  embedded = false,
  error,
  inspectSummary,
  loading,
  onRefresh,
  onTabChange,
  statsResult,
  tab,
}: {
  container?: HostContainerMetadata;
  embedded?: boolean;
  error: string | null;
  inspectSummary: DockerContainerInspectSummary | null;
  loading: boolean;
  onRefresh: () => void;
  onTabChange: (tab: HostContainerInspectorTab) => void;
  statsResult: DockerContainerStatsResult | null;
  tab: HostContainerInspectorTab;
}) {
  const refreshButton = (
    <Button
      aria-label="刷新容器信息"
      className={inspectorIconButtonClassName}
      disabled={!container || loading}
      onClick={onRefresh}
      size="icon"
      title="刷新容器信息"
      type="button"
      variant="ghost"
    >
      <RefreshCw className={cn("h-4 w-4", loading && "animate-spin")} />
    </Button>
  );
  const tabs = (
    <div
      aria-label="容器信息视图"
      className={cn(inspectorTabsClassName, "grid-cols-2")}
      role="tablist"
    >
      {inspectorTabs.map((item) => (
        <button
          aria-selected={tab === item.id}
          className={cn(
            "kerminal-focus-ring h-7 rounded-lg text-xs font-medium transition",
            tab === item.id
              ? "bg-white text-zinc-950 shadow-sm dark:bg-zinc-900 dark:text-zinc-50"
              : "text-zinc-500 hover:text-zinc-800 dark:text-zinc-400 dark:hover:text-zinc-100",
          )}
          key={item.id}
          onClick={() => onTabChange(item.id)}
          role="tab"
          type="button"
        >
          {item.label}
        </button>
      ))}
    </div>
  );

  return (
    <aside
      className={
        embedded ? embeddedInspectorPanelClassName : inspectorPanelClassName
      }
    >
      {embedded ? (
        <div className={embeddedInspectorHeaderClassName}>
          <div className="flex min-w-0 flex-1 items-center justify-end gap-2">
            <div className="min-w-0 flex-1">{tabs}</div>
            {refreshButton}
          </div>
        </div>
      ) : (
        <div className={inspectorHeaderClassName}>
          <div className="flex min-w-0 items-center justify-between gap-2">
            <div className="min-w-0">
              <div className="truncate text-sm font-semibold text-zinc-950 dark:text-zinc-50">
                {container?.name ?? "容器信息"}
              </div>
              <div className="truncate font-mono text-[11px] text-zinc-500 dark:text-zinc-400">
                {container?.shortId ?? "未选择"}
              </div>
            </div>
            {refreshButton}
          </div>
          <div className="flex min-w-0 justify-end">{tabs}</div>
        </div>
      )}
      <div className={cn(inspectorBodyClassName, "min-w-0")}>
        {!container ? (
          <StateMessage>选择容器后查看详情。</StateMessage>
        ) : error ? (
          <StateMessage tone="danger">{error}</StateMessage>
        ) : loading &&
          !hasInspectorData(tab, inspectSummary, statsResult) ? (
          <StateMessage>正在读取容器信息...</StateMessage>
        ) : tab === "details" ? (
          <InspectDetailsPanel
            container={container}
            embedded={embedded}
            summary={inspectSummary}
          />
        ) : (
          <InspectStatsPanel stats={statsResult} />
        )}
      </div>
    </aside>
  );
}

/** Detail fields wrap only in the narrow embedded panel so long IDs and commands do not widen the dock. */
function InspectDetailsPanel({
  container,
  embedded = false,
  summary,
}: {
  container: HostContainerMetadata;
  embedded?: boolean;
  summary: DockerContainerInspectSummary | null;
}) {
  const ports = summary?.ports.length ? summary.ports : container.ports;
  const labels = summary ? Object.entries(summary.labels).slice(0, 6) : [];
  return (
    <div className="grid min-w-0 gap-3 text-xs">
      <div className="grid min-w-0 gap-1.5">
        <InspectorField label="镜像" value={summary?.image ?? container.image} wrap={embedded} />
        <InspectorField
          label="状态"
          value={summary?.status ?? container.statusText}
          wrap={embedded}
        />
        <InspectorField label="运行时" value={container.runtime} wrap={embedded} />
        <InspectorField label="完整 ID" mono value={summary?.id ?? container.id} wrap={embedded} />
        <InspectorField label="工作目录" value={summary?.workingDir ?? "-"} wrap={embedded} />
        <InspectorField label="用户" value={summary?.user ?? "-"} wrap={embedded} />
      </div>
      <InspectorList label="端口" values={ports} wrap={embedded} />
      <InspectorList label="网络" values={summary?.networks ?? []} wrap={embedded} />
      <InspectorList
        label="命令"
        values={[...(summary?.entrypoint ?? []), ...(summary?.command ?? [])]}
        wrap={embedded}
      />
      {labels.length ? (
        <div className="grid min-w-0 gap-1.5">
          <div className="text-[11px] font-medium text-zinc-500 dark:text-zinc-400">
            Labels
          </div>
          <div className="grid min-w-0 gap-1">
            {labels.map(([key, value]) => (
              <div
                className={cn(
                  "min-w-0 rounded-lg bg-black/5 px-2 py-1 font-mono text-[11px] text-zinc-600 dark:bg-white/10 dark:text-zinc-300",
                  embedded ? "whitespace-normal break-all" : "truncate",
                )}
                key={key}
                title={`${key}=${value}`}
              >
                <span className={embedded ? "break-all" : "truncate"}>{key}</span>
                <span className="text-zinc-400"> = </span>
                <span className={embedded ? "break-all" : "truncate"}>{value}</span>
              </div>
            ))}
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** Keep raw stats JSON horizontally scrollable on its own instead of letting it widen the inspector. */
function InspectStatsPanel({
  stats,
}: {
  stats: DockerContainerStatsResult | null;
}) {
  return (
    <div className="grid min-w-0 gap-2 text-xs">
      <div className="grid grid-cols-2 gap-2">
        <StatsMetric label="CPU" value={stats?.cpuPercent} />
        <StatsMetric label="内存" value={stats?.memoryPercent} />
        <StatsMetric label="PIDs" value={stats?.pids} />
        <StatsMetric label="网络" value={stats?.networkIo} />
      </div>
      <InspectorField label="内存使用" value={stats?.memoryUsage ?? "-"} />
      <InspectorField label="Block IO" value={stats?.blockIo ?? "-"} />
      <pre className="max-h-28 max-w-full overflow-x-auto overflow-y-auto whitespace-pre rounded-xl bg-black/5 p-2 font-mono text-[11px] text-zinc-500 dark:bg-white/10 dark:text-zinc-400">
        {stats?.raw || "暂无 stats 数据。"}
      </pre>
    </div>
  );
}

/** Long embedded values may wrap; standalone values retain their compact single-line presentation. */
function InspectorField({
  label,
  mono = false,
  value,
  wrap = false,
}: {
  label: string;
  mono?: boolean;
  value: string;
  wrap?: boolean;
}) {
  return (
    <div className="grid min-w-0 gap-0.5">
      <span className="text-[11px] font-medium text-zinc-500 dark:text-zinc-400">
        {label}
      </span>
      <span
        className={cn(
          "min-w-0 text-zinc-800 dark:text-zinc-200",
          wrap ? "whitespace-normal break-all" : "truncate",
          mono && "font-mono text-[11px]",
        )}
        title={value}
      >
        {value || "-"}
      </span>
    </div>
  );
}

/** Container metadata wraps within the embedded width while preserving compact chips in the standalone dialog. */
function InspectorList({
  label,
  values,
  wrap = false,
}: {
  label: string;
  values: string[];
  wrap?: boolean;
}) {
  return (
    <div className="grid min-w-0 gap-1.5">
      <div className="text-[11px] font-medium text-zinc-500 dark:text-zinc-400">
        {label}
      </div>
      <div className="flex min-w-0 flex-wrap gap-1">
        {values.length ? (
          values.map((value) => (
            <span
              className={cn(
                "inline-flex max-w-full rounded-lg bg-black/5 px-2 py-1 font-mono text-[11px] text-zinc-600 dark:bg-white/10 dark:text-zinc-300",
                wrap ? "whitespace-normal break-all" : "truncate",
              )}
              key={value}
              title={value}
            >
              {value}
            </span>
          ))
        ) : (
          <span className="text-zinc-400 dark:text-zinc-500">-</span>
        )}
      </div>
    </div>
  );
}

function StatsMetric({ label, value }: { label: string; value?: string }) {
  return (
    <div className="rounded-xl bg-black/5 px-2.5 py-2 dark:bg-white/10">
      <div className="text-[11px] text-zinc-500 dark:text-zinc-400">
        {label}
      </div>
      <div className="truncate font-mono text-sm font-semibold text-zinc-900 dark:text-zinc-100">
        {value ?? "-"}
      </div>
    </div>
  );
}

function StateMessage({
  children,
  tone = "muted",
}: {
  children: string;
  tone?: "danger" | "muted";
}) {
  return (
    <div
      className={cn(
        "flex min-h-32 items-center justify-center rounded-[var(--radius-card)] border px-4 py-8 text-center text-sm",
        tone === "danger"
          ? "border-red-500/20 bg-red-500/10 text-red-700 dark:text-red-200"
          : "border-dashed border-[var(--border-subtle)] text-zinc-500 dark:text-zinc-400",
      )}
    >
      {children}
    </div>
  );
}

function hasInspectorData(
  tab: HostContainerInspectorTab,
  inspectSummary: DockerContainerInspectSummary | null,
  statsResult: DockerContainerStatsResult | null,
) {
  if (tab === "details") {
    return Boolean(inspectSummary);
  }
  return Boolean(statsResult);
}
