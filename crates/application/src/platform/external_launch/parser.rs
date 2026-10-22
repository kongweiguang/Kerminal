//! 外部终端与文件客户端参数的严格白名单解析。
//!
//! 此模块只把可信范围内的参数转成连接意图，不连接网络、不解析私有会话文件，也不接收密码。
//!
//! @author kongweiguang

use std::{net::Ipv6Addr, path::PathBuf};

use percent_encoding::percent_decode_str;
use url::Url;

use crate::{AppError, AppResult, platform::command_line::split_command_line};

use super::{
    ExternalActivation, ExternalJumpHostDraft, ExternalLaunchIntent, ExternalLaunchRequest,
    ExternalLaunchSource, ExternalTargetDraft,
};
use kerminal_domain::connections::RemoteProtocol;

const DEFAULT_SSH_PORT: u16 = 22;

/// 只按显式协议前缀或可验证客户端名预识别来源，供应用在完整解析前执行用户禁用策略。
pub(super) fn source_hint(activation: &ExternalActivation) -> Option<ExternalLaunchSource> {
    let arguments = &activation.arguments;
    if arguments.iter().any(|argument| {
        argument
            .get(.."kerminal:".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("kerminal:"))
    }) {
        Some(ExternalLaunchSource::Kerminal)
    } else {
        identify_source(arguments)
    }
}

/// 识别 Kerminal 协议链接或明确支持的终端客户端，再按其窄白名单解析参数。
pub(super) fn parse_activation(
    activation: &ExternalActivation,
) -> AppResult<Option<ExternalLaunchRequest>> {
    let arguments = &activation.arguments;
    if arguments.is_empty() {
        return Ok(None);
    }

    reject_secret_arguments(arguments)?;
    if let Some((index, raw_url)) = arguments.iter().enumerate().find(|(_, argument)| {
        argument
            .get(.."kerminal:".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("kerminal:"))
    }) {
        return parse_kerminal_url(raw_url, index).map(Some);
    }

    let source = identify_source(arguments);
    let Some(source) = source else {
        return Ok(None);
    };

    match source {
        ExternalLaunchSource::Kerminal => parse_kerminal_cli(arguments).map(Some),
        ExternalLaunchSource::OpenSsh => parse_openssh(arguments, source).map(Some),
        ExternalLaunchSource::Putty => parse_putty(arguments).map(Some),
        ExternalLaunchSource::SecureCrt => parse_securecrt(arguments).map(Some),
        ExternalLaunchSource::MobaXterm => parse_mobaxterm(arguments).map(Some),
        ExternalLaunchSource::Xshell => parse_xshell(arguments).map(Some),
        ExternalLaunchSource::SftpClient => parse_sftp_client(arguments).map(Some),
    }
}

/// 按稳定的来源优先级识别启动参数；未知程序参数不猜测，避免接管其他应用的请求。
///
/// @author kongweiguang
fn identify_source(arguments: &[String]) -> Option<ExternalLaunchSource> {
    if arguments.iter().skip(1).any(|argument| {
        argument.eq_ignore_ascii_case("--external-ssh")
            || argument.eq_ignore_ascii_case("--external-sftp")
    }) {
        return Some(ExternalLaunchSource::Kerminal);
    }
    if arguments.iter().skip(1).any(|argument| {
        argument
            .get(.."sftp://".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sftp://"))
    }) {
        return Some(ExternalLaunchSource::SftpClient);
    }

    let executable = arguments.first().map(|value| executable_name(value));
    match executable.as_deref() {
        Some("ssh" | "ssh.exe" | "sftp" | "sftp.exe") => {
            return Some(ExternalLaunchSource::OpenSsh);
        }
        Some("putty" | "putty.exe" | "plink" | "plink.exe") => {
            return Some(ExternalLaunchSource::Putty);
        }
        Some("mobaxterm" | "mobaxterm.exe") => {
            return Some(ExternalLaunchSource::MobaXterm);
        }
        Some("xshell" | "xshell.exe") => return Some(ExternalLaunchSource::Xshell),
        Some("securecrt" | "securecrt.exe") => {
            return Some(ExternalLaunchSource::SecureCrt);
        }
        Some("kerminal" | "kerminal.exe") => {
            return Some(ExternalLaunchSource::Kerminal);
        }
        Some("winscp" | "winscp.exe" | "filezilla" | "filezilla.exe" | "flashfxp") => {
            return Some(ExternalLaunchSource::SftpClient);
        }
        _ => {}
    }

    if has_named_option(arguments, &["/SSH2"]) {
        Some(ExternalLaunchSource::SecureCrt)
    } else if has_named_option(arguments, &["-newtab", "-exec"]) {
        Some(ExternalLaunchSource::MobaXterm)
    } else if has_named_option(arguments, &["-url", "-newwin"])
        || arguments
            .iter()
            .skip(1)
            .any(|argument| starts_with_scheme(argument, "ssh://"))
    {
        Some(ExternalLaunchSource::Xshell)
    } else if has_named_option(arguments, &["--host", "--hostname", "--remote-host"]) {
        None
    } else if has_named_option(arguments, &["-ssh", "-load", "-P"]) {
        Some(ExternalLaunchSource::Putty)
    } else if has_named_option(
        arguments,
        &["-J", "-p", "-i", "-l", "--jump", "--identity-file"],
    ) {
        Some(ExternalLaunchSource::OpenSsh)
    } else {
        None
    }
}

/// 解析 Kerminal 自有 ssh/sftp URL；未知查询字段和凭据字段都必须失败关闭。
fn parse_kerminal_url(raw_url: &str, _argument_index: usize) -> AppResult<ExternalLaunchRequest> {
    let url = Url::parse(raw_url).map_err(|_| AppError::invalid("Kerminal 外部链接无效"))?;
    if url.scheme() != "kerminal"
        || url.username() != ""
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.fragment().is_some()
    {
        return Err(AppError::invalid("Kerminal 外部链接格式无效"));
    }
    let action = url
        .host_str()
        .ok_or_else(|| AppError::invalid("Kerminal 外部链接缺少动作"))?;
    let pairs = strict_query_pairs(&url)?;

    match action {
        "ssh" => {
            ensure_allowed_keys(
                &pairs,
                &[
                    "host",
                    "port",
                    "user",
                    "username",
                    "displayName",
                    "openSftp",
                    "open_sftp",
                ],
            )?;
            let target = query_target(&pairs, RemoteProtocol::Ssh)?;
            let display_name = query_value(&pairs, "displayName")
                .map(|value| validated_display_name(&value))
                .transpose()?;
            let open_sftp = query_value(&pairs, "openSftp")
                .or_else(|| query_value(&pairs, "open_sftp"))
                .is_some_and(|value| truthy(&value));
            let intent = if open_sftp {
                ExternalLaunchIntent::SftpTransfer {
                    remote_path: "/".to_owned(),
                    expected_host_key: None,
                }
            } else {
                ExternalLaunchIntent::SshTerminal {
                    remote_command: Vec::new(),
                }
            };
            Ok(request(
                ExternalLaunchSource::Kerminal,
                target,
                intent,
                display_name,
                None,
            ))
        }
        "sftp" => {
            ensure_allowed_keys(&pairs, &["host", "port", "user", "username", "path"])?;
            let target = query_target(&pairs, RemoteProtocol::Sftp)?;
            let path = query_value(&pairs, "path").unwrap_or_else(|| "/".to_owned());
            validate_remote_path(&path)?;
            Ok(request(
                ExternalLaunchSource::Kerminal,
                target,
                ExternalLaunchIntent::SftpTransfer {
                    remote_path: path,
                    expected_host_key: None,
                },
                None,
                None,
            ))
        }
        _ => Err(AppError::invalid("不支持的 Kerminal 外部链接动作")),
    }
}

/// 把 Kerminal 命令行输入映射到与深链接相同的安全字段，拒绝未知参数。
fn parse_kerminal_cli(arguments: &[String]) -> AppResult<ExternalLaunchRequest> {
    let mut protocol = RemoteProtocol::Ssh;
    let mut host = None;
    let mut port = DEFAULT_SSH_PORT;
    let mut username = None;
    let mut identity_file = None;
    let mut jumps = Vec::new();
    let mut remote_path = None;
    let mut display_name = None;
    let mut expected_host_key = None;
    let mut index = 1;

    while index < arguments.len() {
        let option = arguments[index].as_str();
        if option == "--external-sftp" {
            protocol = RemoteProtocol::Sftp;
            index += 1;
            continue;
        }
        if option == "--external-ssh" {
            protocol = RemoteProtocol::Ssh;
            index += 1;
            continue;
        }
        let (name, inline) = split_long_option(option);
        let value = match name {
            "--host" | "--hostname" | "--remote-host" => {
                if host.is_some() {
                    return Err(AppError::invalid("外部启动重复指定主机"));
                }
                take_option_value(arguments, &mut index, inline, name)?
            }
            "--port" => {
                port = parse_port(&take_option_value(arguments, &mut index, inline, name)?)?;
                continue;
            }
            "--user" | "--username" => {
                username = Some(validated_username(&take_option_value(
                    arguments, &mut index, inline, name,
                )?)?);
                continue;
            }
            "--identity-file" | "-i" => {
                identity_file = Some(nonempty_path(&take_option_value(
                    arguments, &mut index, inline, name,
                )?)?);
                continue;
            }
            "--jump" | "-J" => {
                jumps.extend(parse_jump_chain(&take_option_value(
                    arguments, &mut index, inline, name,
                )?)?);
                continue;
            }
            "--remote-path" => {
                remote_path = Some(take_option_value(arguments, &mut index, inline, name)?);
                continue;
            }
            "--display-name" => {
                display_name = Some(validated_display_name(&take_option_value(
                    arguments, &mut index, inline, name,
                )?)?);
                continue;
            }
            "--host-key" => {
                expected_host_key = Some(take_option_value(arguments, &mut index, inline, name)?);
                continue;
            }
            _ => return Err(AppError::invalid("Kerminal 外部启动参数不受支持")),
        };
        host = Some(validated_host(&value)?);
        index += 1;
    }

    let host = host.ok_or_else(|| AppError::invalid("外部连接缺少主机"))?;
    let target = ExternalTargetDraft {
        protocol,
        host,
        port,
        username,
        jump_hosts: jumps,
    };
    let intent = if protocol == RemoteProtocol::Sftp || remote_path.is_some() {
        let path = remote_path.unwrap_or_else(|| "/".to_owned());
        validate_remote_path(&path)?;
        ExternalLaunchIntent::SftpTransfer {
            remote_path: path,
            expected_host_key,
        }
    } else {
        if expected_host_key.is_some() {
            return Err(AppError::invalid("SSH 终端参数不能包含 SFTP 主机密钥声明"));
        }
        ExternalLaunchIntent::SshTerminal {
            remote_command: Vec::new(),
        }
    };
    Ok(request(
        ExternalLaunchSource::Kerminal,
        target,
        intent,
        display_name,
        identity_file,
    ))
}

/// 解析 OpenSSH 受支持的连接字段并保留远端命令 argv 边界。
fn parse_openssh(
    arguments: &[String],
    source: ExternalLaunchSource,
) -> AppResult<ExternalLaunchRequest> {
    let mut host = None;
    let mut port = DEFAULT_SSH_PORT;
    let mut username = None;
    let mut identity_file = None;
    let mut jumps = Vec::new();
    let mut port_was_set = false;
    let mut index = 1;

    while index < arguments.len() {
        let token = arguments[index].as_str();
        if token == "--" {
            index += 1;
            host = arguments.get(index).cloned();
            if host.is_some() {
                index += 1;
            }
            break;
        }
        if token.starts_with('-') {
            let (name, attached) = split_short_option(token);
            match name {
                "-p" => {
                    if port_was_set {
                        return Err(AppError::invalid("OpenSSH 参数重复指定端口"));
                    }
                    port = parse_port(&next_option_value(arguments, &mut index, attached, name)?)?;
                    port_was_set = true;
                }
                "-l" => {
                    username = Some(validated_username(&next_option_value(
                        arguments, &mut index, attached, name,
                    )?)?)
                }
                "-i" => {
                    identity_file = Some(nonempty_path(&next_option_value(
                        arguments, &mut index, attached, name,
                    )?)?)
                }
                "-J" => jumps.extend(parse_jump_chain(&next_option_value(
                    arguments, &mut index, attached, name,
                )?)?),
                "-4" | "-6" | "-A" | "-a" | "-C" | "-f" | "-g" | "-K" | "-k" | "-M" | "-N"
                | "-n" | "-q" | "-s" | "-T" | "-t" | "-v" | "-X" | "-x" | "-Y" | "-y"
                    if attached.is_none() =>
                {
                    index += 1
                }
                "-o" | "-F" | "-S" | "-W" | "-w" | "-b" | "-B" | "-c" | "-D" | "-E" | "-e"
                | "-I" | "-L" | "-m" | "-O" | "-Q" | "-R" | "-V" => {
                    return Err(AppError::invalid("OpenSSH 参数包含不支持的配置或转发选项"));
                }
                _ => return Err(AppError::invalid("OpenSSH 参数不受支持")),
            }
            continue;
        }
        host = Some(token.to_owned());
        index += 1;
        break;
    }

    let destination = host.ok_or_else(|| AppError::invalid("OpenSSH 参数缺少目标主机"))?;
    let (destination_user, host, destination_port) = parse_user_host_port(&destination)?;
    if username.is_none() {
        username = destination_user;
    }
    if !port_was_set {
        port = destination_port.unwrap_or(DEFAULT_SSH_PORT);
    } else if destination_port.is_some() {
        return Err(AppError::invalid("OpenSSH 目标端口存在重复定义"));
    }
    let target = ExternalTargetDraft {
        protocol: RemoteProtocol::Ssh,
        host,
        port,
        username,
        jump_hosts: jumps,
    };
    Ok(request(
        source,
        target,
        ExternalLaunchIntent::SshTerminal {
            remote_command: arguments[index..].to_vec(),
        },
        None,
        identity_file,
    ))
}

/// 解析 PuTTY/Plink 的公开连接参数；密码参数和已保存会话不被读取或转存。
fn parse_putty(arguments: &[String]) -> AppResult<ExternalLaunchRequest> {
    let mut host = None;
    let mut port = DEFAULT_SSH_PORT;
    let mut username = None;
    let mut identity_file = None;
    let mut port_was_set = false;
    let mut index = 1;
    while index < arguments.len() {
        let token = arguments[index].as_str();
        if token.eq_ignore_ascii_case("-ssh") {
            index += 1;
            continue;
        }
        if token.eq_ignore_ascii_case("-load") {
            return Err(AppError::invalid("PuTTY 保存会话不会被外部启动自动读取"));
        }
        let (name, attached) = split_short_option(token);
        match name.to_ascii_lowercase().as_str() {
            "-p" => {
                if port_was_set {
                    return Err(AppError::invalid("PuTTY 参数重复指定端口"));
                }
                port = parse_port(&next_option_value(arguments, &mut index, attached, name)?)?;
                port_was_set = true;
            }
            "-l" => {
                username = Some(validated_username(&next_option_value(
                    arguments, &mut index, attached, name,
                )?)?)
            }
            "-i" => {
                identity_file = Some(nonempty_path(&next_option_value(
                    arguments, &mut index, attached, name,
                )?)?)
            }
            "-pw" | "-pwfile" | "-passphrase" => {
                return Err(AppError::invalid("PuTTY 密码参数不允许通过外部启动传递"));
            }
            _ if token.starts_with('-') => return Err(AppError::invalid("PuTTY 参数不受支持")),
            _ => {
                if host.replace(token.to_owned()).is_some() {
                    return Err(AppError::invalid("PuTTY 参数包含多个目标主机"));
                }
                index += 1;
            }
        }
    }
    let destination = host.ok_or_else(|| AppError::invalid("PuTTY 参数缺少目标主机"))?;
    let (destination_user, host, destination_port) = parse_user_host_port(&destination)?;
    if username.is_none() {
        username = destination_user;
    }
    if !port_was_set {
        port = destination_port.unwrap_or(DEFAULT_SSH_PORT);
    } else if destination_port.is_some() {
        return Err(AppError::invalid("PuTTY 目标端口存在重复定义"));
    }
    Ok(request(
        ExternalLaunchSource::Putty,
        ExternalTargetDraft {
            protocol: RemoteProtocol::Ssh,
            host,
            port,
            username,
            jump_hosts: Vec::new(),
        },
        ExternalLaunchIntent::SshTerminal {
            remote_command: Vec::new(),
        },
        None,
        identity_file,
    ))
}

/// 解析 SecureCRT 的 /SSH2 直连参数；不加载 profile 或 Windows Credential Manager 项。
fn parse_securecrt(arguments: &[String]) -> AppResult<ExternalLaunchRequest> {
    let mut host = None;
    let mut port = DEFAULT_SSH_PORT;
    let mut username = None;
    let mut identity_file = None;
    let mut index = 1;
    while index < arguments.len() {
        let token = arguments[index].as_str();
        match token.to_ascii_lowercase().as_str() {
            "/ssh2" => index += 1,
            "/l" => {
                username = Some(validated_username(&plain_next_value(
                    arguments, &mut index, "/L",
                )?)?)
            }
            "/p" => port = parse_port(&plain_next_value(arguments, &mut index, "/P")?)?,
            "/i" => {
                identity_file = Some(nonempty_path(&plain_next_value(
                    arguments, &mut index, "/I",
                )?)?)
            }
            "/password" | "/passphrase" => {
                return Err(AppError::invalid(
                    "SecureCRT 密码参数不允许通过外部启动传递",
                ));
            }
            _ if token.starts_with('/') || token.starts_with('-') => {
                return Err(AppError::invalid("SecureCRT 参数不受支持"));
            }
            _ => {
                if host.replace(token.to_owned()).is_some() {
                    return Err(AppError::invalid("SecureCRT 参数包含多个目标主机"));
                }
                index += 1;
            }
        }
    }
    let host = host.ok_or_else(|| AppError::invalid("SecureCRT 参数缺少目标主机"))?;
    Ok(request(
        ExternalLaunchSource::SecureCrt,
        ExternalTargetDraft {
            protocol: RemoteProtocol::Ssh,
            host: validated_host(&host)?,
            port,
            username,
            jump_hosts: Vec::new(),
        },
        ExternalLaunchIntent::SshTerminal {
            remote_command: Vec::new(),
        },
        None,
        identity_file,
    ))
}

/// 只接受 MobaXterm 直接携带的 OpenSSH 命令，私有 .moba profile 不被解密或猜测。
fn parse_mobaxterm(arguments: &[String]) -> AppResult<ExternalLaunchRequest> {
    let marker = arguments
        .iter()
        .position(|argument| {
            argument.eq_ignore_ascii_case("-newtab") || argument.eq_ignore_ascii_case("-exec")
        })
        .ok_or_else(|| AppError::invalid("MobaXterm 只支持显式 SSH 命令启动"))?;
    let command = arguments
        .get(marker + 1)
        .ok_or_else(|| AppError::invalid("MobaXterm 启动命令缺少内容"))?;
    if command.to_ascii_lowercase().ends_with(".moba") {
        return Err(AppError::invalid(
            "MobaXterm 私有会话文件不会被外部启动读取",
        ));
    }
    let mut command_tokens = split_command_line(command)
        .map_err(|_| AppError::invalid("MobaXterm SSH 命令引号格式无效"))?;
    for trailing in &arguments[marker + 2..] {
        command_tokens.extend(
            split_command_line(trailing)
                .map_err(|_| AppError::invalid("MobaXterm SSH 命令引号格式无效"))?,
        );
    }
    if command_tokens.first().is_some_and(|value| {
        executable_name(value).eq_ignore_ascii_case("ssh")
            || executable_name(value).eq_ignore_ascii_case("ssh.exe")
    }) {
        command_tokens.remove(0);
    }
    if command_tokens.is_empty() {
        return Err(AppError::invalid("MobaXterm 启动命令缺少 SSH 目标"));
    }
    let mut argv = vec!["ssh".to_owned()];
    argv.extend(command_tokens);
    parse_openssh(&argv, ExternalLaunchSource::MobaXterm)
}

/// 解析 Xshell -url 中的标准 SSH URL；厂商私有 BHost 和 Base64 信封明确拒绝。
fn parse_xshell(arguments: &[String]) -> AppResult<ExternalLaunchRequest> {
    let mut raw_url = None;
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if argument.eq_ignore_ascii_case("-url") {
            if raw_url.is_some() {
                return Err(AppError::invalid("Xshell 外部启动包含多个 URL"));
            }
            raw_url = Some(
                arguments
                    .get(index + 1)
                    .ok_or_else(|| AppError::invalid("Xshell -url 缺少链接"))?
                    .as_str(),
            );
            index += 2;
        } else if argument.eq_ignore_ascii_case("-newwin") {
            index += 1;
        } else if starts_with_scheme(argument, "ssh://") {
            if raw_url.replace(argument).is_some() {
                return Err(AppError::invalid("Xshell 外部启动包含多个 URL"));
            }
            index += 1;
        } else {
            return Err(AppError::invalid("Xshell 参数不受支持"));
        }
    }
    let raw_url = raw_url.ok_or_else(|| AppError::invalid("Xshell 只支持标准 ssh:// 外部链接"))?;
    if raw_url.to_ascii_lowercase().starts_with("b64") {
        return Err(AppError::invalid("Xshell 私有编码链接不会被解码"));
    }
    parse_remote_url(raw_url, RemoteProtocol::Ssh, ExternalLaunchSource::Xshell)
}

/// 解析 SFTP URL 和有限安全选项；私有站点存储、密码文件与内联认证材料不读取。
fn parse_sftp_client(arguments: &[String]) -> AppResult<ExternalLaunchRequest> {
    let mut raw_url = None;
    let mut identity_file = None;
    let mut host_key = None;
    let mut username = None;
    let mut index = 1;
    while index < arguments.len() {
        let token = arguments[index].as_str();
        if starts_with_scheme(token, "sftp://") {
            if raw_url.replace(token).is_some() {
                return Err(AppError::invalid("SFTP 外部启动不能包含多个 URL"));
            }
            index += 1;
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if lower == "-i" || lower == "/privatekey" {
            identity_file = Some(nonempty_path(&plain_next_value(
                arguments, &mut index, token,
            )?)?);
        } else if lower == "/hostkey" || lower == "--host-key" {
            host_key = Some(plain_next_value(arguments, &mut index, token)?);
        } else if lower == "/username" || lower == "--user" {
            username = Some(validated_username(&plain_next_value(
                arguments, &mut index, token,
            )?)?);
        } else if lower == "/password"
            || lower == "/passphrase"
            || lower == "--password"
            || lower == "--passphrase"
            || lower == "--site"
        {
            return Err(AppError::invalid(
                "SFTP 客户端密码或私有站点参数不允许外部启动",
            ));
        } else if token.starts_with('-') || token.starts_with('/') {
            return Err(AppError::invalid("SFTP 客户端参数不受支持"));
        } else {
            return Err(AppError::invalid("SFTP 客户端包含不支持的位置参数"));
        }
    }
    let raw_url = raw_url.ok_or_else(|| AppError::invalid("SFTP 客户端需要 sftp:// URL"))?;
    let mut request = parse_remote_url(
        raw_url,
        RemoteProtocol::Sftp,
        ExternalLaunchSource::SftpClient,
    )?;
    if request.target.username.is_none() {
        request.target.username = username;
    }
    request.identity_file = identity_file;
    if let ExternalLaunchIntent::SftpTransfer {
        expected_host_key, ..
    } = &mut request.intent
    {
        *expected_host_key = host_key;
    }
    Ok(request)
}

/// 从标准 URL 读取目标与路径，同时保留原始主机文本避免 URL 库的域名改写进入连接匹配。
fn parse_remote_url(
    raw_url: &str,
    protocol: RemoteProtocol,
    source: ExternalLaunchSource,
) -> AppResult<ExternalLaunchRequest> {
    let url = Url::parse(raw_url).map_err(|_| AppError::invalid("外部连接 URL 无效"))?;
    let expected_scheme = if protocol == RemoteProtocol::Sftp {
        "sftp"
    } else {
        "ssh"
    };
    if url.scheme() != expected_scheme
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::invalid("外部连接 URL 含不支持的协议或敏感字段"));
    }
    let (raw_user, host, port) =
        raw_url_authority(raw_url, url.port().unwrap_or(DEFAULT_SSH_PORT))?;
    let username = raw_user
        .map(|value| strict_percent_decode(value, "URL 用户名无效"))
        .transpose()?
        .map(|value| validated_username(&value))
        .transpose()?;
    let path = raw_url_path(raw_url)?;
    let intent = if protocol == RemoteProtocol::Sftp {
        let path = if path.is_empty() {
            "/".to_owned()
        } else {
            path
        };
        validate_remote_path(&path)?;
        ExternalLaunchIntent::SftpTransfer {
            remote_path: path,
            expected_host_key: None,
        }
    } else {
        if !path.is_empty() && path != "/" {
            return Err(AppError::invalid("SSH URL path不能作为远端命令"));
        }
        ExternalLaunchIntent::SshTerminal {
            remote_command: Vec::new(),
        }
    };
    Ok(request(
        source,
        ExternalTargetDraft {
            protocol,
            host,
            port,
            username,
            jump_hosts: Vec::new(),
        },
        intent,
        None,
        None,
    ))
}

/// 把原始 authority 拆成用户、主机和端口，避免把密码悄然归一化后丢失。
fn raw_url_authority(raw_url: &str, default_port: u16) -> AppResult<(Option<&str>, String, u16)> {
    let (_, remainder) = raw_url
        .split_once("://")
        .ok_or_else(|| AppError::invalid("外部连接 URL 缺少 authority"))?;
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    let (userinfo, hostport) = authority
        .rsplit_once('@')
        .map_or((None, authority), |parts| (Some(parts.0), parts.1));
    if userinfo.is_some_and(|value| value.contains(':')) {
        return Err(AppError::invalid("外部连接 URL 不允许携带密码"));
    }
    let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| AppError::invalid("外部 IPv6 主机格式无效"))?;
        let host = &bracketed[..close];
        host.parse::<Ipv6Addr>()
            .map_err(|_| AppError::invalid("外部 IPv6 主机格式无效"))?;
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            let port = suffix
                .strip_prefix(':')
                .ok_or_else(|| AppError::invalid("外部 URL 端口格式无效"))?;
            parse_port(port)?
        };
        (host.to_owned(), port)
    } else {
        if hostport.matches(':').count() > 1 {
            return Err(AppError::invalid("IPv6 外部 URL 必须使用方括号"));
        }
        if let Some((host, raw_port)) = hostport.rsplit_once(':') {
            (validated_host(host)?, parse_port(raw_port)?)
        } else {
            (validated_host(hostport)?, default_port)
        }
    };
    Ok((userinfo, validated_host(&host)?, port))
}

/// 解码 URL path 的百分号字节；路径保持远端路径语义，不参与本地文件解析。
fn raw_url_path(raw_url: &str) -> AppResult<String> {
    let (_, remainder) = raw_url
        .split_once("://")
        .ok_or_else(|| AppError::invalid("外部连接 URL 缺少 authority"))?;
    let path_and_more = remainder
        .find(['/', '?', '#'])
        .map(|index| &remainder[index..])
        .unwrap_or_default();
    let path = path_and_more.split(['?', '#']).next().unwrap_or_default();
    strict_percent_decode(path, "外部 URL 路径编码无效")
}

/// 拒绝重复 query 字段并以严格 UTF-8 解码，避免不同解析层对链接产生不同解释。
fn strict_query_pairs(url: &Url) -> AppResult<Vec<(String, String)>> {
    let Some(query) = url.query() else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = strict_query_component(key)?;
        let value = strict_query_component(value)?;
        if pairs.iter().any(|(existing, _)| existing == &key) {
            return Err(AppError::invalid("外部链接包含重复参数"));
        }
        pairs.push((key, value));
    }
    Ok(pairs)
}

/// 表单 query 的加号表示空格；严格检查百分号编码和 UTF-8。
fn strict_query_component(value: &str) -> AppResult<String> {
    strict_percent_decode(&value.replace('+', " "), "外部链接查询参数编码无效")
}

/// 解码一个百分号编码字段，拒绝无效字节而不是用替代字符掩盖输入错误。
fn strict_percent_decode(value: &str, error: &'static str) -> AppResult<String> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| AppError::invalid(error))
}

/// 提取唯一 query 值；重复项已在 strict_query_pairs 中被拒绝。
fn query_value(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

/// 把 Kerminal URL 的必填字段转换为目标草稿，用户缺省时保留认证提示流程。
fn query_target(
    pairs: &[(String, String)],
    protocol: RemoteProtocol,
) -> AppResult<ExternalTargetDraft> {
    let host = query_value(pairs, "host").ok_or_else(|| AppError::invalid("外部链接缺少 host"))?;
    let username = query_value(pairs, "user").or_else(|| query_value(pairs, "username"));
    if query_value(pairs, "user").is_some() && query_value(pairs, "username").is_some() {
        return Err(AppError::invalid("外部链接不能同时指定 user 和 username"));
    }
    let username = username
        .map(|value| validated_username(&value))
        .transpose()?;
    let port = query_value(pairs, "port")
        .map(|value| parse_port(&value))
        .transpose()?
        .unwrap_or(DEFAULT_SSH_PORT);
    Ok(ExternalTargetDraft {
        protocol,
        host: validated_host(&host)?,
        port,
        username,
        jump_hosts: Vec::new(),
    })
}

/// 对查询字段实施逐 action 白名单，防止未实现的认证和本地路径参数被忽略。
fn ensure_allowed_keys(pairs: &[(String, String)], allowed: &[&str]) -> AppResult<()> {
    if pairs
        .iter()
        .any(|(key, _)| !allowed.contains(&key.as_str()))
    {
        Err(AppError::invalid("外部链接包含不支持的参数"))
    } else {
        Ok(())
    }
}

/// 从 OpenSSH、PuTTY 与 SFTP URL 支持一致的 host[:port] 和 user@host 写法。
fn parse_user_host_port(value: &str) -> AppResult<(Option<String>, String, Option<u16>)> {
    let (username, hostport) = value
        .rsplit_once('@')
        .map_or((None, value), |(user, host)| (Some(user.to_owned()), host));
    let username = username.map(|user| validated_username(&user)).transpose()?;
    if let Some(bracketed) = hostport.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| AppError::invalid("IPv6 目标格式无效"))?;
        let host = &bracketed[..close];
        host.parse::<Ipv6Addr>()
            .map_err(|_| AppError::invalid("IPv6 目标格式无效"))?;
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(parse_port(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| AppError::invalid("目标端口格式无效"))?,
            )?)
        };
        return Ok((username, host.to_owned(), port));
    }
    if hostport.matches(':').count() > 1 {
        return Err(AppError::invalid("IPv6 目标必须使用方括号"));
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((host, port)) => (host, Some(parse_port(port)?)),
        None => (hostport, None),
    };
    Ok((username, validated_host(host)?, port))
}

/// 将 OpenSSH -J 中按顺序列出的跳板解析为应用层有序身份链。
fn parse_jump_chain(value: &str) -> AppResult<Vec<ExternalJumpHostDraft>> {
    let mut jumps = Vec::new();
    for item in value.split(',') {
        if item.trim().is_empty() {
            return Err(AppError::invalid("跳板链包含空目标"));
        }
        let (username, host, port) = parse_user_host_port(item)?;
        jumps.push(ExternalJumpHostDraft {
            host,
            port: port.unwrap_or(DEFAULT_SSH_PORT),
            username,
        });
    }
    if jumps.len() > 8 {
        return Err(AppError::invalid("跳板链超过支持上限"));
    }
    Ok(jumps)
}

/// 验证远端路径为绝对路径且没有控制字符，避免客户端把相对路径解释成执行参数。
fn validate_remote_path(path: &str) -> AppResult<()> {
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err(AppError::invalid("SFTP 远端路径必须是无控制字符的绝对路径"));
    }
    Ok(())
}

/// 排除内联秘密与秘密包络，即便调用方声称来自可信外部客户端也不例外。
fn reject_secret_arguments(arguments: &[String]) -> AppResult<()> {
    for argument in arguments {
        let normalized = argument
            .split_once('=')
            .map_or(argument.as_str(), |(name, _)| name)
            .to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "-pw"
                | "-pwfile"
                | "--password"
                | "--passphrase"
                | "--key-passphrase"
                | "-passphrase"
                | "/password"
                | "/passphrase"
                | "--external-ssh-json"
                | "--external-key-json"
                | "--password-file"
                | "--key-passphrase-file"
        ) || argument.to_ascii_lowercase().starts_with("b64%3e%3e")
            || argument.to_ascii_lowercase().starts_with("b64>>")
        {
            return Err(AppError::invalid("外部启动不能携带密码、口令或私钥信封"));
        }
        if (normalized == "-pw" || normalized == "/password") && argument.contains('=') {
            return Err(AppError::invalid("外部启动不能携带密码"));
        }
    }
    Ok(())
}

/// 按参数名匹配选项时忽略大小写，但不把值误当成选项。
fn has_named_option(arguments: &[String], options: &[&str]) -> bool {
    arguments.iter().any(|argument| {
        let name = argument
            .split_once('=')
            .map_or(argument.as_str(), |(key, _)| key);
        options
            .iter()
            .any(|option| name.eq_ignore_ascii_case(option))
    })
}

/// 取下一项位置的可选参数，支持 --name=value 并拒绝空值。
fn take_option_value(
    arguments: &[String],
    index: &mut usize,
    inline: Option<&str>,
    name: &str,
) -> AppResult<String> {
    let value = if let Some(value) = inline {
        *index += 1;
        value.to_owned()
    } else {
        plain_next_value(arguments, index, name)?
    };
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(AppError::invalid("外部启动参数值无效"));
    }
    Ok(value)
}

/// 取得短选项值，参数在当前元素中连接时不额外消费下一项。
fn next_option_value(
    arguments: &[String],
    index: &mut usize,
    attached: Option<&str>,
    name: &str,
) -> AppResult<String> {
    let value = if let Some(value) = attached {
        *index += 1;
        value.to_owned()
    } else {
        plain_next_value(arguments, index, name)?
    };
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(AppError::invalid("外部启动参数值无效"));
    }
    Ok(value)
}

/// 获取下一个参数并前移游标；不接受缺失值或把另一个选项当作值。
fn plain_next_value(arguments: &[String], index: &mut usize, option: &str) -> AppResult<String> {
    let value = arguments
        .get(*index + 1)
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or_else(|| {
            let _ = option;
            AppError::invalid("外部启动选项缺少参数值")
        })?
        .clone();
    *index += 2;
    Ok(value)
}

/// 将端口限制在非零 u16，避免默认端口补值与连接端口分歧。
fn parse_port(value: &str) -> AppResult<u16> {
    let port = value
        .parse::<u16>()
        .map_err(|_| AppError::invalid("外部连接端口无效"))?;
    if port == 0 {
        return Err(AppError::invalid("外部连接端口无效"));
    }
    Ok(port)
}

/// 拒绝空白、控制字符和 URI 分隔符主机，保留合法 IPv6 原始文本供身份规范化。
fn validated_host(host: &str) -> AppResult<String> {
    let host = host.trim();
    if host.is_empty()
        || host.len() > 253
        || host.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '/' | '\\' | '?' | '#' | '@')
        })
    {
        return Err(AppError::invalid("外部连接主机名无效"));
    }
    Ok(host.to_owned())
}

/// 保留用户名大小写并拒绝空值与控制字符，以便 exact saved lookup 复刻旧身份语义。
fn validated_username(username: &str) -> AppResult<String> {
    let username = username.trim();
    if username.is_empty() || username.chars().any(char::is_control) {
        return Err(AppError::invalid("外部连接用户名无效"));
    }
    Ok(username.to_owned())
}

/// 显示名是外部输入，限制长度并拒绝控制字符以免污染窗口标题和系统通知。
fn validated_display_name(value: &str) -> AppResult<String> {
    if value.len() > 160 || value.chars().any(char::is_control) {
        return Err(AppError::invalid("外部连接显示名无效"));
    }
    Ok(value.to_owned())
}

/// 私钥路径只保留在本次运行请求，不展开、不写配置，也不会进入 Debug 输出。
fn nonempty_path(value: &str) -> AppResult<PathBuf> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(AppError::invalid("外部私钥路径无效"));
    }
    Ok(PathBuf::from(value))
}

/// 仅提取命令可执行文件名，兼容 Windows 路径而不依赖当前操作系统路径语义。
fn executable_name(value: &str) -> String {
    value
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase()
}

/// 识别一个 scheme 前缀时忽略大小写，避免 URL 大小写令来源推断与安全校验分叉。
fn starts_with_scheme(value: &str, scheme: &str) -> bool {
    value
        .get(..scheme.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
}

/// 拆出 `--option=value`，普通选项值仍由调用方按游标消费。
fn split_long_option(value: &str) -> (&str, Option<&str>) {
    value
        .split_once('=')
        .map_or((value, None), |(name, value)| (name, Some(value)))
}

/// 拆出 OpenSSH 风格 `-p22` 等紧凑选项，未知长选项不会被截断后误接受。
fn split_short_option(value: &str) -> (&str, Option<&str>) {
    if value.starts_with("--") || value.len() <= 2 {
        (value, None)
    } else {
        match (value.get(..2), value.get(2..)) {
            (Some(name), Some(attached)) => (name, Some(attached)),
            _ => (value, None),
        }
    }
}

/// 识别 query boolean 的有限真值，其他值一律视为关闭而不触发隐式功能。
fn truthy(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// 从参数数组创建不带诊断原文的应用请求，秘密与未经验证的命令不会被串接。
fn request(
    source: ExternalLaunchSource,
    target: ExternalTargetDraft,
    intent: ExternalLaunchIntent,
    display_name: Option<String>,
    identity_file: Option<PathBuf>,
) -> ExternalLaunchRequest {
    ExternalLaunchRequest {
        source,
        target,
        intent,
        display_name: display_name.filter(|value| !value.trim().is_empty()),
        identity_file,
    }
}
