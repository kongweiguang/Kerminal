// @author kongweiguang
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = fileURLToPath(new URL("../", import.meta.url));

/** 版本化目录必须仍是可独立构建的 Sidecar 源码，不再伪造已编译的 dist/main.js。 */
async function stage(version, output) {
  execFileSync(process.execPath, [join(root, "scripts/stage-version.mjs"), version, output]);
}

/** 1.1 在新的不可变目录中增加界面和真实运行分支，1.0 不能获得复制能力；权限字段不参与版本差异。 */
test("stage 1.0 and 1.1 as independent sidecar sources", async () => {
  const base = await mkdtemp(join(tmpdir(), "kerminal-notes-example-"));
  try {
    const oldDir = join(base, "old");
    const newDir = join(base, "new");
    await stage("1.0.0", oldDir);
    await stage("1.1.0", newDir);
    const oldManifest = JSON.parse(await readFile(join(oldDir, "kerminal-plugin.json"), "utf8"));
    const newManifest = JSON.parse(await readFile(join(newDir, "kerminal-plugin.json"), "utf8"));
    assert.equal(oldManifest.version, "1.0.0");
    assert.equal(newManifest.version, "1.1.0");
    assert.equal(oldManifest.runtime.kind, "sidecar");
    assert.equal(newManifest.runtime.kind, "sidecar");
    // 权限只是描述字段；两个版本保持一致，能力差异必须由按钮和 Sidecar 分支承担。
    assert.deepEqual(newManifest.permissions, oldManifest.permissions);
    assert.equal(oldManifest.permissions?.includes("clipboard.write") ?? false, false);
    const oldViews = await readFile(join(oldDir, "src/views.ts"), "utf8");
    const newViews = await readFile(join(newDir, "src/views.ts"), "utf8");
    assert.doesNotMatch(oldViews, /action: "copy"/);
    assert.match(newViews, /action: "copy"/);
    assert.doesNotMatch(newViews, /\.\.\/\.\.\/\.\.\/ts/);
    assert.match(newViews, /"\.\/builders\.ts"/);
    assert.ok(await readFile(join(newDir, "src/builders.ts"), "utf8"));
    assert.doesNotMatch(await readdir(oldDir).then((entries) => entries.join(",")), /(^|,)dist(,|$)/);
    assert.match(await readFile(join(newDir, "src/main.rs"), "utf8"), /PLUGIN_VERSION == "1\.1\.0"/);
    assert.match(await readFile(join(newDir, "src/main.rs"), "utf8"), /clipboard\.write/);
    // 直接 cargo build 的包版本必须与清单一致，CLI 注入只覆盖握手身份。
    assert.match(await readFile(join(oldDir, "Cargo.toml"), "utf8"), /^version = "1\.0\.0"$/m);
    assert.match(await readFile(join(newDir, "Cargo.toml"), "utf8"), /^version = "1\.1\.0"$/m);
    assert.match(await readFile(join(newDir, "Cargo.toml"), "utf8"), /kerminal-plugin-sidecar/);
  } finally {
    if (!resolve(base).startsWith(`${resolve(tmpdir())}${sep}kerminal-notes-example-`)) throw new Error("unsafe test path");
    await rm(base, { recursive: true, force: true });
  }
});

/** 目标已存在时拒绝覆盖，避免版本化目录被当作可覆盖的构建缓存。 */
test("stage refuses to overwrite an existing directory", async () => {
  const base = await mkdtemp(join(tmpdir(), "kerminal-notes-example-"));
  try {
    const output = join(base, "existing");
    await stage("1.0.0", output);
    await assert.rejects(stage("1.1.0", output), /目标目录已存在/);
  } finally {
    if (!resolve(base).startsWith(`${resolve(tmpdir())}${sep}kerminal-notes-example-`)) throw new Error("unsafe test path");
    await rm(base, { recursive: true, force: true });
  }
});