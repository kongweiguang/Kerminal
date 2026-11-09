// 由 sdk/protocol/schema 生成；请运行 npm run generate 更新。
// @author kongweiguang

import type { JsonValue } from "../json.js";

export type PluginManifest = { "api": string; "categories"?: Array<string>; "contributes"?: Contributions; "description": { "en": string; } & Record<string, string>; "icon"?: (string | null); "id": string; "l10n"?: (string | null); "license"?: (string | null); "name": { "en": string; } & Record<string, string>; "permissions"?: Array<Permission>; "publisher": string; "repository"?: (`https://${string}` | null); "runtime": PluginRuntime; "schema": 1; "version": string; "views"?: Record<string, ViewDefinition>; [extension: `x-${string}`]: JsonValue; };
export type Activation = "on-demand" | "startup";
export type CommandContribution = { "id": string; "keybinding"?: (string | null); "title": string; [extension: `x-${string}`]: JsonValue; };
export type Contributions = { "commands"?: Array<CommandContribution>; "host_types"?: Array<HostTypeContribution>; "settings"?: (SettingsContribution | null); "tabs"?: Array<TabContribution>; "tools"?: Array<ToolContribution>; [extension: `x-${string}`]: JsonValue; };
export type HostTypeContribution = { "id": string; "schema": string; "title": string; "view"?: string | null; [extension: `x-${string}`]: JsonValue; };
export type Permission = string & ("ui.notify" | "ui.dialog" | "ui.file-picker" | "clipboard.read" | "clipboard.write" | "open-external" | "hosts.read" | "terminal.write" | "events.subscribe" | "secrets" | "full-access" | `network:${string}` | `network:${string}`);
export type PluginRuntime = ({ "kind": "none"; [extension: `x-${string}`]: JsonValue; } | { "activation"?: Activation; "kind": "script"; "main": string; [extension: `x-${string}`]: JsonValue; } | { "activation"?: Activation; "kind": "sidecar"; "main": { "linux-aarch64"?: string; "linux-x86_64"?: string; "macos-aarch64"?: string; "macos-x86_64"?: string; "windows-aarch64"?: string; "windows-x86_64"?: string; }; [extension: `x-${string}`]: JsonValue; });
export type SettingsContribution = { "schema": string; "view"?: string | null; [extension: `x-${string}`]: JsonValue; };
export type TabContribution = { "id": string; "state_version": number; "title": string; "view": string; [extension: `x-${string}`]: JsonValue; };
export type ToolContribution = { "icon"?: (string | null); "id": string; "scope": ToolScope; "title": string; "view": string; [extension: `x-${string}`]: JsonValue; };
export type ToolScope = "window" | "tab";
export type ViewDefinition = ({ "file": string; "kind": "native"; [extension: `x-${string}`]: JsonValue; } | { "csp"?: (string | null); "entry": string; "kind": "webview"; [extension: `x-${string}`]: JsonValue; });
