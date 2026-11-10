// @author kongweiguang
import { cp, readFile, writeFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";

/** 静态资源与 CLI 同源；这里只生成网页，Sidecar 按平台通过 CLI 构建。 */
async function build() {
  const root = new URL("../", import.meta.url);
  for (const folder of ["l10n", "web"]) {
    await cp(new URL(`resources/${folder}/`, root), new URL(`${folder}/`, root), { recursive: true });
  }
  const source = await readFile(new URL("src/web.ts", root), "utf8");
  await writeFile(new URL("web/main.js", root), stripTypeScriptTypes(source, { mode: "strip" }));
}

await build();
