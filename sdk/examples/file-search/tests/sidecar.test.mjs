// @author kongweiguang
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { once } from "node:events";
import test from "node:test";

const root = fileURLToPath(new URL("../", import.meta.url));
const executable = process.env.KERMINAL_SEARCH_TEST_BIN || join(root, "bin", process.platform === "win32" ? "file-search.exe" : "file-search");

/** 保留每个请求的回执并确认插件到宿主的请求，模拟双向 RPC 背压。 */
function peer() {
  const process = spawn(executable, [], { stdio: ["pipe", "pipe", "pipe"] });
  const waiting = new Map();
  const events = [];
  const replies = [];
  let sequence = 0;
  let stderr = "";
  process.stderr.on("data", (chunk) => { stderr += chunk.toString(); });
  createInterface({ input: process.stdout }).on("line", (line) => {
    const frame = JSON.parse(line);
    if (frame.method) {
      events.push(frame);
      if (frame.id) process.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: frame.id, result: {} })}\n`);
      return;
    }
    const entry = waiting.get(frame.id);
    if (!entry) return;
    replies.push(frame.id);
    waiting.delete(frame.id);
    clearTimeout(entry.timer);
    if (frame.error) entry.reject(Object.assign(new Error(frame.error.message), { code: frame.error.code }));
    else entry.resolve(frame.result);
  });
  process.on("exit", (code) => {
    for (const entry of waiting.values()) { clearTimeout(entry.timer); entry.reject(new Error(`sidecar exited ${code}: ${stderr}`)); }
    waiting.clear();
  });
  /** 请求发出前登记回执，快速回复也不会落入监听空窗。 */
  function request(method, params = {}) {
    const id = `h:${++sequence}`;
    const response = new Promise((resolveResponse, reject) => {
      const timer = setTimeout(() => reject(new Error(`timeout: ${method}`)), 15_000);
      waiting.set(id, { resolve: resolveResponse, reject, timer });
    });
    process.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return { id, response };
  }
  /** 取消使用原始请求身份，不把后续请求误当成取消目标。 */
  function cancel(id) {
    process.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "$/cancel", params: { id } })}\n`);
  }
  return { process, events, replies, request, cancel };
}

/** 握手使用清单同一身份，版本不符时由真实 Sidecar 拒绝。 */
async function initialize(client) {
  const result = await client.request("initialize", {
    protocol: 1, host_api: "1.0", plugin: { id: "kerminal.file-search", version: "1.0.0" },
    locale: "zh-CN", theme: "dark", data_dir: "", generation: 1,
  }).response;
  assert.equal(result.plugin.id, "kerminal.file-search");
}

/** 临时目录必须位于本次测试创建的目录，避免清理其他用户数据。 */
async function cleanup(path) {
  if (!resolve(path).startsWith(`${resolve(tmpdir())}${sep}kerminal-file-search-`)) throw new Error("unsafe test path");
  await rm(path, { recursive: true, force: true });
}

/** 实际工作子进程按批次输出结果，并可关闭、重开和恢复查询。 */
test("stream search and restore state", async () => {
  const directory = await mkdtemp(join(tmpdir(), "kerminal-file-search-"));
  const client = peer();
  try {
    await mkdir(join(directory, "nested"));
    for (let index = 0; index < 45; index++) await writeFile(join(directory, "nested", `match-${index}.txt`), "");
    await initialize(client);
    await client.request("view.open", { view: "search.page", instance: "one", scope: {} }).response;
    const result = await client.request("view.action", { instance: "one", action: "search", data: { root: directory, query: "match" } }).response;
    assert.equal(result.patch.results.length, 45);
    assert.equal(result.patch.limited, false);
    assert.equal(result.patch.skipped, 0);
    assert.ok(client.events.some((event) => event.method === "view.update" && event.params.patch.results.length >= 20));
    assert.ok(client.events.some((event) => event.method === "$/progress"));
    const saved = await client.request("view.close", { instance: "one" }).response;
    const reopened = await client.request("view.open", { view: "search.page", instance: "two", scope: {}, state: saved.state }).response;
    assert.equal(reopened.data.root, directory);
    assert.equal(reopened.data.query, "match");
    assert.equal(reopened.data.results.length, 0);
    await client.request("shutdown").response;
    await once(client.process, "exit");
  } finally {
    if (client.process.exitCode === null) client.process.kill();
    await cleanup(directory);
  }
});

/** 无效身份不能完成握手；达到结果上限时明确报告结果可能不完整。 */
test("reject wrong identity and report result limit", async () => {
  const directory = await mkdtemp(join(tmpdir(), "kerminal-file-search-"));
  const client = peer();
  try {
    for (let index = 0; index < 501; index++) await writeFile(join(directory, `match-${index}.txt`), "");
    const bad = client.request("initialize", {
      protocol: 1, host_api: "1.0", plugin: { id: "kerminal.file-search", version: "9.9.9" },
      locale: "en", theme: "light", data_dir: "", generation: 1,
    });
    await assert.rejects(bad.response, (error) => error.code === -32602);
    await initialize(client);
    await client.request("view.open", { view: "search.page", instance: "one", scope: {} }).response;
    const result = await client.request("view.action", { instance: "one", action: "search", data: { root: directory, query: "match" } }).response;
    assert.equal(result.patch.results.length, 500);
    assert.equal(result.patch.limited, true);
    await client.request("shutdown").response;
    await once(client.process, "exit");
  } finally {
    if (client.process.exitCode === null) client.process.kill();
    await cleanup(directory);
  }
});

/** 取消回执在子进程 kill 和 wait 之后发出，随后仍可处理心跳。 */
test("cancel worker and remain responsive", async () => {
  const directory = await mkdtemp(join(tmpdir(), "kerminal-file-search-"));
  const client = peer();
  try {
    for (let index = 0; index < 60; index++) await writeFile(join(directory, `match-${index}.txt`), "");
    await initialize(client);
    await client.request("view.open", { view: "search.page", instance: "one", scope: {} }).response;
    const search = client.request("view.action", { instance: "one", action: "search", data: { root: directory, query: "match" } });
    client.cancel(search.id);
    await assert.rejects(search.response, (error) => error.code === -32002);
    const ping = await client.request("$/ping").response;
    assert.equal(typeof ping, "object");
    await client.request("shutdown").response;
    await once(client.process, "exit");
  } finally {
    if (client.process.exitCode === null) client.process.kill();
    await cleanup(directory);
  }
});

/** 关闭成功必须晚于搜索取消回执，防止视图已经释放但所属 worker 仍在运行。 */
test("close waits for the active worker before acknowledging", async () => {
  const directory = await mkdtemp(join(tmpdir(), "kerminal-file-search-"));
  const client = peer();
  try {
    await writeFile(join(directory, "match.txt"), "");
    await initialize(client);
    await client.request("view.open", { view: "search.page", instance: "one", scope: {} }).response;
    const search = client.request("view.action", { instance: "one", action: "search", data: { root: directory, query: "match" } });
    const cancelled = assert.rejects(search.response, (error) => error.code === -32002);
    const close = client.request("view.close", { instance: "one" });
    const saved = await close.response;
    await cancelled;
    assert.equal(saved.state.root, directory);
    assert.ok(client.replies.indexOf(search.id) < client.replies.indexOf(close.id));
    await client.request("shutdown").response;
    await once(client.process, "exit");
  } finally {
    if (client.process.exitCode === null) client.process.kill();
    await cleanup(directory);
  }
});
