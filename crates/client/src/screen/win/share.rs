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
use super::convert::{Converter, Gpu};
use super::encode::Encoder;
use super::{Running, TICKS_PER_SECOND, now_100ns};

pub enum Wake {
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
        match started.recv().unwrap_or_else(|_| Err("the screen share thread stopped".into())) {
            Ok(encoder) => Ok(Share { wake, shared, encoder, thread: Some(thread) }),
            Err(e) => {
                let _ = thread.join();
                Err(e)
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
    let mut pipeline = match Running::start(|| Pipeline::new(source, params, wake)) {
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

struct Pipeline {
    capture: Capture,
    encoder: Encoder,
    converter: Converter,
    gpu: Gpu,
    size: (u32, u32),
    fps: u32,
}

impl Pipeline {
    fn new(source: Source, params: &Params, wake: &Sender<Wake>) -> Result<Pipeline, String> {
        let monitor = match source {
            Source::Monitor(Handle(h)) => HMONITOR(h as _),
            Source::Window(Handle(h)) => unsafe { MonitorFromWindow(HWND(h as _), MONITOR_DEFAULTTONEAREST) },
        };
        let gpu = Gpu::new(monitor)?;
        let capture = Capture::start(&gpu.device, source, params.fps, wake)?;
        let size = fit(capture.size(), params.max);
        let encoder = Encoder::new(&gpu, size, params, wake)?;
        let converter = Converter::new(&gpu, capture.size(), size, params.fps)?;
        Ok(Pipeline { capture, encoder, converter, gpu, size, fps: params.fps })
    }

    fn run(&mut self, wakes: &Receiver<Wake>, shared: &Shared, sink: &mut dyn FnMut(AccessUnit)) -> Result<(), String> {
        let interval = Duration::from_secs(1) / self.fps;
        let early = interval / 4;
        let duration = TICKS_PER_SECOND / i64::from(self.fps);
        let mut due = Instant::now();
        let mut credit = 0u32;
        let mut pending: Option<(Direct3D11CaptureFrame, i64)> = None;
        let mut keyframe = true;
        let mut out = Out { inflight: VecDeque::new(), frame: 0 };
        let mut last: Option<usize> = None;
        let is_async = self.encoder.is_async();
        let ready = |credit: u32, out: &Out| (credit > 0 || !is_async) && Converter::slot_free(out.inflight.len());
        loop {
            let waiting = ready(credit, &out) && (pending.is_some() || (keyframe && last.is_some()));
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
            if arrived && let Some(frame) = self.capture.newest()? {
                pending = Some((frame, now_100ns()));
            }

            let now = Instant::now();
            if !ready(credit, &out) || now + early < due {
                continue;
            }
            let (input, time) = if let Some((frame, time)) = pending.take() {
                let (texture, content) = capture::texture(&frame)?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { texture.GetDesc(&raw mut desc) };
                if (desc.Width, desc.Height) != self.converter.input_size() {
                    self.converter = Converter::new(&self.gpu, (desc.Width, desc.Height), self.size, self.fps)?;
                }
                let used = self.converter.convert(&self.gpu, &texture, content)?;
                last = Some(used);
                (used, time)
            } else if keyframe && let Some(last) = last {
                (last, now_100ns())
            } else {
                continue;
            };
            self.encoder.input(self.converter.output(input), time, duration, keyframe)?;
            keyframe = false;
            credit = credit.saturating_sub(1);
            out.inflight.push_back(time);
            due = if due + interval < now { now + interval } else { due + interval };
            if !is_async {
                while let Some(data) = self.encoder.output()? {
                    out.emit(data, shared, sink);
                }
            }
        }
    }
}

struct Out {
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
