//! Screen sharing: capture and encode on the sharer, decode and show on viewers.
//! Frames travel as H.264 access units over the voice link (`t7_proto::voice`).
#![allow(dead_code, reason = "the app wires screen sharing up in a later change")]

mod annexb;
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
    /// Decodable on its own (IDR).
    pub key: bool,
    pub data: Vec<u8>,
}

/// What to share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Monitor(Handle),
    Window(Handle),
}

/// A monitor or window on this machine, as `sources` listed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(isize);

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub source: Source,
    pub name: String,
}

/// Monitors (the primary first), then the windows that can be captured.
/// Empty where screen sharing is not supported.
pub fn sources() -> Vec<SourceInfo> {
    platform::sources()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    P720_30,
    P1080_30,
    P1080_60,
}

struct Params {
    /// Sources larger than this are scaled down to fit; smaller ones are sent as they are.
    max: (u32, u32),
    fps: u32,
    /// Constant bitrate, bits per second.
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

    /// The encoder in use, for diagnostics: its name and whether it runs on the GPU.
    pub fn encoder(&self) -> &str {
        self.0.encoder()
    }

    /// Counters since the last call.
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

/// A decoded frame.
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
    /// Decoding failed: access units are skipped until the next keyframe.
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

    /// Hands a shown picture back so the next one reuses its memory.
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
