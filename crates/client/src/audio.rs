//! Voice audio: microphone capture → gate (voice activation / push-to-talk)
//! → Opus encode, and per-speaker decode → jitter buffer → mix → speakers.
//!
//! Threads:
//! - cpal's input callback only converts to mono and hands chunks to a
//!   worker thread, which resamples to 48 kHz, gates and encodes.
//! - The network thread decodes received frames into [`Playback`].
//! - cpal's output callback mixes from [`Playback`] and resamples to the device.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use serde::{Deserialize, Serialize};
use t7_proto::ClientId;
use t7_proto::voice::{FRAME_SAMPLES, MAX_OPUS_PAYLOAD, SAMPLE_RATE};

use crate::resample::Resampler;

/// Voice activation stays open this long after the level drops, so word
/// endings and short pauses aren't clipped.
const VA_HOLD: Duration = Duration::from_millis(400);
/// Start playing a speaker once this much audio is buffered (absorbs jitter).
const JITTER_PRIME: usize = FRAME_SAMPLES * 3;
/// Beyond this backlog, drop old audio to keep latency low.
const MAX_BACKLOG: usize = FRAME_SAMPLES * 12;
/// Gaps up to this many frames are concealed; longer ones start a new talk spurt.
const MAX_CONCEAL: u32 = 4;
const SPEAKING_FOR: Duration = Duration::from_millis(250);
const BITRATE: std::num::NonZeroU32 = std::num::NonZeroU32::new(32_000).unwrap();

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TxMode {
    #[default]
    VoiceActivation,
    PushToTalk,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AudioSettings {
    /// Device names; `None` follows the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub mode: TxMode,
    /// Voice activation opens above this input level, in dBFS.
    pub threshold_db: f32,
    pub ptt_key: Option<PttKey>,
    pub input_gain_db: f32,
    /// Linear, 1.0 = unchanged.
    pub output_volume: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            mode: TxMode::VoiceActivation,
            threshold_db: -45.0,
            ptt_key: None,
            input_gain_db: 0.0,
            output_volume: 1.0,
        }
    }
}

/// A push-to-talk key, stored as a Windows virtual-key code.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct PttKey {
    pub vk: u16,
}

/// State shared between the UI, the capture worker and the audio callbacks.
pub struct Shared {
    settings: Mutex<AudioSettings>,
    output_volume: AtomicU32,
    level_db: AtomicU32,
    transmitting: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    /// Push-to-talk key held while our window has focus (non-Windows fallback).
    ptt_local: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(settings: AudioSettings) -> Arc<Self> {
        let s = Arc::new(Self {
            settings: Mutex::new(AudioSettings::default()),
            output_volume: AtomicU32::new(1.0f32.to_bits()),
            level_db: AtomicU32::new((-100.0f32).to_bits()),
            transmitting: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            ptt_local: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        s.set_settings(&settings);
        s
    }

    pub fn set_settings(&self, settings: &AudioSettings) {
        self.output_volume.store(settings.output_volume.to_bits(), Ordering::Relaxed);
        *self.settings.lock().unwrap() = settings.clone();
    }

    pub fn set_status(&self, muted: bool, deafened: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        self.deafened.store(deafened, Ordering::Relaxed);
    }

    pub fn set_ptt_local(&self, down: bool) {
        self.ptt_local.store(down, Ordering::Relaxed);
    }

    /// Latest microphone level in dBFS (after gain).
    pub fn level_db(&self) -> f32 {
        f32::from_bits(self.level_db.load(Ordering::Relaxed))
    }

    pub fn transmitting(&self) -> bool {
        self.transmitting.load(Ordering::Relaxed)
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }

    fn report(&self, ctx: &egui::Context, msg: String) {
        tracing::warn!("{msg}");
        *self.error.lock().unwrap() = Some(msg);
        ctx.request_repaint();
    }
}

struct Speaker {
    decoder: opus1::OpusDecoder,
    queue: VecDeque<f32>,
    primed: bool,
    next_seq: Option<u32>,
    last_heard: Instant,
}

/// Received voice, per speaker, waiting to be mixed into the output.
pub struct Playback {
    speakers: Mutex<HashMap<ClientId, Speaker>>,
    ctx: egui::Context,
}

impl Playback {
    pub fn new(ctx: egui::Context) -> Arc<Self> {
        Arc::new(Self { speakers: Mutex::new(HashMap::new()), ctx })
    }

    /// Decodes one received frame (called on the network thread).
    pub fn push(&self, from: ClientId, seq: u32, payload: &[u8]) {
        let mut speakers = self.speakers.lock().unwrap();
        let now = Instant::now();
        let speaker = match speakers.entry(from) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Ok(decoder) = opus1::OpusDecoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono)
                else {
                    return;
                };
                e.insert(Speaker {
                    decoder,
                    queue: VecDeque::with_capacity(MAX_BACKLOG + FRAME_SAMPLES),
                    primed: false,
                    next_seq: None,
                    last_heard: now - SPEAKING_FOR * 4,
                })
            }
        };

        let mut lost = 0;
        if let Some(next) = speaker.next_seq {
            let gap = seq.wrapping_sub(next);
            if gap as i32 <= -1 {
                return; // late or duplicate
            }
            if gap <= MAX_CONCEAL {
                lost = gap;
            }
        }
        let mut pcm = [0f32; FRAME_SAMPLES];
        for _ in 0..lost {
            if let Ok(n) = speaker.decoder.decode(&[], FRAME_SAMPLES, &mut pcm) {
                speaker.queue.extend(&pcm[..n]);
            }
        }
        match speaker.decoder.decode(payload, FRAME_SAMPLES, &mut pcm) {
            Ok(n) => speaker.queue.extend(&pcm[..n]),
            Err(_) => return, // garbage; ignore
        }
        speaker.next_seq = Some(seq.wrapping_add(1));
        if speaker.queue.len() > MAX_BACKLOG {
            let excess = speaker.queue.len() - JITTER_PRIME;
            speaker.queue.drain(..excess);
        }

        let was_silent = now.duration_since(speaker.last_heard) > SPEAKING_FOR;
        speaker.last_heard = now;
        if was_silent {
            self.ctx.request_repaint(); // talking indicator on
        }
    }

    /// Adds every speaker's next `out.len()` samples (48 kHz) into `out`.
    pub fn mix(&self, out: &mut [f32]) {
        let mut speakers = self.speakers.lock().unwrap();
        for s in speakers.values_mut() {
            if !s.primed {
                if s.queue.len() < JITTER_PRIME {
                    continue;
                }
                s.primed = true;
            }
            let n = out.len().min(s.queue.len());
            for (o, v) in out.iter_mut().zip(s.queue.drain(..n)) {
                *o += v;
            }
            if s.queue.is_empty() {
                s.primed = false;
            }
        }
    }

    /// Clients heard in the last moment.
    pub fn speaking(&self) -> Vec<ClientId> {
        let now = Instant::now();
        let speakers = self.speakers.lock().unwrap();
        speakers
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_heard) < SPEAKING_FOR)
            .map(|(&id, _)| id)
            .collect()
    }

    pub fn clear(&self) {
        self.speakers.lock().unwrap().clear();
    }
}

/// Receives each encoded frame and its sequence number.
pub type VoiceSink = Arc<dyn Fn(u32, &[u8]) + Send + Sync>;

/// Running audio devices. Dropping it stops capture and playback.
pub struct Engine {
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
}

impl Engine {
    pub fn start(shared: &Arc<Shared>, playback: &Arc<Playback>, sink: VoiceSink, ctx: &egui::Context) -> Self {
        let settings = shared.settings.lock().unwrap().clone();
        let host = cpal::default_host();
        let input = open_input(&host, settings.input_device.as_deref(), shared, sink, ctx)
            .map_err(|e| shared.report(ctx, format!("Microphone unavailable: {e}")))
            .ok();
        let output = open_output(&host, settings.output_device.as_deref(), shared, playback, ctx)
            .map_err(|e| shared.report(ctx, format!("Speakers unavailable: {e}")))
            .ok();
        Self { _input: input, _output: output }
    }
}

/// Names of the available (input, output) devices.
pub fn device_names() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let inputs = host.input_devices().map(|it| it.filter_map(|d| device_name(&d)).collect());
    let outputs = host.output_devices().map(|it| it.filter_map(|d| device_name(&d)).collect());
    (inputs.unwrap_or_default(), outputs.unwrap_or_default())
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_owned())
}

fn find_device(
    devices: Result<impl Iterator<Item = cpal::Device>, cpal::Error>,
    default: Option<cpal::Device>,
    wanted: Option<&str>,
) -> Result<cpal::Device, String> {
    if let Some(name) = wanted
        && let Some(d) = devices.ok().and_then(|mut it| it.find(|d| device_name(d).as_deref() == Some(name)))
    {
        return Ok(d);
    }
    default.ok_or_else(|| "no device found".to_owned())
}

fn open_input(
    host: &cpal::Host,
    wanted: Option<&str>,
    shared: &Arc<Shared>,
    sink: VoiceSink,
    ctx: &egui::Context,
) -> Result<cpal::Stream, String> {
    let device = find_device(host.input_devices(), host.default_input_device(), wanted)?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let channels = config.channels as usize;
    let (tx, rx) = mpsc::channel::<Vec<f32>>();

    let err_shared = shared.clone();
    let err_ctx = ctx.clone();
    let on_error = move |e: cpal::Error| report_stream_error(&err_shared, &err_ctx, "Microphone", &e);
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&device, &config, channels, tx, on_error),
        SampleFormat::I16 => build_input::<i16>(&device, &config, channels, tx, on_error),
        SampleFormat::U16 => build_input::<u16>(&device, &config, channels, tx, on_error),
        SampleFormat::I32 => build_input::<i32>(&device, &config, channels, tx, on_error),
        f => return Err(format!("unsupported sample format {f}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;

    let shared = shared.clone();
    let ctx = ctx.clone();
    let rate = config.sample_rate;
    std::thread::Builder::new()
        .name("t7-capture".into())
        .spawn(move || capture_worker(rx, rate, &shared, &sink, &ctx))
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    tx: mpsc::Sender<Vec<f32>>,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mono: Vec<f32> = data
                    .chunks_exact(channels)
                    .map(|frame| frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32)
                    .collect();
                let _ = tx.send(mono);
            },
            on_error,
            None,
        )
        .map_err(|e| e.to_string())
}

/// Resamples captured audio to 48 kHz, gates it and encodes 20 ms frames.
/// Ends when the input stream (and with it the sender) is dropped.
fn capture_worker(rx: mpsc::Receiver<Vec<f32>>, rate: u32, shared: &Shared, sink: &VoiceSink, ctx: &egui::Context) {
    let mut encoder = match opus1::OpusEncoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono, opus1::Application::Voip) {
        Ok(e) => e,
        Err(e) => return shared.report(ctx, format!("Voice encoder failed: {e:?}")),
    };
    encoder.set_bitrate(opus1::Bitrate::BitsPerSecond(BITRATE));
    let mut resampler = Resampler::new(rate, SAMPLE_RATE);
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut packet = [0u8; MAX_OPUS_PAYLOAD];
    let mut seq = 0u32;
    let mut hold_until = Instant::now();

    while let Ok(chunk) = rx.recv() {
        resampler.push(&chunk, &mut pending);
        while pending.len() >= FRAME_SAMPLES {
            let mut frame = [0f32; FRAME_SAMPLES];
            frame.copy_from_slice(&pending[..FRAME_SAMPLES]);
            pending.drain(..FRAME_SAMPLES);

            let (mode, threshold, gain_db, ptt) = {
                let s = shared.settings.lock().unwrap();
                (s.mode, s.threshold_db, s.input_gain_db, s.ptt_key)
            };
            let gain = 10f32.powf(gain_db / 20.0);
            let mut energy = 0.0;
            for v in &mut frame {
                *v *= gain;
                energy += *v * *v;
            }
            let level = 10.0 * (energy / FRAME_SAMPLES as f32).max(1e-10).log10();
            shared.level_db.store(level.to_bits(), Ordering::Relaxed);

            let now = Instant::now();
            let wants = match mode {
                TxMode::VoiceActivation => {
                    if level > threshold {
                        hold_until = now + VA_HOLD;
                    }
                    now < hold_until
                }
                TxMode::PushToTalk => {
                    ptt.is_some_and(|k| key_down_global(k.vk)) || shared.ptt_local.load(Ordering::Relaxed)
                }
            };
            let open = wants && !shared.muted.load(Ordering::Relaxed) && !shared.deafened.load(Ordering::Relaxed);
            if shared.transmitting.swap(open, Ordering::Relaxed) != open {
                ctx.request_repaint();
            }
            if open && let Ok(n) = encoder.encode(&frame, FRAME_SAMPLES, &mut packet) {
                sink(seq, &packet[..n]);
            }
            // Counts silent frames too, so receivers can tell a new talk spurt from loss.
            seq = seq.wrapping_add(1);
        }
    }
    shared.transmitting.store(false, Ordering::Relaxed);
    ctx.request_repaint();
}

fn open_output(
    host: &cpal::Host,
    wanted: Option<&str>,
    shared: &Arc<Shared>,
    playback: &Arc<Playback>,
    ctx: &egui::Context,
) -> Result<cpal::Stream, String> {
    let device = find_device(host.output_devices(), host.default_output_device(), wanted)?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let err_shared = shared.clone();
    let err_ctx = ctx.clone();
    let on_error = move |e: cpal::Error| report_stream_error(&err_shared, &err_ctx, "Speaker", &e);
    let args = (&device, &config, shared.clone(), playback.clone());
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_output::<f32>(args, on_error),
        SampleFormat::I16 => build_output::<i16>(args, on_error),
        SampleFormat::U16 => build_output::<u16>(args, on_error),
        SampleFormat::I32 => build_output::<i32>(args, on_error),
        f => return Err(format!("unsupported sample format {f}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn build_output<T>(
    (device, config, shared, playback): (&cpal::Device, &cpal::StreamConfig, Arc<Shared>, Arc<Playback>),
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(SAMPLE_RATE, config.sample_rate);
    let mut mixed: Vec<f32> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    let rate = config.sample_rate as usize;
    let (mut meter_energy, mut meter_frames) = (0f64, 0usize);
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels;
                mixed.clear();
                mixed.resize(resampler.input_needed(frames), 0.0);
                playback.mix(&mut mixed);
                mono.clear();
                resampler.pull(&mixed, frames, &mut mono);
                let volume = if shared.deafened.load(Ordering::Relaxed) {
                    0.0
                } else {
                    f32::from_bits(shared.output_volume.load(Ordering::Relaxed))
                };
                for (frame, &v) in data.chunks_exact_mut(channels).zip(&mono) {
                    let out = (v * volume).clamp(-1.0, 1.0);
                    meter_energy += (out * out) as f64;
                    frame.fill(T::from_sample(out));
                }
                // Once a second, at debug level: the level actually handed to the device.
                meter_frames += frames;
                if meter_frames >= rate {
                    let db = 10.0 * (meter_energy / meter_frames as f64).max(1e-12).log10();
                    tracing::debug!(level_db = format_args!("{db:.1}"), "playback");
                    (meter_energy, meter_frames) = (0.0, 0);
                }
            },
            on_error,
            None,
        )
        .map_err(|e| e.to_string())
}

/// A glitch (buffer under/overrun) recovers by itself, typically once as a
/// stream starts; only real failures, like an unplugged device, reach the user.
fn report_stream_error(shared: &Shared, ctx: &egui::Context, what: &str, e: &cpal::Error) {
    if e.kind() == cpal::ErrorKind::Xrun {
        tracing::debug!("{what} glitch: {e}");
    } else {
        shared.report(ctx, format!("{what} error: {e}"));
    }
}

#[cfg(windows)]
fn key_down_global(vk: u16) -> bool {
    // Works while another window (e.g. a game) has focus.
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

#[cfg(not(windows))]
fn key_down_global(_vk: u16) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_tone(frames: usize) -> Vec<Vec<u8>> {
        let mut enc =
            opus1::OpusEncoder::new(opus1::SampleRate::Hz48000, opus1::Channels::Mono, opus1::Application::Voip).unwrap();
        let mut buf = [0u8; MAX_OPUS_PAYLOAD];
        (0..frames)
            .map(|f| {
                let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                    .map(|i| (std::f32::consts::TAU * 300.0 * (f * FRAME_SAMPLES + i) as f32 / 48_000.0).sin() * 0.4)
                    .collect();
                let n = enc.encode(&pcm, FRAME_SAMPLES, &mut buf).unwrap();
                buf[..n].to_vec()
            })
            .collect()
    }

    fn mix_frames(p: &Playback, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * FRAME_SAMPLES];
        p.mix(&mut out);
        out
    }

    #[test]
    fn primes_then_plays_and_conceals_loss() {
        let p = Playback::new(egui::Context::default());
        let packets = encoded_tone(10);
        p.push(7, 0, &packets[0]);
        p.push(7, 1, &packets[1]);
        assert!(mix_frames(&p, 1).iter().all(|&v| v == 0.0), "must wait for the jitter buffer");
        // Frame 2 lost: 3 and 4 arrive, the gap is concealed, so 5 frames are queued.
        p.push(7, 3, &packets[3]);
        p.push(7, 4, &packets[4]);
        let out = mix_frames(&p, 5);
        let energy: f32 = out.iter().map(|v| v * v).sum::<f32>() / out.len() as f32;
        assert!(energy > 0.01, "expected audible output, got energy {energy}");
        assert_eq!(p.speaking(), vec![7]);
        // A late duplicate is dropped.
        p.push(7, 1, &packets[1]);
        assert!(mix_frames(&p, 1).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn mixes_speakers_and_ignores_garbage() {
        let p = Playback::new(egui::Context::default());
        let packets = encoded_tone(4);
        for (seq, pkt) in packets.iter().enumerate() {
            p.push(1, seq as u32, pkt);
            p.push(2, seq as u32, pkt);
        }
        p.push(3, 0, &[0xff; 3]);
        let mut speaking = p.speaking();
        speaking.sort();
        assert!(speaking.starts_with(&[1, 2]));
        let both = mix_frames(&p, 3);
        let p1 = Playback::new(egui::Context::default());
        for (seq, pkt) in packets.iter().enumerate() {
            p1.push(1, seq as u32, pkt);
        }
        let one = mix_frames(&p1, 3);
        for (b, o) in both.iter().zip(&one) {
            assert!((b - 2.0 * o).abs() < 1e-5);
        }
    }
}
