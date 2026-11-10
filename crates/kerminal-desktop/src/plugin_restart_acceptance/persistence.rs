//! 四代重启验收的真实标签草稿、跨代恢复和本地保存证据。
//!
//! 这里只通过正式工作区、NativeRenderer、GPUI 指针事件和插件设置端口观察结果；
//! 新进程是否恢复由上一代收据与下一代 Tab ID/草稿的精确衔接证明。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use gpui_kit::{AnyWindowHandle, AsyncApp, ElementId, Entity, Role, Window};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, plugin_settings::PluginSettingsService,
};
use kerminal_domain::{identity::TabId, plugins::PluginId};
use kerminal_ui::RootView;
use serde_json::{Value, json};

use super::{
    NOTES_FIELD, POLL, SAVE_BUTTON, WAIT, active_renderer, control_evidence, inspect_capability,
    io_error, renderer_identity,
};

const NOTES_CONTENT: &str = "acme.notes/notes-editor";
const NOTES_PLUGIN: &str = "acme.notes";
const NOTES_VIEW: &str = "notes.editor";
const SEED_ALPHA: &str = "P7 恢复草稿 A 猫";
const SEED_BETA: &str = "P7 恢复草稿 B 🙂";
// 第二个标签保存后再留下未保存草稿，使两个恢复草稿都不同于插件全局状态，排除跨 Tab 回退到全局值。
const SEED_GAMMA: &str = "P7 恢复草稿 C 猫🙂";
const SAVE_READBACK: &str = "plugin-settings-service.state_get";

/// 验收失败详情只写隔离目录；给每步附加稳定阶段名，避免公开文案掩盖真实等待点。
fn stage(error: AppError, name: &'static str) -> AppError {
    let detail = super::error_chain(&error);
    error.with_source(std::io::Error::other(format!("{name}: {detail}")))
}

/// 已创建的标签以 Tab ID 为跨进程身份；viewId 和实例代次只用于本进程内串线检查。
#[derive(Clone, Debug)]
struct CapturedTab {
    tab_id: String,
    draft: String,
    view_id: String,
    instance: String,
    instance_generation: u64,
    process_generation: u64,
}

impl CapturedTab {
    /// 收据字段保持原始值，脚本再独立与上一代哈希外的 Tab ID/草稿比对。
    fn to_value(&self) -> Value {
        json!({
            "tabId": self.tab_id,
            "contentId": NOTES_CONTENT,
            "draft": self.draft,
            "viewId": self.view_id,
            "renderer": {
                "status": "Ready",
                "error": Value::Null,
                "plugin": NOTES_PLUGIN,
                "view": NOTES_VIEW,
                "instance": self.instance,
                "instanceGeneration": self.instance_generation,
                "processGeneration": self.process_generation
            }
        })
    }
}

/// bootstrap 代没有插件标签；保留同一结构让脚本必须看到明确的空集合。
pub(super) fn bootstrap_evidence() -> Value {
    json!({
        "schemaVersion": 1,
        "phase": 0,
        "mode": "not-applicable-bootstrap",
        "source": "bootstrap",
        "createdNewTabs": false,
        "notesTabCount": 0,
        "tabsBeforeActions": [],
        "tabsAfterActions": [],
        "activationPasses": [],
        "saveActions": [],
        "checks": {"bootstrap": true}
    })
}

/// 每代都在正式视图上完成创建、恢复、激活和版本动作，最后返回活动快照与能力证据。
pub(super) async fn exercise_generation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    settings: &PluginSettingsService,
    phase: u8,
    version: &str,
) -> AppResult<(Value, Value, Value)> {
    match phase {
        1 => seed_two_tabs(cx, root, window, executor, settings, version).await,
        2 => restore_and_count(cx, root, window, executor, settings, version).await,
        3 => restore_and_save(cx, root, window, executor, settings, version).await,
        _ => Err(AppError::invalid("标签恢复验收阶段无效")),
    }
}

/// 1.0 代实际创建两个标签、分别输入并点击 Save；回执中的标签身份供后续进程比对。
async fn seed_two_tabs(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    settings: &PluginSettingsService,
    version: &str,
) -> AppResult<(Value, Value, Value)> {
    let first = open_notes(root, window, cx)?;
    let first_id = first.as_str().to_owned();
    wait_active_note_ready(cx, root, window, &first_id)
        .await
        .map_err(|error| stage(error, "seed:first-ready"))?;
    edit_and_wait(cx, root, window, &first_id, SEED_ALPHA)
        .await
        .map_err(|error| stage(error, "seed:first-edit"))?;
    let first_save = save_active(
        cx, root, window, executor, settings, version, &first_id, SEED_ALPHA,
    )
    .await
    .map_err(|error| stage(error, "seed:first-save"))?;

    let second = open_notes(root, window, cx)?;
    let second_id = second.as_str().to_owned();
    if second_id == first_id {
        return Err(AppError::conflict("新建第二个笔记标签复用了首个标签身份"));
    }
    wait_active_note_ready(cx, root, window, &second_id)
        .await
        .map_err(|error| stage(error, "seed:second-ready"))?;
    edit_and_wait(cx, root, window, &second_id, SEED_BETA)
        .await
        .map_err(|error| stage(error, "seed:second-edit"))?;
    let second_save = save_active(
        cx, root, window, executor, settings, version, &second_id, SEED_BETA,
    )
    .await
    .map_err(|error| stage(error, "seed:second-save"))?;
    // 保留一个未保存草稿，使次要标签的恢复值也不同于最后保存的全局插件状态。
    edit_and_wait(cx, root, window, &second_id, SEED_GAMMA)
        .await
        .map_err(|error| stage(error, "seed:gamma-edit"))?;

    let mut activations = Vec::new();
    let (_, activation) = activate_tab(cx, root, window, &first_id, Some(SEED_ALPHA))
        .await
        .map_err(|error| stage(error, "seed:activate-first"))?;
    activations.push(activation);
    let (_, activation) = activate_tab(cx, root, window, &second_id, Some(SEED_GAMMA))
        .await
        .map_err(|error| stage(error, "seed:activate-second"))?;
    activations.push(activation);
    let (active, activation) = activate_tab(cx, root, window, &first_id, Some(SEED_ALPHA))
        .await
        .map_err(|error| stage(error, "seed:return-first"))?;
    activations.push(activation);

    let before = capture_tabs(&active)?;
    let capability = inspect_capability(cx, root, window, &active, version)
        .await
        .map_err(|error| stage(error, "seed:capability"))?;
    let final_snapshot = wait_active_note(cx, root, window, Some(&first_id), Some(SEED_ALPHA))
        .await
        .map_err(|error| stage(error, "seed:final-wait"))?;
    let after = capture_tabs(&final_snapshot)?;
    let receipt = persistence_receipt(
        1,
        "seeded",
        "new-content-tabs",
        true,
        &before,
        &after,
        activations,
        vec![first_save, second_save],
        checks(&before, &after, None),
    )?;
    Ok((final_snapshot, receipt, capability))
}

/// 1.1 代禁止新建标签；先按工作区恢复两个标签，再只对其中一个执行真实 Count。
async fn restore_and_count(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    _executor: &ApplicationTaskExecutor,
    _settings: &PluginSettingsService,
    version: &str,
) -> AppResult<(Value, Value, Value)> {
    let restored = wait_restored_tabs(cx, root, window).await?;
    let ids = note_tab_ids(&restored)?;
    let first_id = ids[0].clone();
    let second_id = ids[1].clone();

    let mut activations = Vec::new();
    let (first_initial, activation) = activate_tab(cx, root, window, &first_id, None).await?;
    activations.push(activation);
    let first_draft = active_note_draft(&first_initial)?.to_owned();
    let (second_initial, activation) = activate_tab(cx, root, window, &second_id, None).await?;
    activations.push(activation);
    let second_draft = active_note_draft(&second_initial)?.to_owned();
    if first_draft == second_draft {
        return Err(AppError::conflict("恢复后的两个笔记标签草稿相同"));
    }
    let (active, activation) =
        activate_tab(cx, root, window, &first_id, Some(&first_draft)).await?;
    activations.push(activation);
    let before = capture_tabs(&active)?;

    let capability = inspect_capability(cx, root, window, &active, version).await?;
    if capability["behavior"]["tabId"].as_str() != Some(first_id.as_str()) {
        return Err(AppError::conflict("Count 动作不属于恢复的首个标签"));
    }
    let counted = wait_active_note(cx, root, window, Some(&first_id), None).await?;
    let counted_draft = active_note_draft(&counted)?.to_owned();
    if counted_draft == first_draft {
        return Err(AppError::conflict("Count 动作未形成可区分的新草稿"));
    }
    // Count 的每次输入都记录在能力收据中；最终草稿必须等于最后一次真实输入，才能把点击结果与跨代恢复值绑定。
    let last_count_input = capability["behavior"]["cases"]
        .as_array()
        .and_then(|cases| cases.last())
        .and_then(|case| case["input"].as_str())
        .filter(|input| !input.is_empty())
        .ok_or_else(|| AppError::conflict("Count 动作输入收据缺失"))?;
    if counted_draft != last_count_input {
        return Err(AppError::conflict("Count 最终输入与恢复草稿不一致"));
    }
    let (_, activation) = activate_tab(cx, root, window, &second_id, Some(&second_draft)).await?;
    activations.push(activation);
    let (final_snapshot, activation) =
        activate_tab(cx, root, window, &first_id, Some(&counted_draft)).await?;
    activations.push(activation);
    let after = capture_tabs(&final_snapshot)?;
    let receipt = persistence_receipt(
        2,
        "restored",
        "workspace-restore",
        false,
        &before,
        &after,
        activations,
        Vec::new(),
        checks(&before, &after, Some(&first_id)),
    )?;
    Ok((final_snapshot, receipt, capability))
}

/// 回退代先确认两个旧标签未新建，再通过真实 Save 按钮写入不依赖网络的插件状态。
async fn restore_and_save(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    settings: &PluginSettingsService,
    version: &str,
) -> AppResult<(Value, Value, Value)> {
    let restored = wait_restored_tabs(cx, root, window).await?;
    let ids = note_tab_ids(&restored)?;
    let first_id = ids[0].clone();
    let second_id = ids[1].clone();

    let mut activations = Vec::new();
    let (first_initial, activation) = activate_tab(cx, root, window, &first_id, None).await?;
    activations.push(activation);
    let first_draft = active_note_draft(&first_initial)?.to_owned();
    let (second_initial, activation) = activate_tab(cx, root, window, &second_id, None).await?;
    activations.push(activation);
    let second_draft = active_note_draft(&second_initial)?.to_owned();
    if first_draft == second_draft {
        return Err(AppError::conflict("回退后的两个笔记标签草稿相同"));
    }
    let (active, activation) =
        activate_tab(cx, root, window, &first_id, Some(&first_draft)).await?;
    activations.push(activation);
    let before = capture_tabs(&active)?;

    let capability = inspect_capability(cx, root, window, &active, version).await?;
    let save = save_active(
        cx,
        root,
        window,
        executor,
        settings,
        version,
        &first_id,
        &first_draft,
    )
    .await?;
    let (_, activation) = activate_tab(cx, root, window, &second_id, Some(&second_draft)).await?;
    activations.push(activation);
    let (final_snapshot, activation) =
        activate_tab(cx, root, window, &first_id, Some(&first_draft)).await?;
    activations.push(activation);
    let after = capture_tabs(&final_snapshot)?;
    let receipt = persistence_receipt(
        3,
        "restored",
        "workspace-restore",
        false,
        &before,
        &after,
        activations,
        vec![save],
        checks(&before, &after, None),
    )?;
    Ok((final_snapshot, receipt, capability))
}

/// 打开入口仍是正式工作区事务；调用方只接受返回的稳定 Tab ID。
fn open_notes(
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> AppResult<TabId> {
    window
        .update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.external_plugin_acceptance_open_notes(window, cx)
            })
        })
        .map_err(io_error)?
}

/// 已存在标签只通过真实标签控件点击激活，隐藏实例由正式恢复路径按需创建。
async fn activate_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab_id: &str,
    expected: Option<&str>,
) -> AppResult<(Value, Value)> {
    let selector = format!("workspace-tab-label-{tab_id}");
    let control = wait_tab_control(cx, window, &selector).await?;
    crate::plugin_acceptance::click_selector(cx, window, &selector, false)
        .map_err(|error| io_error(std::io::Error::other(error.to_string())))?;
    let snapshot = wait_active_note(cx, root, window, Some(tab_id), expected).await?;
    let draft = active_note_draft(&snapshot)?.to_owned();
    let view_id = active_tab(&snapshot)
        .and_then(|tab| tab["viewId"].as_str())
        .filter(|view| !view.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AppError::conflict("激活标签缺少 viewId"))?;
    Ok((
        snapshot,
        json!({
            "tabId": tab_id,
            "selector": selector,
            "action": "gpui-pointer-dispatch",
            "activeTabId": tab_id,
            "draft": draft,
            "viewId": view_id,
            "tabControl": control
        }),
    ))
}

/// 输入后必须等 renderer 数据与真实 textarea 同时更新，避免只改投影就进入保存。
async fn edit_and_wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab_id: &str,
    draft: &str,
) -> AppResult<Value> {
    crate::plugin_acceptance::edit_input(cx, window, NOTES_FIELD, draft)
        .map_err(|error| io_error(std::io::Error::other(error.to_string())))?;
    wait_active_note(cx, root, window, Some(tab_id), Some(draft)).await
}

/// Save 成功以插件设置服务独立回读为准，并记录动作前后控件与旧状态以拒绝未触发。
async fn save_active(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    settings: &PluginSettingsService,
    version: &str,
    tab_id: &str,
    input: &str,
) -> AppResult<Value> {
    crate::plugin_acceptance::wait_selector(cx, window, SAVE_BUTTON)
        .await
        .map_err(|error| io_error(std::io::Error::other(error.to_string())))?;
    let before = read_plugin_state(executor, settings).await?;
    let before_text = before
        .as_ref()
        .and_then(|state| state["text"].as_str())
        .map(str::to_owned);
    if before_text.as_deref() == Some(input) {
        return Err(AppError::conflict("Save 动作前插件状态已经是目标文本"));
    }
    let control_before = window
        .update(cx, |_, window, _| save_control(window))
        .map_err(io_error)??;
    crate::plugin_acceptance::click_selector(cx, window, SAVE_BUTTON, false)
        .map_err(|error| io_error(std::io::Error::other(error.to_string())))?;
    let observed = wait_plugin_state(cx, window, executor, settings, input).await?;
    // 插件状态回读可能早于宿主下一帧；显式推进绘制后再记录控件，避免把点击前快照标成动作后事实。
    window
        .update(cx, |_, window, app| {
            crate::plugin_acceptance::draw(window, app)
        })
        .map_err(io_error)?;
    let control_after = window
        .update(cx, |_, window, _| save_control(window))
        .map_err(io_error)??;
    let snapshot = wait_active_note(cx, root, window, Some(tab_id), Some(input)).await?;
    let identity = renderer_identity(&snapshot)?;
    if identity.tab != tab_id {
        return Err(AppError::conflict("Save 完成后活动标签身份改变"));
    }
    Ok(json!({
        "status": "confirmed",
        "version": version,
        "tabId": tab_id,
        "input": input,
        "beforeText": before_text,
        "beforeReadback": before.unwrap_or(Value::Null),
        "observedText": observed["text"].clone(),
        "readback": SAVE_READBACK,
        "controlBeforeClick": control_before,
        "controlAfterClick": control_after,
        "instance": identity.instance,
        "instanceGeneration": identity.instance_generation,
        "processGeneration": identity.process_generation
    }))
}

/// Save 按钮是普通控件，仍要求当前绘制中的角色、名称、可点击与正布局。
fn save_control(window: &Window) -> AppResult<Value> {
    control_evidence(window, SAVE_BUTTON)
}

/// 状态读取走共享应用执行器；收据记录实际返回对象，不用 toast 或忙碌图标代偿。
async fn read_plugin_state(
    executor: &ApplicationTaskExecutor,
    settings: &PluginSettingsService,
) -> AppResult<Option<Value>> {
    let settings = settings.clone();
    let plugin = PluginId::parse(NOTES_PLUGIN).map_err(io_error)?;
    executor
        .run(move |cancel| async move { settings.state_get(&plugin, cancel).await })
        .await
}

/// 回读必须出现精确文本；超时保留最后状态并阻止后续退出证据被误算成功。
async fn wait_plugin_state(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    settings: &PluginSettingsService,
    expected: &str,
) -> AppResult<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = read_plugin_state(executor, settings).await?;
        if state
            .as_ref()
            .is_some_and(|state| state["text"] == expected)
        {
            return Ok(state.unwrap_or(Value::Null));
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("保存后插件状态回读超时").with_source(
                std::io::Error::other(state.unwrap_or(Value::Null).to_string()),
            ));
        }
        window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::draw(window, app)
            })
            .map_err(io_error)?;
        cx.background_executor().timer(POLL).await;
    }
}

/// 恢复等待只检查工作区身份和两个笔记标签，renderer 由随后真实点击按需创建。
async fn wait_restored_tabs(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> AppResult<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::checked_isolation(window).map_err(io_error)?;
                crate::plugin_acceptance::draw(window, app);
                Ok::<_, AppError>(root.update(app, |root, cx| {
                    root.external_plugin_acceptance_snapshot(window, cx)
                }))
            })
            .map_err(io_error)??;
        if snapshot["workspaceReady"] == true
            && snapshot["settingsReady"] == true
            && note_tabs(&snapshot).is_ok()
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待工作区恢复两个笔记标签")
                .with_source(std::io::Error::other(snapshot.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 新建标签允许空草稿，但仍要求活动 renderer、具体 Tab 和 textarea 投影一致。
async fn wait_active_note_ready(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab_id: &str,
) -> AppResult<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::checked_isolation(window).map_err(io_error)?;
                crate::plugin_acceptance::draw(window, app);
                Ok::<_, AppError>(root.update(app, |root, cx| {
                    root.external_plugin_acceptance_snapshot(window, cx)
                }))
            })
            .map_err(io_error)??;
        let ready = active_tab(&snapshot).is_some_and(|tab| {
            tab["contentId"] == NOTES_CONTENT
                && snapshot["activeTabId"] == tab_id
                && active_note_draft(&snapshot).is_ok()
        });
        if ready {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待新建活动笔记标签")
                .with_source(std::io::Error::other(snapshot.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 等待活动笔记 renderer 的文本、textarea 和版本身份同时稳定，防止旧绘制满足新动作。
async fn wait_active_note(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab_id: Option<&str>,
    expected: Option<&str>,
) -> AppResult<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::checked_isolation(window).map_err(io_error)?;
                crate::plugin_acceptance::draw(window, app);
                Ok::<_, AppError>(root.update(app, |root, cx| {
                    root.external_plugin_acceptance_snapshot(window, cx)
                }))
            })
            .map_err(io_error)??;
        let ready = active_tab(&snapshot).is_some_and(|tab| {
            let identity = renderer_identity(&snapshot).is_ok();
            let text_ready = active_note_draft(&snapshot).is_ok_and(|draft| {
                !draft.is_empty() && expected.is_none_or(|expected| draft == expected)
            });
            identity
                && text_ready
                && tab["contentId"] == NOTES_CONTENT
                && tab_id.is_none_or(|tab_id| snapshot["activeTabId"] == tab_id)
        });
        if ready {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待活动笔记标签草稿")
                .with_source(std::io::Error::other(snapshot.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 标签控件是真实 Role::Tab 节点；标签名称可重复，因此身份仍以路径里的稳定 ID 为准。
fn tab_control_evidence(window: &Window, selector: &str) -> AppResult<Value> {
    let id: ElementId = selector.to_owned().into();
    let matches = gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .filter(|element| element.path().last() == Some(&id))
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(AppError::conflict("工作区标签控件身份不唯一"));
    }
    let Some(element) = matches.first() else {
        return Ok(Value::Null);
    };
    let bounds = element.bounds();
    Ok(json!({
        "selector": selector,
        "isTab": element.role() == Some(Role::Tab),
        "role": element.role().map(|role| format!("{role:?}")),
        "accessibleName": element.label(),
        "layoutVisible": element.visible(),
        "supportsClick": element.supports_click(),
        "accessibilityDisabled": element.disabled(),
        "bounds": {
            "x": bounds.origin.x.as_f32(), "y": bounds.origin.y.as_f32(),
            "width": bounds.size.width.as_f32(), "height": bounds.size.height.as_f32()
        }
    }))
}

/// 标签必须在当前绘制中可见且可点击；缺失或重复都拒绝，不能以快照里的 Tab ID 代替控件。
async fn wait_tab_control(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> AppResult<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let control = window
            .update(cx, |_, window, app| {
                crate::plugin_acceptance::checked_isolation(window).map_err(io_error)?;
                crate::plugin_acceptance::draw(window, app);
                tab_control_evidence(window, selector)
            })
            .map_err(io_error)??;
        if valid_tab_control(&control, selector) {
            return Ok(control);
        }
        if Instant::now() >= deadline {
            return Err(AppError::timeout("等待可点击的工作区标签控件")
                .with_source(std::io::Error::other(control.to_string())));
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// Role::Tab 与 Click 能力、禁用状态和有限正布局同时成立才算当前标签可操作。
pub(super) fn valid_tab_control(value: &Value, selector: &str) -> bool {
    value["selector"] == selector
        && value["isTab"] == true
        && value["layoutVisible"] == true
        && value["supportsClick"] == true
        && matches!(
            value.get("accessibilityDisabled"),
            Some(Value::Null | Value::Bool(false))
        )
        && value["accessibleName"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
        && ["x", "y", "width", "height"].into_iter().all(|key| {
            value["bounds"][key].as_f64().is_some_and(|number| {
                number.is_finite() && (!matches!(key, "width" | "height") || number > 0.0)
            })
        })
}

/// 只返回工作区中两个笔记内容标签；重复或数量不符直接失败，避免以空白新标签补位。
fn note_tabs(snapshot: &Value) -> AppResult<Vec<&Value>> {
    let tabs = snapshot["tabs"]
        .as_array()
        .ok_or_else(|| AppError::conflict("工作区标签快照缺失"))?;
    let notes = tabs
        .iter()
        .filter(|tab| tab["contentId"] == NOTES_CONTENT)
        .collect::<Vec<_>>();
    if notes.len() != 2 {
        return Err(AppError::conflict("笔记标签数量不是两个"));
    }
    let mut ids = BTreeSet::new();
    for tab in &notes {
        let id = tab["tabId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AppError::conflict("笔记标签身份缺失"))?;
        if !ids.insert(id) {
            return Err(AppError::conflict("笔记标签身份重复"));
        }
    }
    Ok(notes)
}

/// 标签 ID 顺序来自工作区快照；脚本随后用上一代集合验证没有新开标签。
fn note_tab_ids(snapshot: &Value) -> AppResult<Vec<String>> {
    note_tabs(snapshot)?
        .into_iter()
        .map(|tab| {
            tab["tabId"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| AppError::conflict("笔记标签身份缺失"))
        })
        .collect()
}

/// 活动标签必须属于两份笔记，不能由其他内容类型满足 renderer 等待。
fn active_tab(snapshot: &Value) -> Option<&Value> {
    let active = snapshot["activeTabId"].as_str()?;
    snapshot["tabs"]
        .as_array()?
        .iter()
        .find(|tab| tab["tabId"] == active && tab["contentId"] == NOTES_CONTENT)
}

/// 数据与真实 textarea 一致才构成可见草稿；隐藏标签也使用同一判据。
fn active_note_draft(snapshot: &Value) -> AppResult<&str> {
    let (_, renderer) = active_renderer(snapshot)?;
    let text = renderer["data"]["text"]
        .as_str()
        .ok_or_else(|| AppError::conflict("笔记 renderer 文本缺失"))?;
    let field_matches = renderer["fields"].as_array().is_some_and(|fields| {
        fields.iter().any(|field| {
            field["binding"] == "text" && field["kind"] == "textarea" && field["value"] == text
        })
    });
    if !field_matches {
        return Err(AppError::conflict("笔记投影文本与输入控件不一致"));
    }
    Ok(text)
}

/// 每个标签的状态都从自己的 renderer 读取；不能复用活动标签数据制造隔离假象。
fn capture_tabs(snapshot: &Value) -> AppResult<Vec<CapturedTab>> {
    note_tabs(snapshot)?
        .into_iter()
        .map(|tab| {
            let tab_id = tab["tabId"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AppError::conflict("笔记标签身份缺失"))?
                .to_owned();
            let view_id = tab["viewId"]
                .as_str()
                .filter(|view| !view.is_empty())
                .ok_or_else(|| AppError::conflict("笔记标签缺少实例 viewId"))?
                .to_owned();
            let renderer = &tab["renderer"];
            if renderer["status"] != "Ready"
                || renderer["error"] != Value::Null
                || renderer["plugin"] != NOTES_PLUGIN
                || renderer["view"] != NOTES_VIEW
            {
                return Err(AppError::conflict("笔记标签 renderer 身份不可用"));
            }
            let instance = renderer["instance"]
                .as_str()
                .filter(|instance| !instance.is_empty())
                .ok_or_else(|| AppError::conflict("笔记标签实例身份缺失"))?
                .to_owned();
            let instance_generation = renderer["instanceGeneration"]
                .as_u64()
                .ok_or_else(|| AppError::conflict("笔记标签实例代次缺失"))?;
            let process_generation = renderer["processGeneration"]
                .as_u64()
                .filter(|generation| *generation > 0)
                .ok_or_else(|| AppError::conflict("笔记标签进程代次缺失"))?;
            let text = renderer["data"]["text"]
                .as_str()
                .filter(|text| !text.is_empty())
                .ok_or_else(|| AppError::conflict("笔记标签草稿为空"))?;
            let field_matches = renderer["fields"].as_array().is_some_and(|fields| {
                fields.iter().any(|field| {
                    field["binding"] == "text"
                        && field["kind"] == "textarea"
                        && field["value"] == text
                })
            });
            if !field_matches {
                return Err(AppError::conflict("笔记标签草稿与输入控件不一致"));
            }
            Ok(CapturedTab {
                tab_id,
                draft: text.to_owned(),
                view_id,
                instance,
                instance_generation,
                process_generation,
            })
        })
        .collect()
}

/// 隔离检查同时覆盖 Tab/实例/viewId 和草稿，Count 变化只能落在指定标签。
fn checks(before: &[CapturedTab], after: &[CapturedTab], target: Option<&str>) -> Value {
    let unique = |values: Vec<&str>| values.into_iter().collect::<BTreeSet<_>>().len() == 2;
    let stable_views = before
        .iter()
        .zip(after)
        .all(|(before, after)| before.tab_id == after.tab_id && before.view_id == after.view_id);
    let non_target_preserved = target.is_none_or(|target| {
        after.iter().all(|after| {
            after.tab_id == target
                || before
                    .iter()
                    .find(|before| before.tab_id == after.tab_id)
                    .is_some_and(|before| before.draft == after.draft)
        })
    });
    json!({
        "distinctTabIds": unique(before.iter().map(|tab| tab.tab_id.as_str()).collect()),
        "distinctInstances": unique(before.iter().map(|tab| tab.instance.as_str()).collect()),
        "distinctViewIds": unique(before.iter().map(|tab| tab.view_id.as_str()).collect()),
        "distinctDrafts": unique(before.iter().map(|tab| tab.draft.as_str()).collect())
            && unique(after.iter().map(|tab| tab.draft.as_str()).collect()),
        "stableViewIdPerTab": stable_views,
        "nonTargetDraftsPreserved": non_target_preserved
    })
}

/// 构造后立即用同一严格判据自检；测试和脚本再分别覆盖收据篡改与跨代衔接。
fn persistence_receipt(
    phase: u8,
    mode: &str,
    source: &str,
    created_new_tabs: bool,
    before: &[CapturedTab],
    after: &[CapturedTab],
    activations: Vec<Value>,
    save_actions: Vec<Value>,
    checks: Value,
) -> AppResult<Value> {
    let receipt = json!({
        "schemaVersion": 1,
        "phase": phase,
        "mode": mode,
        "source": source,
        "createdNewTabs": created_new_tabs,
        "notesTabCount": before.len(),
        "tabsBeforeActions": before.iter().map(CapturedTab::to_value).collect::<Vec<_>>(),
        "tabsAfterActions": after.iter().map(CapturedTab::to_value).collect::<Vec<_>>(),
        "activationPasses": activations,
        "saveActions": save_actions,
        "checks": checks
    });
    validate_persistence_receipt(&receipt, phase)?;
    Ok(receipt)
}

/// 收据必须能用具体 Tab ID、草稿、控件和回读自洽解释，布尔汇总不能掩盖数组矛盾。
pub(super) fn validate_persistence_receipt(receipt: &Value, phase: u8) -> AppResult<()> {
    if receipt["schemaVersion"] != 1 || receipt["phase"] != phase {
        return Err(AppError::conflict("标签恢复收据身份不符"));
    }
    if phase == 0 {
        if receipt["mode"] != "not-applicable-bootstrap"
            || receipt["source"] != "bootstrap"
            || receipt["createdNewTabs"] != false
            || receipt["notesTabCount"] != 0
            || [
                "tabsBeforeActions",
                "tabsAfterActions",
                "activationPasses",
                "saveActions",
            ]
            .into_iter()
            .any(|key| {
                receipt[key]
                    .as_array()
                    .is_none_or(|items| !items.is_empty())
            })
        {
            return Err(AppError::conflict("bootstrap 标签恢复收据不为空"));
        }
        return Ok(());
    }
    let expected_mode = if phase == 1 { "seeded" } else { "restored" };
    let expected_source = if phase == 1 {
        "new-content-tabs"
    } else {
        "workspace-restore"
    };
    if receipt["mode"] != expected_mode
        || receipt["source"] != expected_source
        || receipt["createdNewTabs"] != (phase == 1)
        || receipt["notesTabCount"] != 2
    {
        return Err(AppError::conflict("标签恢复模式或来源与本代不符"));
    }
    let before = validated_tabs(&receipt["tabsBeforeActions"])?;
    let after = validated_tabs(&receipt["tabsAfterActions"])?;
    if before
        .iter()
        .map(|tab| tab.tab_id.as_str())
        .collect::<BTreeSet<_>>()
        != after
            .iter()
            .map(|tab| tab.tab_id.as_str())
            .collect::<BTreeSet<_>>()
    {
        return Err(AppError::conflict("跨动作前后的标签身份集合改变"));
    }
    if before.iter().any(|tab| {
        after
            .iter()
            .find(|after| after.tab_id == tab.tab_id)
            .is_none()
    }) {
        return Err(AppError::conflict("动作后丢失已恢复标签"));
    }
    if !unique_tabs(&before) || !unique_tabs(&after) {
        return Err(AppError::conflict("标签身份、实例、viewId 或草稿不唯一"));
    }
    for tab in &before {
        if after
            .iter()
            .find(|after| after.tab_id == tab.tab_id)
            .is_none_or(|after| after.view_id != tab.view_id)
        {
            return Err(AppError::conflict("标签在动作期间重建了 viewId"));
        }
    }

    let activations = receipt["activationPasses"]
        .as_array()
        .ok_or_else(|| AppError::conflict("标签点击记录缺失"))?;
    let view_by_tab = before
        .iter()
        .map(|tab| (tab.tab_id.as_str(), tab.view_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut activated = BTreeSet::new();
    for activation in activations {
        let tab_id = activation["tabId"]
            .as_str()
            .filter(|id| view_by_tab.contains_key(id))
            .ok_or_else(|| AppError::conflict("点击记录指向未知标签"))?;
        let selector = format!("workspace-tab-label-{tab_id}");
        if activation["selector"] != selector
            || activation["action"] != "gpui-pointer-dispatch"
            || activation["activeTabId"] != tab_id
            || activation["viewId"] != view_by_tab[tab_id]
            || activation["draft"]
                .as_str()
                .is_none_or(|draft| draft.is_empty())
            || !valid_tab_control(&activation["tabControl"], &selector)
        {
            return Err(AppError::conflict("标签点击记录与真实控件身份不符"));
        }
        activated.insert(tab_id);
    }
    if activated.len() != 2 {
        return Err(AppError::conflict("两个恢复标签未分别经过真实点击"));
    }

    let save_actions = receipt["saveActions"]
        .as_array()
        .ok_or_else(|| AppError::conflict("Save 动作记录缺失"))?;
    let before_by_id = before
        .iter()
        .map(|tab| (tab.tab_id.as_str(), tab))
        .collect::<BTreeMap<_, _>>();
    let mut saved_tabs = BTreeSet::new();
    for action in save_actions {
        let tab_id = action["tabId"]
            .as_str()
            .filter(|id| before_by_id.contains_key(id))
            .ok_or_else(|| AppError::conflict("Save 动作指向未知标签"))?;
        let input = action["input"]
            .as_str()
            .filter(|input| !input.is_empty())
            .ok_or_else(|| AppError::conflict("Save 动作输入为空"))?;
        if action["status"] != "confirmed"
            || action["version"] != "1.0.0"
            || action["observedText"] != input
            || action["readback"] != SAVE_READBACK
            || matches!(action["beforeText"].as_str(), Some(before) if before == input)
            || !super::valid_button(&action["controlBeforeClick"], &["Save", "保存"])
            || !super::valid_button(&action["controlAfterClick"], &["Save", "保存"])
        {
            return Err(AppError::conflict("Save 动作未经独立状态回读确认"));
        }
        saved_tabs.insert(tab_id);
    }
    match phase {
        1 => {
            if save_actions.len() != 2 || saved_tabs.len() != 2 {
                return Err(AppError::conflict("1.0 种子代必须分别保存两个标签"));
            }
            let inputs = save_actions
                .iter()
                .filter_map(|action| action["input"].as_str())
                .collect::<BTreeSet<_>>();
            if inputs.len() != 2 {
                return Err(AppError::conflict("1.0 两个标签必须保存不同文本"));
            }
            let final_global = save_actions
                .last()
                .and_then(|action| action["input"].as_str())
                .ok_or_else(|| AppError::conflict("1.0 最终全局保存文本缺失"))?;
            if before.iter().any(|tab| tab.draft == final_global) {
                return Err(AppError::conflict(
                    "1.0 恢复草稿与最终全局状态相同，无法区分每个标签的草稿",
                ));
            }
            if before.iter().any(|tab| {
                after
                    .iter()
                    .find(|after| after.tab_id == tab.tab_id)
                    .is_none_or(|after| after.draft != tab.draft)
            }) {
                return Err(AppError::conflict("1.0 种子代标签草稿在动作后改变"));
            }
        }
        2 => {
            if !save_actions.is_empty() {
                return Err(AppError::conflict("1.1 恢复代不应产生 Save 动作"));
            }
            let changed = before
                .iter()
                .filter(|before| {
                    after
                        .iter()
                        .find(|after| after.tab_id == before.tab_id)
                        .is_some_and(|after| after.draft != before.draft)
                })
                .map(|tab| tab.tab_id.as_str())
                .collect::<Vec<_>>();
            if changed.len() != 1 {
                return Err(AppError::conflict("1.1 恢复代必须只改变一个标签草稿"));
            }
            let target = changed[0];
            if before.iter().any(|before| {
                after
                    .iter()
                    .find(|after| after.tab_id == before.tab_id)
                    .is_some_and(|after| {
                        (before.tab_id == target && after.draft == before.draft)
                            || (before.tab_id != target && after.draft != before.draft)
                    })
            }) {
                return Err(AppError::conflict("1.1 隔离失败或 Count 目标草稿未变化"));
            }
        }
        3 => {
            if save_actions.len() != 1 {
                return Err(AppError::conflict("回退代必须执行一次本地 Save"));
            }
            let action = &save_actions[0];
            let tab_id = action["tabId"].as_str().unwrap_or_default();
            if action["input"].as_str() != Some(before_by_id[tab_id].draft.as_str()) {
                return Err(AppError::conflict("回退 Save 未保存当前标签草稿"));
            }
            if before.iter().any(|before| {
                after
                    .iter()
                    .find(|after| after.tab_id == before.tab_id)
                    .is_none_or(|after| after.draft != before.draft)
            }) {
                return Err(AppError::conflict("回退代确认后草稿被意外改变"));
            }
        }
        _ => return Err(AppError::conflict("标签恢复收据阶段无效")),
    }
    for check in [
        "distinctTabIds",
        "distinctInstances",
        "distinctViewIds",
        "distinctDrafts",
        "stableViewIdPerTab",
        "nonTargetDraftsPreserved",
    ] {
        if receipt["checks"][check] != true {
            return Err(AppError::conflict("标签恢复隔离检查未满足"));
        }
    }
    Ok(())
}

/// 数组项先验证实际身份字段，再用于集合比较，避免空对象通过数量检查。
fn validated_tabs(value: &Value) -> AppResult<Vec<CapturedTab>> {
    let tabs = value
        .as_array()
        .filter(|tabs| tabs.len() == 2)
        .ok_or_else(|| AppError::conflict("标签恢复数组不是两个"))?;
    tabs.iter()
        .map(|tab| {
            if tab["contentId"] != NOTES_CONTENT {
                return Err(AppError::conflict("标签恢复数组包含非笔记标签"));
            }
            let renderer = &tab["renderer"];
            if renderer["status"] != "Ready"
                || renderer["error"] != Value::Null
                || renderer["plugin"] != NOTES_PLUGIN
                || renderer["view"] != NOTES_VIEW
            {
                return Err(AppError::conflict("标签恢复 renderer 身份不符"));
            }
            Ok(CapturedTab {
                tab_id: nonempty(tab, "tabId")?,
                draft: nonempty(tab, "draft")?,
                view_id: nonempty(tab, "viewId")?,
                instance: nonempty(renderer, "instance")?,
                instance_generation: renderer["instanceGeneration"]
                    .as_u64()
                    .ok_or_else(|| AppError::conflict("标签恢复实例代次缺失"))?,
                process_generation: renderer["processGeneration"]
                    .as_u64()
                    .filter(|generation| *generation > 0)
                    .ok_or_else(|| AppError::conflict("标签恢复进程代次缺失"))?,
            })
        })
        .collect()
}

/// 非空字符串是标签和实例身份的最低要求，空值不能靠数组计数补足。
fn nonempty(value: &Value, key: &str) -> AppResult<String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AppError::conflict("标签恢复字段为空"))
}

/// 身份、实例、viewId 和草稿分别唯一；进程代次只在存在性上校验，不假设跨进程递增。
fn unique_tabs(tabs: &[CapturedTab]) -> bool {
    let unique =
        |values: Vec<&str>| values.into_iter().collect::<BTreeSet<_>>().len() == tabs.len();
    unique(tabs.iter().map(|tab| tab.tab_id.as_str()).collect())
        && unique(tabs.iter().map(|tab| tab.instance.as_str()).collect())
        && unique(tabs.iter().map(|tab| tab.view_id.as_str()).collect())
        && unique(tabs.iter().map(|tab| tab.draft.as_str()).collect())
}

#[cfg(test)]
#[path = "../../tests/support/plugin_restart_persistence_unit.rs"]
mod tests;
