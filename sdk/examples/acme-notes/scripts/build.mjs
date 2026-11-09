// @author kongweiguang
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import { views } from "../src/views.ts";

/** 与 CLI 共用 resources 源并生成可直接加载的目录，安装后无需 Node 或 SDK。 */
async function build() {
  const root = new URL("../", import.meta.url);
  for (const folder of ["l10n", "schemas"]) {
    await cp(new URL(`resources/${folder}/`, root), new URL(`${folder}/`, root), { recursive: true });
  }
  await mkdir(new URL("dist/", root), { recursive: true });
  await mkdir(new URL("views/", root), { recursive: true });
  const source = await readFile(new URL("src/main.ts", root), "utf8");
  await writeFile(new URL("dist/main.js", root), stripTypeScriptTypes(source, { mode: "strip" }));
  for (const [name, view] of Object.entries(views)) {
    await writeFile(new URL(`views/${name}`, root), `${JSON.stringify(view, null, 2)}\n`);
  }
}

await build();
