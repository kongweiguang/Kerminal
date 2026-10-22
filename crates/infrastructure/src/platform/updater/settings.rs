//! updater endpoint、签名公钥和下载限制的受控配置。
//!
//! @author kongweiguang

use std::time::Duration;

use cargo_packager_updater::{Config, WindowsConfig, WindowsUpdateInstallMode, url::Url};
use semver::Version;

use kerminal_application::{AppError, AppResult};

/// 统一限制请求时长和下载包上限，避免远端 metadata 导致无界运行或缓存。
pub(super) const MAX_UPDATE_BYTES: u64 = 1024 * 1024 * 1024;

/// 更新配置只接收明确注入的端点、公钥和当前版本，不从任意用户环境变量取密钥。
#[derive(Clone)]
pub struct UpdaterSettings {
    current_version: Version,
    config: Config,
    timeout: Duration,
    max_update_bytes: u64,
}

impl UpdaterSettings {
    /// 固定 HTTPS、SemVer、passive NSIS 与资源上限，使正式和测试端点走同一验证路径。
    pub fn new(
        current_version: &str,
        endpoints: Vec<Url>,
        public_key: String,
        timeout: Duration,
        max_update_bytes: u64,
    ) -> AppResult<Self> {
        Self::build(
            current_version,
            endpoints,
            public_key,
            timeout,
            max_update_bytes,
        )
    }

    /// 统一校验正式和测试配置，避免测试构建获得更宽松的网络策略。
    fn build(
        current_version: &str,
        endpoints: Vec<Url>,
        public_key: String,
        timeout: Duration,
        max_update_bytes: u64,
    ) -> AppResult<Self> {
        if endpoints.is_empty()
            || endpoints
                .iter()
                .any(|endpoint| !is_allowed_endpoint(endpoint))
        {
            return Err(AppError::invalid("更新地址必须是无凭据的 HTTPS 地址"));
        }
        if public_key.trim().is_empty() {
            return Err(AppError::invalid("更新签名公钥不能为空"));
        }
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(AppError::invalid("更新请求超时时间无效"));
        }
        if max_update_bytes == 0 || max_update_bytes > MAX_UPDATE_BYTES {
            return Err(AppError::invalid("更新包大小限制无效"));
        }
        let current_version = Version::parse(current_version)
            .map_err(|_| AppError::invalid("当前应用版本不是有效 SemVer"))?;
        Ok(Self {
            current_version,
            config: Config {
                endpoints,
                pubkey: public_key,
                windows: Some(WindowsConfig {
                    installer_args: None,
                    install_mode: Some(WindowsUpdateInstallMode::Passive),
                }),
            },
            timeout,
            max_update_bytes,
        })
    }

    /// 仅在 Linux AppImage 运行态启用原位替换，DEB 安装交由系统包管理器更新。
    pub fn supports_current_runtime() -> bool {
        #[cfg(target_os = "linux")]
        {
            std::env::var_os("APPIMAGE").is_some()
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            true
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            false
        }
    }

    /// 返回冻结的 endpoint 顺序，检查逻辑按剩余的全局时限逐个尝试。
    pub(super) fn endpoints(&self) -> &[Url] {
        &self.config.endpoints
    }

    /// 为 SDK Update 保留安装器配置与信任根；metadata 传输只读取，不自行重建此契约。
    pub(super) fn config(&self) -> Config {
        self.config.clone()
    }

    /// 提供固定的当前 SemVer，更新判断继续使用 SDK 相同的 Version 排序规则。
    pub(super) fn current_version(&self) -> &Version {
        &self.current_version
    }

    /// metadata 检查临时收紧了单次超时，更新对象下载时恢复设置中的原始超时。
    pub(super) const fn download_timeout(&self) -> Duration {
        self.timeout
    }

    /// 返回下载上限供验签成功后的结果核对，不能让服务器 metadata 改写此策略。
    pub(super) const fn max_update_bytes(&self) -> u64 {
        self.max_update_bytes
    }
}

/// 更新源在所有构建配置下都必须是无凭据 HTTPS。
fn is_allowed_endpoint(endpoint: &Url) -> bool {
    let has_safe_authority = endpoint.host().is_some()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.fragment().is_none();
    has_safe_authority && endpoint.scheme() == "https"
}
