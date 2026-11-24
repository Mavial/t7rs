//! The share thread: paces capture to the frame rate, scales on the GPU,
//! feeds the encoder and hands access units to the sink.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromWindow};

use super::super::{AccessUnit, Handle, Params, ShareStats, Source, annexb, fit};
use super::capture::{self, Capture};
use super::convert::{Converter, Gpu, SLOTS};
use super::encode::Encoder;
use super::{Runtime, now_100ns};

/// What the share thread waits for.
pub enum Wake {
    /// The capture delivered a frame.
    Frame,
    NeedInput,
    HaveOutput,
    Keyframe,
    Failed(String),
    Stop,
}

pub struct Share {
    wake: Sender<Wake>,
    shared: Arc<Shared>,
    encoder: String,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    ended: AtomicBool,
    captured: AtomicU32,
    encoded: AtomicU32,
    bytes: AtomicU64,
    latency_us: AtomicU64,
    latency_max_us: AtomicU64,
}

type Sink = Box<dyn FnMut(AccessUnit) + Send>;
type Ended = Box<dyn FnOnce() + Send>;

impl Share {
    pub fn start(source: Source, params: Params, sink: Sink, ended: Ended) -> Result<Share, String> {
        let (wake, wakes) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared::default());
        let thread = std::thread::Builder::new()
            .name("screen share".into())
            .spawn({
                let (wake, shared) = (wake.clone(), shared.clone());
                move || {
                    run(source, &params, sink, &wakes, &wake, &shared, &ready);
                    ended();
                }
            })
            .map_err(|e| format!("screen share thread: {e}"))?;
        match started.recv() {
            Ok(Ok(encoder)) => Ok(Share { wake, shared, encoder, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the screen share thread stopped".into())
            }
        }
    }

    pub fn request_keyframe(&self) {
        let _ = self.wake.send(Wake::Keyframe);
    }

    pub fn ended(&self) -> bool {
        self.shared.ended.load(Relaxed)
    }

    pub fn encoder(&self) -> &str {
        &self.encoder
    }

    pub fn take_stats(&self) -> ShareStats {
        let s = &self.shared;
        ShareStats {
            captured: s.captured.swap(0, Relaxed),
            encoded: s.encoded.swap(0, Relaxed),
            bytes: s.bytes.swap(0, Relaxed),
            latency_total: Duration::from_micros(s.latency_us.swap(0, Relaxed)),
            latency_max: Duration::from_micros(s.latency_max_us.swap(0, Relaxed)),
        }
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        let _ = self.wake.send(Wake::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    source: Source,
    params: &Params,
    mut sink: Sink,
    wakes: &Receiver<Wake>,
    wake: &Sender<Wake>,
    shared: &Shared,
    ready: &SyncSender<Result<String, String>>,
) {
    let mut pipeline = match Pipeline::new(source, params, wake) {
        Ok(pipeline) => pipeline,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(pipeline.encoder.name().to_owned()));
    if let Err(e) = pipeline.run(wakes, shared, &mut *sink) {
        tracing::warn!("screen share stopped: {e}");
    }
    shared.ended.store(true, Relaxed);
}

/// Fields drop in order: capture stops first, the runtime goes last.
struct Pipeline {
    capture: Capture,
    encoder: Encoder,
    converter: Converter,
    gpu: Gpu,
    size: (u32, u32),
    fps: u32,
    _runtime: Runtime,
}

impl Pipeline {
    fn new(source: Source, params: &Params, wake: &Sender<Wake>) -> Result<Pipeline, String> {
        let runtime = Runtime::start()?;
        let monitor = match source {
            Source::Monitor(Handle(h)) => HMONITOR(h as _),
            // SAFETY: a plain query; a stale handle gives the nearest monitor.
            Source::Window(Handle(h)) => unsafe { MonitorFromWindow(HWND(h as _), MONITOR_DEFAULTTONEAREST) },
        };
        let gpu = Gpu::new(monitor)?;
        let capture = Capture::start(&gpu.device, source, params.fps, wake)?;
        // The encoded size is fixed for the share; a window that changes size
        // later is scaled into it.
        let size = fit(capture.size(), params.max);
        let encoder = Encoder::new(&gpu, size, params, wake)?;
        let converter = Converter::new(&gpu, capture.size(), size, params.fps)?;
        Ok(Pipeline { capture, encoder, converter, gpu, size, fps: params.fps, _runtime: runtime })
    }

    fn run(&mut self, wakes: &Receiver<Wake>, shared: &Shared, sink: &mut dyn FnMut(AccessUnit)) -> Result<(), String> {
        let interval = Duration::from_secs(1) / self.fps;
        // Frames this early still count as on time, so capture jitter does not halve the rate.
        let early = interval / 4;
        let duration = 10_000_000 / i64::from(self.fps);
        let mut due = Instant::now();
        // Input the asynchronous encoder asked for and has not had yet.
        let mut credit = 0u32;
        // The newest frame not encoded yet, and when it reached this thread.
        let mut pending: Option<(Direct3D11CaptureFrame, i64)> = None;
        let mut keyframe = true;
        let mut out = Out { inflight: VecDeque::new(), frame: 0 };
        let mut slot = 0;
        let mut last: Option<usize> = None;
        loop {
            let waiting = pending.is_some() || (keyframe && last.is_some());
            let timeout = if waiting {
                due.checked_sub(early).unwrap().saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(1)
            };
            let first = match wakes.recv_timeout(timeout) {
                Ok(wake) => Some(wake),
                Err(RecvTimeoutError::Timeout) if !self.capture.alive() => return Ok(()),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            let mut arrived = false;
            for wake in first.into_iter().chain(std::iter::from_fn(|| wakes.try_recv().ok())) {
                match wake {
                    Wake::Frame => {
                        shared.captured.fetch_add(1, Relaxed);
                        arrived = true;
                    }
                    Wake::NeedInput => credit += 1,
                    Wake::HaveOutput => {
                        if let Some(data) = self.encoder.output()? {
                            out.emit(data, shared, sink);
                        }
                    }
                    Wake::Keyframe => keyframe = true,
                    Wake::Failed(e) => return Err(e),
                    Wake::Stop => return Ok(()),
                }
            }
            // Take frames as they come, so the pool always has a free buffer
            // and the newest picture is never the one dropped.
            if arrived && let Some(frame) = self.capture.newest()? {
                pending = Some((frame, now_100ns()));
            }

            let now = Instant::now();
            let encoder_ready = (credit > 0 || !self.encoder.is_async()) && out.inflight.len() < SLOTS - 1;
            if !encoder_ready || now + early < due {
                continue;
            }
            let (input, time) = if let Some((frame, time)) = pending.take() {
                let (texture, content) = capture::texture(&frame)?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: writes into a local.
                unsafe { texture.GetDesc(&raw mut desc) };
                if (desc.Width, desc.Height) != self.converter.input_size() {
                    self.converter = Converter::new(&self.gpu, (desc.Width, desc.Height), self.size, self.fps)?;
                }
                self.converter.convert(&self.gpu, &texture, content, slot)?;
                let used = slot;
                last = Some(used);
                slot = (slot + 1) % SLOTS;
                (used, time)
            } else if keyframe && let Some(last) = last {
                // Nothing moved since: encode the last picture again.
                (last, now_100ns())
            } else {
                continue;
            };
            self.encoder.input(self.converter.output(input), time, duration, keyframe)?;
            keyframe = false;
            credit = credit.saturating_sub(1);
            out.inflight.push_back(time);
            due = if due + interval < now { now + interval } else { due + interval };
            if !self.encoder.is_async() {
                while let Some(data) = self.encoder.output()? {
                    out.emit(data, shared, sink);
                }
            }
        }
    }
}

struct Out {
    /// Arrival times of the frames inside the encoder, oldest first.
    inflight: VecDeque<i64>,
    frame: u32,
}

impl Out {
    fn emit(&mut self, data: Vec<u8>, shared: &Shared, sink: &mut dyn FnMut(AccessUnit)) {
        if let Some(captured) = self.inflight.pop_front() {
            let latency = ((now_100ns() - captured) / 10).max(0) as u64;
            shared.latency_us.fetch_add(latency, Relaxed);
            shared.latency_max_us.fetch_max(latency, Relaxed);
        }
        shared.encoded.fetch_add(1, Relaxed);
        shared.bytes.fetch_add(data.len() as u64, Relaxed);
        let key = annexb::is_key(&data);
        sink(AccessUnit { frame: self.frame, key, data });
        self.frame = self.frame.wrapping_add(1);
    }
}
