// @author kongweiguang

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import Ajv2020 from "ajv/dist/2020.js";

/** 显式区分 null 成功值和错误对象，防止生成类型放行宿主会拒绝的回执。 */
test("RPC schema distinguishes a null result from an invalid null error", async () => {
  const schema = JSON.parse(await readFile(new URL("../../protocol/schema/rpc.schema.json", import.meta.url), "utf8"));
  const validator = new Ajv2020({ allErrors: true, strict: false });
  validator.addFormat("int32", { type: "number", validate: (value) => Number.isInteger(value) && value >= -2147483648 && value <= 2147483647 });
  const validate = validator.compile(schema);
  assert.equal(validate({ jsonrpc: "2.0", id: "p:1", result: null }), true);
  assert.equal(validate({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "invalid request" } }), true);
  assert.equal(validate({ jsonrpc: "2.0", id: null, error: null }), false);
});

/** 同时验证分页层级和排序枚举，防止 schema 漂移掩盖不兼容请求。 */
test("view fetch accepts hierarchy and the frozen sorting directions", async () => {
  const schema = JSON.parse(await readFile(new URL("../../protocol/schema/rpc.schema.json", import.meta.url), "utf8"));
  const validator = new Ajv2020({ allErrors: true, strict: false });
  validator.addFormat("uint64", { type: "number", validate: Number.isInteger });
  const validate = validator.compile({ $defs: schema.$defs, $ref: "#/$defs/ViewFetchParams" });
  const page = { instance: "tab-1", node: "rows", range: [0, 20], parent_key: "parent" };
  assert.equal(validate({ ...page, sort: { column: "name", direction: "asc" } }), true);
  assert.equal(validate({ ...page, sort: { column: "name", direction: "desc" } }), true);
  assert.equal(validate({ ...page, sort: { column: "name", direction: "ascending" } }), false);
  assert.equal(validate({ ...page, sort: { column: "name", direction: "asc", extra: true } }), false);
});
