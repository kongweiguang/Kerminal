<!-- @author kongweiguang -->

# Kerminal 文件化配置规则

长期设置和用户维护清单位于 Kerminal 配置根目录，默认是 `~/.kerminal`；开发运行可用 `KERMINAL_CONFIG_ROOT` 指定隔离目录。修改前确认目标文件、schema 和关联项，不按文件名猜字段。

## 文件与关联

| 路径 | 用途及关联 |
| --- | --- |
| `settings.toml` | 界面、终端行为与工具偏好；未知字段和注释应保留。 |
| `profiles/<id>.toml` | 本地 shell 启动参数、工作目录、环境变量和默认项；`id` 必须等于文件名 stem，`sidebar_group_id`（若有）引用主机分组。 |
| `hosts/groups.toml` | 分组名和排序；分组 `id` 被 host 的 `group_id` 和 profile 的 `sidebar_group_id` 引用。 |
| `hosts/<id>.toml` | SSH、SFTP、Telnet、串口或 RDP 连接资料；当前 schema 为 2，必须声明 `protocol`。 |
| `snippets/<id>.toml` | 可复用的单条命令、变量和使用范围。 |
| `workflows/<id>.toml` | 有序的多步命令；步骤 ID 在文件内唯一，`sort_order` 必须严格递增。 |
| `workspace/session.json` | 工作区 Tab、Pane 和布局恢复数据；应用管理，直接编辑前先备份。 |
| `data/command.sqlite` | 命令历史与索引，由应用管理；不要直接替换或编辑。 |
| `secrets/vault.toml` | 加密凭据正文；由凭据流程维护，可参与配置同步。 |
| `secrets/vault-key.toml` | 本机解密密钥，不进 Git 或同步，也不复制到聊天和日志。 |

普通 TOML 文件使用 `schema_version = 1`；`hosts/*.toml` 使用 `schema_version = 2` 并显式写 `protocol`。当前 schema 不匹配或字段损坏时，应用保留原件并进入只读恢复；validator 不会自动迁移或修复文件。

## Settings

`themeMode`、`interfaceLanguage`、`interfaceDensity` 选择外观；`backgroundEnabled`、`backgroundFit`、`backgroundImagePath` 和透明度字段控制窗口背景；`[terminal]` 保存字体、光标、色盘与滚屏缓冲。设置中暂未识别的扩展键由原文仓储保留。

<!-- example:settings -->
```toml
schema_version = 1
themeMode = "system"
interfaceLanguage = "system"
interfaceDensity = "compact"
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

分组项包含 `id`、`name`、`sort_order` 和时间戳；host 的 `group_id` 必须指向现有分组。Host 必需字段为 `id`、`name`、`host`、`port`、`username`、`auth_type`、`sort_order` 和时间戳。`protocol` 明确选择连接协议；`tags` 只是过滤标签，不决定协议。认证材料只能保存在加密 vault，host 文件只放 `secret_ref` / `key_passphrase_ref` 等引用。

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
schema_version = 2
protocol = "ssh"
id = "build-host"
name = "Build host"
host = "build.example.invalid"
port = 22
username = "deploy"
auth_type = "password"
group_id = "ops"
secret_ref = "credential:kerminal:ssh:build-host:target:password:v1"
tags = ["build"]
sort_order = 10
created_at = "<preserve-existing-timestamp>"
updated_at = "<preserve-existing-timestamp>"
```

Host 示例中的引用遵循 `credential:kerminal:<kind>:<host-id>:<scope>:<material>:v1` 格式：本例 `kind=ssh`、`scope=target`、`material=password`。它只是格式示意，不代表 vault 中存在该条目，也不能直接用于连接。由凭据保存流程创建与当前 host ID 匹配的实际引用。禁止写入 `password`、`credential_secret`、`inline_private_key`、`private_key` 或 `key_passphrase` 正文。

## Snippet

`id` 必须与文件名一致；`title` 是列表名称，`command` 是待插入或执行的命令，`scope` 限定为 `any`、`local` 或 `ssh`。`tags` / `category` 用于组织，`risk` 描述操作风险，`default_action` 选择插入或运行，`sort_order` 控制顺序。`variables` 和 `context_bindings` 可选；模板占位符必须声明变量。敏感变量不得保存默认值或建议值，秘密值在运行时输入，不写入模板文件。

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
