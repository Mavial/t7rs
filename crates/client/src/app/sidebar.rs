//! The sidebar: the server you're on with its menu, the channel tree, a
//! banner when something needs you, and your own panel at the bottom with the
//! mic, sound and share buttons.

use std::time::Duration;

use egui::{
    Align2, CornerRadius, FontId, Layout, Margin, PopupCloseBehavior, RectAlign, RichText, Sense, Stroke, vec2,
};
use t7_proto::model::ServerView;
use t7_proto::perms::{Perm, Scope};
use t7_proto::{MAX_NICKNAME_LEN, Speech};

use super::chat::LineKind;
use super::widgets::{
    Item, menu_button, menu_separator, menu_style, mic_held, paint_avatar, text_input, toggle, truncated,
};
use super::{App, Conn, manages};
use crate::icons::{self, Icon};
use crate::theme::{self, Btn};

/// Height of the sidebar's header, and of the main pane's beside it.
pub(super) const HEADER_H: f32 = 52.0;

const NO_VOICE: &str = "Voice packets aren't getting through. The server's UDP port (same number as the TCP \
                        port, 9988 by default) is probably blocked by a firewall.";

/// What the banner above your panel shows, worked out each frame from the app state.
pub(super) enum Banner {
    UpdateReady(String),
    /// The connection dropped or we were kicked: why, and Reconnect.
    Lost {
        headline: String,
        detail: String,
    },
}

/// Picked from the server menu, applied after drawing it.
#[derive(Clone, Copy)]
enum ServerAction {
    Connect,
    CreateChannel,
    ServerSettings,
    ClaimOwnership,
    CopyAddress,
    Disconnect,
    Reconnect,
    BackToServers,
}

/// Picked from your menu.
#[derive(Clone, Copy)]
enum YouAction {
    Away,
    Settings,
}

impl App {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let window = ui.ctx().content_rect().width();
        egui::Panel::left("sidebar")
            .frame(egui::Frame::new().fill(theme::PANEL))
            .resizable(true)
            .show_separator_line(false)
            .default_size(280.0)
            // Narrower in a small window, so the chat keeps its room.
            .size_range(220.0..=(window * 0.32).clamp(232.0, 480.0))
            .show(ui, |ui| {
                self.sidebar_header(ui);
                self.me_panel(ui);
                self.banner_slot(ui);
                let body = egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 0, bottom: 6 });
                body.show(ui, |ui| match self.conn {
                    Conn::Disconnected => self.saved_servers(ui),
                    // Where the tree will be, so nothing jumps when it arrives.
                    Conn::Connecting(_) => skeleton(ui),
                    Conn::Connected { .. } | Conn::Lost { .. } => self.channel_tree(ui),
                });
            });
    }

    /// Before connecting, the sidebar lists the servers you've been on.
    fn saved_servers(&mut self, ui: &mut egui::Ui) {
        if self.settings.recent.is_empty() {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new("Servers you join show up here.").size(12.0).color(theme::TEXT_FAINT));
            });
            return;
        }
        let (mut connect_to, mut forget) = (None, None);
        ui.add_space(4.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
        let label_pos = egui::pos2(rect.left() + 8.0, rect.center().y);
        ui.painter().text(label_pos, Align2::LEFT_CENTER, "SERVERS", FontId::proportional(11.0), theme::TEXT_FAINT);
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, r) in self.settings.recent.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
                let p = ui.painter();
                if resp.hovered() || resp.context_menu_opened() {
                    p.rect_filled(rect, CornerRadius::same(6), theme::SURFACE_HOVER);
                }
                paint_avatar(p, egui::pos2(rect.left() + 20.0, rect.center().y), 12.0, &r.server_name);
                let x = rect.left() + 40.0;
                let width = rect.right() - 8.0 - x;
                let name = truncated(ui, &r.server_name, FontId::proportional(13.0), theme::TEXT, width);
                let address = truncated(ui, &r.address, FontId::proportional(11.0), theme::TEXT_FAINT, width);
                let top = rect.center().y - f32::midpoint(name.size().y, address.size().y);
                let name_h = name.size().y;
                p.galley(egui::pos2(x, top), name, theme::TEXT);
                p.galley(egui::pos2(x, top + name_h), address, theme::TEXT_FAINT);
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    connect_to = Some(r.address.clone());
                }
                resp.context_menu(|ui| {
                    menu_style(ui, 180.0);
                    if ui.add(Item::new(Icon::ArrowRight, "Connect")).clicked() {
                        connect_to = Some(r.address.clone());
                        ui.close();
                    }
                    if ui.add(Item::new(Icon::Trash, "Remove from list").danger()).clicked() {
                        forget = Some(i);
                        ui.close();
                    }
                });
            }
        });
        if let Some(i) = forget {
            self.settings.recent.remove(i);
        }
        if let Some(address) = connect_to {
            let nickname = self.settings.nickname.clone();
            self.connect(address, nickname, String::new());
        }
    }

    fn restart_to_update(&mut self) {
        let Some(exe) = self.updater.install() else { return };
        // Same arguments, so a command-line connection reconnects.
        match std::process::Command::new(&exe).args(std::env::args().skip(1)).spawn() {
            Ok(_) => self.quit = true,
            Err(e) => self.push(0, LineKind::Error, format!("Updated, but couldn't restart T7: {e}. Start it again.")),
        }
    }

    /// A lost connection comes first: it is why the tree is dimmed.
    pub(super) fn banner(&self) -> Option<Banner> {
        if let Conn::Lost { why, .. } = &self.conn {
            return Some(Banner::Lost { headline: why.headline.clone(), detail: why.detail.clone() });
        }
        match self.updater.status() {
            crate::update::Status::Ready(version) => Some(Banner::UpdateReady(version)),
            _ => None,
        }
    }

    fn address(&self) -> Option<&str> {
        match &self.conn {
            Conn::Connecting(login) | Conn::Connected { login, .. } | Conn::Lost { login, .. } => Some(&login.address),
            Conn::Disconnected => None,
        }
    }

    /// The server's name, or what we know of it while connecting.
    pub(super) fn server_name(&self) -> String {
        if let Some(view) = self.shown_view() {
            return view.info.name.clone();
        }
        let Some(address) = self.address() else { return "T7".to_owned() };
        let recent = self.settings.recent.iter().find(|r| r.address == address);
        recent.map_or_else(|| address.to_owned(), |r| r.server_name.clone())
    }

    /// The dot and words after the server's name, and what they mean.
    fn presence(&self) -> Option<(egui::Color32, String, String)> {
        match &self.conn {
            Conn::Disconnected => None,
            Conn::Lost { .. } => Some((theme::TEXT_FAINT, "Offline".to_owned(), "Not connected".to_owned())),
            Conn::Connecting(login) => {
                Some((theme::AMBER, "Connecting".to_owned(), format!("Connecting to {}", login.address)))
            }
            Conn::Connected { .. } if self.voice.blocked() => {
                Some((theme::AMBER, "no voice".to_owned(), NO_VOICE.to_owned()))
            }
            Conn::Connected { login, .. } => {
                let ping = match self.voice.rtt().or(self.ping) {
                    Some(p) if p < Duration::from_millis(1) => "<1 ms".to_owned(),
                    Some(p) => format!("{} ms", p.as_millis()),
                    None => String::new(),
                };
                Some((theme::GREEN, ping, format!("Connected to {}", login.address)))
            }
        }
    }

    fn sidebar_header(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new().inner_margin(Margin { left: 16, right: 10, top: 0, bottom: 0 });
        let mut picked = None;
        egui::Panel::top("server").frame(frame).exact_size(HEADER_H).show_separator_line(false).show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                picked = self.server_button(ui);
                if let Some((color, text, tip)) = self.presence() {
                    presence_dot(ui, color, &text).on_hover_text(tip);
                }
                let name = self.server_name();
                let galley = truncated(ui, &name, FontId::proportional(14.5), theme::TEXT, ui.available_width());
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), galley.size().y), Sense::hover());
                ui.painter().galley(rect.left_top(), galley, theme::TEXT);
            });
        });
        match picked {
            Some(ServerAction::Connect) => self.open_connect_form(),
            Some(ServerAction::CreateChannel) => self.open_create_channel(None),
            Some(ServerAction::ServerSettings) => self.open_server_settings(),
            Some(ServerAction::ClaimOwnership) => self.open_claim_ownership(),
            Some(ServerAction::CopyAddress) => {
                if let Some(address) = self.address() {
                    ui.ctx().copy_text(address.to_owned());
                }
            }
            Some(ServerAction::Disconnect) => self.disconnect(),
            Some(ServerAction::Reconnect) => self.reconnect(),
            Some(ServerAction::BackToServers) => self.forget_lost(),
            None => {}
        }
    }

    /// `+` to connect while offline, else `⋯` with what the top bar used to hold.
    fn server_button(&self, ui: &mut egui::Ui) -> Option<ServerAction> {
        if matches!(self.conn, Conn::Disconnected) {
            let resp = menu_button(ui, Icon::Plus).on_hover_text("Connect to a server (Ctrl+S)");
            return resp.clicked().then_some(ServerAction::Connect);
        }
        let resp = menu_button(ui, Icon::Dots);
        let mut picked = None;
        let lost = matches!(self.conn, Conn::Lost { .. });
        egui::Popup::menu(&resp).align(RectAlign::BOTTOM_END).show(|ui| {
            picked = if lost {
                lost_menu(ui, self.address().unwrap_or_default())
            } else {
                server_menu(ui, self.view.as_ref(), self.address().unwrap_or_default())
            };
        });
        picked
    }

    fn banner_slot(&mut self, ui: &mut egui::Ui) {
        let Some(banner) = self.banner() else { return };
        let (fill, icon, icon_color, (headline, detail), button) = match &banner {
            Banner::UpdateReady(version) => (
                theme::ACCENT_BG,
                Icon::Update,
                theme::ACCENT_TEXT,
                (format!("T7 {version} is ready"), None),
                Btn::primary("Restart").small(),
            ),
            Banner::Lost { headline, detail } => (
                theme::RED.gamma_multiply(0.12),
                Icon::Warning,
                theme::RED,
                (headline.clone(), Some(detail.clone())),
                Btn::secondary("Reconnect").small(),
            ),
        };
        let mut clicked = false;
        let outer = egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 2, bottom: 8 });
        egui::Panel::bottom("banner").frame(outer).show_separator_line(false).show(ui, |ui| {
            let frame = egui::Frame::new()
                .fill(fill)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 8));
            frame.show(ui, |ui| {
                let width = ui.available_width();
                let button_w =
                    ui.painter().layout_no_wrap("Reconnect".to_owned(), FontId::proportional(12.5), theme::TEXT);
                let button_w = button_w.size().x + 20.0;
                let text_w = width - 14.0 - 8.0 - 8.0 - button_w;
                let p = ui.painter();
                let headline = p.layout(headline, FontId::proportional(12.5), theme::TEXT, text_w);
                let detail = detail.map(|d| p.layout(d, FontId::proportional(11.5), theme::TEXT_DIM, text_w));
                let text_h = headline.size().y + detail.as_ref().map_or(0.0, |d| d.size().y + 1.0);
                let (rect, _) = ui.allocate_exact_size(vec2(width, text_h.max(24.0)), Sense::hover());
                let icon_rect =
                    egui::Rect::from_center_size(egui::pos2(rect.left() + 7.0, rect.center().y), vec2(14.0, 14.0));
                icons::paint(ui.painter(), icon_rect, icon, icon_color);
                let mut text_pos = egui::pos2(rect.left() + 22.0, rect.center().y - text_h / 2.0);
                let headline_h = headline.size().y;
                ui.painter().galley(text_pos, headline, theme::TEXT);
                if let Some(detail) = detail {
                    text_pos.y += headline_h + 1.0;
                    ui.painter().galley(text_pos, detail, theme::TEXT_DIM);
                }
                let slot =
                    egui::Rect::from_min_max(egui::pos2(rect.right() - button_w, rect.top()), rect.right_bottom());
                clicked = ui
                    .put(slot, button)
                    .on_hover_text(match &banner {
                        Banner::UpdateReady(_) => "T7 restarts with the update and reconnects.",
                        Banner::Lost { .. } => "Connect to this server again",
                    })
                    .clicked();
            });
        });
        if clicked {
            match banner {
                Banner::UpdateReady(_) => self.restart_to_update(),
                Banner::Lost { .. } => self.reconnect(),
            }
        }
    }

    fn me_panel(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new().inner_margin(Margin { left: 6, right: 8, top: 0, bottom: 0 });
        egui::Panel::bottom("me").frame(frame).exact_size(56.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let who_w = (ui.available_width() - 3.0 * (32.0 + 2.0)).max(60.0);
                let who = self.who(ui, who_w);
                self.your_menu(&who);
                self.voice_buttons(ui);
                self.share_button(ui);
            });
        });
    }

    /// How you are, in words, colored like the marks in the tree.
    fn status_line(&self) -> (String, egui::Color32) {
        let s = self.settings.status;
        let (text, color) = match &self.conn {
            Conn::Connected { .. } => match self.my_speech() {
                Speech::ServerMuted => ("Muted on the server", theme::RED),
                Speech::NotHere => ("Can't speak here", theme::AMBER),
                Speech::Allowed if s.away => ("Away", theme::AMBER),
                Speech::Allowed if s.deafened => ("Sound off", theme::RED),
                Speech::Allowed if s.muted => ("Microphone muted", theme::RED),
                Speech::Allowed if self.sharing.is_some() => ("Sharing your screen", theme::RED),
                Speech::Allowed => {
                    let channel = self.view.as_ref().and_then(ServerView::my_channel).map(|c| self.channel_name(c));
                    return (channel.map_or_else(|| "Online".to_owned(), |c| format!("In {c}")), theme::GREEN);
                }
            },
            Conn::Connecting(_) => ("Connecting…", theme::TEXT_FAINT),
            Conn::Disconnected | Conn::Lost { .. } => ("Offline", theme::TEXT_FAINT),
        };
        (text.to_owned(), color)
    }

    /// Your avatar, name and status line: one button that opens your menu.
    fn who(&self, ui: &mut egui::Ui, width: f32) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 44.0), Sense::click());
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
        let p = ui.painter();
        if open || resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(8), theme::SURFACE_HOVER);
        }
        let nick = self.view.as_ref().and_then(ServerView::me).map_or(self.settings.nickname.as_str(), |c| &c.nickname);
        let mut x = rect.left() + 6.0;
        // A narrow sidebar drops the avatar before the words.
        if width >= 150.0 {
            let center = egui::pos2(x + 16.0, rect.center().y);
            paint_avatar(p, center, 16.0, nick);
            if self.voice.shared.transmitting() {
                p.circle_stroke(center, 18.5, Stroke::new(2.0, theme::GREEN));
            }
            x += 32.0 + 9.0;
        }
        let text_w = rect.right() - 6.0 - x;
        let (status, color) = self.status_line();
        let name = truncated(ui, nick, FontId::proportional(13.0), theme::TEXT, text_w - 16.0);
        let status = truncated(ui, &status, FontId::proportional(11.5), color, text_w);
        let top = rect.center().y - f32::midpoint(name.size().y, status.size().y);
        let chevron_x = x + name.size().x + 9.0;
        let chevron_y = top + name.size().y / 2.0 + 1.0;
        p.galley(egui::pos2(x, top + status.size().y), status, color);
        p.galley(egui::pos2(x, top), name, theme::TEXT);
        let chevron = egui::Rect::from_center_size(egui::pos2(chevron_x, chevron_y), vec2(11.0, 11.0));
        icons::paint(p, chevron, Icon::ChevronDown, theme::TEXT_FAINT);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Your nickname, away, and settings, so the panel keeps to three buttons.
    fn your_menu(&mut self, who: &egui::Response) {
        let connected = matches!(self.conn, Conn::Connected { .. });
        let away = self.settings.status.away;
        let mut picked = None;
        let popup = egui::Popup::menu(who).close_behavior(PopupCloseBehavior::CloseOnClickOutside);
        popup.align(RectAlign::TOP_START).show(|ui| {
            menu_style(ui, 250.0);
            ui.set_max_width(250.0);
            let frame = egui::Frame::new().inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 8 });
            frame.show(ui, |ui| {
                ui.label(RichText::new("Nickname").size(11.0).color(theme::TEXT_FAINT));
                ui.add_space(5.0);
                ui.add(text_input(&mut self.settings.nickname).char_limit(MAX_NICKNAME_LEN));
                if connected {
                    ui.add_space(5.0);
                    ui.label(RichText::new("Used the next time you connect").size(11.0).color(theme::TEXT_FAINT));
                }
            });
            menu_separator(ui);
            let moon = if away { theme::AMBER } else { theme::TEXT_FAINT };
            if ui.add(Item::new(Icon::Moon, "Away").right("Ctrl+Shift+A").icon_color(moon)).clicked() {
                picked = Some(YouAction::Away);
                ui.close();
            }
            if ui.add(Item::new(Icon::Sliders, "Settings…").right("Ctrl+,")).clicked() {
                picked = Some(YouAction::Settings);
                ui.close();
            }
        });
        match picked {
            Some(YouAction::Away) => {
                let s = self.settings.status;
                self.set_status(t7_proto::ClientStatus { away: !s.away, ..s });
            }
            Some(YouAction::Settings) => self.open_voice_settings(),
            None => {}
        }
    }

    fn voice_buttons(&mut self, ui: &mut egui::Ui) {
        let mut s = self.settings.status;
        let mic_tip = if s.muted { "Unmute microphone (Ctrl+Shift+M)" } else { "Mute microphone (Ctrl+Shift+M)" };
        // When the server holds the mic, it is grey, not red: you didn't do it.
        let mic = match self.my_speech() {
            Speech::Allowed => toggle(ui, &mut s.muted, Icon::Mic, Icon::MicOff, theme::RED, mic_tip),
            Speech::NotHere => {
                mic_held(ui, Icon::MicOff, theme::TEXT_FAINT, "You can't speak in this channel");
                false
            }
            Speech::ServerMuted => {
                let tip = "A moderator muted you on the server. Only a moderator can unmute you.";
                mic_held(ui, Icon::MicLocked, theme::TEXT_FAINT, tip);
                false
            }
        };
        let sound_tip = if s.deafened { "Turn sound on (Ctrl+Shift+D)" } else { "Turn sound off (Ctrl+Shift+D)" };
        let sound = toggle(ui, &mut s.deafened, Icon::Headphones, Icon::HeadphonesOff, theme::RED, sound_tip);
        if mic || sound {
            self.set_status(s);
        }
    }
}

/// Grey rows shaped like channels and the people in them.
fn skeleton(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing.y = 0.0;
    for (i, width) in [0.7, 0.4, 0.55, 0.45, 0.6, 0.35, 0.5].into_iter().enumerate() {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        let p = ui.painter();
        let member = i % 3 != 0;
        let x = rect.left() + if member { 30.0 } else { 8.0 };
        let y = rect.center().y;
        let mark = if member {
            egui::Rect::from_center_size(egui::pos2(x + 8.0, y), vec2(16.0, 16.0))
        } else {
            egui::Rect::from_min_size(egui::pos2(x, y - 5.0), vec2(14.0, 10.0))
        };
        p.rect_filled(mark, CornerRadius::same(if member { 8 } else { 3 }), theme::SURFACE);
        let bar = egui::Rect::from_min_size(egui::pos2(x + 24.0, y - 5.0), vec2(rect.width() * width, 10.0));
        p.rect_filled(bar, CornerRadius::same(4), theme::SURFACE);
    }
}

/// A status dot and a few words, like the ping after the server's name.
fn presence_dot(ui: &mut egui::Ui, color: egui::Color32, text: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(11.5), theme::TEXT_FAINT);
    let text_w = if text.is_empty() { 0.0 } else { galley.size().x + 6.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(7.0 + text_w, 20.0), Sense::hover());
    ui.painter().circle_filled(egui::pos2(rect.left() + 3.5, rect.center().y), 3.5, color);
    let pos = egui::pos2(rect.left() + 13.0, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, theme::TEXT_FAINT);
    resp
}

/// After a lost connection: try again, or put the old tree away.
fn lost_menu(ui: &mut egui::Ui, address: &str) -> Option<ServerAction> {
    menu_style(ui, 220.0);
    let mut picked = None;
    let copy = Item::new(Icon::Copy, "Copy address");
    let copy = if address.chars().count() <= 28 { copy.right(address) } else { copy };
    for (item, action) in [
        (Item::new(Icon::ArrowRight, "Reconnect"), ServerAction::Reconnect),
        (copy, ServerAction::CopyAddress),
        (Item::new(Icon::Cross, "Back to servers"), ServerAction::BackToServers),
    ] {
        if ui.add(item).clicked() {
            picked = Some(action);
            ui.close();
        }
    }
    picked
}

/// Server-wide actions, each shown only to those who may use it.
fn server_menu(ui: &mut egui::Ui, view: Option<&ServerView>, address: &str) -> Option<ServerAction> {
    menu_style(ui, 240.0);
    let mut picked = None;
    let mut item = |ui: &mut egui::Ui, item: Item, action| {
        if ui.add(item).clicked() {
            picked = Some(action);
            ui.close();
        }
    };
    if let Some(view) = view {
        let create = if manages(view, Scope::Server) {
            Some("Create channel…")
        } else {
            view.may(Perm::CreateTemporary).then_some("Create temporary channel…")
        };
        if let Some(label) = create {
            item(ui, Item::new(Icon::Plus, label), ServerAction::CreateChannel);
            menu_separator(ui);
        }
        if view.may(Perm::ManageRoles) || view.may(Perm::Ban) {
            item(ui, Item::new(Icon::Sliders, "Server settings…"), ServerAction::ServerSettings);
        }
        if view.is_owner() {
            ui.add_enabled(false, Item::new(Icon::Key, "You own this server"));
        } else {
            item(ui, Item::new(Icon::Key, "Claim ownership…"), ServerAction::ClaimOwnership);
        }
        menu_separator(ui);
    }
    let copy = Item::new(Icon::Copy, "Copy address");
    // A long address would stretch the menu; it is on the clipboard either way.
    let copy = if address.chars().count() <= 28 { copy.right(address) } else { copy };
    item(ui, copy, ServerAction::CopyAddress);
    let leave = if view.is_some() { "Disconnect" } else { "Cancel" };
    item(ui, Item::new(Icon::Power, leave).danger(), ServerAction::Disconnect);
    picked
}
