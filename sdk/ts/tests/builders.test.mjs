// @author kongweiguang

import assert from "node:assert/strict";
import { copyFile, mkdtemp, mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { test } from "node:test";
import { execFile, execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import Ajv2020 from "ajv/dist/2020.js";
import { ui } from "../dist/builders.js";
import { render } from "../scripts/generate-types.mjs";

const viewSchema = JSON.parse(await readFile(new URL("../../protocol/schema/view.schema.json", import.meta.url), "utf8"));
const validator = new Ajv2020({ allErrors: true, strict: false });
validator.addFormat("double", { type: "number", validate: Number.isFinite });
validator.addFormat("uint32", { type: "number", validate: (value) => Number.isInteger(value) && value >= 0 && value <= 0xffffffff });
const validateView = validator.compile(viewSchema);
const text = ui.text({ text: "Title" });
const items = [{ label: "Open", action: "open" }];
const execFileAsync = promisify(execFile);
const samples = {
  stack: { children: [text], gap: "md" },
  row: { children: [text], gap: "sm" },
  section: { title: "Section", children: [text] },
  scroll: { child: text },
  split: { children: [text, text] },
  tabs: { tabs: [{ id: "main", title: "Main", content: text }] },
  text: { text: "Text", tone: "muted", id: "label", visible: "$visible", "x-test": true },
  markdown: { text: "**Text**" },
  badge: { text: "Ready", tone: "success" },
  icon: { icon: "search" },
  code: { code: "hello", language: "text" },
  progress: { value: "$progress", max: 100 },
  // 矩阵只接受实例数据里的只读绑定，样本与 Rust 校验保持同一形状。
  qrcode: { matrix: "$qr", label: "Scan to pair" },
  empty: { title: "No data", icon: "search" },
  notice: { text: "Notice", tone: "warning" },
  input: { bind: "query", placeholder: "Search", on_submit: "search" },
  textarea: { bind: "notes" },
  number: { bind: "count", min: 0, max: 100, step: 1 },
  select: { bind: "choice", options: [{ label: "One", value: "one" }] },
  switch: { bind: "enabled" },
  checkbox: { bind: "enabled" },
  path: { bind: "path", mode: "open" },
  secret: { bind: "secret" },
  button: { action: "refresh", label: "Refresh", busy: "$busy", variant: "secondary" },
  toolbar: { children: [text] },
  menu: { items },
  list: { items: "$items", key: "id", title: "name", menu: items },
  table: { source: "rpc", id: "table", key: "id", columns: [{ key: "name", title: "Name" }] },
  tree: { source: "rpc", id: "tree", key: "id", title: "name" },
  form: { schema: "schemas/settings.json", bind: "settings" },
};

/** 每种协议节点都由对应构建器生成，避免仅新增类型而漏掉构建入口。 */
test("all schema node types have builders whose JSON validates against the Rust schema", () => {
  const kinds = viewSchema.$defs.NativeNode.oneOf.map((variant) => variant.properties.type.const).sort();
  assert.deepEqual(Object.keys(samples).sort(), kinds);
  assert.deepEqual(Object.keys(ui).filter((name) => name !== "view").sort(), kinds);
  for (const kind of kinds) {
    const view = ui.view({ data: { visible: true, items: [], busy: false, progress: 50 }, root: ui[kind](samples[kind]) });
    assert.equal(validateView(JSON.parse(JSON.stringify(view))), true, `${kind}: ${JSON.stringify(validateView.errors)}`);
  }
});

/** 这些边界也由 Rust 拒绝，Schema 不能只接受宽泛的对象形状。 */
test("schema rejects invalid native view constraints", () => {
  const invalid = [
    { schema: 2, root: { type: "text", text: "bad version" } },
    { schema: 1, root: { type: "button", action: "save" } },
    { schema: 1, root: { type: "input", bind: "bad[0]" } },
    { schema: 1, root: { type: "text", text: "bad binding", visible: "visible" } },
    { schema: 1, root: { type: "split", children: [{ type: "text", text: "one" }] } },
    { schema: 1, root: { type: "table", items: "$items", key: "id", columns: [] } },
    { schema: 1, root: { type: "list", source: "rpc", key: "id", title: "name" } },
    { schema: 1, root: { type: "list", source: "rpc", id: "rows", items: "$items", key: "id", title: "name" } },
    // 二维码矩阵只能是 $ 开头的只读绑定，静态字面量不得进入 Schema 接受的形状。
    { schema: 1, root: { type: "qrcode", matrix: "qr" } },
    // 目录模式是独立枚举值，任意其他字符串不能被当作选择器语义。
    { schema: 1, root: { type: "path", bind: "path", mode: "folder" } },
  ];
  for (const view of invalid) {
    assert.equal(validateView(view), false, JSON.stringify(view));
  }
  // 目录选择与二维码只读绑定必须落在合法一侧，避免只加拒绝而漏掉正向形状。
  for (const root of [
    { type: "path", bind: "path", mode: "directory" },
    { type: "qrcode", matrix: "$qr", label: "Scan to pair" },
  ]) {
    assert.equal(validateView({ schema: 1, root }), true, `${root.type}: ${JSON.stringify(validateView.errors)}`);
  }
});

/** 直接调用 Rust 的视图解析入口，避免 Schema 与宿主实际接受范围分离。 */
test("all builder samples pass the Rust view validator, which rejects an unnamed button", async () => {
  const tempRoot = await realpath(tmpdir());
  const directory = await mkdtemp(join(tempRoot, "kerminal-ts-builders-"));
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const protocol = fileURLToPath(new URL("../../protocol", import.meta.url));
  const checker = join(directory, "checker");
  const viewPath = join(directory, "view.json");
  const cargo = process.platform === "win32" ? "cargo.exe" : "cargo";
  const args = ["run", "--offline", "--quiet", "--manifest-path", join(checker, "Cargo.toml"), "--", viewPath];
  const options = {
    cwd: root, timeout: 300_000, windowsHide: true, maxBuffer: 1024 * 1024,
    env: { ...process.env, CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR ?? join(root, "target") },
  };
  try {
    await mkdir(join(checker, "src"), { recursive: true });
    await copyFile(join(root, "Cargo.lock"), join(checker, "Cargo.lock"));
    await writeFile(join(checker, "Cargo.toml"), `[package]\nname = "kerminal-ts-fixture-check"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n\n[dependencies]\nkerminal-plugin-protocol = { path = ${JSON.stringify(protocol)} }\n`);
    await writeFile(join(checker, "src", "main.rs"), `//! 临时调用协议公开入口，避免复制或模拟 Rust 校验规则。\n//! @author kongweiguang\n\n/// 输入来自构建器生成的 JSON；解析失败以非零退出码保留真实协议错误。\nfn main() -> Result<(), Box<dyn std::error::Error>> {\n    let path = std::env::args_os().nth(1).ok_or("missing view path")?;\n    kerminal_plugin_protocol::parse_native_view(&std::fs::read(path)?)?;\n    Ok(())\n}\n`);
    const view = ui.view({
      data: { visible: true, items: [], busy: false, progress: 50 },
      root: ui.stack({ children: Object.keys(samples).map((kind) => ui[kind](samples[kind])) }),
    });
    await writeFile(viewPath, JSON.stringify(view));
    await execFileAsync(cargo, args, options);

    view.root.children.find((node) => node.type === "button").label = undefined;
    await writeFile(viewPath, JSON.stringify(view));
    await assert.rejects(execFileAsync(cargo, args, options),
      (error) => error.code !== 0 && /view\.button/.test(error.stderr));

    view.root = ui.list({ source: "rpc", id: "rows", items: "$items", key: "id", title: "name" });
    await writeFile(viewPath, JSON.stringify(view));
    await assert.rejects(execFileAsync(cargo, args, options),
      (error) => error.code !== 0 && /view\.collection/.test(error.stderr));

    // 类型层只能约束 matrix 为字符串，$ 绑定格式必须交给真实协议入口拒绝；
    // 这里用构建器产出的同一 JSON 结构验证缺失前缀与目录枚举的边界。
    view.root = ui.qrcode({ matrix: "qr" });
    await writeFile(viewPath, JSON.stringify(view));
    await assert.rejects(execFileAsync(cargo, args, options),
      (error) => error.code !== 0 && /view\.qrcode/.test(error.stderr));

    view.root = ui.path({ bind: "folder", mode: "directory" });
    await writeFile(viewPath, JSON.stringify(view));
    await execFileAsync(cargo, args, options);
    view.root.mode = "folder";
    await writeFile(viewPath, JSON.stringify(view));
    // 非法枚举在反序列化阶段就被拒绝，错误位置不一定带 view.path 前缀。
    await assert.rejects(execFileAsync(cargo, args, options),
      (error) => error.code !== 0 && /unknown variant `folder`|view\.path/.test(error.stderr));
  } finally {
    const resolved = await realpath(directory);
    if (dirname(resolved) !== tempRoot || !basename(resolved).startsWith("kerminal-ts-builders-")) {
      throw new Error("临时目录位置发生变化，拒绝递归清理");
    }
    await rm(resolved, { recursive: true, force: true });
  }
});

/** 工具构建器不放行视觉样式，生成文件必须持续跟随协议 Schema。 */
test("schema rejects arbitrary style properties and generated definitions remain current", async () => {
  assert.equal(validateView(ui.view({ root: { type: "text", text: "No", color: "red" } })), false);
  execFileSync(process.execPath, ["scripts/generate-types.mjs", "--check"], { cwd: new URL("..", import.meta.url) });
  for (const name of ["manifest", "view", "rpc"]) {
    const schema = JSON.parse(await readFile(new URL(`../../protocol/schema/${name}.schema.json`, import.meta.url), "utf8"));
    const generated = await readFile(new URL(`../src/generated/${name}.ts`, import.meta.url), "utf8");
    for (const definition of [schema.title, ...Object.keys(schema.$defs)]) {
      assert.match(generated, new RegExp(`export type ${definition} = `));
    }
  }
});

/** 新字段应进入类型，无法表达的结构必须显式失败而不是悄悄宽化。 */
test("type generation reflects schema changes and rejects unsupported structures", () => {
  const changed = structuredClone(viewSchema);
  changed.$defs.NativeNode.oneOf[0].properties.disabled = { type: "boolean" };
  assert.match(render(changed), /"disabled"\?: boolean/);
  changed.$defs.NativeNode.oneOf[0].properties.disabled = { type: "boolean", prefixItems: [] };
  assert.throws(() => render(changed), /不支持的 Schema 关键字: prefixItems/);
});
