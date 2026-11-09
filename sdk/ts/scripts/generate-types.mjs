// @author kongweiguang

import { readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const sdkDirectory = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const schemaDirectory = resolve(sdkDirectory, "../protocol/schema");
const generatedDirectory = resolve(sdkDirectory, "src/generated");
const check = process.argv.includes("--check");
const keywords = new Set([
  "$defs", "$ref", "$schema", "additionalProperties", "allOf", "anyOf", "const", "default",
  "contains", "description", "enum", "format", "if", "items", "maximum", "maxItems", "maxLength",
  "maxProperties", "minimum", "minItems", "minLength", "minProperties", "not", "oneOf", "pattern",
  "patternProperties", "properties", "propertyNames", "required", "then", "title", "type", "uniqueItems",
]);

/** 显式拒绝未知结构，协议升级不能被生成器悄悄降级为宽泛类型。 */
function checkKeywords(schema) {
  for (const key of Object.keys(schema)) {
    if (!keywords.has(key)) throw new Error(`不支持的 Schema 关键字: ${key}`);
  }
}

/** 只接受协议中已知的条件谓词，复杂数组条件交由 Schema 和 Rust 执行。 */
function checkConditionalPredicate(schema) {
  if (!schema || typeof schema !== "object" || Array.isArray(schema)) throw new Error("无效的条件约束");
  for (const [key, value] of Object.entries(schema)) {
    if (!["const", "contains", "properties", "required", "type"].includes(key)) {
      throw new Error(`不支持的条件约束: ${key}`);
    }
    if (key === "properties") {
      if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("无效的条件属性");
      for (const child of Object.values(value)) checkConditionalPredicate(child);
    } else if (key === "contains") {
      checkConditionalPredicate(value);
    } else if (key === "required" && (!Array.isArray(value) || value.some((name) => typeof name !== "string"))) {
      throw new Error("无效的条件必填属性");
    }
  }
}

/** 单字段枚举条件可表达为联合类型；其余复杂条件由运行时校验保证。 */
function conditionalType(schema, inheritedProperties) {
  if (!schema.if || !schema.then) throw new Error("条件约束必须同时提供 if 与 then");
  checkConditionalPredicate(schema.if);
  checkConditionalPredicate(schema.then);
  const condition = schema.if;
  const required = condition.required;
  const properties = condition.properties;
  if (Array.isArray(required) && required.length === 1 && properties
      && Object.keys(condition).every((key) => key === "required" || key === "properties")
      && Object.keys(properties).length === 1 && Object.hasOwn(properties, required[0])
      && Object.keys(properties[required[0]]).length === 1
      && Object.hasOwn(properties[required[0]], "const")) {
    const name = required[0];
    const original = inheritedProperties[name];
    if (!original) throw new Error(`条件属性缺少定义: ${name}`);
    const literal = JSON.stringify(properties[name].const);
    const base = typeOfSchema(original, inheritedProperties);
    const consequence = typeOfSchema(schema.then, inheritedProperties);
    return `(({ ${JSON.stringify(name)}: ${literal}; } & ${consequence}) | { ${JSON.stringify(name)}?: Exclude<${base}, ${literal}>; })`;
  }
  return "JsonValue";
}

/** 保留对象的必填字段与 x- 扩展键；值约束仍由协议校验器执行。 */
function objectType(schema, inheritedProperties) {
  const properties = schema.properties ?? {};
  const required = new Set(schema.required ?? []);
  const names = new Set([...Object.keys(properties), ...required]);
  const fields = [];
  const additional = schema.additionalProperties;
  for (const name of names) {
    const property = properties[name] ?? inheritedProperties[name] ?? (additional !== false ? additional : undefined);
    if (property === undefined) throw new Error(`必填属性缺少定义: ${name}`);
    fields.push(`${JSON.stringify(name)}${required.has(name) ? "" : "?"}: ${typeOfSchema(property)};`);
  }
  if (schema.not !== undefined) {
    const excluded = schema.not.required;
    if (Object.keys(schema.not).length !== 1 || !Array.isArray(excluded) || excluded.length !== 1) {
      throw new Error("不支持的 Schema not 约束");
    }
    fields.push(`${JSON.stringify(excluded[0])}?: never;`);
  }
  if (schema.patternProperties !== undefined) {
    const entries = Object.entries(schema.patternProperties);
    if (entries.length !== 1 || !["^x-.+", "^x-[\\s\\S]+"].includes(entries[0][0])) {
      throw new Error("不支持的扩展字段规则");
    }
    fields.push(`[extension: \`x-\${string}\`]: ${typeOfSchema(entries[0][1])};`);
  }
  if (additional !== undefined && additional !== false) {
    const dynamic = `Record<string, ${typeOfSchema(additional)}>`;
    return fields.length ? `{ ${fields.join(" ")} } & ${dynamic}` : dynamic;
  }
  return fields.length ? `{ ${fields.join(" ")} }` : "Record<string, never>";
}

/** 将协议使用的 Schema 结构转为类型，元数据和数值范围留给运行时校验。 */
function typeOfSchema(schema, inheritedProperties = {}) {
  if (schema === true) return "JsonValue";
  if (schema === false) return "never";
  if (!schema || typeof schema !== "object" || Array.isArray(schema)) throw new Error("无效的 Schema");
  checkKeywords(schema);
  if (schema.if !== undefined || schema.then !== undefined) {
    if (Object.keys(schema).some((key) => key !== "if" && key !== "then")) {
      throw new Error("不支持组合其他约束的条件分支");
    }
    return conditionalType(schema, inheritedProperties);
  }
  if (schema.not && Object.keys(schema).length === 1 && typeof schema.not.pattern === "string"
      && Object.keys(schema.not).length === 1) return "JsonValue";
  if (schema.contains !== undefined) throw new Error("contains 只支持在条件谓词中使用");
  if (schema.$ref !== undefined) {
    const match = /^#\/\$defs\/([A-Za-z_$][\w$]*)$/.exec(schema.$ref);
    if (!match) throw new Error(`不支持的 Schema 引用: ${schema.$ref}`);
    return match[1];
  }
  if (Object.hasOwn(schema, "const")) return JSON.stringify(schema.const);
  if (schema.enum !== undefined) return schema.enum.map((item) => JSON.stringify(item)).join(" | ") || "never";
  if (Array.isArray(schema.type)) {
    return schema.type.map((type) => typeOfSchema({ ...schema, type }, inheritedProperties)).join(" | ");
  }
  const parts = [];
  if (schema.type === "string" || (!schema.type && schema.pattern)) {
    const prefix = typeof schema.pattern === "string" && /^\^([A-Za-z0-9._:/-]+)/.exec(schema.pattern);
    parts.push(prefix ? `\`${prefix[1]}\${string}\`` : "string");
  } else if (schema.type === "integer" || schema.type === "number") {
    parts.push("number");
  } else if (schema.type === "boolean" || schema.type === "null") {
    parts.push(schema.type);
  } else if (schema.type === "array") {
    const item = typeOfSchema(schema.items ?? true);
    if (Number.isInteger(schema.minItems) && schema.minItems === schema.maxItems && schema.minItems <= 16) {
      parts.push(`[${Array(schema.minItems).fill(item).join(", ")}]`);
    } else if (Number.isInteger(schema.minItems) && schema.minItems > 0 && schema.minItems <= 16) {
      parts.push(`[${Array(schema.minItems).fill(item).join(", ")}, ...Array<${item}>]`);
    } else {
      parts.push(`Array<${item}>`);
    }
  } else if (schema.type === "object" || schema.properties || schema.required || schema.not) {
    parts.push(objectType(schema, inheritedProperties));
  } else if (schema.type !== undefined) {
    throw new Error(`不支持的 Schema 类型: ${schema.type}`);
  }
  const context = { ...inheritedProperties, ...schema.properties };
  for (const key of ["oneOf", "anyOf", "allOf"]) {
    if (schema[key]) {
      const separator = key === "allOf" ? " & " : " | ";
      const variants = schema[key].map((item) => typeOfSchema(item, context));
      const retained = key === "allOf" ? variants.filter((item) => item !== "JsonValue") : variants;
      if (retained.length) parts.push(`(${retained.join(separator)})`);
    }
  }
  return parts.join(" & ") || "JsonValue";
}

/** 显式导出所有定义，RPC 方法参数无需被信封直接引用也能供插件作者使用。 */
export function render(schema) {
  if (!schema.title || !schema.$defs) throw new Error("Schema 缺少标题或定义");
  const declarations = [[schema.title, schema], ...Object.entries(schema.$defs)];
  return [
    "// 由 sdk/protocol/schema 生成；请运行 npm run generate 更新。",
    "// @author kongweiguang",
    "",
    'import type { JsonValue } from "../json.js";',
    "",
    ...declarations.map(([name, definition]) => `export type ${name} = ${typeOfSchema(definition)};`),
    "",
  ].join("\n");
}

/** 使用 Rust 派生的 Schema 生成类型，检查模式不能修改已有产物。 */
async function generate(schemaName) {
  const input = resolve(schemaDirectory, `${schemaName}.schema.json`);
  const output = resolve(generatedDirectory, `${schemaName}.ts`);
  const schema = JSON.parse(await readFile(input, "utf8"));
  const content = render(schema);
  if (check) {
    const existing = await readFile(output, "utf8").catch((error) => {
      if (error.code === "ENOENT") return "";
      throw error;
    });
    if (existing !== content) throw new Error(`${schemaName} 类型与协议 Schema 不一致，请运行 npm run generate`);
  } else {
    await writeFile(output, content);
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  await Promise.all(["manifest", "view", "rpc"].map(generate));
}
