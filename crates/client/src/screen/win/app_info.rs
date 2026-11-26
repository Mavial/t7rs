use std::path::Path;
use std::ptr::null_mut;
use std::slice;

use egui::{Color32, ColorImage};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
    GdiFlush, GetDIBits, GetObjectW, HDC, SelectObject,
};
use windows::Win32::Storage::EnhancedStorage::{PKEY_AppUserModel_ID, PKEY_FileDescription};
use windows::Win32::Storage::Packaging::Appx::{APPLICATION_USER_MODEL_ID_MAX_LENGTH, GetApplicationUserModelId};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{
    IShellItem, IShellItem2, IShellItemImageFactory, SHCreateItemFromParsingName, SHDefExtractIconW,
    SIGDN_NORMALDISPLAY, SIIGBF_ICONONLY, SIIGBF_SCALEUP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DI_NORMAL, DrawIconEx, FindWindowExW, GCLP_HICON, GCLP_HICONSM, GetClassLongPtrW, GetClassNameW,
    GetWindowThreadProcessId, HICON, ICON_BIG, ICON_SMALL, SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT,
    SendMessageTimeoutW, WM_GETICON,
};
use windows::core::{BSTR, HSTRING, Interface, Owned, PCWSTR, PWSTR, w};

const ICON_SIZE: i32 = 32;
const ICON_PIXELS: usize = (ICON_SIZE * ICON_SIZE) as usize;

pub(super) struct AppInfo {
    pub key: Option<String>,
    shell_item: Option<IShellItem>,
    path: Option<String>,
}

impl AppInfo {
    pub fn for_window(window: HWND) -> Self {
        let process = app_process(window);
        let id = window_app_id(window).or_else(|| process.as_deref().and_then(process_app_id));
        let shell_item = id.as_deref().and_then(apps_folder_item);
        let path = process.as_deref().and_then(executable_path);
        let key = id.or_else(|| path.as_deref().map(normalized_path));
        Self { key, shell_item, path }
    }

    pub fn name(&self) -> Option<String> {
        if let Some(name) = self
            .shell_item
            .as_ref()
            .and_then(|item| unsafe { task_string(item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?) })
        {
            return Some(name);
        }
        let path = self.path.as_deref()?;
        let file: Option<IShellItem2> = unsafe { SHCreateItemFromParsingName(&HSTRING::from(path), None).ok() };
        file.and_then(|file| unsafe { task_string(file.GetString(&PKEY_FileDescription).ok()?) })
            .or_else(|| Path::new(path).file_stem()?.to_str().map(str::to_owned))
    }

    pub fn icon(&self, window: HWND) -> Option<ColorImage> {
        self.shell_item
            .as_ref()
            .and_then(shell_image)
            .or_else(|| window_icon(window).and_then(|icon| render_icon(*icon)))
            .or_else(|| file_icon(self.path.as_deref()?).and_then(|icon| render_icon(*icon)))
    }
}

pub(super) struct Apartment;

impl Apartment {
    pub(super) fn enter() -> Option<Self> {
        // If this thread is already in an MTA, the shell calls can use the existing apartment.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok().then(|| Self) }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

unsafe fn task_string(value: PWSTR) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let string = unsafe { value.to_string().ok() };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    string.filter(|s| !s.trim().is_empty())
}

fn window_app_id(window: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(window).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let id = BSTR::try_from(&value).ok()?;
        utf16_string(&id)
    }
}

fn app_process(window: HWND) -> Option<Owned<HANDLE>> {
    let mut class = [0; 32];
    let len = unsafe { GetClassNameW(window, &mut class) } as usize;
    let frame_class = w!("ApplicationFrameWindow");
    let app_window = if &class[..len] == unsafe { frame_class.as_wide() } {
        // An ApplicationFrameHost window can contain a packaged app from another process.
        unsafe { FindWindowExW(Some(window), None, w!("Windows.UI.Core.CoreWindow"), PCWSTR::null()).ok()? }
    } else {
        window
    };
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(app_window, Some(&raw mut pid));
        Some(Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?))
    }
}

fn process_app_id(process: &HANDLE) -> Option<String> {
    let mut id = [0; APPLICATION_USER_MODEL_ID_MAX_LENGTH as usize];
    let mut len = id.len() as u32;
    let result = unsafe { GetApplicationUserModelId(*process, &raw mut len, Some(PWSTR(id.as_mut_ptr()))) };
    if result == ERROR_SUCCESS { utf16_string(id.get(..len as usize)?) } else { None }
}

fn executable_path(process: &HANDLE) -> Option<String> {
    let mut path = vec![0; 512];
    let query = |path: &mut [u16]| {
        let mut len = path.len() as u32;
        unsafe { QueryFullProcessImageNameW(*process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &raw mut len) }
            .map(|()| len as usize)
    };
    let len = query(&mut path)
        .or_else(|error| {
            if error.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult() {
                return Err(error);
            }
            path.resize(32_768, 0);
            query(&mut path)
        })
        .ok()?;
    String::from_utf16(path.get(..len)?).ok().filter(|path| !path.is_empty())
}

fn normalized_path(path: &str) -> String {
    let path = path.replace('/', "\\").to_lowercase();
    if let Some(unc) = path.strip_prefix(r"\\?\unc\") {
        format!(r"\\{unc}")
    } else if let Some(drive_path) = path.strip_prefix(r"\\?\").filter(|p| p.as_bytes().get(1) == Some(&b':')) {
        drive_path.to_owned()
    } else {
        path
    }
}

fn apps_folder_item(app_id: &str) -> Option<IShellItem> {
    if app_id.contains(['\\', '/', ':']) || matches!(app_id, "." | "..") {
        return None;
    }
    let path = HSTRING::from(format!(r"shell:AppsFolder\{app_id}"));
    unsafe { SHCreateItemFromParsingName(&path, None).ok() }
}

fn utf16_string(value: &[u16]) -> Option<String> {
    let end = value.iter().position(|&unit| unit == 0).unwrap_or(value.len());
    let string = String::from_utf16(&value[..end]).ok()?;
    let trimmed = string.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn window_icon(window: HWND) -> Option<Owned<HICON>> {
    unsafe {
        for size in [ICON_BIG, ICON_SMALL] {
            let mut handle = 0;
            let response = SendMessageTimeoutW(
                window,
                WM_GETICON,
                WPARAM(size as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
                25,
                Some(&raw mut handle),
            );
            if response.0 == 0 {
                break;
            }
            if handle != 0
                && let Ok(copy) = CopyIcon(HICON(handle as _))
            {
                return Some(Owned::new(copy));
            }
        }
        for index in [GCLP_HICON, GCLP_HICONSM] {
            let handle = HICON(GetClassLongPtrW(window, index) as _);
            if !handle.is_invalid()
                && let Ok(copy) = CopyIcon(handle)
            {
                return Some(Owned::new(copy));
            }
        }
    }
    None
}

fn file_icon(path: &str) -> Option<Owned<HICON>> {
    let path = HSTRING::from(path);
    let mut icon = HICON::default();
    unsafe {
        let status = SHDefExtractIconW(&path, 0, 0, Some(&raw mut icon), None, ICON_SIZE as u32);
        let icon = Owned::new(icon);
        (status.is_ok() && !icon.is_invalid()).then_some(icon)
    }
}

struct MemoryDc(HDC);

impl MemoryDc {
    fn new() -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(None) };
        (!dc.is_invalid()).then(|| Self(dc))
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        let _ = unsafe { DeleteDC(self.0) };
    }
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn shell_image(item: &IShellItem) -> Option<ColorImage> {
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let size = SIZE { cx: ICON_SIZE, cy: ICON_SIZE };
    unsafe {
        let bitmap = Owned::new(factory.GetImage(size, SIIGBF_ICONONLY | SIIGBF_SCALEUP).ok()?);
        let mut object = BITMAP::default();
        if GetObjectW((*bitmap).into(), size_of::<BITMAP>() as i32, Some((&raw mut object).cast())) == 0 {
            return None;
        }
        let (width, height) = (object.bmWidth, object.bmHeight);
        if !(1..=ICON_SIZE).contains(&width) || !(1..=ICON_SIZE).contains(&height) {
            return None;
        }
        let dc = MemoryDc::new()?;
        let mut info = bitmap_info(width, height);
        let mut bgra = vec![[0u8; 4]; (width * height) as usize];
        if GetDIBits(dc.0, *bitmap, 0, height as u32, Some(bgra.as_mut_ptr().cast()), &raw mut info, DIB_RGB_COLORS)
            != height
        {
            return None;
        }
        let has_alpha = bgra.iter().any(|pixel| pixel[3] != 0);
        let pixels = bgra
            .into_iter()
            .map(|[b, g, r, a]| Color32::from_rgba_premultiplied(r, g, b, if has_alpha { a } else { 255 }))
            .collect();
        Some(ColorImage::new([width as usize, height as usize], pixels))
    }
}

fn render_icon(icon: HICON) -> Option<ColorImage> {
    unsafe {
        let mut bits = null_mut();
        let info = bitmap_info(ICON_SIZE, ICON_SIZE);
        let bitmap = Owned::new(CreateDIBSection(None, &raw const info, DIB_RGB_COLORS, &raw mut bits, None, 0).ok()?);
        // The DC drops first, releasing the selected bitmap before it is deleted.
        let dc = MemoryDc::new()?;
        if bits.is_null() || SelectObject(dc.0, (*bitmap).into()).is_invalid() {
            return None;
        }
        let bits = bits.cast::<[u8; 4]>();
        let draw = |background| {
            bits.cast::<u8>().write_bytes(background, ICON_PIXELS * 4);
            DrawIconEx(dc.0, 0, 0, icon, ICON_SIZE, ICON_SIZE, 0, None, DI_NORMAL).ok()?;
            GdiFlush().as_bool().then(|| slice::from_raw_parts(bits, ICON_PIXELS))
        };
        // The difference between white and black composites gives alpha for both masks and 32-bit icons.
        let black = draw(0)?.to_vec();
        let white = draw(255)?;
        let pixels = black
            .into_iter()
            .zip(white)
            .map(|([b, g, r, _], &[wb, _, _, _])| {
                let a = 255 - wb.saturating_sub(b);
                Color32::from_rgba_premultiplied(r, g, b, a)
            })
            .collect();
        Some(ColorImage::new([ICON_SIZE as usize, ICON_SIZE as usize], pixels))
    }
}
