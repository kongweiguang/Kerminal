<!-- @author kongweiguang -->

# Kerminal GPUI

Kerminal 正在从 Tauri 前端重建为 Rust 原生 GPUI 桌面应用。当前仓库是新架构的开发工作区，版本为 `0.4.0-dev.0`；旧版 `v0.3.45`（基线提交 `17c5d4b`）只用于核对现有配置与用户数据契约。功能迁移和原生验收仍在进行中。

## 工作区结构

- `crates/domain`：业务实体、标识和不变量，不依赖 UI 或基础设施。
- `crates/application`：用例、服务和端口。
- `crates/terminal`：终端状态与解析引擎。
- `crates/infrastructure`：配置、存储、连接和平台适配器。
- `crates/kerminal-ui`：GPUI 界面。
- `crates/kerminal-mcp`：面向 MCP host 的运行态工具。
- `crates/kerminal-desktop`：桌面组合根和启动入口。

测试阶段 GPUI 默认数据根为 `~/.kerminal-gpui`，旧版继续使用 `~/.kerminal`；debug 和 release 的实例身份均按数据根隔离，可同时运行。0.4 的业务代码只读写当前格式；旧数据转换集中在一次性迁移用例，不在正常读取路径保留旧版分支。完整字段、示例和校验要求以 [`config-guide.md`](crates/application/resources/config-guide.md) 为准。

## 一次性数据迁移

迁移以格式标记和实际结构判断是否需要执行，不仅比较应用版本号。普通启动和独立 `kerminal-migrate` 程序调用同一 Rust 用例；完成前不启动正常读写服务。迁移提供预检、备份、校验、幂等及中断恢复，原文件损坏或存在不支持版本时保留数据并报告原因。

安装包将 Windows 包装脚本和 Rust 维护程序放在包资源根目录的 `migration/` 子目录；从该资源根目录运行 Windows 示例（将路径换为待迁移目录）：

```powershell
& .\migration\kerminal-migrate.ps1 -Root 'C:\path\to\config' -DryRun
& .\migration\kerminal-migrate.ps1 -Root 'C:\path\to\config' -Apply
```

Linux/macOS 包内脚本也位于 `migration/kerminal-migrate.sh`，接受 `--root /path/to/config --dry-run` 或 `--apply`，并只调用同目录随包 helper。预检不写入数据；提交前须退出使用同一目录的所有新旧应用。提交成功后格式标记为当前版本，不应再用旧版直接读写；回退使用 `.kerminal-migration/<operation-id>/` 中的迁移前备份。遇到提交结果未知时保留整个操作目录，由同一入口回读恢复，不直接重放写入或删除备份。开发和测试仅使用独立临时根，不能对真实用户目录执行上述示例。

## 开发环境

安装 Rust 1.98.0、仓库 toolchain 中声明的 `rustfmt` 与 `clippy`，并安装 Git。所有本地工作流从仓库根目录运行：

```powershell
cargo xtask fmt
cargo xtask check
cargo xtask clippy
cargo xtask test
cargo xtask ci
cargo xtask source-fingerprint
```

`cargo xtask run` 和 `cargo run -p kerminal-desktop` 默认使用 `~/.kerminal-gpui`；显式设置的 `KERMINAL_CONFIG_ROOT` 优先。`cargo xtask smoke` 仍默认使用独立的 `target/kerminal-native-smoke`，自动化测试保持隔离。构建或测试结果应以实际命令输出为准，本 README 不代表这些门禁已经通过。

并行验收时可将 `KERMINAL_DEV_CONFIG_ROOT` 设为独立的绝对路径，再运行 `cargo xtask run` 或 `smoke`；该变量在这两个入口中优先于 `KERMINAL_CONFIG_ROOT`。需要隔离构建进程时同时设置 `CARGO_TARGET_DIR`。验收目录应放在任务专用的 `target/` 子目录，避免共用配置或占用运行中的二进制。

本地打包前，在仓库根目录将许可证报告工具安装到仓库自己的 `target/` 目录，然后显式选择 Rust target：

```powershell
$target = 'x86_64-pc-windows-msvc'
cargo install cargo-about --version 0.9.2 --locked --root target/tools/cargo-about
cargo install cargo-packager --version 0.11.8 --locked --root target/tools/cargo-packager
$env:PATH = (Resolve-Path 'target/tools/cargo-packager/bin').Path + [IO.Path]::PathSeparator + $env:PATH
cargo +1.98.0 build --locked --release -p kerminal-desktop --target $target
cargo +1.98.0 xtask package --target $target
```

先显式构建桌面 release binary：`cargo xtask package` 验证许可证报告、运行 cargo-packager，再收集实际产物，不会替你构建应用。打包入口会验证仓库私有安装的 `cargo-about 0.9.2`，从锁文件生成第三方依赖许可证报告；工具缺失、版本不符或报告不完整时会停止打包。`cargo-packager 0.11.8` 通过仓库私有工具目录加入当前 PowerShell 的 PATH，避免依赖机器上的全局版本。

平台打包配置中的 `out-dir`、图标、许可证和资源路径都相对 `packaging/` 配置目录解析；cargo-packager 0.11.8 使用显式配置文件时以该文件的父目录作为工作目录。产物先写入 `target/package-output/`，collector 再将实际安装包及必要的 updater archive 整理到 `target/package-candidates/<target>/`。Windows 使用实际 NSIS；Linux 保留 AppImage/DEB，并生成 AppImage 的 tar.gz；macOS 保留实际应用包/DMG，并生成应用包的 tar.gz。三平台配置固定主二进制名为 `kerminal`；xtask 按所选 target 指向对应 release binary。

`cargo xtask source-fingerprint` 输出当前源码树的 SHA-256 JSON，覆盖锁文件和非忽略的新源码。对稳定候选应在构建前后各记录一次，并一并保存实际二进制摘要；单独的源码指纹或 Git revision 不能证明二进制对应关系。

三个平台的打包配置仅声明应用自有 `kerminal://` scheme；应用仍可解析 `ssh://` 和 `sftp://` 输入，但安装包不会注册为系统默认处理程序。cargo-packager 为 Windows 当前用户安装、macOS bundle 和 Linux desktop entry 生成相应元数据。AppImage 只携带 desktop entry；主机是否注册该 scheme 取决于用户使用的 AppImage 集成方式。候选安装包的系统关联安装与卸载尚未在隔离桌面环境实测。

当前支持 `x86_64-pc-windows-msvc`、`x86_64-unknown-linux-gnu`、`x86_64-apple-darwin` 和 `aarch64-apple-darwin`。GitHub Actions 已提供手动触发的 `GPUI unsigned candidate packages` workflow；它按上述 target 构建未签名候选包并上传短期 artifact，不发布 Release 或更新 feed。当前 PR CI 仍只运行质量门禁，不执行打包。

需要手动指定隔离配置目录时，在 PowerShell 中设置环境变量后直接运行桌面包：

```powershell
$env:KERMINAL_CONFIG_ROOT = Join-Path $PWD 'target/kerminal-local-config'
cargo run -p kerminal-desktop
```

GPUI 的开发与发布构建在测试阶段都使用当前用户 home 下的 `.kerminal-gpui`。从旧版导入主机时，只复制 `hosts/*.toml`（含分组）、`profiles/*.toml`、`known_hosts` 和配套的 `secrets/vault.toml`、`secrets/vault-key.toml`，在新目录完成迁移；密钥文件保留原访问权限，不把凭据改写为明文。旧版的会话、历史、Git 仓库及运行态不复制，两个目录后续独立保存，不自动同步。显式选择旧版 `.kerminal` 仍受旧实例迁移检查保护；不要在测试期间将其作为 GPUI 数据根。自动化测试使用临时目录或 `target/` 下的隔离目录。

## 原生窗口性能采集

采集入口需同时启用 Cargo feature 和环境开关。以下命令安排 5 秒预热，再采集 60 秒；输出必须是绝对路径，并且不能覆盖已有文件：

```powershell
$env:KERMINAL_CONFIG_ROOT = Join-Path $PWD 'target/acceptance/native/config'
$env:KERMINAL_ACCEPTANCE_METRICS = '1'
$env:KERMINAL_ACCEPTANCE_METRICS_DURATION_MS = '60000'
$env:KERMINAL_ACCEPTANCE_METRICS_OUTPUT = Join-Path $PWD 'target/acceptance/native/frames.json'
cargo run --locked --release -p kerminal-desktop --features acceptance-metrics
```

采集期间需要实际操作窗口。报告保存测量区间的直方图、样本数和 p95；没有足够绘制样本时明确报告不足。GPUI 的提交和输入处理耗时不代表屏幕实际显示时刻，也不代表 PTY 输入回显延迟。该入口仍待原生构建及实际窗口验证，当前没有可据此宣称的性能结果。

## 当前进度

新工程已建立 Rust workspace，并正在分批迁移终端、连接、文件、配置、MCP 和桌面能力。`cargo xtask run`、`smoke`、测试和打包命令是工作区提供的入口；命令存在不代表当前所有目标平台、功能或产物已经验收。以各模块实现和测试为准，未完成的能力不会在此标记为可用。

当前没有 macOS 真实桌面；按用户确认，本次完成平台实现、源码核对和可用构建检查，原生交互由用户后续验证。macOS 窗口、终端进程、SSH/串口及安装包的未执行项保留为未验证，Windows 构建和隔离测试不代替这些结果。
