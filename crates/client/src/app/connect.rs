//! Before you're on a server: joining one, the first-run form, connecting,
//! the connect dialog, and the warning when a server's certificate changed.

use egui::{Align, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::tls::Fingerprint;

use super::widgets::{dialog_footer, field_label, logo, paint_avatar, text_input, truncated};
use super::{App, Conn, Login};
use crate::icons::Icon;
use crate::net::ServerKey;
use crate::settings::Recent;
use crate::theme::Btn;
use crate::{icons, settings, theme};

/// Width of the first-run form, the recent-server cards and the button above them.
const FORM_W: f32 = 340.0;
const ADDRESS_HINT: &str = "example.com or 1.2.3.4:9988";

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

impl ConnectForm {
    fn ready(&self) -> bool {
        !self.address.trim().is_empty() && !self.nickname.trim().is_empty()
    }
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
    let width = FORM_W.min(ui.available_width());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 50.0), Sense::click());
    let p = ui.painter();
    let (fill, stroke) =
        if resp.hovered() { (theme::SURFACE_HOVER, theme::BORDER_STRONG) } else { (theme::SURFACE, theme::BORDER) };
    p.rect(rect, CornerRadius::same(8), fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    paint_avatar(p, egui::pos2(rect.left() + 26.0, rect.center().y), 14.0, &r.server_name);
    let x = rect.left() + 52.0;
    let text_w = rect.right() - 36.0 - x;
    let name = truncated(ui, &r.server_name, FontId::proportional(13.0), theme::TEXT, text_w);
    let address = truncated(ui, &r.address, FontId::proportional(11.5), theme::TEXT_FAINT, text_w);
    let top = rect.center().y - f32::midpoint(name.size().y, address.size().y);
    let name_h = name.size().y;
    p.galley(egui::pos2(x, top), name, theme::TEXT);
    p.galley(egui::pos2(x, top + name_h), address, theme::TEXT_FAINT);
    let chevron = egui::Rect::from_center_size(rect.right_center() - vec2(18.0, 0.0), vec2(14.0, 14.0));
    icons::paint(p, chevron, Icon::ChevronRight, if resp.hovered() { theme::TEXT } else { theme::TEXT_FAINT });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn input(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    password: bool,
    error: Option<&str>,
) -> egui::Response {
    field_label(ui, label);
    let mut edit = text_input(value).password(password).hint_text(RichText::new(hint).color(theme::TEXT_FAINT));
    if error.is_some() {
        let frame = egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(Stroke::new(1.0, theme::RED))
            .corner_radius(CornerRadius::same(theme::RADIUS))
            .inner_margin(Margin::symmetric(9, 0));
        edit = edit.frame(frame);
    }
    let resp = ui.add(edit);
    if let Some(error) = error {
        ui.add_space(2.0);
        ui.label(RichText::new(error).size(11.5).color(theme::RED));
    }
    ui.add_space(8.0);
    resp
}

fn form_fields(ui: &mut egui::Ui, form: &mut ConnectForm, nickname_first: bool, error: Option<&str>) {
    let (address, nickname) = if nickname_first {
        let nickname = input(ui, "Nickname", &mut form.nickname, "", false, None);
        (input(ui, "Server address", &mut form.address, ADDRESS_HINT, false, error), nickname)
    } else {
        let address = input(ui, "Server address", &mut form.address, ADDRESS_HINT, false, error);
        (address, input(ui, "Nickname", &mut form.nickname, "", false, None))
    };
    if !form.focus_requested {
        if form.address.trim().is_empty() || error.is_some() { address } else { nickname }.request_focus();
        form.focus_requested = true;
    }
    input(ui, "Password", &mut form.password, "Only if the server has one", true, None);
}

impl App {
    pub(super) fn open_connect_form(&mut self) {
        if self.form.is_some() {
            return;
        }
        let address = self
            .settings
            .recent
            .first()
            .map_or_else(|| DEFAULT_SERVER.unwrap_or_default().into(), |r| r.address.clone());
        self.form = Some(ConnectForm {
            address,
            nickname: self.settings.nickname.clone(),
            password: String::new(),
            focus_requested: false,
        });
    }

    pub(super) fn reopen_connect_form(&mut self, login: Login) {
        let Login { address, password } = login;
        self.form =
            Some(ConnectForm { address, nickname: self.settings.nickname.clone(), password, focus_requested: false });
    }

    fn first_run(&self) -> bool {
        matches!(self.conn, Conn::Disconnected) && self.settings.recent.is_empty()
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
        if matches!(self.conn, Conn::Connecting(_) | Conn::Connected { .. }) {
            self.net.disconnect();
        }
    }

    pub(super) fn empty_state(&mut self, ui: &mut egui::Ui) {
        if let Conn::Connecting(login) = &self.conn {
            let (name, address) = (self.server_name(), login.address.clone());
            let mut cancel = false;
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.36).max(20.0));
                ui.add(egui::Spinner::new().size(22.0).color(theme::ACCENT_TEXT));
                ui.add_space(10.0);
                ui.label(RichText::new(format!("Connecting to {name}")).size(17.0).color(theme::TEXT));
                ui.add_space(2.0);
                ui.label(RichText::new(address).color(theme::TEXT_FAINT));
                ui.add_space(14.0);
                cancel = ui.add(Btn::secondary("Cancel")).clicked();
            });
            if cancel {
                self.disconnect();
            }
        } else if self.first_run() {
            self.first_run_form(ui);
        } else {
            self.join_a_server(ui);
        }
    }

    fn join_a_server(&mut self, ui: &mut egui::Ui) {
        let (mut connect_to, mut open_form) = (None, false);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(20.0));
            logo(ui, 56.0);
            ui.add_space(14.0);
            ui.label(RichText::new("Join a server").size(17.0).color(theme::TEXT));
            ui.add_space(2.0);
            let note = "Enter an address a friend gave you, or pick one you've used before.";
            ui.label(RichText::new(note).color(theme::TEXT_DIM));
            if let Some(err) = &self.last_error {
                ui.add_space(8.0);
                ui.label(RichText::new(err).color(theme::RED));
            }
            ui.add_space(16.0);
            open_form = ui.add(Btn::primary("Connect to a server").min_width(FORM_W)).on_hover_text("Ctrl+S").clicked();
            ui.add_space(4.0);
            for r in &self.settings.recent {
                if recent_card(ui, r).clicked() {
                    connect_to = Some(r.address.clone());
                }
            }
        });
        if open_form {
            self.open_connect_form();
        }
        if let Some(address) = connect_to {
            let nick = self.settings.nickname.clone();
            self.connect(address, nick, String::new());
        }
    }

    fn first_run_form(&mut self, ui: &mut egui::Ui) {
        self.open_connect_form();
        let error = self.last_error.as_deref();
        let form = self.form.as_mut().expect("form open");
        let mut submit = false;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.16).max(20.0));
            logo(ui, 56.0);
            ui.add_space(14.0);
            ui.label(RichText::new("Welcome to T7").size(17.0).color(theme::TEXT));
            ui.add_space(2.0);
            ui.label(RichText::new("Pick a nickname and enter the address of a server.").color(theme::TEXT_DIM));
            ui.add_space(14.0);
            ui.allocate_ui_with_layout(vec2(FORM_W, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(FORM_W);
                form_fields(ui, form, true, error);
                ui.add_space(8.0);
                let ready = form.ready();
                let button = ui.add_enabled(ready, Btn::primary("Connect").min_width(FORM_W));
                submit = ready && (button.clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)));
            });
        });
        if submit {
            let f = self.form.take().expect("form open");
            self.connect(f.address, f.nickname, f.password);
        }
    }

    pub(super) fn connect_dialog(&mut self, ctx: &egui::Context) {
        if self.first_run() {
            return;
        }
        let error = self.last_error.as_deref();
        let Some(form) = &mut self.form else { return };
        let mut submit = false;
        let mut cancel = false;
        let modal = theme::modal("connect").show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new("Connect to a server").size(17.0).color(theme::TEXT));
            ui.add_space(14.0);
            form_fields(ui, form, false, error);
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = form.ready();
            }
            dialog_footer(ui, |ui| {
                if ui.add_enabled(form.ready(), Btn::primary("Connect")).clicked() {
                    submit = true;
                }
                if ui.add(Btn::secondary("Cancel")).clicked() {
                    cancel = true;
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new("Ctrl+S").size(11.5).color(theme::TEXT_FAINT));
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
            self.last_error = None;
        }
    }

    pub(super) fn cert_change_dialog(&mut self, ctx: &egui::Context) {
        let Some(change) = &self.cert_change else { return };
        let (mut trust, mut cancel) = (false, false);
        let modal = theme::modal("cert-change").show(ctx, |ui| {
            ui.set_width(480.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                icons::paint(ui.painter(), rect, Icon::Warning, theme::AMBER);
                let title = format!("{} has a new certificate", change.address);
                ui.label(RichText::new(title).size(16.0).color(theme::TEXT));
            });
            ui.add_space(6.0);
            let note = "It shows a different certificate than last time. That's expected if its admin \
                        reinstalled or moved the server. Otherwise someone may be intercepting the \
                        connection. If you're unsure, ask the admin for the fingerprint the server prints \
                        in its log.";
            ui.label(RichText::new(note).size(12.5).color(theme::TEXT_DIM));
            for (title, fingerprint, color, outline) in [
                ("Saved", change.pinned, theme::TEXT_DIM, theme::BORDER),
                ("New", change.presented, theme::TEXT, theme::AMBER),
            ] {
                ui.add_space(14.0);
                ui.label(RichText::new(title.to_uppercase()).size(10.5).strong().color(theme::TEXT_FAINT));
                ui.add_space(2.0);
                egui::Frame::new()
                    .fill(theme::BG)
                    .stroke(Stroke::new(1.0, outline))
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(two_lines(fingerprint)).font(FontId::monospace(12.0)).color(color));
                    });
            }
            dialog_footer(ui, |ui| {
                if ui.add(Btn::primary("Don't connect")).clicked() {
                    cancel = true;
                }
                if ui.add(Btn::secondary("Trust and connect")).clicked() {
                    trust = true;
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
