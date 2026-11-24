//! Windows: Windows Graphics Capture into D3D11, the GPU's video processor
//! for scaling and NV12, Media Foundation's H.264 encoder and decoder. None
//! of it is touched until a share or decoder starts.

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

/// COM's multithreaded apartment and Media Foundation, kept up while a share
/// or decoder lives. Declare it as the last field so it outlives the objects.
struct Runtime(CO_MTA_USAGE_COOKIE);

impl Runtime {
    fn start() -> Result<Runtime, String> {
        // SAFETY: plain calls; each success is undone once in `drop`.
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
        // SAFETY: balances `start`.
        unsafe {
            let _ = MFShutdown();
            let _ = CoDecrementMTAUsage(self.0);
        }
    }
}

/// The performance counter in 100 ns units, the clock capture frames are stamped with.
fn now_100ns() -> i64 {
    let (mut count, mut freq) = (0, 1);
    // SAFETY: plain out-pointer calls, which cannot fail since Windows XP.
    unsafe {
        let _ = QueryPerformanceCounter(&raw mut count);
        let _ = QueryPerformanceFrequency(&raw mut freq);
    }
    (i128::from(count) * 10_000_000 / i128::from(freq)) as i64
}
