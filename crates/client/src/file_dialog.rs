//! Native file dialogs. Call from a worker so the T7 window stays responsive.

use std::path::PathBuf;

#[cfg(windows)]
mod platform {
    use super::PathBuf;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_OVERWRITEPROMPT,
        FOS_PATHMUSTEXIST, FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
        SIGDN_FILESYSPATH,
    };
    use windows::core::PCWSTR;

    struct Apartment;

    impl Apartment {
        fn new() -> windows::core::Result<Self> {
            // The dialog worker owns this apartment for the life of its COM objects.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
            Ok(Self)
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    fn show(dialog: &IFileDialog) -> windows::core::Result<bool> {
        match unsafe { dialog.Show(None) } {
            Ok(()) => Ok(true),
            Err(e) if e.code().0 as u32 == 0x8007_04c7 => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn path(item: &IShellItem) -> windows::core::Result<PathBuf> {
        // The shell owns the allocation contract. Copy before freeing it.
        unsafe {
            let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            let result = PathBuf::from(OsString::from_wide(name.as_wide()));
            CoTaskMemFree(Some(name.0.cast()));
            Ok(result)
        }
    }

    pub fn pick(limit: usize) -> Result<Vec<PathBuf>, String> {
        let run = || {
            let _apartment = Apartment::new()?;
            unsafe {
                let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
                dialog.SetOptions(FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR | FOS_FILEMUSTEXIST | FOS_ALLOWMULTISELECT)?;
                if !show(&dialog)? {
                    return Ok(Vec::new());
                }
                let items = dialog.GetResults()?;
                (0..items.GetCount()?.min(limit as u32 + 1)).map(|i| path(&items.GetItemAt(i)?)).collect()
            }
        };
        run().map_err(|e: windows::core::Error| e.to_string())
    }

    pub fn save(name: &str) -> Result<Option<PathBuf>, String> {
        let run = || {
            let _apartment = Apartment::new()?;
            let name: Vec<_> = name.encode_utf16().chain([0]).collect();
            unsafe {
                let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
                dialog.SetOptions(FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR | FOS_OVERWRITEPROMPT | FOS_PATHMUSTEXIST)?;
                dialog.SetFileName(PCWSTR(name.as_ptr()))?;
                if show(&dialog)? { path(&dialog.GetResult()?).map(Some) } else { Ok(None) }
            }
        };
        run().map_err(|e: windows::core::Error| e.to_string())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::PathBuf;
    use std::process::Command;

    fn choose(script: &str, args: &[&str]) -> Result<Option<PathBuf>, String> {
        let output = Command::new("/usr/bin/osascript")
            .args(["-e", script, "--"])
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        let path = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        let path = path.strip_suffix('\n').unwrap_or(&path);
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }

    pub fn pick(_limit: usize) -> Result<Vec<PathBuf>, String> {
        choose(
            r#"try
            return POSIX path of (choose file with prompt "Attach a file to T7")
        on error number -128
            return ""
        end try"#,
            &[],
        )
        .map(|path| path.into_iter().collect())
    }

    pub fn save(name: &str) -> Result<Option<PathBuf>, String> {
        choose(
            r#"on run argv
            try
                return POSIX path of (choose file name with prompt "Save file from T7" default name (item 1 of argv))
            on error number -128
                return ""
            end try
        end run"#,
            &[name],
        )
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::PathBuf;
    use std::process::Command;

    fn choose(args: &[&str]) -> Result<Option<String>, String> {
        let response = Command::new("zenity")
            .arg("--file-selection")
            .args(args)
            .output()
            .map_err(|e| format!("Could not open the file picker (Zenity): {e}"))?;
        match response.status.code() {
            Some(0) => String::from_utf8(response.stdout)
                .map(|s| Some(s.trim_end_matches('\n').to_owned()))
                .map_err(|e| e.to_string()),
            Some(1) => Ok(None),
            _ => Err("The file picker closed unexpectedly".into()),
        }
    }

    pub fn pick(limit: usize) -> Result<Vec<PathBuf>, String> {
        Ok(choose(&["--multiple", "--separator=\u{1f}"])?
            .map_or_else(Vec::new, |paths| paths.split('\u{1f}').take(limit + 1).map(PathBuf::from).collect()))
    }

    pub fn save(name: &str) -> Result<Option<PathBuf>, String> {
        choose(&["--save", "--confirm-overwrite", &format!("--filename={name}")]).map(|path| path.map(PathBuf::from))
    }
}

pub use platform::{pick, save};
