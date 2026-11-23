<!-- @author kongweiguang -->

<div align="center">
  <img src="resources/icons/128x128.png" width="64" height="64" alt="Kerminal icon" />
  <h1>Kerminal</h1>
  <p><strong>Local terminals, remote hosts, and file transfers in one desktop workspace.</strong></p>
  <p><a href="https://github.com/kongweiguang/Kerminal/releases/latest">Download</a> · <a href="#quick-start">Quick start</a> · <a href="README.md">简体中文</a></p>
</div>

![Kerminal in dark mode with the host list on the left and two local terminals side by side](docs/assets/readme/workbench.png)

Kerminal is a native terminal app built with Rust, GPUI, and GPUI Kit. Work across sessions in tabs and splits, then open session files, containers, or an agent from the tool rail.

<sub>Captured from the native Windows app with demo data.</sub>

## Download and install

Download the package for your platform from [GitHub Releases](https://github.com/kongweiguang/Kerminal/releases/latest).

| Platform | Package |
| --- | --- |
| Windows x64 | `.exe` installer for the current user |
| Linux x64 | `.AppImage` or `.deb` |
| macOS | Apple Silicon or Intel `.dmg` |

The Windows installer is unsigned and the macOS packages are unnotarized. If the OS shows a warning, you can verify the download against `SHA256SUMS.json` on the release page. Linux needs system graphics libraries, and the `.deb` requires glibc 2.35 or later. Installation and desktop interaction on macOS still await testing on a Mac.

**Upgrade from 0.3.x.** Quit the old app completely, including its tray process, and back up the data directory before installing. On first launch, the 0.4 stable release checks and migrates supported formats in `~/.kerminal`. Keep the backup in `.kerminal-migration/` because older versions cannot read migrated data. Development builds and betas keep their separate `~/.kerminal-gpui` data.

## Quick start

1. **Open a terminal.** Click `+` in the title bar to open a local shell. To connect to a server, add an SSH host in the left sidebar and double-click it.
2. **Work side by side.** Use the split buttons in the terminal header to split right or down. Right-click a split button to choose another host for the new pane.
3. **Open session tools.** Use the right tool rail to open files, containers, or server information. For example, once connected over SSH, open the file panel to browse the remote directory, or launch the dual-pane file workspace from the host sidebar to copy between your computer and the server.

## What you can do

- **Manage terminals and connections.** Organize local, SSH, standalone SFTP, Telnet, and serial sessions in tabs and nested splits. RDP uses the platform client. SSH supports passwords, private keys, SSH agents, proxies, and multiple jump hosts.
- **Transfer and edit files.** Browse session files in the sidebar or choose a source and destination in the dual-pane workspace. Cancel or retry SFTP transfers and resolve name conflicts. With `lrzsz` installed on the remote host, use `rz` to upload or `sz` to download in an SSH pane. When the transfer prompt appears, press `Enter` to open the system picker or `Esc` to cancel. ZMODEM downloads keep both files if a name already exists. Open local, SFTP, and container text files in the built-in editor.
- **Operate remote services.** Inspect server resources and processes over SSH, manage Docker, Podman, and Compose, read logs, and open container shells. Manage tmux sessions and SSH forwarding. Available actions depend on the connection, the tools installed on the host, and your account permissions.
- **Keep common commands handy.** Search terminal output, look up command history, save snippets with variables, or broadcast input to the panes you select.
- **Configure your workspace.** Choose light, dark, or system appearance and set the app and terminal palettes independently. Customize shortcuts, shell profiles, TOML configuration, the encrypted vault, and Git configuration sync. See the [configuration guide](crates/application/resources/config-guide.md).

<details>
<summary>See light mode and app palettes</summary>

![Kerminal appearance settings in light mode with app palette choices](docs/assets/readme/appearance.png)

</details>

## AI agents, MCP, and extensions

**Use an agent beside your terminal.** Install and configure the CLI first, then launch Codex, Claude Code, PI Agent, or a custom CLI from the right-hand Agent panel. Switching tabs preserves the agent's target. Terminal settings let you route agent commands to a background terminal. Built-in Codex and Claude Code launch with full access by default.

**Connect an external MCP client.** On first use, select **Start** under **Settings → MCP**, then copy the connection settings. Starting the server here enables auto-start. Select **Stop** to turn it off. The default Streamable HTTP endpoint is `http://127.0.0.1:37657/mcp`. Once connected, call `kerminal.help` to get started. Clients can open background terminals, run SSH commands, transfer files, and manage forwarding. The MCP host handles permissions and approvals. Use optional LAN access on trusted networks only. See the [MCP tool definitions](crates/kerminal-mcp/resources/tool_catalog.json).

**Install extensions.** Manage plugins under **Settings → Extensions**. Plugins can add connection types, workspace views, tools, and commands. Installation and enablement changes take effect after a restart. Sidecars run with your user privileges, so enable only plugins you trust. See the [plugin catalog](https://github.com/kongweiguang/Kerminal/releases/tag/plugins-catalog-v1) and [developer examples](sdk/examples/README.md).

## Run from source

Install the pinned Rust toolchain and your platform's development dependencies. See the [Rust toolchain](rust-toolchain.toml) and [build dependencies](.github/workflows/gpui-release.yml).

```bash
git clone https://github.com/kongweiguang/Kerminal.git
cd Kerminal
cargo xtask run
```

Stable releases use `~/.kerminal` by default. Source builds use `~/.kerminal-gpui`. Set `KERMINAL_CONFIG_ROOT` to use a separate directory. Use `cargo xtask smoke` for a native startup check with isolated data.

## License

Kerminal is licensed under **GPL-3.0-or-later**. [License](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md)
