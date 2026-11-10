// 验收负载只通过正式双向 RPC 更新视图，报告尝试和宿主实际确认的请求数量。
// @author kongweiguang
const sessions = new Map();
const START_DELAY_MS = 8000;
// 标称速率必须高于验收目标，才能如实计入定时器抖动和最后一批确认的耗时。
const UPDATES_PER_TICK = 24;
// 脚本宿主最多 64 个待处理请求，保留一格供超时后的最终状态回执使用。
const MAX_IN_FLIGHT = 63;

/** 发送窗口与确认窗口独立计时；迟到 ACK 必须计入完成耗时，不能抬高确认吞吐。 */
function start(instance) {
  const state = sessions.get(instance);
  if (!state) return;
  const started = Date.now();
  let attempted = 0;
  let acknowledged = 0;
  let failed = 0;
  let inFlight = 0;
  let skippedDueToLimit = 0;
  let sendElapsed = 0;
  let lastSettled = started;
  const timer = setInterval(() => {
    if (sessions.get(instance) !== state) { clearInterval(timer); return; }
    sendElapsed = Date.now() - started;
    for (let i = 0; i < UPDATES_PER_TICK; i++) {
      if (inFlight >= MAX_IN_FLIGHT) { skippedDueToLimit++; continue; }
      const seq = ++attempted;
      inFlight++;
      kerminal.view.update(instance, { seq }).then(() => { acknowledged++; }, () => { failed++; }).finally(() => { inFlight--; lastSettled = Date.now(); });
    }
    if (sendElapsed >= 12000) {
      clearInterval(timer);
      const deadline = Date.now() + 3000;
      const settle = setInterval(() => {
        if (sessions.get(instance) !== state) { clearInterval(settle); return; }
        if (inFlight !== 0 && Date.now() < deadline) return;
        clearInterval(settle);
        const observed = Date.now();
        const finished = inFlight === 0 ? Math.max(started + sendElapsed, lastSettled) : observed;
        kerminal.view.update(instance, {
          done: true, completed: inFlight === 0, attempted, acknowledged, failed, in_flight: inFlight,
          skipped_due_to_in_flight_limit: skippedDueToLimit,
          send_elapsed_ms: sendElapsed, completion_elapsed_ms: finished - started,
          started_unix_ms: started, send_finished_unix_ms: started + sendElapsed,
          finished_unix_ms: finished, observed_unix_ms: observed,
        }).catch(() => console.error('Performance fixture could not deliver its completion status.'));
      }, 20);
      state.settle = settle;
      return;
    }
  }, 10);
  state.timer = timer;
}

/** 隔离副本由验收脚本按预热长度配置延迟；实际帧交集仍由脚本验证。 */
kerminal.onRequest('view.open', params => {
  const state = { timer: 0, settle: 0, delay: 0 };
  sessions.set(params.instance, state);
  state.delay = setTimeout(() => start(params.instance), START_DELAY_MS);
  return { data: { seq: 0, done: false, attempted: 0, acknowledged: 0, failed: 0, elapsed_ms: 0 } };
});

/** 关闭时撤销计时器；不存在的实例不回送成功状态。 */
kerminal.onRequest('view.close', params => {
  const state = sessions.get(params.instance);
  if (!state) throw new Error('stale instance');
  clearTimeout(state.delay); clearInterval(state.timer); clearInterval(state.settle);
  sessions.delete(params.instance);
  return { state_version: 1, state: {} };
});
kerminal.onRequest('view.snapshot', () => ({ state_version: 1, state: {} }));
