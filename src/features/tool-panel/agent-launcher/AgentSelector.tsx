// @author kongweiguang

import { createPortal } from "react-dom";
import {
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type RefObject,
} from "react";
import {
  Check,
  ChevronDown,
  Loader2,
  Pi,
  Plus,
  Sparkles,
  Terminal,
  Wrench,
} from "lucide-react";
import { cn } from "../../../lib/cn";
import type { ExternalAgentId } from "../../../lib/agentLauncherApi";

export type AgentSelectorTone = "ready" | "warning" | "muted";

export interface AgentSelectorOption {
  agentId: ExternalAgentId;
  commandLabel?: string;
  /** 完整配置命令仅用于提示，列表仍用短命令避免撑宽。 */
  launchCommand?: string;
  disabled?: boolean;
  disabledReason?: string;
  key: string;
  name: string;
  statusDetail?: string;
  statusLabel: string;
  tone?: AgentSelectorTone;
}

interface AgentSelectorProps {
  actionState: string | null;
  active?: boolean;
  disabled?: boolean;
  onManageCustomAgents: () => void;
  onSelect: (key: string) => void;
  options: AgentSelectorOption[];
  selectedKey: string;
  triggerRef?: RefObject<HTMLButtonElement | null>;
}

interface AgentSelectorPopoverLayout {
  left: number;
  maxHeight: number;
  side: "bottom" | "top";
  top: number;
  width: number;
}

const VIEWPORT_INSET = 8;
const POPOVER_GAP = 6;
const MIN_POPOVER_HEIGHT = 152;
const MAX_POPOVER_HEIGHT = 320;
const MIN_POPOVER_WIDTH = 240;
const MAX_POPOVER_WIDTH = 360;
const ADD_ENTRY_ID_SUFFIX = "add-custom";

const agentIcons = {
  claude: Sparkles,
  codex: Terminal,
  pi: Pi,
  custom: Wrench,
} satisfies Record<ExternalAgentId, typeof Terminal>;

/**
 * Agent 专用选择器保留内置 Agent 的稳定顺序，并把自定义条目按上游保存顺序接在其后。
 * 浮层通过 body portal 脱离工具面板的滚动裁切，适配左右栏、底栏和浮窗布局。
 */
export function AgentSelector({
  actionState,
  active = true,
  disabled = false,
  onManageCustomAgents,
  onSelect,
  options,
  selectedKey,
  triggerRef: providedTriggerRef,
}: AgentSelectorProps) {
  const listboxId = useId();
  const internalTriggerRef = useRef<HTMLButtonElement>(null);
  const triggerRef = providedTriggerRef ?? internalTriggerRef;
  const popoverRef = useRef<HTMLDivElement>(null);
  const addButtonRef = useRef<HTMLButtonElement>(null);
  const orderedOptions = useMemo(() => orderAgentOptions(options), [options]);
  const selectedIndex = orderedOptions.findIndex(
    (option) => option.key === selectedKey,
  );
  const selectedOption =
    orderedOptions[selectedIndex] ?? orderedOptions[0] ?? null;
  const [open, setOpen] = useState(false);
  const [highlightedIndex, setHighlightedIndex] = useState(() =>
    Math.max(selectedIndex, 0),
  );
  const [popoverLayout, setPopoverLayout] =
    useState<AgentSelectorPopoverLayout | null>(null);
  const interactionDisabled = !active || disabled || actionState !== null;

  useLayoutEffect(() => {
    if (interactionDisabled) {
      setOpen(false);
      setPopoverLayout(null);
    }
  }, [interactionDisabled]);

  useEffect(() => {
    if (!open) {
      setHighlightedIndex(Math.max(selectedIndex, 0));
      return undefined;
    }

    const closeOnPointerDown = (event: PointerEvent) => {
      const target = event.target as Node;
      if (
        !triggerRef.current?.contains(target) &&
        !popoverRef.current?.contains(target)
      ) {
        setOpen(false);
      }
    };
    /**
     * Escape 在捕获阶段归 Agent 浮层独占；否则事件继续冒泡到工作台快捷键，
     * 会把整个 Agent Launcher 收起并让 body portal 脱离可见面板生命周期。
     */
    const closeOnEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") {
        return;
      }
      event.preventDefault();
      event.stopImmediatePropagation();
      setOpen(false);
      window.requestAnimationFrame(() => triggerRef.current?.focus());
    };

    window.addEventListener("pointerdown", closeOnPointerDown);
    window.addEventListener("keydown", closeOnEscape, true);
    return () => {
      window.removeEventListener("pointerdown", closeOnPointerDown);
      window.removeEventListener("keydown", closeOnEscape, true);
    };
  }, [open, selectedIndex, triggerRef]);

  useLayoutEffect(() => {
    if (!open) {
      setPopoverLayout(null);
      return undefined;
    }

    /** 根据实时视口选择上下展开方向，避免工具面板停靠位置改变后浮层被窗口边缘裁切。 */
    const updatePopoverLayout = () => {
      const trigger = triggerRef.current;
      if (!trigger) {
        return;
      }
      setPopoverLayout(resolvePopoverLayout(trigger.getBoundingClientRect()));
    };

    updatePopoverLayout();
    window.addEventListener("resize", updatePopoverLayout);
    window.addEventListener("scroll", updatePopoverLayout, true);
    return () => {
      window.removeEventListener("resize", updatePopoverLayout);
      window.removeEventListener("scroll", updatePopoverLayout, true);
    };
  }, [open, triggerRef]);

  /** 跳过不可用条目循环移动高亮，确保方向键不会落在无法选择的 Agent 上。 */
  const moveHighlight = (direction: 1 | -1) => {
    if (orderedOptions.length === 0) {
      return;
    }
    let nextIndex = highlightedIndex;
    for (let attempt = 0; attempt < orderedOptions.length; attempt += 1) {
      nextIndex =
        (nextIndex + direction + orderedOptions.length) % orderedOptions.length;
      if (!orderedOptions[nextIndex]?.disabled) {
        setHighlightedIndex(nextIndex);
        return;
      }
    }
  };

  /** 选择只更新当前 Agent，不触发启动；启动动作由旁边的分裂按钮显式完成。 */
  const selectOption = (option: AgentSelectorOption) => {
    if (option.disabled || interactionDisabled) {
      return;
    }
    onSelect(option.key);
    setOpen(false);
    window.requestAnimationFrame(() => triggerRef.current?.focus());
  };

  /** 组合框键盘交互保持焦点在触发器，Tab 则进入固定的“添加”入口。 */
  const handleTriggerKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (interactionDisabled) {
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (!open) {
        setOpen(true);
        setHighlightedIndex(Math.max(selectedIndex, 0));
        return;
      }
      moveHighlight(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      if (!open) {
        setOpen(true);
        return;
      }
      const highlightedOption = orderedOptions[highlightedIndex];
      if (highlightedOption) {
        selectOption(highlightedOption);
      }
      return;
    }
    if (event.key === "Tab" && open && !event.shiftKey) {
      event.preventDefault();
      addButtonRef.current?.focus();
    }
  };

  const SelectedIcon = selectedOption
    ? agentIcons[selectedOption.agentId]
    : Terminal;
  const selectedBusy = selectedOption?.key === actionState;

  return (
    <>
      <button
        aria-activedescendant={
          open && orderedOptions[highlightedIndex]
            ? agentOptionId(listboxId, orderedOptions[highlightedIndex].key)
            : undefined
        }
        aria-controls={open ? listboxId : undefined}
        aria-expanded={open}
        aria-haspopup="listbox"
        aria-label="选择 Agent"
        aria-disabled={interactionDisabled || undefined}
        aria-valuetext={selectedOption?.name ?? "未选择"}
        className="kerminal-focus-ring kerminal-pressable flex h-11 min-w-0 flex-1 items-center gap-2.5 rounded-l-[var(--radius-control)] border border-[var(--border-subtle)] bg-[var(--surface-field)] px-3 text-left text-[var(--text-primary)] transition hover:bg-[var(--surface-field-hover)] aria-disabled:cursor-not-allowed aria-disabled:opacity-55 aria-disabled:hover:bg-[var(--surface-field)]"
        onClick={() => {
          if (!interactionDisabled) {
            setOpen((current) => !current);
          }
        }}
        onKeyDown={handleTriggerKeyDown}
        ref={triggerRef}
        role="combobox"
        type="button"
      >
        <span className="grid h-7 w-7 shrink-0 place-items-center rounded-lg bg-[var(--surface-solid)] text-[rgb(var(--app-accent))] shadow-sm shadow-black/5 dark:shadow-black/20">
          {selectedBusy ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <SelectedIcon className="h-4 w-4" strokeWidth={1.8} />
          )}
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px] font-semibold leading-4">
            {selectedOption?.name ?? "选择 Agent"}
          </span>
          <span className="mt-0.5 flex min-w-0 items-center gap-1.5 text-[10px] leading-3.5 text-[var(--text-tertiary)]">
            <span
              className={cn(
                "h-1.5 w-1.5 shrink-0 rounded-full",
                agentToneDotClassName(selectedOption?.tone),
              )}
            />
            <span className="truncate">
              {selectedOption
                ? selectedOption.commandLabel
                  ? `${selectedOption.statusLabel} · ${selectedOption.commandLabel}`
                  : selectedOption.statusLabel
                : "暂无可用 Agent"}
            </span>
          </span>
        </span>
        <ChevronDown
          aria-hidden="true"
          className={cn(
            "h-4 w-4 shrink-0 text-[var(--text-tertiary)] transition-transform duration-150",
            open ? "rotate-180" : "",
          )}
          strokeWidth={1.8}
        />
      </button>

      {active && !interactionDisabled && open && popoverLayout
        ? createPortal(
            <div
              className="kerminal-floating-surface kerminal-floating-enter kerminal-layer-popover fixed flex overflow-hidden rounded-[var(--radius-card)] border text-[13px] text-[var(--text-primary)]"
              data-side={popoverLayout.side}
              ref={popoverRef}
              style={{
                flexDirection: "column",
                left: popoverLayout.left,
                maxHeight: popoverLayout.maxHeight,
                top: popoverLayout.top,
                width: popoverLayout.width,
              }}
            >
              <div
                className="scrollbar-none min-h-0 flex-1 overflow-y-auto p-1.5"
                id={listboxId}
                role="listbox"
              >
                {orderedOptions.map((option, index) => {
                  const Icon = agentIcons[option.agentId];
                  const selected = option.key === selectedKey;
                  const highlighted = index === highlightedIndex;
                  const busy = option.key === actionState;
                  return (
                    <button
                      aria-label={`${option.name}，${option.statusLabel}`}
                      aria-selected={selected}
                      className={cn(
                        "flex w-full min-w-0 items-center gap-2.5 rounded-[var(--radius-control)] px-2.5 py-2 text-left transition-colors duration-150",
                        highlighted || selected
                          ? "bg-[var(--surface-selected)] text-[rgb(var(--app-accent))]"
                          : "text-[var(--text-secondary)] hover:bg-[var(--surface-hover)] hover:text-[var(--text-primary)]",
                        option.disabled &&
                          "cursor-not-allowed opacity-45 hover:bg-transparent",
                      )}
                      disabled={option.disabled}
                      id={agentOptionId(listboxId, option.key)}
                      key={option.key}
                      onClick={() => selectOption(option)}
                      onMouseEnter={() => setHighlightedIndex(index)}
                      role="option"
                      tabIndex={-1}
                      title={option.disabledReason ?? option.statusDetail}
                      type="button"
                    >
                      <span className="grid h-8 w-8 shrink-0 place-items-center rounded-lg bg-[var(--surface-field)]">
                        {busy ? (
                          <Loader2 className="h-4 w-4 animate-spin" />
                        ) : (
                          <Icon className="h-4 w-4" strokeWidth={1.8} />
                        )}
                      </span>
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-[13px] font-semibold leading-4 text-[var(--text-primary)]">
                          {option.name}
                        </span>
                        <span className="mt-0.5 flex min-w-0 items-center gap-1.5 text-[10px] leading-3.5 text-[var(--text-tertiary)]">
                          <span
                            className={cn(
                              "h-1.5 w-1.5 shrink-0 rounded-full",
                              agentToneDotClassName(option.tone),
                            )}
                          />
                          <span className="truncate">
                            {option.commandLabel
                              ? `${option.statusLabel} · ${option.commandLabel}`
                              : option.statusLabel}
                          </span>
                        </span>
                      </span>
                      {selected ? (
                        <Check
                          aria-hidden="true"
                          className="h-4 w-4 shrink-0"
                          strokeWidth={1.9}
                        />
                      ) : null}
                    </button>
                  );
                })}
              </div>
              <div className="shrink-0 border-t border-[var(--border-subtle)] bg-[var(--surface-overlay)] p-1.5">
                <button
                  className="kerminal-focus-ring flex h-9 w-full items-center gap-2 rounded-[var(--radius-control)] px-2.5 text-left text-xs font-semibold text-[rgb(var(--app-accent))] transition hover:bg-[var(--surface-hover)]"
                  id={`${listboxId}-${ADD_ENTRY_ID_SUFFIX}`}
                  onClick={() => {
                    setOpen(false);
                    onManageCustomAgents();
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "ArrowUp") {
                      event.preventDefault();
                      setHighlightedIndex(
                        Math.max(orderedOptions.length - 1, 0),
                      );
                      triggerRef.current?.focus();
                    } else if (event.key === "Escape") {
                      event.preventDefault();
                      setOpen(false);
                      triggerRef.current?.focus();
                    } else if (event.key === "Tab") {
                      window.setTimeout(() => setOpen(false), 0);
                    }
                  }}
                  ref={addButtonRef}
                  type="button"
                >
                  <Plus className="h-4 w-4 shrink-0" strokeWidth={1.9} />
                  <span className="truncate">添加自定义 Agent</span>
                </button>
              </div>
            </div>,
            document.body,
          )
        : null}
    </>
  );
}

/** 内置 Agent 固定为 Codex、Claude、PI，自定义 Agent 保留持久化数组的添加顺序。 */
function orderAgentOptions(options: AgentSelectorOption[]) {
  const rank: Record<ExternalAgentId, number> = {
    codex: 0,
    claude: 1,
    pi: 2,
    custom: 3,
  };
  return options
    .map((option, index) => ({ index, option }))
    .sort(
      (left, right) =>
        rank[left.option.agentId] - rank[right.option.agentId] ||
        left.index - right.index,
    )
    .map(({ option }) => option);
}

/** 根据视口可用空间选择展开侧并限制尺寸，避免任何停靠方向下越过窗口边缘。 */
function resolvePopoverLayout(rect: DOMRect): AgentSelectorPopoverLayout {
  const viewportWidth = Math.max(window.innerWidth, MIN_POPOVER_WIDTH);
  const viewportHeight = Math.max(window.innerHeight, MIN_POPOVER_HEIGHT);
  const width = Math.min(
    MAX_POPOVER_WIDTH,
    Math.max(MIN_POPOVER_WIDTH, rect.width),
    viewportWidth - VIEWPORT_INSET * 2,
  );
  const availableBelow = viewportHeight - rect.bottom - POPOVER_GAP - VIEWPORT_INSET;
  const availableAbove = rect.top - POPOVER_GAP - VIEWPORT_INSET;
  const side =
    availableBelow >= MIN_POPOVER_HEIGHT || availableBelow >= availableAbove
      ? "bottom"
      : "top";
  const availableHeight = side === "bottom" ? availableBelow : availableAbove;
  const maxHeight = Math.max(
    Math.min(MIN_POPOVER_HEIGHT, viewportHeight - VIEWPORT_INSET * 2),
    Math.min(MAX_POPOVER_HEIGHT, availableHeight),
  );
  const left = Math.min(
    Math.max(VIEWPORT_INSET, rect.left),
    Math.max(VIEWPORT_INSET, viewportWidth - width - VIEWPORT_INSET),
  );
  const top =
    side === "bottom"
      ? Math.min(rect.bottom + POPOVER_GAP, viewportHeight - maxHeight - VIEWPORT_INSET)
      : Math.max(VIEWPORT_INSET, rect.top - POPOVER_GAP - maxHeight);
  return { left, maxHeight, side, top, width };
}

/** 将业务状态映射为小尺寸状态点，避免依赖文字颜色传递唯一信息。 */
function agentToneDotClassName(tone: AgentSelectorTone | undefined) {
  if (tone === "warning") {
    return "bg-amber-500";
  }
  if (tone === "muted") {
    return "bg-zinc-400 dark:bg-zinc-500";
  }
  return "bg-emerald-500";
}

/** 为带特殊字符的持久化 key 生成稳定且合法的 aria-activedescendant 目标。 */
function agentOptionId(listboxId: string, optionKey: string) {
  const safeKey = optionKey.replace(/[^a-zA-Z0-9_-]/g, "-");
  return `${listboxId}-option-${safeKey}`;
}
