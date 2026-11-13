// @author kongweiguang

import type { NativeNode, NativeView } from "./generated/view.js";

export type NodeKind = NativeNode["type"];
export type NodeOf<Kind extends NodeKind> = Extract<NativeNode, { type: Kind }>;
/** 对联合类型逐支去掉 type，避免按钮和分页节点的条件约束在映射时丢失。 */
type WithoutNodeType<Node> = Node extends NativeNode ? Omit<Node, "type"> : never;
export type NodeOptions<Kind extends NodeKind> = WithoutNodeType<NodeOf<Kind>>;

/** 每个构建器仅在插件打包时拼出静态 JSON，运行时结构仍由宿主校验。 */
function builder<Kind extends NodeKind>(type: Kind) {
  return (options: NodeOptions<Kind>): NodeOf<Kind> => ({ ...options, type }) as unknown as NodeOf<Kind>;
}

/** 固定视图版本并保留扩展字段，不在运行时提供动态结构操作。 */
function view(options: Omit<NativeView, "schema">): NativeView {
  return { ...options, schema: 1 };
}

export const ui = Object.freeze({
  view,
  stack: builder("stack"),
  row: builder("row"),
  section: builder("section"),
  scroll: builder("scroll"),
  split: builder("split"),
  tabs: builder("tabs"),
  text: builder("text"),
  markdown: builder("markdown"),
  badge: builder("badge"),
  icon: builder("icon"),
  code: builder("code"),
  progress: builder("progress"),
  // 构建器只构造节点数据，不做运行时校验；矩阵必须来自实例数据的只读绑定
  // 这一约束由 Schema 与 Rust 校验执行，构建器入口不额外复制该规则。
  qrcode: builder("qrcode"),
  empty: builder("empty"),
  notice: builder("notice"),
  input: builder("input"),
  textarea: builder("textarea"),
  number: builder("number"),
  select: builder("select"),
  switch: builder("switch"),
  checkbox: builder("checkbox"),
  path: builder("path"),
  secret: builder("secret"),
  button: builder("button"),
  toolbar: builder("toolbar"),
  menu: builder("menu"),
  list: builder("list"),
  table: builder("table"),
  tree: builder("tree"),
  form: builder("form"),
});
