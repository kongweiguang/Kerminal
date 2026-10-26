// @author kongweiguang

import { useCallback, useRef, useState } from "react";
import {
  buildUserFacingError,
  type UserFacingMessage,
} from "../../../lib/userFacingMessage";
import type { AgentLauncherActionState } from "./AgentLauncherView";

/**
 * 串行维护 launcherKey 级操作状态与稳定错误文案，让多个 Custom 的 loading
 * 不会共享 provider 级标识；同步锁覆盖 React 提交前的重复点击，异常统一收口。
 */
export function useAgentLauncherAction() {
  const [actionState, setActionState] =
    useState<AgentLauncherActionState>(null);
  const pendingRef = useRef(false);
  const [actionError, setActionError] = useState<UserFacingMessage | null>(null);

  const runAction = useCallback(
    async (nextAction: string, action: () => Promise<void>) => {
      if (pendingRef.current) {
        return;
      }
      pendingRef.current = true;
      setActionState(nextAction);
      setActionError(null);
      try {
        await action();
      } catch (error) {
        setActionError(
          buildUserFacingError(error, {
            recoveryAction: "请检查目标终端和 Agent 配置后重试。",
            title: "Agent 操作未完成",
          }),
        );
      } finally {
        pendingRef.current = false;
        setActionState(null);
      }
    },
    [],
  );

  return { actionError, actionState, runAction, setActionError };
}
