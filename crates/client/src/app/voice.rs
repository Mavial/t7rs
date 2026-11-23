//! Voice: when the audio engine runs, whether UDP gets through, and the voice
//! settings dialog.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, vec2};
use t7_proto::Speech;

use super::chat::LineKind;
use super::widgets::{SLIDER_VALUE_W, Unit, dialog_header, hint, row, section, slider};
use super::{App, Conn};
use crate::audio::{AudioSettings, PttKey, TxMode, VoiceSink};
use crate::sounds::{Pack, Sound};
use crate::theme::Btn;
use crate::update::{Status, Updater};
use crate::{audio, ptt, settings, sounds, theme};

/// Voice is considered blocked when no UDP pong arrived for this long.
const VOICE_TIMEOUT: Duration = Duration::from_secs(4);

pub(super) struct Voice {
    pub(super) shared: Arc<audio::Shared>,
    pub(super) playback: Arc<audio::Playback>,
    sink: VoiceSink,
    /// Open while connected or while the voice settings are shown.
    engine: Option<audio::Engine>,
    /// Last UDP pong: proof that voice gets through.
    seen: Option<Instant>,
    rtt: Option<Duration>,
    connected_at: Option<Instant>,
}

impl Voice {
    pub(super) fn new(settings: AudioSettings, playback: Arc<audio::Playback>, sink: VoiceSink) -> Self {
        let shared = audio::Shared::new(settings);
        Self { shared, playback, sink, engine: None, seen: None, rtt: None, connected_at: None }
    }

    /// Starts waiting for the first UDP pong of a new connection.
    pub(super) fn connected(&mut self) {
        self.connected_at = Some(Instant::now());
        self.seen = None;
    }

    pub(super) fn disconnected(&mut self) {
        self.connected_at = None;
        self.seen = None;
        self.rtt = None;
    }

    pub(super) fn pong(&mut self, rtt: Duration) {
        self.rtt = Some(rtt);
        self.seen = Some(Instant::now());
    }

    /// Round trip of the last UDP pong.
    pub(super) fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    fn reachable(&self) -> bool {
        self.seen.is_some_and(|t| t.elapsed() < VOICE_TIMEOUT)
    }

    /// Connected long enough that a missing pong means UDP is blocked.
    pub(super) fn blocked(&self) -> bool {
        !self.reachable() && self.connected_at.is_some_and(|t| t.elapsed() > VOICE_TIMEOUT)
    }
}

pub(super) struct AudioDialog {
    /// (label, folder) of every selectable sound pack except the built-in one.
    pub(super) packs: Vec<(String, std::path::PathBuf)>,
    pub(super) inputs: Vec<String>,
    pub(super) outputs: Vec<String>,
    /// Waiting for a key press to bind push-to-talk.
    pub(super) binding: bool,
}

pub(super) fn sounds_dir() -> Option<std::path::PathBuf> {
    settings::data_dir().map(|d| d.join("sounds"))
}

fn open_sounds_folder() {
    let Some(dir) = sounds_dir() else { return };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return tracing::warn!("cannot create {}: {e}", dir.display());
    }
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(&dir).spawn() {
        tracing::warn!("cannot open {}: {e}", dir.display());
    }
}

/// A device picker; `None` follows the system default.
fn device_combo(ui: &mut egui::Ui, id: &str, value: &mut Option<String>, names: &[String]) -> bool {
    let mut changed = false;
    let shown = value.clone().unwrap_or_else(|| "System default".to_owned());
    egui::ComboBox::from_id_salt(id).width(ui.available_width()).selected_text(shown).show_ui(ui, |ui| {
        changed |= ui.selectable_value(value, None, "System default").changed();
        for name in names {
            changed |= ui.selectable_value(value, Some(name.clone()), name).changed();
        }
    });
    changed
}

const METER_MIN_DB: f32 = -70.0;
const METER_MAX_DB: f32 = -5.0;

/// Live microphone level with the activation threshold marked.
fn level_meter(ui: &mut egui::Ui, level_db: f32, threshold_db: f32, open: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let t = |db: f32| ((db - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)).clamp(0.0, 1.0);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), theme::SURFACE_ACTIVE);
    let mut fill = rect;
    fill.set_width(rect.width() * t(level_db));
    p.rect_filled(fill, CornerRadius::same(3), if open { theme::GREEN } else { theme::TEXT_FAINT });
    let x = rect.left() + rect.width() * t(threshold_db);
    p.line_segment(
        [egui::pos2(x, rect.top() - 4.0), egui::pos2(x, rect.bottom() + 4.0)],
        Stroke::new(2.0, theme::ACCENT_TEXT),
    );
}

impl App {
    pub(super) fn open_voice_settings(&mut self) {
        let (inputs, outputs) = audio::device_names();
        let custom = sounds_dir().map(|d| ("Custom folder".to_owned(), d));
        let packs = custom.into_iter().chain(sounds::installed_packs()).collect();
        self.audio_dialog = Some(AudioDialog { packs, inputs, outputs, binding: false });
    }

    /// Keeps the audio engine and its shared state in line with the app state.
    pub(super) fn drive_audio(&mut self, ctx: &egui::Context) {
        let s = self.settings.status;
        let silenced = self.my_speech() != Speech::Allowed;
        let v = &mut self.voice;
        v.shared.set_status(s.muted || silenced, s.deafened);
        let connected = matches!(self.conn, Conn::Connected { .. });
        // A last sound (like "disconnected") finishes before the devices close.
        let finishing = v.engine.is_some() && v.playback.effects_active();
        let wanted = connected || self.audio_dialog.is_some() || finishing;
        if wanted && v.engine.is_none() {
            v.engine = Some(audio::Engine::start(&v.shared, &v.playback, v.sink.clone(), ctx));
        } else if !wanted {
            v.engine = None;
            v.playback.clear_effects();
        }
        if finishing && !connected {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let binding = self.audio_dialog.as_ref().is_some_and(|d| d.binding);
        if let (TxMode::PushToTalk, Some(key), false) = (self.settings.audio.mode, self.settings.audio.ptt_key, binding)
        {
            v.shared.set_ptt_local(ctx.input(|i| ptt::is_down(key.vk, i)));
        }
        // Talking rings fade out without any input event, so poll while anyone talks.
        if v.shared.transmitting() || !v.playback.speaking().is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if matches!(self.conn, Conn::Connected { .. }) && !v.reachable() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(err) = v.shared.take_error() {
            let tab = self.active_tab;
            self.push(tab, LineKind::Error, err);
        }
    }

    pub(super) fn audio_dialog(&mut self, ctx: &egui::Context) {
        let speech = self.my_speech();
        let Some(dlg) = &mut self.audio_dialog else { return };
        let mut a: AudioSettings = self.settings.audio.clone();
        let mut restart = false;
        let mut close = false;
        let mut test_sound = false;
        if dlg.binding {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                dlg.binding = false;
            } else if let Some(vk) = ctx.input(ptt::capture) {
                a.ptt_key = Some(PttKey { vk });
                dlg.binding = false;
            }
        }
        let level = self.voice.shared.level_db();
        let open = self.voice.shared.transmitting();

        let modal = theme::modal("voice-settings").show(ctx, |ui| {
            ui.set_width(420.0);
            dialog_header(ui, "Voice settings", "Changes apply right away.");

            // Keeps the title and the Done button on screen when the window is shorter than the
            // dialog; 150 points covers both plus the modal's margins.
            let body_height = (ui.ctx().content_rect().height() - 150.0).max(120.0);
            egui::ScrollArea::vertical().max_height(body_height).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().interact_size.x = SLIDER_VALUE_W;
                restart = devices_section(ui, &mut a, dlg);
                transmit_section(ui, &mut a, dlg, speech, Level { db: level, open });
                cleanup_section(ui, &mut a);
                volume_section(ui, &mut a);
                test_sound = notifications_section(ui, &mut a, dlg);
            });
            ui.add_space(16.0);
            close = version_footer(ui, &self.updater);
        });
        if modal.should_close() && !dlg.binding {
            close = true;
        }

        if a.sound_pack != self.settings.audio.sound_pack {
            self.sound_pack = Pack::open(a.sound_pack.as_deref());
        }
        if a != self.settings.audio {
            self.voice.shared.set_settings(&a);
            self.settings.audio = a;
        }
        if test_sound {
            self.sound(Sound::Poke);
        }
        if restart {
            self.voice.engine = None; // reopened with the new devices next frame
        }
        if close {
            self.audio_dialog = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(50)); // live level meter
        }
    }
}

/// The microphone's live level, and whether it is sending.
#[derive(Clone, Copy)]
struct Level {
    db: f32,
    open: bool,
}

/// Returns whether a device changed, so the engine reopens.
fn devices_section(ui: &mut egui::Ui, a: &mut AudioSettings, dlg: &AudioDialog) -> bool {
    section(ui, "Devices");
    let mic = row(ui, "Microphone", |ui| device_combo(ui, "mic", &mut a.input_device, &dlg.inputs));
    let speakers = row(ui, "Speakers", |ui| device_combo(ui, "speakers", &mut a.output_device, &dlg.outputs));
    mic | speakers
}

fn transmit_section(ui: &mut egui::Ui, a: &mut AudioSettings, dlg: &mut AudioDialog, speech: Speech, level: Level) {
    section(ui, "Transmit");
    match speech {
        Speech::Allowed => {}
        Speech::NotHere => hint(ui, "You can't speak in this channel, so nothing is sent."),
        Speech::ServerMuted => hint(ui, "A moderator muted you on the server, so nothing is sent."),
    }
    row(ui, "Mode", |ui| {
        theme::segmented(
            ui,
            &mut a.mode,
            &[(TxMode::VoiceActivation, "Voice activation"), (TxMode::PushToTalk, "Push to talk")],
        );
    });
    match a.mode {
        TxMode::VoiceActivation => {
            row(ui, "Input level", |ui| level_meter(ui, level.db, a.threshold_db, level.open));
            row(ui, "Activate at", |ui| {
                slider(ui, &mut a.threshold_db, METER_MIN_DB..=METER_MAX_DB, Unit::Db);
            });
            hint(ui, "Talk normally: the bar should cross the line, and stay left of it when you're quiet.");
        }
        TxMode::PushToTalk => {
            row(ui, "Key", |ui| {
                let text = match (dlg.binding, a.ptt_key) {
                    (true, _) => "Press a key or mouse button…".to_owned(),
                    (false, Some(k)) => ptt::label(k.vk),
                    (false, None) => "Click to set a key".to_owned(),
                };
                if ui.add(Btn::secondary(&text).min_width(200.0)).clicked() {
                    dlg.binding = true;
                }
                if level.open {
                    ui.label(RichText::new("Transmitting").size(12.5).color(theme::GREEN));
                }
            });
            hint(
                ui,
                if cfg!(windows) {
                    "Works while other apps, like games, are focused."
                } else {
                    "Works while T7 is focused."
                },
            );
        }
    }
}

fn cleanup_section(ui: &mut egui::Ui, a: &mut AudioSettings) {
    section(ui, "Sound cleanup");
    theme::switch(ui, &mut a.cleanup.noise_suppression, "Noise suppression")
        .on_hover_text("Removes fans, hum, traffic and other background noise");
    theme::switch(ui, &mut a.cleanup.echo_cancellation, "Echo cancellation")
        .on_hover_text("Stops others from hearing themselves when you use speakers instead of headphones");
    theme::switch(ui, &mut a.cleanup.auto_gain, "Automatic volume")
        .on_hover_text("Brings a quiet microphone up to a normal level");
}

fn volume_section(ui: &mut egui::Ui, a: &mut AudioSettings) {
    section(ui, "Volume");
    row(ui, "Microphone", |ui| {
        slider(ui, &mut a.input_gain_db, -20.0..=20.0, Unit::Db);
    });
    row(ui, "Speakers", |ui| slider(ui, &mut a.output_volume, 0.0..=2.0, Unit::Percent));
}

/// Returns whether Test was clicked.
fn notifications_section(ui: &mut egui::Ui, a: &mut AudioSettings, dlg: &AudioDialog) -> bool {
    let mut test = false;
    section(ui, "Notifications");
    theme::switch(ui, &mut a.sounds, "Notification sounds")
        .on_hover_text("Chimes for connecting, people joining and leaving, pokes and private messages");
    ui.add_enabled_ui(a.sounds, |ui| {
        row(ui, "Volume", |ui| slider(ui, &mut a.sounds_volume, 0.0..=1.0, Unit::Percent));
        row(ui, "Sound pack", |ui| {
            let shown = match &a.sound_pack {
                None => "Built-in".to_owned(),
                Some(dir) => dlg
                    .packs
                    .iter()
                    .find(|(_, d)| d == dir)
                    .map_or_else(|| dir.display().to_string(), |(label, _)| label.clone()),
            };
            let width = ui.available_width() - 64.0;
            egui::ComboBox::from_id_salt("sound-pack").width(width).selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut a.sound_pack, None, "Built-in");
                for (label, dir) in &dlg.packs {
                    ui.selectable_value(&mut a.sound_pack, Some(dir.clone()), label);
                }
            });
            test = ui.add(Btn::secondary("Test").min_width(56.0)).clicked();
        });
        if a.sound_pack.is_some() && a.sound_pack == sounds_dir() {
            let names: Vec<&str> = Sound::ALL.iter().map(|s| s.file_name()).collect();
            row(ui, "", |ui| {
                if ui
                    .add(Btn::secondary("Open custom folder…").small())
                    .on_hover_text(format!(
                        "WAV files with these names replace the built-in sounds: {}",
                        names.join(", ")
                    ))
                    .clicked()
                {
                    open_sounds_folder();
                }
            });
        }
    });
    test
}

/// The version and update state, and the Done button. Returns whether Done was clicked.
fn version_footer(ui: &mut egui::Ui, updater: &Updater) -> bool {
    let mut done = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let version = format!("T7 {}", crate::update::VERSION);
        ui.label(RichText::new(version).size(12.0).color(theme::TEXT_FAINT));
        if updater.enabled() {
            let state = match updater.status() {
                Status::Idle | Status::UpToDate => "up to date".to_owned(),
                Status::Checking => "checking for updates…".to_owned(),
                Status::Downloading(v) => format!("downloading {v}…"),
                Status::Ready(v) => format!("{v} ready, restart to update"),
                Status::Failed(e) => e,
            };
            ui.label(RichText::new(format!("· {state}")).size(12.0).color(theme::TEXT_FAINT));
            if ui.add(Btn::ghost("Check now").small()).clicked() {
                updater.check_now();
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            done = ui.add(Btn::primary("Done")).clicked();
        });
    });
    done
}
