//! 片段目录精确解析、分页与运行快照。
//!
//! @author kongweiguang

use kerminal_domain::{
    commands::{
        SnippetApplicabilityAssessment, SnippetCatalogItem, SnippetCatalogListRequest,
        SnippetCatalogOrigin, SnippetDefinition, SnippetPreference, SnippetPreferenceKey,
        SnippetScope, assess_snippet_applicability,
    },
    connections::RemoteShellDialect,
    preferences::ConfigRevision,
    sessions::{SessionLifecycle, SessionOwner},
};
use serde::{Deserialize, Serialize};

use super::{
    AppError, AppResult, CommandsService, SnippetPreferenceAvailability, validate_catalog_id,
};
use crate::sessions::{SessionFilter, UiSessionBinding};

const MAX_CATALOG_PAGE_SIZE: usize = 100;
const MAX_PREFERENCE_BATCH: usize = 500;

/// Commands 面板的三个目录视图；非空关键词由完整目录搜索覆盖选中视图。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetCatalogSection {
    Common,
    Mine,
    Library,
}

/// 请求一页片段目录；cursor 是上次结果的不透明 continuation，过滤条件变化时应回到第一页。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetCatalogPageRequest {
    pub section: SnippetCatalogSection,
    pub query: Option<String>,
    pub scope: Option<SnippetScope>,
    pub page_size: usize,
    pub cursor: Option<SnippetCatalogCursor>,
}

/// continuation 绑定完整目录、偏好快照及所有筛选参数，私有字段防止调用方拼造偏移。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetCatalogCursor {
    offset: usize,
    fingerprint: u64,
}

/// 一页目录保留准确过滤总数，`next_cursor` 为空代表已读到当前快照末尾。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetCatalogPage {
    pub items: Vec<SnippetCatalogItem>,
    pub total: usize,
    pub next_cursor: Option<SnippetCatalogCursor>,
}

/// 陈旧 continuation 是可恢复的领域结果，UI 可清空 cursor 后从第一页重读。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SnippetCatalogPageOutcome {
    Page(SnippetCatalogPage),
    StaleCursor,
}

/// 运行快照的 revision 区分可 CAS 的用户文件和只读内置资源内容。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum SnippetCatalogRevision {
    User(ConfigRevision),
    Builtin {
        catalog_version: Option<String>,
        content_fingerprint: u64,
    },
}

/// 执行前后比较精确来源、模板定义和 revision，避免重名目录项或文件外改写误用旧内容。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetExecutionCatalogSnapshot {
    pub key: SnippetPreferenceKey,
    pub definition: SnippetDefinition,
    /// 内置目录的适用性声明参与 revision；用户定义始终为空，不继承来源模板限制。
    pub platforms: Vec<String>,
    pub shells: Vec<String>,
    pub revision: SnippetCatalogRevision,
}

/// 目录详情的只读适用性投影，UI 可展示原声明而不从宿主操作系统或目标类型推断。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetApplicabilityProjection {
    pub platforms: Vec<String>,
    pub shells: Vec<String>,
    pub assessment: SnippetApplicabilityAssessment,
}

impl CommandsService {
    /// 直接按来源与 ID 解目录项，再附加单条偏好；详情读取不依赖列表截断或同名回退。
    pub async fn get_snippet_catalog_item(
        &self,
        key: SnippetPreferenceKey,
    ) -> AppResult<Option<SnippetCatalogItem>> {
        validate_catalog_id(&key.snippet_id)?;
        let Some(item) = self
            .snippets
            .get_catalog_item(key.origin, key.snippet_id)
            .await?
        else {
            return Ok(None);
        };
        let mut decorated = decorate_catalog_preferences(self, vec![item]).await?;
        Ok(decorated.pop())
    }

    /// 为执行侧提供精确领域定义；内置指纹同时覆盖可执行定义和适用性声明。
    pub async fn get_snippet_execution_snapshot(
        &self,
        key: SnippetPreferenceKey,
    ) -> AppResult<Option<SnippetExecutionCatalogSnapshot>> {
        validate_catalog_id(&key.snippet_id)?;
        match key.origin {
            SnippetCatalogOrigin::User => {
                let Some(versioned) = self
                    .snippets
                    .get_user_versioned(key.snippet_id.clone())
                    .await?
                else {
                    return Ok(None);
                };
                let revision = versioned
                    .revision
                    .ok_or_else(|| AppError::conflict("用户片段缺少 revision，无法安全执行"))?;
                Ok(Some(SnippetExecutionCatalogSnapshot {
                    key,
                    definition: versioned.definition,
                    platforms: Vec::new(),
                    shells: Vec::new(),
                    revision: SnippetCatalogRevision::User(revision),
                }))
            }
            SnippetCatalogOrigin::Builtin => {
                let Some(item) = self
                    .snippets
                    .get_catalog_item(key.origin, key.snippet_id.clone())
                    .await?
                else {
                    return Ok(None);
                };
                let definition = definition_from_catalog_item(&item);
                let content_fingerprint = stable_fingerprint(&format!(
                    "{definition:?}\nplatforms={:?}\nshells={:?}",
                    item.platforms, item.shells
                ));
                Ok(Some(SnippetExecutionCatalogSnapshot {
                    key,
                    definition,
                    platforms: item.platforms,
                    shells: item.shells,
                    revision: SnippetCatalogRevision::Builtin {
                        catalog_version: item.catalog_version,
                        content_fingerprint,
                    },
                }))
            }
        }
    }

    /// 为详情页返回目录原声明与当前可信会话方言的只读评估；过期 binding 只会得到 Unknown。
    pub async fn snippet_applicability(
        &self,
        key: SnippetPreferenceKey,
        binding: Option<UiSessionBinding>,
    ) -> AppResult<Option<SnippetApplicabilityProjection>> {
        let Some(snapshot) = self.get_snippet_execution_snapshot(key).await? else {
            return Ok(None);
        };
        let shell_dialect = match binding.as_ref() {
            Some(binding) if !snapshot.platforms.is_empty() || !snapshot.shells.is_empty() => {
                self.bound_shell_dialect(binding).await?
            }
            _ => None,
        };
        let assessment = assess_snippet_applicability(
            snapshot.key.origin,
            &snapshot.platforms,
            &snapshot.shells,
            shell_dialect,
        );
        Ok(Some(SnippetApplicabilityProjection {
            platforms: snapshot.platforms,
            shells: snapshot.shells,
            assessment,
        }))
    }

    /// 只读取与 binding 同代、同 owner 的 live dialect；没有可靠目标时不从 UI 或主机环境猜测。
    async fn bound_shell_dialect(
        &self,
        binding: &UiSessionBinding,
    ) -> AppResult<Option<RemoteShellDialect>> {
        if !matches!(&binding.owner, SessionOwner::Ui { .. }) {
            return Ok(None);
        }
        let Some(session) = self
            .sessions
            .list(SessionFilter::default())
            .await?
            .into_iter()
            .find(|session| session.session_id == binding.session_id)
        else {
            return Ok(None);
        };
        if session.lifecycle != SessionLifecycle::Running || session.owner != binding.owner {
            return Ok(None);
        }
        let context = self
            .sessions
            .terminal_command_context(binding.session_id.clone())
            .await?;
        if context.session_id != binding.session_id
            || context.owner != binding.owner
            || context.session_generation != binding.session_generation
        {
            return Ok(None);
        }
        Ok(Some(context.shell_dialect))
    }

    /// 每次读取完整目录快照后再筛选和切页，保证 total 正确且 continuation 能检测目录或偏好变化。
    pub async fn list_snippet_page(
        &self,
        request: SnippetCatalogPageRequest,
    ) -> AppResult<SnippetCatalogPageOutcome> {
        if request
            .query
            .as_ref()
            .is_some_and(|query| query.chars().count() > 256)
        {
            return Err(AppError::invalid("片段关键词超过长度限制"));
        }
        let page_size = request.page_size.clamp(1, MAX_CATALOG_PAGE_SIZE);
        let mut all_items = self
            .snippets
            .list(SnippetCatalogListRequest {
                limit: None,
                ..SnippetCatalogListRequest::default()
            })
            .await?;
        all_items = decorate_catalog_preferences(self, all_items).await?;
        Ok(build_catalog_page(all_items, &request, page_size))
    }
}

/// 从单次目录和偏好快照构造一页，隔离纯筛选规则以便验证 total 与 cursor 边界。
fn build_catalog_page(
    all_items: Vec<SnippetCatalogItem>,
    request: &SnippetCatalogPageRequest,
    page_size: usize,
) -> SnippetCatalogPageOutcome {
    let fingerprint = page_fingerprint(&all_items, request, page_size);
    let fallback = if request.section == SnippetCatalogSection::Common
        && request
            .query
            .as_deref()
            .is_none_or(|query| query.trim().is_empty())
    {
        all_items
            .iter()
            .filter(|item| {
                item.origin == SnippetCatalogOrigin::Builtin
                    && item.pack == "core"
                    && request
                        .scope
                        .is_none_or(|scope| item.scope == scope || item.scope == SnippetScope::Any)
            })
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let query = request
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase);
    let mut matching = select_catalog_items(all_items, request, query.as_deref());
    if request.section == SnippetCatalogSection::Common && query.is_none() {
        let mut common = matching
            .drain(..)
            .enumerate()
            .filter(|(_, item)| item.favorite || item.use_count > 0)
            .collect::<Vec<_>>();
        if common.is_empty() {
            // fallback 只使用内置 core 原顺序，不依赖用户片段或其它 pack 的偏好状态。
            matching = fallback;
        } else {
            common.sort_by(|(left_index, left), (right_index, right)| {
                right
                    .favorite
                    .cmp(&left.favorite)
                    .then_with(|| right.last_used_at_unix_ms.cmp(&left.last_used_at_unix_ms))
                    .then_with(|| right.use_count.cmp(&left.use_count))
                    .then_with(|| left_index.cmp(right_index))
            });
            matching = common.into_iter().map(|(_, item)| item).collect();
        }
    }
    let total = matching.len();
    let offset = match &request.cursor {
        Some(cursor) if cursor.fingerprint != fingerprint || cursor.offset > total => {
            return SnippetCatalogPageOutcome::StaleCursor;
        }
        Some(cursor) => cursor.offset,
        None => 0,
    };
    let end = offset.saturating_add(page_size).min(total);
    let items = matching
        .into_iter()
        .skip(offset)
        .take(end - offset)
        .collect();
    let next_cursor = (end < total).then_some(SnippetCatalogCursor {
        offset: end,
        fingerprint,
    });
    SnippetCatalogPageOutcome::Page(SnippetCatalogPage {
        items,
        total,
        next_cursor,
    })
}

/// 偏好仓储硬性接受 500 个 key；目录超出时分块访问，既不缩减结果也不缓存数据库快照。
pub(super) async fn decorate_catalog_preferences(
    service: &CommandsService,
    items: Vec<SnippetCatalogItem>,
) -> AppResult<Vec<SnippetCatalogItem>> {
    decorate_with_repository(service.snippet_preferences.as_deref(), items).await
}

/// 接收最小 preference port 以复用 500-key 分块逻辑，而不引入数据库缓存或额外状态。
async fn decorate_with_repository(
    repository: Option<&dyn super::SnippetPreferenceRepository>,
    mut items: Vec<SnippetCatalogItem>,
) -> AppResult<Vec<SnippetCatalogItem>> {
    let Some(repository) = repository else {
        return Ok(items);
    };
    if repository.availability() != SnippetPreferenceAvailability::Available {
        return Ok(items);
    }
    for chunk in items.chunks_mut(MAX_PREFERENCE_BATCH) {
        let keys = chunk
            .iter()
            .map(|item| SnippetPreferenceKey {
                origin: item.origin,
                snippet_id: item.id.clone(),
            })
            .collect();
        let preferences = repository.list(keys).await?;
        apply_preferences(chunk, preferences);
    }
    Ok(items)
}

/// 仅覆盖仓储明确返回的偏好行，未建行的目录项继续使用默认状态。
fn apply_preferences(items: &mut [SnippetCatalogItem], preferences: Vec<SnippetPreference>) {
    for item in items {
        let preference = preferences.iter().find(|preference| {
            preference.key.origin == item.origin && preference.key.snippet_id == item.id
        });
        if let Some(preference) = preference {
            item.favorite = preference.favorite;
            item.use_count = preference.use_count;
            item.last_used_at_unix_ms = preference.last_used_at_unix_ms;
        }
    }
}

/// 关键词始终跨目录搜索；空关键词时才依照选中的 Mine 或 Library 分类过滤。
fn select_catalog_items(
    items: Vec<SnippetCatalogItem>,
    request: &SnippetCatalogPageRequest,
    query: Option<&str>,
) -> Vec<SnippetCatalogItem> {
    items
        .into_iter()
        .filter(|item| {
            let scope_matches = request
                .scope
                .is_none_or(|scope| item.scope == scope || item.scope == SnippetScope::Any);
            let section_matches = query.is_some()
                || match request.section {
                    SnippetCatalogSection::Common => true,
                    SnippetCatalogSection::Mine => item.origin == SnippetCatalogOrigin::User,
                    SnippetCatalogSection::Library => item.origin == SnippetCatalogOrigin::Builtin,
                };
            scope_matches && section_matches && query.is_none_or(|query| matches_query(item, query))
        })
        .collect()
}

/// 目录搜索字段与用户可见条目保持一致，并接受稳定 ID 以便精确定位已知项。
fn matches_query(item: &SnippetCatalogItem, query: &str) -> bool {
    item.id.to_lowercase().contains(query)
        || item.title.to_lowercase().contains(query)
        || item.description.to_lowercase().contains(query)
        || item.template.to_lowercase().contains(query)
        || item.category.to_lowercase().contains(query)
        || item.pack.to_lowercase().contains(query)
        || item
            .tags
            .iter()
            .any(|tag| tag.to_lowercase().contains(query))
        || item
            .capabilities
            .iter()
            .any(|capability| capability.to_lowercase().contains(query))
}

/// continuation 的签名覆盖过滤条件和整个已装饰目录，目录或任一偏好改变都会触发陈旧结果。
fn page_fingerprint(
    items: &[SnippetCatalogItem],
    request: &SnippetCatalogPageRequest,
    page_size: usize,
) -> u64 {
    let mut hasher = FingerprintHasher::default();
    hasher.write(
        format!(
            "section={:?};query={:?};scope={:?};pageSize={page_size};",
            request.section, request.query, request.scope
        )
        .as_bytes(),
    );
    for item in items {
        hasher.write(format!("{item:?}\0").as_bytes());
    }
    hasher.finish()
}

/// 运行时 cursor 不落盘，使用固定 FNV-1a 使同一进程的过滤和快照签名可重复计算。
fn stable_fingerprint(value: &str) -> u64 {
    let mut hasher = FingerprintHasher::default();
    hasher.write(value.as_bytes());
    hasher.finish()
}

/// 小型无依赖指纹用于 opaque cursor 与静态 builtin revision，不参与凭据或安全校验。
#[derive(Debug)]
struct FingerprintHasher(u64);

impl Default for FingerprintHasher {
    /// 固定初始值，使 opaque continuation 在请求间可重复验证。
    fn default() -> Self {
        Self(0xcbf29ce484222325)
    }
}

impl FingerprintHasher {
    /// 按字节推进固定 FNV 状态，不将指纹用于认证或持久化安全决策。
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }

    /// 返回当前快照指纹供 cursor 比较。
    fn finish(&self) -> u64 {
        self.0
    }
}

/// 目录投影回转为执行定义时显式复制运行字段，展示统计和来源元数据不会进入命令模板。
fn definition_from_catalog_item(item: &SnippetCatalogItem) -> SnippetDefinition {
    SnippetDefinition {
        id: item.id.clone(),
        title: item.title.clone(),
        command: item.template.clone(),
        description: Some(item.description.clone()),
        tags: item.tags.clone(),
        sort_order: item.sort_order,
        scope: item.scope,
        category: Some(item.category.clone()),
        risk: item.risk,
        default_action: item.default_action,
        variables: item.variables.clone(),
        context_bindings: item.context_bindings.clone(),
        derived_from: item.derived_from.clone(),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/snippet_catalog_page.rs"]
mod tests;
