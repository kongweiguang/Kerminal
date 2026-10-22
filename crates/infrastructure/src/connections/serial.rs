//! Tokio serial transport 与串口设备目录适配器。
//!
//! @author kongweiguang

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{SerialPortDescriptor, SerialPortRepository},
    sessions::{
        SessionOpenCleanupOwner, SessionTransport, SessionTransportControl,
        SessionTransportFactory, SessionTransportOpenError,
    },
};
use kerminal_domain::sessions::{
    SerialDataBits, SerialFlowControl, SerialParity, SerialSessionTarget, SerialStopBits,
    SessionExitStatus, SessionTarget, TerminalSize,
};
use tokio::{io, sync::Mutex, task::JoinHandle};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, SerialStream, StopBits};
use tokio_util::sync::CancellationToken;

/// 串口桥接只使用固定容量的 duplex buffer，反向复制会随终端读写速度施加背压。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SerialRuntimeConfig {
    pub channel_buffer_bytes: usize,
}

impl Default for SerialRuntimeConfig {
    /// 限制单个串口会话的排队字节数，避免断开或 UI 停顿时无界积压数据。
    fn default() -> Self {
        Self {
            channel_buffer_bytes: 256 * 1024,
        }
    }
}

/// 将领域串口目标的每项线制设置一对一映射到 tokio-serial 设置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SerialSettings {
    baud_rate: u32,
    data_bits: DataBits,
    stop_bits: StopBits,
    parity: Parity,
    flow_control: FlowControl,
}

impl SerialSettings {
    /// 在打开设备前校验目标，避免驱动以隐式默认值修正损坏的旧配置。
    fn from_target(target: &SerialSessionTarget) -> AppResult<Self> {
        if target.baud_rate < 300 || target.baud_rate > 4_000_000 {
            return Err(AppError::invalid("串口波特率必须在 300 到 4000000 之间"));
        }
        Ok(Self {
            baud_rate: target.baud_rate,
            data_bits: match target.data_bits {
                SerialDataBits::Five => DataBits::Five,
                SerialDataBits::Six => DataBits::Six,
                SerialDataBits::Seven => DataBits::Seven,
                SerialDataBits::Eight => DataBits::Eight,
            },
            stop_bits: match target.stop_bits {
                SerialStopBits::One => StopBits::One,
                SerialStopBits::Two => StopBits::Two,
            },
            parity: match target.parity {
                SerialParity::None => Parity::None,
                SerialParity::Even => Parity::Even,
                SerialParity::Odd => Parity::Odd,
            },
            flow_control: match target.flow_control {
                SerialFlowControl::None => FlowControl::None,
                SerialFlowControl::Software => FlowControl::Software,
                SerialFlowControl::Hardware => FlowControl::Hardware,
            },
        })
    }
}

/// 创建本地异步串口会话；串口不支持 PTY 窗口尺寸语义。
pub struct SerialSessionTransportFactory {
    config: SerialRuntimeConfig,
}

impl SerialSessionTransportFactory {
    /// 注入会话缓冲预算，便于平台装配和资源边界测试使用相同实现。
    pub fn new(config: SerialRuntimeConfig) -> AppResult<Self> {
        if config.channel_buffer_bytes == 0 {
            return Err(AppError::invalid("串口通道缓冲容量必须大于零"));
        }
        Ok(Self { config })
    }
}

#[async_trait]
impl SessionTransportFactory for SerialSessionTransportFactory {
    /// 按领域参数打开唯一目标设备；取消只作用于打开阶段，生命周期关闭交由 control 管理。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        _size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let SessionTarget::Serial(target) = target else {
            return Err(AppError::invalid("串口 transport 收到了非串口目标").into());
        };
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("串口打开已取消").into());
        }
        let device = serial_device_name(&target.device)?;
        let settings = SerialSettings::from_target(target)?;
        let mut open = tokio::task::spawn_blocking(move || {
            tokio_serial::new(device, settings.baud_rate)
                .data_bits(settings.data_bits)
                .stop_bits(settings.stop_bits)
                .parity(settings.parity)
                .flow_control(settings.flow_control)
                .timeout(Duration::from_millis(100))
                .open_native_async()
        });
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Windows CreateFile 和 Unix open 都不可由 Tokio 中止；owner 保留 join handle，接手迟到设备句柄。
                let cleanup = SerialOpenCleanupOwner::new(open);
                return Err(SessionTransportOpenError::with_cleanup(
                    AppError::cancelled("串口打开已取消"),
                    Arc::new(cleanup),
                ));
            }
            result = &mut open => {
                result
                    .map_err(|error| AppError::infrastructure_with_source("串口打开 task 异常退出", error))?
                    .map_err(|error| AppError::infrastructure_with_source("无法打开串口设备", error))?
            }
        };
        if cancel.is_cancelled() {
            drop(stream);
            return Err(AppError::cancelled("串口打开已取消").into());
        }
        Ok(make_serial_transport(
            stream,
            self.config.channel_buffer_bytes,
        ))
    }
}

/// 保留不可取消的阻塞设备打开任务，直到迟到的 stream 被关闭或原生 open 返回错误。
///
/// @author kongweiguang
struct SerialOpenCleanupOwner {
    state: Mutex<SerialOpenCleanupState>,
}

/// 超时只把仍工作的 JoinHandle 放回 owner，不遗失串口资源的后续回收入口。
struct SerialOpenCleanupState {
    task: Option<JoinHandle<tokio_serial::Result<SerialStream>>>,
    finished: bool,
}

impl SerialOpenCleanupOwner {
    /// 把唯一串口 open task 移交给可由 SessionManager 重试的 owner。
    ///
    /// @author kongweiguang
    fn new(task: JoinHandle<tokio_serial::Result<SerialStream>>) -> Self {
        Self {
            state: Mutex::new(SerialOpenCleanupState {
                task: Some(task),
                finished: false,
            }),
        }
    }
}

#[async_trait]
impl SessionOpenCleanupOwner for SerialOpenCleanupOwner {
    /// 共用绝对 deadline 等待阻塞 open 完成；超时保留 handle，成功 stream 在此处立即 Drop。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        let mut state = tokio::time::timeout_at(deadline, self.state.lock())
            .await
            .map_err(|_| AppError::timeout("串口取消打开资源回收锁"))?;
        if state.finished {
            return Ok(());
        }
        let Some(mut task) = state.task.take() else {
            state.finished = true;
            return Ok(());
        };
        let joined = match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(joined) => joined,
            Err(_) => {
                state.task = Some(task);
                return Err(AppError::timeout("串口取消打开等待设备释放"));
            }
        };
        state.finished = true;
        match joined {
            Ok(Ok(stream)) => drop(stream),
            Ok(Err(_)) => {}
            Err(error) => {
                // worker 已结束且栈展开会 Drop 局部资源；首轮报告 panic，重试即可确认已清理。
                return Err(AppError::infrastructure_with_source(
                    "串口取消打开 task 异常退出",
                    error,
                ));
            }
        }
        Ok(())
    }
}

/// 返回拥有所有权的 UTF-8 设备名，以满足阻塞打开 task 的 `'static` 生命周期约束。
///
/// @author kongweiguang
fn serial_device_name(device: &Path) -> AppResult<String> {
    let value = device
        .to_str()
        .ok_or(AppError::invalid("串口设备路径必须是有效文本"))?;
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(AppError::invalid("串口设备路径不能为空或包含控制字符"));
    }
    Ok(value.to_owned())
}

/// 建立两个有限方向的拷贝通道，让 Tokio 串口流的 Drop 成为可靠关闭边界。
///
/// @author kongweiguang
fn make_serial_transport(
    mut serial: SerialStream,
    channel_buffer_bytes: usize,
) -> SessionTransport {
    let (terminal, mut bridge) = io::duplex(channel_buffer_bytes);
    let (reader, writer) = io::split(terminal);
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = task_cancellation.cancelled() => Ok(SessionExitStatus {
                code: None,
                signal: Some("serial session closed".to_owned()),
                success: true,
            }),
            result = io::copy_bidirectional(&mut serial, &mut bridge) => {
                result.map(|_| SessionExitStatus {
                    code: None,
                    signal: Some("serial device disconnected".to_owned()),
                    success: true,
                }).map_err(|error| {
                    AppError::infrastructure_with_source("串口会话读写失败", error)
                })
            }
        }
    });
    let control = SerialSessionTransportControl {
        cancellation,
        state: Mutex::new(SerialControlState {
            task: Some(task),
            finished: None,
        }),
    };
    SessionTransport::new(reader, writer, control)
}

/// 串口设备读取与终端输出共享同一个 Tokio task，避免双向写竞争和轮询线程。
struct SerialSessionTransportControl {
    cancellation: CancellationToken,
    state: Mutex<SerialControlState>,
}

/// 串行化对 join handle 的消费，并缓存首次终态供重复 close/wait 调用。
struct SerialControlState {
    task: Option<JoinHandle<AppResult<SessionExitStatus>>>,
    finished: Option<AppResult<SessionExitStatus>>,
}

#[async_trait]
impl SessionTransportControl for SerialSessionTransportControl {
    /// 调整终端 emulator 的显示尺寸；串口线路本身没有 PTY 的窗口尺寸控制协议。
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        if self.cancellation.is_cancelled() {
            return Err(AppError::conflict("串口会话已关闭"));
        }
        Ok(())
    }

    /// 用调用方统一的截止时间取消双向拷贝；超时后保留 task 句柄供下一次关闭继续 join。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.cancellation.cancel();
        self.finish_until(Some(deadline)).await.map(|_| ())
    }

    /// 等待设备断开或显式关闭，不把设备访问错误伪装成正常终端退出。
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        self.finish().await
    }
}

impl SerialSessionTransportControl {
    /// 缓存 task 终态，使 close 与 wait_exit 可重复调用且不丢失首次错误。
    async fn finish(&self) -> AppResult<SessionExitStatus> {
        self.finish_until(None).await
    }

    /// 按同一绝对截止时间获取状态锁并 join；超时路径不消费仍在运行的 JoinHandle。
    ///
    /// @author kongweiguang
    async fn finish_until(
        &self,
        deadline: Option<tokio::time::Instant>,
    ) -> AppResult<SessionExitStatus> {
        let mut state = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, self.state.lock())
                .await
                .map_err(|_| AppError::timeout("串口关闭等待状态锁"))?,
            None => self.state.lock().await,
        };
        if let Some(result) = &state.finished {
            return result.clone();
        }
        let Some(mut task) = state.task.take() else {
            let result = Err(AppError::infrastructure("串口会话缺少终态"));
            state.finished = Some(result.clone());
            return result;
        };
        let joined = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(joined) => joined,
                Err(_) => {
                    state.task = Some(task);
                    return Err(AppError::timeout("串口设备句柄释放"));
                }
            },
            None => task.await,
        };
        let result = match joined {
            Ok(result) => result,
            Err(error) => Err(AppError::infrastructure_with_source(
                "串口会话 task 异常退出",
                error,
            )),
        };
        state.finished = Some(result.clone());
        result
    }
}

/// 使用 tokio-serial 只读枚举系统当前串口，并将第三方类型映射成应用 DTO。
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSerialPortRepository;

#[async_trait]
impl SerialPortRepository for TokioSerialPortRepository {
    /// 把可能阻塞的系统设备枚举移出 Tokio worker，取消后不会杀掉共享 runtime 线程。
    async fn list(&self, cancel: CancellationToken) -> AppResult<Vec<SerialPortDescriptor>> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("串口设备枚举已取消"));
        }
        let ports = tokio::task::spawn_blocking(tokio_serial::available_ports)
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("串口设备枚举 task 异常退出", error)
            })?
            .map_err(|error| AppError::infrastructure_with_source("无法枚举串口设备", error))?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("串口设备枚举已取消"));
        }
        Ok(ports
            .into_iter()
            .map(|port| {
                let (manufacturer, product, vendor_id, product_id) = match port.port_type {
                    tokio_serial::SerialPortType::UsbPort(info) => (
                        info.manufacturer,
                        info.product,
                        Some(info.vid),
                        Some(info.pid),
                    ),
                    _ => (None, None, None, None),
                };
                SerialPortDescriptor {
                    name: port.port_name,
                    manufacturer,
                    product,
                    vendor_id,
                    product_id,
                }
            })
            .collect())
    }
}

#[cfg(test)]
#[path = "../../tests/support/serial_options.rs"]
mod tests;
