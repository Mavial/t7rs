mod capture;
mod convert;
mod decode;
mod encode;
mod share;

pub use capture::sources;
pub use decode::Decoder;
pub use share::Share;

use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

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

const TICKS_PER_SECOND: i64 = 10_000_000;

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

struct Running<T> {
    objects: ManuallyDrop<T>,
    _runtime: Runtime,
}

impl<T> Running<T> {
    fn start(make: impl FnOnce() -> Result<T, String>) -> Result<Running<T>, String> {
        let runtime = Runtime::start()?;
        Ok(Running { objects: ManuallyDrop::new(make()?), _runtime: runtime })
    }
}

impl<T> Deref for Running<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.objects
    }
}

impl<T> DerefMut for Running<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.objects
    }
}

impl<T> Drop for Running<T> {
    fn drop(&mut self) {
        // SAFETY: dropped once, here, before the runtime it needs goes.
        unsafe { ManuallyDrop::drop(&mut self.objects) }
    }
}

fn now_100ns() -> i64 {
    let (mut count, mut freq) = (0, 1);
    // SAFETY: plain out-pointer calls, which cannot fail since Windows XP.
    unsafe {
        let _ = QueryPerformanceCounter(&raw mut count);
        let _ = QueryPerformanceFrequency(&raw mut freq);
    }
    (i128::from(count) * i128::from(TICKS_PER_SECOND) / i128::from(freq)) as i64
}
