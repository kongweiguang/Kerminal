//! 正式市场目录不可用时的只读示例预览。
//!
//! 示例只提供名称、描述、权限与仓库等展示元数据；占位资产仅供展示可用版本，
//! 安装由页面状态机拒绝，因此不需要伪造签名或改变市场信任边界。
//!
//! @author kongweiguang

use std::sync::OnceLock;

use kerminal_application::extensions::PluginCatalog;
use kerminal_domain::plugins::PluginId;

/// 内嵌 JSON 与 v1 目录契约同构，便于同一套列表与详情渲染复用。
/// 资产 URL 只指向示例仓库，长度为 64 个 0 的 sha256 明确表示“不可校验”，界面不会用它启动下载。
const SAMPLE_CATALOG: &str = r#"
{
  "schema": 1,
  "source_id": "example-preview",
  "sequence": 0,
  "plugins": [
    {
      "id": "example.notes",
      "name": {
        "zh-CN": "工作台便笺",
        "en": "Workbench Notes"
      },
      "description": {
        "zh-CN": "在工作台侧栏记录便笺，内容仅保存在本机。",
        "en": "Keep short notes in the workbench sidebar; content stays on this device."
      },
      "publisher": "example",
      "categories": ["示例"],
      "icon_url": null,
      "readme_url": null,
      "repository": "https://github.com/kongweiguang/Kerminal-Plugins",
      "permissions": ["ui.notify", "clipboard.write"],
      "versions": [
        {
          "version": "1.0.0",
          "api": "^1.0",
          "published_at": "2026-11-08",
          "changelog_url": null,
          "assets": [
            {
              "platform": "any",
              "url": "https://github.com/kongweiguang/Kerminal-Plugins",
              "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
              "size": 1
            }
          ]
        }
      ]
    },
    {
      "id": "example.http-check",
      "name": {
        "zh-CN": "HTTP 检查",
        "en": "HTTP Check"
      },
      "description": {
        "zh-CN": "请求指定 URL，并显示状态码、响应头和耗时。",
        "en": "Request a URL and show its status code, response headers, and timing."
      },
      "publisher": "example",
      "categories": ["示例"],
      "icon_url": null,
      "readme_url": null,
      "repository": "https://github.com/kongweiguang/Kerminal-Plugins",
      "permissions": ["network:example.com", "ui.notify", "open-external"],
      "versions": [
        {
          "version": "1.0.0",
          "api": "^1.0",
          "published_at": "2026-11-08",
          "changelog_url": null,
          "assets": [
            {
              "platform": "any",
              "url": "https://github.com/kongweiguang/Kerminal-Plugins",
              "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
              "size": 1
            }
          ]
        }
      ]
    },
    {
      "id": "example.file-search",
      "name": {
        "zh-CN": "文件搜索",
        "en": "File Search"
      },
      "description": {
        "zh-CN": "按名称搜索工作区文件，并跳转到匹配结果。",
        "en": "Search workspace files by name and jump to a match."
      },
      "publisher": "example",
      "categories": ["示例"],
      "icon_url": null,
      "readme_url": null,
      "repository": "https://github.com/kongweiguang/Kerminal-Plugins",
      "permissions": ["ui.file-picker", "hosts.read", "open-external"],
      "versions": [
        {
          "version": "1.0.0",
          "api": "^1.0",
          "published_at": "2026-11-08",
          "changelog_url": null,
          "assets": [
            {
              "platform": "any",
              "url": "https://github.com/kongweiguang/Kerminal-Plugins",
              "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
              "size": 1
            }
          ]
        }
      ]
    }
  ]
}
"#;

/// 只解析一次并长期复用；常量已在测试中锁定，若字段漂移应在开发阶段立即暴露而不是静默空列表。
pub(super) fn catalog() -> &'static PluginCatalog {
    static CATALOG: OnceLock<PluginCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(SAMPLE_CATALOG).expect("内嵌示例目录必须符合 v1 市场目录契约")
    })
}

/// 识别当前选择是否属于示例，避免为示例发起 README 网络读取或安装确认。
pub(super) fn contains(id: &PluginId) -> bool {
    catalog().plugins.iter().any(|plugin| &plugin.id == id)
}

/// 只有真实目录缺席且 id 属于示例时才按预览处理；真实目录存在时同 id 仍是普通目录条目。
/// 安装入口和运行状态机共用此判定，避免界面隐藏按钮却留下另一条可执行路径。
pub(super) fn is_preview_entry(catalog_present: bool, id: &PluginId) -> bool {
    !catalog_present && contains(id)
}
