//! Windows executable discovery and batch-shim command-line encoding for ConPTY.
//!
//! Batch argument encoding adapts Rust std's security-hardened `append_bat_arg` algorithm from
//! [`library/std/src/sys/args/windows.rs`](https://github.com/rust-lang/rust/blob/main/library/std/src/sys/args/windows.rs),
//! Copyright The Rust Project Developers, licensed MIT OR Apache-2.0. It preserves this crate's
//! structured `OsStr` argv and `CreateProcessW` call, and rejects input that cmd cannot represent
//! safely.
//!
//! @author kongweiguang

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use std::os::windows::ffi::{OsStrExt, OsStringExt};

/// Distinguishes native CreateProcess targets from batch shims and selected but unsupported files.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ResolvedProgram {
    Native(OsString),
    Batch(OsString),
    Unsupported(OsString),
    NotFound,
}

enum Candidate {
    Selected(ResolvedProgram),
    PosixShim(PathBuf),
    NotFound,
}

/// 按系统 reparse tag 识别应用执行别名，避免把所有 WindowsApps 路径或零字节文件误作 Store 入口。
/// 输入是 cmdline 已解析、以 NUL 结尾的完整 exe；只查文件元数据，不展开或替换用户选定的程序。
///
/// @author kongweiguang
pub(crate) fn is_app_execution_alias(exe: &[u16]) -> io::Result<bool> {
    use winapi::um::fileapi::{FindClose, FindFirstFileW};
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::minwinbase::WIN32_FIND_DATAW;
    use winapi::um::winnt::{FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_APPEXECLINK};

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: cmdline supplies a NUL-terminated path; data is initialized and writable.
    let search = unsafe { FindFirstFileW(exe.as_ptr(), &mut data) };
    if search == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this function owns the successful search handle and closes it exactly once.
    unsafe { FindClose(search) };
    Ok(data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && data.dwReserved0 == IO_REPARSE_TAG_APPEXECLINK)
}

/// Resolves a command in PATH-directory then PATHEXT order without skipping a selected batch shim.
///
/// An extensionless shebang file is not a Windows process image, so it is skipped while searching
/// that directory's PATHEXT candidates and then later PATH entries. Other selected unsupported
/// formats stop resolution rather than silently changing which executable the user chose.
///
/// @author kongweiguang
pub(crate) fn resolve_program(
    program: &OsStr,
    path_value: Option<&OsStr>,
    pathext_value: Option<&OsStr>,
) -> io::Result<ResolvedProgram> {
    let extensions = path_extensions(pathext_value);
    let requested_path = Path::new(program);

    if !is_bare_program_name(requested_path) {
        let requested_path = std::path::absolute(requested_path)?;
        return Ok(
            match resolve_in_directory(
                requested_path.parent().unwrap_or_else(|| Path::new(".")),
                requested_path.file_name().unwrap_or(program),
                &extensions,
            ) {
                Candidate::Selected(resolved) => resolved,
                Candidate::PosixShim(path) => ResolvedProgram::Unsupported(path.into_os_string()),
                Candidate::NotFound => ResolvedProgram::NotFound,
            },
        );
    }

    let mut skipped_posix_shim = None;
    if let Some(path_value) = path_value {
        for directory in std::env::split_paths(path_value) {
            // PATH entries are interpreted against the parent process, not the child's requested cwd.
            let directory = std::path::absolute(directory)?;
            match resolve_in_directory(&directory, program, &extensions) {
                Candidate::Selected(ResolvedProgram::Unsupported(path)) => {
                    return Ok(ResolvedProgram::Unsupported(path));
                }
                Candidate::Selected(selected) => return Ok(selected),
                Candidate::PosixShim(path) => {
                    skipped_posix_shim.get_or_insert(path.into_os_string());
                }
                Candidate::NotFound => {}
            }
        }
    }

    Ok(skipped_posix_shim.map_or(ResolvedProgram::NotFound, ResolvedProgram::Unsupported))
}

/// Resolves one PATH directory with its exact name ahead of PATHEXT entries.
///
/// @author kongweiguang
fn resolve_in_directory(directory: &Path, program: &OsStr, extensions: &[OsString]) -> Candidate {
    let exact = directory.join(program);
    let mut skipped_posix_shim = None;
    if exact.is_file() {
        if is_extensionless(&exact) && is_posix_shebang(&exact) {
            skipped_posix_shim = Some(exact.clone());
        } else {
            return Candidate::Selected(classify_candidate(exact));
        }
    }

    if !is_extensionless(Path::new(program)) {
        return skipped_posix_shim.map_or(Candidate::NotFound, Candidate::PosixShim);
    }

    for extension in extensions {
        let Some(candidate) = with_extension(&exact, extension) else {
            continue;
        };
        if candidate.is_file() {
            return Candidate::Selected(classify_candidate(candidate));
        }
    }

    skipped_posix_shim.map_or(Candidate::NotFound, Candidate::PosixShim)
}

/// Classifies only formats that CreateProcessW can start directly or through cmd.exe.
///
/// @author kongweiguang
fn classify_candidate(path: PathBuf) -> ResolvedProgram {
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("exe") | Some("com") => ResolvedProgram::Native(path.into_os_string()),
        Some("cmd") | Some("bat") => ResolvedProgram::Batch(path.into_os_string()),
        Some(_) => ResolvedProgram::Unsupported(path.into_os_string()),
    }
}

/// Returns PATHEXT in its configured order, with Windows' standard native/batch defaults if absent.
///
/// Invalid entries are ignored as candidates; they never reorder valid extensions.
///
/// @author kongweiguang
fn path_extensions(pathext_value: Option<&OsStr>) -> Vec<OsString> {
    let configured = pathext_value.unwrap_or(OsStr::new(".COM;.EXE;.BAT;.CMD"));
    std::env::split_paths(configured)
        .filter_map(|extension| {
            let value = extension.to_str()?;
            let suffix = value.strip_prefix('.')?;
            if suffix.is_empty()
                || suffix
                    .chars()
                    .any(|character| matches!(character, '\\' | '/' | ':' | ';'))
            {
                return None;
            }
            Some(OsString::from(value))
        })
        .collect()
}

/// Adds a PATHEXT suffix without replacing an explicitly supplied extension.
///
/// @author kongweiguang
fn with_extension(path: &Path, extension: &OsStr) -> Option<PathBuf> {
    if !is_extensionless(path) {
        return None;
    }
    let suffix = extension.to_str()?.strip_prefix('.')?;
    if suffix.is_empty() {
        return None;
    }
    let mut candidate = path.to_path_buf();
    candidate.set_extension(suffix);
    Some(candidate)
}

/// Detects a POSIX shebang without interpreting or logging any script contents.
///
/// @author kongweiguang
fn is_posix_shebang(path: &Path) -> bool {
    let mut prefix = [0u8; 2];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut prefix))
        .is_ok_and(|()| prefix == *b"#!")
}

/// Checks whether a path has no extension under Windows path rules.
///
/// @author kongweiguang
fn is_extensionless(path: &Path) -> bool {
    path.extension().is_none()
}

/// Distinguishes a PATH lookup name from a path explicitly selected by the caller.
///
/// @author kongweiguang
fn is_bare_program_name(path: &Path) -> bool {
    use std::path::Component;

    matches!(path.components().next(), Some(Component::Normal(_))) && path.components().count() == 1
}

/// Returns the system cmd.exe path so PATH cannot substitute a different command interpreter.
///
/// @author kongweiguang
pub(crate) fn system_command_prompt() -> io::Result<OsString> {
    use winapi::um::sysinfoapi::GetSystemDirectoryW;

    let mut buffer = vec![0u16; 260];
    // SAFETY: the buffer is writable for the supplied element count and does not outlive the call.
    let mut length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize >= buffer.len() {
        buffer.resize(length as usize + 1, 0);
        // SAFETY: the resized buffer is writable for the supplied element count.
        length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 || length as usize >= buffer.len() {
            return Err(io::Error::last_os_error());
        }
    }

    let mut path = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
    path.push("cmd.exe");
    Ok(path.into_os_string())
}

/// Builds Rust std's hardened batch invocation while preserving the original argv as distinct items.
///
/// `cmd /e:ON` is required for Rust std's percent-expansion neutralizer; `/v:OFF` prevents delayed
/// `!VAR!` expansion and `/d` disables user AutoRun commands. NUL/CR/LF and unsafe shim paths fail
/// closed. The caller supplies this command line directly to the existing ConPTY CreateProcessW.
///
/// @author kongweiguang
pub(crate) fn batch_command_line(script: &OsStr, args: &[OsString]) -> io::Result<Vec<u16>> {
    let script: Vec<u16> = script.encode_wide().collect();
    if script.contains(&0)
        || script.contains(&(b'"' as u16))
        || script.last() == Some(&(b'\\' as u16))
        || script.contains(&(b'%' as u16))
    {
        return Err(invalid_batch_arguments());
    }

    let mut command: Vec<u16> = "cmd.exe /e:ON /v:OFF /d /c \"".encode_utf16().collect();
    command.push(b'"' as u16);
    command.extend(script);
    command.push(b'"' as u16);

    for argument in args {
        command.push(b' ' as u16);
        append_batch_argument(&mut command, argument)?;
    }
    command.push(b'"' as u16);

    // CreateProcessW limits its command line to 32,767 UTF-16 code units including the terminator.
    if command.len() + 1 > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows batch command line is too long",
        ));
    }
    Ok(command)
}

/// Encodes one batch parameter using the Rust std compatibility algorithm and rejects CR/LF/NUL.
///
/// @author kongweiguang
fn append_batch_argument(command: &mut Vec<u16>, argument: &OsStr) -> io::Result<()> {
    let wide: Vec<u16> = argument.encode_wide().collect();
    if wide.contains(&0) || wide.iter().any(|unit| matches!(*unit, 0x0a | 0x0d)) {
        return Err(invalid_batch_arguments());
    }

    let mut quote = wide.is_empty() || wide.last() == Some(&(b'\\' as u16));
    for decoded in char::decode_utf16(wide.iter().copied()) {
        if let Ok(character) = decoded {
            let ascii_needs_quotes = character.is_ascii()
                && !(character.is_ascii_alphanumeric() || "#$*+-./:?@\\_".contains(character));
            quote |= ascii_needs_quotes || character.is_control();
        }
    }

    if quote {
        command.push(b'"' as u16);
    }

    let mut backslashes = 0usize;
    for unit in wide {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else {
            if unit == b'"' as u16 {
                command.extend(std::iter::repeat(b'\\' as u16).take(backslashes));
                command.push(b'"' as u16);
            } else if unit == b'%' as u16 || unit == 0x0d {
                // Rust std's no-op substring expansion prevents cmd from expanding `%NAME%` input.
                command.extend_from_slice(&[
                    b'%' as u16,
                    b'%' as u16,
                    b'c' as u16,
                    b'd' as u16,
                    b':' as u16,
                    b'~' as u16,
                    b',' as u16,
                ]);
            }
            backslashes = 0;
        }
        command.push(unit);
    }

    if quote {
        command.extend(std::iter::repeat(b'\\' as u16).take(backslashes));
        command.push(b'"' as u16);
    }
    Ok(())
}

/// Keeps batch parser errors generic so diagnostics never include user arguments or executable paths.
///
/// @author kongweiguang
fn invalid_batch_arguments() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "batch file arguments are invalid",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::convert::TryInto;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    /// Pins Windows' empty-parent semantics so a bare command still uses PATH/PATHEXT lookup.
    ///
    /// @author kongweiguang
    #[test]
    fn bare_program_names_are_distinguished_from_explicit_paths() {
        assert_eq!(Path::new("codex").parent(), Some(Path::new("")));
        assert!(is_bare_program_name(Path::new("codex")));
        assert!(is_bare_program_name(Path::new("codex.cmd")));
        assert!(!is_bare_program_name(Path::new(r".\codex")));
        assert!(!is_bare_program_name(Path::new(r"tools\codex")));
        assert!(!is_bare_program_name(Path::new(r"C:codex")));
        assert!(!is_bare_program_name(Path::new(r"C:\tools\codex")));
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        /// Creates an exclusive temporary directory; Drop removes only the directory this test owns.
        ///
        /// @author kongweiguang
        fn create() -> io::Result<Self> {
            for _ in 0..16 {
                let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "portable-pty-win-launch-{}-{nonce}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate an isolated launcher fixture directory",
            ))
        }

        /// Exposes the unique test-owned root for files created by the fixture.
        ///
        /// @author kongweiguang
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        /// Removes the uniquely owned fixture root after every success or failure path.
        ///
        /// @author kongweiguang
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Confirms PATH directory order, PATHEXT order, and POSIX shim skipping without spawning a process.
    ///
    /// @author kongweiguang
    #[test]
    fn path_and_pathext_order_skip_only_extensionless_posix_shims() {
        let root = TestDirectory::create().expect("temporary root");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir(&first).expect("first path directory");
        fs::create_dir(&second).expect("second path directory");
        fs::write(first.join("codex"), b"#!/bin/sh\nexit 0\n").expect("POSIX shim");
        fs::write(first.join("codex.cmd"), b"@echo off\r\n").expect("batch shim");
        fs::write(first.join("codex.exe"), b"native image fixture").expect("native fixture");
        fs::write(second.join("codex.exe"), b"later native fixture").expect("later native");

        let path = std::env::join_paths([&first, &second]).expect("PATH fixture");
        let batch_first = resolve_program(
            OsStr::new("codex"),
            Some(&path),
            Some(OsStr::new(".CMD;.EXE")),
        )
        .expect("resolve first PATH candidate");
        assert_eq!(
            batch_first,
            ResolvedProgram::Batch(first.join("codex.CMD").into_os_string())
        );

        let native_first = resolve_program(
            OsStr::new("codex"),
            Some(&path),
            Some(OsStr::new(".EXE;.CMD")),
        )
        .expect("resolve PATHEXT native candidate");
        assert_eq!(
            native_first,
            ResolvedProgram::Native(first.join("codex.EXE").into_os_string())
        );

        fs::remove_file(first.join("codex.exe")).expect("remove first native fixture");
        let later_native = resolve_program(
            OsStr::new("codex"),
            Some(&std::env::join_paths([&first, &second]).expect("updated PATH fixture")),
            Some(OsStr::new(".EXE;.CMD")),
        )
        .expect("resolve first PATH batch candidate");
        assert_eq!(
            later_native,
            ResolvedProgram::Batch(first.join("codex.CMD").into_os_string()),
            "a selected earlier .cmd must not be skipped for a later .exe"
        );

        let default_extensions = resolve_program(
            OsStr::new("codex"),
            Some(&std::env::join_paths([&first, &second]).expect("default PATH fixture")),
            None,
        )
        .expect("resolve default PATHEXT candidate");
        assert_eq!(
            default_extensions,
            ResolvedProgram::Batch(first.join("codex.CMD").into_os_string()),
            "default PATHEXT must include batch shims after native executable suffixes"
        );
    }

    /// Confirms the selected .cmd receives each structured argv item through real cmd.exe unchanged.
    ///
    /// This compiles a tiny recorder into the test-owned directory, invokes only the generated
    /// fixture shim, and asserts both the binary argv receipt and absence of the injection marker.
    ///
    /// @author kongweiguang
    #[test]
    fn batch_arguments_round_trip_through_real_cmd_without_injection() {
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::process::CommandExt;

        let root = TestDirectory::create().expect("temporary root");
        let helper_source = root.path().join("argv-recorder.rs");
        let helper = root.path().join("argv-recorder.exe");
        let shim = root.path().join("record-argv.cmd");
        let record = root.path().join("arguments.bin");
        let marker = root.path().join("INJECTED_MARKER");

        fs::write(
            &helper_source,
            r#"use std::{env, fs::File, io::Write};
use std::os::windows::ffi::OsStrExt;
fn main() {
    let path = env::var_os("KERMINAL_ARGV_RECORD").expect("record path");
    let mut file = File::create(path).expect("record file");
    for argument in env::args_os().skip(1) {
        let wide: Vec<u16> = argument.encode_wide().collect();
        file.write_all(&(wide.len() as u32).to_le_bytes()).expect("length");
        for unit in wide {
            file.write_all(&unit.to_le_bytes()).expect("argument");
        }
    }
}"#,
        )
        .expect("write isolated helper source");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let compile = Command::new(rustc)
            .arg("--edition=2021")
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .output()
            .expect("run rustc for isolated argv recorder");
        assert!(
            compile.status.success(),
            "isolated helper compilation failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );

        fs::write(
            &shim,
            b"@echo off\r\nsetlocal DisableDelayedExpansion\r\n\"%~dp0argv-recorder.exe\" %*\r\nexit /b %errorlevel%\r\n",
        )
        .expect("write isolated cmd shim");

        let arguments = vec![
            OsString::from("argument with spaces"),
            OsString::from("embedded\"quote"),
            OsString::from("trailing-backslash\\"),
            OsString::from("percent%PATH%value"),
            OsString::from("bang!value"),
            OsString::from("caret^value"),
            OsString::from("pipe|and&redirect<>value"),
            OsString::from("中文🙂"),
            OsString::new(),
            OsString::from(format!(
                "safe&echo INJECTED>{}",
                marker.file_name().unwrap().to_string_lossy()
            )),
        ];

        let mut builder = crate::CommandBuilder::new(&shim);
        builder.args(&arguments);
        builder.cwd(root.path());
        let (command_prompt, command_line) = builder.cmdline().expect("build PTY launch command");
        let command_prompt = OsString::from_wide(&command_prompt[..command_prompt.len() - 1]);
        let command_line = &command_line[..command_line.len() - 1];
        let command_line = String::from_utf16(command_line).expect("valid command line UTF-16");
        let arguments_tail = command_line
            .strip_prefix("cmd.exe")
            .expect("std-compatible argv[0]");

        let output = Command::new(command_prompt)
            .raw_arg(arguments_tail)
            .current_dir(root.path())
            .env("KERMINAL_ARGV_RECORD", &record)
            .output()
            .expect("start only the isolated cmd fixture");
        assert!(
            output.status.success(),
            "fixture cmd failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!marker.exists(), "an argument executed a shell command");

        let bytes = fs::read(&record).expect("read isolated argv receipt");
        let actual = decode_wide_arguments(&bytes).expect("well-formed argv receipt");
        assert_eq!(actual, arguments);
    }

    /// Decodes the test helper's length-prefixed UTF-16 receipt without lossy Unicode conversion.
    ///
    /// @author kongweiguang
    fn decode_wide_arguments(bytes: &[u8]) -> io::Result<Vec<OsString>> {
        let mut offset = 0usize;
        let mut arguments = Vec::new();
        while offset < bytes.len() {
            let length_bytes: [u8; 4] = bytes
                .get(offset..offset + 4)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing length"))?
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid length"))?;
            offset += 4;
            let units_len = u32::from_le_bytes(length_bytes) as usize;
            let bytes_len = units_len
                .checked_mul(2)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "length overflow"))?;
            let encoded = bytes
                .get(offset..offset + bytes_len)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated argument"))?;
            let units = encoded
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>();
            arguments.push(OsString::from_wide(&units));
            offset += bytes_len;
        }
        Ok(arguments)
    }

    /// Confirms command-injection control characters and NUL/CR/LF are rejected by the encoder.
    ///
    /// @author kongweiguang
    #[test]
    fn batch_argument_encoder_rejects_unrepresentable_input() {
        assert!(batch_command_line(
            OsStr::new(r"C:\tools\cli.cmd"),
            &[OsString::from("line\nfeed")]
        )
        .is_err());
        assert!(batch_command_line(
            OsStr::new(r"C:\tools\cli.cmd"),
            &[OsString::from("carriage\rreturn")]
        )
        .is_err());
        let nul = OsString::from_wide(&[b'a' as u16, 0, b'b' as u16]);
        assert!(batch_command_line(OsStr::new(r"C:\tools\cli.cmd"), &[nul]).is_err());
        assert!(batch_command_line(OsStr::new(r"C:\tools\%TEMP%\cli.cmd"), &[]).is_err());
    }
}
