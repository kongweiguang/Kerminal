//! 可中断下载只写私有暂存文件；最终信任仍由签名目录的大小和摘要决定。
//!
//! 网络边界由同目录的 `network.rs` 承担：请求前完成 URL 授权、DNS 全量分类和
//! 地址固定，逐跳接管重定向，并把 DNS、请求与 body 绑定到同一个绝对截止时间。
//! 本模块仍是 `pub(super)` 私有入口，只接收已通过签名目录校验的资产，不对外
//! 暴露任意 URL 下载能力。传输完成不等于完整性确认：size 与 SHA-256 仍由
//! 安装器在解包前最终核验。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError, AppResult,
    extensions::{CatalogAsset, InstallProgress},
};
use reqwest::header::{CONTENT_RANGE, ETAG, HeaderMap, HeaderValue, IF_RANGE, RANGE};
use std::{future::Future, io::SeekFrom, path::Path, time::Duration};
use tokio::{
    fs::OpenOptions,
    io::{AsyncSeekExt, AsyncWriteExt},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// 以私有子模块嵌入现有网络边界，避免复制出第二套 URL/DNS/重定向策略。
///
/// 相对路径以本文件所在目录为基准，指向同目录的 `network.rs`；该文件由安装器
/// 统一维护，这里只做包含，不修改其内容。`allow(dead_code)` 只吸收该模块中仅供
/// 自身测试使用的条目在非测试构建里的未使用告警；本任务不允许改动 `network.rs`，
/// 只能在包含处抑制这条非行为性告警。
#[allow(dead_code)]
#[path = "network.rs"]
mod network;

use network::{AssetNetworkError, AssetNetworkSession};

/// 重试共享同一个绝对截止时间；旧实现按单次请求计时，最坏会拉长到四次之和。
///
/// 这里取旧单次上限的四倍作为整体预算：正常包仍有充足时间，最坏等待与旧实现
/// 同一量级，同时避免把一次已经耗尽的截止时间重新计时后继续请求。
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(480);

/// 保留旧实现的四次尝试上限；策略拒绝、取消和超时不进入可重试分支。
const MAX_ATTEMPTS: usize = 4;

/// 安装链路统一入口；绝对截止时间在这里一次性固定，测试通过 `download_until`
/// 注入更短时限来验证超时路径，避免真实等待整个下载预算。
pub(super) async fn download(
    asset: &CatalogAsset,
    path: &Path,
    _allow_loopback: bool,
    progress: &tokio::sync::watch::Sender<InstallProgress>,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    download_until(
        asset,
        path,
        progress,
        cancellation,
        Instant::now() + DOWNLOAD_DEADLINE,
    )
    .await
}

/// 带 deadline 的实际下载实现；测试用例通过它注入短时限验证超时路径。
///
/// `_allow_loopback` 只保留旧调用点的参数位置：资产已由签名目录授权，局域网或
/// 回环初始源必须继续可用，不能再拿旧白名单在这里二次拒绝。真正的边界由
/// `AssetNetworkSession` 在请求前统一判定，重定向仍逐跳复核。
pub(super) async fn download_until(
    asset: &CatalogAsset,
    path: &Path,
    progress: &tokio::sync::watch::Sender<InstallProgress>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> AppResult<()> {
    // 已取消的下载连地址校验和暂存检查都不应继续，避免取消被随后的策略错误掩盖。
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("插件下载已取消"));
    }
    // 先复用网络模块的纯语法授权，让“地址不合法”与暂存状态无关：即使暂存恰好
    // 已完整、无需再发请求，也不能把无效资产地址当成下载成功。
    network::validate_asset_url(&asset.url).map_err(AssetNetworkError::into_app_error)?;
    // 用 symlink_metadata 显式拒绝链接与 Windows 重解析点：下载器不跟随链接写
    // 暂存，防止暂存路径被替换成指向其他位置的入口。该检查只决定初始续传长度，
    // 后续所有写入都通过 open_verified_staging 的 no-follow 打开 + 句柄级复核
    // 进行，按名检查与按名打开之间的替换窗口由打开后的句柄证据兜底。
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if is_reparse_or_symlink(&metadata) => {
            return Err(AppError::invalid("插件下载暂存路径不安全"));
        }
        Ok(metadata) if metadata.is_file() => Some(metadata),
        Ok(_) => return Err(AppError::invalid("插件下载暂存路径不安全")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(AppError::infrastructure_with_source(
                "检查插件下载暂存失败",
                e,
            ));
        }
    };
    // 重试与 200 回退都会改写 offset；入口长度先按 symlink_metadata 记录，
    // 打开时必须区分“入口原本不存在”与“原本存在后消失”：前者允许独占新建
    // 常规暂存文件（首次安装的普通场景），后者只能失败，绝不重建。
    let entry_staging_exists = metadata.is_some();
    let mut offset = metadata.as_ref().map_or(0, std::fs::Metadata::len);
    // 打开时用于复核句柄长度的期望值：入口确认过的长度在本次会话内唯一可信；
    // 200 回退会把 offset 归零重写，但文件在当前时刻仍是入口长度，因此这里
    // 先固定保存，避免拿归零后的 offset 误判既有文件“被替换”。
    let entry_offset = offset;
    if offset > asset.size {
        return Err(AppError::invalid("插件暂存超过目录大小"));
    }
    progress.send_replace(InstallProgress::Downloading {
        received: offset,
        total: asset.size,
    });
    if offset == asset.size {
        // 入口发现的完整暂存属于上一次会话的遗留成果。此路径不发任何网络请求；
        // 传输完成同样不代表完整性确认，size 与 SHA-256 由安装器最终核验。
        // 磁盘 sync 由阻塞线程池执行、本身不可取消：取消与截止时间只能在调用
        // 前拦截、在返回后复核；wall-clock 不保证有界，也不会中途丢弃仍在运行
        // 的 OS 线程 IO——sync 自身会完整跑完，结果在复核点之后才被接受或拒绝。
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件下载已取消"));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("插件下载"));
        }
        // sync 等待期间 token 可能已取消、绝对截止时间可能已到期；磁盘调用不
        // 可取消，因此完成后统一复核再声明成功，避免借捷径吞掉取消/超时。
        await_sync_then_verify(
            sync_verified_complete(path, asset.size),
            cancellation,
            deadline,
        )
        .await?;
        return Ok(());
    }
    // 同一 session 在全部重试之间复用 pin：DNS rebinding 不能靠重新解析绕开，
    // 重定向累积的固定地址也随本次下载一起结束。
    let mut session = AssetNetworkSession::system();
    // 暂存句柄延迟到首次真正写入前才打开：策略拒绝、响应无效、取消、超限都在
    // 打开之前返回，失败的下载不会留下新建的空暂存文件（生产语义里“已下载
    // 一半”的遗留物只能来自真实写入）。一旦打开，同一句柄跨越所有重试与收尾
    // sync 持有，绝不在检查之后重新按名打开：按名打开正是替换竞态的入口
    // （检查到打开之间换文件、换链接），no-follow 打开 + 打开后立即复核
    // 类型/长度 + 独占共享模式，把“先检查后截断”收敛成对同一句柄的判定。
    let mut staging: Option<tokio::fs::File> = None;
    // ETag 值类型显式锚定为 HeaderValue：None 初始化无法从后续 insert(If-Range, ...)
    // 与比较处推断出元素类型，E0282 要求在声明处给出。
    let mut strong_etag: Option<HeaderValue> = None;
    let mut last_error = None;
    for _ in 0..MAX_ATTEMPTS {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件下载已取消"));
        }
        progress.send_replace(InstallProgress::Downloading {
            received: offset,
            total: asset.size,
        });
        let mut headers = HeaderMap::new();
        // 只有同次下载内确认的强 ETag 才允许发 Range 续传。服务器对不带
        // If-Range 的 Range 请求返回 206 完全合法，但旧暂存可能是另一个版本的
        // 同长度前缀，直接拼接会混合两个版本；没有可验证的强 ETag 时整包重取，
        // 旧暂存在收到 200 并确认重写之前保持原样，取消或失败都不会提前销毁它。
        let mut resume_requested = false;
        if let Some(etag) = strong_etag.as_ref().filter(|_| offset > 0) {
            headers.insert(
                RANGE,
                HeaderValue::from_str(&format!("bytes={offset}-"))
                    .map_err(|_| AppError::invalid("插件续传范围无效"))?,
            );
            headers.insert(IF_RANGE, etag.clone());
            resume_requested = true;
        }
        // 请求、重定向、body 与每次重试共用同一个绝对截止时间。这里刻意不再给
        // 单次尝试单独计时：否则一次慢但仍在推进的下载会在尝试上限处被截断，
        // 而若把它当普通传输错误重试，又会掩盖“超时必须上抛”的契约。
        let mut response = match session
            .get(&asset.url, headers, deadline, cancellation)
            .await
        {
            Ok(response) => response,
            // 只有连接/请求阶段的明确传输错误才重试；策略拒绝、取消和已耗尽的
            // 截止时间必须原样上抛，不能被包装成普通重试。
            Err(error) if retryable_transport(&error) => {
                last_error = Some(error.into_app_error());
                continue;
            }
            Err(error) => return Err(error.into_app_error()),
        };
        let response_etag = match classify_etag(response.headers()) {
            EtagPresence::Strong(value) => Some(value.clone()),
            // 首次 200 携带弱值、畸形或多值 ETag 时不能当作续传锚点：把这类值
            // 发进 If-Range 等于让服务器按无效条件判定版本。按原有无强 ETag
            // 路径整包重取即可，不需要为此失败。
            _ => None,
        };
        let mut response_end = asset.size;
        let append = if offset == 0 {
            if response.status() != reqwest::StatusCode::OK {
                return Err(AppError::invalid("插件下载响应无效"));
            }
            false
        } else if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            // 206 只在本轮确实发出了 Range+If-Range 时才有可核对的前缀语义；
            // 整包重取请求收到 206 说明服务器行为与请求不符，不能把剩余区间
            // 拼到一个没有验证过的旧前缀后面。
            if !resume_requested {
                return Err(AppError::invalid("插件续传响应无效"));
            }
            let expected_prefix = format!("bytes {offset}-");
            let range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| AppError::invalid("插件续传范围缺失"))?;
            let tail = range
                .strip_prefix(&expected_prefix)
                .and_then(|value| value.split_once('/'))
                .ok_or_else(|| AppError::invalid("插件续传范围无效"))?;
            let end = tail
                .0
                .parse::<u64>()
                .map_err(|_| AppError::invalid("插件续传范围无效"))?;
            let total = tail
                .1
                .parse::<u64>()
                .map_err(|_| AppError::invalid("插件续传范围无效"))?;
            if total != asset.size || end < offset || end >= total {
                return Err(AppError::invalid("插件续传范围不匹配"));
            }
            // 已发送 Range+If-Range 的合法 206 不必回显 ETag：缺失不当作不匹配，
            // 这里的范围校验与安装器最终 SHA/size 门禁继续兜底。但只要给出了
            // ETag 头，就必须是单个合法强值且与续传锚点一致——弱值、畸形、
            // 多值或不匹配都说明响应状态可疑，保守拒绝，不把"有头但解析失败"
            // 当缺失放行。分类复用与 200 锚点同一个判定，避免两处标准漂移。
            match classify_etag(response.headers()) {
                EtagPresence::Absent => {}
                EtagPresence::Strong(value) => {
                    if Some(value) != strong_etag.as_ref() {
                        return Err(AppError::invalid("插件续传版本标识不匹配"));
                    }
                }
                EtagPresence::Suspicious => {
                    return Err(AppError::invalid("插件续传版本标识不匹配"));
                }
            }
            response_end = end + 1;
            true
        } else if response.status() == reqwest::StatusCode::OK {
            // 续传请求被服务器以整包 200 回应（版本已变或临时不支持 Range）：
            // 放弃旧前缀，从零重写，避免把未经验证的旧字节留在暂存里。
            offset = 0;
            false
        } else {
            return Err(AppError::invalid("插件续传响应无效"));
        };
        if !append {
            strong_etag = response_etag;
        }
        // 整包响应先按声明长度拒绝超限，避免把明显过大的 body 读进暂存；206 的
        // 长度只是剩余区间，不能拿来和目录总大小比较，因此只检查从零写入的分支。
        if !append
            && response
                .content_length()
                .is_some_and(|length| length > asset.size)
        {
            return Err(AppError::invalid("插件下载超过目录大小"));
        }
        // 已通过响应判定、长度门禁才需要写句柄；此时才按入口状态决定创建
        // 语义。入口原本不存在（首次市场安装的 pkg.kplugin 尚未创建）唯一
        // 允许独占 create_new：并发出现的对象会让创建失败，绝不覆盖未知内容。
        // 入口原本存在则必须原样打开：检查后被删除或替换都明确失败，不重建，
        // 重建会把下载成果重新绑定到可疑路径。创建本身是有副作用操作，打开
        // 前后都复核取消/截止：等待期间到达的取消或超时不再继续后续截断。
        if staging.is_none() {
            let create = if entry_staging_exists {
                StagingCreate::Existing
            } else {
                StagingCreate::Fresh
            };
            // 入口确认过的长度在会话内唯一可信；Fresh 新建文件必为 0。200
            // 回退会把 offset 归零，因此不能用当时的 offset 复核既有文件。
            let expected = if matches!(create, StagingCreate::Fresh) {
                0
            } else {
                entry_offset
            };
            staging = Some(
                guard_staging_side_effect(
                    cancellation,
                    deadline,
                    open_verified_staging(path, expected, create),
                )
                .await?,
            );
        }
        let mut file = staging.take().expect("暂存在上一行已准备为 Some");
        // 只写已验证句柄，绝不重新按名打开：截断动作发生在句柄复核之后
        // （200 回退）或打开时已被长度复核约束（206 续传），检查与写入之间
        // 不存在可替换窗口。从零重写先截断到 0 再 seek 到写入起点，保持长度
        // 与位置语义一致；tokio File 的 set_len 不改变句柄。错误路径提前返回
        // 时句柄被丢弃（关闭），由调用方决定清理暂存，不留半打开状态。
        // 截断同样是有副作用操作，启动前与返回后都复核取消/截止：收到取消后
        // 不得再销毁既有前缀；已经执行完的 OS 截断不能原子撤回，只能如实报告。
        if !append {
            guard_staging_side_effect(cancellation, deadline, async {
                file.set_len(0)
                    .await
                    .map_err(|e| AppError::infrastructure_with_source("重置插件下载暂存失败", e))
            })
            .await?;
        }
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| AppError::infrastructure_with_source("定位插件下载暂存失败", e))?;
        let mut interrupted = false;
        // 写循环的取消/截止与在途写边界（依据锁定 Tokio 1.53.1
        // `fs/file.rs` 实现）：
        // - `write_all` 在把缓冲交给阻塞线程池后即返回（poll_write 先
        //   spawn_mandatory_blocking 再置 State::Busy），返回不证明 OS 写已
        //   结束；句柄 drop 也不等待在途任务——阻塞任务持有 `Arc<std::fs::File>`
        //   在后台静默完成且写错误丢失。因此任何提前返回前都必须先 `flush()`
        //   排空在途写，否则把“写盘成败未知”的状态遗留给调用方。
        // - `flush()` 只等待在途操作排空，不做每块 fsync，开销可接受；在途写
        //   失败的错误经 flush 以 io::Error 呈现（或已在 write_all 返回时报出），
        //   排空不会静默吞错。
        // - 所有提前返回经 [`drain_inflight`]：它先无条件等待在途写完成（排空
        //   不是新副作用，取消/过期也不能跳过），写盘错误优先上浮，再复核
        //   取消/截止。只有真正的新副作用（创建、截断、下一次 write_all）才
        //   先预检再执行：本循环中该预检就是每次写入前的 drain 调用——它同时
        //   完成“排空上一块 + 复核是否允许新写入”两件事。
        loop {
            let chunk = match response.chunk().await {
                Ok(chunk) => chunk,
                // 单次 body 传输错误才允许续传；超时、取消和策略错误在这里终止，
                // 不能落到下一次尝试里被当成普通网络抖动。排空在途写后返回，
                // 且写盘错误优先于传输错误：吞掉真实 io 错误会把坏盘伪成网络抖动。
                Err(error) if retryable_transport(&error) => {
                    drain_inflight(&mut file, cancellation, deadline).await?;
                    last_error = Some(error.into_app_error());
                    interrupted = true;
                    break;
                }
                // 不可重试的传输错误同样先排空在途写，再原样上抛网络错误；
                // 写盘失败已在上一行以真实 io 错误优先上抛。
                Err(error) => {
                    drain_inflight(&mut file, cancellation, deadline).await?;
                    return Err(error.into_app_error());
                }
            };
            let Some(chunk) = chunk else {
                // 正常 EOF 同样可能带着最后一次 write_all 的在途任务返回：
                // 服务器端早于声明长度关闭连接时，EOF 不保证已写满。出口先
                // 排空，使后续的长度不匹配判定与返回都建立在落盘可知之上。
                drain_inflight(&mut file, cancellation, deadline).await?;
                break;
            };
            if chunk.len() as u64 > response_end.saturating_sub(offset) {
                // 超限拒绝前同样排空在途写：拒绝的是本 chunk，已收字节必须
                // 落盘成败可知，不能带着在途任务返回。
                drain_inflight(&mut file, cancellation, deadline).await?;
                return Err(AppError::invalid("插件下载超过目录大小"));
            }
            // chunk 已到手但尚未写入：启动新副作用前的预检点。取消或已过期
            // 的预算不得再启动写入；在途写属于已启动副作用，先 flush 等待其
            // 真实完成（不丢弃、不伪装回收），失败则以写盘错误优先上抛，
            // 再按取消/截止报告——顺序与收尾 sync 的“结果优先、复核在后”
            // 契约一致，避免取消把真实坏盘伪造成“用户取消成功”。
            drain_inflight(&mut file, cancellation, deadline).await?;
            file.write_all(&chunk)
                .await
                .map_err(|e| AppError::infrastructure_with_source("写入插件下载失败", e))?;
            offset += chunk.len() as u64;
            progress.send_replace(InstallProgress::Downloading {
                received: offset,
                total: asset.size,
            });
        }
        if interrupted {
            if offset == asset.size {
                // body 以传输错误收尾时，即使已收满声明字节也不能声明完成：缺少
                // 干净 EOF 说明帧结构可能被截断或篡改，这里按真实传输错误上抛。
                // 暂存文件在本函数层面原样保留；但生产安装入口对下载失败会清理
                // 暂存，且清理结果未知时也不伪称成功，所以“失败后字节可复用”只
                // 是本函数对调用方的最小承诺，不能当成安装链路的普遍保证。
                return Err(last_error
                    .take()
                    .unwrap_or_else(|| AppError::infrastructure("插件下载中断，重试次数已用尽")));
            }
            // 未收满的中断才允许进入下一轮，按强 ETag 决定续传或整包重取。
        } else if offset != response_end {
            // 长度不匹配拒绝：EOF 出口已排空在途写并复核取消/截止，这里返回
            // 时句柄上没有未知状态。
            return Err(AppError::invalid("插件下载响应长度不匹配"));
        }
        progress.send_replace(InstallProgress::Downloading {
            received: offset,
            total: asset.size,
        });
        if offset == asset.size {
            // 正常下载的收尾 sync 同样由阻塞线程池执行、不可取消：等待期间到达
            // 的取消或已到期的截止时间必须复核，否则最后一次网络尝试会借 sync
            // 吞掉用户已经发出的取消。sync 仍走同一个已验证句柄，不再按名打开。
            await_sync_then_verify(
                async {
                    file.sync_all()
                        .await
                        .map_err(|e| AppError::infrastructure_with_source("同步插件下载失败", e))
                },
                cancellation,
                deadline,
            )
            .await?;
            return Ok(());
        }
        // 本轮未写完：把句柄放回槽位，下一轮续传或重试继续用同一句柄。EOF
        // 与传输错误出口都已排空：下一轮即使连接/头校验立即失败，也不会带着
        // 在途写丢弃句柄。
        staging = Some(file);
    }
    // 重试耗尽必须返回真实失败；保留最后一次可重试错误的原因，便于内部诊断，
    // 其 URL 已由网络边界脱敏，不会进入界面或日志。
    Err(last_error.unwrap_or_else(|| AppError::infrastructure("插件下载中断，重试次数已用尽")))
}

/// sync 返回后的取消/截止复核；磁盘调用不可取消时的唯一把关点。
///
/// 拦截只能在调用前、复核只能在返回后：sync 是阻塞线程池上的 OS 线程 IO，
/// 不能在等待中途被丢弃或伪装成已回收，wall-clock 因此不保证有界。函数单独
/// 提取为 `pub(super)`，让测试用已取消/已过期的确定参数直接判别这条把关逻辑，
/// 不依赖不可靠的真实竞态时序。
pub(super) fn verify_still_live(
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
) -> AppResult<()> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("插件下载已取消"));
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(AppError::timeout("插件下载"));
    }
    Ok(())
}

/// 等待给定落盘 future 完成后复核取消与截止时间；两条生产收尾共用此把关。
///
/// 磁盘调用不可取消：这里不丢弃仍在运行的 future，而是继续等待到它给出结果
/// （成功或失败都原样透传），随后统一复核取消/截止，等待期间到达的取消或已
/// 到期的截止时间因此不会借收尾 sync 被吞成成功。wall-clock 不受磁盘调用约束，
/// 截止只能在返回后报告。把后置复核集中在一个被两条生产路径实际调用的函数里，
/// 测试才能通过注入受控 future 在确定时序下判别“sync 挂起期间取消/过期”。
/// 排空在途写走 [`drain_inflight`]，它复用本函数的“结果优先、复核在后”契约。
pub(super) async fn await_sync_then_verify(
    sync: impl Future<Output = AppResult<()>>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> AppResult<()> {
    sync.await?;
    verify_still_live(cancellation, deadline)
}

/// 排空在途写统一入口：先无条件等待 flush 完成/失败，再复核取消/截止。
///
/// 排空（flush）不是新的磁盘写副作用，只是等待已启动 IO 完成，因此**不做
/// 副作用预检**：即使令牌已取消或预算已过期，也必须先等 flush 给出结果并
/// 处理其真实错误，再按取消/截止报告——否则预检会跳过 flush，把上一块已
/// 启动的写重新遗留在后台（drop 句柄后阻塞任务静默完成、错误丢失）。写盘
/// 错误优先于取消/截止上浮：坏盘不能被伪报成“用户取消”或“超时”。
/// 实现就是 `await_sync_then_verify`（结果优先、复核在后），直接复用它保证
/// 生产路径与已验证的收尾契约完全同源，不另建平行实现；写循环各提前返回
/// （可重试/不可重试传输错误、超限、EOF 出口、每 chunk 写入前）都经此入口。
///
/// 参数泛化为 `AsyncWrite + Unpin` 而非具体 `tokio::fs::File`：生产行为完全
/// 不变（tokio File 的 flush 即等待内部阻塞池上的在途写排空，泛型不改变该
/// 语义），泛化只为让测试能传入受控 flush 的 AsyncWrite 桩**直接驱动本生产
/// 入口**，捕捉将来在外层重新加入前置取消检查之类的回归；不是为测试另建
/// 第二入口。
#[inline]
pub(super) async fn drain_inflight<W: tokio::io::AsyncWrite + Unpin + ?Sized>(
    file: &mut W,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> AppResult<()> {
    await_sync_then_verify(
        async {
            file.flush()
                .await
                .map_err(|e| AppError::infrastructure_with_source("写入插件下载失败", e))
        },
        cancellation,
        deadline,
    )
    .await
}

/// 包住有副作用的异步操作：启动前预检取消/截止，返回后再次复核。
///
/// 响应的 `chunk()` 只在自身轮询时观察取消与截止，而 `session.get()` 返回到
/// 首次写入之间还有创建暂存、截断等文件 await：令牌或截止时间恰好在这段窗口
/// 到达时，若没有预检就会先创建空文件或先销毁既有前缀，之后才由 body 读到
/// 取消。预检让“已取消/已过期”在副作用启动前终止；后置复核让等待期间到达的
/// 取消/超时仍被如实报告，而不是被一次成功的打开或截断吞成继续写入。
///
/// 边界必须诚实：操作一旦真正开始，其 OS 副作用（CREATE_NEW、set_len）不能
/// 原子撤回。这里不丢弃仍在运行的 future、不按名删除或恢复文件来伪装回收；
/// 句柄由调用方作用域关闭，副作用状态如实保留给下一次会话重新判定。与
/// `await_sync_then_verify` 一样，操作自身的失败优先原样透传，复核只决定
/// 成功能否被接受。
pub(super) async fn guard_staging_side_effect<T>(
    cancellation: &CancellationToken,
    deadline: Instant,
    operation: impl Future<Output = AppResult<T>>,
) -> AppResult<T> {
    verify_still_live(cancellation, deadline)?;
    let value = operation.await?;
    verify_still_live(cancellation, deadline)?;
    Ok(value)
}

/// 完整暂存的句柄级复核与补同步；竞态窗口的判别点集中在这里。
///
/// 以 `Existing` 语义打开（完整暂存必然在入口被观察到存在）：打开即用
/// no-follow 与独占共享模式，打开后凭**句柄自身**的类型与长度复核，而不是
/// 沿用入口检查的旧元数据：路径被删除时打开直接失败，绝不重建出零字节文件
/// 伪报成功；被不同大小的文件替换时按句柄证据明确失败。`pub(super)` 是为了
/// 把竞态窗口作为最小注入点暴露给测试，用删除/替换后的确定状态直接判别
/// 该逻辑。
pub(super) async fn sync_verified_complete(path: &Path, expected_size: u64) -> AppResult<()> {
    let file = open_verified_staging(path, expected_size, StagingCreate::Existing).await?;
    file.sync_all()
        .await
        .map_err(|e| AppError::infrastructure_with_source("同步插件下载失败", e))?;
    Ok(())
}

/// 暂存的创建语义，由**入口观察到的存在性**决定，而不是由打开时刻猜测。
///
/// 首次市场安装时安装器只建好 stage 目录，`pkg.kplugin` 尚不存在，下载器
/// 必须能创建常规暂存文件；而入口已确认存在的暂存若在检查后消失，说明路径
/// 已被第三方改动，重建会把下载成果重新绑定到可疑对象。两种情形靠入口的
/// `symlink_metadata` 结果区分，不依赖“文件不存在就新建”的启发式。
#[derive(Clone, Copy)]
pub(super) enum StagingCreate {
    /// 入口已确认存在：只允许原样打开；消失、被替换或被追加别名都失败且不重建。
    Existing,
    /// 入口确认不存在：`create_new` 独占创建；并发出现的任何对象都使创建失败。
    Fresh,
}

/// 以 no-follow 打开暂存并在句柄上复核类型与长度，返回已验证的写入句柄。
///
/// 语义约束：
/// - 创建模式见 [`StagingCreate`]：只有入口确认不存在时才 `create_new(true)`
///   （Unix `O_CREAT|O_EXCL`、Windows `CREATE_NEW`），路径一旦已被占用（含
///   悬空符号链接、重解析点）即失败，绝不跟随、截断或覆盖未知对象；入口确认
///   存在时用 `create(false)`，文件消失后只失败、不重建。
/// - Windows 用 `FILE_FLAG_OPEN_REPARSE_POINT`（0x0020_0000）打开最终组件：
///   对符号链接会拿到链接自身（重解析属性可见）而不是目标文件，配合打开后
///   的属性复核把链接/重解析点拒绝在写入之前——不带该标志时按名打开拿到
///   的是目标句柄，句柄级 metadata 看不到链接，复核会失效。对常规文件该
///   标志无副作用，常规打开照常成功。
/// - Unix 用 `O_NOFOLLOW` 达成同样的最终组件 no-follow；openat 级祖先目录
///   防护超出本文件能力，作为剩余边界如实声明。
/// - 独占共享模式（Windows `share_mode(0)`）使并发按名替换即便成功也不能
///   与本句柄同时打开写入，收缩攻击窗口；Unix 无对应共享语义，依赖布局层
///   目录权限。
/// - 打开后立即在句柄上复核：必须是常规文件、非链接/重解析点、长度等于期望
///   值（Fresh 必为 0，Existing 为入口确认过的前缀长度），任何不符都拒绝，
///   绝不以 create 重建。
/// - 硬链接数大于 1 时拒绝：另一个名字指向同一 inode 意味着外部可以经由
///   其他路径改写下载成果，下载器不接受这种别名。
pub(super) async fn open_verified_staging(
    path: &Path,
    expected_offset: u64,
    create: StagingCreate,
) -> AppResult<tokio::fs::File> {
    let fresh = matches!(create, StagingCreate::Fresh);
    let mut options = OpenOptions::new();
    options.write(true);
    if fresh {
        options.create_new(true);
    } else {
        options.create(false);
    }
    #[cfg(unix)]
    {
        // `custom_flags` 是 tokio `OpenOptions` 的 inherent 方法（unix 块，
        // 签名 i32），inherent 方法优先于 std 的 `OpenOptionsExt` trait 方法，
        // 因此导入 std trait 会被判定未使用；跨平台都只调用 tokio API，两个
        // 分支都不需要 std 扩展 trait，避免 clippy `-D warnings` 失败。
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        // 0x0020_0000 = FILE_FLAG_OPEN_REPARSE_POINT：见上方文档的 no-follow
        // 语义。`custom_flags` 与 `share_mode` 均为 tokio `OpenOptions` 的
        // inherent 方法（windows 块），无需 std 扩展 trait。
        options.custom_flags(0x0020_0000).share_mode(0);
    }
    let file = match options.open(path).await {
        Ok(file) => file,
        // Fresh 模式下路径已被占用（含并发创建/链接替换）不是意外：拒绝创建
        // 才是正确结果，不能回退成按名打开去覆写对方。
        Err(error) if fresh && error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(AppError::invalid("插件下载暂存被并发创建"));
        }
        Err(error) => {
            return Err(AppError::infrastructure_with_source(
                "打开插件下载暂存失败",
                error,
            ));
        }
    };
    let opened = file
        .metadata()
        .await
        .map_err(|e| AppError::infrastructure_with_source("读取插件下载暂存失败", e))?;
    let expected = if fresh { 0 } else { expected_offset };
    if is_reparse_or_symlink(&opened) || !opened.is_file() || opened.len() != expected {
        return Err(AppError::invalid("插件下载暂存在检查后被替换"));
    }
    if !ensure_single_link(&file, &opened).await {
        return Err(AppError::invalid("插件下载暂存被多路径引用"));
    }
    Ok(file)
}

/// 句柄级硬链接数复核；无法确认链接数的平台必须拒绝，不把未知当已验证。
///
/// Unix 直接读 `nlink`；Windows 标准库不暴露链接数，用
/// `GetFileInformationByHandle` 取 `nNumberOfLinks`（workspace 已启用
/// `Win32_Storage_FileSystem`）；其余平台按不可验证处理，拒绝打开。
async fn ensure_single_link(file: &tokio::fs::File, metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 1
    }
    #[cfg(windows)]
    {
        let checked = {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            // 阻塞系统调用在异步上下文执行是可接受的：单次调用无等待、纳秒级。
            let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
            ok != 0 && information.nNumberOfLinks == 1
        };
        let _ = metadata;
        checked
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, metadata);
        false
    }
}

/// 按 RFC 9110 entity-tag 语法对响应携带的 ETag 头做单一分类。
///
/// 合法**强**校验值必须整体以 DQUOTE 开头并以 DQUOTE 结尾，引号内部每字节
/// 符合 etagc（`0x21`、`0x23..=0x7E`、`0x80..=0xFF`）：该范围允许逗号，因此
/// `"a,b"` 是合法强值；只有内部 DQUOTE 才使 `"a","b"` 这类形态非法。弱值
/// `W/"…"` 不是强校验值，不能作为 If-Range 锚点。直接校验 HeaderValue 的
/// 原始字节：合法 obs-text（0x80..=0xFF）不经过 UTF-8/可见 ASCII 的 to_str
/// 约束，避免把合法值误拒。多个 ETag 头或单头内多值语义不明，同样可疑。
/// 分类三态被两条生产路径共用：200 阶段只有 Strong 才保存为 If-Range 锚点
/// （其余按无锚点整包重取），206 阶段 Absent 可接受、Strong 必须与锚点一致、
/// Suspicious 一律拒绝；两处标准由此不会漂移。
fn classify_etag(headers: &HeaderMap) -> EtagPresence<'_> {
    let mut values = headers.get_all(ETAG).iter();
    let Some(first) = values.next() else {
        return EtagPresence::Absent;
    };
    // 重复头（或合并后的多值）没有可判定语义，与畸形同视为可疑。
    if values.next().is_some() {
        return EtagPresence::Suspicious;
    }
    let bytes = first.as_bytes();
    let strong = bytes.len() >= 2
        && bytes[0] == b'"'
        && bytes[bytes.len() - 1] == b'"'
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|byte| matches!(byte, 0x21 | 0x23..=0x7E | 0x80..=0xFF));
    if strong {
        EtagPresence::Strong(first)
    } else {
        EtagPresence::Suspicious
    }
}

/// ETag 头分类结果：无头、单个合法强值、或弱值/畸形/多值的可疑形态。
enum EtagPresence<'a> {
    Absent,
    Strong(&'a HeaderValue),
    Suspicious,
}

/// 请求与 body 阶段共用的可重试判定。
///
/// 策略拒绝、取消和超时在 `AssetNetworkError` 中各有独立分类，这里只放行传输层
/// 失败。reqwest 会把流中途断开归到 decode 类而不是 body 类；下载没有启用内容
/// 编码，所以下载路径上的 decode 失败就是传输被截断，只有这种中断才允许续传。
fn retryable_transport(error: &AssetNetworkError) -> bool {
    let AssetNetworkError::Transport { source, .. } = error else {
        return false;
    };
    error.is_retryable() || source.is_body() || source.is_decode()
}

/// 判断暂存路径元数据是否为链接或 Windows 重解析点。
///
/// 非 Windows 平台 `is_symlink()` 即为完整判定；Windows 的 symlink 与 junction
/// 都带有 `FILE_ATTRIBUTE_REPARSE_POINT`（0x400），`Metadata::is_symlink()` 只
/// 识别其中一部分，因此按安装器既有 `safe_metadata` 的同一标准用属性位把全部
/// 重解析点一并拒绝。该判定基于属性位的平台文档语义，不额外声称覆盖未列举的
/// 文件系统形态。
fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_download.rs"]
mod tests;
