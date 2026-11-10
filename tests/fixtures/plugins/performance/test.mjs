// 夹具测试只验证计数和停止逻辑；真实吞吐必须来自独立桌面运行记录。
// @author kongweiguang
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const source = await readFile(new URL('./main.js', import.meta.url), 'utf8');

/** 每轮隔离时钟与 RPC 回执，复现迟到确认和最终状态投递失败。 */
function harness(delayed, rejectCompletion = false) {
  const handlers = new Map();
  const timers = new Map();
  const patches = [];
  const pending = [];
  const errors = [];
  let outstanding = 0;
  let next = 0;
  let now = 0;
  const context = vm.createContext({
    kerminal: {
      onRequest: (name, handler) => handlers.set(name, handler),
      view: { update: (_instance, patch) => {
        // 与真实脚本宿主相同的上限，确保测试能发现最终状态因请求槽耗尽而无法送达。
        if (outstanding >= 64) return Promise.reject(new Error('overloaded'));
        if (rejectCompletion && patch.done) return Promise.reject(new Error('delivery failed'));
        patches.push(patch);
        if (delayed && !patch.done) {
          outstanding++;
          return new Promise(resolve => pending.push(() => { outstanding--; resolve(); }));
        }
        return Promise.resolve();
      } },
    },
    console: { error: message => errors.push(message) },
    Date: { now: () => now },
    setTimeout: callback => { const id = ++next; timers.set(id, callback); return id; },
    setInterval: callback => { const id = ++next; timers.set(id, callback); return id; },
    clearTimeout: id => timers.delete(id),
    clearInterval: id => timers.delete(id),
  });
  vm.runInContext(source, context);
  handlers.get('view.open')({ instance: 'test' });
  const delay = [...timers.entries()][0];
  timers.delete(delay[0]);
  delay[1]();
  return {
    timers, patches, pending, errors,
    setNow: value => { now = value; },
    close: () => handlers.get('view.close')({ instance: 'test' }),
  };
}

/** 显式清空 Promise 链，避免测试把 JS 微任务延迟混入业务确认时间。 */
async function flush() {
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
}

/** 只推进发送计时器，模拟抖动并在真实移除后停止，避免测试人为补发。 */
async function send(load, intervalMs = 10) {
  const [id, interval] = [...load.timers.entries()][0];
  let elapsed = 0;
  while (load.timers.has(id)) {
    elapsed += intervalMs;
    assert.ok(elapsed <= 12000 + intervalMs, 'sending timer did not stop');
    load.setNow(elapsed);
    interval();
    await flush();
  }
  return elapsed;
}

for (const delayed of [false, true]) {
  const load = harness(delayed);
  await send(load);
  if (delayed) {
    load.setNow(14500);
    for (const acknowledge of load.pending) acknowledge();
    await flush();
  }
  load.setNow(delayed ? 14520 : 12020);
  [...load.timers.values()][0]();
  await flush();
  const report = load.patches.at(-1);
  assert.equal(report.done, true);
  assert.equal(report.completed, true);
  const expectedAttempts = delayed ? 63 : 28800;
  assert.equal(report.attempted, expectedAttempts);
  assert.equal(report.acknowledged, expectedAttempts);
  assert.equal(report.failed, 0);
  assert.equal(report.in_flight, 0);
  assert.equal(report.skipped_due_to_in_flight_limit, delayed ? 28800 - 63 : 0);
  assert.equal(report.send_elapsed_ms, 12000);
  assert.equal(report.send_finished_unix_ms, 12000);
  assert.equal(report.completion_elapsed_ms, delayed ? 14500 : 12000);
  assert.equal(report.finished_unix_ms, delayed ? 14500 : 12000);
  assert.equal(report.acknowledged * 1000 / report.completion_elapsed_ms < 1000, delayed);
  load.close();
  assert.equal(load.timers.size, 0);
}

// 轻度抖动不应让标称目标必然失败；严重抖动则必须如实留下不足 1000/s 的样本。
for (const [intervalMs, targetExpected] of [[11, true], [25, false]]) {
  const load = harness(false);
  const elapsed = await send(load, intervalMs);
  load.setNow(elapsed + 20);
  [...load.timers.values()][0]();
  await flush();
  const report = load.patches.at(-1);
  assert.equal(report.completed, true);
  assert.equal(report.attempted, report.acknowledged);
  assert.equal(report.skipped_due_to_in_flight_limit, 0);
  assert.equal(report.acknowledged * 1000 / report.completion_elapsed_ms >= 1000, targetExpected);
  load.close();
  assert.equal(load.timers.size, 0);
}

const unfinished = harness(true);
await send(unfinished);
unfinished.setNow(15000);
[...unfinished.timers.values()][0]();
await flush();
assert.equal(unfinished.patches.at(-1).completed, false);
assert.equal(unfinished.patches.at(-1).in_flight, 63);
assert.equal(unfinished.patches.at(-1).skipped_due_to_in_flight_limit, 28800 - 63);
assert.equal(unfinished.patches.at(-1).completion_elapsed_ms, 15000);
unfinished.close();
assert.equal(unfinished.timers.size, 0);

// 关闭后迟到 ACK 只能释放回执，不得恢复计时器或再提交完成状态。
const closedEarly = harness(true);
closedEarly.setNow(10);
[...closedEarly.timers.values()][0]();
await flush();
assert.equal(closedEarly.pending.length, 24);
closedEarly.close();
for (const acknowledge of closedEarly.pending) acknowledge();
await flush();
assert.equal(closedEarly.timers.size, 0);
assert.equal(closedEarly.patches.length, 24);
assert.equal(closedEarly.patches.some(patch => patch.done), false);

// 最终状态无法投递时只记录失败，不能留下模拟成功回执。
const rejectedCompletion = harness(false, true);
await send(rejectedCompletion);
rejectedCompletion.setNow(12020);
[...rejectedCompletion.timers.values()][0]();
await flush();
assert.equal(rejectedCompletion.patches.some(patch => patch.done), false);
assert.deepEqual(rejectedCompletion.errors, ['Performance fixture could not deliver its completion status.']);
rejectedCompletion.close();
assert.equal(rejectedCompletion.timers.size, 0);
console.log('fixture jitter, acknowledgement, in-flight limit, timeout and cleanup passed');
