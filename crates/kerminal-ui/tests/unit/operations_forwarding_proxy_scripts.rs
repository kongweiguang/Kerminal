// @author kongweiguang

/// 配置脚本保留实际 SOCKS URL，并将任意会话 ID 约束到安全路径字符集。
#[test]
fn setup_script_uses_proxy_url_and_safe_session_id() {
    let script = super::user_proxy_setup_script("session/with spaces", "socks5h://127.0.0.1:1080");

    assert!(script.contains("KERM_PROXY_URL='socks5h://127.0.0.1:1080'"));
    assert!(script.contains("KERM_ID='session_with_spaces'"));
    assert!(script.contains("$KERM_PROFILE.kerminal-backup."));
    assert!(script.contains("KERM_ENV_EOF"));
    assert!(
        script
            .contains("if value=\"$($pip_cmd config --user get global.proxy 2>/dev/null)\"; then")
    );
    assert!(script.contains("printf \"git %s %s\\\\n\""));
}

/// 撤销脚本只以安全 ID 定位已有状态，并保留恢复应用级配置的入口。
#[test]
fn undo_script_restores_saved_user_configuration() {
    let script = super::user_proxy_undo_script("session:one");

    assert!(script.contains("KERM_ID='session_one'"));
    assert!(script.contains("restore_git_config http.proxy"));
    assert!(script.contains("restore_npm_config https-proxy"));
    assert!(script.contains("rm -f \"$KERM_ENV\" \"$KERM_STATE\""));
}
