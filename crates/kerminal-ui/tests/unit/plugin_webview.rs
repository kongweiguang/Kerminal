//! @author kongweiguang

//! 原生视图几何契约留在测试目录，不向生产模块引入夹具代码。

use gpui_kit::{point, px, size};

use super::*;

/// 分数 DPI 分别舍入边缘，左右相邻区域共享同一像素边界，不能出现重复缩放或缝隙。
#[test]
fn fractional_dpi_rounds_shared_edges_once() {
    let left = physical_bounds(
        Bounds {
            origin: point(px(0.3), px(4.)),
            size: size(px(41.4), px(10.)),
        },
        1.25,
    )
    .unwrap();
    let right = physical_bounds(
        Bounds {
            origin: point(px(41.7), px(4.)),
            size: size(px(37.2), px(10.)),
        },
        1.25,
    )
    .unwrap();
    assert_eq!(left.x + left.width as i32, right.x);
    assert_eq!(left.height, right.height);
    assert_eq!(left.y, 5);
}

/// 空交集和损坏布局不得生成尺寸溢出的原生窗口，隐藏比猜测最小窗口位置更可靠。
#[test]
fn empty_or_non_finite_layout_has_no_native_bounds() {
    let empty = Bounds {
        origin: point(px(1.), px(1.)),
        size: size(px(0.), px(10.)),
    };
    assert!(physical_bounds(empty, 1.).is_none());
    let full = Bounds {
        origin: point(px(1.), px(1.)),
        size: size(px(10.), px(10.)),
    };
    assert!(physical_bounds(full, f32::NAN).is_none());
    assert!(physical_bounds(full, 0.).is_none());
}

/// 冻结夹具保留嵌套入口，以覆盖模块和样式相对目录，不触及系统原生引擎。
fn resource_document() -> PluginWebViewResourceDocument {
    PluginWebViewResourceDocument::new(
        "example.probe".into(),
        "instance-one".into(),
        "ui/index.html".into(),
        [
            (
                "ui/index.html".into(),
                resource(
                    "text/html; charset=utf-8",
                    b"<script type='module' src='./main.js'></script>",
                ),
            ),
            (
                "ui/main.js".into(),
                resource(
                    "text/javascript; charset=utf-8",
                    b"import './nested/module.mjs';",
                ),
            ),
            (
                "ui/nested/module.mjs".into(),
                resource(
                    "text/javascript; charset=utf-8",
                    b"export const ready = true;",
                ),
            ),
            (
                "ui/style.css".into(),
                resource("text/css; charset=utf-8", b"body { color: green; }"),
            ),
            (
                "ui/image.png".into(),
                resource("image/png", &[137, 80, 78, 71]),
            ),
            ("ui/font.woff2".into(), resource("font/woff2", b"wOF2")),
            (
                "ui/other.html".into(),
                resource("text/html; charset=utf-8", b"other"),
            ),
        ],
    )
    .unwrap()
}

/// 不同插件共享相同资源路径时，协议仍只能读取本实例绑定的插件来源。
fn other_resource_document() -> PluginWebViewResourceDocument {
    let mut document = resource_document();
    document.plugin_id = "example.other".into();
    document
}

/// 字节采用与生产投影一致的 Arc，防止测试绕过不可变资源边界。
fn resource(content_type: &'static str, bytes: &[u8]) -> PluginWebViewResource {
    PluginWebViewResource {
        content_type,
        bytes: std::sync::Arc::from(bytes),
    }
}

/// 使用真实 HTTP 请求类型检验协议回调，确保状态和 MIME 是最终交付给 Wry 的响应。
fn request(url: &str) -> wry::http::Request<Vec<u8>> {
    wry::http::Request::builder()
        .uri(url)
        .body(Vec::new())
        .unwrap()
}

/// 两种平台 URL 必须读取同一冻结字节；资源入口目录和准确 MIME 是模块加载的必要条件。
#[test]
fn frozen_resources_serve_nested_modules_styles_images_and_fonts() {
    let protocol = document::DocumentProtocol::new(resource_document().into());
    let url = protocol.url();
    assert!(url.ends_with("/ui/index.html"));
    let origin = url.strip_suffix("/ui/index.html").unwrap();
    assert_eq!(origin, "kplugin://example.probe");
    let windows = origin.replacen("kplugin://", "http://kplugin.", 1);
    for base in [origin, windows.as_str()] {
        for (path, mime, bytes) in [
            (
                "ui/main.js",
                "text/javascript; charset=utf-8",
                b"import './nested/module.mjs';".as_slice(),
            ),
            (
                "ui/nested/module.mjs",
                "text/javascript; charset=utf-8",
                b"export const ready = true;".as_slice(),
            ),
            (
                "ui/style.css",
                "text/css; charset=utf-8",
                b"body { color: green; }".as_slice(),
            ),
            ("ui/image.png", "image/png", [137, 80, 78, 71].as_slice()),
            ("ui/font.woff2", "font/woff2", b"wOF2".as_slice()),
        ] {
            let response = protocol.response(&request(&format!("{base}/{path}?version=1")));
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["Content-Type"], mime);
            assert_eq!(response.body().as_ref(), bytes);
            assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
            // 插件启用后不做能力限制：正式资源文档不注入限制性 CSP，页面不得再出现 connect-src 'none'。
            if let Some(csp) = response.headers().get("Content-Security-Policy") {
                assert!(!csp.to_str().unwrap().contains("connect-src 'none'"));
            }
        }
    }
}

/// 不同实例、authority 欺骗和编码点段均无权读取资源，拒绝后不能泄漏原文档字节。
#[test]
fn frozen_resource_route_rejects_cross_origin_traversal_and_non_get() {
    let protocol = document::DocumentProtocol::new(resource_document().into());
    let other = document::DocumentProtocol::new(other_resource_document().into());
    let url = protocol.url();
    let origin = url.strip_suffix("/ui/index.html").unwrap();
    for denied in [
        other.url(),
        format!("{origin}.evil/ui/index.html"),
        format!("{origin}@evil/ui/index.html"),
        format!("{origin}:80/ui/index.html"),
        format!("{origin}/ui/../ui/index.html"),
        format!("{origin}/ui/%2e%2e/ui/index.html"),
        format!("{origin}/ui/%252e%252e/ui/index.html"),
        format!("{origin}/ui%2findex.html"),
        format!("{origin}/ui%5cindex.html"),
        format!("{origin}//ui/index.html"),
        format!("{origin}/ui/missing.js"),
        "https://example.com/ui/index.html".into(),
    ] {
        let response = protocol.response(&request(&denied));
        assert_eq!(response.status(), 404, "{denied}");
        assert!(response.body().is_empty());
    }
    // HTTP传输类型在进入协议前拒绝空authority的file URL；导航层也必须独立拒绝它。
    assert!("file:///ui/index.html".parse::<wry::http::Uri>().is_err());
    assert!(!protocol.permits_document("file:///ui/index.html"));
    let mut post = request(&url);
    *post.method_mut() = wry::http::Method::POST;
    assert_eq!(protocol.response(&post).status(), 404);
    let mut foreign_origin = request(&url);
    foreign_origin
        .headers_mut()
        .insert("Origin", "https://example.com".parse().unwrap());
    assert_eq!(protocol.response(&foreign_origin).status(), 404);
}

/// 同源不足以授予桥接：只有本实例固定入口和片段导航允许，其他 HTML/查询/编码别名拒绝。
#[test]
fn bridge_origin_requires_exact_instance_entrypoint() {
    let protocol = document::DocumentProtocol::new(resource_document().into());
    let url = protocol.url();
    let windows = url.replacen("kplugin://", "http://kplugin.", 1);
    assert!(protocol.permits_document(&url));
    assert!(protocol.permits_document(&format!("{windows}#draft")));
    for denied in [
        url.replace("index.html", "other.html"),
        url.replace("index.html", "%69ndex.html"),
        url.replace("/ui/", "/ui/../ui/"),
        format!("{url}?instance=other"),
        windows.replace("http://", "https://"),
        document::DocumentProtocol::new(other_resource_document().into()).url(),
    ] {
        assert!(!protocol.permits_document(&denied), "{denied}");
    }
}

/// UTF-8 包内路径应可访问，但编码出的分隔符和可替换 ASCII 的别名不能绕过路径约束。
#[test]
fn unicode_paths_round_trip_without_accepting_encoded_aliases() {
    let document = PluginWebViewResourceDocument::new(
        "example.probe".into(),
        "instance-one".into(),
        "界面/入口 页面.html".into(),
        [(
            "界面/入口 页面.html".into(),
            resource("text/html; charset=utf-8", b"ok"),
        )],
    )
    .unwrap();
    let protocol = document::DocumentProtocol::new(document.into());
    let url = protocol.url();
    assert!(url.starts_with("kplugin://example.probe/"));
    assert_eq!(protocol.response(&request(&url)).body().as_ref(), b"ok");
    assert!(protocol.permits_document(&url));
    assert!(!protocol.permits_document(&url.replace("%E7", "%e7")));
    assert!(!protocol.permits_document(&url.replacen("/%E7", "%2F%E7", 1)));
}

/// 源集合中的歧义不能留给浏览器规范化；重复项即便字节相同也须在冻结阶段拒绝。
#[test]
fn resource_document_rejects_aliases_unavailable_entry_and_mime_confusion() {
    for paths in [
        vec!["ui/index.html", "ui/index.html"],
        vec!["ui/index.html", "UI/index.html"],
        vec!["ui/index.html", "ui/./escape.html"],
        vec!["ui/index.html", "ui/../escape.html"],
        vec!["ui/index.html", "ui/CON.html"],
        vec!["ui/index.html", "ui/a?.html"],
    ] {
        assert!(
            PluginWebViewResourceDocument::new(
                "example.probe".into(),
                "id".into(),
                "ui/index.html".into(),
                paths
                    .into_iter()
                    .map(|path| (path.into(), resource("text/html; charset=utf-8", b"html")))
            )
            .is_err()
        );
    }
    assert!(
        PluginWebViewResourceDocument::new(
            "example.probe".into(),
            "id".into(),
            "ui/missing.html".into(),
            [(
                "ui/index.html".into(),
                resource("text/html; charset=utf-8", b"html")
            )]
        )
        .is_err()
    );
    assert!(
        PluginWebViewResourceDocument::new(
            "example.probe".into(),
            "id".into(),
            "ui/index.html".into(),
            [(
                "ui/index.html".into(),
                resource("text/javascript; charset=utf-8", b"script")
            )]
        )
        .is_err()
    );
}

/// 单资源、总量和数量各有独立门禁，Arc 共享不会绕过按实际响应字节计算的预算。
#[test]
fn resource_document_enforces_independent_size_and_count_limits() {
    let large = PluginWebViewResource {
        content_type: "text/html; charset=utf-8",
        bytes: std::sync::Arc::from(vec![0; 8 * 1024 * 1024 + 1]),
    };
    assert!(
        PluginWebViewResourceDocument::new(
            "example.probe".into(),
            "id".into(),
            "index.html".into(),
            [("index.html".into(), large)]
        )
        .is_err()
    );
    let chunk = PluginWebViewResource {
        content_type: "text/html; charset=utf-8",
        bytes: std::sync::Arc::from(vec![0; 7 * 1024 * 1024]),
    };
    assert!(
        PluginWebViewResourceDocument::new(
            "example.probe".into(),
            "id".into(),
            "0.html".into(),
            (0..5).map(|index| (format!("{index}.html"), chunk.clone()))
        )
        .is_err()
    );
    assert!(
        PluginWebViewResourceDocument::new(
            "example.probe".into(),
            "id".into(),
            "0.html".into(),
            (0..1025).map(|index| (
                format!("{index}.html"),
                resource("text/html; charset=utf-8", b"")
            ))
        )
        .is_err()
    );
}

/// 原内联入口与其 CSP 保持兼容，但不会因为新增资源能力而开放相邻路径。
#[test]
fn inline_acceptance_api_retains_its_isolated_document() {
    let source: PluginWebViewSource = PluginWebViewDocument {
        instance_id: "inline".into(),
        html: "<script>fixture()</script>".into(),
    }
    .into();
    assert!(source.validate().is_ok());
    let protocol = document::DocumentProtocol::new(source);
    let url = protocol.url();
    let response = protocol.response(&request(&url));
    assert_eq!(response.status(), 200);
    assert_eq!(response.body().as_ref(), b"<script>fixture()</script>");
    assert!(
        response.headers()["Content-Security-Policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'unsafe-inline'")
    );
    assert_eq!(
        protocol
            .response(&request(&url.replace("index.html", "main.js")))
            .status(),
        404
    );
}

/// 受验包只暴露冻结资源；插件启用后不再要求网络权限，也不得注入限制性 CSP。
#[test]
fn prepared_webview_uses_frozen_resources_without_network_gate() {
    use kerminal_application::external_contributions::PreparedPlugin;
    use kerminal_domain::plugins::{PluginId, PluginVersion};
    use kerminal_plugin_protocol::{ManifestPolicy, ValidatedPackage, parse_manifest};
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

    // 清单保留 csp 字段以验证协议兼容，但 UI 侧必须忽略它，不生成任何网络限制。
    let manifest = serde_json::json!({
        "schema": 1, "id": "acme.web", "name": {"en": "Web"},
        "description": {"en": "Web"}, "version": "1.0.0", "api": "^1.0",
        "publisher": "acme", "runtime": {"kind": "none"},
        "views": {"page": {"kind": "webview", "entry": "web/index.html", "csp": "https://example.com"}}
    });
    let manifest = parse_manifest(
        &serde_json::to_vec(&manifest).unwrap(),
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    let mut prepared = PreparedPlugin {
        id: PluginId::parse("acme.web").unwrap(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        directory: PathBuf::from("must-not-be-read"),
        package: Arc::new(ValidatedPackage {
            manifest,
            files: BTreeMap::new(),
            views: BTreeMap::new(),
            configuration_schemas: BTreeMap::new(),
        }),
        native_views: BTreeMap::new(),
        localizations: BTreeMap::new(),
        resources: BTreeMap::from([(
            "web/index.html".into(),
            Arc::from(b"<html></html>".as_slice()),
        )]),
    };
    let source =
        PluginWebViewResourceDocument::from_prepared(&prepared, "page", "instance".into()).unwrap();
    let protocol = document::DocumentProtocol::new(source.into());
    let response = protocol.response(&request(&protocol.url()));
    assert_eq!(response.body().as_ref(), b"<html></html>");
    assert!(
        response
            .headers()
            .get("Content-Security-Policy")
            .is_none_or(|csp| !csp.to_str().unwrap().contains("connect-src 'none'")),
        "正式资源文档不得限制网络"
    );
    // 资源集合仍是唯一来源：入口缺失必须在构造阶段失败，不能回退读取磁盘。
    prepared.resources.clear();
    assert!(
        PluginWebViewResourceDocument::from_prepared(&prepared, "page", "instance".into()).is_err()
    );
}

/// 外部导航仅排队 HTTPS 候选；权限和实际打开由宿主独立决定。
#[test]
fn external_navigation_is_bounded_and_does_not_open_directly() {
    let (sender, receiver) = std::sync::mpsc::sync_channel(4);
    document::enqueue_external_navigation(&sender, "view", "http://example.com");
    document::enqueue_external_navigation(&sender, "view", "https://example.com/\ninvalid");
    document::enqueue_external_navigation(
        &sender,
        "view",
        &format!("https://example.com/{}", "x".repeat(4096)),
    );
    assert!(receiver.try_recv().is_err());
    document::enqueue_external_navigation(&sender, "view", "https://example.com/docs");
    assert!(
        matches!(receiver.try_recv(), Ok(PluginWebViewEvent::ExternalNavigation { instance_id, url }) if instance_id == "view" && url == "https://example.com/docs")
    );
}
