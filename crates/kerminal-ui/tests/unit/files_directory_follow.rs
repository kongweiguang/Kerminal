//! 跟随必须以最新目录意图收敛，开关取消与手动导航各自保有请求所有权。
//!
//! @author kongweiguang

use gpui_kit::TestAppContext;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    files::FileEndpoint,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

use crate::features::files::{action_test_support as support, finish_panel_load_request};

/// 固定已提交清单与可信 SSH 候选，使测试只控制输入顺序，不依赖实际网络时延。
///
/// @author kongweiguang
fn fixture(app: &mut TestAppContext, endpoint: FileEndpoint) -> support::FilesPaneFixture {
    support::entity_fixture(app, endpoint, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.current_terminal_binding = Some(UiSessionBinding {
            session_id: SessionId::new(),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            session_generation: 1,
        });
        support::seed_listing(pane, 1, 1, Vec::new());
    })
}

/// 同主机换终端和重连都应保留已显示清单，并让关闭跟随真正撤销新会话的自动读取。
///
/// @author kongweiguang
#[gpui_kit::test]
fn rebinding_same_host_keeps_listing_and_follow_cancellation(app: &mut TestAppContext) {
    for reconnect in [false, true] {
        let host = HostId::new();
        let a = support::remote_endpoint(&host, "/srv/a");
        let b = support::remote_endpoint(&host, "/srv/b");
        let fixture = fixture(app, a.clone());
        app.update(|cx| {
            fixture.pane.update(cx, |pane, cx| {
                let binding = UiSessionBinding {
                    session_id: SessionId::new(),
                    owner: SessionOwner::Ui {
                        tab_id: TabId::new(),
                        pane_id: PaneId::new(),
                    },
                    session_generation: 1,
                };
                pane.current_terminal_binding = Some(binding.clone());
                pane.set_follow_terminal_directory(true, cx);
                let next_binding = UiSessionBinding {
                    session_id: if reconnect {
                        binding.session_id.clone()
                    } else {
                        SessionId::new()
                    },
                    owner: binding.owner,
                    session_generation: 2,
                };
                pane.set_current_terminal_target(Some(b.clone()), None, Some(next_binding), cx);
                let generation = pane.panels[1].generation;
                let token = pane.panels[1]
                    .root_request_cancellation
                    .as_ref()
                    .unwrap()
                    .cancellation
                    .clone();
                pane.set_follow_terminal_directory(false, cx);
                assert!(token.is_cancelled(), "新会话的自动读取必须响应关闭跟随");
                assert!(!pane.panels[1].loading);
                assert_eq!(pane.panels[1].endpoint, a);
                assert_eq!(pane.panels[1].listing.as_ref().unwrap().directory, a);
                assert!(!finish_panel_load_request(
                    pane.panels[1].generation,
                    generation,
                    &mut pane.panels[1].requested_endpoint,
                    &b,
                ));
                pane.set_follow_terminal_directory(true, cx);
                assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&b));
            })
        });
    }
}

/// 重连上报相同 CWD 时仍需接管旧手动读取，不能因目录字符串相同而漏掉会话代次变化。
///
/// @author kongweiguang
#[gpui_kit::test]
fn reconnecting_same_directory_supersedes_manual_request(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let manual = support::remote_endpoint(&host, "/srv/manual");
    let fixture = fixture(app, a.clone());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.open_endpoint_on_side(1, manual, cx);
            let token = pane.panels[1]
                .root_request_cancellation
                .as_ref()
                .unwrap()
                .cancellation
                .clone();
            let mut binding = pane.current_terminal_binding.clone().unwrap();
            binding.session_generation += 1;
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding), cx);
            assert!(token.is_cancelled());
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&a));
            pane.set_follow_terminal_directory(false, cx);
            assert!(!pane.panels[1].loading);
            assert_eq!(pane.panels[1].listing.as_ref().unwrap().directory, a);
        })
    });
}

/// 文件 Tab 的父目录同步独立于隐藏的终端跟随开关；重复渲染同一文件仍保留手动浏览位置。
///
/// @author kongweiguang
#[gpui_kit::test]
fn file_tab_parent_navigation_ignores_hidden_follow_setting(app: &mut TestAppContext) {
    for (enabled, local) in [(false, false), (true, false), (false, true), (true, true)] {
        let host = HostId::new();
        let endpoint = |path: &str| {
            if local {
                FileEndpoint::Local {
                    path: std::path::PathBuf::from(format!("C:/work/{path}")),
                }
            } else {
                support::remote_endpoint(&host, &format!("/srv/{path}"))
            }
        };
        let a = endpoint("a");
        let b = endpoint("b");
        let manual = endpoint("manual");
        let fixture = fixture(app, a.clone());
        app.update(|cx| {
            fixture.pane.update(cx, |pane, cx| {
                pane.follow_terminal_directory = enabled;
                pane.set_current_terminal_target(Some(a.clone()), None, None, cx);
                pane.open_endpoint_on_side(1, manual.clone(), cx);
                pane.set_current_terminal_target(Some(a), None, None, cx);
                assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&manual));
                let token = pane.panels[1]
                    .root_request_cancellation
                    .as_ref()
                    .unwrap()
                    .cancellation
                    .clone();
                pane.set_current_terminal_target(Some(b.clone()), None, None, cx);
                assert!(token.is_cancelled());
                assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&b));
                assert!(pane.current_terminal_binding.is_none());
            })
        });
    }
}

/// 快速返回已显示目录也必须替换在途读取；否则 B 的慢成功会覆盖终端最终的 A。
///
/// @author kongweiguang
#[gpui_kit::test]
fn returning_to_committed_directory_supersedes_in_flight_follow(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let b = support::remote_endpoint(&host, "/srv/中文 dir");
    let fixture = fixture(app, a.clone());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(
                Some(b.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            let generation_b = pane.panels[1].generation;
            let token_b = pane.panels[1]
                .root_request_cancellation
                .as_ref()
                .unwrap()
                .cancellation
                .clone();
            pane.set_current_terminal_target(
                Some(a.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            assert!(token_b.is_cancelled());
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&a));
            assert!(!finish_panel_load_request(
                pane.panels[1].generation,
                generation_b,
                &mut pane.panels[1].requested_endpoint,
                &b,
            ));
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&a));
            let generation_a = pane.panels[1].generation;
            pane.set_current_terminal_target(
                Some(a.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            assert_eq!(
                pane.panels[1].generation, generation_a,
                "相同快照不能反复重启慢读取"
            );
        })
    });
}

/// 关闭自动跟随必须撤销 provider 和回执资格，已显示清单仍可继续浏览。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabling_follow_cancels_only_its_pending_navigation(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let b = support::remote_endpoint(&host, "/srv/b");
    let fixture = fixture(app, a.clone());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(
                Some(b.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            let generation = pane.panels[1].generation;
            let token = pane.panels[1]
                .root_request_cancellation
                .as_ref()
                .unwrap()
                .cancellation
                .clone();
            pane.set_follow_terminal_directory(false, cx);
            assert!(token.is_cancelled());
            assert!(!pane.panels[1].loading);
            assert_eq!(pane.panels[1].endpoint, a);
            assert_eq!(pane.panels[1].listing.as_ref().unwrap().directory, a);
            assert!(!finish_panel_load_request(
                pane.panels[1].generation,
                generation,
                &mut pane.panels[1].requested_endpoint,
                &b,
            ));
            assert!(pane.panels[1].directory_error.is_none());
            pane.set_follow_terminal_directory(true, cx);
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&b));
        })
    });
}

/// 用户接管导航后取消资格立即移交；关闭跟随不能终止用户刚提交的目录读取。
///
/// @author kongweiguang
#[gpui_kit::test]
fn manual_navigation_survives_disabling_follow(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let b = support::remote_endpoint(&host, "/srv/b");
    let manual = support::remote_endpoint(&host, "/srv/manual");
    let fixture = fixture(app, a);
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(
                Some(b),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            pane.open_endpoint_on_side(1, manual.clone(), cx);
            let generation = pane.panels[1].generation;
            let token = pane.panels[1]
                .root_request_cancellation
                .as_ref()
                .unwrap()
                .cancellation
                .clone();
            pane.set_follow_terminal_directory(false, cx);
            assert!(!token.is_cancelled());
            assert_eq!(pane.panels[1].generation, generation);
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&manual));
        })
    });
}

/// 关闭期间仅更新候选目录；重新开启立即以最新候选替换旧的手动在途导航。
///
/// @author kongweiguang
#[gpui_kit::test]
fn enabling_follow_uses_latest_target_even_when_old_listing_matches(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let manual = support::remote_endpoint(&host, "/srv/manual");
    let fixture = fixture(app, a.clone());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.open_endpoint_on_side(1, manual.clone(), cx);
            let token = pane.panels[1]
                .root_request_cancellation
                .as_ref()
                .unwrap()
                .cancellation
                .clone();
            pane.set_current_terminal_target(
                Some(a.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&manual));
            pane.set_follow_terminal_directory(true, cx);
            assert!(token.is_cancelled());
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&a));
        })
    });
}

/// 目标失效立即清空旧列表并取消读取；同主机恢复后重新读取，不接受退出终端的旧数据。
///
/// @author kongweiguang
#[gpui_kit::test]
fn unavailable_target_cancels_follow_and_recovery_rebinds(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let b = support::remote_endpoint(&host, "/srv/b");
    let fixture = fixture(app, a);
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(
                Some(b.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            let token = pane.panels[1]
                .root_request_cancellation
                .as_ref()
                .unwrap()
                .cancellation
                .clone();
            pane.set_current_terminal_target(None, None, pane.current_terminal_binding.clone(), cx);
            assert!(token.is_cancelled());
            assert!(pane.panels[1].listing.is_none());
            assert!(pane.current_terminal_endpoint.is_none());
            assert!(!pane.compact_target_is_available());
            pane.set_current_terminal_target(
                Some(b.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&b));
            assert!(pane.compact_target_is_available());
        })
    });
}

/// 失败状态在相同快照下不形成重试风暴；用户关开一次即可重试同目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_follow_waits_for_explicit_retry_and_toggle_can_retry_same_directory(
    app: &mut TestAppContext,
) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/srv/a");
    let fixture = fixture(app, a.clone());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.follow_terminal_directory = true;
            pane.panels[1].failed_directory = Some(a.clone());
            pane.panels[1].directory_error = Some("connection lost".to_owned());
            let generation = pane.panels[1].generation;
            pane.set_current_terminal_target(
                Some(a.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            assert_eq!(pane.panels[1].generation, generation);
            assert!(pane.panels[1].directory_error.is_some());
            pane.set_follow_terminal_directory(false, cx);
            pane.set_follow_terminal_directory(true, cx);
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&a));
            assert!(pane.panels[1].directory_error.is_none());
            assert!(pane.panels[1].loading);
        })
    });
}

/// 即使关闭跟随，换主机仍必须绑定新文件系统，不能继续操作上一服务器的列表。
///
/// @author kongweiguang
#[gpui_kit::test]
fn switching_hosts_binds_new_host_while_follow_is_off(app: &mut TestAppContext) {
    let a = support::remote_endpoint(&HostId::new(), "/srv/a");
    let b = support::remote_endpoint(&HostId::new(), "/srv/b");
    let fixture = fixture(app, a);
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(
                Some(b.clone()),
                None,
                pane.current_terminal_binding.clone(),
                cx,
            );
            assert!(!pane.follow_terminal_directory);
            assert_eq!(pane.panels[1].endpoint, b);
            assert!(pane.panels[1].listing.is_none());
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&b));
        })
    });
}
