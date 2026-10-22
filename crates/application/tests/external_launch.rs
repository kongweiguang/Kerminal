//! 外部启动的应用边界回归测试。
//!
//! 使用内存端口验证解析、分类与一次性认证闭环，不启动网络连接或写入用户配置。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    platform::external_launch::{
        ExternalActivation, ExternalAuthenticationHint, ExternalLaunchIntent,
        ExternalLaunchOutcome, ExternalLaunchService, ExternalTargetClassification,
        ExternalTargetLease, ExternalTargetLeaseOwner, ExternalTemporaryTargetRegistry,
        ExternalTemporaryTargetSpec, SavedExternalTargetLookup,
    },
    preferences::settings::{ExternalLaunchSettings, ExternalLaunchToolSetting},
};
use kerminal_domain::{connections::RemoteProtocol, identity::HostId};
use zeroize::Zeroizing;

#[derive(Clone)]
struct TestSavedLookup {
    matched_host: Option<HostId>,
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

#[async_trait]
impl SavedExternalTargetLookup for TestSavedLookup {
    /// 测试只返回保存目标身份，特意不暴露连接 profile 或 Vault 凭据。
    async fn find_exact(
        &self,
        _target: &kerminal_application::platform::external_launch::ExternalTargetIdentity,
    ) -> AppResult<Option<HostId>> {
        if let Some((entered, release)) = &self.gate {
            entered.notify_one();
            release.notified().await;
        }
        Ok(self.matched_host.clone())
    }
}

#[derive(Default)]
struct RegistryState {
    registered: Vec<ExternalTemporaryTargetSpec>,
    passwords: Vec<(HostId, String)>,
    active_scopes: HashMap<HostId, usize>,
    released: Vec<HostId>,
    cleared: Vec<HostId>,
}

#[derive(Clone, Default)]
struct TestTemporaryRegistry {
    state: Arc<Mutex<RegistryState>>,
}

#[async_trait]
impl ExternalTemporaryTargetRegistry for TestTemporaryRegistry {
    /// 临时注册只保存在测试内存，并为初始消费者建立一个可回收 owner scope。
    async fn register(
        &self,
        target: ExternalTemporaryTargetSpec,
    ) -> AppResult<ExternalTargetLease> {
        let host_id = HostId::parse("external:test-launch")
            .map_err(|_| kerminal_application::AppError::invalid("测试临时主机标识生成失败"))?;
        {
            let mut state = self.state.lock().expect("test mutex poisoned");
            state.registered.push(target);
            state.active_scopes.insert(host_id.clone(), 1);
        }
        ExternalTargetLease::new_registered(host_id, Arc::new(self.clone()))
    }

    /// 记录用户提示输入供测试断言，生产实现由凭据适配器接管 Zeroizing 值。
    async fn set_password(&self, host_id: &HostId, password: Zeroizing<String>) -> AppResult<()> {
        self.state
            .lock()
            .expect("test mutex poisoned")
            .passwords
            .push((host_id.clone(), password.to_string()));
        Ok(())
    }

    /// 进程退出时清空测试 registry，旧 lease 的后续 Drop 必须安全成为空操作。
    async fn clear_all(&self) -> AppResult<()> {
        let mut state = self.state.lock().expect("test mutex poisoned");
        let active = std::mem::take(&mut state.active_scopes);
        state.cleared.extend(active.into_keys());
        Ok(())
    }
}

impl ExternalTargetLeaseOwner for TestTemporaryRegistry {
    /// 测试替身只对仍在表内的目标增加引用，模拟退出清理后的失效行为。
    fn retain_scope(&self, host_id: &HostId) -> AppResult<()> {
        let mut state = self.state.lock().expect("test mutex poisoned");
        let count = state
            .active_scopes
            .get_mut(host_id)
            .ok_or_else(|| kerminal_application::AppError::not_found("临时外部连接已关闭"))?;
        *count += 1;
        Ok(())
    }

    /// lease Drop 只调整内存引用数，并在最后一个 owner 离开时记录 profile 清理。
    fn release_scope(&self, host_id: &HostId) {
        let mut state = self.state.lock().expect("test mutex poisoned");
        let Some(count) = state.active_scopes.get_mut(host_id) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            state.active_scopes.remove(host_id);
            state.released.push(host_id.clone());
        }
    }
}

/// 构造只依赖内存端口的服务，避免测试意外触发连接或持久化。
fn service(matched_host: Option<HostId>) -> (ExternalLaunchService, TestTemporaryRegistry) {
    service_with_settings(matched_host, ExternalLaunchSettings::default())
}

/// 提供可注入偏好的隔离服务装配，所有协议与存储依赖仍留在内存测试替身。
fn service_with_settings(
    matched_host: Option<HostId>,
    settings: ExternalLaunchSettings,
) -> (ExternalLaunchService, TestTemporaryRegistry) {
    let registry = TestTemporaryRegistry::default();
    let service = ExternalLaunchService::new(
        Arc::new(TestSavedLookup {
            matched_host,
            gate: None,
        }),
        Arc::new(registry.clone()),
        settings,
    );
    (service, registry)
}

/// 统一创建仿照 Windows 启动参数的有界 activation DTO。
fn activation(arguments: &[&str]) -> ExternalActivation {
    ExternalActivation {
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        working_directory: Some(r"C:\Users\tester".to_owned()),
    }
}

/// Kerminal SSH URL 应生成临时目标，并仅将保存匹配用于 Known 展示分类。
#[tokio::test]
async fn kerminal_ssh_url_routes_as_typed_known_target() {
    let saved_host = HostId::parse("saved:one").expect("valid host id");
    let (service, registry) = service(Some(saved_host.clone()));
    let outcome = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=Example.COM.&port=2222&user=alice",
        ]))
        .await
        .expect("valid URL routes");

    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("SSH URL should be ready with explicit username");
    };
    assert_eq!(target.host_id().as_str(), "external:test-launch");
    assert_eq!(target.identity.protocol, RemoteProtocol::Ssh);
    assert_eq!(target.identity.endpoint.host, "example.com");
    assert_eq!(target.identity.endpoint.username, "alice");
    assert_eq!(
        target.classification,
        ExternalTargetClassification::Known {
            saved_host_id: saved_host
        }
    );
    let state = registry.state.lock().expect("test mutex poisoned");
    assert_eq!(state.registered.len(), 1);
    assert_eq!(
        state.registered[0].authentication,
        ExternalAuthenticationHint::Agent
    );
}

/// URI query allowlist拒绝密码或未知字段，且拒绝结果之前不会注册连接目标。
#[tokio::test]
async fn kerminal_url_rejects_credentials_and_unknown_parameters() {
    for raw_url in [
        "kerminal://ssh?host=example.com&user=alice&password=secret",
        "kerminal://ssh?host=example.com&user=alice&futureOption=true",
    ] {
        let (service, registry) = service(None);
        assert!(
            service
                .handle_activation(activation(&["kerminal.exe", raw_url]))
                .await
                .is_err()
        );
        assert!(
            registry
                .state
                .lock()
                .expect("test mutex poisoned")
                .registered
                .is_empty()
        );
    }
}

/// OpenSSH 连接参数保留远程命令 argv 和有序跳板，不把命令拼接成 shell 字符串。
#[tokio::test]
async fn openssh_preserves_remote_command_arguments_and_jump_chain() {
    let (service, _) = service(None);
    let outcome = service
        .handle_activation(activation(&[
            "ssh.exe",
            "-p",
            "2200",
            "-J",
            "gate@relay.example:2222",
            "alice@db.example",
            "uptime",
            "&&",
            "id",
        ]))
        .await
        .expect("supported OpenSSH args route");

    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("explicit username should avoid prompt");
    };
    assert_eq!(target.identity.endpoint.port, 2200);
    assert_eq!(target.identity.jump_hosts.len(), 1);
    assert_eq!(target.identity.jump_hosts[0].host, "relay.example");
    assert_eq!(target.identity.jump_hosts[0].port, 2222);
    assert_eq!(target.identity.jump_hosts[0].username, "gate");
    assert_eq!(
        target.intent,
        ExternalLaunchIntent::SshTerminal {
            remote_command: vec!["uptime".into(), "&&".into(), "id".into()]
        }
    );
}

/// `--` 后的 OpenSSH destination 仍按 host 解析，避免把选项结束符当目标主机。
#[tokio::test]
async fn openssh_double_dash_keeps_destination_and_command_boundaries() {
    let (service, _) = service(None);
    let outcome = service
        .handle_activation(activation(&["ssh", "--", "alice@db.example", "uptime"]))
        .await
        .expect("supported OpenSSH args route");
    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("explicit username should avoid prompt");
    };
    assert_eq!(target.identity.endpoint.host, "db.example");
    assert_eq!(
        target.intent,
        ExternalLaunchIntent::SshTerminal {
            remote_command: vec!["uptime".into()]
        }
    );
}

/// SFTP URL 路径严格百分号解码为远端绝对路径，并要求用户明确提供认证。
#[tokio::test]
async fn sftp_url_requests_password_without_identity_file() {
    let saved_host = HostId::parse("saved:sftp").expect("valid host id");
    let (service, registry) = service(Some(saved_host.clone()));
    let outcome = service
        .handle_activation(activation(&[
            "WinSCP.exe",
            "sftp://alice@files.example/home/alice/My%20Files",
        ]))
        .await
        .expect("valid SFTP URL routes");

    let ExternalLaunchOutcome::PasswordRequired { target } = outcome else {
        panic!("SFTP requires one-time authentication when it has no key");
    };
    assert_eq!(target.identity.protocol, RemoteProtocol::Sftp);
    assert!(target.authentication_required);
    assert_eq!(
        target.intent,
        ExternalLaunchIntent::SftpTransfer {
            remote_path: "/home/alice/My Files".into(),
            expected_host_key: None,
        }
    );
    assert_eq!(
        target.classification,
        ExternalTargetClassification::Known {
            saved_host_id: saved_host
        }
    );
    service
        .provide_password(&target.lease, "one-time-secret".to_owned())
        .await
        .expect("the UI-supplied secret belongs to the runtime target");
    assert_eq!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .passwords[0]
            .1,
        "one-time-secret"
    );
}

/// URL userinfo 密码必须被拒绝，不能被误当成用户名或写入临时认证仓储。
#[tokio::test]
async fn sftp_url_rejects_password_userinfo() {
    let (service, registry) = service(None);
    assert!(
        service
            .handle_activation(activation(&[
                "filezilla.exe",
                "sftp://alice:secret@files.example/home",
            ]))
            .await
            .is_err()
    );
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );
}

/// PuTTY 安全直连字段保留 Windows 私钥路径，但口令开关不能跨进程边界传入。
#[tokio::test]
async fn putty_accepts_key_path_and_rejects_inline_password() {
    let (launch_service, registry) = service(None);
    let outcome = launch_service
        .handle_activation(activation(&[
            r"C:\Program Files\PuTTY\putty.exe",
            "-ssh",
            "-P",
            "2222",
            "-l",
            "alice",
            "-i",
            r"C:\keys\id_ed25519",
            "host.example",
        ]))
        .await
        .expect("safe PuTTY args route");
    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("explicit username should avoid prompt");
    };
    assert_eq!(target.identity.endpoint.port, 2222);
    assert_eq!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered[0]
            .authentication,
        ExternalAuthenticationHint::PrivateKey {
            path: r"C:\keys\id_ed25519".into()
        }
    );

    let (launch_service, registry) = service(None);
    assert!(
        launch_service
            .handle_activation(activation(&[
                "putty.exe",
                "-ssh",
                "-pwsecret",
                "host.example"
            ]))
            .await
            .is_err()
    );
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );
}

/// 全局开关关闭时在 parser 前拒绝，即使输入像连接链接也不查询或注册目标。
#[tokio::test]
async fn disabled_external_launch_is_rejected_before_parsing_or_registration() {
    let (service, registry) = service_with_settings(
        None,
        ExternalLaunchSettings {
            enabled: false,
            ..ExternalLaunchSettings::default()
        },
    );
    let error = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=example.com&user=alice",
        ]))
        .await
        .expect_err("disabled launch policy must reject requests");

    assert_eq!(error.public_message(), "外部启动已在设置中禁用");
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );
}

/// 关闭第三方参数仍允许原生 Kerminal 链接，单个客户端禁用项则精确拒绝该来源。
#[tokio::test]
async fn vendor_argument_and_per_tool_policies_are_applied() {
    let (service, registry) = service_with_settings(
        None,
        ExternalLaunchSettings {
            accept_vendor_args: false,
            ..ExternalLaunchSettings::default()
        },
    );
    let error = service
        .handle_activation(activation(&["ssh.exe", "alice@host.example"]))
        .await
        .expect_err("vendor arguments should be denied");
    assert_eq!(error.public_message(), "第三方客户端参数接收已在设置中禁用");
    let native = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=host.example&user=alice",
        ]))
        .await
        .expect("native links remain available");
    assert!(matches!(native, ExternalLaunchOutcome::Ready { .. }));
    assert_eq!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .len(),
        1
    );

    let (disabled_tool_service, disabled_tool_registry) = service_with_settings(
        None,
        ExternalLaunchSettings {
            disabled_tools: vec![ExternalLaunchToolSetting::Openssh],
            ..ExternalLaunchSettings::default()
        },
    );
    let error = disabled_tool_service
        .handle_activation(activation(&["ssh.exe", "--password", "secret"]))
        .await
        .expect_err("the source policy must run before vendor argument parsing");
    assert_eq!(error.public_message(), "此外部客户端已在设置中禁用");
    assert!(
        disabled_tool_registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );
}

/// 在精确保存目标查询期间收到禁用偏好时，恢复后仍须重新检查并阻止临时目标创建。
#[tokio::test]
async fn policy_update_during_saved_lookup_prevents_target_registration() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let registry = TestTemporaryRegistry::default();
    let service = ExternalLaunchService::new(
        Arc::new(TestSavedLookup {
            matched_host: None,
            gate: Some((entered.clone(), release.clone())),
        }),
        Arc::new(registry.clone()),
        ExternalLaunchSettings::default(),
    );
    let activation = activation(&[
        "kerminal.exe",
        "kerminal://ssh?host=host.example&user=alice",
    ]);
    let pending_service = service.clone();
    let request = tokio::spawn(async move { pending_service.handle_activation(activation).await });
    entered.notified().await;

    service
        .update_settings(ExternalLaunchSettings {
            disabled_tools: vec![ExternalLaunchToolSetting::KerminalNative],
            ..ExternalLaunchSettings::default()
        })
        .expect("apply disabled source preference while lookup waits");
    release.notify_one();
    let result = request.await.expect("join external launch request");

    assert!(result.is_err());
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );
}

/// MobaXterm 命令解析保留 Windows 路径反斜线且只生成 OpenSSH 连接意图。
#[tokio::test]
async fn mobaxterm_command_keeps_windows_key_path() {
    let (service, registry) = service(None);
    let outcome = service
        .handle_activation(activation(&[
            "MobaXterm.exe",
            "-newtab",
            r"ssh -i C:\keys\id_ed25519 alice@host.example",
        ]))
        .await
        .expect("supported direct MobaXterm command routes");
    assert!(matches!(outcome, ExternalLaunchOutcome::Ready { .. }));
    assert_eq!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered[0]
            .authentication,
        ExternalAuthenticationHint::PrivateKey {
            path: r"C:\keys\id_ed25519".into()
        }
    );
}

/// 无关普通启动参数保持无动作，避免桌面应用启动参数被误解为连接请求。
#[tokio::test]
async fn unrelated_activation_is_ignored_without_registering_target() {
    let (service, registry) = service(None);
    assert_eq!(
        service
            .handle_activation(activation(&["another-app.exe", "--version"]))
            .await
            .expect("unrelated app args are ignored"),
        ExternalLaunchOutcome::Ignored
    );
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );
}

/// 缺少用户名时先返回原请求，用户补全后才创建外部目标。
#[tokio::test]
async fn missing_username_waits_for_explicit_user_input() {
    let (service, registry) = service(None);
    let outcome = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=host.example",
        ]))
        .await
        .expect("username-free URL remains a typed request");
    let ExternalLaunchOutcome::UsernameRequired { request } = outcome else {
        panic!("missing username must be prompted before target creation");
    };
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .registered
            .is_empty()
    );

    let outcome = service
        .supply_username(request, "alice".into())
        .await
        .expect("explicit username resumes the same parsed request");
    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("SSH agent auth can proceed after username is supplied");
    };
    assert_eq!(target.identity.endpoint.username, "alice");
}

/// Debug 输出必须隐藏原始 argv，以防启动链接中的路径、命令和敏感文本进入日志。
#[test]
fn activation_debug_redacts_arguments_and_paths() {
    let activation = activation(&[
        "kerminal.exe",
        "kerminal://ssh?host=internal.example&user=alice",
        "secret-like-argument",
    ]);
    let rendered = format!("{activation:?}");
    assert!(!rendered.contains("internal.example"));
    assert!(!rendered.contains("alice"));
    assert!(!rendered.contains("secret-like-argument"));
    assert!(!rendered.contains("C:\\Users\\tester"));
}

/// 普通 Clone 共享一个 scope，retain 为独立 consumer 增加 scope，最后关闭时才回收 profile。
#[tokio::test]
async fn temporary_target_lease_releases_after_last_consumer_closes() {
    let (service, registry) = service(None);
    let outcome = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=host.example&user=alice",
        ]))
        .await
        .expect("valid SSH URL routes");
    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("explicit username should route");
    };
    let shared_clone = target.clone();
    let retained = target.lease.retain().expect("consumer scope is retained");
    let host_id = target.host_id().clone();

    drop(target);
    {
        let state = registry.state.lock().expect("test mutex poisoned");
        assert_eq!(state.active_scopes[&host_id], 2);
        assert!(
            state.released.is_empty(),
            "shared clone still owns the initial scope"
        );
    }
    drop(shared_clone);
    assert_eq!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .active_scopes[&host_id],
        1,
        "retained file/terminal consumer keeps the in-memory profile alive"
    );

    drop(retained);
    let state = registry.state.lock().expect("test mutex poisoned");
    assert!(!state.active_scopes.contains_key(&host_id));
    assert_eq!(state.released, vec![host_id]);
}

/// 退出清理立即移除 profile，且旧 lease 后续释放或 retain 不会复活该目标。
#[tokio::test]
async fn shutdown_clear_invalidates_outstanding_leases() {
    let (service, registry) = service(None);
    let outcome = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=host.example&user=alice",
        ]))
        .await
        .expect("valid SSH URL routes");
    let ExternalLaunchOutcome::Ready { target } = outcome else {
        panic!("explicit username should route");
    };
    let host_id = target.host_id().clone();
    let consumer = target.lease.retain().expect("consumer scope is retained");

    service
        .clear_temporary_targets_on_shutdown()
        .await
        .expect("shutdown clears transient profiles and credentials");
    assert!(target.lease.retain().is_err());
    assert!(consumer.retain().is_err());
    assert_eq!(
        registry.state.lock().expect("test mutex poisoned").cleared,
        vec![host_id]
    );

    drop(target);
    drop(consumer);
    assert!(
        registry
            .state
            .lock()
            .expect("test mutex poisoned")
            .released
            .is_empty()
    );
}

/// 动态偏好只投影为 SSH 成功后的附加 SFTP 动作，绝不替换显式 SFTP 意图。
/// @author kongweiguang
#[tokio::test]
async fn auto_open_sftp_is_applied_only_to_ssh_terminal_routes() {
    let (service, _) = service(None);
    let default_outcome = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=host.example&user=alice",
        ]))
        .await
        .expect("SSH route uses the current default setting");
    let ExternalLaunchOutcome::Ready { target } = default_outcome else {
        panic!("explicit username should route");
    };
    assert!(!target.open_sftp_after_connect);

    let settings = ExternalLaunchSettings {
        auto_open_sftp: true,
        ..ExternalLaunchSettings::default()
    };
    service
        .update_settings(settings)
        .expect("saved preference becomes active for the next request");

    let ssh_outcome = service
        .handle_activation(activation(&[
            "kerminal.exe",
            "kerminal://ssh?host=host.example&user=alice",
        ]))
        .await
        .expect("SSH route keeps its terminal intent");
    let ExternalLaunchOutcome::Ready { target: ssh_target } = ssh_outcome else {
        panic!("explicit username should route");
    };
    assert!(matches!(
        ssh_target.intent,
        ExternalLaunchIntent::SshTerminal { .. }
    ));
    assert!(ssh_target.open_sftp_after_connect);

    let sftp_outcome = service
        .handle_activation(activation(&[
            "WinSCP.exe",
            "sftp://alice@files.example/home/alice",
        ]))
        .await
        .expect("explicit SFTP route remains a file-transfer intent");
    let ExternalLaunchOutcome::PasswordRequired {
        target: sftp_target,
    } = sftp_outcome
    else {
        panic!("SFTP without a key requires an authentication prompt");
    };
    assert!(matches!(
        sftp_target.intent,
        ExternalLaunchIntent::SftpTransfer { .. }
    ));
    assert!(!sftp_target.open_sftp_after_connect);
}
