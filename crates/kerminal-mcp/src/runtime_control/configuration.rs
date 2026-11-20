//! MCP 监听配置的串行应用、回滚和敏感客户端配置生成。
//!
//! @author kongweiguang

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    mcp_runtime::{McpClientConfig, McpRuntimeControl, McpRuntimeState},
    preferences::settings::{McpBindMode, McpSettings, VersionedSettings},
};
use kerminal_domain::preferences::ConfigRevision;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::McpRuntimeController;

impl McpRuntimeController {
    /// 在唯一配置门内先验证新端口、替换 listener，再以读取时 revision 提交设置。
    pub(super) async fn apply_configuration(
        &self,
        mut requested: McpSettings,
        expected_revision: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<VersionedSettings> {
        let _configuration = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("MCP 设置已取消")),
            guard = self.configuration_gate.lock() => guard,
        };
        self.ensure_available()?;
        let preferences = self
            .preferences
            .as_ref()
            .ok_or_else(|| AppError::unsupported("MCP 设置服务不可用"))?;
        let loaded = preferences.load_settings().await?;
        if loaded.revision != expected_revision {
            return Err(AppError::conflict(
                "设置已变更，请重新读取后再应用 MCP 配置",
            ));
        }
        // 启用意图只由手动启停用例维护；监听草稿来自 UI 的 struct 字面量，
        // 不能让它缺省构造出的 false 覆盖用户已确认的自动启动选择。
        requested.auto_start = loaded.settings.mcp.auto_start;
        if requested.port == 0 {
            return Err(AppError::invalid("MCP 端口需要在 1 到 65535 之间"));
        }
        let state = self.snapshot().state;
        if !matches!(
            state,
            McpRuntimeState::Running | McpRuntimeState::Stopped | McpRuntimeState::Failed
        ) {
            return Err(AppError::conflict(
                "MCP 仍在启动、同步或停止，请先完成当前操作",
            ));
        }
        let previous = if state == McpRuntimeState::Running {
            let mut current = self
                .settings
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // Running 快照里的启用意图可能落后于磁盘（例如运行中改过文件），它不描述
            // 当前 listener；统一成权威值，避免只改意图就触发一次无意义的监听重启。
            current.auto_start = requested.auto_start;
            current
        } else {
            loaded.settings.mcp.clone()
        };
        if requested.bind_mode == McpBindMode::Lan {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("MCP 设置已取消"));
            }
            let store = self
                .token_store
                .as_ref()
                .ok_or_else(|| AppError::unsupported("MCP 令牌服务不可用"))?;
            let reference = requested
                .token_ref
                .as_deref()
                .or(previous.token_ref.as_deref());
            let token = store.load_or_create(reference).await?;
            requested.token_ref = Some(token.reference().to_owned());
        } else if requested.token_ref.is_none() {
            requested.token_ref = previous.token_ref.clone();
        }
        requested = requested.validated()?;
        if requested == loaded.settings.mcp
            && (state != McpRuntimeState::Running || requested == previous)
        {
            return Ok(loaded);
        }
        let was_running = state == McpRuntimeState::Running;
        let next_address = SocketAddr::new(
            match requested.bind_mode {
                McpBindMode::Loopback => Ipv4Addr::LOCALHOST,
                McpBindMode::Lan => Ipv4Addr::UNSPECIFIED,
            }
            .into(),
            requested.port,
        );
        let reserved = if was_running && requested.port != previous.port {
            Some(crate::server::bind_listener(next_address).await?)
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("MCP 设置已取消"));
        }

        if was_running {
            self.stop_inner(Instant::now() + Duration::from_secs(5))
                .await?;
            self.set_config(requested.clone())?;
            if let Err(error) = self.start_core(CancellationToken::new(), reserved).await {
                self.restore_configuration(previous).await?;
                return Err(error);
            }
        }

        let mut next = loaded.settings;
        next.mcp = requested.clone();
        let saved = match preferences.save_settings(next, expected_revision).await {
            Ok(saved) => Ok(saved),
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                match preferences.load_settings().await {
                    Ok(readback) if readback.settings.mcp == requested => Ok(readback),
                    Ok(_) | Err(_) => return Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match saved {
            Ok(saved) => {
                if !was_running {
                    self.set_config(requested)?;
                }
                Ok(saved)
            }
            Err(error) => {
                if was_running {
                    self.restore_configuration(previous).await?;
                }
                Err(error)
            }
        }
    }

    /// 先收尾新 listener，再按旧配置恢复；任何清理或重启失败都保留真实状态供重试。
    async fn restore_configuration(&self, previous: McpSettings) -> AppResult<()> {
        self.stop_inner(Instant::now() + Duration::from_secs(5))
            .await?;
        self.set_config(previous)?;
        self.start_core(CancellationToken::new(), None)
            .await
            .map(|_| ())
    }

    /// 手动启停的意图必须在触碰 listener 前确认；结果未知只按权威回读核对，
    /// 不重放写入，避免把另一次并发保存误当成自己的已提交结果。
    ///
    /// 有 deadline 时只在真正写盘前检查一次是否还有预算；已经开始原子替换就完整
    /// 等待结果，不能用 `timeout` 丢弃 future 而留下无法判断的磁盘提交。
    pub(super) async fn persist_auto_start(
        &self,
        auto_start: bool,
        deadline: Option<Instant>,
        cancellation: Option<&CancellationToken>,
    ) -> AppResult<VersionedSettings> {
        let preferences = self
            .preferences
            .as_ref()
            .ok_or_else(|| AppError::unsupported("MCP 设置服务不可用"))?;
        let loaded = preferences.load_settings().await?;
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::cancelled("MCP start was cancelled"));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(AppError::timeout("MCP configuration operation"));
        }
        if loaded.settings.mcp.auto_start == auto_start {
            return Ok(loaded);
        }
        let expected_revision = loaded.revision;
        let mut next = loaded.settings;
        next.mcp.auto_start = auto_start;
        match preferences
            .save_settings(next.clone(), expected_revision)
            .await
        {
            Ok(saved) => Ok(saved),
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                // 只确认提交的同一份设置，避免采用另一笔编辑改变的端口或监听范围。
                match preferences.load_settings().await {
                    Ok(readback) if readback.settings == next => Ok(readback),
                    Ok(_) | Err(_) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 结果必须来自当前运行中的 listener，避免复制已经过期的本机网卡地址。
    pub(super) async fn current_lan_addresses(&self) -> AppResult<Vec<Ipv4Addr>> {
        let owner = self.owner.lock().await;
        if self.snapshot().state != McpRuntimeState::Running {
            return Err(AppError::conflict("请先启动 MCP 服务"));
        }
        let server = owner
            .server
            .as_ref()
            .ok_or_else(|| AppError::conflict("MCP 服务未运行"))?;
        Ok(server.lan_addresses().to_vec())
    }

    /// 仅显式复制时从 Vault 读取令牌；地址必须是 listener 启动时验证的本机网卡。
    pub(super) async fn create_lan_client_config(
        &self,
        address: Ipv4Addr,
    ) -> AppResult<McpClientConfig> {
        let _configuration = self.configuration_gate.lock().await;
        let addresses = self.current_lan_addresses().await?;
        if !addresses.contains(&address) {
            return Err(AppError::invalid("MCP 局域网地址不属于当前监听器"));
        }
        let settings = self
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if settings.bind_mode != McpBindMode::Lan {
            return Err(AppError::conflict("MCP 服务未启用局域网监听"));
        }
        let reference = settings
            .token_ref
            .as_deref()
            .ok_or_else(|| AppError::invalid("MCP 令牌引用缺失"))?;
        let token = self
            .token_store
            .as_ref()
            .ok_or_else(|| AppError::unsupported("MCP 令牌服务不可用"))?
            .load_or_create(Some(reference))
            .await?;
        Ok(McpClientConfig::new(format!(
            "{{\n  \"mcpServers\": {{\n    \"kerminal\": {{\n      \"type\": \"http\",\n      \"url\": \"http://{address}:{}/mcp\",\n      \"headers\": {{ \"Authorization\": \"Bearer {}\" }}\n    }}\n  }}\n}}",
            settings.port,
            token.secret(),
        )))
    }
}
