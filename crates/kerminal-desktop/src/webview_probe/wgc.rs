//! 原生验收专用 WGC 抓图：GPU 子窗口不保证能由 PrintWindow 读回。
//!
//! @author kongweiguang

use std::{
    fs, io,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::IDXGIDevice,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::Interface as _,
};

struct Apartment;

impl Drop for Apartment {
    /// 每次成功初始化都在同一后台线程配对释放，不改变 GPUI UI 线程的 COM apartment。
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl Drop for Capture {
    /// 超时、设备丢失和写盘失败同样关闭采集，避免后台捕获资源挂到下一阶段。
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// 只允许当前候选自己的父窗；失败也输出诊断，绝不把引擎截图当成系统合成结果。
pub(super) fn capture(parent: u64, path: &Path, overlay: bool, modal: bool) -> Value {
    match capture_inner(parent, path, overlay, modal) {
        Ok((width, height, count, reported_width, reported_height)) => json!({
            "mode":"windows-graphics-capture", "captured":true,
            "processId":std::process::id(), "parentWindow":parent,
            "file":path.file_name().and_then(|name| name.to_str()),
            "width":width, "height":height, "webViewBodyPixels":count.body,
            "fixturePixels":count, "fixtureAccepted":count.accepted(overlay),
            "itemReportedWidth":reported_width, "itemReportedHeight":reported_height,
        }),
        Err(error) => json!({
            "mode":"windows-graphics-capture", "captured":false,
            "processId":std::process::id(), "parentWindow":parent,
            "error":error.to_string(),
        }),
    }
}

/// 为重复出现的 HRESULT 标明捕获阶段；只记录固定 API 名称与系统错误，不采集页面或用户数据。
fn capture_stage<T>(stage: &str, result: windows::core::Result<T>) -> io::Result<T> {
    result.map_err(|error| io::Error::other(format!("WGC {stage}: {error}")))
}

/// FreeThreaded frame pool 不激活窗口；浮层须同时有网页细节和宿主标记，颜色兼容已知蒙层而非接受空白。
fn capture_inner(
    parent: u64,
    path: &Path,
    overlay: bool,
    modal: bool,
) -> Result<(u32, u32, super::pixels::FixturePixels, i32, i32), Box<dyn std::error::Error>> {
    let mut owner = 0;
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            parent as _,
            &mut owner,
        );
    }
    if owner != std::process::id() {
        return Err(io::Error::other("WGC target is not owned by the acceptance process").into());
    }
    capture_stage("RoInitialize", unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)
    })?;
    let _apartment = Apartment;
    if !capture_stage("IsSupported", GraphicsCaptureSession::IsSupported())? {
        return Err(io::Error::other("Windows Graphics Capture is unavailable").into());
    }
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = capture_stage("CreateForWindow", unsafe {
        interop.CreateForWindow(HWND(parent as _))
    })?;
    let mut dimensions = capture_stage("Item.Size", item.Size())?;
    let reported_width = dimensions.Width;
    let reported_height = dimensions.Height;
    if dimensions.Width <= 0 || dimensions.Height <= 0 {
        let mut native = windows_sys::Win32::Foundation::RECT::default();
        if unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(parent as _, &mut native)
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // 部分桌面的 WGC item 尚未报尺寸；仅用真实 HWND 边界分配缓冲，仍必须获得真实捕获帧。
        dimensions.Width = native.right.saturating_sub(native.left);
        dimensions.Height = native.bottom.saturating_sub(native.top);
    }
    if dimensions.Width <= 0
        || dimensions.Height <= 0
        || dimensions.Width > 8192
        || dimensions.Height > 8192
    {
        return Err(io::Error::other(format!(
            "WGC target has invalid dimensions: {}x{} (item {reported_width}x{reported_height})",
            dimensions.Width, dimensions.Height
        ))
        .into());
    }
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or_else(|| io::Error::other("WGC D3D device missing"))?;
    let context = context.ok_or_else(|| io::Error::other("WGC D3D context missing"))?;
    let dxgi: IDXGIDevice = device.cast()?;
    let runtime_device: IDirect3DDevice =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    let pool = capture_stage(
        "CreateFreeThreaded",
        Direct3D11CaptureFramePool::CreateFreeThreaded(
            &runtime_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            dimensions,
        ),
    )?;
    let session = capture_stage("CreateCaptureSession", pool.CreateCaptureSession(&item))?;
    capture_stage(
        "SetIsCursorCaptureEnabled",
        session.SetIsCursorCaptureEnabled(false),
    )?;
    let capture = Capture { pool, session };
    capture_stage("StartCapture", capture.session.StartCapture())?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut latest = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match capture.pool.TryGetNextFrame() {
            Ok(frame) => {
                let content = frame.ContentSize()?;
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                let pixels =
                    read_texture(&device, &context, &texture, content.Width, content.Height)?;
                frame.Close()?;
                let count = super::pixels::count_fixture_pixels(&pixels, modal);
                latest = Some((content.Width as u32, content.Height as u32, pixels, count));
                if count.accepted(overlay) {
                    break;
                }
            }
            Err(error) => last_error = Some(format!("TryGetNextFrame: {error}")),
        }
        thread::sleep(Duration::from_millis(40));
    }
    let (width, height, pixels, count) = latest.ok_or_else(|| {
        io::Error::other(format!(
            "WGC produced no frame (item {reported_width}x{reported_height}, native pool {}x{}): {}",
            dimensions.Width, dimensions.Height,
            last_error.unwrap_or_default()
        ))
    })?;
    write_bitmap(path, width, height, &pixels)?;
    Ok((width, height, count, reported_width, reported_height))
}

/// 显式 staging readback 保留系统捕获的 BGRA 原始像素，并检查每行 stride 与内容尺寸。
fn read_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: i32,
    height: i32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    if width <= 0
        || height <= 0
        || width as u32 > desc.Width
        || height as u32 > desc.Height
        || width > 8192
        || height > 8192
    {
        return Err(io::Error::other("WGC frame exceeds its texture allocation").into());
    }
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
    let staging = staging.ok_or_else(|| io::Error::other("WGC staging texture missing"))?;
    unsafe { context.CopyResource(&staging, texture) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let row_size = width as usize * 4;
    if mapped.pData.is_null() || (mapped.RowPitch as usize) < row_size {
        unsafe { context.Unmap(&staging, 0) };
        return Err(io::Error::other("WGC returned an invalid pixel stride").into());
    }
    let mut pixels = Vec::with_capacity(row_size * height as usize);
    for row in 0..height as usize {
        let source = unsafe {
            std::slice::from_raw_parts(
                mapped
                    .pData
                    .cast::<u8>()
                    .add(row * mapped.RowPitch as usize),
                row_size,
            )
        };
        pixels.extend_from_slice(source);
    }
    unsafe { context.Unmap(&staging, 0) };
    Ok(pixels)
}

/// 无编码依赖的 top-down BMP 只存真实捕获像素，便于外部 runner 独立校验和导出 PNG。
fn write_bitmap(path: &Path, width: u32, height: u32, pixels: &[u8]) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(54 + pixels.len());
    bytes.extend_from_slice(b"BM");
    bytes.extend_from_slice(&(54 + pixels.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&54u32.to_le_bytes());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&(width as i32).to_le_bytes());
    bytes.extend_from_slice(&(-(height as i32)).to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 24]);
    bytes.extend_from_slice(pixels);
    fs::write(path, bytes)
}
