//! 文件用例与协议适配端口。
//!
//! 业务层只处理已保存主机引用和文件元数据；路径 IO、SFTP 通道与持久化由基础设施适配。
//!
//! @author kongweiguang

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FileEntry,
    FilePreview, TextDocument, WriteTextRequest, WriteTextResult, WriteTextStrategy,
};
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

pub mod clipboard_download;
pub mod editor;

/// 文本读取与编辑共享领域层预算，避免多个入口各自漂移出不同上限。
pub const MAX_TEXT_FILE_READ_BYTES: usize = kerminal_domain::files::editor::MAX_EDITOR_BYTES;

/// 预览独立限制内存占用；超过它的内容由适配器标记 truncated，不进入编辑器。
pub const MAX_FILE_PREVIEW_BYTES: usize = 8 * 1024 * 1024;

/// 文件适配器按目标选择协议实现；容器、SFTP 和本机文件系统共享同一用例契约。
#[async_trait]
pub trait FileAccessProvider: Send + Sync + 'static {
    /// 判断适配器能否操作此目标，避免在服务层按协议字符串散落分支。
    fn supports(&self, endpoint: &FileEndpoint) -> bool;

    /// 列出目录的一层内容；递归加载由调用方显式发起以限制网络和 UI 工作量。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing>;

    /// 以有界字节数读取预览，并返回二进制与截断信息供 UI 决定只读展示。
    async fn preview_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<FilePreview>;

    /// 读取可编辑文本及打开时 revision；实现必须拒绝超限或不可解码的编辑内容。
    async fn read_text_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<TextDocument>;

    /// 比较 revision 后写入新版本；覆盖冲突必须由用户请求明确授权。
    async fn write_text_file(
        &self,
        cancellation: CancellationToken,
        request: WriteTextRequest,
    ) -> AppResult<WriteTextResult>;

    /// 创建单层目录，已存在时返回明确冲突而不伪装成成功。
    async fn create_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<()>;

    /// 在同一文件系统内重命名；跨连接移动应由传输用例处理。
    async fn rename(
        &self,
        cancellation: CancellationToken,
        source: FileEndpoint,
        destination: FileEndpoint,
    ) -> AppResult<()>;

    /// 设置权限位；调用方在传入前必须排除协议不支持的高位。
    async fn chmod(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        mode: u32,
    ) -> AppResult<()>;

    /// 删除一个文件或目录；目录实现不得跟随符号链接递归进入树外路径。
    async fn delete(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        kind: DeleteTargetKind,
    ) -> AppResult<()>;
}

/// 书签存储单独隔离，便于使用原子文件事务保存用户路径而不污染文件操作适配器。
#[async_trait]
pub trait FileBookmarkRepository: Send + Sync + 'static {
    /// 返回指定目标的路径书签，加载失败必须可见而不能退化为空列表。
    async fn list(
        &self,
        cancellation: CancellationToken,
        target: BookmarkTarget,
    ) -> AppResult<Vec<FileBookmark>>;

    /// 设置收藏状态；相同状态重复提交必须幂等。
    async fn set(
        &self,
        cancellation: CancellationToken,
        bookmark: FileBookmark,
        bookmarked: bool,
    ) -> AppResult<Vec<FileBookmark>>;
}

/// 可克隆文件服务句柄只持有 application ports，UI 和 MCP 不直接依赖协议实现。
#[derive(Clone)]
pub struct FileService {
    providers: Arc<[Arc<dyn FileAccessProvider>]>,
    bookmarks: Arc<dyn FileBookmarkRepository>,
}

impl FileService {
    /// 组装确定的文件适配器集合；空集合被拒绝，避免界面出现永远失败的假能力。
    pub fn new(
        providers: Vec<Arc<dyn FileAccessProvider>>,
        bookmarks: Arc<dyn FileBookmarkRepository>,
    ) -> AppResult<Self> {
        if providers.is_empty() {
            return Err(AppError::unsupported("没有注册文件系统适配器"));
        }
        Ok(Self {
            providers: providers.into(),
            bookmarks,
        })
    }

    /// 从唯一匹配的适配器执行一次文件用例；重叠注册作为装配错误显式暴露。
    fn provider_for(&self, endpoint: &FileEndpoint) -> AppResult<Arc<dyn FileAccessProvider>> {
        let mut matched = self
            .providers
            .iter()
            .filter(|provider| provider.supports(endpoint));
        let provider = matched
            .next()
            .cloned()
            .ok_or_else(|| AppError::unsupported("当前文件目标没有可用适配器"))?;
        if matched.next().is_some() {
            return Err(AppError::infrastructure("文件目标匹配到多个适配器"));
        }
        Ok(provider)
    }

    /// 返回指定路径的一层目录内容，避免 UI 递归扫描大型远端目录。
    pub async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        ensure_not_cancelled(&cancellation)?;
        self.provider_for(&endpoint)?
            .list_directory(cancellation, endpoint)
            .await
    }

    /// 通过本机适配器已使用的 symlink_metadata 目录投影检查上传源类型，不让 UI 直接访问磁盘。
    ///
    /// @author kongweiguang
    pub async fn inspect_local_transfer_source(
        &self,
        cancellation: CancellationToken,
        path: PathBuf,
    ) -> AppResult<FileEntry> {
        ensure_not_cancelled(&cancellation)?;
        let endpoint = FileEndpoint::Local { path: path.clone() };
        let parent = path
            .parent()
            .ok_or_else(|| AppError::invalid("上传源必须是具体的本机目录项"))?;
        let parent_endpoint = FileEndpoint::Local {
            path: parent.to_path_buf(),
        };
        let provider = self.provider_for(&parent_endpoint)?;
        if !provider.supports(&endpoint) {
            return Err(AppError::unsupported("本机文件适配器不支持该上传源"));
        }
        let listing = provider
            .list_directory(cancellation, parent_endpoint)
            .await?;
        listing
            .entries
            .into_iter()
            .find(|entry| entry.endpoint == endpoint)
            .ok_or_else(|| AppError::not_found("本机上传源已不存在或无法枚举"))
    }

    /// 读取限长预览；调用方必须把用户配置的最大尺寸限制在服务端策略上限内。
    pub async fn preview_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<FilePreview> {
        ensure_not_cancelled(&cancellation)?;
        if max_bytes == 0 {
            return Err(AppError::invalid("预览字节上限必须大于零"));
        }
        self.provider_for(&endpoint)?
            .preview_file(
                cancellation,
                endpoint,
                max_bytes.min(MAX_FILE_PREVIEW_BYTES),
            )
            .await
    }

    /// 为编辑器读取文本 revision，超过限制时由适配器返回截断只读文档或拒绝编辑。
    pub async fn read_text_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<TextDocument> {
        ensure_not_cancelled(&cancellation)?;
        if max_bytes == 0 {
            return Err(AppError::invalid("文本读取字节上限必须大于零"));
        }
        self.provider_for(&endpoint)?
            .read_text_file(
                cancellation,
                endpoint,
                max_bytes.min(MAX_TEXT_FILE_READ_BYTES),
            )
            .await
    }

    /// 保存文本仅接收显式 revision、新建或用户授权覆盖策略，拒绝隐式读改写。
    pub async fn write_text_file(
        &self,
        cancellation: CancellationToken,
        request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        ensure_not_cancelled(&cancellation)?;
        validate_write_text_request(&request)?;
        self.provider_for(&request.endpoint)?
            .write_text_file(cancellation, request)
            .await
    }

    /// 创建目录并由适配器区分已存在、权限错误和真实创建结果。
    pub async fn create_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        self.provider_for(&endpoint)?
            .create_directory(cancellation, endpoint)
            .await
    }

    /// 重命名只通过同时支持两端点的同一适配器执行，跨主机操作不能退化成伪移动。
    pub async fn rename(
        &self,
        cancellation: CancellationToken,
        source: FileEndpoint,
        destination: FileEndpoint,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        if !source.same_filesystem(&destination) {
            return Err(AppError::unsupported("重命名源和目标必须位于同一文件系统"));
        }
        let provider = self.provider_for(&source)?;
        if !provider.supports(&destination) {
            return Err(AppError::unsupported("重命名目标必须位于同一文件系统"));
        }
        provider.rename(cancellation, source, destination).await
    }

    /// 限制权限位为 Unix 文件权限范围，避免把 UI 文本直接当成任意属性写入。
    pub async fn chmod(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        mode: u32,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        if mode > 0o7777 {
            return Err(AppError::invalid("文件权限超出支持范围"));
        }
        self.provider_for(&endpoint)?
            .chmod(cancellation, endpoint, mode)
            .await
    }

    /// 删除动作必须携带目标类型，适配器据此拒绝危险的递归/非递归混用。
    pub async fn delete(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        kind: DeleteTargetKind,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        self.provider_for(&endpoint)?
            .delete(cancellation, endpoint, kind)
            .await
    }

    /// 读取按目标隔离的书签；空目标不能与其他主机的路径收藏混合。
    pub async fn list_bookmarks(
        &self,
        cancellation: CancellationToken,
        target: BookmarkTarget,
    ) -> AppResult<Vec<FileBookmark>> {
        ensure_not_cancelled(&cancellation)?;
        self.bookmarks.list(cancellation, target).await
    }

    /// 书签设置是显式状态赋值，重复收藏或取消不会生成重复记录。
    pub async fn set_bookmark(
        &self,
        cancellation: CancellationToken,
        bookmark: FileBookmark,
        bookmarked: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        ensure_not_cancelled(&cancellation)?;
        self.bookmarks.set(cancellation, bookmark, bookmarked).await
    }
}

/// 在应用边界尽早拒绝已撤销请求，适配器随后负责其当前通道与提交阶段的安全收尾。
///
/// @author kongweiguang
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("文件操作已取消"))
    } else {
        Ok(())
    }
}

/// 集中校验文本写入意图，使 MCP、UI 与其它应用入口采用同一并发语义。
///
/// @author kongweiguang
fn validate_write_text_request(request: &WriteTextRequest) -> AppResult<()> {
    if request.content.len() > MAX_TEXT_FILE_READ_BYTES {
        return Err(AppError::invalid("文本内容超过 10 MiB 编辑上限"));
    }
    match &request.strategy {
        WriteTextStrategy::CompareRevision(revision)
            if revision.content_digest.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) =>
        {
            Err(AppError::invalid("文本 revision 摘要无效"))
        }
        _ => Ok(()),
    }
}
