//! 工作区依赖方向检查，避免模块化单体逐步退化为相互引用的服务层。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use regex::Regex;
use serde_json::Value;

type CheckResult<T> = Result<T, Box<dyn Error>>;

const EXPECTED_WORKSPACE_PACKAGES: [&str; 8] = [
    "kerminal-domain",
    "kerminal-application",
    "kerminal-terminal",
    "kerminal-infrastructure",
    "kerminal-ui",
    "kerminal-mcp",
    "kerminal-desktop",
    "xtask",
];

const UI_FORBIDDEN_RESOURCE_DEPENDENCIES: &[&str] = &[
    "arboard",
    "axum",
    "clipboard-rs",
    "copypasta",
    "diesel",
    "directories",
    "dirs",
    "fast-socks5",
    "hyper",
    "keyring",
    "libc",
    "mio",
    "monaco-editor",
    "notify-debouncer-full",
    "notify",
    "notify-rust",
    "open",
    "portable-pty",
    "process-wrap",
    "react",
    "react-dom",
    "reqwest",
    "rmcp",
    "rusqlite",
    "russh",
    "russh_sftp",
    "rfd",
    "serialport",
    "single-application",
    "sqlx",
    "telnet",
    "tokio",
    "toml",
    "toml_edit",
    "ureq",
    "windows",
    "windows-sys",
    "xterm",
    "xterm-addon-fit",
    "xterm-addon-web-links",
];

const COMPOSITION_ROOT_ALLOWED_DEPENDENCIES: &[(&str, &str)] = &[("kerminal-desktop", "rfd")];

const LEGACY_WEB_SOURCE_EXTENSIONS: &[&str] = &[
    "cjs", "css", "html", "js", "jsx", "mjs", "svelte", "ts", "tsx", "vue",
];

const LEGACY_WEB_FILENAMES: &[&str] = &[
    "angular.json",
    "bun.lock",
    "bun.lockb",
    "index.html",
    "package-lock.json",
    "package.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

const PINNED_LEGACY_READBACK_COMMIT: &str = "17c5d4be46172ca508f92546bfae5fe5ef477f11";
const PINNED_LEGACY_READBACK_SCRIPT: &str = "scripts/acceptance/legacy-readback.ps1";
const PINNED_LEGACY_WORKSPACE_DECODER: &str =
    "scripts/acceptance/legacy-readback/legacy_workspace_readback.mjs";

/// 读取 Cargo 元数据并验证固定的 DDD 包边界和工具链契约。
pub fn check(root: &Path) -> CheckResult<()> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--locked", "--format-version", "1", "--no-deps"])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let packages = index_packages(&metadata)?;
    let violations = validate_packages(root, &packages);
    let mut violations = violations;
    validate_lock_graph(root, &mut violations);
    if violations.is_empty() {
        println!(
            "architecture boundaries: OK ({} workspace packages)",
            packages.len()
        );
        Ok(())
    } else {
        Err(violations.join("\n").into())
    }
}

/// 将 Cargo 元数据转成按包名索引的视图，后续边界规则不依赖包数组顺序。
fn index_packages(metadata: &Value) -> CheckResult<BTreeMap<String, Value>> {
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata did not return a package array")?;
    let mut indexed = BTreeMap::new();
    for package in packages {
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or("cargo metadata package has no name")?;
        indexed.insert(name.to_owned(), package.clone());
    }
    Ok(indexed)
}

/// 以精确 workspace 集合固定分层边界，避免新增包绕过既有依赖规则。
fn validate_packages(root: &Path, packages: &BTreeMap<String, Value>) -> Vec<String> {
    let mut violations = Vec::new();
    let package_names = packages.keys().cloned().collect::<BTreeSet<_>>();
    validate_workspace_package_set(&package_names, &mut violations);

    for package in packages.values() {
        let Some(name) = package.get("name").and_then(Value::as_str) else {
            continue;
        };
        if package.get("edition").and_then(Value::as_str) != Some("2024") {
            violations.push(format!("{name} must use Rust edition 2024"));
        }
        if package.get("rust_version").and_then(Value::as_str) != Some("1.98") {
            violations.push(format!("{name} must declare rust-version = 1.98"));
        }

        let deps = dependency_names(package);
        if deps
            .iter()
            .any(|dependency| dependency.starts_with("tauri"))
        {
            violations.push(format!("{name} must not depend on Tauri: {deps:?}"));
        }
        validate_internal_edges(name, &deps, &mut violations);
        validate_version_pins(name, package, &mut violations);
        validate_external_tests(name, package, &mut violations);
        if name == "kerminal-ui" {
            validate_ui_resource_sources(package, &mut violations);
        }
    }

    validate_legacy_web_inputs(root, &mut violations);
    violations
}

/// 比较完整包名集合而非逐项检查，新增的未分层 crate 也必须经过架构评审。
fn validate_workspace_package_set(names: &BTreeSet<String>, violations: &mut Vec<String>) {
    let expected = EXPECTED_WORKSPACE_PACKAGES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    for missing in expected.difference(names) {
        violations.push(format!("missing workspace package: {missing}"));
    }
    for unexpected in names.difference(&expected) {
        violations.push(format!("unexpected workspace package: {unexpected}"));
    }
}

/// 扫描产品输入，同时仅豁免固定 17c 旧版回读脚本实际调用的 workspace decoder。
fn validate_legacy_web_inputs(root: &Path, violations: &mut Vec<String>) {
    for legacy_path in [
        "src",
        "src-tauri",
        ".github/workflows/quality.yml",
        ".github/workflows/release.yml",
    ] {
        if root.join(legacy_path).exists() {
            violations.push(format!(
                "legacy Tauri/React build input remains: {legacy_path}"
            ));
        }
    }

    let mut candidates = Vec::new();
    if let Err(error) = collect_legacy_web_inputs(root, &mut candidates) {
        violations.push(format!("cannot inspect legacy web inputs: {error}"));
        return;
    }
    for candidate in candidates {
        if is_pinned_legacy_workspace_decoder(root, &candidate) {
            continue;
        }
        violations.push(format!(
            "legacy web build input remains: {}",
            candidate.strip_prefix(root).unwrap_or(&candidate).display()
        ));
    }
}

/// 只放行路径、冻结基线和调用关系均匹配的旧版回读 decoder，避免豁免扩散到脚本目录。
fn is_pinned_legacy_workspace_decoder(root: &Path, candidate: &Path) -> bool {
    let Some(relative) = candidate.strip_prefix(root).ok() else {
        return false;
    };
    if relative != Path::new(PINNED_LEGACY_WORKSPACE_DECODER) {
        return false;
    }

    let Ok(helper) = fs::read_to_string(candidate) else {
        return false;
    };
    if !helper.contains("decodeWorkspaceSessionSnapshot")
        || !helper.contains("assert.equal(source.version, 3")
        || !helper.contains("decoded.session.activeTabId")
    {
        return false;
    }

    let Ok(runner) = fs::read_to_string(root.join(PINNED_LEGACY_READBACK_SCRIPT)) else {
        return false;
    };
    runner.contains(&format!(
        "$script:BaselineCommit = '{PINNED_LEGACY_READBACK_COMMIT}'"
    )) && runner.contains("legacy_workspace_readback.mjs")
        && runner.contains("& $nodeExe $nodeTest")
}

/// 按文件名和源码扩展名识别前端构建入口，不扫描文件内容中的普通文本。
fn collect_legacy_web_inputs(directory: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    const EXCLUDED_DIRECTORIES: &[&str] = &[
        ".codegraph",
        ".git",
        ".updeng",
        ".next",
        "__tests__",
        "coverage",
        "dist",
        "docs",
        "documentation",
        "fixtures",
        "license",
        "licenses",
        "node_modules",
        "resources",
        "target",
        "test",
        "tests",
        "third-party",
        "third_party",
        "thirdparty",
        "vendor",
    ];

    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let directory_name = entry.file_name();
            if directory_name.to_str().is_some_and(|name| {
                EXCLUDED_DIRECTORIES
                    .iter()
                    .any(|excluded| name.eq_ignore_ascii_case(excluded))
            }) {
                continue;
            }
            collect_legacy_web_inputs(&path, found)?;
        } else if kind.is_file() && is_legacy_web_input(&path) {
            found.push(path);
        }
    }
    Ok(())
}

/// 使用明确的旧前端入口名与语言扩展名，避免依赖脆弱的仓库全文词语搜索。
fn is_legacy_web_input(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    if LEGACY_WEB_FILENAMES.contains(&file_name.as_str())
        || [
            "next.config.",
            "postcss.config.",
            "tailwind.config.",
            "vite.config.",
            "webpack.config.",
        ]
        .iter()
        .any(|prefix| file_name.starts_with(prefix))
        || file_name.starts_with("tsconfig") && file_name.ends_with(".json")
    {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            LEGACY_WEB_SOURCE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// 检查源码只引用 crate 外置测试文件，避免测试实现进入产品模块树。
fn validate_external_tests(name: &str, package: &Value, violations: &mut Vec<String>) {
    let Some(manifest) = package.get("manifest_path").and_then(Value::as_str) else {
        return;
    };
    let Some(package_root) = Path::new(manifest).parent() else {
        return;
    };
    let source_root = package_root.join("src");
    if !source_root.is_dir() {
        return;
    }

    let mut sources = Vec::new();
    if let Err(error) = collect_rust_sources(&source_root, &mut sources) {
        violations.push(format!("cannot inspect {name} source tree: {error}"));
        return;
    }

    for source in sources {
        match fs::read_to_string(&source) {
            Ok(contents) => {
                validate_source_test_policy(name, package_root, &source, &contents, violations)
            }
            Err(error) => violations.push(format!("cannot read {}: {error}", source.display())),
        }
    }
}

/// 以语法锚定的属性检查避免把注释或测试文案当成实现，同时验证 path 确实落在 tests/。
fn validate_source_test_policy(
    package_name: &str,
    package_root: &Path,
    source: &Path,
    contents: &str,
    violations: &mut Vec<String>,
) {
    let test_attribute =
        Regex::new(r#"^\s*#\s*\[\s*(?:test\s*\]|tokio\s*::\s*test(?:\s*\(|\s*\]))"#)
            .expect("static test attribute expression is valid");
    let cfg_attribute = Regex::new(r#"^\s*#\s*\[\s*cfg\s*\((?P<expr>[^]]*)\)\s*\]\s*$"#)
        .expect("static cfg attribute expression is valid");
    let path_attribute = Regex::new(r##"^\s*#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]\s*$"##)
        .expect("static path attribute expression is valid");
    let module_declaration =
        Regex::new(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;")
            .expect("static external module expression is valid");
    let lines = contents.lines().collect::<Vec<_>>();

    for (index, line) in lines.iter().enumerate() {
        if test_attribute.is_match(line) {
            violations.push(format!(
                "{package_name} contains inline test function in {}:{}; move the implementation under tests/",
                source.display(),
                index + 1
            ));
        }

        let Some(captures) = cfg_attribute.captures(line) else {
            continue;
        };
        let expression = captures.name("expr").map_or("", |value| value.as_str());
        if !cfg_expression_is_test_only(expression) {
            continue;
        }

        if next_test_module_path(&lines, index, &path_attribute, &module_declaration).is_some_and(
            |path| {
                path.as_deref().is_some_and(|relative_path| {
                    is_external_test_path(package_root, source, relative_path)
                })
            },
        ) {
            continue;
        }

        // cfg(test) 的 use 仅准备外置测试编译环境，不包含测试行为或产品状态。
        if next_attribute_item(&lines, index).is_some_and(|item| item.starts_with("use ")) {
            continue;
        }

        violations.push(format!(
            "{package_name} contains test-only source item in {}:{}; src may keep only a #[cfg(test)] module path under tests/",
            source.display(),
            index + 1
        ));
    }
}

/// 只拒绝在 `test = false` 时不可能启用的项，避免误报跨平台生产解析器的测试构建。
fn cfg_expression_is_test_only(expression: &str) -> bool {
    cfg_expression_without_test(expression) == Some(false)
}

/// 将 `test` 固定为 false，并保留其它 cfg 条件的未知性以判断是否仍有生产构建路径。
/// 解析不完整时返回 `None`，让扫描器按可能存在生产路径处理而不产生误判。
///
/// @author kongweiguang
fn cfg_expression_without_test(expression: &str) -> Option<bool> {
    let expression = expression.trim();
    if expression == "test" {
        return Some(false);
    }
    let open = expression.find('(')?;
    if !expression.ends_with(')') {
        return None;
    }
    let operator = expression[..open].trim();
    let inner = &expression[open + 1..expression.len() - 1];
    let values = split_cfg_arguments(inner)
        .into_iter()
        .map(cfg_expression_without_test)
        .collect::<Vec<_>>();
    match operator {
        "all" if values.contains(&Some(false)) => Some(false),
        "all" if values.iter().all(|value| *value == Some(true)) => Some(true),
        "all" => None,
        "any" if values.contains(&Some(true)) => Some(true),
        "any" if values.iter().all(|value| *value == Some(false)) => Some(false),
        "any" => None,
        "not" if values.len() == 1 => values[0].map(|value| !value),
        // 未知组合按可能含生产路径处理，防止扫描器把内容误判为测试专属实现。
        _ => None,
    }
}

/// 拆分 Rust cfg 表达式的顶层参数，仅供识别 test 谓词，不解释其它平台条件。
fn split_cfg_arguments(expression: &str) -> Vec<&str> {
    let mut arguments = Vec::new();
    let mut depth = 0_u32;
    let mut start = 0;
    for (index, character) in expression.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                arguments.push(expression[start..index].trim());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let tail = expression[start..].trim();
    if !tail.is_empty() {
        arguments.push(tail);
    }
    arguments
}

/// 读取 cfg(test) 后紧邻的外置模块声明及其 path；无字面 path 的默认 src 子模块不合规。
fn next_test_module_path(
    lines: &[&str],
    cfg_index: usize,
    path_attribute: &Regex,
    module_declaration: &Regex,
) -> Option<Option<String>> {
    let mut path = None;
    for line in lines.iter().skip(cfg_index + 1) {
        let item = line.trim();
        if item.is_empty() || item.starts_with("//") || item.starts_with("///") {
            continue;
        }
        if item.starts_with("#[") {
            if let Some(captures) = path_attribute.captures(item) {
                path = captures.get(1).map(|value| value.as_str().to_owned());
            }
            continue;
        }
        return module_declaration.is_match(item).then_some(path);
    }
    None
}

/// 解析外置测试路径并要求文件真实存在于当前 crate 的 tests/ 子树内。
fn is_external_test_path(package_root: &Path, source: &Path, relative_path: &str) -> bool {
    let Some(tests_root) = package_root.join("tests").canonicalize().ok() else {
        return false;
    };
    let Some(target) = source
        .parent()
        .map(|parent| parent.join(relative_path))
        .and_then(|path| path.canonicalize().ok())
    else {
        return false;
    };
    target.is_file() && target.starts_with(tests_root)
}

/// 找到下一项用于判断 cfg(test) 是纯导入还是实际测试专用实现。
fn next_attribute_item<'a>(lines: &'a [&'a str], cfg_index: usize) -> Option<&'a str> {
    lines.iter().skip(cfg_index + 1).find_map(|line| {
        let item = line.trim();
        (!item.is_empty() && !item.starts_with('#') && !item.starts_with("//")).then_some(item)
    })
}

/// 收集 crate 的 Rust 源文件，递归处理按业务职责拆分的模块目录。
fn collect_rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_rust_sources(&path, sources)?;
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
    Ok(())
}

/// UI 通过 application 接口取资源；框架窗口、焦点、剪贴板和纯地址值仍由 UI 正常使用。
fn validate_ui_resource_sources(package: &Value, violations: &mut Vec<String>) {
    let Some(manifest) = package.get("manifest_path").and_then(Value::as_str) else {
        return;
    };
    let Some(package_root) = Path::new(manifest).parent() else {
        return;
    };
    let source_root = package_root.join("src");
    if !source_root.is_dir() {
        return;
    }

    let patterns = [
        (
            Regex::new(r"\b(?:std|tokio)::(?:fs|process)(?:::|\b)")
                .expect("static filesystem/process API expression is valid"),
            "filesystem/process",
        ),
        (
            Regex::new(
                r"\b(?:std|tokio)::net::(?:TcpStream|TcpListener|UdpSocket|UnixStream|UnixListener)\b",
            )
            .expect("static socket API expression is valid"),
            "network socket",
        ),
        (
            Regex::new(
                r"\buse\s+(?:std|tokio)::net::\s*\{[^}]*\b(?:TcpStream|TcpListener|UdpSocket|UnixStream|UnixListener)\b",
            )
            .expect("static grouped socket import expression is valid"),
            "network socket",
        ),
        (
            Regex::new(r"(?s)\buse\s+(?:std|tokio)::\s*\{[^;]*\b(?:fs|process)\b[^;]*;")
                .expect("static grouped resource import expression is valid"),
            "filesystem/process",
        ),
        (
            Regex::new(
                r"(?s)\buse\s+(?:std|tokio)::\s*\{[^;]*\bnet\s*::\s*\{[^}]*\b(?:TcpStream|TcpListener|UdpSocket|UnixStream|UnixListener)\b[^}]*\}[^;]*;",
            )
            .expect("static nested socket import expression is valid"),
            "network socket",
        ),
    ];
    let mut sources = Vec::new();
    if let Err(error) = collect_rust_sources(&source_root, &mut sources) {
        violations.push(format!("cannot inspect UI source tree: {error}"));
        return;
    }

    for source in sources {
        let contents = match fs::read_to_string(&source) {
            Ok(contents) => contents,
            Err(error) => {
                violations.push(format!("cannot read {}: {error}", source.display()));
                continue;
            }
        };
        let code = contents
            .lines()
            .map(|line| {
                let line = line.trim_start();
                if line.starts_with("//") || line.starts_with('*') {
                    ""
                } else {
                    line.split_once("//").map_or(line, |(code, _)| code)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        for (pattern, category) in &patterns {
            if let Some(found) = pattern.find(&code) {
                let line_number = code[..found.start()]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1;
                violations.push(format!(
                    "kerminal-ui source directly uses {category} API in {}:{}; route resource access through application",
                    source.display(),
                    line_number
                ));
            }
        }
    }
}

/// 同时记录 Cargo 实际 package 名与依赖 key，避免 package rename 躲过层级禁用规则。
fn dependency_names(package: &Value) -> BTreeSet<String> {
    package
        .get("dependencies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|dependency| {
            dependency.get("kind").is_none_or(Value::is_null)
                || dependency.get("kind").and_then(Value::as_str) == Some("normal")
        })
        .flat_map(|dependency| {
            [
                dependency.get("name").and_then(Value::as_str),
                dependency.get("rename").and_then(Value::as_str),
            ]
            .into_iter()
            .flatten()
            .map(str::to_owned)
        })
        .collect()
}

/// 同时识别仓库当前的别名与 Cargo metadata 中的真实第三方 package 名。
fn contains_forbidden_dependency(deps: &BTreeSet<String>, dependency: &str) -> bool {
    match dependency {
        "russh_sftp" => deps.contains("russh_sftp") || deps.contains("bssh-russh-sftp"),
        "telnet" => deps.contains("telnet") || deps.contains("justelnet-core"),
        "serialport" => deps.contains("serialport") || deps.contains("tokio-serial"),
        _ => deps.contains(dependency),
    }
}

/// 只为桌面组合根保留启动 GPUI 前的致命错误对话框依赖，不放宽 UI 资源边界。
fn is_explicit_composition_root_dependency_allowed(package: &str, dependency: &str) -> bool {
    COMPOSITION_ROOT_ALLOWED_DEPENDENCIES.contains(&(package, dependency))
}

/// 按 DDD 层次限制内部依赖，并把 UI 资源访问留在 application 端口之后。
fn validate_internal_edges(name: &str, deps: &BTreeSet<String>, violations: &mut Vec<String>) {
    let forbidden = match name {
        "kerminal-domain" => &[
            "kerminal-application",
            "kerminal-terminal",
            "kerminal-infrastructure",
            "kerminal-ui",
            "kerminal-mcp",
            "kerminal-desktop",
            "async-trait",
            "tokio",
            "tokio-util",
            "gpui-kit",
            "gpui-pre",
            "gpui-pre-platform",
            "portable-pty",
            "russh",
            "russh_sftp",
            "rusqlite",
            "keyring",
            "serialport",
            "telnet",
            "axum",
            "rfd",
            "single-application",
        ][..],
        "kerminal-application" => &[
            "kerminal-infrastructure",
            "kerminal-ui",
            "kerminal-mcp",
            "kerminal-desktop",
            "gpui-kit",
            "gpui-pre",
            "gpui-pre-platform",
            "portable-pty",
            "russh",
            "russh_sftp",
            "rusqlite",
            "keyring",
            "serialport",
            "telnet",
            "axum",
            "rfd",
            "single-application",
        ][..],
        "kerminal-terminal" => &[
            "kerminal-application",
            "kerminal-infrastructure",
            "kerminal-ui",
            "kerminal-mcp",
            "kerminal-desktop",
            "portable-pty",
            "tokio",
            "tokio-util",
            "gpui-kit",
            "gpui-pre",
            "gpui-pre-platform",
            "russh",
            "russh_sftp",
            "rusqlite",
            "keyring",
            "serialport",
            "telnet",
            "axum",
            "rfd",
            "single-application",
        ][..],
        "kerminal-infrastructure" => &[
            "kerminal-ui",
            "kerminal-mcp",
            "kerminal-desktop",
            "gpui-kit",
            "gpui-pre",
            "gpui-pre-platform",
            "rmcp",
            "axum",
        ][..],
        "kerminal-ui" => &[
            "kerminal-infrastructure",
            "kerminal-mcp",
            "kerminal-desktop",
            "russh",
            "russh_sftp",
            "rusqlite",
            "keyring",
            "serialport",
            "portable-pty",
            "telnet",
            "tokio",
            "rmcp",
            "axum",
            "rfd",
            "single-application",
        ][..],
        "kerminal-mcp" => &[
            "kerminal-infrastructure",
            "kerminal-ui",
            "kerminal-desktop",
            "kerminal-terminal",
            "gpui-kit",
            "gpui-pre",
            "gpui-pre-platform",
            "portable-pty",
            "russh",
            "russh_sftp",
            "rusqlite",
            "keyring",
            "serialport",
            "telnet",
            "rfd",
            "single-application",
        ][..],
        "kerminal-desktop" => &[
            "portable-pty",
            "russh",
            "russh_sftp",
            "rusqlite",
            "keyring",
            "serialport",
            "telnet",
            "axum",
            "rmcp",
            "rfd",
        ][..],
        "xtask" => &[
            "kerminal-domain",
            "kerminal-application",
            "kerminal-terminal",
            "kerminal-infrastructure",
            "kerminal-ui",
            "kerminal-mcp",
            "kerminal-desktop",
        ][..],
        _ => &[],
    };

    for dependency in forbidden {
        if contains_forbidden_dependency(deps, dependency)
            && !is_explicit_composition_root_dependency_allowed(name, dependency)
        {
            violations.push(format!("{name} has forbidden dependency {dependency}"));
        }
    }

    if name == "kerminal-ui" {
        for dependency in UI_FORBIDDEN_RESOURCE_DEPENDENCIES {
            if !forbidden.contains(dependency) && contains_forbidden_dependency(deps, dependency) {
                violations.push(format!(
                    "{name} has forbidden direct resource dependency {dependency}"
                ));
            }
        }
    }
}

/// 按 Cargo.lock 的实际解析结果拒绝退役运行栈并固定兼容的 GPUI 发布线。
fn validate_lock_graph(root: &Path, violations: &mut Vec<String>) {
    let path = root.join("Cargo.lock");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            violations.push(format!("cannot read Cargo.lock: {error}"));
            return;
        }
    };
    let document: toml::Value = match toml::from_str(&contents) {
        Ok(document) => document,
        Err(error) => {
            violations.push(format!("cannot parse Cargo.lock: {error}"));
            return;
        }
    };
    let Some(packages) = document.get("package").and_then(toml::Value::as_array) else {
        violations.push("Cargo.lock does not contain a package array".to_owned());
        return;
    };

    validate_forbidden_lock_packages(packages, violations);
    for (name, expected) in [
        ("gpui-kit", "0.7.3"),
        ("gpui-kit-assets", "0.7.3"),
        ("gpui-base", "0.7.3"),
        ("gpui-component", "0.7.3"),
        ("gpui-pre", "0.3.9"),
        ("gpui-pre-platform", "0.3.9"),
        ("gpui-pre-macros", "0.3.9"),
    ] {
        validate_locked_version(packages, name, expected, violations);
    }
    for package in packages {
        let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        if name.starts_with("gpui-pre-") && name != "gpui-pre-reqwest" {
            let version = package.get("version").and_then(toml::Value::as_str);
            if version != Some("0.3.9") {
                violations.push(format!(
                    "Cargo.lock resolves {name} to {version:?}, expected GPUI snapshot 0.3.9"
                ));
            }
        }
    }
}

/// 拒绝旧桌面/Web运行栈的解析产物，但保留独立的 Windows 通知后端 crate。
fn validate_forbidden_lock_packages(packages: &[toml::Value], violations: &mut Vec<String>) {
    for package in packages {
        let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        if is_legacy_runtime_package(name) {
            violations.push(format!("Cargo.lock resolves legacy runtime package {name}"));
        }
    }
}

/// 只匹配旧运行时本体及其组件；notify-rust 使用的 winrt 通知 crate 与 Tauri 本体无关。
fn is_legacy_runtime_package(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let is_tauri = (name == "tauri" || name.starts_with("tauri-") || name.starts_with("tauri_"))
        && name != "tauri-winrt-notification";
    is_tauri
        || name == "react"
        || name == "react-dom"
        || name.starts_with("react-")
        || name == "xterm"
        || name.starts_with("xterm-")
        || name == "monaco-editor"
        || name == "monaco-editor-core"
        || name.starts_with("monaco-editor-")
}

/// 每个 GPUI crate 必须只出现一次且版本精确匹配，排除 lockfile 中的并行快照。
fn validate_locked_version(
    packages: &[toml::Value],
    name: &str,
    expected: &str,
    violations: &mut Vec<String>,
) {
    let matching = packages
        .iter()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some(name))
        .collect::<Vec<_>>();
    let versions = matching
        .iter()
        .filter_map(|package| package.get("version").and_then(toml::Value::as_str))
        .collect::<BTreeSet<_>>();
    if matching.len() != 1 || versions.len() != 1 || !versions.contains(expected) {
        violations.push(format!(
            "Cargo.lock must resolve {name} exactly once at {expected}, found {versions:?}"
        ));
    }
}

#[cfg(test)]
#[path = "../tests/unit/architecture.rs"]
mod tests;

/// GPUI Kit 和配对 GPUI snapshot 必须精确锁在兼容发布线上。
fn validate_version_pins(name: &str, package: &Value, violations: &mut Vec<String>) {
    if name == "kerminal-ui" || name == "kerminal-desktop" {
        check_requirement(package, "gpui-kit", "=0.7.3", name, violations);
        check_requirement(package, "gpui-pre", "=0.3.9", name, violations);
    }
}

/// 校验 workspace 继承后的依赖版本要求，防止小版本漂移混入第二套 GPUI。
fn check_requirement(
    package: &Value,
    dependency_name: &str,
    expected: &str,
    package_name: &str,
    violations: &mut Vec<String>,
) {
    let dependency = package
        .get("dependencies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|dependency| {
            dependency.get("name").and_then(Value::as_str) == Some(dependency_name)
                || dependency.get("rename").and_then(Value::as_str)
                    == Some(dependency_name.replace('-', "_").as_str())
        });
    let found = dependency
        .and_then(|dependency| dependency.get("req"))
        .and_then(Value::as_str);
    if found != Some(expected) {
        violations.push(format!(
            "{package_name} must pin {dependency_name} to {expected}, found {found:?}"
        ));
    }
}
