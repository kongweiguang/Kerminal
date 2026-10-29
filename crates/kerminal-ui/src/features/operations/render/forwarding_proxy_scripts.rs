// @author kongweiguang

/// 为已运行的远端 SOCKS 隧道生成与 main 一致的用户级配置脚本；调用方只负责展示或复制。
pub(super) fn user_proxy_setup_script(session_id: &str, proxy_url: &str) -> String {
    let session_id = shell_safe_slug(session_id);
    let quoted_proxy = shell_quote(proxy_url);
    let quoted_no_proxy = shell_quote("localhost,127.0.0.1");
    let environment = [
        format!("export ALL_PROXY={quoted_proxy}"),
        format!("export all_proxy={quoted_proxy}"),
        format!("export NO_PROXY={quoted_no_proxy}"),
        format!("export no_proxy={quoted_no_proxy}"),
    ]
    .join("\n");

    [
        r##"#!/bin/sh"##,
        r##"set -eu"##,
        r##""##,
        r##"# Kerminal remote SOCKS user-level setup."##,
        r##"# Writes only under the current remote user's $HOME. No root required."##,
        r##"# Review this script before running it on the remote host."##,
        &format!("KERM_PROXY_URL={quoted_proxy}"),
        &format!("KERM_NO_PROXY={quoted_no_proxy}"),
        r##"KERM_HOME="${HOME:?HOME is required}""##,
        &format!("KERM_ID={}", shell_quote(&session_id)),
        r##"KERM_DIR="$KERM_HOME/.kerminal/network-assist""##,
        r##"KERM_ENV="$KERM_DIR/$KERM_ID.env""##,
        r##"KERM_STATE="$KERM_DIR/$KERM_ID.state""##,
        r##"KERM_PROFILE="$KERM_HOME/.profile""##,
        r##"KERM_BEGIN="# >>> kerminal network assist $KERM_ID >>>""##,
        r##"KERM_END="# <<< kerminal network assist $KERM_ID <<<""##,
        r##""##,
        r##"mkdir -p "$KERM_DIR""##,
        r##"umask 077"##,
        r##""##,
        r##"save_git_config() {"##,
        r##"  key="$1""##,
        r##"  if value="$(git config --global --get "$key" 2>/dev/null)"; then"##,
        r##"    printf "git %s %s\\n" "$key" "$value" >> "$KERM_STATE""##,
        r##"  else"##,
        r##"    printf "git %s __KERM_UNSET__\\n" "$key" >> "$KERM_STATE""##,
        r##"  fi"##,
        r##"}"##,
        r##""##,
        r##"save_npm_config() {"##,
        r##"  key="$1""##,
        r##"  value="$(npm config get "$key" --location=user 2>/dev/null || true)""##,
        r##"  if [ -n "$value" ] && [ "$value" != "null" ] && [ "$value" != "undefined" ]; then"##,
        r##"    printf "npm %s %s\\n" "$key" "$value" >> "$KERM_STATE""##,
        r##"  else"##,
        r##"    printf "npm %s __KERM_UNSET__\\n" "$key" >> "$KERM_STATE""##,
        r##"  fi"##,
        r##"}"##,
        r##""##,
        r##"save_pip_config() {"##,
        r##"  pip_cmd="$1""##,
        r##"  if value="$($pip_cmd config --user get global.proxy 2>/dev/null)"; then"##,
        r##"    printf "pip global.proxy %s\\n" "$value" >> "$KERM_STATE""##,
        r##"  else"##,
        r##"    printf "pip global.proxy __KERM_UNSET__\\n" >> "$KERM_STATE""##,
        r##"  fi"##,
        r##"}"##,
        r##""##,
        r##"if [ ! -f "$KERM_STATE" ]; then"##,
        r##"  : > "$KERM_STATE""##,
        r##"  if command -v git >/dev/null 2>&1; then"##,
        r##"    save_git_config http.proxy"##,
        r##"    save_git_config https.proxy"##,
        r##"  fi"##,
        r##"  if command -v npm >/dev/null 2>&1; then"##,
        r##"    save_npm_config proxy"##,
        r##"    save_npm_config https-proxy"##,
        r##"  fi"##,
        r##"  if command -v python3 >/dev/null 2>&1 && python3 -m pip --version >/dev/null 2>&1; then"##,
        r##"    save_pip_config "python3 -m pip""##,
        r##"  elif command -v pip3 >/dev/null 2>&1; then"##,
        r##"    save_pip_config pip3"##,
        r##"  elif command -v pip >/dev/null 2>&1; then"##,
        r##"    save_pip_config pip"##,
        r##"  fi"##,
        r##"fi"##,
        r##""##,
        r##"cat > "$KERM_ENV" <<'KERM_ENV_EOF'"##,
        &environment,
        r##"KERM_ENV_EOF"##,
        r##""##,
        r##"touch "$KERM_PROFILE""##,
        r##"cp "$KERM_PROFILE" "$KERM_PROFILE.kerminal-backup.$(date +%Y%m%d%H%M%S)""##,
        r##"if ! grep -F "$KERM_BEGIN" "$KERM_PROFILE" >/dev/null 2>&1; then"##,
        r##"  {"##,
        r##"    printf "\\n%s\\n" "$KERM_BEGIN""##,
        r##"    printf "[ -f %s ] && . %s\\n" "$KERM_ENV" "$KERM_ENV""##,
        r##"    printf "%s\\n" "$KERM_END""##,
        r##"  } >> "$KERM_PROFILE""##,
        r##"fi"##,
        r##""##,
        r##"if command -v git >/dev/null 2>&1; then"##,
        r##"  git config --global http.proxy "$KERM_PROXY_URL""##,
        r##"  git config --global https.proxy "$KERM_PROXY_URL""##,
        r##"fi"##,
        r##"if command -v npm >/dev/null 2>&1; then"##,
        r##"  npm config set proxy "$KERM_PROXY_URL" --location=user"##,
        r##"  npm config set https-proxy "$KERM_PROXY_URL" --location=user"##,
        r##"fi"##,
        r##"if command -v python3 >/dev/null 2>&1 && python3 -m pip --version >/dev/null 2>&1; then"##,
        r##"  python3 -m pip config --user set global.proxy "$KERM_PROXY_URL""##,
        r##"elif command -v pip3 >/dev/null 2>&1; then"##,
        r##"  pip3 config --user set global.proxy "$KERM_PROXY_URL""##,
        r##"elif command -v pip >/dev/null 2>&1; then"##,
        r##"  pip config --user set global.proxy "$KERM_PROXY_URL""##,
        r##"fi"##,
        r##""##,
        r##"printf "Kerminal user proxy config installed. Restart the shell or run: . %s\\n" "$KERM_ENV""##,
    ]
    .join("\n")
}

/// 为远端 SOCKS ID 生成可逆的用户级撤销脚本；脚本只有在用户粘贴执行后才会改动远端配置。
pub(super) fn user_proxy_undo_script(session_id: &str) -> String {
    let session_id = shell_safe_slug(session_id);
    [
        r##"#!/bin/sh"##,
        r##"set -eu"##,
        r##""##,
        r##"# Kerminal remote SOCKS user-level undo."##,
        r##"# Writes only under the current remote user's $HOME. No root required."##,
        &format!("KERM_ID={}", shell_quote(&session_id)),
        r##"KERM_HOME="${HOME:?HOME is required}""##,
        r##"KERM_DIR="$KERM_HOME/.kerminal/network-assist""##,
        r##"KERM_ENV="$KERM_DIR/$KERM_ID.env""##,
        r##"KERM_STATE="$KERM_DIR/$KERM_ID.state""##,
        r##"KERM_PROFILE="$KERM_HOME/.profile""##,
        r##"KERM_BEGIN="# >>> kerminal network assist $KERM_ID >>>""##,
        r##"KERM_END="# <<< kerminal network assist $KERM_ID <<<""##,
        r##""##,
        r##"state_value() {"##,
        r##"  tool="$1""##,
        r##"  key="$2""##,
        r##"  grep -F "$tool $key " "$KERM_STATE" 2>/dev/null | tail -n 1 | sed "s/^$tool $key //""##,
        r##"}"##,
        r##""##,
        r##"restore_git_config() {"##,
        r##"  key="$1""##,
        r##"  command -v git >/dev/null 2>&1 || return 0"##,
        r##"  value="$(state_value git "$key")""##,
        r##"  if [ "$value" = "__KERM_UNSET__" ] || [ -z "$value" ]; then"##,
        r##"    git config --global --unset "$key" >/dev/null 2>&1 || true"##,
        r##"  else"##,
        r##"    git config --global "$key" "$value""##,
        r##"  fi"##,
        r##"}"##,
        r##""##,
        r##"restore_npm_config() {"##,
        r##"  key="$1""##,
        r##"  command -v npm >/dev/null 2>&1 || return 0"##,
        r##"  value="$(state_value npm "$key")""##,
        r##"  if [ "$value" = "__KERM_UNSET__" ] || [ -z "$value" ]; then"##,
        r##"    npm config delete "$key" --location=user >/dev/null 2>&1 || true"##,
        r##"  else"##,
        r##"    npm config set "$key" "$value" --location=user"##,
        r##"  fi"##,
        r##"}"##,
        r##""##,
        r##"restore_pip_config() {"##,
        r##"  value="$(state_value pip global.proxy)""##,
        r##"  if command -v python3 >/dev/null 2>&1 && python3 -m pip --version >/dev/null 2>&1; then"##,
        r##"    pip_cmd="python3 -m pip""##,
        r##"  elif command -v pip3 >/dev/null 2>&1; then"##,
        r##"    pip_cmd="pip3""##,
        r##"  elif command -v pip >/dev/null 2>&1; then"##,
        r##"    pip_cmd="pip""##,
        r##"  else"##,
        r##"    return 0"##,
        r##"  fi"##,
        r##"  if [ "$value" = "__KERM_UNSET__" ] || [ -z "$value" ]; then"##,
        r##"    $pip_cmd config --user unset global.proxy >/dev/null 2>&1 || true"##,
        r##"  else"##,
        r##"    $pip_cmd config --user set global.proxy "$value""##,
        r##"  fi"##,
        r##"}"##,
        r##""##,
        r##"if [ -f "$KERM_PROFILE" ]; then"##,
        r##"  cp "$KERM_PROFILE" "$KERM_PROFILE.kerminal-undo-backup.$(date +%Y%m%d%H%M%S)""##,
        r##"  tmp="$KERM_PROFILE.kerminal-tmp.$$""##,
        r##"  awk -v begin="$KERM_BEGIN" -v end="$KERM_END" '$0 == begin {skip=1; next} $0 == end {skip=0; next} !skip {print}' "$KERM_PROFILE" > "$tmp""##,
        r##"  mv "$tmp" "$KERM_PROFILE""##,
        r##"fi"##,
        r##""##,
        r##"if [ -f "$KERM_STATE" ]; then"##,
        r##"  restore_git_config http.proxy"##,
        r##"  restore_git_config https.proxy"##,
        r##"  restore_npm_config proxy"##,
        r##"  restore_npm_config https-proxy"##,
        r##"  restore_pip_config"##,
        r##"else"##,
        r##"  command -v git >/dev/null 2>&1 && git config --global --unset http.proxy >/dev/null 2>&1 || true"##,
        r##"  command -v git >/dev/null 2>&1 && git config --global --unset https.proxy >/dev/null 2>&1 || true"##,
        r##"  command -v npm >/dev/null 2>&1 && npm config delete proxy --location=user >/dev/null 2>&1 || true"##,
        r##"  command -v npm >/dev/null 2>&1 && npm config delete https-proxy --location=user >/dev/null 2>&1 || true"##,
        r##"fi"##,
        r##""##,
        r##"rm -f "$KERM_ENV" "$KERM_STATE""##,
        r##"rmdir "$KERM_DIR" 2>/dev/null || true"##,
        r##"printf "Kerminal user proxy config removed. Restart the shell to clear sourced env.\\n""##,
    ]
    .join("\n")
}

/// Shell 单引号转义保持 URL 和稳定 ID 作为单一文本参数传递。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 过滤远端文件名片段，避免将会话 ID 插入 profile 路径或 heredoc 标记。
fn shell_safe_slug(value: &str) -> String {
    let slug = value
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/operations_forwarding_proxy_scripts.rs"]
mod tests;
