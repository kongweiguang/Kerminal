//! 标签恢复收据的纯合成测试；不创建 GPUI 窗口，也不冒充真实跨进程证据。
//! @author kongweiguang

use super::{bootstrap_evidence, valid_tab_control, validate_persistence_receipt};
use serde_json::{Value, json};

const NOTES: &str = "acme.notes/notes-editor";

/// 标签事实同时包含持久身份、进程内 viewId、renderer 实例和非空草稿。
fn tab(id: &str, draft: &str, view: &str, instance: &str) -> Value {
    json!({
        "tabId": id, "contentId": NOTES, "draft": draft, "viewId": view,
        "renderer": {
            "status": "Ready", "error": null, "plugin": "acme.notes", "view": "notes.editor",
            "instance": instance, "instanceGeneration": 1, "processGeneration": 4
        }
    })
}

/// 标签控件必须声明真实 Role::Tab 且具备当前绘制中的可点击正布局。
fn tab_control(id: &str) -> Value {
    json!({
        "selector": format!("workspace-tab-label-{id}"),
        "isTab": true, "role": "Tab", "accessibleName": "Workspace Notes",
        "layoutVisible": true, "supportsClick": true, "accessibilityDisabled": null,
        "bounds": {"x": 10, "y": 20, "width": 140, "height": 32}
    })
}

/// 点击记录绑定真实标签路径、点击后活动身份和同一 viewId。
fn activation(id: &str, draft: &str, view: &str) -> Value {
    json!({
        "tabId": id, "selector": format!("workspace-tab-label-{id}"),
        "action": "gpui-pointer-dispatch", "activeTabId": id, "draft": draft,
        "viewId": view, "tabControl": tab_control(id)
    })
}

/// Save 动作包含具体输入、旧状态、独立回读以及动作前后控件事实。
fn save(id: &str, input: &str, before: Value) -> Value {
    json!({
        "status": "confirmed", "version": "1.0.0", "tabId": id, "input": input,
        "beforeText": before, "beforeReadback": {"text": before},
        "observedText": input, "readback": "plugin-settings-service.state_get",
        "controlBeforeClick": save_button(), "controlAfterClick": save_button(),
        "instance": format!("instance-{id}"), "instanceGeneration": 1, "processGeneration": 4
    })
}

/// 保存控件复用普通按钮完整语义，防止验证器只看名字。
fn save_button() -> Value {
    json!({
        "selector": "external-button-root.2.0", "isButton": true, "accessibleName": "Save",
        "layoutVisible": true, "supportsClick": true, "accessibilityDisabled": null,
        "bounds": {"x": 10, "y": 60, "width": 80, "height": 28}
    })
}

/// 三代阶段使用各自真实会出现的动作集合；phase 1 保存两个标签，phase 2 只改变一个标签。
fn receipt(phase: u8) -> Value {
    let (before, after, activations, saves, target) = match phase {
        1 => (
            vec![
                tab("tab-a", "seed alpha", "view-a", "instance-a"),
                tab("tab-b", "seed gamma", "view-b", "instance-b"),
            ],
            vec![
                tab("tab-a", "seed alpha", "view-a", "instance-a"),
                tab("tab-b", "seed gamma", "view-b", "instance-b"),
            ],
            vec![
                activation("tab-a", "seed alpha", "view-a"),
                activation("tab-b", "seed gamma", "view-b"),
                activation("tab-a", "seed alpha", "view-a"),
            ],
            vec![
                save("tab-a", "seed alpha", Value::Null),
                save("tab-b", "seed beta", json!("seed alpha")),
            ],
            None,
        ),
        2 => (
            vec![
                tab("tab-a", "seed alpha", "view-a", "instance-a"),
                tab("tab-b", "seed beta", "view-b", "instance-b"),
            ],
            vec![
                tab("tab-a", "count alpha", "view-a", "instance-a"),
                tab("tab-b", "seed beta", "view-b", "instance-b"),
            ],
            vec![
                activation("tab-a", "seed alpha", "view-a"),
                activation("tab-b", "seed beta", "view-b"),
                activation("tab-a", "count alpha", "view-a"),
            ],
            Vec::new(),
            Some("tab-a"),
        ),
        _ => unreachable!("synthetic persistence phase"),
    };
    let mode = if phase == 1 { "seeded" } else { "restored" };
    let source = if phase == 1 {
        "new-content-tabs"
    } else {
        "workspace-restore"
    };
    json!({
        "schemaVersion": 1, "phase": phase, "mode": mode, "source": source,
        "createdNewTabs": phase == 1, "notesTabCount": 2,
        "tabsBeforeActions": before, "tabsAfterActions": after,
        "activationPasses": activations, "saveActions": saves,
        "checks": {
            "distinctTabIds": true, "distinctInstances": true, "distinctViewIds": true,
            "distinctDrafts": true, "stableViewIdPerTab": true,
            "nonTargetDraftsPreserved": target.is_none() || target == Some("tab-a")
        }
    })
}

/// 每个阶段的正例必须通过，否则后续负例无法区分具体字段回归。
#[test]
fn persistence_phases_accept_consistent_receipts() {
    assert!(validate_persistence_receipt(&bootstrap_evidence(), 0).is_ok());
    for phase in 1..=2 {
        assert!(
            validate_persistence_receipt(&receipt(phase), phase).is_ok(),
            "phase {phase}"
        );
    }
}

/// 旧四代形状的第三阶段必须在状态机处拒绝，不能静默接受已移除的回退代。
#[test]
fn persistence_rejects_removed_rollback_phase() {
    let mut rollback = receipt(2);
    rollback["phase"] = json!(3);
    assert!(validate_persistence_receipt(&rollback, 3).is_err());
}

/// 新 Tab ID、丢失标签、重复身份或重复 viewId 都不能被内容数量掩盖。
#[test]
fn persistence_rejects_new_missing_or_duplicate_tab_identities() {
    let mut changed = receipt(2);
    changed["tabsAfterActions"][1]["tabId"] = json!("tab-c");
    assert!(validate_persistence_receipt(&changed, 2).is_err());

    let mut missing = receipt(2);
    let first_after = missing["tabsAfterActions"][0].clone();
    missing["tabsAfterActions"] = json!([first_after]);
    assert!(validate_persistence_receipt(&missing, 2).is_err());

    let mut duplicate = receipt(2);
    duplicate["tabsBeforeActions"][1]["tabId"] = json!("tab-a");
    assert!(validate_persistence_receipt(&duplicate, 2).is_err());

    let mut duplicate_view = receipt(2);
    duplicate_view["tabsBeforeActions"][1]["viewId"] = json!("view-a");
    assert!(validate_persistence_receipt(&duplicate_view, 2).is_err());

    let mut duplicate_instance = receipt(2);
    duplicate_instance["tabsBeforeActions"][1]["renderer"]["instance"] = json!("instance-a");
    assert!(validate_persistence_receipt(&duplicate_instance, 2).is_err());

    let mut empty = receipt(2);
    empty["tabsBeforeActions"][1]["draft"] = json!("");
    assert!(validate_persistence_receipt(&empty, 2).is_err());
}

/// 恢复代只能改变 Count 目标草稿，非目标漂移或目标未变化都拒绝。
#[test]
fn persistence_requires_single_target_change_and_preserves_other_draft() {
    let mut non_target = receipt(2);
    non_target["tabsAfterActions"][1]["draft"] = json!("drifted");
    assert!(validate_persistence_receipt(&non_target, 2).is_err());

    let mut target_unchanged = receipt(2);
    target_unchanged["tabsAfterActions"][0]["draft"] = json!("seed alpha");
    assert!(validate_persistence_receipt(&target_unchanged, 2).is_err());

    let mut rebuilt = receipt(2);
    rebuilt["tabsAfterActions"][0]["viewId"] = json!("view-new");
    assert!(validate_persistence_receipt(&rebuilt, 2).is_err());

    let mut save_in_restore = receipt(2);
    save_in_restore["saveActions"] = json!([save("tab-a", "count alpha", json!("seed beta"))]);
    assert!(validate_persistence_receipt(&save_in_restore, 2).is_err());
}

/// Save 必须使用具体标签草稿、动作前不同状态和独立回读，空输入或旧值不算动作；
/// 三代契约里只有 1.0 种子代会保存，1.1 恢复代不得写回。
#[test]
fn persistence_requires_confirmed_local_save_actions() {
    let mut wrong_observed = receipt(1);
    wrong_observed["saveActions"][0]["observedText"] = json!("other");
    assert!(validate_persistence_receipt(&wrong_observed, 1).is_err());

    let mut stale = receipt(1);
    stale["saveActions"][0]["input"] = json!("seed beta");
    stale["saveActions"][0]["observedText"] = json!("seed beta");
    stale["saveActions"][0]["beforeText"] = json!("seed beta");
    assert!(validate_persistence_receipt(&stale, 1).is_err());

    let mut duplicate_input = receipt(1);
    duplicate_input["saveActions"][1]["input"] = json!("seed alpha");
    duplicate_input["saveActions"][1]["observedText"] = json!("seed alpha");
    assert!(validate_persistence_receipt(&duplicate_input, 1).is_err());

    let mut missing = receipt(1);
    let first_save = missing["saveActions"][0].clone();
    missing["saveActions"] = json!([first_save]);
    assert!(validate_persistence_receipt(&missing, 1).is_err());

    let mut wrong_version = receipt(1);
    wrong_version["saveActions"][0]["version"] = json!("1.1.0");
    assert!(validate_persistence_receipt(&wrong_version, 1).is_err());

    let mut global_fallback = receipt(1);
    global_fallback["tabsBeforeActions"][1]["draft"] = json!("seed beta");
    global_fallback["tabsAfterActions"][1]["draft"] = json!("seed beta");
    assert!(validate_persistence_receipt(&global_fallback, 1).is_err());

    let mut no_click = receipt(1);
    no_click["saveActions"][0]["controlAfterClick"]["supportsClick"] = json!(false);
    assert!(validate_persistence_receipt(&no_click, 1).is_err());
}

/// 标签必须经具体 selector 的真实点击记录覆盖，Role 或 viewId 不符不能由汇总布尔补偿。
#[test]
fn persistence_requires_real_tab_click_evidence() {
    let mut selector = receipt(2);
    selector["activationPasses"][0]["selector"] = json!("workspace-tab-hitbox-tab-a");
    assert!(validate_persistence_receipt(&selector, 2).is_err());

    let mut role = receipt(2);
    role["activationPasses"][0]["tabControl"]["isTab"] = json!(false);
    assert!(validate_persistence_receipt(&role, 2).is_err());

    let mut view = receipt(2);
    view["activationPasses"][0]["viewId"] = json!("view-other");
    assert!(validate_persistence_receipt(&view, 2).is_err());

    let mut one_tab = receipt(2);
    one_tab["activationPasses"] = json!([activation("tab-a", "seed alpha", "view-a")]);
    assert!(validate_persistence_receipt(&one_tab, 2).is_err());

    let mut missing_control = receipt(2);
    missing_control["activationPasses"][0]["tabControl"] = Value::Null;
    assert!(!valid_tab_control(
        &missing_control["activationPasses"][0]["tabControl"],
        "workspace-tab-label-tab-a"
    ));
}

/// 模式、来源和 bootstrap 空集合也必须一致，防止恢复代被标成新开或反过来。
#[test]
fn persistence_rejects_mode_source_and_bootstrap_mismatches() {
    let mut source = receipt(2);
    source["source"] = json!("new-content-tabs");
    assert!(validate_persistence_receipt(&source, 2).is_err());

    let mut created = receipt(2);
    created["createdNewTabs"] = json!(true);
    assert!(validate_persistence_receipt(&created, 2).is_err());

    let mut mode = receipt(1);
    mode["mode"] = json!("restored");
    assert!(validate_persistence_receipt(&mode, 1).is_err());

    let mut bootstrap = bootstrap_evidence();
    bootstrap["tabsBeforeActions"] = json!([tab("tab-a", "draft", "view-a", "instance-a")]);
    assert!(validate_persistence_receipt(&bootstrap, 0).is_err());
}
