//! 文件名到 GPUI Kit 内置语法高亮语言的窄映射。
//!
//! @author kongweiguang

/// 仅为 Kit 已随 `tree-sitter-languages` 提供的 grammar 选择语言，未知后缀留作纯文本。
pub(super) fn language_for_file_name(file_name: &str) -> Option<&'static str> {
    let name = file_name.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    let language = match name.as_str() {
        "makefile" | "gnumakefile" => "make",
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => "bash",
        _ => match name.rsplit_once('.').map(|(_, extension)| extension)? {
            "astro" => "astro",
            "bash" | "sh" | "zsh" => "bash",
            "c" | "h" => "c",
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
            "cmake" => "cmake",
            "cs" => "csharp",
            "css" | "scss" => "css",
            "diff" | "patch" => "diff",
            "ejs" => "ejs",
            "ex" | "exs" => "elixir",
            "go" => "go",
            "graphql" | "gql" => "graphql",
            "htm" | "html" => "html",
            "java" => "java",
            "js" | "mjs" | "cjs" | "jsx" => "javascript",
            "json" | "jsonc" => "json",
            "kt" | "kts" | "ktm" => "kotlin",
            "lua" => "lua",
            "md" | "mdx" | "markdown" => "markdown",
            "php" | "php3" | "php4" | "php5" | "phtml" => "php",
            "proto" | "protobuf" => "proto",
            "py" | "pyi" => "python",
            "rb" => "ruby",
            "rs" => "rust",
            "scala" => "scala",
            "sql" => "sql",
            "svelte" => "svelte",
            "swift" => "swift",
            "toml" => "toml",
            "ts" | "mts" | "cts" => "typescript",
            "tsx" => "tsx",
            "yaml" | "yml" => "yaml",
            "zig" => "zig",
            _ => return None,
        },
    };
    Some(language)
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_language.rs"]
mod tests;
