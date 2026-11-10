<!-- @author kongweiguang -->

# 外部插件示例

| 示例 | 运行方式 | 发布包 |
| --- | --- | --- |
| `acme-notes` | QuickJS + 原生视图 | `any` |
| `http-check` | QuickJS + 主机类型 + 原生结果 Tab | `any` |
| `file-search` | Rust Sidecar + 包内 WebView | Windows x86_64、Linux x86_64、macOS x86_64/aarch64 |

三个目录都可直接使用正式 CLI 构建。工作目录切到对应目录后运行 `kerminal-plugin build`、`kerminal-plugin check`、`kerminal-plugin package`；`kerminal.*` 两个官方示例为每条命令加 `--official`。文件搜索另需 Rust，支持 `--platform` 指定目标。CLI 使用 `resources/` 作为静态资源源，`src/views.ts` 默认导出包内视图路径，`src/web.ts` 单独构建为网页 `web/main.js`。脚本示例仍可用 Node.js 24 运行 `scripts/build.mjs`，生成现有原生验收使用的可直接加载目录；根目录的 `l10n/`、`schemas/`、`web/` 是这一流程的派生产物，修改静态资产时编辑 `resources/`。

导出一个独立插件仓库（目标必须不存在）：

```powershell
node sdk/examples/scripts/export.mjs acme-notes C:\plugins\notes
node sdk/examples/scripts/export.mjs http-check C:\plugins\http-check
node sdk/examples/scripts/export.mjs file-search C:\plugins\file-search
```

导出目录包含源码、资源、清单、开发说明和 `.github/workflows/release.yml`，不包含本机构建的 Sidecar。SDK 依赖改用 `@kerminal/sdk`，与 CLI 统一从固定的 `KERMINAL_SDK_REF` 提交构建。工作流核对 tag 与清单版本，脚本生成通用包，Sidecar 在四个平台分别构建；macOS 配置签名与公证 Secret。每个平台包通过正式校验后才上传，收齐全部资产并回读一致后才发布 Release，再输出官方目录登记片段。导出与本地构建不代表已执行外部发布、市场登记、跨平台构建或真实桌面验收。
