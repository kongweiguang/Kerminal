//! 虚拟文件清单对真实本机副本和受控远端目录的契约测试。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    files::{
        FileAccessProvider, FileService,
        drag_manifest::{build_drag_manifest, validate_prepared_drag_manifest},
        drag_out::DragOutSource,
    },
};
use kerminal_domain::{
    files::{
        DeleteTargetKind, DirectoryListing, FileEndpoint, FileEntry, FileEntryKind, FilePreview,
        TextDocument, TransferKind, WriteTextRequest, WriteTextResult,
    },
    identity::HostId,
};
use kerminal_infrastructure::files::{JsonFileBookmarkRepository, LocalFileProvider};
use tokio_util::sync::CancellationToken;

struct FixtureProvider {
    listings: HashMap<FileEndpoint, DirectoryListing>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl FileAccessProvider for FixtureProvider {
    /// 只接受本测试构造的远端端点，避免影响真实本机暂存校验。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Remote { .. })
    }

    /// 返回精确受控的目录响应，以验证异常条目和响应身份不会被默默忽略。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("fixture cancelled"));
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.listings
            .get(&endpoint)
            .cloned()
            .ok_or_else(|| AppError::not_found("fixture directory missing"))
    }

    /// 清单不应读取文件正文，误调用时立即失败而不是构造伪内容。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("fixture preview is unavailable"))
    }

    /// 文件正文不参与清单枚举，避免测试替身掩盖错误调用。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Err(AppError::unsupported("fixture read is unavailable"))
    }

    /// 清单是只读用例，测试替身拒绝所有写入。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("fixture write is unavailable"))
    }

    /// 清单无权新建源目录。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("fixture create is unavailable"))
    }

    /// 清单无权修改源条目名称。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("fixture rename is unavailable"))
    }

    /// 清单无权修改源文件权限。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("fixture chmod is unavailable"))
    }

    /// 清单无权删除源条目。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("fixture delete is unavailable"))
    }
}

/// 混合真实本机端口和确定的远端替身，书签根限定在测试临时目录。
///
/// @author kongweiguang
fn service(root: &Path, listings: Vec<(FileEndpoint, DirectoryListing)>) -> FileService {
    service_with_counter(root, listings, Arc::new(AtomicUsize::new(0)))
}

/// 测试可观察目录读取次数，以验证同一次多选复用已有远端响应。
///
/// @author kongweiguang
fn service_with_counter(
    root: &Path,
    listings: Vec<(FileEndpoint, DirectoryListing)>,
    calls: Arc<AtomicUsize>,
) -> FileService {
    FileService::new(
        vec![
            Arc::new(LocalFileProvider),
            Arc::new(FixtureProvider {
                listings: listings.into_iter().collect(),
                calls,
            }),
        ],
        Arc::new(JsonFileBookmarkRepository::new(root.join("config"))),
    )
    .expect("fixture providers have disjoint endpoints")
}

/// 同目录多选只读取一次父目录，但每个选项仍使用完整端点和名称核对。
///
/// @author kongweiguang
#[tokio::test]
async fn selected_roots_share_one_parent_directory_read() -> AppResult<()> {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut entries = Vec::new();
    let mut sources = Vec::new();
    for index in 0..100 {
        let name = format!("file-{index:03}.txt");
        let path = format!("/fixture/{name}");
        entries.push(entry(&path, &name, FileEntryKind::File, Some(3)));
        sources.push(source(remote(&path), &name, TransferKind::File));
    }
    let service = service_with_counter(
        temp.path(),
        vec![listing("/fixture", entries)],
        calls.clone(),
    );
    let manifest = build_drag_manifest(service, CancellationToken::new(), sources).await?;
    assert_eq!(manifest.entries.len(), 100);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    Ok(())
}

/// 固定逻辑主机身份，测试 endpoint 的完整路径匹配而不连接 SSH。
///
/// @author kongweiguang
fn remote(path: &str) -> FileEndpoint {
    FileEndpoint::remote(
        HostId::parse("manifest-test").expect("valid fixture host"),
        path,
    )
    .expect("valid remote path")
}

/// 构造只含清单关注字段的条目，其 endpoint 可被测试故意改成恶意结果。
///
/// @author kongweiguang
fn entry(path: &str, name: &str, kind: FileEntryKind, size: Option<u64>) -> FileEntry {
    FileEntry {
        endpoint: remote(path),
        name: name.to_owned(),
        kind,
        size,
        modified: None,
        permissions: None,
        is_hidden: false,
        raw: None,
    }
}

/// 目录身份独立于键，便于验证 provider 返回错误目录时被拒绝。
///
/// @author kongweiguang
fn listing(path: &str, entries: Vec<FileEntry>) -> (FileEndpoint, DirectoryListing) {
    let directory = remote(path);
    (directory.clone(), DirectoryListing { directory, entries })
}

/// 根选择的顺序就是文件描述符根索引，测试只使用明确的非临时连接。
///
/// @author kongweiguang
fn source(endpoint: FileEndpoint, name: &str, kind: TransferKind) -> DragOutSource {
    DragOutSource {
        endpoint,
        name: name.to_owned(),
        kind,
        external_target_lease: None,
    }
}

/// 文件、嵌套目录和空目录都必须保留，顺序独立于 provider 返回顺序。
///
/// @author kongweiguang
#[tokio::test]
async fn manifest_preserves_selected_roots_nested_files_and_empty_directories() -> AppResult<()> {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    let service = service(
        temp.path(),
        vec![
            listing(
                "/fixture",
                vec![
                    entry("/fixture/folder", "folder", FileEntryKind::Directory, None),
                    entry("/fixture/one.txt", "one.txt", FileEntryKind::File, Some(3)),
                ],
            ),
            listing(
                "/fixture/folder",
                vec![
                    entry(
                        "/fixture/folder/empty",
                        "empty",
                        FileEntryKind::Directory,
                        None,
                    ),
                    entry(
                        "/fixture/folder/z.txt",
                        "z.txt",
                        FileEntryKind::File,
                        Some(7),
                    ),
                    entry(
                        "/fixture/folder/A.txt",
                        "A.txt",
                        FileEntryKind::File,
                        Some(2),
                    ),
                ],
            ),
            listing("/fixture/folder/empty", vec![]),
        ],
    );
    let manifest = build_drag_manifest(
        service,
        CancellationToken::new(),
        vec![
            source(remote("/fixture/one.txt"), "one.txt", TransferKind::File),
            source(remote("/fixture/folder"), "folder", TransferKind::Directory),
        ],
    )
    .await?;
    let actual = manifest
        .entries
        .iter()
        .map(|item| {
            (
                item.relative_path.as_str(),
                item.root_index,
                item.kind,
                item.size,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            ("one.txt", 0, TransferKind::File, Some(3)),
            ("folder", 1, TransferKind::Directory, None),
            ("folder/A.txt", 1, TransferKind::File, Some(2)),
            ("folder/empty", 1, TransferKind::Directory, None),
            ("folder/z.txt", 1, TransferKind::File, Some(7)),
        ]
    );
    Ok(())
}

/// 实际暂存根需与原清单的目录结构、类型及已知大小一致，新增项目也不能被忽略。
///
/// @author kongweiguang
#[tokio::test]
async fn prepared_tree_is_checked_for_missing_extra_or_changed_entries() -> AppResult<()> {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    let source_root = temp.path().join("source").join("folder");
    let prepared_root = temp.path().join("prepared").join("folder");
    for root in [&source_root, &prepared_root] {
        std::fs::create_dir_all(root.join("empty")).expect("create empty fixture directory");
        std::fs::write(root.join("file.txt"), b"abc").expect("create fixture file");
    }
    let service = service(temp.path(), vec![]);
    let manifest = build_drag_manifest(
        service.clone(),
        CancellationToken::new(),
        vec![source(
            FileEndpoint::Local {
                path: source_root.clone(),
            },
            "folder",
            TransferKind::Directory,
        )],
    )
    .await?;
    let paths = vec![(prepared_root.clone(), true)];
    validate_prepared_drag_manifest(service.clone(), CancellationToken::new(), &manifest, &paths)
        .await?;

    std::fs::write(prepared_root.join("file.txt"), b"changed").expect("change size");
    assert!(
        validate_prepared_drag_manifest(
            service.clone(),
            CancellationToken::new(),
            &manifest,
            &paths,
        )
        .await
        .is_err()
    );
    std::fs::write(prepared_root.join("file.txt"), b"abc").expect("restore size");
    std::fs::remove_file(prepared_root.join("file.txt")).expect("remove file fixture");
    std::fs::create_dir(prepared_root.join("file.txt")).expect("replace file with directory");
    assert!(
        validate_prepared_drag_manifest(
            service.clone(),
            CancellationToken::new(),
            &manifest,
            &paths,
        )
        .await
        .is_err()
    );
    std::fs::remove_dir(prepared_root.join("file.txt")).expect("remove changed fixture type");
    std::fs::write(prepared_root.join("file.txt"), b"abc").expect("restore fixture file");
    std::fs::write(prepared_root.join("added.txt"), b"new").expect("add extra file");
    assert!(
        validate_prepared_drag_manifest(
            service.clone(),
            CancellationToken::new(),
            &manifest,
            &paths,
        )
        .await
        .is_err()
    );
    std::fs::remove_file(prepared_root.join("added.txt")).expect("remove extra test file");
    std::fs::remove_dir(prepared_root.join("empty")).expect("remove empty test directory");
    assert!(
        validate_prepared_drag_manifest(
            service.clone(),
            CancellationToken::new(),
            &manifest,
            &paths,
        )
        .await
        .is_err()
    );
    assert!(
        validate_prepared_drag_manifest(
            service,
            CancellationToken::new(),
            &manifest,
            &[(prepared_root, false)],
        )
        .await
        .is_err()
    );
    Ok(())
}

/// Windows 设备别名、ADS、尾随空格和无法表示的单层名称不能进入文件描述符。
///
/// @author kongweiguang
#[tokio::test]
async fn rejects_windows_reserved_and_unrepresentable_names() {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    for invalid in [
        "CON.txt",
        "CON .txt",
        "conin$",
        "CONOUT$",
        "CLOCK$.txt",
        "LPT¹.log",
        "COM²",
        "COM³.txt",
        "AUX.",
        "trailing ",
        "data:stream",
        "a?b",
        "x\u{0007}",
    ] {
        let service = service(
            temp.path(),
            vec![
                listing(
                    "/fixture",
                    vec![entry(
                        "/fixture/folder",
                        "folder",
                        FileEntryKind::Directory,
                        None,
                    )],
                ),
                listing(
                    "/fixture/folder",
                    vec![entry(
                        "/fixture/folder/placeholder",
                        invalid,
                        FileEntryKind::File,
                        Some(0),
                    )],
                ),
            ],
        );
        assert!(
            build_drag_manifest(
                service,
                CancellationToken::new(),
                vec![source(
                    remote("/fixture/folder"),
                    "folder",
                    TransferKind::Directory
                )],
            )
            .await
            .is_err(),
            "invalid name must fail: {invalid:?}"
        );
    }
}

/// 大小写冲突、特殊文件及伪造的直接子项都必须阻断整批，不能跳过坏项。
///
/// @author kongweiguang
#[tokio::test]
async fn rejects_collision_special_files_and_wrong_child_identity() {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    let cases = [
        vec![
            entry(
                "/fixture/folder/A.txt",
                "A.txt",
                FileEntryKind::File,
                Some(1),
            ),
            entry(
                "/fixture/folder/a.txt",
                "a.txt",
                FileEntryKind::File,
                Some(1),
            ),
        ],
        vec![
            entry("/fixture/folder/K", "K", FileEntryKind::File, Some(1)),
            entry("/fixture/folder/K", "K", FileEntryKind::File, Some(1)),
        ],
        vec![
            entry("/fixture/folder/Σ", "Σ", FileEntryKind::File, Some(1)),
            entry("/fixture/folder/ς", "ς", FileEntryKind::File, Some(1)),
        ],
        vec![entry(
            "/fixture/folder/link",
            "link",
            FileEntryKind::Symlink,
            None,
        )],
        vec![entry(
            "/fixture/folder/device",
            "device",
            FileEntryKind::Other,
            None,
        )],
        vec![entry(
            "/fixture/folder/other/file",
            "file",
            FileEntryKind::File,
            Some(1),
        )],
    ];
    for children in cases {
        let service = service(
            temp.path(),
            vec![
                listing(
                    "/fixture",
                    vec![entry(
                        "/fixture/folder",
                        "folder",
                        FileEntryKind::Directory,
                        None,
                    )],
                ),
                listing("/fixture/folder", children),
            ],
        );
        assert!(
            build_drag_manifest(
                service,
                CancellationToken::new(),
                vec![source(
                    remote("/fixture/folder"),
                    "folder",
                    TransferKind::Directory
                )],
            )
            .await
            .is_err()
        );
    }
}

/// 260 UTF-16 单元的描述符会覆盖 NUL；259 单元仍应允许以免错误收紧边界。
///
/// @author kongweiguang
#[tokio::test]
async fn checks_file_descriptor_utf16_path_limit() {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    for (length, is_valid) in [(252, true), (253, false)] {
        let name = "a".repeat(length);
        let path = format!("/fixture/folder/{name}");
        let service = service(
            temp.path(),
            vec![
                listing(
                    "/fixture",
                    vec![entry(
                        "/fixture/folder",
                        "folder",
                        FileEntryKind::Directory,
                        None,
                    )],
                ),
                listing(
                    "/fixture/folder",
                    vec![entry(&path, &name, FileEntryKind::File, Some(0))],
                ),
            ],
        );
        assert_eq!(
            build_drag_manifest(
                service,
                CancellationToken::new(),
                vec![source(
                    remote("/fixture/folder"),
                    "folder",
                    TransferKind::Directory
                )],
            )
            .await
            .is_ok(),
            is_valid
        );
    }
}

/// 目录深度和条目总数采用固定预算；达到上限可用，超过上限整批失败。
///
/// @author kongweiguang
#[tokio::test]
async fn directory_depth_and_entry_limits_are_enforced() {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    for (child_depth, is_valid) in [(63, true), (64, false)] {
        let mut listings = vec![listing(
            "/fixture",
            vec![entry(
                "/fixture/root",
                "root",
                FileEntryKind::Directory,
                None,
            )],
        )];
        let mut parent = "/fixture/root".to_owned();
        for _ in 0..child_depth {
            let child = format!("{parent}/d");
            listings.push(listing(
                &parent,
                vec![entry(&child, "d", FileEntryKind::Directory, None)],
            ));
            parent = child;
        }
        listings.push(listing(&parent, vec![]));
        let service = service(temp.path(), listings);
        assert_eq!(
            build_drag_manifest(
                service,
                CancellationToken::new(),
                vec![source(
                    remote("/fixture/root"),
                    "root",
                    TransferKind::Directory
                )],
            )
            .await
            .is_ok(),
            is_valid
        );
    }

    for (child_count, is_valid) in [(9_999, true), (10_000, false)] {
        let children = (0..child_count)
            .map(|index| {
                let name = format!("file-{index:05}");
                entry(
                    &format!("/fixture/root/{name}"),
                    &name,
                    FileEntryKind::File,
                    Some(0),
                )
            })
            .collect();
        let service = service(
            temp.path(),
            vec![
                listing(
                    "/fixture",
                    vec![entry(
                        "/fixture/root",
                        "root",
                        FileEntryKind::Directory,
                        None,
                    )],
                ),
                listing("/fixture/root", children),
            ],
        );
        assert_eq!(
            build_drag_manifest(
                service,
                CancellationToken::new(),
                vec![source(
                    remote("/fixture/root"),
                    "root",
                    TransferKind::Directory
                )],
            )
            .await
            .is_ok(),
            is_valid
        );
    }
}

/// 来源类型、目录回执身份和取消令牌属于清单冻结边界，不得被降级成空列表。
///
/// @author kongweiguang
#[tokio::test]
async fn rejects_stale_root_wrong_listing_and_cancelled_request() {
    let temp = tempfile::tempdir().expect("isolated fixture directory");
    let root_listing = listing(
        "/fixture",
        vec![entry(
            "/fixture/folder",
            "folder",
            FileEntryKind::Directory,
            None,
        )],
    );
    let wrong_listing = (
        remote("/fixture/folder"),
        DirectoryListing {
            directory: remote("/different"),
            entries: vec![],
        },
    );
    let service = service(temp.path(), vec![root_listing, wrong_listing]);
    assert!(
        build_drag_manifest(
            service.clone(),
            CancellationToken::new(),
            vec![source(
                remote("/fixture/folder"),
                "folder",
                TransferKind::File
            )],
        )
        .await
        .is_err()
    );
    assert!(
        build_drag_manifest(
            service.clone(),
            CancellationToken::new(),
            vec![source(
                remote("/fixture/folder"),
                "folder",
                TransferKind::Directory
            )],
        )
        .await
        .is_err()
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        build_drag_manifest(
            service,
            cancellation,
            vec![source(
                remote("/fixture/folder"),
                "folder",
                TransferKind::Directory
            )],
        )
        .await
        .is_err()
    );
}
