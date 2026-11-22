//! Notification sounds.
//!
//! Built in: short original chimes, synthesized while they play (no audio
//! files, no memory). A sound pack can replace them:
//! - a custom folder with WAV files named after the sounds ([`Sound::file_name`]);
//!   missing files fall back to the chimes;
//! - an ini pack: a folder whose `settings.ini` maps events to WAV
//!   files. Packs are read where they are installed and nothing is copied. An
//!   event the pack leaves empty stays silent.
//!
//! Pack files are decoded when they play, so a pack costs no memory at rest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use t7_proto::voice::SAMPLE_RATE;

use crate::resample::Resampler;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sound {
    Connected,
    Disconnected,
    /// Our own connection dropped.
    ConnectionLost,
    /// Someone connected to the server, landing in our channel.
    UserConnected,
    /// Someone in our channel disconnected.
    UserDisconnected,
    /// Someone in our channel timed out.
    UserLostConnection,
    /// Someone switched into our channel.
    UserJoined,
    /// Someone switched out of our channel.
    UserLeft,
    /// We switched channels.
    ChannelSwitched,
    Poke,
    /// A private message from someone else.
    Message,
    MicMuted,
    MicUnmuted,
}

impl Sound {
    pub const ALL: [Sound; 13] = [
        Sound::Connected,
        Sound::Disconnected,
        Sound::ConnectionLost,
        Sound::UserConnected,
        Sound::UserDisconnected,
        Sound::UserLostConnection,
        Sound::UserJoined,
        Sound::UserLeft,
        Sound::ChannelSwitched,
        Sound::Poke,
        Sound::Message,
        Sound::MicMuted,
        Sound::MicUnmuted,
    ];

    /// File name that replaces this sound in a custom folder.
    pub fn file_name(self) -> &'static str {
        match self {
            Sound::Connected => "connected.wav",
            Sound::Disconnected => "disconnected.wav",
            Sound::ConnectionLost => "connection_lost.wav",
            Sound::UserConnected => "user_connected.wav",
            Sound::UserDisconnected => "user_disconnected.wav",
            Sound::UserLostConnection => "user_lost_connection.wav",
            Sound::UserJoined => "user_joined.wav",
            Sound::UserLeft => "user_left.wav",
            Sound::ChannelSwitched => "channel_switched.wav",
            Sound::Poke => "poke.wav",
            Sound::Message => "message.wav",
            Sound::MicMuted => "mic_muted.wav",
            Sound::MicUnmuted => "mic_unmuted.wav",
        }
    }

    /// The event key for this sound in a pack's `settings.ini`.
    fn ini_key(self) -> &'static str {
        match self {
            Sound::Connected => "CONNECTION_CONNECTED",
            Sound::Disconnected => "CONNECTION_DISCONNECTED",
            Sound::ConnectionLost => "CONNECTION_LOST_CONNECTION",
            Sound::UserConnected => "CLIENT_CONNECTION_CONNECTED_CURRENT_CHANNEL",
            Sound::UserDisconnected => "CLIENT_CONNECTION_DISCONNECTED_CURRENT_CHANNEL",
            Sound::UserLostConnection => "CLIENT_CONNECTION_LOST_CONNECTION_CURRENT_CHANNEL",
            Sound::UserJoined => "CLIENT_SWITCHED_TO_CURRENT_CHANNEL_STAYS",
            Sound::UserLeft => "CLIENT_SWITCHED_FROM_CURRENT_CHANNEL_STAYS",
            Sound::ChannelSwitched => "YOU_SWITCHED_CHANNEL",
            Sound::Poke => "OTHER_RECEIVED_POKE",
            Sound::Message => "CHAT_RECEIVED_MESSAGE_CLIENT",
            Sound::MicMuted => "SOUND_CAPTURE_MUTED",
            Sound::MicUnmuted => "SOUND_CAPTURE_UNMUTED",
        }
    }

    fn notes(self) -> &'static [Note] {
        match self {
            Sound::Connected => &[Note { at: 0.00, hz: 659.3, gain: 0.9 }, Note { at: 0.09, hz: 987.8, gain: 1.0 }],
            Sound::Disconnected => &[Note { at: 0.00, hz: 987.8, gain: 1.0 }, Note { at: 0.09, hz: 659.3, gain: 0.9 }],
            Sound::ConnectionLost => &[
                Note { at: 0.00, hz: 659.3, gain: 0.9 },
                Note { at: 0.10, hz: 523.3, gain: 0.9 },
                Note { at: 0.20, hz: 392.0, gain: 1.0 },
            ],
            Sound::UserConnected | Sound::UserJoined => {
                &[Note { at: 0.00, hz: 880.0, gain: 0.6 }, Note { at: 0.07, hz: 1174.7, gain: 0.6 }]
            }
            Sound::UserDisconnected | Sound::UserLostConnection | Sound::UserLeft => {
                &[Note { at: 0.00, hz: 1174.7, gain: 0.6 }, Note { at: 0.07, hz: 880.0, gain: 0.6 }]
            }
            Sound::ChannelSwitched => &[Note { at: 0.00, hz: 784.0, gain: 0.7 }, Note { at: 0.00, hz: 1174.7, gain: 0.35 }],
            Sound::Poke => &[
                Note { at: 0.00, hz: 880.0, gain: 1.0 },
                Note { at: 0.12, hz: 880.0, gain: 1.0 },
                Note { at: 0.24, hz: 1318.5, gain: 1.1 },
            ],
            Sound::Message => &[Note { at: 0.00, hz: 1318.5, gain: 0.6 }, Note { at: 0.06, hz: 1568.0, gain: 0.6 }],
            Sound::MicMuted => &[Note { at: 0.00, hz: 523.3, gain: 0.6 }, Note { at: 0.06, hz: 392.0, gain: 0.6 }],
            Sound::MicUnmuted => &[Note { at: 0.00, hz: 392.0, gain: 0.6 }, Note { at: 0.06, hz: 523.3, gain: 0.6 }],
        }
    }
}

/// One bell-like tone: starts at `at` seconds into the sound.
struct Note {
    at: f32,
    hz: f32,
    gain: f32,
}

/// Each note rings this long; by then its decay is inaudible.
const NOTE_LEN: f32 = 0.35;
const ATTACK: f32 = 0.004;
const DECAY: f32 = 0.08;
/// Peak level of a chime at full notification volume.
const LEVEL: f32 = 0.22;
/// Pack files longer than this are cut short.
const MAX_FILE_SECS: usize = 4;

// ---- sound packs -----------------------------------------------------------

/// What to play for a sound.
pub enum Resolved {
    Chime,
    File(PathBuf),
    Silent,
}

/// The selected sound pack, as an index of file paths (no audio loaded).
pub enum Pack {
    BuiltIn,
    /// WAVs named after the sounds; missing ones fall back to the chimes.
    Custom(PathBuf),
    /// An ini pack: `None` means the pack keeps that event silent.
    Ini(HashMap<Sound, Option<PathBuf>>),
}

impl Pack {
    /// Opens the pack in `dir`: an ini pack if it has a `settings.ini`,
    /// otherwise a custom folder. `None` is the built-in chimes.
    pub fn open(dir: Option<&Path>) -> Self {
        let Some(dir) = dir else { return Pack::BuiltIn };
        let ini = dir.join("settings.ini");
        match std::fs::read_to_string(&ini) {
            Ok(text) => Pack::Ini(parse_ini(&text, dir)),
            Err(_) => Pack::Custom(dir.to_owned()),
        }
    }

    pub fn resolve(&self, sound: Sound) -> Resolved {
        match self {
            Pack::BuiltIn => Resolved::Chime,
            Pack::Custom(dir) => {
                let path = dir.join(sound.file_name());
                if path.is_file() { Resolved::File(path) } else { Resolved::Chime }
            }
            Pack::Ini(files) => match files.get(&sound) {
                Some(Some(path)) => Resolved::File(path.clone()),
                _ => Resolved::Silent,
            },
        }
    }
}

/// Reads the `[soundfiles]` section: `KEY = play("file.wav")`, or empty for silence.
fn parse_ini(text: &str, dir: &Path) -> HashMap<Sound, Option<PathBuf>> {
    let mut entries: HashMap<&str, Option<&str>> = HashMap::new();
    let mut in_section = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[soundfiles]");
            continue;
        }
        if !in_section || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let file = value.split('"').nth(1).filter(|f| !f.is_empty() && value.contains("play("));
        entries.insert(key.trim(), file);
    }
    Sound::ALL
        .into_iter()
        .map(|sound| {
            let file = entries.get(sound.ini_key()).copied().flatten();
            // T7 has no friend lists, so every user is the "neutral" client type.
            let path = file.map(|f| dir.join(f.replace("${clientType}", "neutral")));
            (sound, path)
        })
        .collect()
}

/// Sound packs found on this machine: (label, folder).
pub fn installed_packs() -> Vec<(String, PathBuf)> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let roots = [
        env("ProgramFiles").map(|p| p.join("TeamSpeak 3 Client").join("sound")),
        env("ProgramFiles(x86)").map(|p| p.join("TeamSpeak 3 Client").join("sound")),
        env("LOCALAPPDATA").map(|p| p.join("Programs").join("TeamSpeak 3 Client").join("sound")),
        env("APPDATA").map(|p| p.join("TS3Client").join("sound")),
        env("HOME").map(|p| p.join(".ts3client").join("sound")),
    ];
    let mut packs: Vec<(String, PathBuf)> = roots
        .into_iter()
        .flatten()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| {
            // Text-to-speech packs (`say("...")`, no audio files) aren't supported.
            std::fs::read_to_string(dir.join("settings.ini")).is_ok_and(|ini| ini.contains("play(") || !ini.contains("say("))
        })
        .map(|dir| {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (format!("Installed · {name}"), dir)
        })
        .collect();
    packs.sort();
    packs.dedup_by(|a, b| a.1 == b.1);
    packs
}

/// Reads any PCM/float WAV as 48 kHz mono, at most a few seconds.
pub fn load_wav(path: &Path) -> Result<Arc<[f32]>, hound::Error> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 / scale)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = interleaved.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect();
    let mut out = Vec::with_capacity(mono.len() * SAMPLE_RATE as usize / spec.sample_rate.max(1) as usize + 1);
    Resampler::new(spec.sample_rate, SAMPLE_RATE).push(&mono, &mut out);
    out.truncate(SAMPLE_RATE as usize * MAX_FILE_SECS);
    Ok(out.into())
}

// ---- mixing ----------------------------------------------------------------

enum Source {
    Chime(&'static [Note]),
    Samples(Arc<[f32]>),
}

struct Playing {
    source: Source,
    pos: usize,
    volume: f32,
}

impl Playing {
    fn len(&self) -> usize {
        match &self.source {
            Source::Chime(notes) => {
                let last = notes.iter().map(|n| n.at).fold(0.0, f32::max);
                ((last + NOTE_LEN) * SAMPLE_RATE as f32) as usize
            }
            Source::Samples(s) => s.len(),
        }
    }

    fn sample(&self, i: usize) -> f32 {
        match &self.source {
            Source::Chime(notes) => {
                let t = i as f32 / SAMPLE_RATE as f32;
                notes
                    .iter()
                    .filter(|n| t >= n.at && t < n.at + NOTE_LEN)
                    .map(|n| {
                        let dt = t - n.at;
                        let env = (dt / ATTACK).min(1.0) * (-dt / DECAY).exp();
                        let phase = std::f32::consts::TAU * n.hz * dt;
                        n.gain * env * (phase.sin() + 0.25 * (2.0 * phase).sin())
                    })
                    .sum::<f32>()
                    * LEVEL
            }
            Source::Samples(s) => s[i],
        }
    }
}

/// Sounds currently playing, mixed into the speaker output.
#[derive(Default)]
pub struct Effects {
    playing: Vec<Playing>,
}

impl Effects {
    pub fn play_chime(&mut self, sound: Sound, volume: f32) {
        self.playing.push(Playing { source: Source::Chime(sound.notes()), pos: 0, volume });
    }

    pub fn play_samples(&mut self, samples: Arc<[f32]>, volume: f32) {
        self.playing.push(Playing { source: Source::Samples(samples), pos: 0, volume });
    }

    pub fn active(&self) -> bool {
        !self.playing.is_empty()
    }

    pub fn clear(&mut self) {
        self.playing.clear();
    }

    /// Adds the next `out.len()` samples (48 kHz) of every playing sound.
    pub fn mix(&mut self, out: &mut [f32]) {
        for p in &mut self.playing {
            let len = p.len();
            for (o, i) in out.iter_mut().zip(p.pos..len) {
                *o += p.sample(i) * p.volume;
            }
            p.pos = (p.pos + out.len()).min(len);
        }
        self.playing.retain(|p| p.pos < p.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(fx: &mut Effects, chunk: usize, chunks: usize) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..chunks {
            let mut buf = vec![0.0; chunk];
            fx.mix(&mut buf);
            all.extend(buf);
        }
        all
    }

    #[test]
    fn every_chime_is_audible_short_and_finishes() {
        for sound in Sound::ALL {
            let mut fx = Effects::default();
            fx.play_chime(sound, 1.0);
            // Odd chunk size, like a real output callback.
            let out = render(&mut fx, 441, 100);
            let peak = out.iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!(peak > 0.05 && peak <= 1.0, "{sound:?} peak {peak}");
            assert!(!fx.active(), "{sound:?} still playing after 0.9 s");
        }
    }

    #[test]
    fn volume_scales_and_samples_play_once() {
        let (mut quiet, mut loud) = (Effects::default(), Effects::default());
        quiet.play_chime(Sound::Poke, 0.5);
        loud.play_chime(Sound::Poke, 1.0);
        let (q, l) = (render(&mut quiet, 480, 10), render(&mut loud, 480, 10));
        assert!(q.iter().zip(&l).all(|(q, l)| (q * 2.0 - l).abs() < 1e-6));

        let mut fx = Effects::default();
        fx.play_samples(Arc::from(vec![0.5f32; 100]), 1.0);
        let out = render(&mut fx, 480, 1);
        assert!(out[..100].iter().all(|&v| v == 0.5) && out[100..].iter().all(|&v| v == 0.0));
        assert!(!fx.active());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("t7-sounds-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_ini_packs() {
        let dir = temp_dir("ini");
        std::fs::write(
            dir.join("settings.ini"),
            "[info]\nCONNECTION_CONNECTED = play(\"ignored.wav\")\n[soundfiles]\n# connection\n\
             CONNECTION_CONNECTED       = play(\"connected.wav\")\n\
             CONNECTION_DISCONNECTED    =\n\
             CLIENT_CONNECTION_CONNECTED_CURRENT_CHANNEL = play(\"${clientType}_joined.wav\")\n",
        )
        .unwrap();
        let pack = Pack::open(Some(&dir));
        assert!(matches!(pack.resolve(Sound::Connected), Resolved::File(p) if p == dir.join("connected.wav")));
        assert!(matches!(pack.resolve(Sound::UserConnected), Resolved::File(p) if p == dir.join("neutral_joined.wav")));
        assert!(matches!(pack.resolve(Sound::Disconnected), Resolved::Silent), "empty entry is silence");
        assert!(matches!(pack.resolve(Sound::Poke), Resolved::Silent), "missing entry is silence");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn custom_folders_fall_back_to_chimes_and_load_wavs() {
        let dir = temp_dir("custom");
        let spec = hound::WavSpec { channels: 2, sample_rate: 24_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(dir.join("poke.wav"), spec).unwrap();
        for _ in 0..2400 {
            w.write_sample(i16::MAX / 2).unwrap();
            w.write_sample(i16::MAX / 2).unwrap();
        }
        w.finalize().unwrap();

        let pack = Pack::open(Some(&dir));
        assert!(matches!(pack.resolve(Sound::Message), Resolved::Chime));
        let Resolved::File(path) = pack.resolve(Sound::Poke) else { panic!("poke.wav not used") };
        let samples = load_wav(&path).unwrap();
        // 0.1 s of stereo at 24 kHz becomes 0.1 s of mono at 48 kHz, level kept.
        assert!((samples.len() as i32 - 4800).abs() <= 2, "{} samples", samples.len());
        assert!((samples[100] - 0.5).abs() < 0.01);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
