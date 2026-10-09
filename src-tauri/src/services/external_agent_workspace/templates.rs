//! External agent workspace generated text templates.
//!
//! @author kongweiguang

pub(crate) const CONFIG_REFERENCE_BODY: &str = r#"<!-- @author kongweiguang -->

# Kerminal Configuration Guide

Read this file before editing Kerminal configuration. Do not guess field names or relationships from filenames alone.

## Workspace Layout

```text
~/.kerminal/
  AGENTS.md
  CLAUDE.md
  kerminal-config.md
  .codex/config.toml
  .mcp.json
  .pi/mcp.json
  settings.toml
  profiles/*.toml
  hosts/groups.toml
  hosts/*.toml
  snippets/*.toml
  workflows/*.toml
  data/command.sqlite
  data/port-forwards/
  logs/
  cache/
  exports/
  temp/
  secrets/vault.toml
  secrets/vault-key.toml
```

## Agent MCP Discovery

- `.mcp.json` remains the project config used by Claude and compatible custom clients.
- Pi uses its native project config at `.pi/mcp.json`. Pi loads it for a trusted working directory; the file in `~/.kerminal` applies to the global Kerminal workspace, while each isolated session has its own `<session-root>/.pi/mcp.json`.
- Pi's project server name overrides a same-named user-level server. Kerminal updates only `mcpServers.kerminal`, preserves other servers and top-level settings, and writes its session endpoint to the same value recorded in `context/mcp-endpoint.json`.
- Pi's `timeout` is measured in seconds and Kerminal sets it to `60`.

## Cross-platform Paths

- `~/.kerminal` means the current user's Kerminal root on Windows, macOS, and Linux. Do not replace it with a machine-specific path such as `C:/Users/...`, `/Users/...`, or `/home/...` unless the user explicitly asks for an absolute path.
- `~/.ssh/id_ed25519` is valid for local SSH/SFTP private key paths. Kerminal expands `~`, `~/...`, and `~\...` to the current user's home directory before passing the path to OpenSSH, russh, or SFTP.
- Do not use `~otheruser/...`; Kerminal only expands the current user's home notation.
- Prefer portable examples in docs and generated config. Use platform-specific shells only inside profile examples.

## Edit Protocol

1. Identify whether the request changes settings, a terminal profile, a host, a host group, a snippet, or a workflow.
2. Use precise search for an id, name, host, tag, or field. Do not reformat every TOML file.
3. Preserve the file's supported `schema_version`, comments, unknown fields, timestamps, and sort order unless the request needs them changed. Host files require schema v2; other ordinary TOML files currently use schema v1.
4. Keep each file relationship valid: host `group_id` must reference `hosts/groups.toml`; file names must match `id`; workflow step ids must be unique.
5. Do not write passwords, inline private keys, key passphrases, tokens, or other plaintext secrets to ordinary TOML files. Do not read or edit `secrets/` unless the user explicitly asks for credential work.
6. After edits, call MCP tool `kerminal.config.validate` with `scope = "all"` or the narrowest matching scope. If Kerminal MCP is unavailable, manually check the rules below and tell the user validation was manual only.

## Validation

- Preferred validation path: call MCP tool `kerminal.config.validate`.
- Use `scope = "all"` for cross-file edits, or one of `settings`, `profiles`, `hosts`, `snippets`, `workflows` for narrow edits.
- The tool is read-only. It validates the current runtime workspace through Kerminal's own TOML loaders and relationship checks; it does not edit config files and does not require Node.js or a source checkout.
- If the MCP server is unavailable, manually check schema versions, file ids, group references, workflow step ids, sort order, forbidden secret-like keys, and host secret field names before reporting success.

## Runtime MCP Boundaries

Use Kerminal MCP for live app features that require the running application or saved connection context: app guide, tool help, terminal sessions, SSH commands, SFTP files, tmux sessions, containers, port forwarding, server info, command history, diagnostics, runtime snapshot, and authorized credential saving.

Use `kerminal.app_guide` when an external Agent needs the Kerminal product/UI structure map before choosing lower-level tools. It explains the machine sidebar, terminal workspace, right tool panel, Agent Launcher, configuration workspace, and matching MCP tool families; it does not perform UI choreography.

Use `kerminal.config_guide` when an external Agent needs these generated configuration rules through MCP, especially when it was not launched inside the initialized `~/.kerminal` workspace. It returns the same guide content as `kerminal-config.md` and does not perform config CRUD.

Use `kerminal.tool_help` with `toolId`, `family`, or `query` when an external Agent needs exact input schema, example arguments, safety annotations, and deliberately absent-tool guidance for one tool or tool family.

## SFTP Transfers

- Use `sftp.transfer.enqueue` with the canonical `source`, `destination`, `kind`, and `conflictPolicy` fields; `idleTimeoutSeconds` is the only optional transfer timeout override. Each endpoint is either `{"type":"local","path":"..."}` for the computer running Kerminal or `{"type":"remote","hostId":"...","path":"..."}` for a saved SSH/SFTP host.
- Supported routes are local -> remote, remote -> local, same-host remote copy, and cross-host remote copy. The operation always copies and never deletes the source. Each call queues one file or directory.
- `kind` is required and is `file` or `directory`. `conflictPolicy` is required and is `overwrite`, `skip`, or `rename`.
- `idleTimeoutSeconds` is optional for enqueue and must be 30-3600. It defaults from `[sftp].idleTimeoutSeconds` (180 seconds) and limits only consecutive missing byte progress; there is no transfer total-duration limit. Saved-host `sshOptions.terminal.connectTimeoutSeconds` is the separate connection timeout and defaults to 30 seconds. Legacy `timeoutSeconds` is read-compatible only and a normal settings save writes `idleTimeoutSeconds`.

```json
{
  "source": { "type": "remote", "hostId": "server-a", "path": "/data/report" },
  "destination": { "type": "remote", "hostId": "server-b", "path": "/backup/report" },
  "kind": "directory",
  "conflictPolicy": "rename"
}
```

Recommended sequence: confirm remote endpoints with `sftp.list` when needed, call `sftp.transfer.enqueue`, then call `sftp.transfer.list` with the returned `transfer.id` as `transferId`. A missing final file while a task is queued, running, waiting, or recovering does not mean failure. For a terminal task with `retryable=true`, call `sftp.transfer.retry` with that id; `resumable=true` means retained confirmed partial data may be used after validation, while `resumable=false` starts a fresh transfer. It returns one idempotent successor id (accepted, not completed); query that id until completion. Call `sftp.transfer.cancel` only when the user asks to stop it; cancellation is a request, so query until a terminal state and keep `succeeded` if commit already finished. Enqueue, list, retry, and cancel are short calls; they return before the background transfer completes. The 60-second MCP client call guard applies only to these calls, never to the queued transfer lifetime. Kerminal selects `transportMode` automatically and does not expose bridge or staging choices. The MCP host owns confirmation, approval, permissions, hooks, and audit.

Queue wait time does not consume the idle budget. Five seconds without confirmed progress zeroes the UI speed and shows a waiting state. Connecting, transferring, and committing refresh activity; a safe single-file upload/download automatically reconnects once after an idle timeout under the same task id, without bypassing queue order. A second idle timeout reaches `failureKind = "idleTimeout"`. That failure keeps the `.kerminal-part` data and reports the adopted threshold, confirmed bytes, and retryability without credentials or internal temporary paths. Use `sftp.transfer.retry` for manual recovery instead of rebuilding the request; repeated retry requests return the same successor and cannot create competing writers. Do not raise `idleTimeoutSeconds` to an hour just because a file is large. Final files are committed atomically only after the full size matches.

If `failureKind = "commitUnknown"`, the final rename may have completed without an acknowledgement. The task is not retryable: inspect the destination file and its content before any new write.

`sftp.upload`, `sftp.upload_directory`, `sftp.download`, and `sftp.download_directory` are deliberately absent from tools/list. A cached call only returns the migration sequence `enqueue -> list -> cancel/retry` and never starts a synchronous transfer.

Do not use SFTP transfer for local -> local copies; use the local filesystem capability instead. Missing hosts, credentials, host-key trust, SFTP subsystem, or path permissions are recoverable transfer errors.

## Terminal Execution

- Every external Agent session uses global terminal scope across all Kerminal tabs. `targetBinding` identifies a preferred user terminal only; it is not an access restriction, and it does not by itself authorize visible-Tab interaction.
- External MCP calls default to background execution. Do not open, switch, or create a UI Tab unless the user explicitly asks to operate the current or a specified Tab.
- For non-interactive SSH commands, use `ssh.command` or `ssh.command_on_resolved_host` first and consume stdout/stderr in the MCP response. Do not create a PTY just to run a command.
- For a persistent shell, interactive input, or a local shell, use `terminal.create` with `target = "local"` or `target = "ssh"` plus a saved `hostId` when needed. Use the returned `sessionId` explicitly with `terminal.snapshot`, `terminal.write`, and `terminal.close`; `terminal.create` is headless and does not open a Tab. Close every headless session you create with `terminal.close` when the task completes, including after an error when cleanup is possible. An interactive shell does not imply UI-Tab interaction. `shell` applies only to local targets, SSH uses the saved host's login shell, and `agentSessionId` is optional correlation metadata rather than a scope restriction.
- Only when the user explicitly asks to operate the current or a specified UI Tab, call `terminal.list` to discover and confirm the requested visible terminal, then pass its explicit `sessionId` to `terminal.snapshot`/`terminal.write`; when a managed Agent session context exists, refresh it with `kerminal.agent.current_session`/`kerminal.agent.target_context` as needed. A background failure must be reported and must not fall back to a visible Tab; a missing or stale requested target must be reported for reconfirmation, not replaced with another Tab.
- A nested session workspace generated by Kerminal belongs to the built-in right-panel Agent. Follow its session-local `AGENTS.md`/`CLAUDE.md`; those rules retain the existing `targetBinding` binding and scope/permissions, and are intentionally distinct from this external-MCP default.

Container runtime tools include lifecycle/status tools and container file tools. Use `container.files.list` and `container.files.preview` before editing; use `container.files.write_text`, `container.files.upload`, `container.files.download`, `container.files.create_directory`, `container.files.rename`, `container.files.chmod`, and `container.files.delete` only for explicitly requested container file work. `container.files.delete` is destructive and depends on MCP host approval/audit.

Do not look for MCP CRUD tools for `settings.*`, `profile.*`, `remote_host.*`, `snippet.*`, `workflow.*`, or `workspace.*`; those configuration changes are direct file edits plus `kerminal.config.validate`.

## Runtime Auto-refresh

When Kerminal is running, it watches file-backed configuration and refreshes the UI after external Agent, editor, or script edits settle. Valid changes to `settings.toml`, `profiles/*.toml`, `hosts/groups.toml`, `hosts/*.toml`, `snippets/*.toml`, and `workflows/*.toml` are reloaded from the same typed runtime loaders used by validation.

Successful external refreshes show a concise auto-closing notice such as `cfg: +1 host "staging-api"`, `cfg: hosts +2, snippets +1`, or `cfg: settings reloaded`. Notices are generated from public UI state only. They must not expose local absolute paths, raw TOML values, passwords, private keys, tokens, or secret filenames.

If a save leaves TOML temporarily invalid, Kerminal keeps the last-known-good UI, does not clear the host tree, and does not close existing terminals. It may show `cfg: invalid TOML, kept last-known-good`; fix the file and save again. Auto-refresh is user feedback, not validation. Always run `kerminal.config.validate` before reporting success.

## File Relationships

| File | Purpose | Relationship |
| --- | --- | --- |
| `settings.toml` | App appearance, terminal appearance, keybindings, SFTP performance | Standalone; business ranges are validated by Kerminal. |
| `profiles/*.toml` | Local terminal launch profiles | Filename must be `<id>.toml`; optional `sidebar_group_id` pins the profile into an existing host group in the left sidebar. |
| `hosts/groups.toml` | Host groups and ordering | `groups[].id` is referenced by host `group_id`; `__ungrouped__` is runtime-only and must not be written. |
| `hosts/*.toml` | Host metadata for SSH/SFTP/Telnet/Serial/RDP/container targets | Filename must be `<id>.toml`; every host uses schema v2 with an explicit `protocol`; saved secret values are referenced by `secret_ref` / `key_passphrase_ref` and encrypted in the vault. |
| `snippets/*.toml` | Reusable single commands | Filename must be `<id>.toml`; `scope` is `any`, `local`, or `ssh`. |
| `workflows/*.toml` | Multi-step command workflows | Filename must be `<id>.toml`; `[[steps]]` are stored in the same file and sorted by `sort_order`. |
| `data/command.sqlite` | Command history and suggestion data | Do not edit directly. Use MCP `history.search` to read history. |
| `secrets/vault.toml` | Encrypted SSH passwords, inline private keys, key passphrases, and jump-host secrets | Do not edit directly; save credentials through the UI or Kerminal credential tools. |
| `secrets/vault-key.toml` | Local workspace vault key | Must stay local and ignored by Git; do not copy it into chat, docs, logs, or commits. |

## Common Rules

- Host TOML files use top-level `schema_version = 2`; other ordinary TOML files currently use `schema_version = 1`.
- Wrapper fields use snake_case: `schema_version`, `group_id`, `sidebar_group_id`, `auth_type`, `sort_order`, `created_at`, `updated_at`, `ssh_options`, `requires_confirmation`.
- `settings.toml` business fields follow the app settings model and often use camelCase, for example `themeMode` and `interfaceDensity`. Preserve existing settings field spelling.
- IDs should be stable ASCII identifiers using letters, numbers, `.`, `_`, or `-`.
- Sort fields are numeric and drive UI order. Prefer increments such as `10`, `20`, `30`.
- Timestamps are strings. Preserve existing values unless creating or intentionally updating an entry.
- Never add plaintext keys such as `password`, `credential_secret`, `inline_private_key`, `apiKey`, `privateKey`, `key_passphrase`, or `token` to ordinary config files. `secret_ref` and `key_passphrase_ref` are encrypted vault references, not plaintext.
- Do not edit `secrets/vault*.toml` directly. Save or replace SSH passwords, inline private keys, key passphrases, and jump-host secrets through the UI save flow, `kerminal.host.upsert_with_credential`, or `kerminal.vault.encrypt_secret`.

## Required Field Matrix

These fields must be present when an Agent creates or rewrites a file. Agent-authored files must be explicit.

| File | Required fields |
| --- | --- |
| `settings.toml` | `schema_version` plus documented settings fields being changed. |
| `profiles/*.toml` | `schema_version`, `id`, `name`, `shell`, `is_default`, `sort_order`, `created_at`, `updated_at`. |
| `hosts/groups.toml` | `schema_version`; each `[[groups]]` entry needs `id`, `name`, `sort_order`, `created_at`, `updated_at`. |
| `hosts/*.toml` | `schema_version = 2`, `protocol`, `id`, `name`, `host`, `port`, `username`, `auth_type`, `tags`, `sort_order`, `created_at`, `updated_at`; saved password or inline-private-key hosts also need `secret_ref`, key passphrases use `key_passphrase_ref`, and jump-host secrets use `[[ssh_options.jump_hosts]].secret_ref`; all references are generated by the save flow. |
| `snippets/*.toml` | `schema_version`, `id`, `title`, `command`, `scope`, `sort_order`, `created_at`, `updated_at`. |
| `workflows/*.toml` | `schema_version`, `id`, `title`, `scope`, `sort_order`, `created_at`, `updated_at`; each `[[steps]]` needs `id`, `title`, `command`, `requires_confirmation`, `sort_order`, `created_at`, `updated_at`. |

Use string timestamps such as `"1"` only for examples. Preserve real timestamp strings in existing files.

## Common Change Recipes

Create a host group:

1. Edit `hosts/groups.toml`.
2. Append one `[[groups]]` entry with `id`, `name`, `sort_order`, `created_at`, and `updated_at`.
3. Do not use `groups` or `__ungrouped__` as an id.
4. Validate with `kerminal.config.validate` using `scope = "hosts"`.

Create a local profile:

1. Create `profiles/<id>.toml`.
2. Fill `id`, `name`, `shell`, `args`, `cwd`, `is_default`, `sort_order`, `created_at`, and `updated_at`.
3. Add `sidebar_group_id` only when the profile should appear in the left host tree; the group must exist in `hosts/groups.toml`.
4. Do not put secrets in `[env]`.

Create a snippet:

1. Create `snippets/<id>.toml`.
2. Fill `title`, `command`, `scope`, `sort_order`, `created_at`, and `updated_at`.
3. Use `scope = "any"` for cross-target commands, `local` for local-only commands, or `ssh` for SSH commands.
4. Do not put passwords, tokens, or private keys in `command`.

Create a workflow:

1. Create `workflows/<id>.toml`.
2. Fill workflow metadata first, then add `[[steps]]` entries in execution order.
3. Every step needs a unique `id`; `sort_order` must increase in file order.
4. Set `requires_confirmation = true` for dangerous local workflow steps. This is UI policy, not an MCP approval queue.

Save a host password or inline private key:

1. Only do this when the user explicitly asks for credential work.
2. Use the Kerminal UI save flow or call `kerminal.host.upsert_with_credential` / `kerminal.vault.encrypt_secret`; do not edit `secrets/vault*.toml` directly.
3. Confirm the matching `hosts/<id>.toml` references the generated `secret_ref` / `key_passphrase_ref` and contains no `credential_secret`, `password`, `inline_private_key`, key passphrase, or private key body.
4. Validate with `kerminal.config.validate` using `scope = "hosts"` or `scope = "all"`.

## settings.toml

Purpose: app appearance, terminal appearance, Agent Launcher definitions, keybindings, SFTP performance, and low-frequency preferences.

Minimal example:

```toml
schema_version = 1
themeMode = "dark"
interfaceDensity = "compact"

[agentLauncher]
selectedAgentKey = "custom:9d045678-983a-4ed1-ab39-bd46bccb1fa3"

[[agentLauncher.customAgents]]
id = "9d045678-983a-4ed1-ab39-bd46bccb1fa3"
name = "PI Agent"
command = "pi"

[toolRail]
order = ["context", "agentLauncher", "sftp", "snippets", "tmux", "ports", "system", "logs"]
hidden = []
bottom = ["logs"]

[toolRail.panelPlacements]
context = "attached"
agentLauncher = "attached"
sftp = "attached"
snippets = "attached"
tmux = "attached"
ports = "attached"
system = "attached"
logs = "attached"

[terminal.keywordHighlights]
enabled = true

[[terminal.keywordHighlights.rules]]
id = "errors"
enabled = true
pattern = "error|exception"
matchMode = "regex"
caseSensitive = false
note = "Errors"
style = "red"
```

Known settings groups:

- `themeMode`: `dark`, `light`, or `system`.
- `interfaceDensity`: `compact`, `comfortable`, or `spacious`.
- `[appearance]`: background image path, background opacity, window opacity, interface language.
- `[terminal]`: font family, font size, line height, scrollback, cursor, color scheme, inline suggestion settings, and global keyword highlights.
- `[terminal.keywordHighlights]`: global switch plus up to 64 ordered `rules`; earlier rules win overlaps. Each rule uses a unique `id`, a non-empty `pattern` up to 256 characters, `matchMode = "literal" | "wholeWord" | "regex"`, `caseSensitive`, an optional `note` up to 160 characters, and `style = "red" | "orange" | "yellow" | "green" | "cyan" | "blue" | "purple" | "pink" | "custom"`. Regex rules reject empty matches, backreferences, lookarounds, and inline flags.
- `[terminal.keywordHighlights.rules.customColors.light]` and `.dark`: required for `style = "custom"`; each theme must define at least one of `foreground` or `background` as `#RRGGBB`.
- `[[keybindings]]`: keyboard shortcuts.
- `[sftp]`: transfer concurrency, timeout, packet size, pipeline depth.
- `[agentLauncher]`: global Agent selector state. `selectedAgentKey` is `builtin:codex`, `builtin:claude`, or `custom:<uuid>` pointing to one current definition. `customAgents` preserves addition order and accepts at most 32 entries; every UUID is unique, names are unique ignoring case and at most 64 characters, and trimmed commands are non-empty and at most 4096 characters. Custom commands are stored as plaintext in `settings.toml`; never put API keys, passwords, tokens, or other secrets in them. Editing or deleting a definition does not rewrite or delete existing Agent session snapshots.
- `[toolRail]`: right tool rail layout. `order` is the complete tool order, `hidden` removes ordinary entries, and `bottom` assigns arbitrary tools to the bottom-fixed rail section. `[toolRail.panelPlacements]` independently maps each tool id to `"attached"` (a resizable right panel), `"left"` (a resizable panel between the host sidebar and terminal), `"bottom"` (a resizable panel below the terminal workspace), or `"center"` (an initially centered, draggable non-modal workspace window). One tool can remain open in each different placement at the same time; opening another tool in the same placement replaces that placement only. Omitted tools default to `"attached"`. The legacy single `panelPlacement` field is accepted only for migration and is rewritten as the per-tool table, so new edits must not add it. At least one tool remains visible after normalization. Supported ids are `context`, `agentLauncher`, `sftp`, `snippets`, `tmux`, `ports`, `system`, and `logs`.

Do not invent settings fields. If a field is not already present and is not documented here, inspect the current Kerminal settings model or ask the user.

## profiles/*.toml

Purpose: local terminal profile.

macOS/Linux example:

```toml
schema_version = 1
id = "default-zsh"
name = "zsh"
shell = "zsh"
args = ["-l"]
cwd = "~/.kerminal"
is_default = true
sort_order = 10
created_at = "1"
updated_at = "1"

[env]
RUST_LOG = "info"
```

Windows example:

```toml
schema_version = 1
id = "default-pwsh"
name = "PowerShell"
shell = "pwsh"
args = ["-NoLogo"]
cwd = "~/.kerminal"
is_default = true
sort_order = 20
created_at = "1"
updated_at = "1"
```

Fields:

- `id`: stable profile id and filename stem.
- `name`: user-visible name.
- `shell`: executable or shell command.
- `args`: default arguments. Common examples are `["-l"]` for `zsh` on macOS/Linux and `["-NoLogo"]` for PowerShell on Windows.
- `cwd`: default working directory; `~/.kerminal` is portable across Windows, macOS, and Linux. Omit or set null when inheriting app cwd.
- `[env]`: environment overrides. Do not put tokens or passwords here.
- `is_default`: default local terminal profile flag.
- `sidebar_group_id`: optional host group id. Set it only when this local profile should appear as a saved left-sidebar connection; the value must exist in `hosts/groups.toml`.
- `sort_order`, `created_at`, `updated_at`: UI ordering and metadata.

## hosts/groups.toml

Purpose: host group names and ordering.

```toml
schema_version = 1

[[groups]]
id = "prod"
name = "Production"
sort_order = 10
created_at = "1"
updated_at = "1"
```

Rules:

- `groups[].id` must be unique.
- Host `group_id` values must either be absent/empty or reference one of these ids.
- Do not write `__ungrouped__`; Kerminal uses it only at runtime.

## hosts/*.toml

Purpose: ordinary host metadata. Credentials and inline private keys are not stored here.

Every saved host uses schema v2 and declares its protocol explicitly. Tags are ordinary user labels and never determine protocol:

```toml
schema_version = 2
protocol = "ssh"
id = "prod-web-01"
group_id = "prod"
name = "prod-web-01"
host = "10.0.0.10"
port = 22
username = "deploy"
auth_type = "key"
credential_ref = "~/.ssh/id_ed25519"
secret_ref = "credential:kerminal:ssh-host:prod-web-01:target:private-key:v1"
key_passphrase_ref = "credential:kerminal:ssh-host:prod-web-01:target:key-passphrase:v1"
tags = ["prod", "web"]
sort_order = 10
created_at = "1"
updated_at = "1"

[ssh_options.proxy]
protocol = "none"

[[ssh_options.jump_hosts]]
index = 0
host = "jump.example.com"
port = 22
username = "deploy"
auth_type = "password"
secret_ref = "credential:kerminal:jump-host:prod-web-01:jump-0:password:v1"
```

An independent SFTP-only host uses the same schema and declares `protocol = "sftp"`:

```toml
schema_version = 2
protocol = "sftp"
id = "prod-files-01"
group_id = "prod"
name = "prod-files-01"
host = "files.example.com"
port = 22
username = "transfer"
auth_type = "agent"
tags = ["prod", "files"]
sort_order = 20
created_at = "1"
updated_at = "1"

[ssh_options.proxy]
protocol = "none"

[ssh_options.transfer]
enabled = true
remote_start_directory = "/incoming"
local_start_directory = ""
preserve_timestamps = true
follow_symlinks = false
max_concurrent_transfers = 2
```

Fields:

- `id`: stable host id and filename stem.
- `protocol`: required and one of `ssh`, `sftp`, `rdp`, `telnet`, or `serial`. It is authoritative; tags do not override it.
- `group_id`: optional group id from `hosts/groups.toml`.
- `name`: user-visible host name.
- `host`: DNS name, IP, serial endpoint, or target host value depending on target type.
- `port`: target port when applicable.
- `username`: login username when applicable.
- `auth_type`: `password`, `key`, or `agent`.
- `credential_ref`: path or reference for key-based auth. For local private key files, `~/.ssh/id_ed25519` is portable and Kerminal expands it to the current user's home directory. This is not the secret body.
- `secret_ref`: encrypted vault reference for a saved password or inline private key. Top-level `secret_ref` belongs to the target host; `[[ssh_options.jump_hosts]].secret_ref` belongs to that jump host.
- `key_passphrase_ref`: encrypted vault reference for a private-key passphrase.
- `tags`: user labels.
- `ssh_options`: proxy, tunnel, jump host, terminal, and transfer settings. Preserve existing nested tables unless the request targets them.
- SFTP-only hosts may use proxy, jump-host, and transfer settings. They must not define terminal or tunnel behavior because terminal, command, tmux, container, system-info, and port-forward capabilities are rejected by the runtime policy.

Rules:

- Use schema v2 for every host and always provide the explicit protocol. The normal loader rejects schema v1. During an app upgrade, Kerminal may consume a valid v1 host once in the startup upgrader, create a recoverable transaction backup, and atomically rewrite it as v2; agents must never create or rewrite v1 files.
- Do not put `credential_secret`, `password`, `inline_private_key`, private key bodies, key passphrases, API keys, or tokens here.
- Passwords, inline keys, key passphrases, and jump-host secrets are encrypted in `secrets/vault.toml`; ordinary host files only keep `secret_ref` / `key_passphrase_ref`.

Host creation checklist:

1. Read `hosts/groups.toml` and choose an existing `group_id`, or omit `group_id` for the runtime ungrouped bucket. Never write `__ungrouped__`.
2. Pick a stable ASCII `id` and create `hosts/<id>.toml`; the filename stem and `id` must match exactly.
3. Write `schema_version = 2` and the explicit `protocol` for every host. Fill every required host field.
4. Use `auth_type = "agent"` when relying on ssh-agent; use `auth_type = "key"` with `credential_ref = "~/.ssh/id_ed25519"` for a local key path; use `auth_type = "password"` only for metadata and do not write the password in this file.
5. For saved passwords, inline private keys, key passphrases, or jump-host secrets, use the UI save flow or authorized credential tools and verify that only `secret_ref` / `key_passphrase_ref` landed in host TOML.
6. Preserve or add `[ssh_options.*]` only when requested. SFTP-only hosts may use proxy, jump, and transfer settings but not terminal or tunnel settings.
7. Call `kerminal.config.validate` with `scope = "hosts"` or `scope = "all"` and fix every diagnostic before reporting success.

Common host failures:

- File exists but app does not show it: check `id` versus filename, TOML parse errors, missing required fields, and validator diagnostics.
- Host is rejected: confirm `schema_version = 2` and a supported explicit `protocol`. Missing/unknown protocols and unsafe or malformed legacy files intentionally fail closed. A valid schema v1 host is accepted only by the one-time startup upgrader; migration failure preserves the original batch and requires fixing the reported file before restart.
- Host appears in wrong group: check `group_id` references an id in `hosts/groups.toml`; `__ungrouped__` must not be written.
- Login still asks for credentials: ordinary host TOML intentionally does not store password, inline key, or key passphrase plaintext. Credential work requires explicit user instruction and the UI/vault save flow so valid `secret_ref` / `key_passphrase_ref` references exist.
- Validator passes manually but app behaves differently: rerun MCP `kerminal.config.validate` because it uses Kerminal runtime loaders.

## secrets/vault.toml and secrets/vault-key.toml

Purpose: encrypted credentials for saved SSH hosts, jump hosts, inline private keys, and key passphrases. Do not read or edit these files unless the user explicitly asks for credential work.

Rules:

- `secrets/vault.toml` stores encrypted entries referenced by `secret_ref` / `key_passphrase_ref` in host TOML.
- `secrets/vault-key.toml` and `secrets/vault-key.toml.bak.*` contain local workspace keys and must stay out of Git.
- `.storage-transactions/`, `backups/`, `.storage.lock`, and `storage-manifest.toml` are local recovery state; they must not be synchronized because transaction copies can contain key material.
- Do not hand-write ciphertext, keys, passwords, or private key bodies; use the UI or credential tools, and never copy secret values into chat, docs, logs, tests, or ordinary config files.
- Ensure `.gitignore` keeps all key and recovery paths above local before syncing a Kerminal workspace; Workspace Sync also removes them from its staged set.

## snippets/*.toml

Purpose: reusable command snippets.

```toml
schema_version = 1
id = "restart-service"
title = "Restart service"
description = "Restart app service"
command = "systemctl restart app"
tags = ["systemd"]
scope = "ssh"
sort_order = 10
created_at = "1"
updated_at = "1"
```

Fields:

- `scope`: `any`, `local`, or `ssh`.
- `command`: command text. Do not embed passwords, tokens, or private keys.

## workflows/*.toml

Purpose: multi-step command workflows. Workflow metadata and steps are stored in one file.

```toml
schema_version = 1
id = "deploy-check"
title = "Deploy check"
description = "Check service before deploy"
tags = ["deploy"]
scope = "ssh"
sort_order = 10
created_at = "1"
updated_at = "1"

[[steps]]
id = "check-disk"
title = "Check disk"
description = "Show disk usage"
command = "df -h"
scope = "ssh"
requires_confirmation = false
sort_order = 10
created_at = "1"
updated_at = "1"
```

Rules:

- Workflow `scope` and step `scope` are `any`, `local`, or `ssh`.
- Step `scope` may be omitted/null to inherit workflow intent.
- Step ids must be unique inside the workflow file.
- Step `sort_order` should increase in file order.
- Use `requires_confirmation = true` for dangerous workflow steps. This is local workflow UI policy, not MCP pending/approval.
"#;
