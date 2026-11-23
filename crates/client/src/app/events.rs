//! What the network thread reports, applied to the app state and the chat.

use t7_proto::model::ServerView;
use t7_proto::{C2S, ClientStatus, LeaveReason, S2C, Speech, TextTarget};

use super::chat::{LineKind, TabKind, base_tabs};
use super::connect::{CertChange, MAX_RECENT};
use super::{App, Conn};
use crate::net::{Ended, NetEvent, ServerKey};
use crate::settings::Recent;
use crate::sounds::Sound;

impl App {
    pub(super) fn poll_network(&mut self) {
        while let Ok(ev) = self.net.rx.try_recv() {
            match ev {
                NetEvent::Message(s, msg) if s == self.session => self.on_message(msg),
                NetEvent::Ping(s, rtt) if s == self.session => self.ping = Some(rtt),
                NetEvent::VoicePing(s, rtt) if s == self.session => self.voice.pong(rtt),
                NetEvent::Secured(s, fingerprint) if s == self.session => {
                    // Trust on first use: the first certificate seen at an address is pinned.
                    if let Conn::Connecting { address, .. } = &self.conn {
                        self.settings.server_certs.entry(ServerKey::of(address)).or_insert(fingerprint);
                    }
                }
                NetEvent::Closed(s, ended) if s == self.session => self.on_closed(ended),
                _ => {} // stale session
            }
        }
    }

    fn on_closed(&mut self, ended: Ended) {
        let reason = match ended {
            Ended::ByUser => None,
            Ended::Failed(reason) => Some(reason),
            Ended::CertChanged { pinned, presented } => {
                if let Conn::Connecting { address, password, .. } = &self.conn {
                    let (address, password) = (address.clone(), password.clone());
                    self.cert_change = Some(CertChange { address, password, pinned, presented });
                }
                Some("The server's certificate has changed".to_owned())
            }
        };
        let kicked = self.kicked.take();
        let reason = kicked.clone().or(reason);
        let was_connected = matches!(self.conn, Conn::Connected { .. });
        self.conn = Conn::Disconnected;
        self.view = None;
        self.ping = None;
        self.voice.disconnected();
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
        if !was_connected || kicked.is_some() {
            self.last_error = reason;
        }
        self.active_tab = 0;
    }

    fn on_message(&mut self, msg: S2C) {
        match msg {
            S2C::Welcome { your_id, server, channels, clients, permissions, .. } => {
                let address = match &self.conn {
                    Conn::Connecting { address, .. } => address.clone(),
                    _ => String::new(),
                };
                let server_key = ServerKey::of(&address);
                let recent = Recent { address, server_name: server.name.clone() };
                self.settings.recent.retain(|r| r.address != recent.address);
                self.settings.recent.insert(0, recent);
                self.settings.recent.truncate(MAX_RECENT);

                let name = server.name.clone();
                let welcome = server.welcome.clone();
                for c in &clients {
                    self.apply_user_volume(c.id, &c.nickname);
                }
                self.view = Some(ServerView::new(your_id, server, channels, clients, permissions));
                self.conn = Conn::Connected { server: server_key };
                self.voice.connected();
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
            }
            S2C::ClientJoined(info) => {
                self.apply_user_volume(info.id, &info.nickname);
                let line = format!("{} connected", info.nickname);
                let here = self.view.as_ref().and_then(ServerView::my_channel) == Some(info.channel);
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
                    LeaveReason::Disconnected => Some("disconnected"),
                    LeaveReason::TimedOut => Some("timed out"),
                    // The kick notice already said so.
                    LeaveReason::Kicked => None,
                };
                self.apply(&S2C::ClientLeft { id, reason });
                if let Some(verb) = verb {
                    self.push(0, LineKind::System, format!("{name} {verb}"));
                }
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
            S2C::ClientShown(ref info) => {
                self.apply_user_volume(info.id, &info.nickname);
                let here = self.view.as_ref().and_then(ServerView::my_channel) == Some(info.channel);
                let line = format!("{} joined the channel", info.nickname);
                self.apply(&msg);
                if here {
                    self.push(1, LineKind::System, line);
                    self.sound(Sound::UserJoined);
                }
            }
            S2C::ClientHidden { id } => {
                let Some(view) = &self.view else { return };
                let Some(c) = view.clients.get(&id) else { return };
                let (name, here) = (c.nickname.clone(), Some(c.channel) == view.my_channel());
                self.apply(&msg);
                if here {
                    self.push(1, LineKind::System, format!("{name} left the channel"));
                    self.sound(Sound::UserLeft);
                }
            }
            S2C::ClientStatus { .. } | S2C::Permissions(_) | S2C::Roles(_) => self.apply(&msg),
            S2C::ClientSpeech { id, speech } => {
                let mine = self.view.as_ref().is_some_and(|v| v.my_id == id);
                let was = self.my_speech();
                self.apply(&msg);
                let line = match (was, speech) {
                    (_, Speech::ServerMuted) => Some("A moderator muted you on the server"),
                    (Speech::ServerMuted, _) => Some("A moderator unmuted you on the server"),
                    (_, Speech::NotHere) => Some("You can't speak in this channel"),
                    (_, Speech::Allowed) => None,
                };
                if let Some(line) = line.filter(|_| mine) {
                    let tab = self.active_tab;
                    self.push(tab, LineKind::System, line);
                }
            }
            S2C::Channels(_) => {
                self.apply(&msg);
                self.sync_channel_tab_title();
            }
            S2C::ClientRole { id, ref role } => {
                let Some(view) = &self.view else { return };
                let line = match (id == view.my_id, role) {
                    (true, Some(role)) => format!("Your role is {role} now"),
                    (true, None) => "You no longer have a role".to_owned(),
                    (false, Some(role)) => format!("{} is {role} now", view.nickname(id).unwrap_or("Someone")),
                    (false, None) => String::new(),
                };
                self.apply(&msg);
                if !line.is_empty() {
                    self.push(0, LineKind::System, line);
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
            S2C::ClientKicked { id, by, from_server, reason } => self.on_kicked(id, by, from_server, &reason),
            S2C::Bans(list) => {
                if let Some(settings) = &mut self.server_settings {
                    settings.bans = Some(list);
                }
            }
            S2C::Poked { from, message } => self.on_poked(from, message),
            S2C::Error { message } => {
                let tab = self.active_tab;
                self.push(tab, LineKind::Error, message);
            }
            S2C::Rejected { .. } | S2C::Pong { .. } => {} // handled by the net layer
        }
    }

    pub(super) fn apply(&mut self, msg: &S2C) {
        if let Some(v) = &mut self.view {
            v.apply(msg);
        }
    }
}
