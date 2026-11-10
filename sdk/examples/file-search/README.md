<!-- @author kongweiguang -->

# File Search / 文件搜索

包内 WebView 展示本地文件名搜索结果；Rust Sidecar 从指定目录递归搜索，不跟随符号链接，最多遍历 100,000 项、返回 500 项；达到上限或遇到无法读取的目录时界面提示结果可能不完整。结果分批通过 `view.update` 推送，并发送 `$/progress`。取消时 Sidecar 杀掉并等待自己的工作子进程，关闭标准输入或正常关闭时也会停止并回收搜索任务。Sidecar 崩溃后的重启与进程树强制回收由 Kerminal 监督器负责；恢复只保留路径和查询，旧结果需要重新搜索。

本示例按语言中立的 NDJSON / JSON-RPC 协议自行实现 stdio、任务取消和工作进程回收，用于展示不依赖特定语言 SDK 的接入方式。`kerminal-plugin create --template sidecar-rust` 生成的模板使用正式 Rust SDK，由 SDK 接管握手、分帧、路由、并发与取消。

使用 Node.js 24 运行 `node scripts/build.mjs` 生成 `web/main.js`。构建后按平台将真实 Sidecar 放入清单对应的 `bin/` 路径：Windows x86_64 使用 `bin/file-search.exe`，Linux x86_64 与 macOS x86_64/aarch64 使用各自平台包中的 `bin/file-search`。四个平台必须各自构建，不得将一个平台的二进制冒充其他平台；macOS 发行包还需签名和公证。安装用户无需 Node 或 Rust。

网页只使用 `window.kerminal`，通过 `view.action` 发出搜索和草稿动作，以 `AbortSignal` 取消，通过 `view.update` 接收结果；宿主必须把动作路由到该实例的 Sidecar，并将更新送回该 WebView。当前宿主路由仍在集成，该桥接、崩溃重启及进程树回收尚未做端到端验收。
