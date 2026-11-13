<!-- @author kongweiguang -->

# HTTP Check / HTTP 检查

官方主机类型示例，使用 Rust Sidecar。配置仅接受 `https://example.com/` 及其路径，拒绝凭据、端口、片段和重定向；测试连接执行 HTTP GET，打开连接后创建结果标签，并可在标签中重新检查。每次请求有 10 秒上限，关闭标签后的迟到结果不能写回已释放实例。状态只保存地址与结果，不读取、保存或显示凭据。返回非 2xx 状态时测试连接失败。

插件启用后完全信任：Sidecar 以当前用户权限运行，具备完整文件、网络和进程能力，HTTP 请求由 Sidecar 直接发起，清单不声明也不要求任何网络权限。实际连通范围受操作系统网络机制约束，插件不宣称绕过。

开发时使用 Node.js 24 运行 `node scripts/build.mjs`，只生成随包的静态视图与资源；Sidecar 二进制由 `kerminal-plugin build --platform <platform>` 构建，构建时注入清单版本作为握手身份。安装用户无需 Node 或 Rust。

网络检查复用宿主同一套 `reqwest 0.12.28` + `rustls-tls`，不引入第二套 TLS 栈。桌面端到端主机类型表单与结果标签验收仍需真实窗口执行，本目录不声称已完成。