// @author kongweiguang
import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));

/** Sidecar 迁移后仍需保留主机类型、结果标签和目标约束，不能只改清单。 */
test("manifest and source keep the host form contract", async () => {
  const manifest = JSON.parse(await readFile(`${root}kerminal-plugin.json`, "utf8"));
  assert.equal(manifest.runtime.kind, "sidecar");
  assert.equal(manifest.runtime.main["windows-x86_64"], "bin/sidecar.exe");
  assert.equal(Object.hasOwn(manifest, "permissions"), false);
  assert.equal(manifest.contributes.host_types[0].id, "http-endpoint");
  assert.equal(manifest.contributes.host_types[0].schema, "schemas/host.json");
  assert.equal(manifest.contributes.tabs[0].id, "http-result");
  const backend = await readFile(`${root}src/main.rs`, "utf8");
  for (const method of ["host.test", "host.open", "view.open", "view.action", "view.snapshot", "view.close"]) {
    assert.match(backend, new RegExp(`"${method.replace(".", "\\.")}"`));
  }
  assert.match(backend, /https:\/\/example\.com/);
  assert.match(backend, /Duration::from_secs\(10\)/);
  assert.match(backend, /Policy::none\(\)/);
  const cargo = await readFile(`${root}Cargo.toml`, "utf8");
  assert.match(cargo, /reqwest = \{ version = "=0\.12\.28"/);
  assert.match(cargo, /rustls-tls/);
});

/** 结果视图保持原结构，重复检查只替换数据而不重建标签。 */
test("result view remains the frozen native structure", async () => {
  const view = JSON.parse(await readFile(`${root}views/result.json`, "utf8"));
  assert.equal(view.schema, 1);
  const refresh = view.root.children.find((node) => node.type === "button");
  assert.equal(refresh.action, "refresh");
  assert.equal(refresh.busy, "$busy");
});

/** 分发目录不再保留脚本入口；仅保留构建器与资源。 */
test("script entry and dist output are gone", async () => {
  const entries = await readdir(`${root}src`);
  assert.equal(entries.includes("main.ts"), false);
  assert.equal(entries.includes("main.rs"), true);
  const rootEntries = await readdir(root);
  assert.equal(rootEntries.includes("dist"), false);
});