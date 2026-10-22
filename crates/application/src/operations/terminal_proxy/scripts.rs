//! 生成当前交互 shell 可执行且可条件恢复的代理环境脚本。
//!
//! @author kongweiguang

use kerminal_domain::connections::RemoteShellDialect;

use crate::{AppError, AppResult};

use super::ProxyScripts;

const MAX_PROXY_URL_BYTES: usize = 2_048;
const PROXY_VARIABLES: [(&str, &str); 4] = [
    ("ALL_PROXY", "ALL_PROXY_UPPER"),
    ("all_proxy", "ALL_PROXY_LOWER"),
    ("NO_PROXY", "NO_PROXY_UPPER"),
    ("no_proxy", "NO_PROXY_LOWER"),
];

/// 校验生成边界并按已探测的 shell 方言构造可撤销脚本。
pub(super) fn build_scripts(
    proxy_url: &str,
    dialect: RemoteShellDialect,
    token: &str,
) -> AppResult<ProxyScripts> {
    if proxy_url.is_empty()
        || proxy_url.len() > MAX_PROXY_URL_BYTES
        || proxy_url.chars().any(char::is_control)
        || token.is_empty()
        || !token.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(AppError::invalid("SOCKS 代理脚本输入无效"));
    }
    let (apply_script, restore_script) = match dialect {
        RemoteShellDialect::Posix => build_posix(proxy_url, token),
        RemoteShellDialect::Fish => build_fish(proxy_url, token),
        RemoteShellDialect::PowerShell => build_powershell(proxy_url, token),
        RemoteShellDialect::Unknown => {
            return Err(AppError::capability_not_supported(
                "proxy shell dialect is unknown",
            ));
        }
    };
    Ok(ProxyScripts {
        apply_script,
        restore_script,
    })
}

/// POSIX 脚本在当前 shell scope 中保存原值，只在变量仍等于本次值时恢复。
fn build_posix(proxy_url: &str, token: &str) -> (String, String) {
    let prefix = format!("__KERMINAL_PROXY_{token}");
    let quoted_proxy = posix_quote(proxy_url);
    let mut apply = String::new();
    let mut restore = String::new();
    for (variable, label) in PROXY_VARIABLES {
        let name = format!("{prefix}_{label}");
        apply.push_str(&format!(
            "{name}_SET=${{{variable}+x}}; {name}_VALUE=${{{variable}-}}; "
        ));
    }
    apply.push_str(&format!("{prefix}_ACTIVE={}; ", posix_quote(token)));
    for variable in ["ALL_PROXY", "all_proxy"] {
        apply.push_str(&format!("{variable}={quoted_proxy}; export {variable}; "));
    }
    apply.push_str(&format!("{prefix}_APPLIED={}; :", posix_quote(token)));

    restore.push_str(&format!(
        "if [ \"${{{prefix}_ACTIVE-}}\" = {} ]; then ",
        posix_quote(token)
    ));
    restore.push_str(&format!("{prefix}_RESTORE=full; "));
    for (variable, label) in PROXY_VARIABLES {
        if matches!(variable, "ALL_PROXY" | "all_proxy") {
            let name = format!("{prefix}_{label}");
            restore.push_str(&format!(
                "if [ \"${{{variable}-}}\" = {quoted_proxy} ]; then \
                 if [ \"${{{name}_SET-}}\" = x ]; then {variable}=\"${{{name}_VALUE-}}\"; export {variable}; \
                 else unset {variable}; fi; \
                 else {prefix}_RESTORE=partial; fi; "
            ));
        }
    }
    restore.push_str(&format!(
        "{prefix}_RESTORE_RESULT=\"${{{prefix}_RESTORE}}\"; \
         unset {prefix}_ACTIVE {prefix}_APPLIED {prefix}_RESTORE "
    ));
    for (_, label) in PROXY_VARIABLES {
        restore.push_str(&format!(" {prefix}_{label}_SET {prefix}_{label}_VALUE"));
    }
    restore.push_str("; fi");
    (apply, restore)
}

/// Fish 使用原生 set/test 语法，避免将 POSIX parameter expansion 当作 Fish 语法。
fn build_fish(proxy_url: &str, token: &str) -> (String, String) {
    let prefix = format!("__KERMINAL_PROXY_{token}");
    let quoted_proxy = fish_quote(proxy_url);
    let mut apply = String::new();
    let mut restore = String::new();
    for (variable, label) in PROXY_VARIABLES {
        let name = format!("{prefix}_{label}");
        apply.push_str(&format!(
            "if set -q {variable}; set -g {name}_SET 1; set -g {name}_VALUE \"${variable}\"; \
             else; set -g {name}_SET 0; set -g {name}_VALUE ''; end; "
        ));
    }
    apply.push_str(&format!("set -g {prefix}_ACTIVE {}; ", fish_quote(token)));
    for variable in ["ALL_PROXY", "all_proxy"] {
        apply.push_str(&format!("set -gx {variable} {quoted_proxy}; "));
    }
    apply.push_str(&format!(
        "set -g {prefix}_APPLIED {}; true",
        fish_quote(token)
    ));

    restore.push_str(&format!(
        "if test \"${prefix}_ACTIVE\" = {}; ",
        fish_quote(token)
    ));
    restore.push_str(&format!("set -g {prefix}_RESTORE full; "));
    for (variable, label) in PROXY_VARIABLES {
        if matches!(variable, "ALL_PROXY" | "all_proxy") {
            let name = format!("{prefix}_{label}");
            restore.push_str(&format!(
                "if test \"${variable}\" = {quoted_proxy}; \
                 if test \"${name}_SET\" = 1; set -gx {variable} \"${name}_VALUE\"; \
                 else; set -e {variable}; end; \
                 else; set -g {prefix}_RESTORE partial; end; "
            ));
        }
    }
    restore.push_str(&format!(
        "set -g {prefix}_RESTORE_RESULT \"${prefix}_RESTORE\"; \
         set -e {prefix}_ACTIVE {prefix}_APPLIED {prefix}_RESTORE "
    ));
    for (_, label) in PROXY_VARIABLES {
        restore.push_str(&format!(" {prefix}_{label}_SET {prefix}_{label}_VALUE"));
    }
    restore.push_str("; end");
    (apply, restore)
}

/// PowerShell 同时覆盖 Windows 的大小写折叠环境变量和 Unix 的区分大小写环境变量。
fn build_powershell(proxy_url: &str, token: &str) -> (String, String) {
    let prefix = format!("__KERMINAL_PROXY_{token}");
    let quoted_proxy = powershell_quote(proxy_url);
    let mut apply = String::new();
    let mut restore = String::new();
    for (variable, label) in PROXY_VARIABLES {
        let name = format!("{prefix}_{label}");
        apply.push_str(&format!(
            "$global:{name}_SET = Test-Path Env:{variable}; $global:{name}_VALUE = $env:{variable}; "
        ));
    }
    apply.push_str(&format!(
        "$global:{prefix}_ACTIVE = {}; ",
        powershell_quote(token)
    ));
    apply.push_str(&format!(
        "if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {{ \
         $env:ALL_PROXY = {quoted_proxy} \
         }} else {{ $env:ALL_PROXY = {quoted_proxy}; $env:all_proxy = {quoted_proxy} }}; "
    ));
    apply.push_str(&format!(
        "$global:{prefix}_APPLIED = {}; $null = $true",
        powershell_quote(token)
    ));

    restore.push_str(&format!(
        "if ($global:{prefix}_ACTIVE -ceq {}) {{ ",
        powershell_quote(token)
    ));
    restore.push_str(&format!("$global:{prefix}_RESTORE = 'full'; "));
    restore.push_str(&format!(
        "if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {{ \
         if ($env:ALL_PROXY -ceq {quoted_proxy}) {{ \
           if ($global:{prefix}_ALL_PROXY_UPPER_SET) {{ $env:ALL_PROXY = $global:{prefix}_ALL_PROXY_UPPER_VALUE }} \
           else {{ Remove-Item Env:ALL_PROXY -ErrorAction SilentlyContinue }} \
         }} else {{ $global:{prefix}_RESTORE = 'partial' }} \
         }} else {{ "
    ));
    for (variable, label) in PROXY_VARIABLES {
        if matches!(variable, "ALL_PROXY" | "all_proxy") {
            let name = format!("{prefix}_{label}");
            restore.push_str(&format!(
                "if ($env:{variable} -ceq {quoted_proxy}) {{ \
                 if ($global:{name}_SET) {{ $env:{variable} = $global:{name}_VALUE }} \
                 else {{ Remove-Item Env:{variable} -ErrorAction SilentlyContinue }} \
                 }} else {{ $global:{prefix}_RESTORE = 'partial' }}; "
            ));
        }
    }
    restore.push_str(&format!(
        "}}; $global:{prefix}_RESTORE_RESULT = $global:{prefix}_RESTORE; \
         Remove-Variable -Scope Global -Name '{prefix}_ACTIVE','{prefix}_APPLIED','{prefix}_RESTORE' -ErrorAction SilentlyContinue; "
    ));
    for (_, label) in PROXY_VARIABLES {
        restore.push_str(&format!(
            "Remove-Variable -Scope Global -Name '{prefix}_{label}_SET','{prefix}_{label}_VALUE' -ErrorAction SilentlyContinue; "
        ));
    }
    restore.push_str("}; $null = $true");
    (apply, restore)
}

/// POSIX 单引号转义确保代理 URL 内容不会成为额外 shell 语法。
fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Fish 单引号规则保留反斜线和引号作为字面值。
fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// PowerShell 单引号通过成对单引号编码，拒绝 URL 参与表达式解析。
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
