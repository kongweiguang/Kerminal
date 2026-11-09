// @author kongweiguang
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import test from "node:test";

const root = fileURLToPath(new URL("../", import.meta.url));

/** 模拟宿主协议边界，验证插件调用和实例状态而不伪造桌面验收。 */
async function load(scriptPath = join(root, "dist/main.js")) {
  const handlers = new Map();
  const calls = [];
  let saved = { text: "Saved note" };
  const host = {
    environment: { locale: "zh-CN", theme: "dark" },
    onRequest(method, handler) { handlers.set(method, handler); },
    async request(method, params) {
      calls.push({ method, params });
      if (method === "state.get") return { state: saved };
      if (method === "settings.get") return { settings: { showPreview: true } };
      if (method === "state.set") saved = params.state;
      return {};
    },
    view: { async update(instance, patch) { calls.push({ method: "view.update", instance, patch }); return {}; } },
  };
  const script = new vm.Script(await readFile(scriptPath, "utf8"));
  script.runInNewContext({ kerminal: host, fetch: async () => ({ ok: true }) });
  return { handlers, calls };
}

/** 保存只更新侧栏，另一个编辑器的草稿不会被后台补丁覆盖。 */
test("save, settings, instances and snapshots", async () => {
  const { handlers, calls } = await load();
  const open = handlers.get("view.open");
  const panel = await open({ view: "notes.panel", instance: "panel", scope: {} });
  assert.equal(panel.data.saved, "Saved note");
  assert.equal(panel.data.showPreview, true);
  const first = await open({ view: "notes.editor", instance: "a", scope: {}, state: { text: "Draft A" } });
  const second = await open({ view: "notes.editor", instance: "b", scope: {}, state: { text: "Draft B" } });
  assert.equal(first.data.text, "Draft A");
  assert.equal(second.data.text, "Draft B");
  await handlers.get("view.action")({ instance: "a", action: "save", data: { text: "Saved A" } });
  assert.equal(calls.find((call) => call.method === "state.set").params.state.text, "Saved A");
  assert.equal(calls.find((call) => call.method === "view.update").instance, "panel");
  assert.equal(handlers.get("view.snapshot")({ instance: "b" }).state.text, "Draft B");
  assert.equal(handlers.get("view.close")({ instance: "b", data: { text: "Unsaved B" } }).state.text, "Unsaved B");
  await handlers.get("settings.changed")({ settings: { showPreview: false } });
  assert.equal(calls.at(-1).patch.showPreview, false);
  await handlers.get("command.run")({ command: "check-service", context: {} });
  assert.equal(calls.at(-1).method, "ui.notify");
});

/** 1.1 在新的不可变目录中增加界面、权限和真实复制动作。 */
test("stage 1.0 and 1.1 with different capability", async () => {
  const base = await mkdtemp(join(tmpdir(), "kerminal-notes-example-"));
  try {
    const oldDir = join(base, "old");
    const newDir = join(base, "new");
    execFileSync(process.execPath, [join(root, "scripts/stage-version.mjs"), "1.0.0", oldDir]);
    execFileSync(process.execPath, [join(root, "scripts/stage-version.mjs"), "1.1.0", newDir]);
    const oldManifest = JSON.parse(await readFile(join(oldDir, "kerminal-plugin.json"), "utf8"));
    const newManifest = JSON.parse(await readFile(join(newDir, "kerminal-plugin.json"), "utf8"));
    assert.equal(oldManifest.version, "1.0.0");
    assert.equal(newManifest.version, "1.1.0");
    assert.equal(oldManifest.permissions.includes("clipboard.write"), false);
    assert.equal(newManifest.permissions.includes("clipboard.write"), true);
    if (process.env.KERMINAL_PROTOCOL_CHECKER) {
      execFileSync(process.env.KERMINAL_PROTOCOL_CHECKER, [oldDir, "third-party", "any"]);
      execFileSync(process.env.KERMINAL_PROTOCOL_CHECKER, [newDir, "third-party", "any"]);
    }
    const editor = JSON.parse(await readFile(join(newDir, "views/editor.json"), "utf8"));
    assert.equal(editor.root.children.find((node) => node.type === "toolbar").children.at(-1).action, "copy");
    const { handlers, calls } = await load(join(newDir, "dist/main.js"));
    await handlers.get("view.open")({ view: "notes.editor", instance: "a", scope: {} });
    await handlers.get("view.action")({ instance: "a", action: "copy", data: { text: "Fresh draft" } });
    assert.equal(calls.at(-1).method, "clipboard.write");
    assert.equal(calls.at(-1).params.text, "Fresh draft");
  } finally {
    if (!resolve(base).startsWith(`${resolve(tmpdir())}${sep}kerminal-notes-example-`)) throw new Error("unsafe test path");
    await rm(base, { recursive: true, force: true });
  }
});
