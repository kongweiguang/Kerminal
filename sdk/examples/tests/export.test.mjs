// @author kongweiguang
import assert from "node:assert/strict";
import { access, mkdtemp, mkdir, readFile, readdir, rm, symlink } from "node:fs/promises";
import { execFileSync, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const exportScript = fileURLToPath(new URL("../scripts/export.mjs", import.meta.url));
const sdk = fileURLToPath(new URL("../../ts/", import.meta.url));

/** 文件存在性检查让“没有锁文件”也成为明确断言，而不是忽略。 */
async function exists(path) {
  try {
    await access(path);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}

/** 在独立临时项目解析实际 SDK 视图构建器，确保导出不依赖主仓库相对路径。 */
test("exported examples remain independent buildable sidecars", async () => {
  const temporary = await mkdtemp(join(tmpdir(), "kerminal-plugin-export-"));
  try {
    for (const name of ["acme-notes", "http-check", "file-search"]) {
      const output = join(temporary, name);
      execFileSync(process.execPath, [exportScript, name, output]);
      const manifest = JSON.parse(await readFile(join(output, "kerminal-plugin.json"), "utf8"));
      const packageJson = JSON.parse(await readFile(join(output, "package.json"), "utf8"));
      assert.equal(manifest.runtime.kind, "sidecar");
      assert.equal(packageJson.devDependencies["@kerminal/sdk"], "file:.kerminal-sdk/sdk/ts");
      const workflow = await readFile(join(output, ".github/workflows/release.yml"), "utf8");
      assert.match(workflow, /notarytool submit/);
      assert.match(workflow, /支持 sidecar 和 none/);
      assert.ok(await readFile(join(output, "resources/l10n/en.json"), "utf8"));
      assert.match(await readFile(join(output, "Cargo.toml"), "utf8"), /kerminal-plugin-sidecar/);
      const sourceRoot = fileURLToPath(new URL(`../${name}/`, import.meta.url));
      assert.equal(await exists(join(output, "Cargo.lock")), await exists(join(sourceRoot, "Cargo.lock")));
      assert.equal(await exists(join(output, "src/main.ts")), false);
      assert.equal(await exists(join(output, "src/main.rs")), true);
      const web = join(output, "src/web.ts");
      if (await exists(web)) {
        assert.doesNotMatch(await readFile(web, "utf8"), /\.\.\/\.\.\/\.\.\/ts/);
      }
      // 独立仓库只带静态视图构建脚本；版本化辅助脚本依赖主仓库路径，不能被导出。
      assert.deepEqual(await readdir(join(output, "scripts")), ["build.mjs"]);
      assert.equal(await exists(join(output, "scripts/stage-version.mjs")), false);
      // 用临时 SDK 链接代替 npm install，直接验证导出仓库里的构建脚本可以生成清单引用的资源。
      await mkdir(join(output, "node_modules/@kerminal"), { recursive: true });
      await symlink(sdk, join(output, "node_modules/@kerminal/sdk"), process.platform === "win32" ? "junction" : "dir");
      execFileSync(process.execPath, [join(output, "scripts/build.mjs")], { cwd: output, stdio: "pipe" });
      for (const view of Object.values(manifest.views)) {
        if (view.kind === "native") {
          assert.equal(JSON.parse(await readFile(join(output, view.file), "utf8")).schema, 1);
        } else {
          // WebView 不再依赖 csp 声明；入口和打包脚本仍必须真实存在。
          assert.equal(Object.hasOwn(view, "csp"), false);
          assert.ok(await exists(join(output, view.entry)));
          assert.ok(await exists(join(output, "web/main.js")));
        }
      }
      assert.ok(await exists(join(output, "l10n/en.json")));
      assert.match(await readFile(join(output, "README.md"), "utf8"), /KERMINAL_SDK_REF/);
      assert.match(await readFile(join(output, "README.md"), "utf8"), /完全信任/);
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