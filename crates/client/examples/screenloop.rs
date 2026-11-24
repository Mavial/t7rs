//! Screen sharing end to end on one machine: capture, encode, decode, show.
//!
//!     cargo run --release --example screenloop -- [SECONDS] [--monitor N | --window TITLE] [--quality 720p30|1080p30|1080p60] [--at X,Y]
//!     cargo run --release --example screenloop -- --decode-bench [SECONDS]
//!
//! The loop shares monitor N (0 is the primary), or the first window whose
//! title contains TITLE, and shows what a viewer
//! would see in a window (`--at` places it, e.g. on another monitor), with a
//! forced keyframe every 5 s. Each second it prints the capture rate, the
//! bitrate, encode latency (frame on screen to access unit out), decode
//! latency, keyframe sizes and the process's CPU.
//!
//! `--decode-bench` records SECONDS of the primary monitor at 1080p60, stops
//! sharing, then decodes the recording at 60 fps and reports what that costs.

#[path = "../src/screen/mod.rs"]
mod screen;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use screen::{AccessUnit, Decoder, Picture, Quality, Share, Source};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

struct Args {
    seconds: u64,
    monitor: usize,
    window: Option<String>,
    quality: Quality,
    at: Option<(i32, i32)>,
    decode_bench: bool,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = parse_args();
    let sources = screen::sources();
    for info in &sources {
        println!("source: {}", info.name);
    }
    let mut monitors = sources.iter().filter(|s| matches!(s.source, Source::Monitor(_)));
    let chosen = match &args.window {
        Some(title) if !args.decode_bench => {
            sources.iter().find(|s| matches!(s.source, Source::Window(_)) && s.name.contains(title.as_str()))
        }
        _ => monitors.nth(if args.decode_bench { 0 } else { args.monitor }),
    };
    let Some(chosen) = chosen else {
        eprintln!("no such source (screen sharing may not be supported here)");
        std::process::exit(1);
    };
    println!("sharing: {}", chosen.name);
    if args.decode_bench {
        decode_bench(chosen.source, args.seconds);
    } else {
        share_loop(chosen.source, &args);
    }
}

fn parse_args() -> Args {
    let mut args =
        Args { seconds: 20, monitor: 0, window: None, quality: Quality::P1080_60, at: None, decode_bench: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--monitor" => args.monitor = it.next().and_then(|n| n.parse().ok()).unwrap_or(0),
            "--window" => args.window = it.next(),
            "--quality" => {
                args.quality = match it.next().as_deref() {
                    Some("720p30") => Quality::P720_30,
                    Some("1080p30") => Quality::P1080_30,
                    _ => Quality::P1080_60,
                }
            }
            "--at" => {
                args.at = it.next().and_then(|at| {
                    let (x, y) = at.split_once(',')?;
                    Some((x.parse().ok()?, y.parse().ok()?))
                });
            }
            "--decode-bench" => args.decode_bench = true,
            n => args.seconds = n.parse().unwrap_or(args.seconds),
        }
    }
    args
}

/// What the sink and the decoder saw in the current second.
#[derive(Default)]
struct Second {
    keyframes: Vec<usize>,
    /// The largest access unit that is not a keyframe.
    largest: usize,
    decode: Vec<Duration>,
    /// Access unit out of the encoder to decode start: the channel's queueing.
    queued: Duration,
    shown: u32,
}

enum Event {
    Redraw,
    Exit,
}

fn share_loop(monitor: Source, args: &Args) {
    let event_loop = EventLoop::<Event>::with_user_event().build().expect("event loop");
    let proxy = event_loop.create_proxy();
    let second = Arc::new(Mutex::new(Second::default()));
    let newest: Arc<Mutex<Option<Picture>>> = Arc::default();
    let (units, unit_rx) = mpsc::channel::<(AccessUnit, Instant)>();
    let (recycle, recycled) = mpsc::channel::<Picture>();

    let share = {
        let second = second.clone();
        Share::start(monitor, args.quality, move |au| {
            let mut s = second.lock().unwrap();
            if au.key {
                s.keyframes.push(au.data.len());
            } else {
                s.largest = s.largest.max(au.data.len());
            }
            drop(s);
            let _ = units.send((au, Instant::now()));
        })
    };
    let share = Arc::new(share.unwrap_or_else(|e| {
        eprintln!("cannot share: {e}");
        std::process::exit(1);
    }));
    println!("encoder: {}", share.encoder());

    let decoding = std::thread::spawn({
        let (share, second, newest, proxy) = (Arc::downgrade(&share), second.clone(), newest.clone(), proxy.clone());
        move || {
            let mut decoder = Decoder::new().expect("decoder");
            while let Ok((au, sent)) = unit_rx.recv() {
                while let Ok(picture) = recycled.try_recv() {
                    decoder.recycle(picture);
                }
                let start = Instant::now();
                let picture = decoder.decode(&au);
                let took = start.elapsed();
                {
                    let mut s = second.lock().unwrap();
                    s.decode.push(took);
                    s.queued = s.queued.max(start - sent);
                }
                if decoder.wants_keyframe()
                    && let Some(share) = share.upgrade()
                {
                    share.request_keyframe();
                }
                if let Some(picture) = picture {
                    if let Some(old) = newest.lock().unwrap().replace(picture) {
                        decoder.recycle(old);
                    }
                    let _ = proxy.send_event(Event::Redraw);
                }
            }
        }
    });

    let reporting = std::thread::spawn({
        let (share, second, seconds) = (share.clone(), second.clone(), args.seconds);
        move || report(&share, &second, seconds, &proxy)
    });

    let mut viewer = Viewer { window: None, surface: None, newest, recycle, at: args.at, second };
    event_loop.run_app(&mut viewer).expect("event loop");
    reporting.join().unwrap();
    let ended = share.ended();
    drop(viewer);
    // The decode thread ends once the share, and with it the sink, is gone.
    let stop = Instant::now();
    let mut share = share;
    while let Err(shared) = Arc::try_unwrap(share) {
        share = shared;
        std::thread::yield_now();
    }
    decoding.join().unwrap();
    println!("ended early: {ended}; stopped in {:?}", stop.elapsed());
}

fn report(share: &Share, second: &Mutex<Second>, seconds: u64, proxy: &EventLoopProxy<Event>) {
    let start = Instant::now();
    let mut cpu = process_cpu();
    for t in 1..=seconds {
        std::thread::sleep((start + Duration::from_secs(t)).saturating_duration_since(Instant::now()));
        let now_cpu = process_cpu();
        let stats = share.take_stats();
        let s = std::mem::take(&mut *second.lock().unwrap());
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let decode_avg = s.decode.iter().sum::<Duration>() / (s.decode.len().max(1) as u32);
        let decode_max = s.decode.iter().max().copied().unwrap_or_default();
        println!(
            "t={t:>2}s capture {:>3} fps | encoded {:>3} fps {:>5} kbit/s, largest {:>3} KB, latency avg {:.1} ms max {:.1} ms | \
             decode avg {:.1} ms max {:.1} ms, queued max {:.1} ms, shown {} | keyframes {:?} | CPU {:.1}% of one core",
            stats.captured,
            stats.encoded,
            stats.bytes * 8 / 1000,
            s.largest / 1000,
            ms(stats.latency_total) / f64::from(stats.encoded.max(1)),
            ms(stats.latency_max),
            ms(decode_avg),
            ms(decode_max),
            ms(s.queued),
            s.shown,
            s.keyframes.iter().map(|k| format!("{} KB", k / 1000)).collect::<Vec<_>>(),
            now_cpu.saturating_sub(cpu).as_secs_f64() * 100.0,
        );
        cpu = now_cpu;
        if t % 5 == 0 {
            share.request_keyframe();
        }
        if share.ended() {
            println!("share ended");
            break;
        }
    }
    let _ = proxy.send_event(Event::Exit);
}

struct Viewer {
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    newest: Arc<Mutex<Option<Picture>>>,
    recycle: mpsc::Sender<Picture>,
    at: Option<(i32, i32)>,
    second: Arc<Mutex<Second>>,
}

impl ApplicationHandler<Event> for Viewer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs =
            Window::default_attributes().with_title("screenloop").with_inner_size(PhysicalSize::new(960, 540));
        if let Some((x, y)) = self.at {
            attrs = attrs.with_position(PhysicalPosition::new(x, y));
        }
        let window = Rc::new(event_loop.create_window(attrs).expect("window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("softbuffer"));
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Redraw => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Event::Exit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}

impl Viewer {
    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
        let Some(picture) = self.newest.lock().unwrap().take() else { return };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        surface.resize(w, h).expect("resize");
        let mut buffer = surface.buffer_mut().expect("buffer");
        blit(&picture, &mut buffer, size.width as usize, size.height as usize);
        buffer.present().expect("present");
        self.second.lock().unwrap().shown += 1;
        let _ = self.recycle.send(picture);
    }
}

/// Nearest-neighbour scale of `picture` to fill `out`.
fn blit(picture: &Picture, out: &mut [u32], width: usize, height: usize) {
    let (pw, ph) = (picture.width as usize, picture.height as usize);
    if pw == 0 || ph == 0 {
        return;
    }
    let columns: Vec<usize> = (0..width).map(|x| x * pw / width).collect();
    for (y, row) in out.chunks_exact_mut(width).enumerate().take(height) {
        let src = &picture.pixels[(y * ph / height) * pw..][..pw];
        for (o, &x) in row.iter_mut().zip(&columns) {
            *o = src[x];
        }
    }
}

fn decode_bench(monitor: Source, seconds: u64) {
    let recording: Arc<Mutex<Vec<AccessUnit>>> = Arc::default();
    let share = {
        let recording = recording.clone();
        Share::start(monitor, Quality::P1080_60, move |au| recording.lock().unwrap().push(au)).expect("share")
    };
    println!("recording {seconds} s with {}", share.encoder());
    let cpu = process_cpu();
    std::thread::sleep(Duration::from_secs(seconds));
    let share_cpu = process_cpu().saturating_sub(cpu);
    drop(share);
    let units = std::mem::take(&mut *recording.lock().unwrap());
    let bytes: usize = units.iter().map(|u| u.data.len()).sum();
    let keys = units.iter().filter(|u| u.key).count();
    let mut sizes: Vec<usize> = units.iter().filter(|u| !u.key).map(|u| u.data.len()).collect();
    sizes.sort_unstable();
    let size = |p: usize| sizes.get(sizes.len().saturating_sub(1) * p / 100).copied().unwrap_or(0) / 1000;
    println!(
        "recorded {} access units ({keys} keyframes), {:.0} kbit/s, delta frames p50 {} KB p99 {} KB max {} KB;          sharing alone: CPU {:.1}% of one core",
        units.len(),
        bytes as f64 * 8.0 / 1000.0 / seconds as f64,
        size(50),
        size(99),
        size(100),
        share_cpu.as_secs_f64() / seconds as f64 * 100.0,
    );

    for recycle in [true, false] {
        let mut decoder = Decoder::new().expect("decoder");
        let mut latencies = Vec::with_capacity(units.len());
        let mut size = (0, 0);
        let interval = Duration::from_secs(1) / 60;
        let cpu = process_cpu();
        let start = Instant::now();
        for (i, au) in units.iter().enumerate() {
            std::thread::sleep((start + interval * i as u32).saturating_duration_since(Instant::now()));
            let t = Instant::now();
            let picture = decoder.decode(au);
            latencies.push(t.elapsed());
            if let Some(picture) = picture {
                size = (picture.width, picture.height);
                if recycle {
                    decoder.recycle(picture);
                }
            }
        }
        let wall = start.elapsed();
        let used = process_cpu().saturating_sub(cpu);
        latencies.sort();
        let pct = |p: usize| latencies[(latencies.len() - 1) * p / 100].as_secs_f64() * 1000.0;
        println!(
            "paced 60 fps, {}x{}, recycling pictures: {recycle}: CPU {:.1}% of one core, decode+convert p50 {:.2} ms p99 {:.2} ms max {:.2} ms",
            size.0,
            size.1,
            used.as_secs_f64() / wall.as_secs_f64() * 100.0,
            pct(50),
            pct(99),
            pct(100)
        );
    }

    let mut decoder = Decoder::new().expect("decoder");
    let cpu = process_cpu();
    let start = Instant::now();
    let mut shown = 0;
    for au in &units {
        if let Some(picture) = decoder.decode(au) {
            shown += 1;
            decoder.recycle(picture);
        }
    }
    let wall = start.elapsed();
    println!(
        "unpaced: {shown} pictures in {wall:?}, {:.2} ms each, CPU {:?} ({:.0}% of one core while running)",
        wall.as_secs_f64() * 1000.0 / f64::from(shown.max(1)),
        process_cpu().saturating_sub(cpu),
        process_cpu().saturating_sub(cpu).as_secs_f64() / wall.as_secs_f64() * 100.0
    );
}

/// CPU time this process has used so far. Counted in cycles, as tick-based
/// process times are only sampled every 15.6 ms and swing wildly per second.
#[cfg(all(windows, target_arch = "x86_64"))]
fn process_cpu() -> Duration {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn QueryProcessCycleTime(process: isize, cycles: *mut u64) -> i32;
    }
    // The cycle counter runs at a fixed rate; measure it once against the clock.
    static HZ: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    let hz = *HZ.get_or_init(|| {
        let (start, cycles) = (Instant::now(), unsafe { std::arch::x86_64::_rdtsc() });
        std::thread::sleep(Duration::from_millis(200));
        (unsafe { std::arch::x86_64::_rdtsc() } - cycles) as f64 / start.elapsed().as_secs_f64()
    });
    let mut cycles = 0;
    // SAFETY: an out-pointer to a local.
    unsafe { QueryProcessCycleTime(GetCurrentProcess(), &raw mut cycles) };
    Duration::from_secs_f64(cycles as f64 / hz)
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn process_cpu() -> Duration {
    Duration::ZERO
}
