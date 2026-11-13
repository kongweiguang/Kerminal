<!-- @author kongweiguang -->

# 外部插件示例

| 示例 | 运行方式 | 发布包 |
| --- | --- | --- |
| `acme-notes` | Rust Sidecar + 原生视图 | Windows x86_64、Linux x86_64、macOS x86_64/aarch64 |
| `http-check` | Rust Sidecar + 主机类型 + 原生结果 Tab | Windows x86_64、Linux x86_64、macOS x86_64/aarch64 |
| `file-search` | Rust Sidecar + 包内 WebView | Windows x86_64、Linux x86_64、macOS x86_64/aarch64 |

三个目录都可直接使用正式 CLI 构建。工作目录切到对应目录后运行 `kerminal-plugin build --platform <平台>`、`kerminal-plugin check`、`kerminal-plugin package`；`kerminal.*` 两个官方示例为每条命令加 `--official`。Sidecar 需要 Rust 工具链，四个平台必须分别构建，不得把一个平台的二进制冒充其他平台。CLI 使用 `resources/` 作为静态资源源，`src/views.ts` 默认导出包内视图路径，`src/web.ts` 单独构建为网页 `web/main.js`；acme-notes 与 http-check 的 `scripts/build.mjs` 只用于本地生成派生的 `l10n/`、`schemas/`、`views/`，不编译 Sidecar。

插件启用后完全信任：Sidecar 以当前用户权限运行，具备完整文件、网络和进程能力；WebView 也可直接联网，实际连通范围受浏览器和操作系统网络机制约束，插件不宣称绕过。清单不声明 `network:` 或其他能力门禁，`permissions` 即使保留也只作描述。HTTP 检查由 Sidecar 直接发起网络请求。安装用户无需 Node 或 Rust；示例目录根部的 `l10n/`、`schemas/`、`views/`、`web/` 是构建脚本派生产物，修改静态资产时编辑 `resources/` 与 `src/`。

导出一个独立插件仓库（目标必须不存在）：

```powershell
node sdk/examples/scripts/export.mjs acme-notes C:\plugins\notes
node sdk/examples/scripts/export.mjs http-check C:\plugins\http-check
node sdk/examples/scripts/export.mjs file-search C:\plugins\file-search
```

导出目录包含源码、资源、静态视图构建脚本、清单、开发说明、Sidecar `Cargo.toml`（源目录有 `Cargo.lock` 时一并复制）和 `.github/workflows/release.yml`，不包含本机构建的 Sidecar 二进制或 `.kerminal/` 验收产物。SDK 依赖改用 `@kerminal/sdk`，与 CLI 统一从固定的 `KERMINAL_SDK_REF` 提交构建。工作流核对 tag 与清单版本：`runtime.kind = none` 生成通用包，`sidecar` 在四个平台分别构建；macOS 配置签名与公证 Secret。每个平台包通过正式校验后才上传，收齐全部资产并回读一致后才发布 Release，再输出官方目录登记片段。导出与本地构建不代表已执行外部发布、市场登记、跨平台构建或真实桌面验收。