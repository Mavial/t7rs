//! Connecting: the empty state with recent servers, the connect dialog, and
//! the warning when a server's certificate changed.

use egui::{Align2, CornerRadius, FontId, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::tls::Fingerprint;

use super::widgets::{dialog_footer, dialog_header, field, field_label, logo};
use super::{App, Conn, Login};
use crate::icons::Icon;
use crate::net::ServerKey;
use crate::settings::Recent;
use crate::theme::Btn;
use crate::{icons, settings, theme};

/// Width of the recent-server cards and the button above them.
const RECENT_W: f32 = 320.0;

pub(super) const MAX_RECENT: usize = 8;
/// Server prefilled for first-time users. Set at build time, e.g.
/// `T7_DEFAULT_SERVER=example.com cargo build --release -p t7-client`.
pub(super) const DEFAULT_SERVER: Option<&str> = option_env!("T7_DEFAULT_SERVER");

pub(super) struct ConnectForm {
    address: String,
    nickname: String,
    password: String,
    focus_requested: bool,
}

/// A server came back with a different certificate than the pinned one.
pub(super) struct CertChange {
    pub(super) address: String,
    pub(super) password: String,
    pub(super) pinned: Fingerprint,
    pub(super) presented: Fingerprint,
}

/// A fingerprint as two rows of 16 bytes, so it fits a dialog.
fn two_lines(fingerprint: Fingerprint) -> String {
    let text = fingerprint.to_string();
    let (first, second) = text.split_at(16 * 3);
    format!("{}\n{second}", first.trim_end_matches(':'))
}

fn recent_card(ui: &mut egui::Ui, r: &Recent) -> egui::Response {
    let width = RECENT_W.min(ui.available_width());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 50.0), Sense::click());
    let p = ui.painter();
    let (fill, stroke) =
        if resp.hovered() { (theme::SURFACE_HOVER, theme::BORDER_STRONG) } else { (theme::SURFACE, theme::BORDER) };
    p.rect(rect, CornerRadius::same(8), fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    let x = rect.left() + 14.0;
    p.text(
        egui::pos2(x, rect.top() + 17.0),
        Align2::LEFT_CENTER,
        &r.server_name,
        FontId::proportional(13.5),
        theme::TEXT,
    );
    p.text(
        egui::pos2(x, rect.top() + 34.0),
        Align2::LEFT_CENTER,
        &r.address,
        FontId::proportional(12.0),
        theme::TEXT_FAINT,
    );
    let chevron = egui::Rect::from_center_size(rect.right_center() - vec2(18.0, 0.0), vec2(16.0, 16.0));
    icons::paint(p, chevron, Icon::ChevronRight, if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

impl App {
    pub(super) fn open_connect_form(&mut self) {
        let address = self
            .settings
            .recent
            .first()
            .map_or_else(|| DEFAULT_SERVER.unwrap_or("localhost").into(), |r| r.address.clone());
        self.form = Some(ConnectForm {
            address,
            nickname: self.settings.nickname.clone(),
            password: String::new(),
            focus_requested: false,
        });
    }

    pub(super) fn connect(&mut self, address: String, nickname: String, password: String) {
        let address = address.trim().to_owned();
        let nickname = nickname.trim().to_owned();
        if address.is_empty() || nickname.is_empty() {
            return;
        }
        self.settings.nickname.clone_from(&nickname);
        let pinned = self.settings.server_certs.get(&ServerKey::of(&address)).copied();
        let login_password = Some(password.clone()).filter(|p| !p.is_empty());
        let key = self.settings.identity_key.get_or_insert_with(settings::new_identity_key).clone();
        self.session = self.net.connect(address.clone(), nickname, login_password, pinned, key);
        self.conn = Conn::Connecting(Login { address, password });
        self.view = None;
        self.ping = None;
        self.last_error = None;
        self.kicked = None;
        self.form = None;
    }

    pub(super) fn disconnect(&mut self) {
        if !matches!(self.conn, Conn::Disconnected) {
            self.net.disconnect();
        }
    }

    pub(super) fn empty_state(&mut self, ui: &mut egui::Ui) {
        let mut connect_to = None;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(20.0));
            logo(ui, 64.0);
            ui.add_space(16.0);
            if let Conn::Connecting(Login { address, .. }) = &self.conn {
                ui.label(RichText::new("Connecting…").size(22.0).strong().color(theme::TEXT));
                ui.add_space(6.0);
                ui.label(RichText::new(address).color(theme::TEXT_DIM));
                ui.add_space(14.0);
                ui.add(egui::Spinner::new().size(22.0).color(theme::ACCENT_TEXT));
            } else {
                ui.label(RichText::new("Not connected").size(22.0).strong().color(theme::TEXT));
                ui.add_space(4.0);
                ui.label(RichText::new("Connect to a T7 server to start talking.").color(theme::TEXT_DIM));
                if let Some(err) = &self.last_error {
                    ui.add_space(10.0);
                    ui.label(RichText::new(err).color(theme::RED));
                }
                ui.add_space(18.0);
                if ui.add(Btn::primary("Connect to server").min_width(RECENT_W)).clicked() {
                    self.open_connect_form();
                }

                if !self.settings.recent.is_empty() {
                    ui.add_space(30.0);
                    ui.label(RichText::new("RECENT").size(11.0).strong().color(theme::TEXT_FAINT));
                    ui.add_space(4.0);
                    for r in &self.settings.recent {
                        if recent_card(ui, r).clicked() {
                            connect_to = Some(r.address.clone());
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

    pub(super) fn connect_dialog(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.form else { return };
        let mut submit = false;
        let mut cancel = false;
        let modal = theme::modal("connect").show(ctx, |ui| {
            ui.set_width(360.0);
            dialog_header(ui, "Connect to server", "Enter an address like example.com or 1.2.3.4:9988");
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
            dialog_footer(ui, |ui| {
                let ready = !form.address.trim().is_empty() && !form.nickname.trim().is_empty();
                if ui.add_enabled(ready, Btn::primary("Connect")).clicked() {
                    submit = true;
                }
                if ui.add(Btn::secondary("Cancel")).clicked() {
                    cancel = true;
                }
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

    pub(super) fn cert_change_dialog(&mut self, ctx: &egui::Context) {
        let Some(change) = &self.cert_change else { return };
        let (mut trust, mut cancel) = (false, false);
        let modal = theme::modal("cert-change").show(ctx, |ui| {
            ui.set_width(400.0);
            let note = format!(
                "{} shows a different certificate than last time. That is expected if its admin \
                 replaced it. Otherwise someone may be intercepting the connection. The server \
                 prints its fingerprint when it starts, so ask the admin which is right.",
                change.address
            );
            dialog_header(ui, "Server certificate changed", &note);
            for (title, fingerprint, color) in
                [("Saved", change.pinned, theme::TEXT_DIM), ("New", change.presented, theme::TEXT)]
            {
                field_label(ui, title);
                egui::Frame::new()
                    .fill(theme::BG)
                    .stroke(Stroke::new(1.0, theme::BORDER))
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(two_lines(fingerprint)).font(FontId::monospace(12.0)).color(color));
                    });
                ui.add_space(6.0);
            }
            dialog_footer(ui, |ui| {
                if ui.add(Btn::danger("Trust new certificate and connect")).clicked() {
                    trust = true;
                }
                if ui.add(Btn::secondary("Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
        if trust {
            let change = self.cert_change.take().expect("dialog open");
            self.settings.server_certs.insert(ServerKey::of(&change.address), change.presented);
            let nickname = self.settings.nickname.clone();
            self.connect(change.address, nickname, change.password);
        } else if cancel || modal.should_close() {
            self.cert_change = None;
        }
    }
}
