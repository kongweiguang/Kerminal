<!-- @author kongweiguang -->

# Workspace Notes / 工作区笔记

纯 TypeScript 脚本插件示例：侧栏显示已保存内容，中央标签编辑笔记，设置页提供预览选项，`Ctrl+Alt+N` 检查 `example.com`。保存的内容存入插件状态；标签快照优先恢复当前实例草稿。静态界面由 SDK 构建器生成。

开发时使用 Node.js 24，在本目录运行 `node scripts/build.mjs`。随包的 `dist/main.js` 和 `views/*.json` 可直接加载，安装用户无需 Node。清单只开放 `network:example.com` 与通知权限，网络检查不会将笔记内容上传。

宿主需要实现受白名单约束的 `fetch`、插件状态 API，以及在 `view.snapshot`/`view.close` 传入当前视图数据以完整恢复未提交草稿；当前冻结协议的实例参数尚不包含该数据，因此仅凭插件无法保证未触发动作的输入可以跨重启恢复。宿主原生界面负责脏字段保护。本示例不声称桌面端到端验收通过。

供 P7 构造两个版本的实际包目录：分别运行 `node scripts/stage-version.mjs 1.0.0 <新目录>` 与 `node scripts/stage-version.mjs 1.1.0 <新目录>`。1.1.0 在编辑器增加“复制笔记”，并新增 `clipboard.write` 权限；1.0.0 不提供该能力。目标目录必须不存在。各目录仍须使用正式打包和安装流程，生成目录本身不代表已发布。
