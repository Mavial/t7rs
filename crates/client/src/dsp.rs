use nnnoiseless::DenoiseState;
use serde::{Deserialize, Serialize};
use sonora::config::{AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter};
use sonora::{AudioProcessing, Config, StreamConfig};

pub const FRAME: usize = 480;

/// `RNNoise` works on 16-bit-scaled samples.
const I16_SCALE: f32 = 32768.0;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct Cleanup {
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub auto_gain: bool,
}

impl Default for Cleanup {
    fn default() -> Self {
        Self { noise_suppression: true, echo_cancellation: true, auto_gain: true }
    }
}

pub struct Processor {
    cleanup: Cleanup,
    front: AudioProcessing,
    denoise: Box<DenoiseState<'static>>,
    gain: Option<AudioProcessing>,
    scratch: [f32; FRAME],
}

impl Processor {
    pub fn new(cleanup: Cleanup) -> Self {
        let front = Config {
            high_pass_filter: Some(HighPassFilter::default()),
            echo_canceller: cleanup.echo_cancellation.then(EchoCanceller::default),
            ..Default::default()
        };
        let gain = Config {
            gain_controller2: Some(GainController2 {
                adaptive_digital: Some(AdaptiveDigital::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        Self {
            cleanup,
            front: build(front),
            denoise: DenoiseState::new(),
            gain: cleanup.auto_gain.then(|| build(gain)),
            scratch: [0.0; FRAME],
        }
    }

    pub fn cleanup(&self) -> Cleanup {
        self.cleanup
    }

    pub fn render(&mut self, frame: &[f32; FRAME]) {
        if self.cleanup.echo_cancellation {
            let result = self.front.process_render_f32(&[frame], &mut [&mut self.scratch]);
            debug_assert!(result.is_ok(), "invalid echo reference format: {result:?}");
        }
    }

    pub fn capture(&mut self, frame: &mut [f32; FRAME]) -> f32 {
        self.scratch.copy_from_slice(frame);
        let result = self.front.process_capture_f32(&[&self.scratch], &mut [frame]);
        debug_assert!(result.is_ok(), "invalid capture format: {result:?}");

        for (s, v) in self.scratch.iter_mut().zip(frame.iter()) {
            *s = v * I16_SCALE;
        }
        let mut denoised = [0f32; FRAME];
        let speech = self.denoise.process_frame(&mut denoised, &self.scratch);
        if self.cleanup.noise_suppression {
            for (v, d) in frame.iter_mut().zip(denoised) {
                *v = d / I16_SCALE;
            }
        }

        if let Some(gain) = &mut self.gain {
            self.scratch.copy_from_slice(frame);
            let result = gain.process_capture_f32(&[&self.scratch], &mut [frame]);
            debug_assert!(result.is_ok(), "invalid gain control format: {result:?}");
        }
        speech
    }
}

fn build(config: Config) -> AudioProcessing {
    AudioProcessing::builder()
        .config(config)
        .capture_config(StreamConfig::new(48_000, 1))
        .render_config(StreamConfig::new(48_000, 1))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms_db(x: &[f32]) -> f32 {
        let e: f32 = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        10.0 * e.max(1e-12).log10()
    }

    fn white(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (s >> 40) as f32 / (1u64 << 23) as f32 - 1.0
            })
            .collect()
    }

    fn fan(n: usize, seed: u64) -> Vec<f32> {
        let (mut a, mut b) = (0f32, 0f32);
        white(n, seed)
            .into_iter()
            .map(|v| {
                a += 0.05 * (v - a);
                b += 0.2 * (v - b);
                (a * 3.0 + b) * 0.03
            })
            .collect()
    }

    fn run(p: &mut Processor, mic: &[f32], speakers: Option<&[f32]>) -> (Vec<f32>, usize) {
        let mut out = Vec::with_capacity(mic.len());
        let mut speech_frames = 0;
        for (i, chunk) in mic.as_chunks::<FRAME>().0.iter().enumerate() {
            let reference = speakers.map_or([0.0; FRAME], |s| s[i * FRAME..(i + 1) * FRAME].try_into().unwrap());
            p.render(&reference);
            let mut frame = *chunk;
            if p.capture(&mut frame) > 0.5 {
                speech_frames += 1;
            }
            out.extend_from_slice(&frame);
        }
        (out, speech_frames)
    }

    #[test]
    fn removes_speaker_echo() {
        let n = 48_000 * 8;
        let far: Vec<f32> = white(n, 1).iter().map(|v| v * 0.1).collect();
        let delay = 48 * 60;
        let mic: Vec<f32> = (0..n).map(|i| if i >= delay { far[i - delay] * 0.5 } else { 0.0 }).collect();
        let only_aec = Cleanup { noise_suppression: false, echo_cancellation: true, auto_gain: false };
        let (out, _) = run(&mut Processor::new(only_aec), &mic, Some(&far));
        let tail = 48_000 * 4..n;
        let removed = rms_db(&mic[tail.clone()]) - rms_db(&out[tail]);
        assert!(removed > 20.0, "echo only reduced by {removed:.1} dB");
    }

    #[test]
    fn removes_room_noise_without_calling_it_speech() {
        let n = 48_000 * 4;
        let noise = fan(n, 2);
        let (out, speech_frames) = run(&mut Processor::new(Cleanup::default()), &noise, None);
        let tail = 48_000..n;
        let removed = rms_db(&noise[tail.clone()]) - rms_db(&out[tail]);
        assert!(removed > 25.0, "noise only reduced by {removed:.1} dB");
        assert!(speech_frames < 10, "{speech_frames} noise frames taken for speech");
    }

    #[test]
    fn cleanup_off_leaves_voice_band_untouched() {
        let n = 48_000 * 2;
        let tone: Vec<f32> =
            (0..n).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 0.05).collect();
        let off = Cleanup { noise_suppression: false, echo_cancellation: false, auto_gain: false };
        let (out, _) = run(&mut Processor::new(off), &tone, None);
        let change = rms_db(&tone[24_000..]) - rms_db(&out[24_000..]);
        assert!(change.abs() < 1.0, "1 kHz tone changed by {change:.1} dB with cleanup off");
    }
}
