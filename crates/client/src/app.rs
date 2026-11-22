use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Margin, RichText, Sense, Stroke,
    text::LayoutJob, vec2,
};
use t7_proto::model::ServerView;
use t7_proto::{
    C2S, ChannelId, ChannelSettings, ClientId, ClientStatus, LeaveReason, MAX_CHANNEL_DEPTH, MAX_CHANNEL_NAME_LEN,
    MAX_CHANNEL_TOPIC_LEN, PasswordChange, S2C, TextTarget,
};

use crate::audio::{self, AudioSettings, PttKey, TxMode, VoiceSink};
use crate::icons::{self, Icon};
use crate::net::{Net, NetEvent, SessionId};
use crate::settings::{self, Recent, Settings};
use crate::sounds::{self, Pack, Resolved, Sound};
use crate::{ptt, theme};

const MAX_LINES: usize = 1000;
const MAX_RECENT: usize = 8;
/// Server prefilled for first-time users. Set at build time, e.g.
/// `T7_DEFAULT_SERVER=example.com cargo build --release -p t7-client`.
const DEFAULT_SERVER: Option<&str> = option_env!("T7_DEFAULT_SERVER");

enum Conn {
    Disconnected,
    Connecting { address: String },
    Connected,
}

enum LineKind {
    System,
    Error,
    Msg { from: ClientId, name: String },
}

struct Line {
    time: String,
    kind: LineKind,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TabKind {
    Server,
    Channel,
    Private(ClientId),
}

struct Tab {
    kind: TabKind,
    title: String,
    lines: Vec<Line>,
    unread: u32,
}

struct ConnectForm {
    address: String,
    nickname: String,
    password: String,
    focus_requested: bool,
}

/// Connect-on-startup request from the command line.
#[derive(Default)]
pub struct Launch {
    pub address: String,
    pub nickname: Option<String>,
}

/// Something picked in the channel tree, applied after drawing it.
enum TreeAction {
    Create(Option<ChannelId>),
    Edit(ChannelId),
    Shift(ChannelId, i32),
    MakeDefault(ChannelId),
    Delete(ChannelId),
    AdminLogin,
}

/// Payload while dragging yourself onto another channel.
struct DragSelf;

/// Voice is considered blocked when no UDP pong arrived for this long.
const VOICE_TIMEOUT: Duration = Duration::from_secs(4);

struct Voice {
    shared: Arc<audio::Shared>,
    playback: Arc<audio::Playback>,
    sink: VoiceSink,
    /// Open while connected or while the voice settings are shown.
    engine: Option<audio::Engine>,
    /// Last UDP pong: proof that voice gets through.
    seen: Option<Instant>,
    rtt: Option<Duration>,
    connected_at: Option<Instant>,
}

impl Voice {
    fn reachable(&self) -> bool {
        self.seen.is_some_and(|t| t.elapsed() < VOICE_TIMEOUT)
    }

    /// Connected long enough that a missing pong means UDP is blocked.
    fn blocked(&self) -> bool {
        !self.reachable() && self.connected_at.is_some_and(|t| t.elapsed() > VOICE_TIMEOUT)
    }
}

struct PokeForm {
    target: ClientId,
    name: String,
    message: String,
    focus_requested: bool,
}

/// A poke someone sent us, waiting to be acknowledged.
struct PokeAlert {
    from: String,
    message: String,
    time: String,
}

/// The channel-related dialog on screen, if any (one at a time).
enum ChannelDialog {
    Form(ChannelForm),
    Password { channel: ChannelId, name: String, password: String, focused: bool },
    Delete { channel: ChannelId, name: String, occupants: usize },
    AdminLogin { token: String, focused: bool },
}

/// Creating (`editing: None`) or editing a channel.
struct ChannelForm {
    editing: Option<ChannelId>,
    parent: Option<ChannelId>,
    name: String,
    topic: String,
    limited: bool,
    limit: u32,
    /// Editing a channel that already has a password: keep protection on?
    had_password: bool,
    protected: bool,
    password: String,
    temporary: bool,
    focused: bool,
}

impl ChannelForm {
    fn settings(&self) -> ChannelSettings {
        let password = if self.had_password && !self.protected {
            PasswordChange::Remove
        } else if self.password.is_empty() {
            PasswordChange::Keep
        } else {
            PasswordChange::Set(self.password.clone())
        };
        ChannelSettings {
            name: self.name.trim().to_owned(),
            topic: self.topic.trim().to_owned(),
            max_clients: self.limited.then_some(self.limit),
            password,
        }
    }
}

struct AudioDialog {
    /// (label, folder) of every selectable sound pack except the built-in one.
    packs: Vec<(String, std::path::PathBuf)>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    /// Waiting for a key press to bind push-to-talk.
    binding: bool,
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
    /// Address of the connected server, for per-server admin tokens.
    server_address: String,
    /// Token sent but not yet confirmed; saved once the server accepts it.
    pending_admin_token: Option<String>,
    /// Ask the window manager to flash the taskbar entry.
    attention: bool,
    updater: crate::update::Updater,
    /// Set by "Restart to update": the shell closes the window after this frame.
    quit: bool,
}

impl App {
    pub fn new(ctx: &egui::Context, mut settings: Settings, launch: Option<Launch>) -> Self {
        theme::apply(ctx);
        if settings.nickname.is_empty() {
            settings.nickname = std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "User".into());
        }
        let playback = audio::Playback::new(ctx.clone());
        let net = Net::spawn(ctx.clone(), playback.clone());
        let voice = Voice {
            shared: audio::Shared::new(settings.audio.clone()),
            playback,
            sink: net.voice_sink(),
            engine: None,
            seen: None,
            rtt: None,
            connected_at: None,
        };
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
            pokes: Default::default(),
            sound_pack: Pack::BuiltIn,
            channel_dialog: None,
            server_address: String::new(),
            pending_admin_token: None,
            attention: false,
            updater: crate::update::Updater::start(ctx.clone()),
            quit: false,
        };
        app.sound_pack = Pack::open(app.settings.audio.sound_pack.as_deref());
        if let Some(l) = launch {
            let nick = l.nickname.unwrap_or_else(|| app.settings.nickname.clone());
            app.connect(l.address, nick, String::new());
        } else if app.settings.recent.is_empty() && DEFAULT_SERVER.is_some() {
            // First run of a build with a baked-in server: just ask for a nickname.
            app.open_connect_form();
        }
        app
    }

    // ----- actions ---------------------------------------------------------

    fn open_connect_form(&mut self) {
        let address = self
            .settings
            .recent
            .first()
            .map(|r| r.address.clone())
            .unwrap_or_else(|| DEFAULT_SERVER.unwrap_or("localhost").into());
        self.form = Some(ConnectForm {
            address,
            nickname: self.settings.nickname.clone(),
            password: String::new(),
            focus_requested: false,
        });
    }

    fn connect(&mut self, address: String, nickname: String, password: String) {
        let address = address.trim().to_owned();
        let nickname = nickname.trim().to_owned();
        if address.is_empty() || nickname.is_empty() {
            return;
        }
        self.settings.nickname = nickname.clone();
        let password = Some(password).filter(|p| !p.is_empty());
        self.session = self.net.connect(address.clone(), nickname, password);
        self.conn = Conn::Connecting { address };
        self.view = None;
        self.ping = None;
        self.last_error = None;
        self.form = None;
    }

    fn disconnect(&mut self) {
        if !matches!(self.conn, Conn::Disconnected) {
            self.net.disconnect();
        }
    }

    fn join_channel(&mut self, channel: ChannelId) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        if view.my_channel() == Some(channel) {
            return;
        }
        if ch.has_password && !self.am_admin() {
            let name = ch.name.clone();
            self.channel_dialog = Some(ChannelDialog::Password { channel, name, password: String::new(), focused: false });
        } else {
            self.net.send(C2S::JoinChannel { channel, password: None });
        }
    }

    fn am_admin(&self) -> bool {
        self.view.as_ref().and_then(|v| v.me()).is_some_and(|c| c.admin)
    }

    fn open_create_channel(&mut self, parent: Option<ChannelId>) {
        let parent_temporary =
            parent.and_then(|p| self.view.as_ref()?.channels.get(&p).map(|c| c.temporary)).unwrap_or(false);
        self.channel_dialog = Some(ChannelDialog::Form(ChannelForm {
            editing: None,
            parent,
            name: String::new(),
            topic: String::new(),
            limited: false,
            limit: 5,
            had_password: false,
            protected: false,
            password: String::new(),
            temporary: !self.am_admin() || parent_temporary,
            focused: false,
        }));
    }

    fn open_edit_channel(&mut self, channel: ChannelId) {
        let Some(ch) = self.view.as_ref().and_then(|v| v.channels.get(&channel)) else { return };
        self.channel_dialog = Some(ChannelDialog::Form(ChannelForm {
            editing: Some(channel),
            parent: ch.parent,
            name: ch.name.clone(),
            topic: ch.topic.clone(),
            limited: ch.max_clients.is_some(),
            limit: ch.max_clients.unwrap_or(5),
            had_password: ch.has_password,
            protected: ch.has_password,
            password: String::new(),
            temporary: ch.temporary,
            focused: false,
        }));
    }

    fn open_delete_channel(&mut self, channel: ChannelId) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        // Everyone in the channel or below it.
        let mut below = vec![channel];
        let mut i = 0;
        while i < below.len() {
            let parent = below[i];
            below.extend(view.channels.values().filter(|c| c.parent == Some(parent)).map(|c| c.id));
            i += 1;
        }
        let occupants = view.clients.values().filter(|c| below.contains(&c.channel)).count();
        self.channel_dialog = Some(ChannelDialog::Delete { channel, name: ch.name.clone(), occupants });
    }

    /// Moves a channel one place up (`-1`) or down (`+1`) among its siblings.
    fn shift_channel(&mut self, channel: ChannelId, delta: i32) {
        let Some(view) = &self.view else { return };
        let Some(ch) = view.channels.get(&channel) else { return };
        let mut siblings: Vec<_> = view.channels.values().filter(|c| c.parent == ch.parent).collect();
        siblings.sort_by_key(|c| (c.order, c.id));
        let Some(index) = siblings.iter().position(|c| c.id == channel) else { return };
        let Some(position) = index.checked_add_signed(delta as isize).filter(|&p| p < siblings.len()) else { return };
        self.net.send(C2S::MoveChannel { id: channel, parent: ch.parent, position: position as u32 });
    }

    fn authenticate(&mut self, token: String) {
        self.pending_admin_token = Some(token.clone());
        self.net.send(C2S::Authenticate { token });
    }

    fn set_status(&mut self, status: ClientStatus) {
        let mic_off = |s: ClientStatus| s.muted || s.deafened;
        let was_off = mic_off(self.settings.status);
        self.settings.status = status;
        if matches!(self.conn, Conn::Connected) {
            self.net.send(C2S::SetStatus(status));
            match (was_off, mic_off(status)) {
                (false, true) => self.sound(Sound::MicMuted),
                (true, false) => self.sound(Sound::MicUnmuted),
                _ => {}
            }
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

    fn open_poke(&mut self, target: ClientId) {
        let Some(name) = self.view.as_ref().and_then(|v| v.nickname(target)) else { return };
        self.poke_form = Some(PokeForm { target, name: name.to_owned(), message: String::new(), focus_requested: false });
    }

    fn on_poked(&mut self, from: ClientId, message: String) {
        let from = self.view.as_ref().and_then(|v| v.nickname(from)).unwrap_or("Someone").to_owned();
        let line = if message.is_empty() { format!("{from} poked you") } else { format!("{from} poked you: {message}") };
        self.push(0, LineKind::System, line);
        self.sound(Sound::Poke);
        let time = chrono::Local::now().format("%H:%M").to_string();
        self.pokes.push_back(PokeAlert { from, message, time });
        self.attention = true;
    }

    fn open_private(&mut self, id: ClientId) {
        let idx = self.private_tab(id);
        self.active_tab = idx;
        self.tabs[idx].unread = 0;
        self.focus_input = true;
    }

    fn send_chat(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() || !matches!(self.conn, Conn::Connected) {
            return;
        }
        let target = match self.tabs[self.active_tab].kind {
            TabKind::Server => TextTarget::Server,
            TabKind::Channel => TextTarget::Channel,
            TabKind::Private(id) => TextTarget::Client(id),
        };
        self.net.send(C2S::SendText { target, text });
        self.input.clear();
    }

    // ----- chat bookkeeping ------------------------------------------------

    fn push(&mut self, tab: usize, kind: LineKind, text: impl Into<String>) {
        let time = chrono::Local::now().format("%H:%M").to_string();
        let t = &mut self.tabs[tab];
        t.lines.push(Line { time, kind, text: text.into() });
        if t.lines.len() > MAX_LINES {
            t.lines.drain(..t.lines.len() - MAX_LINES);
        }
        if tab != self.active_tab {
            t.unread += 1;
        }
    }

    fn private_tab(&mut self, id: ClientId) -> usize {
        if let Some(i) = self.tabs.iter().position(|t| t.kind == TabKind::Private(id)) {
            return i;
        }
        let title = self
            .view
            .as_ref()
            .and_then(|v| v.nickname(id))
            .unwrap_or("unknown")
            .to_owned();
        self.tabs.push(Tab { kind: TabKind::Private(id), title, lines: Vec::new(), unread: 0 });
        self.tabs.len() - 1
    }

    fn close_tab(&mut self, idx: usize) {
        if matches!(self.tabs[idx].kind, TabKind::Private(_)) {
            self.tabs.remove(idx);
            if self.active_tab >= idx && self.active_tab > 0 {
                self.active_tab -= 1;
            }
        }
    }

    fn channel_name(&self, id: ChannelId) -> String {
        self.view
            .as_ref()
            .and_then(|v| v.channels.get(&id))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "?".into())
    }

    fn sync_channel_tab_title(&mut self) {
        if let Some(ch) = self.view.as_ref().and_then(|v| v.my_channel()) {
            self.tabs[1].title = self.channel_name(ch);
        }
    }

    // ----- network events --------------------------------------------------

    fn poll_network(&mut self) {
        while let Ok(ev) = self.net.rx.try_recv() {
            match ev {
                NetEvent::Message(s, msg) if s == self.session => self.on_message(msg),
                NetEvent::Ping(s, rtt) if s == self.session => self.ping = Some(rtt),
                NetEvent::VoicePing(s, rtt) if s == self.session => {
                    self.voice.rtt = Some(rtt);
                    self.voice.seen = Some(Instant::now());
                }
                NetEvent::Closed(s, reason) if s == self.session => self.on_closed(reason),
                _ => {} // stale session
            }
        }
    }

    fn on_closed(&mut self, reason: Option<String>) {
        let was_connected = matches!(self.conn, Conn::Connected);
        self.conn = Conn::Disconnected;
        self.view = None;
        self.ping = None;
        self.voice.connected_at = None;
        self.voice.seen = None;
        self.voice.rtt = None;
        match (&reason, was_connected) {
            (Some(r), true) => self.push(0, LineKind::Error, format!("Disconnected: {r}")),
            (None, true) => self.push(0, LineKind::System, "Disconnected"),
            _ => {}
        }
        if was_connected {
            self.sound(if reason.is_some() { Sound::ConnectionLost } else { Sound::Disconnected });
        }
        // A server newer than us: fetch the update right away.
        if reason.as_deref().is_some_and(|r| r.contains("please update your T7")) {
            self.updater.check_now();
        }
        if !was_connected {
            self.last_error = reason;
        }
        self.active_tab = 0;
    }

    fn on_message(&mut self, msg: S2C) {
        match msg {
            S2C::Welcome { your_id, server, channels, clients, .. } => {
                let address = match &self.conn {
                    Conn::Connecting { address } => address.clone(),
                    _ => String::new(),
                };
                self.server_address = address.clone();
                let recent = Recent { address, server_name: server.name.clone() };
                self.settings.recent.retain(|r| r.address != recent.address);
                self.settings.recent.insert(0, recent);
                self.settings.recent.truncate(MAX_RECENT);

                let name = server.name.clone();
                let welcome = server.welcome.clone();
                for c in &clients {
                    self.apply_user_volume(c.id, &c.nickname);
                }
                self.view = Some(ServerView::new(your_id, server, channels, clients));
                self.conn = Conn::Connected;
                self.voice.connected_at = Some(Instant::now());
                self.voice.seen = None;
                self.tabs = base_tabs();
                self.active_tab = 1;
                self.sync_channel_tab_title();
                self.push(0, LineKind::System, format!("Connected to {name}"));
                self.sound(Sound::Connected);
                if !welcome.is_empty() {
                    self.push(0, LineKind::System, welcome);
                }
                let ch = self.tabs[1].title.clone();
                self.push(1, LineKind::System, format!("You joined {ch}"));
                self.tabs[0].unread = 0;
                self.focus_input = true;
                if self.settings.status != ClientStatus::default() {
                    self.net.send(C2S::SetStatus(self.settings.status));
                }
                if let Some(token) = self.settings.admin_tokens.get(&self.server_address).cloned() {
                    self.authenticate(token);
                }
            }
            S2C::ClientJoined(info) => {
                self.apply_user_volume(info.id, &info.nickname);
                let line = format!("{} connected", info.nickname);
                let here = self.view.as_ref().and_then(|v| v.my_channel()) == Some(info.channel);
                self.apply(&S2C::ClientJoined(info.clone()));
                self.push(0, LineKind::System, line.clone());
                if here {
                    self.push(1, LineKind::System, format!("{} joined the channel", info.nickname));
                    self.sound(Sound::UserConnected);
                }
            }
            S2C::ClientLeft { id, reason } => {
                let Some(view) = &self.view else { return };
                let Some(c) = view.clients.get(&id) else { return };
                let name = c.nickname.clone();
                let here = Some(c.channel) == view.my_channel();
                let verb = match reason {
                    LeaveReason::Disconnected => "disconnected",
                    LeaveReason::TimedOut => "timed out",
                    LeaveReason::Kicked => "was kicked",
                };
                self.apply(&S2C::ClientLeft { id, reason });
                self.push(0, LineKind::System, format!("{name} {verb}"));
                if here {
                    self.push(1, LineKind::System, format!("{name} left the channel"));
                    self.sound(match reason {
                        LeaveReason::TimedOut => Sound::UserLostConnection,
                        LeaveReason::Disconnected | LeaveReason::Kicked => Sound::UserDisconnected,
                    });
                }
                if let Some(i) = self.tabs.iter().position(|t| t.kind == TabKind::Private(id)) {
                    self.push(i, LineKind::System, format!("{name} went offline"));
                }
            }
            S2C::ClientMoved { id, channel } => {
                let Some(view) = &self.view else { return };
                let Some(c) = view.clients.get(&id) else { return };
                let name = c.nickname.clone();
                let from = c.channel;
                let mine = view.my_channel();
                let is_me = id == view.my_id;
                self.apply(&S2C::ClientMoved { id, channel });
                if is_me {
                    self.sync_channel_tab_title();
                    let ch = self.channel_name(channel);
                    self.push(1, LineKind::System, format!("You joined {ch}"));
                    self.sound(Sound::ChannelSwitched);
                } else if Some(channel) == mine {
                    self.push(1, LineKind::System, format!("{name} joined the channel"));
                    self.sound(Sound::UserJoined);
                } else if Some(from) == mine {
                    self.push(1, LineKind::System, format!("{name} left the channel"));
                    self.sound(Sound::UserLeft);
                }
            }
            S2C::ClientStatus { .. } => self.apply(&msg),
            S2C::Channels(_) => {
                self.apply(&msg);
                self.sync_channel_tab_title();
            }
            S2C::ClientAdmin { id, admin } => {
                self.apply(&msg);
                let Some(view) = &self.view else { return };
                let name = view.nickname(id).unwrap_or("Someone").to_owned();
                if id == view.my_id {
                    if let Some(token) = self.pending_admin_token.take() {
                        self.settings.admin_tokens.insert(self.server_address.clone(), token);
                    }
                    self.push(0, LineKind::System, "You are a server admin now");
                } else if admin {
                    self.push(0, LineKind::System, format!("{name} is a server admin now"));
                }
            }
            S2C::Text { from, target, channel, text } => {
                let Some(view) = &self.view else { return };
                let name = view.nickname(from).unwrap_or("unknown").to_owned();
                let my_id = view.my_id;
                let tab = match target {
                    TextTarget::Server => 0,
                    TextTarget::Channel if Some(channel) == view.my_channel() => 1,
                    TextTarget::Channel => return,
                    TextTarget::Client(to) => self.private_tab(if from == my_id { to } else { from }),
                };
                self.push(tab, LineKind::Msg { from, name }, text);
                if from != my_id && matches!(target, TextTarget::Client(_)) {
                    self.sound(Sound::Message);
                }
            }
            S2C::Poked { from, message } => self.on_poked(from, message),
            S2C::Error { message } => {
                // A saved token that no longer works (e.g. the server's was reset) is forgotten.
                if message == "wrong admin token" && self.pending_admin_token.take().is_some() {
                    self.settings.admin_tokens.remove(&self.server_address);
                }
                let tab = self.active_tab;
                self.push(tab, LineKind::Error, message);
            }
            S2C::Rejected { .. } | S2C::Pong { .. } => {} // handled by the net layer
        }
    }

    fn apply(&mut self, msg: &S2C) {
        if let Some(v) = &mut self.view {
            v.apply(msg);
        }
    }

    // ----- UI --------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (connect, mute, deafen) = ctx.input_mut(|i| {
            use egui::{Key, KeyboardShortcut, Modifiers};
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::M)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::D)),
            )
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
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(Margin::symmetric(14, 10))
            .stroke(Stroke::new(1.0, theme::BORDER));
        egui::Panel::top("top").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                logo(ui, 26.0);
                ui.add_space(4.0);
                let name = self.view.as_ref().map(|v| v.info.name.as_str()).unwrap_or("T7");
                ui.label(RichText::new(name).size(16.0).strong().color(theme::TEXT));

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    match self.conn {
                        Conn::Connected => {
                            if ui.button("Disconnect").clicked() {
                                self.disconnect();
                            }
                        }
                        Conn::Connecting { .. } => {
                            if ui.button("Cancel").clicked() {
                                self.disconnect();
                            }
                        }
                        Conn::Disconnected => {
                            if ui.add(theme::primary_button("Connect")).on_hover_text("Ctrl+S").clicked() {
                                self.open_connect_form();
                            }
                        }
                    }
                    ui.add_space(6.0);
                    self.status_pill(ui);
                    match self.updater.status() {
                        crate::update::Status::Ready(version) => {
                            ui.add_space(6.0);
                            let label = format!("Restart to update to {version}");
                            if ui.add(theme::primary_button(&label)).on_hover_text("The update is downloaded and verified").clicked() {
                                self.restart_to_update();
                            }
                        }
                        crate::update::Status::Downloading(version) => {
                            ui.add_space(6.0);
                            ui.label(RichText::new(format!("Downloading {version}…")).size(12.5).color(theme::TEXT_FAINT));
                        }
                        _ => {}
                    }
                });
            });
        });
    }

    fn status_pill(&self, ui: &mut egui::Ui) {
        let (color, text) = match &self.conn {
            Conn::Connected if self.voice.blocked() => (theme::AMBER, "Connected · no voice".to_owned()),
            Conn::Connected => {
                let ping = match self.voice.rtt.or(self.ping) {
                    Some(p) if p < Duration::from_millis(1) => " · <1 ms".to_owned(),
                    Some(p) => format!(" · {} ms", p.as_millis()),
                    None => String::new(),
                };
                (theme::GREEN, format!("Connected{ping}"))
            }
            Conn::Connecting { address } => (theme::AMBER, format!("Connecting to {address}…")),
            Conn::Disconnected => (theme::TEXT_FAINT, "Not connected".to_owned()),
        };
        let pill = egui::Frame::new()
            .fill(theme::SURFACE)
            .corner_radius(CornerRadius::same(255))
            .inner_margin(Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, color);
                    ui.label(RichText::new(text).size(12.5).color(theme::TEXT_DIM));
                });
            });
        if self.voice.blocked() {
            pill.response.on_hover_text(
                "Voice packets aren't getting through. The server's UDP port (same number as \
                 the TCP port, 9988 by default) is probably blocked by a firewall.",
            );
        }
    }

    fn self_bar(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(Margin::symmetric(12, 8))
            .stroke(Stroke::new(1.0, theme::BORDER));
        egui::Panel::bottom("self").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let nick = self
                    .view
                    .as_ref()
                    .and_then(|v| v.me())
                    .map(|c| c.nickname.clone())
                    .unwrap_or_else(|| self.settings.nickname.clone());
                avatar(ui, &nick, 30.0, self.voice.shared.transmitting());
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new(&nick).strong().color(theme::TEXT));
                    let s = self.settings.status;
                    let (txt, col) = if !matches!(self.conn, Conn::Connected) {
                        ("Offline", theme::TEXT_FAINT)
                    } else if s.away {
                        ("Away", theme::AMBER)
                    } else if s.deafened {
                        ("Sound off", theme::RED)
                    } else if s.muted {
                        ("Microphone muted", theme::RED)
                    } else {
                        ("Online", theme::GREEN)
                    };
                    ui.label(RichText::new(txt).size(11.5).color(col));
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, Icon::Sliders, "Voice settings").clicked() {
                        let (inputs, outputs) = audio::device_names();
                        let custom = sounds_dir().map(|d| ("Custom folder".to_owned(), d));
                        let packs = custom.into_iter().chain(sounds::installed_packs()).collect();
                        self.audio_dialog = Some(AudioDialog { packs, inputs, outputs, binding: false });
                    }
                    ui.add_space(4.0);
                    let mut s = self.settings.status;
                    let changed = toggle(ui, &mut s.away, Icon::Moon, Icon::Moon, theme::AMBER, "Away")
                        | toggle(ui, &mut s.deafened, Icon::Headphones, Icon::HeadphonesOff, theme::RED, "Turn sound off (Ctrl+Shift+D)")
                        | toggle(ui, &mut s.muted, Icon::Mic, Icon::MicOff, theme::RED, "Mute microphone (Ctrl+Shift+M)");
                    if changed {
                        self.set_status(s);
                    }
                });
            });
        });
    }

    fn channel_tree(&mut self, ui: &mut egui::Ui) {
        let speaking = self.voice.playback.speaking();
        let me_talking = self.voice.shared.transmitting();
        let volumes = self.settings.user_volumes.clone();
        let mut volume_change: Option<(ClientId, String, f32)> = None;
        let frame = egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::symmetric(8, 10));
        egui::Panel::left("tree")
            .frame(frame)
            .resizable(true)
            .default_size(290.0)
            .size_range(220.0..=520.0)
            .show(ui, |ui| {
                let Some(view) = &self.view else { return };
                let mut join = None;
                let mut private = None;
                let mut poke = None;
                let mut action: Option<TreeAction> = None;
                let admin = view.me().is_some_and(|c| c.admin);
                // Mirrors the server's rule: admins, or the creator of a temporary channel.
                let may_manage = |ch: &t7_proto::ChannelInfo| admin || (ch.temporary && ch.creator == Some(view.my_id));

                ui.horizontal(|ui| {
                    ui.label(RichText::new("CHANNELS").size(11.0).color(theme::TEXT_FAINT).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 20.0), Sense::click());
                        let color = if resp.hovered() { theme::TEXT } else { theme::TEXT_FAINT };
                        if resp.hovered() {
                            ui.painter().rect_filled(rect, CornerRadius::same(5), theme::SURFACE_HOVER);
                        }
                        icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), Icon::Dots, color);
                        egui::Popup::menu(&resp).show(|ui| {
                            let label = if admin { "Create channel…" } else { "Create temporary channel…" };
                            if ui.button(label).clicked() {
                                action = Some(TreeAction::Create(None));
                            }
                            if admin {
                                ui.add_enabled(false, egui::Button::new("You're a server admin"));
                            } else if ui.button("Admin login…").clicked() {
                                action = Some(TreeAction::AdminLogin);
                            }
                        });
                    });
                });
                ui.add_space(4.0);

                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    let mine = view.my_channel();
                    let dragging_self = egui::DragAndDrop::has_payload_of_type::<DragSelf>(ui.ctx());

                    for &(depth, ch_id) in view.tree() {
                        let ch = &view.channels[&ch_id];
                        let clients = view.clients_in(ch_id);
                        let indent = 4.0 + depth as f32 * 16.0;
                        let is_mine = mine == Some(ch_id);
                        let full = ch.max_clients.is_some_and(|m| clients.len() as u32 >= m);

                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                        let drop_hover = dragging_self && resp.contains_pointer() && !is_mine;
                        let bg = if drop_hover {
                            theme::ACCENT_BG
                        } else if is_mine {
                            theme::SURFACE
                        } else if resp.hovered() {
                            theme::SURFACE_HOVER.gamma_multiply(0.7)
                        } else {
                            Color32::TRANSPARENT
                        };
                        let p = ui.painter();
                        p.rect_filled(rect, CornerRadius::same(6), bg);
                        if drop_hover {
                            p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                        }
                        let icon_color = if is_mine { theme::ACCENT_TEXT } else { theme::TEXT_FAINT };
                        let x = rect.left() + indent + 6.0;
                        icons::paint(p, egui::Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), vec2(15.0, 15.0)), Icon::Speaker, icon_color);
                        let name_color = if is_mine { theme::TEXT } else { theme::TEXT_DIM };
                        let name_rect = p.text(egui::pos2(x + 22.0, rect.center().y), Align2::LEFT_CENTER, &ch.name, FontId::proportional(14.0), name_color);
                        let mut after = name_rect.right() + 6.0;
                        if ch.has_password {
                            let r = egui::Rect::from_center_size(egui::pos2(after + 6.0, rect.center().y), vec2(12.0, 12.0));
                            icons::paint(p, r, Icon::Lock, theme::TEXT_FAINT);
                            after += 16.0;
                        }
                        if ch.temporary {
                            p.text(egui::pos2(after, rect.center().y + 0.5), Align2::LEFT_CENTER, "temporary", FontId::proportional(11.0), theme::TEXT_FAINT);
                        }

                        let count = match ch.max_clients {
                            Some(m) => format!("{}/{}", clients.len(), m),
                            None if !clients.is_empty() => clients.len().to_string(),
                            None => String::new(),
                        };
                        if !count.is_empty() {
                            let c = if full { theme::AMBER } else { theme::TEXT_FAINT };
                            p.text(rect.right_center() - vec2(8.0, 0.0), Align2::RIGHT_CENTER, count, FontId::proportional(11.5), c);
                        }

                        if resp.double_clicked() || resp.dnd_release_payload::<DragSelf>().is_some() {
                            join = Some(ch_id);
                        }
                        let mut tip: Vec<String> = Vec::new();
                        if !ch.topic.is_empty() {
                            tip.push(ch.topic.clone());
                        }
                        if ch.default {
                            tip.push("New people land here.".into());
                        }
                        if ch.temporary {
                            tip.push("Temporary: removed once everyone has left.".into());
                        }
                        let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip.join("\n")) };
                        resp.context_menu(|ui| {
                            if ui.add_enabled(!is_mine && !full, egui::Button::new("Join channel")).clicked() {
                                join = Some(ch_id);
                                ui.close();
                            }
                            ui.separator();
                            if depth + 1 < MAX_CHANNEL_DEPTH && ui.button("Create sub-channel…").clicked() {
                                action = Some(TreeAction::Create(Some(ch_id)));
                                ui.close();
                            }
                            if may_manage(ch) && ui.button("Edit channel…").clicked() {
                                action = Some(TreeAction::Edit(ch_id));
                                ui.close();
                            }
                            if admin {
                                if ui.button("Move up").clicked() {
                                    action = Some(TreeAction::Shift(ch_id, -1));
                                }
                                if ui.button("Move down").clicked() {
                                    action = Some(TreeAction::Shift(ch_id, 1));
                                }
                                if !ch.default && !ch.temporary && ui.button("Make default channel").clicked() {
                                    action = Some(TreeAction::MakeDefault(ch_id));
                                    ui.close();
                                }
                            }
                            if may_manage(ch) && !ch.default && ui.button("Delete channel…").clicked() {
                                action = Some(TreeAction::Delete(ch_id));
                                ui.close();
                            }
                        });

                        for c in clients {
                            let is_me = c.id == view.my_id;
                            let sense = if is_me { Sense::click_and_drag() } else { Sense::click() };
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), sense);
                            if resp.hovered() {
                                ui.painter().rect_filled(rect, CornerRadius::same(6), theme::SURFACE_HOVER.gamma_multiply(0.7));
                            }
                            let p = ui.painter();
                            let ax = rect.left() + indent + 34.0;
                            let center = egui::pos2(ax, rect.center().y);
                            paint_avatar(p, center, 10.0, &c.nickname);
                            if speaking.contains(&c.id) || (is_me && me_talking) {
                                p.circle_stroke(center, 12.5, Stroke::new(2.0, theme::GREEN));
                            }
                            let color = if c.status.away { theme::TEXT_FAINT } else { theme::TEXT };
                            let name_rect = p.text(egui::pos2(ax + 18.0, rect.center().y), Align2::LEFT_CENTER, &c.nickname, FontId::proportional(13.5), color);
                            let mut name_end = name_rect.right();
                            if c.admin {
                                let r = egui::Rect::from_center_size(egui::pos2(name_end + 11.0, rect.center().y), vec2(12.0, 12.0));
                                icons::paint(p, r, Icon::Shield, theme::ACCENT_TEXT);
                                name_end += 16.0;
                            }
                            let name_rect = egui::Rect::from_min_max(name_rect.min, egui::pos2(name_end, name_rect.max.y));
                            let volume = volumes.get(&c.nickname).copied().unwrap_or(1.0);
                            let hint = if is_me {
                                Some("you".to_owned())
                            } else if volume == 0.0 {
                                Some("muted for you".to_owned())
                            } else if volume != 1.0 {
                                Some(format!("{:.0}%", volume * 100.0))
                            } else {
                                None
                            };
                            if let Some(hint) = hint {
                                p.text(name_rect.right_center() + vec2(6.0, 0.5), Align2::LEFT_CENTER, hint, FontId::proportional(11.0), theme::TEXT_FAINT);
                            }
                            let mut x = rect.right() - 16.0;
                            for (on, icon, col) in [
                                (c.status.away, Icon::Moon, theme::AMBER),
                                (c.status.deafened, Icon::HeadphonesOff, theme::RED),
                                (c.status.muted, Icon::MicOff, theme::RED),
                            ] {
                                if on {
                                    let r = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), vec2(14.0, 14.0));
                                    icons::paint(p, r, icon, col);
                                    x -= 20.0;
                                }
                            }

                            if is_me {
                                if resp.drag_started() {
                                    resp.dnd_set_drag_payload(DragSelf);
                                }
                                if resp.dragged() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                                }
                                resp.on_hover_text("Drag onto a channel to move");
                            } else {
                                if resp.double_clicked() {
                                    private = Some(c.id);
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Send private message").clicked() {
                                        private = Some(c.id);
                                        ui.close();
                                    }
                                    if ui.button("Poke…").clicked() {
                                        poke = Some(c.id);
                                        ui.close();
                                    }
                                    ui.separator();
                                    ui.label(RichText::new("Volume").size(12.0).color(theme::TEXT_DIM));
                                    let mut v = volume;
                                    ui.horizontal(|ui| {
                                        let slider = egui::Slider::new(&mut v, 0.0..=2.0)
                                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                            .custom_parser(|s| s.trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0));
                                        ui.add(slider);
                                        if ui.add_enabled(v != 1.0, egui::Button::new("Reset").small()).clicked() {
                                            v = 1.0;
                                        }
                                    });
                                    if v != volume {
                                        volume_change = Some((c.id, c.nickname.clone(), v));
                                    }
                                });
                            }
                        }
                    }
                });

                if let Some(ch) = join {
                    self.join_channel(ch);
                }
                if let Some(id) = private {
                    self.open_private(id);
                }
                if let Some(id) = poke {
                    self.open_poke(id);
                }
                if let Some((id, nickname, v)) = volume_change.take() {
                    self.set_user_volume(id, &nickname, v);
                }
                match action {
                    Some(TreeAction::Create(parent)) => self.open_create_channel(parent),
                    Some(TreeAction::Edit(ch)) => self.open_edit_channel(ch),
                    Some(TreeAction::Shift(ch, delta)) => self.shift_channel(ch, delta),
                    Some(TreeAction::MakeDefault(ch)) => self.net.send(C2S::SetDefaultChannel { id: ch }),
                    Some(TreeAction::Delete(ch)) => self.open_delete_channel(ch),
                    Some(TreeAction::AdminLogin) => {
                        self.channel_dialog = Some(ChannelDialog::AdminLogin { token: String::new(), focused: false });
                    }
                    None => {}
                }
            });
    }

    fn chat(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(16, 12));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if self.view.is_none() {
                self.empty_state(ui);
                return;
            }

            // Tabs
            let mut close = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                for i in 0..self.tabs.len() {
                    let tab = &self.tabs[i];
                    let label = match tab.kind {
                        TabKind::Server => "Server".to_owned(),
                        TabKind::Channel => format!("# {}", tab.title),
                        TabKind::Private(_) => format!("@ {}", tab.title),
                    };
                    let active = i == self.active_tab;
                    let mut text = RichText::new(label).size(13.5);
                    text = if active { text.color(theme::TEXT) } else { text.color(theme::TEXT_DIM) };
                    let btn = egui::Button::new(text)
                        .fill(if active { theme::SURFACE } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE)
                        .corner_radius(theme::RADIUS);
                    let resp = ui.add(btn);
                    if tab.unread > 0 {
                        badge(ui, resp.rect, tab.unread);
                    }
                    if resp.clicked() {
                        self.active_tab = i;
                        self.focus_input = true;
                    }
                    if matches!(tab.kind, TabKind::Private(_)) {
                        if resp.middle_clicked() {
                            close = Some(i);
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Close").clicked() {
                                close = Some(i);
                                ui.close();
                            }
                        });
                    }
                }
            });
            if let Some(i) = close {
                self.close_tab(i);
            }
            self.tabs[self.active_tab].unread = 0;

            ui.add_space(4.0);
            ui.separator();

            // Composer pinned to the bottom, messages fill the rest.
            let kind = self.tabs[self.active_tab].kind;
            let hint = match kind {
                TabKind::Server => "Message everyone on the server".to_owned(),
                TabKind::Channel => format!("Message #{}", self.tabs[1].title),
                TabKind::Private(_) => format!("Message @{}", self.tabs[self.active_tab].title),
            };
            egui::Panel::bottom("composer")
                .frame(egui::Frame::new().inner_margin(Margin { top: 8, ..Default::default() }))
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .corner_radius(CornerRadius::same(10))
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .inner_margin(Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            let edit = egui::TextEdit::singleline(&mut self.input)
                                .hint_text(RichText::new(hint).color(theme::TEXT_FAINT))
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .char_limit(t7_proto::MAX_TEXT_LEN)
                                .font(FontId::proportional(14.5));
                            let resp = ui.add(edit);
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                self.send_chat();
                                self.focus_input = true;
                            }
                            if std::mem::take(&mut self.focus_input) {
                                resp.request_focus();
                            }
                        });
                });

            let view = self.view.as_ref().expect("checked above");
            let tab = &self.tabs[self.active_tab];
            egui::ScrollArea::vertical()
                .id_salt(kind)
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    for line in &tab.lines {
                        ui.label(layout_line(line, view.my_id, ui.available_width()));
                    }
                });
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let mut connect_to = None;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(20.0));
            logo(ui, 64.0);
            ui.add_space(14.0);
            match &self.conn {
                Conn::Connecting { address } => {
                    ui.label(RichText::new("Connecting…").size(22.0).strong().color(theme::TEXT));
                    ui.add_space(6.0);
                    ui.label(RichText::new(address).color(theme::TEXT_DIM));
                    ui.add_space(14.0);
                    ui.add(egui::Spinner::new().size(22.0).color(theme::ACCENT_TEXT));
                }
                _ => {
                    ui.label(RichText::new("Not connected").size(22.0).strong().color(theme::TEXT));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Connect to a T7 server to start talking.").color(theme::TEXT_DIM));
                    if let Some(err) = &self.last_error {
                        ui.add_space(10.0);
                        ui.label(RichText::new(err).color(theme::RED));
                    }
                    ui.add_space(16.0);
                    if ui.add(theme::primary_button("Connect to server").min_size(vec2(180.0, 36.0))).clicked() {
                        self.open_connect_form();
                    }

                    if !self.settings.recent.is_empty() {
                        ui.add_space(28.0);
                        ui.label(RichText::new("RECENT").size(11.0).strong().color(theme::TEXT_FAINT));
                        ui.add_space(6.0);
                        for r in &self.settings.recent {
                            if recent_card(ui, r).clicked() {
                                connect_to = Some(r.address.clone());
                            }
                        }
                    }
                }
            }
        });
        if let Some(address) = connect_to {
            let nick = self.settings.nickname.clone();
            self.connect(address, nick, String::new());
        }
    }

    fn connect_dialog(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.form else { return };
        let mut submit = false;
        let mut cancel = false;
        let modal = egui::Modal::new(Id::unique("connect")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new("Connect to server").size(19.0).strong().color(theme::TEXT));
            ui.add_space(2.0);
            ui.label(RichText::new("Enter an address like example.com or 1.2.3.4:9988").size(12.5).color(theme::TEXT_DIM));
            ui.add_space(14.0);

            let address = field(ui, "Server address", &mut form.address, false);
            let nickname = field(ui, "Nickname", &mut form.nickname, false);
            if !form.focus_requested {
                // Start where typing is needed: the address, or the nickname if it's prefilled.
                if form.address.trim().is_empty() { address } else { nickname }.request_focus();
                form.focus_requested = true;
            }
            field(ui, "Password (optional)", &mut form.password, true);

            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ready = !form.address.trim().is_empty() && !form.nickname.trim().is_empty();
                    if ui.add_enabled(ready, theme::primary_button("Connect")).clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        });
        if modal.should_close() {
            cancel = true;
        }
        if submit {
            let f = self.form.take().expect("form open");
            self.connect(f.address, f.nickname, f.password);
        } else if cancel {
            self.form = None;
        }
    }
}

impl App {
    /// Keeps the audio engine and its shared state in line with the app state.
    fn drive_audio(&mut self, ctx: &egui::Context) {
        let s = self.settings.status;
        let v = &mut self.voice;
        v.shared.set_status(s.muted, s.deafened);
        let connected = matches!(self.conn, Conn::Connected);
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
        if let (TxMode::PushToTalk, Some(key), false) = (self.settings.audio.mode, self.settings.audio.ptt_key, binding) {
            v.shared.set_ptt_local(ctx.input(|i| ptt::is_down(key.vk, i)));
        }
        // Talking rings fade out without any input event, so poll while anyone talks.
        if v.shared.transmitting() || !v.playback.speaking().is_empty() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if matches!(self.conn, Conn::Connected) && !v.reachable() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(err) = v.shared.take_error() {
            let tab = self.active_tab;
            self.push(tab, LineKind::Error, err);
        }
    }

    fn audio_dialog(&mut self, ctx: &egui::Context) {
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

        let modal = egui::Modal::new(Id::unique("voice-settings")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.spacing_mut().slider_width = 250.0;
            ui.label(RichText::new("Voice settings").size(19.0).strong().color(theme::TEXT));

            section(ui, "Microphone");
            restart |= device_combo(ui, "mic", &mut a.input_device, &dlg.inputs);
            section(ui, "Speakers");
            restart |= device_combo(ui, "speakers", &mut a.output_device, &dlg.outputs);

            section(ui, "Transmit");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut a.mode, TxMode::VoiceActivation, "Voice activation");
                ui.selectable_value(&mut a.mode, TxMode::PushToTalk, "Push to talk");
            });
            ui.add_space(6.0);
            match a.mode {
                TxMode::VoiceActivation => {
                    level_meter(ui, level, a.threshold_db, open);
                    ui.add(
                        egui::Slider::new(&mut a.threshold_db, METER_MIN_DB..=METER_MAX_DB)
                            .text("activation level")
                            .suffix(" dB")
                            .fixed_decimals(0),
                    );
                    ui.label(
                        RichText::new("Talk normally: the bar should cross the line, and stay left of it when you're quiet.")
                            .size(11.5)
                            .color(theme::TEXT_FAINT),
                    );
                }
                TxMode::PushToTalk => {
                    ui.horizontal(|ui| {
                        let text = match (dlg.binding, a.ptt_key) {
                            (true, _) => "Press a key or mouse button…".to_owned(),
                            (false, Some(k)) => ptt::label(k.vk),
                            (false, None) => "Click to set a key".to_owned(),
                        };
                        if ui.add(egui::Button::new(text).min_size(vec2(220.0, 30.0))).clicked() {
                            dlg.binding = true;
                        }
                        if open {
                            ui.label(RichText::new("Transmitting").color(theme::GREEN));
                        }
                    });
                    let note = if cfg!(windows) {
                        "Works while other apps, like games, are focused."
                    } else {
                        "Works while T7 is focused."
                    };
                    ui.label(RichText::new(note).size(11.5).color(theme::TEXT_FAINT));
                }
            }

            section(ui, "Sound cleanup");
            ui.checkbox(&mut a.cleanup.noise_suppression, "Noise suppression")
                .on_hover_text("Removes fans, hum, traffic and other background noise");
            ui.checkbox(&mut a.cleanup.echo_cancellation, "Echo cancellation")
                .on_hover_text("Stops others from hearing themselves when you use speakers instead of headphones");
            ui.checkbox(&mut a.cleanup.auto_gain, "Automatic volume")
                .on_hover_text("Brings a quiet microphone up to a normal level");

            section(ui, "Notifications");
            ui.checkbox(&mut a.sounds, "Notification sounds")
                .on_hover_text("Chimes for connecting, people joining and leaving, pokes and private messages");
            ui.add_enabled(
                a.sounds,
                egui::Slider::new(&mut a.sounds_volume, 0.0..=1.0)
                    .text("sound volume")
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .custom_parser(|s| s.trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)),
            );
            ui.horizontal(|ui| {
                let shown = match &a.sound_pack {
                    None => "Built-in".to_owned(),
                    Some(dir) => dlg
                        .packs
                        .iter()
                        .find(|(_, d)| d == dir)
                        .map(|(label, _)| label.clone())
                        .unwrap_or_else(|| dir.display().to_string()),
                };
                egui::ComboBox::from_id_salt("sound-pack").width(250.0).selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut a.sound_pack, None, "Built-in");
                    for (label, dir) in &dlg.packs {
                        ui.selectable_value(&mut a.sound_pack, Some(dir.clone()), label);
                    }
                });
                if ui.small_button("Test").clicked() {
                    test_sound = true;
                }
            });
            if a.sound_pack.is_some() && a.sound_pack == sounds_dir() {
                let names: Vec<&str> = Sound::ALL.iter().map(|s| s.file_name()).collect();
                if ui
                    .small_button("Open custom folder…")
                    .on_hover_text(format!("WAV files with these names replace the built-in sounds: {}", names.join(", ")))
                    .clicked()
                {
                    open_sounds_folder();
                }
            }

            section(ui, "Volume");
            ui.add(egui::Slider::new(&mut a.input_gain_db, -20.0..=20.0).text("microphone").suffix(" dB").fixed_decimals(0));
            ui.add(
                egui::Slider::new(&mut a.output_volume, 0.0..=2.0)
                    .text("speakers")
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .custom_parser(|s| s.trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)),
            );

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("T7 {}", crate::update::VERSION)).size(11.5).color(theme::TEXT_FAINT));
                if self.updater.enabled() {
                    let state = match self.updater.status() {
                        crate::update::Status::Idle | crate::update::Status::UpToDate => "up to date".to_owned(),
                        crate::update::Status::Checking => "checking for updates…".to_owned(),
                        crate::update::Status::Downloading(v) => format!("downloading {v}…"),
                        crate::update::Status::Ready(v) => format!("{v} ready, restart to update"),
                        crate::update::Status::Failed(e) => e,
                    };
                    ui.label(RichText::new(format!("· {state}")).size(11.5).color(theme::TEXT_FAINT));
                    if ui.small_button("Check now").clicked() {
                        self.updater.check_now();
                    }
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::primary_button("Done")).clicked() {
                    close = true;
                }
            });
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

impl App {
    fn poke_dialog(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.poke_form else { return };
        let (mut send, mut cancel) = (false, false);
        let modal = egui::Modal::new(Id::unique("poke")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new(format!("Poke {}", form.name)).size(19.0).strong().color(theme::TEXT));
            ui.add_space(2.0);
            ui.label(RichText::new("They get a popup and a sound.").size(12.5).color(theme::TEXT_DIM));
            ui.add_space(12.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut form.message)
                    .hint_text("Message (optional)")
                    .char_limit(t7_proto::MAX_POKE_LEN)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 7)),
            );
            if !form.focus_requested {
                edit.request_focus();
                form.focus_requested = true;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                send = true;
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(theme::primary_button("Poke")).clicked() {
                    send = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if send {
            let form = self.poke_form.take().expect("open");
            self.net.send(C2S::Poke { target: form.target, message: form.message.trim().to_owned() });
            self.push(0, LineKind::System, format!("You poked {}", form.name));
        } else if cancel || modal.should_close() {
            self.poke_form = None;
        }
    }

    fn poke_alert(&mut self, ctx: &egui::Context) {
        let Some(alert) = self.pokes.front() else { return };
        let more = self.pokes.len() - 1;
        let mut ok = false;
        let modal = egui::Modal::new(Id::unique("poked")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new(format!("{} poked you", alert.from)).size(19.0).strong().color(theme::TEXT));
            ui.label(RichText::new(&alert.time).size(12.0).color(theme::TEXT_FAINT));
            if !alert.message.is_empty() {
                ui.add_space(10.0);
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&alert.message).size(14.5).color(theme::TEXT));
                    });
            }
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if more > 0 { format!("OK ({more} more)") } else { "OK".to_owned() };
                if ui.add(theme::primary_button(&label)).clicked() {
                    ok = true;
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                ok = true;
            }
        });
        if ok || modal.should_close() {
            self.pokes.pop_front();
        }
    }
}

impl App {
    fn channel_dialog(&mut self, ctx: &egui::Context) {
        // A dialog about a channel that just disappeared (deleted, or an emptied
        // temporary channel) closes by itself.
        let about = match &self.channel_dialog {
            Some(ChannelDialog::Form(f)) => f.editing.or(f.parent),
            Some(ChannelDialog::Password { channel, .. } | ChannelDialog::Delete { channel, .. }) => Some(*channel),
            _ => None,
        };
        let gone = about.is_some_and(|id| !self.view.as_ref().is_some_and(|v| v.channels.contains_key(&id)));
        if gone {
            self.channel_dialog = None;
        }
        let Some(dialog) = &mut self.channel_dialog else { return };
        let admin = self.view.as_ref().and_then(|v| v.me()).is_some_and(|c| c.admin);
        let channel_name = |id: Option<ChannelId>| {
            id.and_then(|id| self.view.as_ref()?.channels.get(&id).map(|c| c.name.clone()))
        };
        let parent_name = match dialog {
            ChannelDialog::Form(f) if f.editing.is_none() => channel_name(f.parent),
            _ => None,
        };
        let default_name = self
            .view
            .as_ref()
            .and_then(|v| v.channels.values().find(|c| c.default).map(|c| c.name.clone()))
            .unwrap_or_else(|| "the default channel".into());

        let mut send: Option<C2S> = None;
        let mut login: Option<String> = None;
        let mut close = false;
        let modal = egui::Modal::new(Id::unique("channel-dialog")).show(ctx, |ui| {
            ui.set_width(380.0);
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            match dialog {
                ChannelDialog::Form(form) => {
                    let title = match (&form.editing, &parent_name) {
                        (Some(_), _) => format!("Edit {}", form.name),
                        (None, Some(parent)) => format!("New channel in {parent}"),
                        (None, None) => "New channel".to_owned(),
                    };
                    ui.label(RichText::new(title).size(19.0).strong().color(theme::TEXT));
                    ui.add_space(12.0);
                    ui.label(RichText::new("Name").size(12.0).color(theme::TEXT_DIM));
                    let name = ui.add(
                        egui::TextEdit::singleline(&mut form.name)
                            .char_limit(MAX_CHANNEL_NAME_LEN)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 7)),
                    );
                    if !form.focused {
                        name.request_focus();
                        form.focused = true;
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Topic (optional)").size(12.0).color(theme::TEXT_DIM));
                    ui.add(
                        egui::TextEdit::singleline(&mut form.topic)
                            .char_limit(MAX_CHANNEL_TOPIC_LEN)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 7)),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut form.limited, "Limit to");
                        ui.add_enabled(form.limited, egui::DragValue::new(&mut form.limit).range(1..=1000).suffix(" people"));
                    });
                    if form.had_password {
                        ui.checkbox(&mut form.protected, "Password protected");
                    }
                    if !form.had_password || form.protected {
                        let label = if form.had_password { "New password (empty keeps the current one)" } else { "Password (optional)" };
                        ui.add_space(4.0);
                        ui.label(RichText::new(label).size(12.0).color(theme::TEXT_DIM));
                        ui.add(
                            egui::TextEdit::singleline(&mut form.password)
                                .password(true)
                                .desired_width(f32::INFINITY)
                                .margin(Margin::symmetric(10, 7)),
                        );
                    }
                    if form.editing.is_none() {
                        ui.add_space(6.0);
                        if admin {
                            ui.checkbox(&mut form.temporary, "Temporary: removed once everyone has left");
                        } else {
                            ui.label(
                                RichText::new("Temporary: removed once everyone has left. You'll move into it.")
                                    .size(12.0)
                                    .color(theme::TEXT_FAINT),
                            );
                        }
                    }
                    ui.add_space(14.0);
                    let ready = !form.name.trim().is_empty();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = if form.editing.is_some() { "Save" } else { "Create" };
                        if ui.add_enabled(ready, theme::primary_button(label)).clicked() || (ready && enter) {
                            let settings = form.settings();
                            send = Some(match form.editing {
                                Some(id) => C2S::EditChannel { id, settings },
                                None => C2S::CreateChannel { parent: form.parent, settings, temporary: form.temporary },
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                ChannelDialog::Password { channel, name, password, focused } => {
                    ui.label(RichText::new(format!("Join {name}")).size(19.0).strong().color(theme::TEXT));
                    ui.add_space(2.0);
                    ui.label(RichText::new("This channel has a password.").size(12.5).color(theme::TEXT_DIM));
                    ui.add_space(12.0);
                    let field = ui.add(
                        egui::TextEdit::singleline(password)
                            .password(true)
                            .hint_text("Password")
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 7)),
                    );
                    if !*focused {
                        field.request_focus();
                        *focused = true;
                    }
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(theme::primary_button("Join")).clicked() || enter {
                            send = Some(C2S::JoinChannel { channel: *channel, password: Some(password.clone()) });
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                ChannelDialog::Delete { channel, name, occupants } => {
                    ui.label(RichText::new(format!("Delete {name}?")).size(19.0).strong().color(theme::TEXT));
                    ui.add_space(6.0);
                    let mut text = "Its sub-channels are deleted too.".to_owned();
                    if *occupants > 0 {
                        let who = if *occupants == 1 { "The 1 person".to_owned() } else { format!("The {occupants} people") };
                        text.push_str(&format!(" {who} inside will move to {default_name}."));
                    }
                    ui.label(RichText::new(text).size(13.0).color(theme::TEXT_DIM));
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let delete = egui::Button::new(RichText::new("Delete").color(Color32::WHITE).strong())
                            .fill(theme::RED)
                            .stroke(Stroke::NONE)
                            .corner_radius(theme::RADIUS);
                        if ui.add(delete).clicked() {
                            send = Some(C2S::DeleteChannel { id: *channel });
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                ChannelDialog::AdminLogin { token, focused } => {
                    ui.label(RichText::new("Admin login").size(19.0).strong().color(theme::TEXT));
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("Admins can create, edit, rearrange and delete any channel. The token is in the server's log and in admin_token.txt next to its config.")
                            .size(12.5)
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(12.0);
                    let field = ui.add(
                        egui::TextEdit::singleline(token)
                            .password(true)
                            .hint_text("Admin token")
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 7)),
                    );
                    if !*focused {
                        field.request_focus();
                        *focused = true;
                    }
                    ui.add_space(14.0);
                    let ready = !token.trim().is_empty();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(ready, theme::primary_button("Log in")).clicked() || (ready && enter) {
                            login = Some(token.trim().to_owned());
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if let Some(msg) = send {
            self.net.send(msg);
            close = true;
        }
        if let Some(token) = login {
            self.authenticate(token);
            close = true;
        }
        if close || modal.should_close() {
            self.channel_dialog = None;
        }
    }
}

fn sounds_dir() -> Option<std::path::PathBuf> {
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

/// What the window shell needs from the app.
impl App {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll_network();
        let ctx = ui.ctx().clone();
        self.drive_audio(&ctx);
        self.shortcuts(&ctx);
        self.top_bar(ui);
        self.self_bar(ui);
        if self.view.is_some() {
            self.channel_tree(ui);
        }
        self.chat(ui);
        self.connect_dialog(&ctx);
        self.audio_dialog(&ctx);
        self.poke_dialog(&ctx);
        self.poke_alert(&ctx);
        self.channel_dialog(&ctx);
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

    fn restart_to_update(&mut self) {
        let Some(exe) = self.updater.install() else { return };
        // Same arguments, so a command-line connection reconnects.
        match std::process::Command::new(&exe).args(std::env::args().skip(1)).spawn() {
            Ok(_) => self.quit = true,
            Err(e) => self.push(0, LineKind::Error, format!("Updated, but couldn't restart T7: {e}. Start it again.")),
        }
    }
}

fn base_tabs() -> Vec<Tab> {
    vec![
        Tab { kind: TabKind::Server, title: "Server".into(), lines: Vec::new(), unread: 0 },
        Tab { kind: TabKind::Channel, title: "Channel".into(), lines: Vec::new(), unread: 0 },
    ]
}

/// One chat line as a single wrapped text layout: cheap to lay out and render.
fn layout_line(line: &Line, my_id: ClientId, width: f32) -> LayoutJob {
    use egui::text::TextFormat;
    let fmt = |size: f32, color: Color32| TextFormat { font_id: FontId::proportional(size), color, ..Default::default() };
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    job.append(&line.time, 0.0, fmt(11.5, theme::TEXT_FAINT));
    match &line.kind {
        LineKind::System => job.append(&line.text, 10.0, TextFormat { italics: true, ..fmt(13.5, theme::TEXT_DIM) }),
        LineKind::Error => job.append(&line.text, 10.0, fmt(13.5, theme::RED)),
        LineKind::Msg { from, name } => {
            let color = if *from == my_id { theme::ACCENT_TEXT } else { theme::avatar_color(name) };
            job.append(name, 10.0, fmt(14.0, color));
            job.append(&line.text, 8.0, fmt(14.0, theme::TEXT));
        }
    }
    job
}

fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same((size * 0.22) as u8), theme::ACCENT);
    p.text(rect.center(), Align2::CENTER_CENTER, "T7", FontId::proportional(size * 0.42), theme::ON_ACCENT);
}

fn paint_avatar(p: &egui::Painter, center: egui::Pos2, radius: f32, name: &str) {
    p.circle_filled(center, radius, theme::avatar_color(name));
    let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    p.text(center, Align2::CENTER_CENTER, initial, FontId::proportional(radius * 1.1), Color32::WHITE);
}

fn avatar(ui: &mut egui::Ui, name: &str, size: f32, talking: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_avatar(ui.painter(), rect.center(), size / 2.0, name);
    if talking {
        ui.painter().circle_stroke(rect.center(), size / 2.0 + 2.5, Stroke::new(2.0, theme::GREEN));
    }
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(38.0, 34.0), Sense::click());
    let (fill, color) = if resp.hovered() { (theme::SURFACE_HOVER, theme::TEXT) } else { (theme::SURFACE, theme::TEXT_DIM) };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, color);
    resp.on_hover_text(tip)
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
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
    let t = |db: f32| ((db - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)).clamp(0.0, 1.0);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(5), theme::SURFACE_ACTIVE);
    let mut fill = rect;
    fill.set_width(rect.width() * t(level_db));
    p.rect_filled(fill, CornerRadius::same(5), if open { theme::GREEN } else { theme::TEXT_FAINT });
    let x = rect.left() + rect.width() * t(threshold_db);
    p.line_segment([egui::pos2(x, rect.top() - 3.0), egui::pos2(x, rect.bottom() + 3.0)], Stroke::new(2.0, theme::ACCENT_TEXT));
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).size(12.0).color(theme::TEXT_DIM));
}

fn badge(ui: &egui::Ui, anchor: egui::Rect, count: u32) {
    let text = if count > 99 { "99+".to_owned() } else { count.to_string() };
    let center = anchor.right_top() + vec2(2.0, 3.0);
    let w = (text.len() as f32 * 6.0 + 8.0).max(16.0);
    let r = egui::Rect::from_center_size(center, vec2(w, 16.0));
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(8), theme::ACCENT);
    p.text(r.center(), Align2::CENTER_CENTER, text, FontId::proportional(10.5), theme::ON_ACCENT);
}

/// An icon toggle button; red when active (muted, deafened...).
fn toggle(ui: &mut egui::Ui, on: &mut bool, icon: Icon, icon_on: Icon, on_color: Color32, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(38.0, 34.0), Sense::click());
    let (fill, color, icon) = match (*on, resp.hovered()) {
        (true, _) => (on_color.gamma_multiply(0.18), on_color, icon_on),
        (false, true) => (theme::SURFACE_HOVER, theme::TEXT, icon),
        (false, false) => (theme::SURFACE, theme::TEXT_DIM, icon),
    };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, color);
    let clicked = resp.on_hover_text(tip).clicked();
    if clicked {
        *on = !*on;
    }
    clicked
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool) -> egui::Response {
    ui.label(RichText::new(label).size(12.0).color(theme::TEXT_DIM));
    let resp = ui.add(
        egui::TextEdit::singleline(value)
            .password(password)
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(10, 7)),
    );
    ui.add_space(6.0);
    resp
}

fn recent_card(ui: &mut egui::Ui, r: &Recent) -> egui::Response {
    let width = 320.0_f32.min(ui.available_width());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 48.0), Sense::click());
    let p = ui.painter();
    let fill = if resp.hovered() { theme::SURFACE_HOVER } else { theme::SURFACE };
    p.rect(rect, CornerRadius::same(10), fill, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
    let x = rect.left() + 14.0;
    p.text(egui::pos2(x, rect.top() + 16.0), Align2::LEFT_CENTER, &r.server_name, FontId::proportional(14.0), theme::TEXT);
    p.text(egui::pos2(x, rect.top() + 33.0), Align2::LEFT_CENTER, &r.address, FontId::proportional(12.0), theme::TEXT_FAINT);
    let chevron = egui::Rect::from_center_size(rect.right_center() - vec2(20.0, 0.0), vec2(16.0, 16.0));
    icons::paint(p, chevron, Icon::ChevronRight, if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
