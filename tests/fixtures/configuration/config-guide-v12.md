<!-- @author kongweiguang -->

# Kerminal 文件化配置规则

长期设置和用户维护清单位于 Kerminal 配置根目录，默认是 `~/.kerminal`；开发运行可用 `KERMINAL_CONFIG_ROOT` 指定隔离目录。修改前确认目标文件、schema 和关联项，不按文件名猜字段。

## 文件与关联

| 路径 | 用途及关联 |
| --- | --- |
| `settings.toml` | 界面、终端行为与工具偏好；未知字段和注释应保留。 |
| `profiles/<id>.toml` | 本地 shell 启动参数、工作目录、环境变量和默认项；`id` 必须等于文件名 stem，`sidebar_group_id`（若有）引用主机分组。 |
| `hosts/groups.toml` | 分组名和排序；分组 `id` 被 host 的 `group_id` 和 profile 的 `sidebar_group_id` 引用。 |
| `hosts/<id>.toml` | SSH、SFTP、Telnet、串口或 RDP 连接资料；当前 schema 为 3，必须声明 `protocol`。 |
| `snippets/<id>.toml` | 可复用的单条命令、变量和使用范围。 |
| `workflows/<id>.toml` | 有序的多步命令；步骤 ID 在文件内唯一，`sort_order` 必须严格递增。 |
| `workspace/session.json` | 工作区 Tab、Pane 和布局恢复数据；应用管理，直接编辑前先备份。 |
| `data/command.sqlite` | 命令历史与索引，由应用管理；不要直接替换或编辑。 |
| `data/port-forwards/definitions.json` | 应用管理的转发定义，schema 为 1；不保存 PID、监听器或自动启动状态，不通过 MCP CRUD 编辑。 |
| `data/transfer-recovery.json` | 应用管理的传输恢复事实，`schemaVersion` 为 1；不是待重放任务清单，不手改状态或清理所有权。 |
| `secrets/vault.toml` | 加密凭据正文；由凭据流程维护，可参与配置同步。 |
| `secrets/vault-key.toml` | 本机解密密钥，不进 Git 或同步，也不复制到聊天和日志。 |

普通 TOML 文件使用 `schema_version = 1`；`hosts/*.toml` 使用 `schema_version = 3` 并显式写 `protocol`。当前 schema 不匹配或字段损坏时，应用保留原件并进入只读恢复；validator 不会自动迁移或修复文件。0.4 以前的 host 格式只通过一次性迁移转换，普通读取不会猜测旧字段或补默认值。

## Settings

`themeMode` 和 `interfaceDensity` 是顶层外观选项；界面配色、语言、背景开关、背景适配方式、背景图片路径与透明度统一放在 `[appearance]` 下。`[terminal]` 保存字体、光标、色盘与滚屏缓冲。设置中暂未识别的扩展键由原文仓储保留。

`[appearance].colorScheme` 是应用界面配色，可为 `kerminal`、`jetbrains`、`fleet`、`xcode`、`claude`，缺省为 `kerminal`；未知值属于无效配置。每套配色都提供浅色和深色，`themeMode` 为 `dark`、`light` 或 `system`，`system` 按系统明暗外观选择当前应用配色的对应版本。`[terminal].colorScheme` 和终端的浅深色盘是独立设置，修改应用配色不会修改终端颜色设置。旧文件缺少应用配色字段时直接使用默认值，读取不会改写文件。

`[workbenchLayout]` 保存稳定的工作台用户意图：`hostSidebarCollapsed` 默认 `false`，`hostSidebarWidth` 默认 `220` 且范围为 `168`–`360`；`dockPanelWidths` 默认空表，右侧 Attached 工具共用 `360` 逻辑像素内容宽度，显式宽度范围为 `130`–`1360`；`bottomPanelHeight` 默认 `240` 且范围为 `180`–`4000`。

以上宽高均为逻辑像素内容尺寸，不包含 `4px` 调整槽。窗口可用宽度不足时，运行时按中心区域最小宽度等布局预算临时夹紧；窄窗抽屉夹紧到实际剩余视口，这些夹紧值不写回设置。省略整个表或任一字段都使用当前默认值，不改变现有 `schema_version = 1`。

`toolRail.order` 保存八个当前工具的全局顺序，默认依次为 `agentLauncher`、`sftp`、`snippets`、`tmux`、`ports`、`system`、`logs`、`containers`；`containers` 默认位于该数组末尾。`toolRail.hidden` 保存隐藏项，默认空数组，用户设置中至少要保留一个可见工具。`toolRail.bottom` 保存固定在底部区域的工具，默认只有 `logs`；它必须是 `order` 中现有工具的子集，未列入的工具显示在主要区域。因此默认 rail 主区依次显示 `agentLauncher`、`sftp`、`snippets`、`tmux`、`ports`、`system`、`containers`，`logs` 单独固定在 rail 底部。`toolRail.panelPlacements` 按工具分别配置内容打开位置，八项默认均为 `attached`：`attached` 贴靠右栏，`left` 打开在左侧栏，`bottom` 打开在底部面板，`center` 打开为居中自由浮窗。工具顺序、可见性、底部区域和内容位置彼此独立。

当前八个产品 ToolId 为 `agentLauncher`、`sftp`、`snippets`、`tmux`、`ports`、`system`、`logs` 和 `containers`；新配置不包含已退役的 `context` 入口。旧设置中的 `context` 值可继续读取并在保存时保留，但不会作为自定义工具栏项展示。旧 `order` 缺少当前工具时，读取归一化会按默认顺序补齐缺项并保留用户已有顺序；`panelPlacements` 缺项默认按 `attached` 处理。未知 ID 是严格解码/校验错误；不得静默丢弃，原文件保留并进入既有只读恢复。配置仍通过文件或应用设置界面维护，不新增 MCP CRUD。

`dockPanelWidths` 使用相同 ToolId 映射保存各工具被拖动后的 Attached 宽度覆盖，默认空表；兼容键 `context` 仍可读取，但不参与共享宽度选择。Attached 工具共用 `360` 逻辑像素内容宽度，旧配置有多个不同宽度时优先使用当前 Attached 工具对应的旧值；该项缺失时取非 `context` 项中的最大值。用户拖动 Attached Dock 后会把同一内容宽度写入各右侧工具键，拖回默认值则清除覆盖。Center 居中宿主使用自己的布局，不套用 Attached 共享宽度；紧凑工具抽屉复用共享宽度作为首选值并受剩余视口约束。读取只建立共享运行时宽度，不自动改写旧配置。

`keybindings` 是快捷键行数组，每个 `action` 只能出现一次。`action` 使用应用已提供的动作标识；未知动作会保留在设置中但不会执行，不要用配置新增动作或 MCP 工具。`label` 和 `description` 是界面说明；`scope` 为 `global`、`terminal` 或 `workspace`；`editable` 控制设置页是否允许编辑该行。

`binding` 是通用回退键位；Windows 的 `windowsBinding` 和 macOS 的 `macBinding` 为空时回退到 `binding`。`linuxBinding` 缺省时沿用旧 Linux 行为：先使用 `windowsBinding`，该字段为空时再回退到 `binding`；写成空字符串则显式禁用 Linux 绑定。`windowsAlternateBinding` 是 Windows 专用附加键位，缺省或空字符串表示不启用备用键位。

内置 `terminal.paste` 默认只作用于终端：Windows 主键为 `Ctrl+V`、备用键为 `Ctrl+Shift+V`，Linux 为 `Ctrl+Shift+V`，macOS 为 `Cmd+V`。它的通用 `binding` 刻意留空；这样用户清空一个平台的覆盖值时，不会意外继承其他平台的键位。一次性旧格式迁移只会为已有的非空快捷键列表补入缺失的退出和粘贴动作；已有行及其空绑定保持不变，明确的 `keybindings = []` 也保持为空。补入粘贴动作时，若已有行占用了某个平台的默认粘贴键，只清空新粘贴行对应平台的冲突绑定。正常读取和保存不会恢复缺失动作。

<!-- example:settings -->
```toml
schema_version = 1
themeMode = "system"
interfaceDensity = "compact"
keybindings = [{ action = "terminal.paste", label = "粘贴到终端", description = "", binding = "", windowsBinding = "Ctrl+V", windowsAlternateBinding = "Ctrl+Shift+V", linuxBinding = "Ctrl+Shift+V", macBinding = "Cmd+V", scope = "terminal", editable = true }]

[appearance]
colorScheme = "kerminal"
interfaceLanguage = "system"
backgroundEnabled = false
backgroundFit = "cover"
backgroundImagePath = ""
backgroundOpacity = 100
windowOpacity = 100

[terminal]
fontFamily = "Cascadia Code"
fontSize = 15
lineHeight = 1.35
scrollback = 5000
cursorStyle = "block"
cursorBlink = true
colorScheme = "kerminal"
selectionCopy = false

[workbenchLayout]
hostSidebarCollapsed = false
hostSidebarWidth = 220
dockPanelWidths = {}
bottomPanelHeight = 240

[toolRail]
order = ["agentLauncher", "sftp", "snippets", "tmux", "ports", "system", "logs", "containers"]
hidden = []
bottom = ["logs"]

[toolRail.panelPlacements]
agentLauncher = "attached"
sftp = "attached"
snippets = "attached"
tmux = "attached"
ports = "attached"
system = "attached"
logs = "attached"
containers = "attached"
```

## Profile

`id`、`name`、`shell` 是必需项。`shell` 是本机 shell 可执行文件，`args` 为启动参数，`cwd` 为可选工作目录，`env` 为进程环境覆盖，`is_default` 表示本地新会话的默认 profile，`sidebar_group_id` 为可选侧栏分组，`sort_order` 控制列表顺序，`created_at` / `updated_at` 是保留的时间戳字符串。

<!-- example:profile -->
```toml
schema_version = 1
id = "work"
name = "Work shell"
shell = "<installed-shell-executable>"
args = []
cwd = "<optional-working-directory>"
env = { PROJECT_MODE = "work" }
is_default = true
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"
```

把示例中的可执行文件、目录和时间戳替换成真实值。创建、更新、删除 profile 使用配置界面；切换默认项需通过单次 profile 操作，不能分别手改多个文件的 `is_default`。

## Host 与分组

分组项包含 `id`、`name`、`sort_order` 和时间戳；host 的 `group_id` 必须指向现有分组。所有 host 必需 `id`、`name`、`protocol`、`auth_type`、`sort_order` 和时间戳。可选根字段 `note` 最多 4096 个字符，允许多行和制表符但拒绝其它控制字符；应用保存时纯空白备注会省略，有内容时保留用户输入的换行。SSH、SFTP、Telnet、RDP 还必须声明 `host`、`port`、`username`；串口主机改用唯一的 `[serial]` 表保存设备与线路参数，不重复写网络 endpoint。`tags` 只是用户标签，不决定协议，也不能保存串口参数。认证材料只能保存在加密 vault，host 文件只放 `secret_ref` / `key_passphrase_ref` 等引用。

SSH/SFTP 高级项使用当前 snake_case 字段：`[ssh_options.terminal]` 的 `connect_timeout_seconds` 为 1–600 秒（默认 30），`keepalive_seconds` 为 0–600 秒（默认 60，0 关闭）；`term` 是无空白/控制字符、最多 128 字节的终端类型，省略时使用 `xterm-256color`；`working_directory` 为非空且不含控制字符的默认目录。显式启动和恢复目录优先，默认目录不覆盖正在浏览或恢复的位置。

`[ssh_options.sftp]` 的 `enabled` 默认 true；可选 `local_start_directory` 必须是当前平台绝对路径，`remote_start_directory` 使用规范远端路径；`preserve_mtime` 默认 true，`follow_source_symlinks` 默认 false。`concurrency` 省略时继承全局每主机并发，填写时为 1–8 且仍受全局总上限约束。禁用 SFTP 只拒绝之后的文件/传输准入，不断开 SSH 终端或强杀在途任务。链接选项仅解引用源端，并检查范围、环路和断链；目标写入和删除不跟随链接。

RDP 显示项为 `[rdp_options]` 的 `fullscreen`（默认 true）及可选 `width`、`height`。尺寸必须成对出现，宽为 640–8192，高为 480–8192；窗口模式才使用指定尺寸。旧 camelCase 字段只在一次性迁移入口转换，不在普通读取时作为别名。

<!-- example:groups -->
```toml
schema_version = 1

[[groups]]
id = "ops"
name = "Operations"
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"
```

<!-- example:host -->
```toml
schema_version = 3
protocol = "ssh"
id = "build-host"
name = "Build host"
host = "build.example.invalid"
port = 22
username = "deploy"
auth_type = "password"
group_id = "ops"
secret_ref = "credential:kerminal:ssh:build-host:target:password:v1"
note = "由发布团队维护。\n调整前请确认维护窗口。"
tags = ["build"]
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"

[ssh_options.terminal]
connect_timeout_seconds = 30
keepalive_seconds = 60
term = "xterm-256color"
working_directory = "/srv/work"

[ssh_options.sftp]
enabled = true
remote_start_directory = "/srv/work"
preserve_mtime = true
follow_source_symlinks = false
concurrency = 2
```

串口协议不写 `host`、`port`、`username`，设备路径只在 `serial.device` 保存一次；串口设置必须完整填写，`data_bits` 只能为 5–8，`stop_bits` 只能为 1 或 2，`parity` 为 `none`、`even` 或 `odd`，`flow_control` 为 `none`、`xonxoff` 或 `rtscts`。普通 `tags` 不能包含 `serial`、`serial-port`、`serial-baud`、`serial-data-bits`、`serial-stop-bits`、`serial-parity` 或 `serial-flow` 保留键。

<!-- example:serial-host -->
```toml
schema_version = 3
protocol = "serial"
id = "console"
name = "Device console"
auth_type = "agent"
note = "实验室设备控制台"
tags = ["lab"]
sort_order = 20
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"

[serial]
device = "COM7"
baud_rate = 115200
data_bits = 8
stop_bits = 1
parity = "none"
flow_control = "none"
```

Host 示例中的引用遵循 `credential:kerminal:<kind>:<host-id>:<scope>:<material>:v1` 格式：本例 `kind=ssh`、`scope=target`、`material=password`。它只是格式示意，不代表 vault 中存在该条目，也不能直接用于连接。由凭据保存流程创建与当前 host ID 匹配的实际引用。禁止写入 `password`、`credential_secret`、`inline_private_key`、`private_key` 或 `key_passphrase` 正文。

## Snippet

`id` 必须与文件名一致；`title` 是列表名称，`command` 是待插入或执行的命令，`scope` 限定为 `any`、`local` 或 `ssh`。`tags` / `category` 用于组织，`risk` 描述操作风险，`default_action` 选择插入或运行，`sort_order` 控制顺序。`variables` 和 `context_bindings` 可选；每个变量都必须显式提供 `name`、`label`、`kind` 和 `render_strategy`，模板占位符必须声明变量。敏感变量不得保存默认值或建议值，秘密值在运行时输入，不写入模板文件。

<!-- example:snippet -->
```toml
schema_version = 1
id = "inspect-host"
title = "Inspect host"
command = "hostname && uptime"
description = "Read basic host status"
tags = ["inspect"]
scope = "ssh"
category = "diagnostics"
risk = "inspect"
default_action = "insert"
variables = []
context_bindings = []
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"
```

## Workflow

`title`、`scope`、`sort_order` 和时间戳描述整个流程；`[[steps]]` 按 `sort_order` 递增执行。每步有唯一 `id`、`title` 和 `command`；`scope` 可按步骤进一步收窄，`requires_confirmation` 是 UI 风险提示，不代表 MCP 审批。

<!-- example:workflow -->
```toml
schema_version = 1
id = "inspect-release"
title = "Inspect release host"
description = "Run read-only checks in order"
tags = ["inspect"]
scope = "ssh"
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"

[[steps]]
id = "hostname"
title = "Show hostname"
command = "hostname"
scope = "ssh"
requires_confirmation = false
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"

[[steps]]
id = "uptime"
title = "Show uptime"
command = "uptime"
scope = "ssh"
requires_confirmation = false
sort_order = 20
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"
```

## 修改与保密规则

- 只改请求涉及的文件和字段；保留现有 schema、注释、未知字段、时间戳和排序。应用写入按原始文件 revision 做并发检查。
- 文件 ID 使用 ASCII 字母、数字、点、下划线或连字符；不能是 `.` 或 `..`。普通 host ID 不能是保留名 `groups`，路径文件名 stem 必须与文档 `id` 一致。
- `hosts/groups.toml` 的 `groups[].id` 必须唯一。Host `group_id` 与 profile `sidebar_group_id` 若设置，必须引用现存分组；`__ungrouped__` 是运行时标识，不写入文件。
- 不要手工编辑 `secrets/`。`vault-key.toml`、密钥备份、`.storage-transactions/`、vault 事务 journal、SQLite、运行日志、Agent 运行态文件和 `.storage.lock` 属于本机状态；加密 `vault.toml` 可以同步。
- 未经请求不要直接编辑 `workspace/session.json`、`data/command.sqlite` 或 `secrets/`。`.gitignore` 必须排除本机 vault key 和备份，不能因此排除加密 vault 正文。

## 校验

修改完成后调用 `kerminal.config.validate`，选择 `all`、`settings`、`profiles`、`hosts`、`snippets` 或 `workflows`；跨文件修改使用 `all`。省略 scope 也表示 `all`。validator 是只读操作：`valid = false` 表示报告中有诊断，但仍是一次成功的校验响应；按相对路径、行列和键修复后重新运行。它不会修复、格式化、覆盖文件或执行配置 CRUD。若 MCP 未运行，手动核对同一规则并明确说明未运行 validator。

配置文件直接由用户、应用配置界面或获准的工作区文件编辑完成；MCP 不提供 settings/profile/host/snippet/workflow CRUD 工具。MCP 仅承载必须依赖运行态应用、终端、已保存连接或诊断上下文的能力。
