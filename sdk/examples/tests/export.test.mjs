// @author kongweiguang
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import test from "node:test";

const exportScript = fileURLToPath(new URL("../scripts/export.mjs", import.meta.url));
const sdk = fileURLToPath(new URL("../../ts/", import.meta.url));

/** 在独立临时项目解析实际 SDK 视图构建器，确保导出不依赖主仓库相对路径。 */
test("exported examples build views with the SDK package", async () => {
  const temporary = await mkdtemp(join(tmpdir(), "kerminal-plugin-export-"));
  try {
    for (const name of ["acme-notes", "http-check", "file-search"]) {
      const output = join(temporary, name);
      execFileSync(process.execPath, [exportScript, name, output]);
      const manifest = JSON.parse(await readFile(join(output, "kerminal-plugin.json"), "utf8"));
      const packageJson = JSON.parse(await readFile(join(output, "package.json"), "utf8"));
      assert.equal(packageJson.devDependencies["@kerminal/sdk"], "file:.kerminal-sdk/sdk/ts");
      assert.match(await readFile(join(output, ".github/workflows/release.yml"), "utf8"), /notarytool submit/);
      assert.ok(await readFile(join(output, "resources/l10n/en.json"), "utf8"));
      if (manifest.runtime.kind === "script") {
        await mkdir(join(output, "node_modules/@kerminal"), { recursive: true });
        await symlink(sdk, join(output, "node_modules/@kerminal/sdk"), process.platform === "win32" ? "junction" : "dir");
        const views = await import(pathToFileURL(join(output, "src/views.ts")).href);
        for (const view of Object.values(manifest.views)) {
          assert.equal(views.default[view.file]?.schema, 1);
        }
        assert.doesNotMatch(await readFile(join(output, "src/main.ts"), "utf8"), /\.\.\/\.\.\/\.\.\/ts/);
      } else {
        assert.match(await readFile(join(output, "Cargo.toml"), "utf8"), /kerminal-plugin-sidecar/);
        assert.doesNotMatch(await readFile(join(output, "src/web.ts"), "utf8"), /\.\.\/\.\.\/\.\.\/ts/);
      }
      const before = await readFile(join(output, "kerminal-plugin.json"), "utf8");
      const duplicate = spawnSync(process.execPath, [exportScript, name, output]);
      assert.notEqual(duplicate.status, 0);
      assert.equal(await readFile(join(output, "kerminal-plugin.json"), "utf8"), before);
    }
  } finally {
    assert.ok(resolve(temporary).startsWith(resolve(tmpdir())));
    await rm(temporary, { recursive: true, force: true });
  }
});
