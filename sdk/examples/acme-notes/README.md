<!-- @author kongweiguang -->

# Workspace Notes / 工作区笔记

Rust Sidecar 示例：侧栏显示已保存内容，中央标签编辑笔记，设置页提供预览选项，`Ctrl+Alt+N` 检查 `example.com`。保存的内容存入插件状态；标签快照优先恢复当前实例草稿。静态界面由 SDK 构建器生成。插件启用后完全信任：Sidecar 以当前用户权限运行，具备完整文件、网络和进程能力；清单不再承担能力门禁，通知与剪贴板都由 Sidecar 直接调用。

开发时使用 Node.js 24 运行 `node scripts/build.mjs`，只生成随包的静态视图与资源。Sidecar 二进制由 `kerminal-plugin build --platform <platform>` 构建，构建时注入清单版本作为握手身份；安装用户无需 Node 或 Rust。示例不声明 `network:example.com`，也不要求任何联网权限：网络检查由 Sidecar 直接完成，实际连通范围受操作系统网络机制约束，插件不宣称绕过。

宿主需要实现插件状态 API、设置读取、`view.update` 侧栏补丁，以及在 `view.snapshot`/`view.close` 传入当前视图数据以完整恢复未提交草稿；当前冻结协议的实例参数尚不包含该数据，因此仅凭插件无法保证未触发动作的输入可以跨重启恢复。宿主原生界面负责脏字段保护。本示例不声称桌面端到端验收通过。

供 P7 构造两个版本：分别运行 `node scripts/stage-version.mjs 1.0.0 <新目录>` 与 `node scripts/stage-version.mjs 1.1.0 <新目录>`。两者都会生成可独立构建的 Sidecar 源码目录；1.1.0 在编辑器增加“复制笔记”，复制由 Sidecar 自身完成，1.0.0 不提供该能力。版本脚本不修改 `permissions`，因为该字段只作描述，不参与能力开关。设置 `KERMINAL_PLUGIN_CLI` 指向 `kerminal-plugin` 时，脚本会直接构建并输出真实包目录，否则目录仍是源码，需要再执行 `kerminal-plugin build --platform <平台>`。各目录仍须使用正式打包和安装流程，生成目录本身不代表已发布。