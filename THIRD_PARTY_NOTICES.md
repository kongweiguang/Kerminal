<!-- @author kongweiguang -->

# 第三方来源与许可证说明

审计范围：GPUI 重建 worktree 的源码、资源、静态命令元数据和锁定依赖；核对日期：2026-10-17。

## 本项目许可证

本工作区的项目代码采用 GNU GPL-3.0-or-later，许可证正文见根目录 [LICENSE](LICENSE)。这是 Kerminal 自身代码的许可证，不代表下文 API 依赖的代码被复制进源码。

## 源码与静态数据复用审计

除下表明确登记的 vendored 源码外，已检查的终端、连接、文件、偏好设置、编辑器、运维、MCP 与工作区实现没有逐段复制或改写其他项目的源码、静态数据或资源。模块通过 Cargo 依赖调用公开 API 的情况与 vendored 源码复用分开记录。

| 来源 | 固定来源 | 本项目中的关系 | 许可证情况 |
| --- | --- | --- | --- |
| GPUI core | [gpui-pre 0.3.9，Zed commit `93d745668805029476e6d7fbb19652116fed51de`](https://github.com/zed-industries/zed/tree/93d745668805029476e6d7fbb19652116fed51de/crates/gpui) | `vendor/gpui-pre` 保留上游源码，加入共享输入 handler ownership、focus identity、按 focus 路由及显式窗口可见性能力接口；支持 Windows IME composition handler pin 与桌面隐藏恢复。补丁边界见 [VENDOR_SOURCE.md](vendor/gpui-pre/VENDOR_SOURCE.md)。 | Apache-2.0；保留原 [LICENSE-APACHE](vendor/gpui-pre/LICENSE-APACHE)。 |
| GPUI Linux 平台适配 | [gpui-pre-linux 0.3.9，Zed commit `93d745668805029476e6d7fbb19652116fed51de`](https://github.com/zed-industries/zed/tree/93d745668805029476e6d7fbb19652116fed51de/crates/gpui_linux) | `vendor/gpui-pre-linux` 保留上游平台实现，修复 Linux X11/Wayland IME 焦点归属及 Wayland `done(serial)` 路由、MIME 准确的 Wayland 文本/图像剪贴板读写，并加入 X11 map/unmap 和 Wayland XDG unmap/remap；来源与补丁范围见 [VENDOR_SOURCE.md](vendor/gpui-pre-linux/VENDOR_SOURCE.md)。 | Apache-2.0；保留原 [LICENSE-APACHE](vendor/gpui-pre-linux/LICENSE-APACHE)。 |
| GPUI macOS 平台适配 | [gpui-pre-macos 0.3.9，Zed commit `93d745668805029476e6d7fbb19652116fed51de`](https://github.com/zed-industries/zed/tree/93d745668805029476e6d7fbb19652116fed51de/crates/gpui_macos) | `vendor/gpui-pre-macos` 保留上游平台实现，加入焦点绑定的 AppKit marked-text 路由、状态机回归测试和原生窗口隐藏恢复，并将处理器注册前到达的 URL 批次按序暂存、锁外投递；异步回调无法携带来源标记，取消保护限于同步 `discardMarkedText` 回调。补丁边界见 [VENDOR_SOURCE.md](vendor/gpui-pre-macos/VENDOR_SOURCE.md)。 | Apache-2.0；保留原 [LICENSE-APACHE](vendor/gpui-pre-macos/LICENSE-APACHE)。 |
| GPUI Windows 平台适配 | [gpui-pre-windows 0.3.9，Zed commit `93d745668805029476e6d7fbb19652116fed51de`](https://github.com/zed-industries/zed/tree/93d745668805029476e6d7fbb19652116fed51de/crates/gpui_windows) | `vendor/gpui-pre-windows` 保留上游源文件，加入共享输入 handler、IME composition focus pin、嵌套事件 epoch 保护及保留窗口位置和最大化状态的隐藏恢复；补丁范围和来源见 [VENDOR_SOURCE.md](vendor/gpui-pre-windows/VENDOR_SOURCE.md)。 | Apache-2.0；保留原 [LICENSE-APACHE](vendor/gpui-pre-windows/LICENSE-APACHE)。 |
| GPUI Kit 默认图标 | [gpui-kit-assets 0.7.3 发布包](https://docs.rs/crate/gpui-kit-assets/0.7.3/source/)（由 `gpui-kit 0.7.3` 引入） | 使用 GPUI Kit 提供的图标资产 API；本仓没有复制该 crate 的 SVG。发布包除 crate 的 Apache-2.0 元数据外，还附带 Lucide 图标集及其独立来源说明。 | 原包 `LICENSE-LUCIDE` 已原样保存在 [GPUI_KIT_LUCIDE_LICENSE.txt](licenses/GPUI_KIT_LUCIDE_LICENSE.txt)：其中列明 Lucide Icons and Contributors 的 ISC 条款，以及所列 Feather 衍生图标的 MIT 条款。 |
| 本地 PTY | [portable-pty 0.9.0，上游包记录的 WezTerm revision `f8921727a11b9f8b073e8c24821d72fd41283500`](https://github.com/wezterm/wezterm/tree/f8921727a11b9f8b073e8c24821d72fd41283500/pty) | `vendor/portable-pty` 保留上游 crate 并增加 Windows 原子 Job Object 启动适配；来源及局部改动见 [UPSTREAM.md](vendor/portable-pty/UPSTREAM.md)。 | MIT；保留原 [LICENSE.md](vendor/portable-pty/LICENSE.md)。 |
| 单实例 listener 生命周期 | [single-application 0.1.1，上游包记录的 revision `f38a3f11e2cc56addca76e70d580dace0b974ec1`](https://github.com/mengyou1024/single-application/tree/f38a3f11e2cc56addca76e70d580dace0b974ec1) | `vendor/single-application` 保留原 crate，为 Tokio primary listener 增加有界连接/128 KiB frame、取消和 deadline-aware join，并新增可返回传输错误的发送 API；消息 JSON framing 未改变。来源与补丁范围见 [VENDOR_SOURCE.md](vendor/single-application/VENDOR_SOURCE.md)。 | MIT；保留上游 [LICENSE](vendor/single-application/LICENSE)。 |
| 更新 SDK | [cargo-packager-updater 0.2.3，commit `2e71af6d882157e706062b99f978c1723c9c5c64`](https://github.com/crabnebula-dev/cargo-packager/tree/2e71af6d882157e706062b99f978c1723c9c5c64/crates/updater) | `vendor/cargo-packager-updater` 沿用精确发布包，局部修正 Windows NSIS 的 PowerShell 启动参数；保留 SDK 的下载、签验、解包和其它平台实现。来源与边界见 [UPSTREAM_PATCHES.md](vendor/cargo-packager-updater/UPSTREAM_PATCHES.md)。补丁仍在整合验收，不能视作实际升级通过。 | Apache-2.0 OR MIT；保留原 [LICENSE_APACHE-2.0](vendor/cargo-packager-updater/LICENSE_APACHE-2.0) 与 [LICENSE_MIT](vendor/cargo-packager-updater/LICENSE_MIT)。 |
| Zed terminal 键位映射 | [keys.rs，commit `80f1bb45fa5b8f278a98a6e38a016c2f89c7eb12`](https://github.com/zed-industries/zed/blob/80f1bb45fa5b8f278a98a6e38a016c2f89c7eb12/crates/terminal/src/mappings/keys.rs) | `crates/terminal/src/input.rs` 是本项目独立实现，仅参考传统终端键位行为；没有移植 Zed 的类型、函数或代码片段。该实现另按 Kitty/xterm 协议处理完整键盘模式、IME 与粘贴。 | 被参考的 Zed terminal crate 在该提交的 manifest 标为 GPL-3.0-or-later；其 [LICENSE-GPL](https://github.com/zed-industries/zed/blob/80f1bb45fa5b8f278a98a6e38a016c2f89c7eb12/LICENSE-GPL) 仅作为来源许可证记录。本项目没有复制该 crate 的代码。 |
| 其他 GPUI 终端项目与 shell-rs | 各上游仓库仅用于产品与实现调研 | 包括调研过的 11 个 GPUI 终端项目；仅作设计参考，不作为依赖，未复制其源码或资源。 | 不产生本项目的源码复用声明。 |
| 命令规格 TOML | 冻结旧版 Kerminal 基线 `17c5d4be46172ca508f92546bfae5fe5ef477f11` 中的 49 个 `src-tauri/command-specs/v1/**` 文件 | `crates/infrastructure/resources/command-specs/v1/**` 是项目维护的命令元数据；旧版与本次基线逐文件一致。条目中的手册、Git 文档等 URL 是信息来源链接，不表示复制了对应上游说明文本。 | 由 Kerminal 项目维护；不是随包带入的上游命令手册或源码。 |
| MCP 工具目录与 fixture | 同一冻结旧版 Kerminal 基线 `17c5d4be46172ca508f92546bfae5fe5ef477f11` 的运行态工具契约 | `crates/kerminal-mcp/resources/tool_catalog.json` 与测试 fixture 是 Kerminal 自有版本化契约数据；Agent 配置模板也是本项目编写。它们不含 Codex、Claude、Pi 或其他 Agent 产品的源码/资源。 | 由 Kerminal 项目维护，不是第三方静态资源。 |
| Kerminal 品牌图标 | 冻结旧版 Kerminal 基线 `17c5d4be46172ca508f92546bfae5fe5ef477f11` 的 `src-tauri/icons/{icon.ico,icon.icns,32x32.png,128x128.png,128x128@2x.png}` | `resources/icons/` 中这五个文件与该基线对应文件逐字节一致，作为原有品牌资源沿用；未从 GPUI Kit 或其他第三方包复制。 | 本次核查未找到基线之前的设计者或独立授权记录，因此只记录可验证的项目来源，不推断更早的作者或许可。 |

该结论针对本次重建中终端、连接、文件、偏好设置、编辑器、运维、MCP/Agent、配置存储与组合根等模块 owner 已确认的范围，并由源码引用检索交叉核对。新增外部源码、生成数据、图标或字体时，必须在来源文件、固定版本和许可证确认后更新本说明及自动依赖报告。

## API 依赖示例

以下是锁文件中与核心架构相关的 API 依赖示例，不是完整依赖清单，也不表示其源码由本项目复制。实际版本以 [Cargo.lock](Cargo.lock) 为准；许可证标识来自相应锁定 crate 的 Cargo 包元数据。传递依赖、构建依赖与平台依赖的全量报告由下节的 `cargo-about` 生成。

| Crate | 锁定版本 | 用途 | 包元数据中的许可证 | 上游来源 |
| --- | --- | --- | --- | --- |
| `gpui-kit` | 0.7.3 | GPUI 通用界面组件 API | Apache-2.0 | [longbridge/gpui-kit](https://github.com/longbridge/gpui-kit) |
| `gpui-kit-assets` | 0.7.3（`gpui-kit` 的传递依赖） | GPUI Kit 默认图标资产 | crate 元数据为 Apache-2.0；图标集另见随包的 [Lucide/Feather 许可文本](licenses/GPUI_KIT_LUCIDE_LICENSE.txt) | [gpui-kit-assets 0.7.3 发布包](https://docs.rs/crate/gpui-kit-assets/0.7.3/source/) |
| `gpui-pre` | 0.3.9（本地 patch） | GPUI snapshot API 与共享输入 handler seam | Apache-2.0 | [zed-industries/zed](https://github.com/zed-industries/zed) |
| `gpui-pre-linux` | 0.3.9（本地 patch） | GPUI Linux X11/Wayland IME 焦点路由 | Apache-2.0 | [zed-industries/zed](https://github.com/zed-industries/zed) |
| `gpui-pre-macos` | 0.3.9（本地 patch） | GPUI AppKit marked-text、窗口可见性与早到 URL 路由 | Apache-2.0 | [zed-industries/zed](https://github.com/zed-industries/zed) |
| `gpui-pre-windows` | 0.3.9（本地 patch） | GPUI Windows IME composition 路由 | Apache-2.0 | [zed-industries/zed](https://github.com/zed-industries/zed) |
| `gpui-pre-macros`、`gpui-pre-platform` | 0.3.9 | GPUI snapshot 配套 API | Apache-2.0 | [zed-industries/zed](https://github.com/zed-industries/zed) |
| `alacritty_terminal` | 0.26.0 | 终端仿真器 API | Apache-2.0 | [alacritty/alacritty](https://github.com/alacritty/alacritty) |
| `portable-pty` | 0.9.0（本地 patch） | 本地 PTY API | MIT | [wezterm/wezterm](https://github.com/wezterm/wezterm) |
| `linkify` | 0.11.0 | 终端普通网址发现；与 OSC 8 共享安全校验和 cell 映射 | MIT OR Apache-2.0 | [robinst/linkify](https://github.com/robinst/linkify) |
| `single-application` | 0.1.1（本地 patch） | 单实例 socket 与消息收发 | MIT | [mengyou1024/single-application](https://github.com/mengyou1024/single-application) |
| `russh`、`bssh-russh-sftp` | 0.62.5、2.3.0 | SSH 与 SFTP API | Apache-2.0 | [warp-tech/russh](https://github.com/warp-tech/russh)、[lablup/bssh](https://github.com/lablup/bssh) |
| `justelnet-core`、`tokio-serial`、`serialport` | 0.1.0、5.5.0、4.10.1 | Telnet 与串口 API | MIT OR Apache-2.0、MIT、MPL-2.0 | [justelnet](https://github.com/kihyun1998/justelnet)、[tokio-serial](https://github.com/berkowski/tokio-serial)、[serialport-rs](https://github.com/serialport/serialport-rs) |

本表只帮助读者辨认核心技术来源。所有目标平台实际使用的直接与传递依赖，以及相应原始许可证正文，应以生成的依赖报告为准；不可将本表当作完整合规清单。

## 本轮补充核实的依赖来源

锁定依赖图中 `tray-icon 0.26.1` 引入 `ksni 0.3.6`。该发布包的 Cargo 元数据声明 `license = "Unlicense"`，并附带 `UNLICENSE`；上游仓库也将该文件列为其许可证。Unlicense 是 SPDX 标准标识，定义为公有领域贡献声明；GNU 项目将其公有领域条款及附加宽松许可说明为与 GNU GPL 兼容。因此，自动报告的全局接受集只增加这个已核实标识，不放宽 GPL 项目代码或其他未审许可证。自动报告会包含发布包对应的完整许可文本。

- [ksni 上游仓库及许可证入口](https://github.com/iovxw/ksni)
- [ksni 0.3.6 发布包元数据与文件](https://docs.rs/crate/ksni/0.3.6)
- [SPDX：Unlicense 标识及正文](https://spdx.org/licenses/Unlicense.html)
- [GNU：各类许可证及其评论（Unlicense 与 GPL 兼容性）](https://www.gnu.org/licenses/license-list.zh-cn.html)

`crates/infrastructure/src/platform/updater/{metadata,download,runtime,settings}.rs` 调用固定版本 `cargo-packager-updater 0.2.3` 的公开 API。业务适配逻辑由 Kerminal 编写；SDK 原始源码及 Windows 启动补丁单独登记于上表，不混入业务实现。发布包元数据声明 `Apache-2.0 OR MIT`，完整依赖许可证正文由自动报告提供。上游信息见 [crabnebula-dev/cargo-packager](https://github.com/crabnebula-dev/cargo-packager)。

## 锁定依赖许可证报告

使用已审查的 [about.toml](about.toml) 与 [HTML 模板](licenses/third-party-dependencies.hbs)，从当前工作区和 Cargo 锁定依赖图生成报告：

```powershell
cargo about generate licenses/third-party-dependencies.hbs > licenses/THIRD_PARTY_DEPENDENCIES.html
```

报告覆盖 Windows x64、macOS x64/arm64、Linux x64 目标，包含传递依赖与构建依赖，并排除仅用于测试的开发依赖。配置只接受已列入审查的 SPDX 许可证标识；不能解析、未列入允许集或需要例外的依赖必须使生成失败，待检查该锁定版本的原始许可证后再明确处理，禁止为使构建通过而静默放宽。`Unlicense` 是为已核实的 `ksni 0.3.6` 依赖单独纳入；GPL-3.0-or-later 仍只对本工作区自有 crate 按包名接受。

每份候选安装包都必须携带根目录本说明、生成的 `licenses/THIRD_PARTY_DEPENDENCIES.html`，以及依赖包单独提供且未由 crate 元数据报告覆盖的补充许可文件（当前包括 `licenses/GPUI_KIT_LUCIDE_LICENSE.txt`）。当前 worktree 已在私有工具目录安装 `cargo-about 0.9.2` 与 `cargo-packager 0.11.8`，并以 `generate licenses/third-party-dependencies.hbs --config about.toml --workspace --locked --fail` 实际生成报告、验证必需 HTML 标记后原子写回。

补充 terminal 的直接 `vte 0.15.0`、UI 的直接 `image 0.25.10` 及 `linkify 0.11.0` 后，当前 lock SHA-256 为 `9df1e3d18c99f90b83ae879932e024d3b47f497362714e00c0fb0ca20950c266`，未更新已有依赖版本。对应报告已重生成，包含 `linkify 0.11.0`；报告为 1,231,346 bytes，SHA-256 为 `4c31d1c82f4b5fbcfe1c92bedd0a3199463536ccddaac0c63cb9628d2194eb1c`。证据为 `target/diagnostics/cargo-about-linkify-20261017T021014Z.evidence.txt` 及同 stem 的 `.generate.stdout.log` / `.generate.stderr.log`。依赖图改变后须重新生成；安装包随附路径仍待候选打包验证。本文件不是已生成报告的替代品。
