use std::sync::mpsc::Sender;

use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowTextW, IsWindow,
    IsWindowVisible, MONITORINFOF_PRIMARY, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Interface};

use super::super::{Handle, Source, SourceInfo};
use super::Context;
use super::share::Wake;

pub fn sources() -> Vec<SourceInfo> {
    let mut monitors: Vec<(bool, RECT, HMONITOR)> = Vec::new();
    // SAFETY: the callback only runs during the call and gets `monitors` back.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(add_monitor), LPARAM(&raw mut monitors as isize));
    }
    monitors.sort_by_key(|(primary, ..)| !primary);
    let mut list: Vec<SourceInfo> = (monitors.into_iter().enumerate())
        .map(|(i, (_, r, monitor))| SourceInfo {
            source: Source::Monitor(Handle(monitor.0 as isize)),
            name: format!("Screen {} ({} × {})", i + 1, r.right - r.left, r.bottom - r.top),
        })
        .collect();
    unsafe {
        let _ = EnumWindows(Some(add_window), LPARAM(&raw mut list as isize));
    }
    list
}

unsafe extern "system" fn add_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<(bool, RECT, HMONITOR)>) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        list.push((info.dwFlags & MONITORINFOF_PRIMARY != 0, info.rcMonitor, monitor));
    }
    true.into()
}

unsafe extern "system" fn add_window(window: HWND, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<SourceInfo>) };
    if let Some(name) = capturable(window) {
        list.push(SourceInfo { source: Source::Window(Handle(window.0 as isize)), name });
    }
    true.into()
}

/// The title of a window people would recognise as an app: visible, not
/// cloaked (other virtual desktops, suspended store apps), not a tool window
/// or an owned dialog, not the desktop.
fn capturable(window: HWND) -> Option<String> {
    unsafe {
        if !IsWindowVisible(window).as_bool() || window == GetShellWindow() {
            return None;
        }
        if GetWindow(window, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return None;
        }
        if GetWindowLongPtrW(window, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0 {
            return None;
        }
        let mut cloaked = 0u32;
        let size = size_of::<u32>() as u32;
        if DwmGetWindowAttribute(window, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size).is_ok() && cloaked != 0 {
            return None;
        }
        let mut title = [0u16; 256];
        let len = GetWindowTextW(window, &mut title);
        let title = String::from_utf16_lossy(&title[..len.max(0) as usize]);
        (!title.trim().is_empty()).then_some(title)
    }
}

pub struct Capture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    source: Source,
    device: IDirect3DDevice,
    size: SizeInt32,
}

impl Capture {
    pub fn start(d3d: &ID3D11Device, source: Source, fps: u32, wake: &Sender<Wake>) -> Result<Capture, String> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("Windows Graphics Capture is not available")?;
        // SAFETY: the handles came from `sources`; a stale one fails here.
        let item: GraphicsCaptureItem = unsafe {
            match source {
                Source::Monitor(Handle(h)) => interop.CreateForMonitor(HMONITOR(h as _)),
                Source::Window(Handle(h)) => interop.CreateForWindow(HWND(h as _)),
            }
        }
        .context("cannot capture this source")?;
        let dxgi: IDXGIDevice = d3d.cast().context("DXGI device")?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .context("WinRT Direct3D device")?;
        let size = item.Size().context("capture size")?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .context("capture frame pool")?;
        let frame = wake.clone();
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            let _ = frame.send(Wake::Frame);
            Ok(())
        }))
        .context("frame handler")?;
        let session = pool.CreateCaptureSession(&item).context("capture session")?;
        // Newer Windows only; each is fine to miss.
        let _ = session.SetIsCursorCaptureEnabled(true);
        if GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
            .and_then(|a| a.join())
            .is_ok()
        {
            let _ = session.SetIsBorderRequired(false);
        }
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 7_500_000 / i64::from(fps) });
        session.StartCapture().context("starting capture")?;
        Ok(Capture { session, pool, source, device, size })
    }

    /// The source still exists. Polled, because the capture item's Closed
    /// event did not fire when a captured window closed (Windows 11 26300).
    pub fn alive(&self) -> bool {
        // SAFETY: plain queries; a stale handle fails them.
        unsafe {
            match self.source {
                Source::Window(Handle(h)) => IsWindow(Some(HWND(h as _))).as_bool(),
                Source::Monitor(Handle(h)) => {
                    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                    GetMonitorInfoW(HMONITOR(h as _), &raw mut info).as_bool()
                }
            }
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.size.Width as u32, self.size.Height as u32)
    }

    /// The newest frame waiting, dropping older ones so their buffers go back
    /// to the pool. Follows the source's size for the frames after it.
    pub fn newest(&mut self) -> Result<Option<Direct3D11CaptureFrame>, String> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        if let Some(frame) = &newest {
            let size = frame.ContentSize().context("frame size")?;
            if size != self.size && size.Width > 0 && size.Height > 0 {
                (self.pool.Recreate(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size))
                    .context("resizing the frame pool")?;
                self.size = size;
            }
        }
        Ok(newest)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// A frame's pixels, with the size of its valid top-left part.
pub fn texture(frame: &Direct3D11CaptureFrame) -> Result<(ID3D11Texture2D, (u32, u32)), String> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface().and_then(|s| s.cast()).context("frame surface")?;
    // SAFETY: a capture surface wraps a D3D11 texture.
    let texture = unsafe { access.GetInterface::<ID3D11Texture2D>() }.context("frame texture")?;
    let size = frame.ContentSize().context("frame size")?;
    Ok((texture, (size.Width.max(0) as u32, size.Height.max(0) as u32)))
}
