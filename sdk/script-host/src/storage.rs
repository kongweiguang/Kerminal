// @author kongweiguang

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cap_std::fs::{Dir, OpenOptions};
use serde_json::{Value, json};

const MAX_VALUE_BYTES: usize = 4 * 1024 * 1024;
const TEMP_PREFIX: &str = ".kerminal-tmp-";

/// 目录句柄由可信握手参数打开，所有后续文件操作都从此能力向下解析。
pub(crate) fn open_root(path: &str) -> Result<Dir, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("storage data directory must be absolute".into());
    }
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
        .map_err(|error| format!("storage data directory: {error}"))
}

/// 键值和文件使用不同子目录，避免插件文件名与内部记录冲突。
pub(crate) fn execute(root: Dir, request: Value) -> Result<Value, String> {
    let op = required(&request, "op")?;
    match op {
        "get" => {
            let key = key_path(required(&request, "key")?)?;
            let Some(dir) = open_optional(&root, "kv")? else {
                return Ok(Value::Null);
            };
            let Some(bytes) = read_optional(&dir, &key)? else {
                return Ok(Value::Null);
            };
            serde_json::from_slice(&bytes).map_err(|error| format!("invalid stored value: {error}"))
        }
        "set" => {
            let key = key_path(required(&request, "key")?)?;
            let value = request.get("value").ok_or("missing storage value")?;
            let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
            ensure_size(&bytes)?;
            let dir = open_created(&root, "kv")?;
            write_atomic(&dir, Path::new(&key), &bytes)?;
            Ok(Value::Null)
        }
        "delete" => {
            let key = key_path(required(&request, "key")?)?;
            let Some(dir) = open_optional(&root, "kv")? else {
                return Ok(json!(false));
            };
            remove_optional(&dir, Path::new(&key)).map(|removed| json!(removed))
        }
        "readFile" => {
            let path = relative_path(required(&request, "path")?, false)?;
            let dir = open_optional(&root, "files")?.ok_or("file not found")?;
            let bytes = read_optional(&dir, &path)?.ok_or("file not found")?;
            Ok(json!(STANDARD.encode(bytes)))
        }
        "writeFile" => {
            let path = relative_path(required(&request, "path")?, false)?;
            let bytes = STANDARD
                .decode(required(&request, "data")?)
                .map_err(|_| "invalid base64 data")?;
            ensure_size(&bytes)?;
            let dir = open_created(&root, "files")?;
            write_atomic(&dir, &path, &bytes)?;
            Ok(Value::Null)
        }
        "removeFile" => {
            let path = relative_path(required(&request, "path")?, false)?;
            let Some(dir) = open_optional(&root, "files")? else {
                return Ok(json!(false));
            };
            remove_optional(&dir, &path).map(|removed| json!(removed))
        }
        "list" => {
            let path = relative_path(
                request.get("path").and_then(Value::as_str).unwrap_or(""),
                true,
            )?;
            let Some(root) = open_optional(&root, "files")? else {
                return Ok(json!([]));
            };
            let dir = if path.as_os_str().is_empty() {
                root
            } else {
                root.open_dir(path).map_err(|error| error.to_string())?
            };
            let mut names = Vec::new();
            for entry in dir.entries().map_err(|error| error.to_string())? {
                let name = entry
                    .map_err(|error| error.to_string())?
                    .file_name()
                    .into_string()
                    .map_err(|_| "storage filename is not UTF-8")?;
                if !name.starts_with(TEMP_PREFIX) {
                    names.push(name);
                }
            }
            names.sort();
            Ok(json!(names))
        }
        _ => Err("unknown storage operation".into()),
    }
}

/// 必填字符串不允许借 JSON 类型转换绕过路径与键校验。
fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {field}"))
}

/// 十六进制小写文件名避免大小写不敏感文件系统混淆不同键。
fn key_path(key: &str) -> Result<String, String> {
    if key.is_empty() || key.len() > 100 || key.chars().any(char::is_control) {
        return Err(
            "storage key must contain 1 to 100 UTF-8 bytes without control characters".into(),
        );
    }
    Ok(key
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// 跨平台拒绝绝对路径、向上遍历、流名称及内部临时文件名。
fn relative_path(text: &str, allow_empty: bool) -> Result<PathBuf, String> {
    if text.is_empty() && allow_empty {
        return Ok(PathBuf::new());
    }
    if text.is_empty() || text.len() > 1024 || text.contains(['\\', ':', '\0']) {
        return Err("invalid storage path".into());
    }
    let path = Path::new(text);
    if path.components().any(|part| match part {
        Component::Normal(name) => name.to_string_lossy().starts_with(TEMP_PREFIX),
        _ => true,
    }) {
        return Err("invalid storage path".into());
    }
    Ok(path.to_path_buf())
}

/// 读操作不创建目录，缺失目录保留与缺失键一致的语义。
fn open_optional(root: &Dir, name: &str) -> Result<Option<Dir>, String> {
    match root.open_dir(name) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// 只在首次写入时创建子目录，句柄解析继续受能力边界约束。
fn open_created(root: &Dir, name: &str) -> Result<Dir, String> {
    root.create_dir_all(name)
        .map_err(|error| error.to_string())?;
    root.open_dir(name).map_err(|error| error.to_string())
}

/// 限制单项大小并检测超长文件，避免读取时无限增长进程内存。
fn read_optional(root: &Dir, path: impl AsRef<Path>) -> Result<Option<Vec<u8>>, String> {
    let file = match root.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_VALUE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    ensure_size(&bytes)?;
    Ok(Some(bytes))
}

/// 上限对读取与写入一致，不能以大文件绕开脚本堆限制。
fn ensure_size(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_VALUE_BYTES {
        Err("storage item exceeds 4 MiB".into())
    } else {
        Ok(())
    }
}

/// 临时文件在目标目录内创建并同步后替换，清理失败必须随原错误一并返回。
fn write_atomic(root: &Dir, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().filter(|part| !part.as_os_str().is_empty());
    if let Some(parent) = parent {
        root.create_dir_all(parent)
            .map_err(|error| error.to_string())?;
    }
    let dir = match parent {
        Some(parent) => root.open_dir(parent),
        None => root.try_clone(),
    }
    .map_err(|error| error.to_string())?;
    let name = path.file_name().ok_or("invalid storage filename")?;
    let temporary = format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4());
    let mut created = false;
    let result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = dir.open_with(&temporary, &options)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&temporary, &dir, name)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) if created => match dir.remove_file(&temporary) {
            Ok(()) => Err(error.to_string()),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(error.to_string()),
            Err(cleanup) => Err(format!("{error}; temporary file cleanup failed: {cleanup}")),
        },
        Err(error) => Err(error.to_string()),
    }
}

/// 删除缺失项目返回 false，其他错误必须保留给调用者。
fn remove_optional(root: &Dir, path: &Path) -> Result<bool, String> {
    match root.remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}
