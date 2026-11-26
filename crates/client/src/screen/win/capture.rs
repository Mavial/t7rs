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
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetClientRect, GetShellWindow, GetWindow,
    GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, MONITORINFOF_PRIMARY, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Interface, Owned};

use super::super::{Handle, LeagueWindow, Source, SourceInfo};
use super::share::Wake;
use super::{Context, TICKS_PER_SECOND};

const MIN_UPDATE_INTERVAL_AT_1_FPS: i64 = TICKS_PER_SECOND * 3 / 4;

pub fn sources() -> Vec<SourceInfo> {
    let mut monitors: Vec<(bool, RECT, HMONITOR)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(add_monitor), lparam(&mut monitors));
    }
    monitors.sort_by_key(|(primary, ..)| !primary);
    let mut list: Vec<SourceInfo> = (monitors.into_iter().enumerate())
        .map(|(i, (_, r, monitor))| SourceInfo {
            source: Source::Monitor(Handle(monitor.0 as isize)),
            name: format!("Screen {} ({} × {})", i + 1, r.right - r.left, r.bottom - r.top),
        })
        .collect();
    if let Some(window) = list_windows(Some(&mut list)) {
        list.push(SourceInfo { source: Source::League(window), name: "League of Legends".into() });
    }
    list
}

fn lparam<T>(list: &mut T) -> LPARAM {
    LPARAM(&raw mut *list as isize)
}

/// # Safety
///
/// `list` is what [`lparam`] made of a `T` for the enumeration calling back.
unsafe fn handed<'a, T>(list: LPARAM) -> &'a mut T {
    // SAFETY: the enumeration calls back synchronously on the borrowing thread.
    unsafe { &mut *(list.0 as *mut T) }
}

unsafe extern "system" fn add_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
    let list: &mut Vec<(bool, RECT, HMONITOR)> = unsafe { handed(list) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        list.push((info.dwFlags & MONITORINFOF_PRIMARY != 0, info.rcMonitor, monitor));
    }
    true.into()
}

struct WindowList<'a> {
    ordinary: Option<&'a mut Vec<SourceInfo>>,
    league_processes: Vec<(u32, bool)>,
    league: Option<LeagueWindow>,
}

fn league_processes() -> Vec<(u32, bool)> {
    let mut league = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return league };
        let snapshot = Owned::new(snapshot);
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut read = Process32FirstW(*snapshot, &raw mut entry);
        while read.is_ok() {
            let name = entry.szExeFile.split(|&c| c == 0).next().unwrap_or_default();
            let name = String::from_utf16_lossy(name);
            let in_game = name.eq_ignore_ascii_case("League of Legends.exe");
            if in_game || name.eq_ignore_ascii_case("LeagueClientUx.exe") {
                league.push((entry.th32ProcessID, in_game));
            }
            read = Process32NextW(*snapshot, &raw mut entry);
        }
    }
    league
}

pub(super) fn list_windows(ordinary: Option<&mut Vec<SourceInfo>>) -> Option<LeagueWindow> {
    let mut list = WindowList { ordinary, league_processes: league_processes(), league: None };
    if list.ordinary.is_none() && list.league_processes.is_empty() {
        return None;
    }
    unsafe {
        let _ = EnumWindows(Some(add_window), lparam(&mut list));
    }
    list.league
}

pub(super) fn window_pid(window: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut pid)) };
    pid
}

unsafe extern "system" fn add_window(window: HWND, list: LPARAM) -> BOOL {
    let list: &mut WindowList<'_> = unsafe { handed(list) };
    let pid = window_pid(window);
    let handle = Handle(window.0 as isize);
    if let Some(&(_, in_game)) = list.league_processes.iter().find(|&&(id, _)| id == pid) {
        if list.league.is_none_or(|old| in_game && !old.in_game) && capturable(window).is_some() {
            list.league = Some(LeagueWindow { handle, pid, in_game });
        }
    } else if let Some(ordinary) = &mut list.ordinary
        && let Some(name) = capturable(window)
    {
        ordinary.push(SourceInfo { source: Source::Window(handle), name });
    }
    true.into()
}

/// The title of a window people would recognise as an app: visible and
/// restored, not cloaked (other virtual desktops, suspended store apps), not a
/// tool window, panel or owned dialog, not excluded from capture, not the
/// desktop or taskbar.
fn capturable(window: HWND) -> Option<String> {
    unsafe {
        if !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() || window == GetShellWindow() {
            return None;
        }
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        if style & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
            return None;
        }
        if style & WS_EX_APPWINDOW.0 == 0 && GetWindow(window, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return None;
        }
        let mut cloaked = 0u32;
        let size = size_of::<u32>() as u32;
        if DwmGetWindowAttribute(window, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size).is_ok() && cloaked != 0 {
            return None;
        }
        let mut bounds = RECT::default();
        if GetClientRect(window, &raw mut bounds).is_ok()
            && (bounds.right <= bounds.left || bounds.bottom <= bounds.top)
        {
            return None;
        }
        let mut affinity = 0;
        if GetWindowDisplayAffinity(window, &raw mut affinity).is_ok() && affinity != 0 {
            return None;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(window, &mut class);
        if matches!(
            String::from_utf16_lossy(&class[..len as usize]).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return None;
        }
        let length = GetWindowTextLengthW(window);
        if length <= 0 {
            return None;
        }
        let mut title = vec![0u16; length.min(32767) as usize + 1];
        let len = GetWindowTextW(window, &mut title);
        let title = String::from_utf16_lossy(&title[..len.max(0) as usize]);
        (!title.trim().is_empty()).then_some(title)
    }
}

pub struct Capture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    pub(super) source: Source,
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
                Source::League(w) => interop.CreateForWindow(HWND(w.handle.0 as _)),
            }
        }
        .context("cannot capture this source")?;
        if let Source::League(w) = source
            && window_pid(HWND(w.handle.0 as _)) != w.pid
        {
            return Err("the League of Legends window closed".into());
        }
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
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: MIN_UPDATE_INTERVAL_AT_1_FPS / i64::from(fps) });
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
                Source::League(w) => window_pid(HWND(w.handle.0 as _)) == w.pid,
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
