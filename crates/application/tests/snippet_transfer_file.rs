//! 片段传输文件应用用例的大小、取消和回执契约测试。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{
        SnippetTransferFileConfirmation, SnippetTransferFilePort, SnippetTransferFileService,
        SnippetTransferFileWriteReceipt,
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct FakeTransferFilePort {
    read_source: Mutex<String>,
    read_calls: Mutex<usize>,
    write_calls: Mutex<usize>,
}

#[async_trait]
impl SnippetTransferFilePort for FakeTransferFilePort {
    /// 模拟受控 UTF-8 文件读取，并记录是否越过服务的取消门禁。
    async fn read_import_file(
        &self,
        _path: PathBuf,
        _max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<String> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("测试导入已取消"));
        }
        *self.read_calls.lock().expect("read calls mutex") += 1;
        Ok(self.read_source.lock().expect("source mutex").clone())
    }

    /// 模拟导出回执，以验证应用层将底层确认类型原样返回。
    async fn write_export_file(
        &self,
        _path: PathBuf,
        source: String,
        _max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<SnippetTransferFileWriteReceipt> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("测试导出已取消"));
        }
        *self.write_calls.lock().expect("write calls mutex") += 1;
        Ok(SnippetTransferFileWriteReceipt {
            bytes_written: source.len(),
            confirmation: SnippetTransferFileConfirmation::ReadBackConfirmed,
        })
    }
}

/// 服务检测端口返回的超限文本，避免信任单个基础设施实现的防护。
#[tokio::test]
async fn rejects_oversized_import_after_port_read() {
    let port = Arc::new(FakeTransferFilePort::default());
    *port.read_source.lock().expect("source mutex") =
        "x".repeat(kerminal_application::commands::MAX_SNIPPET_TRANSFER_BYTES + 1);
    let service = SnippetTransferFileService::new(port.clone());

    let result = service
        .read_import_file(PathBuf::from("selected.json"), CancellationToken::new())
        .await;

    assert!(result.is_err());
    assert_eq!(*port.read_calls.lock().expect("read calls mutex"), 1);
}

/// 超限导出在调用文件端口前失败，防止 UI 或替代适配器越过统一预算。
#[tokio::test]
async fn rejects_oversized_export_before_port_call() {
    let port = Arc::new(FakeTransferFilePort::default());
    let service = SnippetTransferFileService::new(port.clone());

    let result = service
        .write_export_file(
            PathBuf::from("selected.json"),
            "x".repeat(kerminal_application::commands::MAX_SNIPPET_TRANSFER_BYTES + 1),
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_err());
    assert_eq!(*port.write_calls.lock().expect("write calls mutex"), 0);
}

/// 取消的读写不会交给文件端口，应用执行器可安全保留后台任务收尾。
#[tokio::test]
async fn cancellation_is_checked_before_file_io() {
    let port = Arc::new(FakeTransferFilePort::default());
    let service = SnippetTransferFileService::new(port.clone());
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(
        service
            .read_import_file(PathBuf::from("selected.json"), cancellation.clone())
            .await
            .is_err()
    );
    assert!(
        service
            .write_export_file(
                PathBuf::from("selected.json"),
                "{}".to_owned(),
                cancellation,
            )
            .await
            .is_err()
    );
    assert_eq!(*port.read_calls.lock().expect("read calls mutex"), 0);
    assert_eq!(*port.write_calls.lock().expect("write calls mutex"), 0);
}

/// 读回确认类型由文件端口原样返回，供界面区分正常提交与未知结果后的确认。
#[tokio::test]
async fn preserves_readback_confirmation_receipt() {
    let port = Arc::new(FakeTransferFilePort::default());
    let service = SnippetTransferFileService::new(port);

    let result = service
        .write_export_file(
            PathBuf::from("selected.json"),
            "{}".to_owned(),
            CancellationToken::new(),
        )
        .await
        .expect("export receipt");

    assert_eq!(result.bytes_written, 2);
    assert_eq!(
        result.confirmation,
        SnippetTransferFileConfirmation::ReadBackConfirmed
    );
}
