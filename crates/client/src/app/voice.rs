//! Voice: when the audio engine runs, whether UDP gets through, and the voice
//! settings dialog.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align, Layout, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::Speech;

use super::chat::LineKind;
use super::sharing::QUALITIES;
use super::widgets::{
    SLIDER_VALUE_W, Unit, described_switch, dialog_header, hint, list_row, row, row_hint, section, slider, value_box,
};
use super::{App, Conn};
use crate::audio::{AudioSettings, PttKey, TxMode, VoiceSink};
use crate::screen::Quality;
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
    pub(super) page: SettingsPage,
    /// (label, folder) of every selectable sound pack except the built-in one.
    pub(super) packs: Vec<(String, std::path::PathBuf)>,
    pub(super) inputs: Vec<String>,
    pub(super) outputs: Vec<String>,
    /// Waiting for a key press to bind push-to-talk.
    pub(super) binding: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsPage {
    Voice,
    Cleanup,
    Notifications,
    Sharing,
    About,
}

impl SettingsPage {
    const ALL: [Self; 5] = [Self::Voice, Self::Cleanup, Self::Notifications, Self::Sharing, Self::About];

    fn title(self) -> &'static str {
        match self {
            Self::Voice => "Voice",
            Self::Cleanup => "Sound cleanup",
            Self::Notifications => "Notifications",
            Self::Sharing => "Screen sharing",
            Self::About => "About",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Self::Voice => "Changes apply right away.",
            Self::Cleanup => "Runs on your microphone before anyone hears it.",
            Self::Notifications => "Sounds for what happens while you're on a server.",
            Self::Sharing => "What others get when you share your screen.",
            Self::About => "",
        }
    }
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

impl App {
    pub(super) fn open_voice_settings(&mut self) {
        let (inputs, outputs) = audio::device_names();
        let custom = sounds_dir().map(|d| ("Custom folder".to_owned(), d));
        let packs = custom.into_iter().chain(sounds::installed_packs()).collect();
        self.audio_dialog = Some(AudioDialog { page: SettingsPage::Voice, packs, inputs, outputs, binding: false });
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
        let mut quality = self.settings.screen_quality;
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
        let level = Level { db: self.voice.shared.level_db(), open: self.voice.shared.transmitting() };
        let height = (ctx.content_rect().height() - 48.0).clamp(240.0, SETTINGS_H);
        let frame = theme::modal_frame().inner_margin(Margin::ZERO);

        let modal = theme::modal("voice-settings").frame(frame).show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                settings_nav(ui, &mut dlg.page, height);
                let (rect, _) = ui.allocate_exact_size(vec2(PAGE_W, height), Sense::hover());
                let inner = rect.shrink2(vec2(22.0, 20.0));
                let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
                let page = dlg.page;
                dialog_header(ui, page.title(), page.note());
                let body = ui.available_height() - theme::CONTROL_H - 14.0;
                egui::ScrollArea::vertical().max_height(body).auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().interact_size.x = SLIDER_VALUE_W;
                    match page {
                        SettingsPage::Voice => restart = voice_page(ui, &mut a, dlg, speech, level),
                        SettingsPage::Cleanup => cleanup_page(ui, &mut a),
                        SettingsPage::Notifications => test_sound = notifications_page(ui, &mut a, dlg),
                        SettingsPage::Sharing => sharing_page(ui, &mut quality),
                        SettingsPage::About => about_page(ui, &self.updater),
                    }
                });
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui.add(Btn::primary("Done")).clicked();
                });
            });
        });
        if modal.should_close() && !dlg.binding {
            close = true;
        }
        let page = dlg.page;

        if a.sound_pack != self.settings.audio.sound_pack {
            self.sound_pack = Pack::open(a.sound_pack.as_deref());
        }
        if a != self.settings.audio {
            self.voice.shared.set_settings(&a);
            self.settings.audio = a;
        }
        self.settings.screen_quality = quality;
        if test_sound {
            self.sound(Sound::Poke);
        }
        if restart {
            self.voice.engine = None; // reopened with the new devices next frame
        }
        if close {
            self.audio_dialog = None;
        } else if page == SettingsPage::Voice {
            ctx.request_repaint_after(Duration::from_millis(50)); // live level meter
        }
    }
}

const SETTINGS_H: f32 = 530.0;
const NAV_W: f32 = 180.0;
const PAGE_W: f32 = 500.0;

/// The microphone's live level, and whether it is sending.
#[derive(Clone, Copy)]
struct Level {
    db: f32,
    open: bool,
}

fn settings_nav(ui: &mut egui::Ui, page: &mut SettingsPage, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(NAV_W, height), Sense::hover());
    ui.painter().vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, theme::BORDER));
    let inner = rect.shrink2(vec2(8.0, 16.0));
    let ui = &mut ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
    ui.spacing_mut().item_spacing.y = 2.0;
    for p in SettingsPage::ALL {
        if list_row(ui, p.title(), "", *page == p, Sense::click()).clicked() {
            *page = p;
        }
    }
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        let version = format!("T7 {}", crate::update::VERSION);
        ui.label(RichText::new(version).size(11.5).color(theme::TEXT_FAINT));
    });
}

fn voice_page(ui: &mut egui::Ui, a: &mut AudioSettings, dlg: &mut AudioDialog, speech: Speech, level: Level) -> bool {
    section(ui, "Devices");
    let mic = row(ui, "Microphone", |ui| device_combo(ui, "mic", &mut a.input_device, &dlg.inputs));
    let speakers = row(ui, "Speakers", |ui| device_combo(ui, "speakers", &mut a.output_device, &dlg.outputs));

    section(ui, "Talk with");
    let modes = [(TxMode::VoiceActivation, "Voice activation"), (TxMode::PushToTalk, "Push to talk")];
    theme::segmented(ui, &mut a.mode, &modes);
    match speech {
        Speech::Allowed => {}
        Speech::NotHere => hint(ui, "You can't speak in this channel, so nothing is sent."),
        Speech::ServerMuted => hint(ui, "A moderator muted you on the server, so nothing is sent."),
    }
    ui.add_space(6.0);
    match a.mode {
        TxMode::VoiceActivation => {
            row(ui, "Sensitivity", |ui| sensitivity(ui, &mut a.threshold_db, level));
            row_hint(
                ui,
                "Talk normally. The green bar should pass the handle when you speak and stay left of it \
                 when you're quiet.",
            );
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
            row_hint(
                ui,
                if cfg!(windows) {
                    "Works while other apps, like games, are focused."
                } else {
                    "Works while T7 is focused."
                },
            );
        }
    }

    section(ui, "Volume");
    row(ui, "Microphone", |ui| slider(ui, &mut a.input_gain_db, -20.0..=20.0, Unit::Db));
    row(ui, "Speakers", |ui| slider(ui, &mut a.output_volume, 0.0..=2.0, Unit::Percent));
    mic | speakers
}

fn sensitivity(ui: &mut egui::Ui, threshold_db: &mut f32, level: Level) {
    // Sized and painted like the other sliders, as egui lays one out.
    let width = ui.available_width() - SLIDER_VALUE_W - ui.spacing().item_spacing.x;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 16.0), Sense::click_and_drag());
    let knob = rect.height() / 2.5;
    let track = rect.x_range().shrink(knob);
    let span = METER_MAX_DB - METER_MIN_DB;
    if let Some(pos) = resp.interact_pointer_pos() {
        let t = ((pos.x - track.min) / track.span()).clamp(0.0, 1.0);
        *threshold_db = (METER_MIN_DB + t * span).round();
    }
    let x = |db: f32| track.min + track.span() * ((db - METER_MIN_DB) / span).clamp(0.0, 1.0);
    let rail_h = ui.spacing().slider_rail_height / 2.0;
    let y = rect.center().y;
    let rail = egui::Rect::from_x_y_ranges(rect.x_range(), y - rail_h..=y + rail_h);
    let radius = ui.visuals().widgets.inactive.corner_radius;
    let p = ui.painter();
    p.rect_filled(rail, radius, ui.visuals().widgets.inactive.bg_fill);
    let heard = rail.with_max_x(x(level.db));
    p.rect_filled(heard, radius, if level.open { theme::GREEN } else { theme::TEXT_FAINT });
    let v = ui.style().interact(&resp);
    let handle = egui::pos2(x(*threshold_db), y);
    p.add(egui::epaint::CircleShape {
        center: handle,
        radius: knob + v.expansion,
        fill: v.bg_fill,
        stroke: v.fg_stroke,
    });
    value_box(ui, threshold_db, METER_MIN_DB..=METER_MAX_DB, Unit::Db);
}

fn cleanup_page(ui: &mut egui::Ui, a: &mut AudioSettings) {
    let c = &mut a.cleanup;
    described_switch(ui, &mut c.noise_suppression, "Noise suppression", "Removes fans, keyboards and background hum.");
    let echo = "Stops others hearing themselves through your speakers.";
    described_switch(ui, &mut c.echo_cancellation, "Echo cancellation", echo);
    let gain = "Keeps you at a steady level when you lean back or shout.";
    described_switch(ui, &mut c.auto_gain, "Automatic volume", gain);
}

/// Returns whether Test was clicked.
fn notifications_page(ui: &mut egui::Ui, a: &mut AudioSettings, dlg: &AudioDialog) -> bool {
    let mut test = false;
    let what = "Chimes for connecting, people joining and leaving, pokes and private messages.";
    described_switch(ui, &mut a.sounds, "Notification sounds", what);
    ui.add_space(8.0);
    ui.add_enabled_ui(a.sounds, |ui| {
        row(ui, "Sounds", |ui| {
            let shown = match &a.sound_pack {
                None => BUILT_IN.to_owned(),
                Some(dir) => dlg
                    .packs
                    .iter()
                    .find(|(_, d)| d == dir)
                    .map_or_else(|| dir.display().to_string(), |(label, _)| label.clone()),
            };
            let width = ui.available_width() - 64.0;
            egui::ComboBox::from_id_salt("sound-pack").width(width).selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut a.sound_pack, None, BUILT_IN);
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
        row(ui, "Volume", |ui| slider(ui, &mut a.sounds_volume, 0.0..=1.0, Unit::Percent));
        row_hint(ui, "Separate from voices and from your microphone.");
    });
    test
}

const BUILT_IN: &str = "T7 (built in)";

fn sharing_page(ui: &mut egui::Ui, quality: &mut Quality) {
    row(ui, "Quality", |ui| theme::segmented(ui, quality, &QUALITIES));
    row_hint(ui, "Higher quality needs a faster upload. You can also change it each time you start sharing.");
}

fn about_page(ui: &mut egui::Ui, updater: &Updater) {
    row(ui, "Version", |ui| ui.label(RichText::new(crate::update::VERSION).color(theme::TEXT)));
    if !updater.enabled() {
        return row_hint(ui, "This build doesn't update itself.");
    }
    row(ui, "Updates", |ui| {
        let state = match updater.status() {
            Status::Idle | Status::UpToDate => "Up to date".to_owned(),
            Status::Checking => "Checking…".to_owned(),
            Status::Downloading(v) => format!("Downloading {v}…"),
            Status::Ready(v) => format!("{v} is ready, restart to update"),
            Status::Failed(e) => e,
        };
        ui.label(RichText::new(state).color(theme::TEXT));
        if ui.add(Btn::secondary("Check now").small()).clicked() {
            updater.check_now();
        }
    });
}
