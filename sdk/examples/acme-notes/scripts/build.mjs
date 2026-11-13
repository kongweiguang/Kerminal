// @author kongweiguang
import { cp, mkdir, writeFile } from "node:fs/promises";
import { views } from "../src/views.ts";

/** 与 CLI 共用 resources 源；Sidecar 二进制由 `kerminal-plugin build` 构建，避免本地脚本重复编译。 */
async function build() {
  const root = new URL("../", import.meta.url);
  for (const folder of ["l10n", "schemas"]) {
    await cp(new URL(`resources/${folder}/`, root), new URL(`${folder}/`, root), { recursive: true });
  }
  await mkdir(new URL("views/", root), { recursive: true });
  for (const [name, view] of Object.entries(views)) {
    await writeFile(new URL(`views/${name}`, root), `${JSON.stringify(view, null, 2)}\n`);
  }
}

await build();