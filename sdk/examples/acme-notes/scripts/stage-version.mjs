// @author kongweiguang
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

/** 为两个版本各自生成不可变目录，已存在的目标必须由调用方处理。 */
async function stage() {
  const [version, output, ...extra] = process.argv.slice(2);
  if (!output || extra.length || !["1.0.0", "1.1.0"].includes(version)) {
    throw new Error("Usage: node scripts/stage-version.mjs <1.0.0|1.1.0> <new-directory>");
  }
  const source = new URL("../", import.meta.url);
  const destination = pathToFileURL(`${resolve(output)}/`);
  await mkdir(destination);
  for (const name of ["dist", "l10n", "schemas", "views"]) {
    await cp(new URL(`${name}/`, source), new URL(`${name}/`, destination), { recursive: true, errorOnExist: true });
  }
  const manifest = JSON.parse(await readFile(new URL("kerminal-plugin.json", source), "utf8"));
  manifest.version = version;
  if (version === "1.1.0") {
    manifest.permissions.push("clipboard.write");
    const editorPath = new URL("views/editor.json", destination);
    const editor = JSON.parse(await readFile(editorPath, "utf8"));
    const toolbar = editor.root.children.find((node) => node.type === "toolbar");
    if (!toolbar) throw new Error("Editor toolbar missing");
    toolbar.children.push({ type: "button", label: "%editor.copy%", action: "copy" });
    await writeFile(editorPath, `${JSON.stringify(editor, null, 2)}\n`);
    const scriptPath = new URL("dist/main.js", destination);
    const sourceScript = await readFile(scriptPath, "utf8");
    const expected = 'const EXAMPLE_VERSION = "1.0.0";';
    if (!sourceScript.includes(expected)) throw new Error("Version marker missing");
    await writeFile(scriptPath, sourceScript.replace(expected, 'const EXAMPLE_VERSION = "1.1.0";'));
  }
  await writeFile(new URL("kerminal-plugin.json", destination), `${JSON.stringify(manifest, null, 2)}\n`);
}

await stage();
