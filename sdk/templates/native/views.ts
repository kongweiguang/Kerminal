// @author kongweiguang

// 静态对象可替换为 @kerminal/sdk 的 ui.view 等构建器，构建时统一序列化。
export default {
  "views/panel.json": {
    schema: 1,
    data: {},
    root: { type: "text", text: "%panel.body%" },
  },
};
