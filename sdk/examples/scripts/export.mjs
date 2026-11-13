// @author kongweiguang
import { access, cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const examples = new Set(["acme-notes", "http-check", "file-search"]);

/** 仅当源目录存在该文件时复制，避免为无锁示例伪造 Cargo.lock。 */
async function copyIfPresent(source, destination) {
  try {
    await access(source);
  } catch (error) {
    if (error.code === "ENOENT") return;
    throw error;
  }
  await cp(source, destination);
}

/** 导出只复制作者源码和资源，不携带本机二进制、锁外依赖或验收产物。 */
async function exportExample() {
  const [name, output, ...extra] = process.argv.slice(2);
  if (!examples.has(name) || !output || extra.length) {
    throw new Error("Usage: node sdk/examples/scripts/export.mjs <acme-notes|http-check|file-search> <new-directory>");
  }
  const source = fileURLToPath(new URL(`../${name}/`, import.meta.url));
  const destination = resolve(output);
  const manifest = JSON.parse(await readFile(resolve(source, "kerminal-plugin.json"), "utf8"));
  await mkdir(dirname(destination), { recursive: true });
  // 不覆盖既有仓库；创建失败时不会改写任何已有文件。
  await mkdir(destination);
  for (const folder of ["src", "resources"]) {
    await cp(resolve(source, folder), resolve(destination, folder), { recursive: true, errorOnExist: true });
  }
  // 只带静态视图构建脚本；acme-notes 的 stage-version 依赖主仓库 SDK 相对路径，
  // 不属于独立插件仓库的运行链路，带出后会形成无法解析的引用。
  await mkdir(resolve(destination, "scripts"), { recursive: true });
  await copyIfPresent(resolve(source, "scripts/build.mjs"), resolve(destination, "scripts/build.mjs"));
  for (const file of ["main.ts", "views.ts", "web.ts"]) {
    const target = resolve(destination, "src", file);
    let text;
    try { text = await readFile(target, "utf8"); }
    catch (error) { if (error.code === "ENOENT") continue; throw error; }
    const references = new Map([
      ["../../../ts/src/builders.ts", "@kerminal/sdk/builders"],
      ["../../../ts/src/index.js", "@kerminal/sdk"],
      ["../../../ts/src/json.js", "@kerminal/sdk"],
      ["../../../ts/src/browser.js", "@kerminal/sdk/browser"],
    ]);
    for (const [from, to] of references) text = text.replaceAll(from, to);
    await writeFile(target, text);
  }
  await cp(resolve(source, "kerminal-plugin.json"), resolve(destination, "kerminal-plugin.json"));
  if (manifest.runtime.kind === "sidecar") {
    // Sidecar 独立仓库必须带着自己的清单才能通过 CLI 构建；已有锁文件用于可复现构建。
    await copyIfPresent(resolve(source, "Cargo.toml"), resolve(destination, "Cargo.toml"));
    await copyIfPresent(resolve(source, "Cargo.lock"), resolve(destination, "Cargo.lock"));
  }
  const workflow = resolve(destination, ".github/workflows/release.yml");
  await mkdir(dirname(workflow), { recursive: true });
  await cp(new URL("../../templates/release.yml", import.meta.url), workflow);
  await writeFile(resolve(destination, ".gitignore"), "# @author kongweiguang\n.kerminal/\n.kerminal-sdk/\nnode_modules/\ntarget/\n");
  await writeFile(resolve(destination, "package.json"), `${JSON.stringify({
    private: true, type: "module", devDependencies: { "@kerminal/sdk": "file:.kerminal-sdk/sdk/ts" },
  }, null, 2)}\n`);
  const usage = await readFile(new URL("../../templates/README.md", import.meta.url), "utf8");
  await writeFile(resolve(destination, "README.md"), `${usage.replaceAll("__PLUGIN_ID__", manifest.id)}\n独立示例本地准备：将 Kerminal 仓库以明确提交检出到本目录的 \`.kerminal-sdk/\`，依次运行 \`npm ci --prefix .kerminal-sdk/sdk/ts\`、\`npm run build --prefix .kerminal-sdk/sdk/ts\`、\`npm install --no-audit --no-fund\`。Sidecar 示例先用 Node 24 运行 \`node scripts/build.mjs\` 生成静态视图，再执行 \`kerminal-plugin build --platform <平台>\`。发布工作流会从 \`KERMINAL_SDK_REF\` 自动完成这一步。官方保留 ID 的示例（\`kerminal.*\`）需要为 CLI 加 \`--official\`，工作流对应设置仓库变量 \`KERMINAL_PLUGIN_OFFICIAL=true\`。\n`);
  console.log(`Exported ${manifest.id} to ${destination}`);
}

await exportExample();