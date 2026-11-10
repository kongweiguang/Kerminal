<!-- @author kongweiguang -->

# HTTP Check / HTTP 检查

官方主机类型示例。配置仅接受 `https://example.com/` 及其路径；测试连接执行 HTTP GET，打开连接后创建结果标签，并可在标签中重新检查。状态只保存地址与结果，不读取、保存或显示凭据。返回非 2xx 状态时测试连接失败。

开发时使用 Node.js 24，在本目录运行 `node scripts/build.mjs`，生成随包分发的 `dist/main.js` 与静态视图。安装用户无需 Node。权限只开放 `network:example.com`。

运行需要脚本宿主提供受权限约束的 `fetch`、`AbortController`，并将已验证的官方插件来源传入脚本宿主；当前实现这些部分仍在集成中，桌面端到端流程尚未验收。
