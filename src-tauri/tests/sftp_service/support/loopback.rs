//! SFTP 服务 loopback 测试支持。
//!
//! @author kongweiguang

use russh::{
    keys::{self, PrivateKey},
    server::{Auth, Msg, Server as _, Session},
    Channel, ChannelId, Pty,
};
use russh_sftp::protocol::{
    Attrs, Data, File as ProtocolFile, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use std::{
    collections::{HashMap, HashSet},
    io::{self, SeekFrom},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpListener,
    time::sleep,
};

#[derive(Debug)]
pub(crate) struct LoopbackSftpServer {
    pub(crate) addr: SocketAddr,
    pub(crate) auth_successes: Arc<AtomicUsize>,
    private_key: PrivateKey,
    task: tokio::task::JoinHandle<()>,
}

impl LoopbackSftpServer {
    pub(crate) fn clone_private_key_for_restart(&self) -> PrivateKey {
        self.private_key.clone()
    }
}

impl Drop for LoopbackSftpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
pub(crate) struct LoopbackSftpJumpServer {
    pub(crate) addr: SocketAddr,
    pub(crate) direct_tcpip_requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

pub(crate) const LOOPBACK_SFTP_SHELL_READY_MARKER: &str = "kerminal-loopback-sftp-shell-ready";

impl Drop for LoopbackSftpJumpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LoopbackSftpJumpServerState {
    direct_tcpip_requests: Arc<AtomicUsize>,
    target_addr: SocketAddr,
}

struct LoopbackSftpJumpSession {
    direct_tcpip_requests: Arc<AtomicUsize>,
    target_addr: SocketAddr,
}

impl russh::server::Server for LoopbackSftpJumpServerState {
    type Handler = LoopbackSftpJumpSession;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        LoopbackSftpJumpSession {
            direct_tcpip_requests: Arc::clone(&self.direct_tcpip_requests),
            target_addr: self.target_addr,
        }
    }
}

impl russh::server::Handler for LoopbackSftpJumpSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == "jump" && password == "jump-secret" {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    /// 仅接受已验证的 jump 目标，避免测试代理意外转发到未登记地址。
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if host_to_connect != self.target_addr.ip().to_string()
            || port_to_connect != u32::from(self.target_addr.port())
        {
            drop(reply);
            return Ok(());
        }

        self.direct_tcpip_requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let target_addr = self.target_addr;
        // 先接受再代理，避免 russh 0.62 将未确认通道按丢弃处理。
        reply.accept().await;
        tokio::spawn(async move {
            if let Ok(mut target_stream) = tokio::net::TcpStream::connect(target_addr).await {
                let mut channel_stream = channel.into_stream();
                let _ =
                    tokio::io::copy_bidirectional(&mut channel_stream, &mut target_stream).await;
            }
        });

        Ok(())
    }
}

#[derive(Clone)]
struct LoopbackSshServer {
    allow_shell: bool,
    rename_policy: LoopbackSftpRenamePolicy,
    auth_successes: Arc<AtomicUsize>,
    root: PathBuf,
    symlinks: Arc<HashMap<String, String>>,
}

struct LoopbackSshSession {
    allow_shell: bool,
    rename_policy: LoopbackSftpRenamePolicy,
    auth_successes: Arc<AtomicUsize>,
    root: PathBuf,
    symlinks: Arc<HashMap<String, String>>,
    channels: tokio::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>,
    exec_scripts: HashMap<ChannelId, Vec<u8>>,
}

impl russh::server::Server for LoopbackSshServer {
    type Handler = LoopbackSshSession;

    /// Carry the immutable fixture policy into each SSH connection so a test can exercise one
    /// consistent server capability without sharing per-channel file handles.
    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        LoopbackSshSession {
            allow_shell: self.allow_shell,
            rename_policy: self.rename_policy,
            auth_successes: Arc::clone(&self.auth_successes),
            root: self.root.clone(),
            symlinks: self.symlinks.clone(),
            channels: tokio::sync::Mutex::new(HashMap::new()),
            exec_scripts: HashMap::new(),
        }
    }
}

impl russh::server::Handler for LoopbackSshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == "deploy" && password == "secret" {
            self.auth_successes.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    /// 显式接受会话通道，使 loopback fixture 与 russh 0.62 的真实服务端握手语义一致。
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.allow_shell {
            session.channel_failure(channel)?;
            return Ok(());
        }
        session.channel_success(channel)?;
        session.data(
            channel,
            format!("{LOOPBACK_SFTP_SHELL_READY_MARKER}\r\n$ ").into_bytes(),
        )?;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(script) = self.exec_scripts.remove(&channel) {
            match execute_loopback_rm_rf_script(&self.root, &script).await {
                Ok(()) => {
                    session.exit_status_request(channel, 0)?;
                }
                Err(message) => {
                    session.extended_data(channel, 1, format!("{message}\n").into_bytes())?;
                    session.exit_status_request(channel, 1)?;
                }
            }
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
        session.close(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.allow_shell {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        if data != b"sh -s" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let _ = self.channels.lock().await.remove(&channel_id);
        self.exec_scripts.insert(channel_id, Vec::new());
        session.channel_success(channel_id)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(script) = self.exec_scripts.get_mut(&channel) {
            script.extend_from_slice(data);
        }
        Ok(())
    }

    /// Attach the configured rename policy to the real SFTP dispatcher used by this session.
    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        let channel = self.channels.lock().await.remove(&channel_id);
        let Some(channel) = channel else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };

        session.channel_success(channel_id)?;
        russh_sftp::server::run(
            channel.into_stream(),
            LoopbackSftpFs::with_policy(
                self.root.clone(),
                self.symlinks.clone(),
                self.rename_policy,
            ),
        )
        .await;
        Ok(())
    }
}

async fn execute_loopback_rm_rf_script(root: &Path, script: &[u8]) -> Result<(), String> {
    let script = String::from_utf8_lossy(script);
    let remote_path = parse_loopback_rm_rf_script(script.trim())
        .ok_or_else(|| "unsupported script".to_owned())?;
    let fs = LoopbackSftpFs::new(root.to_path_buf());
    let local_path = fs
        .resolve_path(&remote_path)
        .map_err(|status| format!("invalid path: {status:?}"))?;
    fs::remove_dir_all(local_path)
        .await
        .map_err(|error| format!("remove_dir_all failed: {error}"))
}

fn parse_loopback_rm_rf_script(script: &str) -> Option<String> {
    parse_shell_single_quoted_string(script.strip_prefix("rm -rf -- ")?)
}

fn parse_shell_single_quoted_string(value: &str) -> Option<String> {
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    if chars.next()? != '\'' {
        return None;
    }
    let mut in_quote = true;
    while let Some(ch) = chars.next() {
        if in_quote {
            if ch == '\'' {
                in_quote = false;
            } else {
                output.push(ch);
            }
            continue;
        }

        if ch == '\\' && chars.peek() == Some(&'\'') {
            let _ = chars.next();
            output.push('\'');
            continue;
        }
        if ch == '\'' {
            in_quote = true;
            continue;
        }
        return None;
    }

    (!in_quote).then_some(output)
}

struct LoopbackSftpFs {
    root: PathBuf,
    symlinks: Arc<HashMap<String, String>>,
    rename_policy: LoopbackSftpRenamePolicy,
    next_handle: u64,
    handles: HashMap<String, LoopbackSftpHandle>,
    rejected_first_writes: HashSet<String>,
}

/// Keep the no-overwrite case deterministic across Windows and Unix host filesystem semantics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LoopbackSftpRenamePolicy {
    #[default]
    ReplaceExisting,
    RejectExisting,
}

enum LoopbackSftpHandle {
    File {
        file: fs::File,
        path: String,
    },
    Directory {
        entries: Vec<ProtocolFile>,
        consumed: bool,
    },
}

impl LoopbackSftpFs {
    /// Retain existing replace-friendly behavior for fixtures that are not about commit fallback.
    fn new(root: PathBuf) -> Self {
        Self::with_policy(
            root,
            Arc::new(HashMap::new()),
            LoopbackSftpRenamePolicy::ReplaceExisting,
        )
    }

    /// Keep the rename policy on the filesystem handler so all SFTP sessions in one fixture see
    /// the same deterministic server behavior while per-connection handles remain isolated.
    fn with_policy(
        root: PathBuf,
        symlinks: Arc<HashMap<String, String>>,
        rename_policy: LoopbackSftpRenamePolicy,
    ) -> Self {
        Self {
            root,
            symlinks,
            rename_policy,
            next_handle: 0,
            handles: HashMap::new(),
            rejected_first_writes: HashSet::new(),
        }
    }

    fn next_handle(&mut self, prefix: &str) -> String {
        self.next_handle = self.next_handle.saturating_add(1);
        format!("{prefix}-{}", self.next_handle)
    }

    fn resolve_path(&self, remote_path: &str) -> Result<PathBuf, StatusCode> {
        let resolved = self
            .virtual_symlink_target(remote_path)
            .unwrap_or_else(|| remote_path.to_owned());
        self.resolve_physical_path(&resolved)
    }

    fn resolve_physical_path(&self, remote_path: &str) -> Result<PathBuf, StatusCode> {
        let mut local_path = self.root.clone();
        let normalized = remote_path.replace('\\', "/");
        for segment in normalized.split('/') {
            match segment {
                "" | "." => {}
                ".." => return Err(StatusCode::PermissionDenied),
                value => local_path.push(value),
            }
        }
        Ok(local_path)
    }

    /// Reproduce servers that omit Unix mode bits or return permission bits without a type tag.
    async fn attrs_for_path(&self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let local_path = self.resolve_path(&path)?;
        let metadata = fs::metadata(local_path).await.map_err(Self::io_status)?;
        Ok(Attrs {
            id,
            attrs: Self::file_attributes_for_path(&path, &metadata),
        })
    }

    /// SFTP v3 attributes may legally omit permissions; marker-specific fields keep the fixture
    /// deterministic while retaining real sizes and host metadata for all other paths.
    fn file_attributes_for_path(remote_path: &str, metadata: &std::fs::Metadata) -> FileAttributes {
        let mut attrs = FileAttributes::from(metadata);
        let name = remote_path.rsplit('/').next().unwrap_or(remote_path);
        if name.contains("omit-mode") {
            attrs.permissions = None;
        } else if name.contains("mode-only") && !name.ends_with(".kerminal-part") {
            // A mode without S_IFREG/S_IFDIR/S_IFLNK proves permissions, not the entry type.
            attrs.permissions = Some(0o644);
        }
        attrs
    }

    async fn symlink_attrs_for_path(&self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let Some(target) = self.virtual_symlink_target(&path) else {
            return self.attrs_for_path(id, path).await;
        };
        let mut attrs = FileAttributes::empty();
        attrs.size = Some(target.len() as u64);
        attrs.permissions = Some(0o120777);
        Ok(Attrs { id, attrs })
    }

    fn virtual_symlink_target(&self, remote_path: &str) -> Option<String> {
        let normalized = normalize_loopback_remote_path(remote_path);
        self.symlinks.get(&normalized).cloned()
    }

    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_owned(),
            language_tag: "en-US".to_owned(),
        }
    }

    fn io_status(error: io::Error) -> StatusCode {
        match error.kind() {
            io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        }
    }
}

impl russh_sftp::server::Handler for LoopbackSftpFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    /// 用独立路径模拟服务端拒绝创建，避免依赖 Windows 与 Unix 不同的权限位语义。
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        if filename.contains("reject-upload-open") {
            return Err(StatusCode::PermissionDenied);
        }
        let local_path = self.resolve_path(&filename)?;
        let options: std::fs::OpenOptions = pflags.into();
        let file = options.open(local_path).map_err(Self::io_status)?;
        let handle = self.next_handle("file");
        self.handles.insert(
            handle.clone(),
            LoopbackSftpHandle::File {
                file: fs::File::from_std(file),
                path: filename,
            },
        );
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        let Some(open_handle) = self.handles.remove(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        if let LoopbackSftpHandle::File { mut file, .. } = open_handle {
            file.flush().await.map_err(Self::io_status)?;
        }
        Ok(Self::ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(LoopbackSftpHandle::File { file, .. }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };

        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(Self::io_status)?;
        let mut data = vec![0; len as usize];
        let bytes = file.read(&mut data).await.map_err(Self::io_status)?;
        if bytes == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(bytes);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(LoopbackSftpHandle::File { file, path }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };

        if path.contains("reject-write") {
            return Err(StatusCode::Failure);
        }
        if path.contains("reject-first-write") && self.rejected_first_writes.insert(path.clone()) {
            return Err(StatusCode::Failure);
        }

        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(Self::io_status)?;
        file.write_all(&data).await.map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.symlink_attrs_for_path(id, path).await
    }

    /// Keep FSTAT and LSTAT/STAT on the same attribute profile so a missing type bit cannot be
    /// hidden by the upload's handle-based size verification.
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(open_handle) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        match open_handle {
            LoopbackSftpHandle::File { file, path } => {
                let metadata = file.metadata().await.map_err(Self::io_status)?;
                Ok(Attrs {
                    id,
                    attrs: Self::file_attributes_for_path(path, &metadata),
                })
            }
            LoopbackSftpHandle::Directory { .. } => Err(StatusCode::Failure),
        }
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(Self::ok(id))
    }

    /// 模拟允许 OPEN/WRITE 却禁止修改属性的服务端，捕获新上传对 FSETSTAT 的多余依赖。
    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        if matches!(self.handles.get(&handle), Some(LoopbackSftpHandle::File { path, .. }) if path.contains("reject-fsetstat"))
        {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(Self::ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        if self.virtual_symlink_target(&path).is_some() {
            return Err(StatusCode::Failure);
        }
        let local_path = self.resolve_path(&path)?;
        let mut entries = fs::read_dir(local_path).await.map_err(Self::io_status)?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(Self::io_status)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata().await.map_err(Self::io_status)?;
            files.push(ProtocolFile::new(name, FileAttributes::from(&metadata)));
        }
        files.sort_by(|left, right| left.filename.cmp(&right.filename));

        let handle = self.next_handle("dir");
        self.handles.insert(
            handle.clone(),
            LoopbackSftpHandle::Directory {
                entries: files,
                consumed: false,
            },
        );
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(LoopbackSftpHandle::Directory { entries, consumed }) =
            self.handles.get_mut(&handle)
        else {
            return Err(StatusCode::NoSuchFile);
        };
        if *consumed {
            return Err(StatusCode::Eof);
        }
        *consumed = true;
        Ok(Name {
            id,
            files: entries.clone(),
        })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let local_path = self.resolve_path(&filename)?;
        fs::remove_file(local_path).await.map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let local_path = self.resolve_path(&path)?;
        fs::create_dir(local_path).await.map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let local_path = self.resolve_path(&path)?;
        fs::remove_dir(local_path).await.map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    /// 将 STAT 拒绝与 OPEN 拒绝分开注入，保证上传诊断不会只保留相同的协议错误。
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        if path.contains("reject-upload-stat") {
            return Err(StatusCode::PermissionDenied);
        }
        self.attrs_for_path(id, path).await
    }

    /// Inject deterministic existing-target and commit/rollback refusals before filesystem
    /// mutation, while default fixtures continue to use the host filesystem's rename behavior.
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        if self.rename_policy != LoopbackSftpRenamePolicy::ReplaceExisting {
            let target = self.resolve_physical_path(&newpath)?;
            let target_exists = fs::symlink_metadata(&target).await.is_ok()
                || self.virtual_symlink_target(&newpath).is_some();
            if target_exists {
                return Err(StatusCode::Failure);
            }
        }

        let old_path = self.resolve_path(&oldpath)?;
        let new_path = self.resolve_path(&newpath)?;
        fs::rename(old_path, new_path)
            .await
            .map_err(Self::io_status)?;
        Ok(Self::ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        if let Some(target) = self.virtual_symlink_target(&path) {
            return Ok(Name {
                id,
                files: vec![ProtocolFile::dummy(target)],
            });
        }
        Ok(Name {
            id,
            files: vec![ProtocolFile::dummy(path)],
        })
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let Some(target) = self.virtual_symlink_target(&path) else {
            return Err(StatusCode::NoSuchFile);
        };
        Ok(Name {
            id,
            files: vec![ProtocolFile::dummy(target)],
        })
    }
}

#[path = "loopback/server.rs"]
mod server;
pub(crate) use server::*;

fn normalize_loopback_remote_path(path: &str) -> String {
    let mut normalized = path.trim().replace('\\', "/");
    if normalized.is_empty() {
        return "/".to_owned();
    }
    if !normalized.starts_with('/') {
        normalized = format!("/{normalized}");
    }
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}
