<!-- @author kongweiguang -->

# __PLUGIN_ID__

需要 Kerminal 随附的 `kerminal-plugin` CLI。作者构建需要 Node.js 24；Rust Sidecar 模板另需 Rust 工具链。安装插件的用户不需要这些工具。

Rust Sidecar 模板使用正式 `kerminal-plugin-sdk`。首次构建前，将 Kerminal SDK 仓库检出到本项目的 `.kerminal-sdk/`，并切换到作者确认的完整提交 SHA；该目录必须包含 `sdk/rust/Cargo.toml`。例如使用 `git clone --no-checkout <SDK仓库地址> .kerminal-sdk` 和 `git -C .kerminal-sdk checkout --detach <完整40位提交SHA>`。CLI 与 SDK 应来自同一提交，不能使用会漂移的默认分支代替固定版本。发布工作流会用 `KERMINAL_SDK_REF` 自动准备同一路径。

```powershell
kerminal-plugin build
kerminal-plugin check
kerminal-plugin dev --kerminal C:\path\to\kerminal.exe
kerminal-plugin package
```

`dev` 使用独立的临时配置根，监听源码变更后重建并请求重载；构建失败保留上一份有效包。结束测试时关闭该 Kerminal 窗口。

`src/main.ts` 是 QuickJS 入口，`src/views.ts` 默认导出包内路径到静态视图的映射。`src/web.ts` 是可选浏览器入口，单独打包为 `web/main.js`；DOM 代码只放在这里。`resources/` 原样复制到包根，不允许符号链接。Rust 模板的二进制名称固定为 `kerminal-plugin-sidecar`，CLI 根据清单版本注入握手身份并构建当前平台，可用 `--platform` 指定其他已安装的目标工具链。

构建产物为 `.kerminal/build/`，`.kplugin` 位于 `.kerminal/packages/`。将生成包交给 Kerminal 的“从文件安装”，安装后重启生效。Sidecar 可执行文件可额外运行 `kerminal-plugin conformance <exe>`；常规模式覆盖握手、身份、帧、心跳及关闭。取消和背压探针需要实现 `conformance.block` 的专用测试插件并显式加 `--fixture-mode`，不能用普通插件的常规通过代替。这些检查不代替真实界面、崩溃恢复和多平台验收。

发布前设置仓库变量 `KERMINAL_SDK_REF` 为包含 CLI 的完整 40 位提交 SHA，可选 `KERMINAL_SDK_REPOSITORY` 指定 SDK 仓库。推送与清单版本完全一致的 `v<version>` 标签会运行 `.github/workflows/release.yml`，通用脚本生成 `any` 包，Sidecar 构建 Windows x86_64、Linux x86_64、macOS x86_64/aarch64 四个资产，并逐平台运行真实 Sidecar 的常规 conformance。macOS 签名与公证所需 Secret 见工作流头部；未配置时有明确警告，不能声称可通过 Gatekeeper。工作流输出市场登记片段，插件 Release 不会自动登记市场。
