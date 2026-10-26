// @author kongweiguang

import { useCallback, useEffect, useMemo, useState } from "react";
import { isBroadcastCommandTargetMode } from "./broadcastCommandPolicy";
import type { TerminalPane } from "../workspace/contracts/index";
import {
  createBroadcastTargetOption,
  filterBroadcastTargetsByPaneIds,
  resolveBroadcastTargetPaneIds,
  type BroadcastTargetMode,
  type BroadcastTargetOption,
} from "./terminalBroadcastTargets";

interface UseTerminalBroadcastTargetsOptions {
  activePaneIds: string[];
  focusedPaneId: string;
  panesById: Map<string, TerminalPane>;
}

interface UseTerminalBroadcastTargetsResult {
  broadcastTargets: BroadcastTargetOption[];
  broadcastTargetMode: BroadcastTargetMode;
  broadcastTargetOptions: BroadcastTargetOption[];
  handleBroadcastTargetModeChange: (mode: BroadcastTargetMode) => void;
  handleToggleCustomTarget: (paneId: string, selected: boolean) => void;
  selectedTargetPaneIds: string[];
}

export function useTerminalBroadcastTargets({
  activePaneIds,
  focusedPaneId,
  panesById,
}: UseTerminalBroadcastTargetsOptions): UseTerminalBroadcastTargetsResult {
  const broadcastTargetOptions = useMemo(
    () =>
      activePaneIds.flatMap((paneId) => {
        const pane = panesById.get(paneId);
        if (!pane || !isBroadcastCommandTargetMode(pane.mode)) {
          return [];
        }
        return [createBroadcastTargetOption(pane)];
      }),
    [activePaneIds, panesById],
  );
  const [broadcastTargetMode, setBroadcastTargetMode] =
    useState<BroadcastTargetMode>("all");
  const [customBroadcastTargetPaneIds, setCustomBroadcastTargetPaneIds] =
    useState<string[]>([]);
  const selectedTargetPaneIds = useMemo(
    () =>
      resolveBroadcastTargetPaneIds(
        broadcastTargetMode,
        broadcastTargetOptions,
        focusedPaneId,
        customBroadcastTargetPaneIds,
      ),
    [
      broadcastTargetMode,
      broadcastTargetOptions,
      customBroadcastTargetPaneIds,
      focusedPaneId,
    ],
  );
  const broadcastTargets = useMemo(
    () =>
      filterBroadcastTargetsByPaneIds(
        broadcastTargetOptions,
        selectedTargetPaneIds,
      ),
    [broadcastTargetOptions, selectedTargetPaneIds],
  );
  useEffect(() => {
    const validTargetPaneIds = new Set(
      broadcastTargetOptions.map((target) => target.paneId),
    );
    setCustomBroadcastTargetPaneIds((current) => {
      const next = current.filter((paneId) => validTargetPaneIds.has(paneId));
      if (next.length === current.length) {
        return current;
      }
      return next;
    });
  }, [broadcastTargetOptions]);

  const handleBroadcastTargetModeChange = useCallback(
    (mode: BroadcastTargetMode) => {
      setBroadcastTargetMode(mode);
      if (mode !== "custom") {
        return;
      }
      setCustomBroadcastTargetPaneIds((current) =>
        current.length > 0 ? current : selectedTargetPaneIds,
      );
    },
    [selectedTargetPaneIds],
  );
  const handleToggleCustomTarget = useCallback(
    (paneId: string, selected: boolean) => {
      setBroadcastTargetMode("custom");
      setCustomBroadcastTargetPaneIds(() => {
        const nextSelected = new Set(selectedTargetPaneIds);
        if (selected) {
          nextSelected.add(paneId);
        } else {
          nextSelected.delete(paneId);
        }
        return broadcastTargetOptions
          .map((target) => target.paneId)
          .filter((targetPaneId) => nextSelected.has(targetPaneId));
      });
    },
    [broadcastTargetOptions, selectedTargetPaneIds],
  );

  return {
    broadcastTargets,
    broadcastTargetMode,
    broadcastTargetOptions,
    handleBroadcastTargetModeChange,
    handleToggleCustomTarget,
    selectedTargetPaneIds,
  };
}
