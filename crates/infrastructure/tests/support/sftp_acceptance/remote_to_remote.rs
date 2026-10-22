//! 双远端 SFTP 传输与两跳连接验收，不把同机 fixture 误当成同 endpoint 优化。
//!
//! @author kongweiguang

use std::fs;

use kerminal_domain::files::{
    ConflictPolicy, FileEndpoint, TransferKind, TransferStatus, WriteTextRequest, WriteTextStrategy,
};
use tokio_util::sync::CancellationToken;

use super::{
    AcceptanceResult, SftpAcceptanceFixture, remote_child, request, require, run_transfer,
};

/// 在不同 HostId 和独立随机目录间执行文件、目录复制，并从两端回读验证数据。
///
/// @author kongweiguang
pub(super) async fn exercise_remote_to_remote(fixture: &SftpAcceptanceFixture) -> AcceptanceResult {
    require(
        fixture.host_id != fixture.source_host_id,
        "dual-remote fixture reused one host identity",
    )?;

    let local_source = fixture.local_root.path().join("remote-copy-source.bin");
    let bytes = (0..96 * 1024)
        .map(|index| ((index * 47 + index / 113) % 256) as u8)
        .collect::<Vec<_>>();
    fs::write(&local_source, &bytes)?;
    let remote_source = remote_child(
        &fixture.source_host_id,
        &fixture.source_workspace,
        "source-文件.bin",
    )?;
    let uploaded = run_transfer(
        fixture,
        "remote_to_remote.upload_source_file",
        request(
            FileEndpoint::Local { path: local_source },
            remote_source.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        uploaded.status == TransferStatus::Succeeded,
        "dual-remote test source upload did not complete",
    )?;

    let remote_destination =
        remote_child(&fixture.host_id, &fixture.workspace, "destination-文件.bin")?;
    let source_host = remote_source
        .host_id()
        .ok_or_else(|| std::io::Error::other("dual-remote source lost its host identity"))?;
    let destination_host = remote_destination
        .host_id()
        .ok_or_else(|| std::io::Error::other("dual-remote destination lost its host identity"))?;
    require(
        source_host != destination_host,
        "cross-host transfer accidentally used one remote endpoint",
    )?;
    let copied = run_transfer(
        fixture,
        "remote_to_remote.copy_file",
        request(
            remote_source.clone(),
            remote_destination.clone(),
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        copied.status == TransferStatus::Succeeded
            && copied.commit_summary.committed_entries() == 1,
        "remote-to-remote file transfer did not confirm its destination",
    )?;
    for endpoint in [&remote_source, &remote_destination] {
        let preview = fixture
            .files
            .preview_file(CancellationToken::new(), endpoint.clone(), bytes.len())
            .await?;
        require(
            preview.bytes == bytes,
            "remote-to-remote file content differs at one endpoint",
        )?;
    }

    let source_tree = remote_child(
        &fixture.source_host_id,
        &fixture.source_workspace,
        "source-tree",
    )?;
    fixture
        .files
        .create_directory(CancellationToken::new(), source_tree.clone())
        .await?;
    let source_tree_path = endpoint_path(&source_tree)?;
    let source_nested = remote_child(&fixture.source_host_id, &source_tree_path, "nested")?;
    fixture
        .files
        .create_directory(CancellationToken::new(), source_nested.clone())
        .await?;
    write_remote_text(
        fixture,
        &fixture.source_host_id,
        &source_tree_path,
        "replace.txt",
        "from second remote",
    )
    .await?;
    write_remote_text(
        fixture,
        &fixture.source_host_id,
        &endpoint_path(&source_nested)?,
        "nested.txt",
        "nested from second remote",
    )
    .await?;

    let destination_tree = remote_child(&fixture.host_id, &fixture.workspace, "destination-tree")?;
    fixture
        .files
        .create_directory(CancellationToken::new(), destination_tree.clone())
        .await?;
    let destination_tree_path = endpoint_path(&destination_tree)?;
    write_remote_text(
        fixture,
        &fixture.host_id,
        &destination_tree_path,
        "preserve.txt",
        "target-only",
    )
    .await?;
    write_remote_text(
        fixture,
        &fixture.host_id,
        &destination_tree_path,
        "replace.txt",
        "old target value",
    )
    .await?;

    let merged = run_transfer(
        fixture,
        "remote_to_remote.merge_directory",
        request(
            source_tree,
            destination_tree.clone(),
            TransferKind::Directory,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    require(
        merged.status == TransferStatus::Succeeded
            && merged.commit_summary.committed_entries() >= 2,
        "remote-to-remote directory merge did not confirm copied entries",
    )?;
    let top = fixture
        .files
        .list_directory(CancellationToken::new(), destination_tree.clone())
        .await?;
    require(
        top.entries.iter().any(|entry| entry.name == "preserve.txt"),
        "remote-to-remote merge removed a destination-only file",
    )?;
    require(
        read_remote_text(
            fixture,
            &fixture.host_id,
            &destination_tree_path,
            "replace.txt",
        )
        .await?
            == "from second remote",
        "remote-to-remote merge did not replace the matching file",
    )?;
    let copied_nested = remote_child(&fixture.host_id, &destination_tree_path, "nested")?;
    require(
        read_remote_text(
            fixture,
            &fixture.host_id,
            &endpoint_path(&copied_nested)?,
            "nested.txt",
        )
        .await?
            == "nested from second remote",
        "remote-to-remote directory transfer lost a nested file",
    )?;
    Ok(())
}

/// 从 fixture 自有 endpoint 中提取 POSIX 路径，防止测试拼接未校验的远端路径。
///
/// @author kongweiguang
fn endpoint_path(
    endpoint: &FileEndpoint,
) -> Result<kerminal_domain::files::RemotePath, std::io::Error> {
    match endpoint {
        FileEndpoint::Remote { path, .. } => Ok(path.clone()),
        _ => Err(std::io::Error::other("expected a remote endpoint")),
    }
}

/// 在指定 HostId 与已隔离目录下创建文本文件，保持 remote source/target 分别可检查。
/// 写入回执只需确认成功，具体 revision 由后续回读断言验证。
///
/// @author kongweiguang
async fn write_remote_text(
    fixture: &SftpAcceptanceFixture,
    host_id: &kerminal_domain::identity::HostId,
    parent: &kerminal_domain::files::RemotePath,
    name: &str,
    content: &str,
) -> kerminal_application::AppResult<()> {
    fixture
        .files
        .write_text_file(
            CancellationToken::new(),
            WriteTextRequest {
                endpoint: remote_child(host_id, parent, name)?,
                content: content.to_owned(),
                strategy: WriteTextStrategy::CreateNew,
            },
        )
        .await?;
    Ok(())
}

/// 读取小型 UTF-8 远端文件并拒绝截断或二进制预览。
///
/// @author kongweiguang
async fn read_remote_text(
    fixture: &SftpAcceptanceFixture,
    host_id: &kerminal_domain::identity::HostId,
    parent: &kerminal_domain::files::RemotePath,
    name: &str,
) -> kerminal_application::AppResult<String> {
    let preview = fixture
        .files
        .preview_file(
            CancellationToken::new(),
            remote_child(host_id, parent, name)?,
            1024,
        )
        .await?;
    if preview.binary || preview.truncated {
        return Err(kerminal_application::AppError::invalid(
            "remote test text preview was binary or truncated",
        ));
    }
    String::from_utf8(preview.bytes)
        .map_err(|_| kerminal_application::AppError::invalid("remote test text was not UTF-8"))
}
