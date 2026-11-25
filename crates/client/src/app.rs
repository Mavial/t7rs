//! The client UI: the app's state, and what the window shell calls each frame.
//! Each part of the window lives in a submodule.

mod channel_dialog;
mod chat;
mod connect;
mod events;
mod moderation;
mod server_settings;
mod sharing;
mod shelf;
mod sidebar;
#[cfg(test)]
mod snapshot;
mod tree;
mod voice;
mod widgets;

use std::time::Duration;

use t7_proto::model::ServerView;
use t7_proto::perms::{Perm, Scope};
use t7_proto::{C2S, ChannelId, ClientId, ClientStatus, Speech};

use crate::net::{Net, ServerKey, SessionId};
use crate::settings::Settings;
use crate::sounds::{Pack, Resolved, Sound};
use crate::{audio, sounds, theme};
use channel_dialog::ChannelDialog;
use chat::{Tab, base_tabs};
use connect::{CertChange, ConnectForm};
use moderation::{Moderation, PokeAlert, PokeForm};
use server_settings::ServerSettings;
use voice::{AudioDialog, Voice};

/// Where and how we log in: kept to retry after trusting a changed
/// certificate, and to reconnect.
#[derive(Clone)]
struct Login {
    address: String,
    password: String,
}

enum Conn {
    Disconnected,
    Connecting(Login),
    Connected { server: ServerKey, login: Login },
    Lost { server: ServerKey, login: Login, why: Lost, last: Box<ServerView> },
}

#[derive(Clone)]
struct Lost {
    headline: String,
    detail: String,
}

/// Connect-on-startup request from the command line.
#[derive(Default)]
pub struct Launch {
    pub address: String,
    pub nickname: Option<String>,
}

pub struct App {
    net: Net,
    settings: Settings,
    session: SessionId,
    conn: Conn,
    view: Option<ServerView>,
    ping: Option<Duration>,
    tabs: Vec<Tab>,
    active_tab: usize,
    input: String,
    focus_input: bool,
    form: Option<ConnectForm>,
    /// Shown on the empty state / connect dialog after a failed attempt.
    last_error: Option<String>,
    voice: Voice,
    audio_dialog: Option<AudioDialog>,
    poke_form: Option<PokeForm>,
    pokes: std::collections::VecDeque<PokeAlert>,
    sound_pack: Pack,
    channel_dialog: Option<ChannelDialog>,
    moderation: Option<Moderation>,
    server_settings: Option<ServerSettings>,
    /// Who kicked us off the server and why, for when the connection closes.
    kicked: Option<Lost>,
    cert_change: Option<CertChange>,
    /// Ask the window manager to flash the taskbar entry.
    attention: bool,
    updater: crate::update::Updater,
    /// Set by the Restart button: the shell closes the window after this frame.
    quit: bool,
    sharing: Option<sharing::Sharing>,
    watching: Vec<sharing::Watching>,
    screen_wake: crate::watch::Wake,
    share_sources: Vec<crate::screen::SourceInfo>,
    shelf: shelf::Shelf,
}

impl App {
    pub fn new(
        ctx: &egui::Context,
        mut settings: Settings,
        launch: Option<Launch>,
        screen_wake: crate::watch::Wake,
    ) -> Self {
        theme::apply(ctx);
        if settings.nickname.is_empty() {
            settings.nickname =
                std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "User".into());
        }
        let playback = audio::Playback::new(ctx.clone());
        let net = Net::spawn(ctx.clone(), playback.clone());
        let voice = Voice::new(settings.audio.clone(), playback, net.voice_sink());
        let mut app = Self {
            net,
            settings,
            session: 0,
            conn: Conn::Disconnected,
            view: None,
            ping: None,
            tabs: base_tabs(),
            active_tab: 0,
            input: String::new(),
            focus_input: false,
            form: None,
            last_error: None,
            voice,
            audio_dialog: None,
            poke_form: None,
            pokes: std::collections::VecDeque::default(),
            sound_pack: Pack::BuiltIn,
            channel_dialog: None,
            moderation: None,
            server_settings: None,
            kicked: None,
            cert_change: None,
            attention: false,
            updater: crate::update::Updater::start(ctx.clone()),
            quit: false,
            sharing: None,
            watching: Vec::new(),
            screen_wake,
            share_sources: Vec::new(),
            shelf: shelf::Shelf::new(),
        };
        app.sound_pack = Pack::open(app.settings.audio.sound_pack.as_deref());
        if let Some(l) = launch {
            let nick = l.nickname.unwrap_or_else(|| app.settings.nickname.clone());
            app.connect(l.address, nick, String::new());
        }
        app
    }

    fn set_status(&mut self, status: ClientStatus) {
        let mic_off = |s: ClientStatus| s.muted || s.deafened;
        let was_off = mic_off(self.settings.status);
        self.settings.status = status;
        if matches!(self.conn, Conn::Connected { .. }) {
            self.net.send(C2S::SetStatus(status));
            match (was_off, mic_off(status)) {
                (false, true) => self.sound(Sound::MicMuted),
                (true, false) => self.sound(Sound::MicUnmuted),
                _ => {}
            }
        }
    }

    fn my_speech(&self) -> Speech {
        self.view.as_ref().and_then(ServerView::me).map_or(Speech::Allowed, |me| me.speech)
    }

    fn shown_view(&self) -> Option<&ServerView> {
        match &self.conn {
            Conn::Lost { last, .. } => Some(last),
            _ => self.view.as_ref(),
        }
    }

    fn server_key(&self) -> Option<&ServerKey> {
        match &self.conn {
            Conn::Connected { server, .. } | Conn::Lost { server, .. } => Some(server),
            Conn::Disconnected | Conn::Connecting(_) => None,
        }
    }

    fn reconnect(&mut self) {
        let Conn::Lost { login, .. } = &self.conn else { return };
        let Login { address, password } = login.clone();
        let nickname = self.settings.nickname.clone();
        self.connect(address, nickname, password);
    }

    fn forget_lost(&mut self) {
        if matches!(self.conn, Conn::Lost { .. }) {
            self.conn = Conn::Disconnected;
            self.active_tab = 0;
        }
    }

    fn sound(&self, sound: Sound) {
        let a = &self.settings.audio;
        if !a.sounds {
            return;
        }
        let playback = &self.voice.playback;
        match self.sound_pack.resolve(sound) {
            Resolved::Chime => playback.play_chime(sound, a.sounds_volume),
            Resolved::Silent => {}
            Resolved::File(path) => match sounds::load_wav(&path) {
                Ok(samples) => playback.play_samples(samples, a.sounds_volume),
                Err(e) => {
                    tracing::warn!("sound pack file {}: {e}", path.display());
                    playback.play_chime(sound, a.sounds_volume);
                }
            },
        }
    }

    fn user_volume(&self, nickname: &str) -> f32 {
        self.settings.user_volumes.get(nickname).copied().unwrap_or(1.0)
    }

    fn apply_user_volume(&self, id: ClientId, nickname: &str) {
        self.voice.playback.set_user_volume(id, self.user_volume(nickname));
    }

    fn set_user_volume(&mut self, id: ClientId, nickname: &str, volume: f32) {
        if volume == 1.0 {
            self.settings.user_volumes.remove(nickname);
        } else {
            self.settings.user_volumes.insert(nickname.to_owned(), volume);
        }
        self.voice.playback.set_user_volume(id, volume);
    }

    fn channel_name(&self, id: ChannelId) -> String {
        self.view.as_ref().and_then(|v| v.channels.get(&id)).map_or_else(|| "?".into(), |c| c.name.clone())
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let [connect, mute, deafen, away, settings] = ctx.input_mut(|i| {
            use egui::{Key, KeyboardShortcut, Modifiers};
            let shift = Modifiers::COMMAND | Modifiers::SHIFT;
            [
                (Modifiers::COMMAND, Key::S),
                (shift, Key::M),
                (shift, Key::D),
                (shift, Key::A),
                (Modifiers::COMMAND, Key::Comma),
            ]
            .map(|(modifiers, key)| i.consume_shortcut(&KeyboardShortcut::new(modifiers, key)))
        });
        if connect && self.form.is_none() {
            self.open_connect_form();
        }
        if mute {
            let s = self.settings.status;
            self.set_status(ClientStatus { muted: !s.muted, ..s });
        }
        if deafen {
            let s = self.settings.status;
            self.set_status(ClientStatus { deafened: !s.deafened, ..s });
        }
        if away {
            let s = self.settings.status;
            self.set_status(ClientStatus { away: !s.away, ..s });
        }
        if settings && self.audio_dialog.is_none() {
            self.open_voice_settings();
        }
    }
}

/// Where kicked people and those in a deleted channel go.
fn default_channel_name(view: &ServerView) -> String {
    view.channels.values().find(|c| c.default).map_or_else(|| "the default channel".into(), |c| c.name.clone())
}

fn manages(view: &ServerView, scope: Scope) -> bool {
    match scope {
        Scope::Channel(id) => view.may_in(id, Perm::ManageChannels),
        Scope::Server => view.may(Perm::ManageChannels),
    }
}

/// What the window shell needs from the app.
impl App {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll_network();
        self.sync_screens();
        let ctx = ui.ctx().clone();
        self.sync_shelf(&ctx);
        self.drive_audio(&ctx);
        self.shortcuts(&ctx);
        self.sidebar(ui);
        self.main_pane(ui);
        self.connect_dialog(&ctx);
        self.audio_dialog(&ctx);
        self.poke_dialog(&ctx);
        self.moderation_dialog(&ctx);
        self.server_settings_dialog(&ctx);
        self.poke_alert(&ctx);
        self.channel_dialog(&ctx);
        self.cert_change_dialog(&ctx);
    }

    /// True once after a poke arrived: the shell flashes the taskbar entry.
    pub fn take_attention(&mut self) -> bool {
        std::mem::take(&mut self.attention)
    }

    pub fn title(&self) -> String {
        match &self.view {
            Some(v) => format!("{} — T7", v.info.name),
            None => "T7".to_owned(),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn on_exit(&mut self) {
        self.disconnect();
        // A downloaded update is put in place now and runs next time.
        self.updater.install();
        // Give the network thread a moment to send the goodbye.
        std::thread::sleep(Duration::from_millis(50));
    }

    pub fn take_quit(&mut self) -> bool {
        std::mem::take(&mut self.quit)
    }
}
