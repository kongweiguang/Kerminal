//! updater endpoint、签名公钥和下载限制的受控配置。
//!
//! @author kongweiguang

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cargo_packager_updater::{Config, WindowsConfig, WindowsUpdateInstallMode, url::Url};
use minisign_verify::PublicKey;
use semver::Version;

use kerminal_application::{AppError, AppResult};

#[cfg(windows)]
#[path = "windows_install.rs"]
mod windows_install;

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
    /// 正式构建在装配时验证嵌入公钥，避免无效信任根让更新入口看似可用却永远验签失败。
    ///
    /// @author kongweiguang
    pub fn from_release_config(
        current_version: &str,
        endpoint: &str,
        public_key: &str,
    ) -> AppResult<Self> {
        let endpoint = Url::parse(endpoint).map_err(|_| AppError::invalid("更新地址格式无效"))?;
        let decoded_key = STANDARD
            .decode(public_key)
            .map_err(|_| AppError::invalid("更新签名公钥格式无效"))?;
        let key_text = std::str::from_utf8(&decoded_key)
            .map_err(|_| AppError::invalid("更新签名公钥格式无效"))?;
        PublicKey::decode(key_text).map_err(|_| AppError::invalid("更新签名公钥格式无效"))?;

        Self::new(
            current_version,
            vec![endpoint],
            public_key.to_owned(),
            Duration::from_secs(120),
            MAX_UPDATE_BYTES,
        )
    }

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

    /// 只在可确认的正式安装形态启用更新，避免开发可执行文件触发用户安装器。
    pub fn supports_current_runtime() -> bool {
        #[cfg(target_os = "linux")]
        {
            std::env::var_os("APPIMAGE").is_some()
        }
        #[cfg(target_os = "windows")]
        {
            windows_install::is_current_install()
        }
        #[cfg(target_os = "macos")]
        {
            is_current_macos_bundle()
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

/// macOS 原位更新只接受应用包内的主程序，不把裸开发二进制解释为可替换的 bundle。
#[cfg(target_os = "macos")]
fn is_current_macos_bundle() -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let Some(macos) = executable.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    executable
        .file_name()
        .is_some_and(|name| name == "kerminal")
        && macos.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && contents
            .parent()
            .and_then(|bundle| bundle.extension())
            .is_some_and(|extension| extension == "app")
}

/// 更新源在所有构建配置下都必须是无凭据 HTTPS。
fn is_allowed_endpoint(endpoint: &Url) -> bool {
    let has_safe_authority = endpoint.host().is_some()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.fragment().is_none();
    has_safe_authority && endpoint.scheme() == "https"
}
