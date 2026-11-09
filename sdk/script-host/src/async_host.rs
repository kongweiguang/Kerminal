// @author kongweiguang

use std::collections::HashMap;
use std::sync::mpsc::SyncSender;
use std::thread;

use cap_std::fs::Dir;
use kerminal_plugin_protocol::Permission;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{Input, network, storage};

const MAX_OPERATIONS: usize = 16;
const MAX_ARGUMENT_BYTES: usize = 6 * 1024 * 1024;

/// 所有后台操作都经有界请求表返回解释器线程，不跨线程传递 QuickJS 对象。
pub(crate) struct AsyncHost {
    sender: SyncSender<Input>,
    permissions: Vec<Permission>,
    root: Option<Dir>,
    next: u32,
    operations: HashMap<u32, CancellationToken>,
    network: Option<(tokio::runtime::Runtime, reqwest::Client)>,
}

impl AsyncHost {
    /// 后台运行时延迟到第一次网络请求，普通插件启动不承担网络线程成本。
    pub(crate) fn new(sender: SyncSender<Input>, permissions: Vec<Permission>) -> Self {
        Self {
            sender,
            permissions,
            root: None,
            next: 0,
            operations: HashMap::new(),
            network: None,
        }
    }

    /// 数据目录来自已校验的宿主握手，插件不能自行选择其他根目录。
    pub(crate) fn set_root(&mut self, path: &str) -> Result<(), String> {
        self.root = Some(storage::open_root(path)?);
        Ok(())
    }

    /// 限制并发及参数大小，完成结果经主事件队列回到原脚本进程。
    pub(crate) fn start(&mut self, text: &str) -> Result<u32, String> {
        if text.len() > MAX_ARGUMENT_BYTES {
            return Err("operation arguments exceed size limit".into());
        }
        if self.operations.len() >= MAX_OPERATIONS {
            return Err("too many background operations".into());
        }
        let request: Value =
            serde_json::from_str(text).map_err(|_| "invalid operation arguments")?;
        let kind = request
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("missing operation kind")?;
        let id = self.next_id();
        let cancellation = CancellationToken::new();
        let sender = self.sender.clone();
        match kind {
            "storage" => {
                let root = self
                    .root
                    .as_ref()
                    .ok_or("storage is not initialized")?
                    .try_clone()
                    .map_err(|error| error.to_string())?;
                thread::spawn(move || {
                    let result = storage::execute(root, request);
                    let _ = sender.send(Input::Native { id, result });
                });
            }
            "fetch" => {
                if self.network.is_none() {
                    self.network = Some(network_runtime()?);
                }
                let (runtime, client) =
                    self.network.as_ref().ok_or("network runtime unavailable")?;
                let client = client.clone();
                let permissions = self.permissions.clone();
                let token = cancellation.clone();
                runtime.spawn(async move {
                    let result = tokio::select! {
                        _ = token.cancelled() => Err("operation aborted".to_owned()),
                        result = network::fetch(client, permissions, request) => result,
                    };
                    let _ = sender.send(Input::Native { id, result });
                });
            }
            _ => return Err("unknown background operation".into()),
        }
        self.operations.insert(id, cancellation);
        Ok(id)
    }

    /// 中止会取消尚在连接或读取中的 HTTP future，而非仅忽略最终结果。
    pub(crate) fn cancel(&self, id: u32) {
        if let Some(token) = self.operations.get(&id) {
            token.cancel();
        }
    }

    /// 完成后释放配额，迟到或重复完成不会占用新的操作编号。
    pub(crate) fn finish(&mut self, id: u32) -> bool {
        self.operations.remove(&id).is_some()
    }

    /// 循环分配非零编号，避免长时间运行后与仍在途操作相撞。
    fn next_id(&mut self) -> u32 {
        loop {
            self.next = self.next.wrapping_add(1);
            if self.next != 0 && !self.operations.contains_key(&self.next) {
                return self.next;
            }
        }
    }
}

impl Drop for AsyncHost {
    /// 退出时通知所有网络任务取消，避免后台请求延续到下一代插件进程。
    fn drop(&mut self) {
        for token in self.operations.values() {
            token.cancel();
        }
        if let Some((runtime, _)) = self.network.take() {
            runtime.shutdown_background();
        }
    }
}

/// 单个进程复用一套网络执行器，禁止每次 fetch 新建线程池。
fn network_runtime() -> Result<(tokio::runtime::Runtime, reqwest::Client), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| format!("network runtime: {error}"))?;
    let client = {
        let _entered = runtime.enter();
        network::client()?
    };
    Ok((runtime, client))
}
