mod capture;
mod convert;
mod decode;
mod encode;
mod share;

pub use capture::sources;
pub use decode::Decoder;
pub use share::Share;

use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFShutdown, MFStartup};
use windows::Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

trait Context<T> {
    fn context(self, what: &str) -> Result<T, String>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn context(self, what: &str) -> Result<T, String> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

struct Runtime(CO_MTA_USAGE_COOKIE);

impl Runtime {
    fn start() -> Result<Runtime, String> {
        let cookie = unsafe { CoIncrementMTAUsage() }.context("starting COM")?;
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            let _ = unsafe { CoDecrementMTAUsage(cookie) };
            return Err(format!("starting Media Foundation: {e}"));
        }
        Ok(Runtime(cookie))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            let _ = CoDecrementMTAUsage(self.0);
        }
    }
}

fn now_100ns() -> i64 {
    let (mut count, mut freq) = (0, 1);
    // SAFETY: plain out-pointer calls, which cannot fail since Windows XP.
    unsafe {
        let _ = QueryPerformanceCounter(&raw mut count);
        let _ = QueryPerformanceFrequency(&raw mut freq);
    }
    (i128::from(count) * 10_000_000 / i128::from(freq)) as i64
}
