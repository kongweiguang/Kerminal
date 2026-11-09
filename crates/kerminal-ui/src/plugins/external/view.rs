//! 已校验原生视图的窗口线程投影，动态值只读取宿主持有的数据快照。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kerminal_plugin_protocol::{
    Permission,
    native::{NativeNode, NativeView},
};
use serde_json::Value;

use super::state::{BindingPath, ViewState, ViewStateError};

/// 静态节点和每个实例的动态数据分离，避免远端更新替换正在编辑的草稿。
pub struct NativeViewProjection {
    view: Arc<NativeView>,
    state: ViewState,
    translations: Arc<BTreeMap<String, BTreeMap<String, String>>>,
    locale: String,
}

impl NativeViewProjection {
    /// 视图结构必须预先校验；打开新实例时每个实例拥有独立草稿与分页缓存。
    pub fn new(view: Arc<NativeView>, generation: u64) -> Self {
        let state = ViewState::new(Value::Object(view.data.clone()), generation);
        Self {
            view,
            state,
            translations: Arc::new(BTreeMap::new()),
            locale: "en".to_owned(),
        }
    }

    /// 目录在后台冻结后再注入；切换语言时继续复用视图和用户草稿。
    pub fn with_translations(
        mut self,
        translations: Arc<BTreeMap<String, BTreeMap<String, String>>>,
        locale: impl Into<String>,
    ) -> Self {
        self.translations = translations;
        self.locale = locale.into();
        self
    }

    /// 开发重载只替换校验后的静态树，实例草稿与远端数据仍由原状态持有。
    pub fn replace_view(
        &mut self,
        view: Arc<NativeView>,
        translations: Arc<BTreeMap<String, BTreeMap<String, String>>>,
    ) {
        self.view = view;
        self.translations = translations;
    }

    /// 语言变化只更换文案查找顺序，不重新创建实体或输入状态。
    pub fn set_locale(&mut self, locale: impl Into<String>) {
        self.locale = locale.into();
    }

    /// 只借用冻结的结构，渲染期间不读取插件目录或运行时进程。
    pub fn view(&self) -> &NativeView {
        &self.view
    }

    /// 输入、动作和分页共同使用一个实例状态，避免视图与提交数据分叉。
    pub fn state(&self) -> &ViewState {
        &self.state
    }

    /// 修改仅限窗口线程；后台结果须先核验实例及进程代次。
    pub fn state_mut(&mut self) -> &mut ViewState {
        &mut self.state
    }

    /// 可见性只接受布尔值；缺失或错误类型都隐藏节点，避免意外暴露内容。
    pub fn is_visible(&self, node: &NativeNode) -> bool {
        node.visible.as_deref().is_none_or(|binding| {
            BindingPath::parse(binding)
                .ok()
                .and_then(|path| self.state.value(&path))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
    }

    /// 文本绑定只解析对象键，不执行表达式，也不把不存在的字段显示成 JSON null。
    pub fn text(&self, text: &str) -> String {
        if let Some(binding) = text.strip_prefix('$') {
            return BindingPath::parse(binding)
                .ok()
                .and_then(|path| self.state.value(&path))
                .map(display_value)
                .unwrap_or_default();
        }
        let Some(key) = text
            .strip_prefix('%')
            .and_then(|text| text.strip_suffix('%'))
        else {
            return text.to_owned();
        };
        self.translations
            .get(&self.locale)
            .and_then(|messages| messages.get(key))
            .or_else(|| {
                self.translations
                    .get("en")
                    .and_then(|messages| messages.get(key))
            })
            .cloned()
            .unwrap_or_else(|| text.to_owned())
    }

    /// 局部更新复用状态层的 RFC 7386 合并规则，保留发生冲突的用户草稿。
    pub fn apply_patch(&mut self, patch: &Value) -> Result<(), ViewStateError> {
        self.state.apply_patch(patch)
    }
}

/// 只允许已授权插件打开无用户凭据的 HTTPS 链接，与宿主 API 的 URL 约束保持一致。
pub fn allowed_external_url(permissions: &BTreeSet<Permission>, url: &str) -> Option<url::Url> {
    if !permissions.contains(&Permission::OpenExternal)
        && !permissions.contains(&Permission::FullAccess)
    {
        return None;
    }
    let url = url::Url::parse(url).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// 界面以自然标量文本展示值，复合结构保持合法 JSON 而不执行插件内容。
pub fn display_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/external_native_view.rs"]
mod tests;
