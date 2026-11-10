// @author kongweiguang

declare const kerminal: { onRequest(method: string, handler: (params: any) => unknown): void };

/** 初始值由插件进程提供，视图结构始终由安装前生成的静态 JSON 决定。 */
kerminal.onRequest("view.open", () => ({ data: {} }));

/** 未保存状态的示例显式返回空对象，宿主无需等待外部资源。 */
kerminal.onRequest("view.close", () => ({}));
