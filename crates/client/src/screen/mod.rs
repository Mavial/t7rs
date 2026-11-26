#[cfg(any(windows, test))]
mod annexb;
#[cfg(any(windows, test))]
mod nv12;
#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
use unsupported as platform;
#[cfg(windows)]
use win as platform;

use std::time::Duration;

pub mod packet;
pub mod scale;

/// One encoded H.264 frame in Annex B form, the unit the encoder emits and
/// the decoder takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// Counts up by one per frame on each share.
    pub frame: u32,
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), expect(dead_code, reason = "only the Windows backend lists sources"))]
pub enum Source {
    Monitor(Handle),
    Window(Handle),
    League(LeagueWindow),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeagueWindow {
    handle: Handle,
    pid: u32,
    in_game: bool,
}

impl Source {
    #[cfg_attr(not(windows), allow(dead_code, reason = "only the examples select windows on this platform"))]
    pub fn is_window(self) -> bool {
        !matches!(self, Self::Monitor(_))
    }

    #[cfg(windows)]
    fn handle(self) -> Handle {
        match self {
            Self::Monitor(h) | Self::Window(h) => h,
            Self::League(w) => w.handle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(isize);

#[cfg(test)]
impl Handle {
    pub fn fake(id: isize) -> Self {
        Self(id)
    }
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub source: Source,
    pub name: String,
}

pub(crate) struct AppWindows {
    pub name: String,
    pub icon: Option<egui::ColorImage>,
    pub windows: Vec<SourceInfo>,
}

#[derive(Default)]
pub(crate) struct PickerSources {
    pub screens: Vec<SourceInfo>,
    pub apps: Vec<AppWindows>,
}

pub(crate) fn picker_sources() -> PickerSources {
    platform::picker_sources()
}

/// Monitors (the primary first), then the windows that can be captured.
/// Empty where screen sharing is not supported.
#[cfg_attr(not(windows), allow(dead_code, reason = "only the examples list ungrouped sources on this platform"))]
pub fn sources() -> Vec<SourceInfo> {
    platform::sources()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Quality {
    #[serde(rename = "720p30")]
    P720_30,
    #[default]
    #[serde(rename = "1080p30")]
    P1080_30,
    #[serde(rename = "1080p60")]
    P1080_60,
}

#[cfg_attr(not(windows), expect(dead_code, reason = "only the Windows backend encodes"))]
struct Params {
    max: (u32, u32),
    fps: u32,
    bitrate: u32,
}

impl Quality {
    fn params(self) -> Params {
        match self {
            Quality::P720_30 => Params { max: (1280, 720), fps: 30, bitrate: 2_500_000 },
            Quality::P1080_30 => Params { max: (1920, 1080), fps: 30, bitrate: 4_000_000 },
            Quality::P1080_60 => Params { max: (1920, 1080), fps: 60, bitrate: 6_000_000 },
        }
    }
}

/// The encoded size for a `source`-sized picture: scaled down to fit `max`
/// with its aspect ratio kept, in even dimensions as 4:2:0 needs.
#[cfg(any(windows, test))]
fn fit(source: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let scale =
        (f64::from(max.0) / f64::from(source.0.max(1))).min(f64::from(max.1) / f64::from(source.1.max(1))).min(1.0);
    let even = |n: u32, max: u32| ((f64::from(n) * scale / 2.0).round() as u32 * 2).min(n.min(max) & !1).max(2);
    (even(source.0, max.0), even(source.1, max.1))
}

/// A running share: captures `source`, encodes it and hands each access unit
/// to the sink on the share's own thread. Dropping it stops the share.
pub struct Share(platform::Share);

impl Share {
    /// `ended` runs on the share's thread once it stops, for any reason.
    pub fn start(
        source: Source,
        quality: Quality,
        sink: impl FnMut(AccessUnit) + Send + 'static,
        ended: impl FnOnce() + Send + 'static,
    ) -> Result<Share, String> {
        platform::Share::start(source, quality.params(), Box::new(sink), Box::new(ended)).map(Share)
    }

    /// Makes the next access unit a keyframe, also when the picture is still.
    pub fn request_keyframe(&self) {
        self.0.request_keyframe();
    }

    /// The source went away (window closed, display lost) or capture failed.
    pub fn ended(&self) -> bool {
        self.0.ended()
    }

    pub fn encoder(&self) -> &str {
        self.0.encoder()
    }

    pub fn take_stats(&self) -> ShareStats {
        self.0.take_stats()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ShareStats {
    /// Frames the capture delivered, including ones skipped to hold the frame rate.
    pub captured: u32,
    pub encoded: u32,
    pub bytes: u64,
    /// From the capture handing over a frame to its access unit leaving the encoder.
    pub latency_total: Duration,
    pub latency_max: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// `0x00RRGGBB`, row by row, as softbuffer takes them.
    pub pixels: Vec<u32>,
}

/// Turns one share's access units back into pictures. The first one it gets
/// must be a keyframe, and each after it the next frame.
pub struct Decoder {
    platform: platform::Decoder,
    failed: bool,
    spare: Picture,
}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        Ok(Decoder { platform: platform::Decoder::new()?, failed: false, spare: Picture::default() })
    }

    /// The picture `au` completes, if any. After an error nothing is decoded
    /// until the next keyframe, so ask the sharer for one.
    pub fn decode(&mut self, au: &AccessUnit) -> Result<Option<Picture>, String> {
        if self.failed && !au.key {
            return Ok(None);
        }
        let decoded = self.platform.decode(&au.data, &mut self.spare);
        self.failed = decoded.is_err();
        decoded.map(|done| done.then(|| std::mem::take(&mut self.spare)))
    }

    pub fn recycle(&mut self, picture: Picture) {
        self.spare = picture;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_sources_scale_down_to_fit_and_small_ones_stay() {
        assert_eq!(fit((2560, 1440), (1920, 1080)), (1920, 1080));
        assert_eq!(fit((3440, 1440), (1920, 1080)), (1920, 804));
        assert_eq!(fit((1080, 1920), (1920, 1080)), (608, 1080));
        assert_eq!(fit((1001, 701), (1920, 1080)), (1000, 700));
        assert_eq!(fit((1920, 1080), (1280, 720)), (1280, 720));
        assert_eq!(fit((0, 0), (1280, 720)), (2, 2));
    }
}
