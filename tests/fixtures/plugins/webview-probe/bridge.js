// @author kongweiguang
// 只通过 Wry 注入的消息通道回报；探针不冒充尚未开放的正式插件 API。
export function send(value) { window.ipc.postMessage(JSON.stringify(value)); }
