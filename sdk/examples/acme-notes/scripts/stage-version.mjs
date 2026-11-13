// @author kongweiguang
import { cp, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const source = fileURLToPath(new URL("../", import.meta.url));
const builders = fileURLToPath(new URL("../../../ts/src/builders.ts", import.meta.url));
const COPY_LINE = '        ui.button({ label: "%editor.check%", action: "check", busy: "$busy" }),\n';
const COPY_BUTTON = '        ui.button({ label: "%editor.copy%", action: "copy" }),\n';

/** Sidecar 产物由 CLI 按清单版本构建；这里只准备可独立构建的版本化源码。 */
function platform() {
  if (process.platform === "win32") return "windows-x86_64";
  if (process.platform === "darwin") return process.arch === "arm64" ? "macos-aarch64" : "macos-x86_64";
  return "linux-x86_64";
}

/** 1.1.0 的复制能力体现在按钮和 Sidecar 运行时分支中；权限只是描述字段，不参与能力开关。 */
async function stageVersion(version, output) {
  if (!output || !["1.0.0", "1.1.0"].includes(version)) {
    throw new Error("Usage: node scripts/stage-version.mjs <1.0.0|1.1.0> <new-directory>");
  }
  const destination = resolve(output);
  if (existsSync(destination)) throw new Error("目标目录已存在，拒绝覆盖");
  const temporary = join(dirname(destination), `.stage-${randomUUID()}`);
  await mkdir(temporary);
  try {
    for (const item of ["Cargo.toml", "src", "resources"]) {
      await cp(join(source, item), join(temporary, item), { recursive: true, errorOnExist: true });
    }
    const manifest = JSON.parse(await readFile(join(source, "kerminal-plugin.json"), "utf8"));
    manifest.version = version;
    // 直接 cargo build 时版本来自 Cargo.toml，必须与清单一致；CLI 仍可用环境变量覆盖握手版本。
    const cargoPath = join(temporary, "Cargo.toml");
    const cargo = await readFile(cargoPath, "utf8");
    const versionField = /(\[package\][\s\S]*?\nversion\s*=\s*")[^"]+(")/;
    if (!versionField.test(cargo)) throw new Error("Cargo.toml package version marker missing");
    await writeFile(cargoPath, cargo.replace(versionField, `$1${version}$2`));
    const editorPath = join(temporary, "src", "views.ts");
    // 版本目录脱离示例树后也必须能构建：复制构建器与它引用的类型生成物，改用局部相对引用。
    await mkdir(join(temporary, "src", "generated"), { recursive: true });
    await cp(new URL("../../../ts/src/generated/view.ts", import.meta.url), join(temporary, "src", "generated", "view.ts"));
    await cp(builders, join(temporary, "src", "builders.ts"));
    const editor = (await readFile(editorPath, "utf8")).replace(
      "../../../ts/src/builders.ts",
      "./builders.ts",
    );
    if (version === "1.1.0") {
      if (!editor.includes(COPY_LINE)) throw new Error("Editor toolbar marker missing");
      await writeFile(editorPath, editor.replace(COPY_LINE, `${COPY_LINE}${COPY_BUTTON}`));
    } else if (editor.includes('action: "copy"')) {
      throw new Error("1.0.0 不应包含复制入口");
    }
    await writeFile(join(temporary, "kerminal-plugin.json"), `${JSON.stringify(manifest, null, 2)}\n`);
    if (process.env.KERMINAL_PLUGIN_CLI) {
      // 提供 CLI 时直接产出真实包目录；否则保留源码目录并明确告知仍需构建。
      execFileSync(process.env.KERMINAL_PLUGIN_CLI, ["build", "--platform", platform()], {
        cwd: temporary,
        stdio: "inherit",
        windowsHide: true,
      });
      await cp(join(temporary, ".kerminal", "build"), destination, { recursive: true, errorOnExist: true });
    } else {
      await rename(temporary, destination);
      console.log(`Staged ${version} source at ${destination}; run kerminal-plugin build --platform ${platform()} to produce an installable package.`);
      return;
    }
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
  console.log(`Built ${version} package at ${destination}`);
}

const [version, output, ...extra] = process.argv.slice(2);
if (extra.length) throw new Error("unexpected arguments");
await stageVersion(version, output);