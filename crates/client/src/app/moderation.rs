//! Kicks, bans and pokes: their dialogs, and the notices when they happen.

use egui::{CornerRadius, Margin, RichText, Stroke};
use t7_proto::{C2S, ClientId, MAX_REASON_LEN};

use super::chat::LineKind;
use super::widgets::{dialog_footer, dialog_header, section, text_input};
use super::{App, default_channel_name};
use crate::sounds::Sound;
use crate::theme;
use crate::theme::Btn;

pub(super) struct PokeForm {
    target: ClientId,
    name: String,
    message: String,
    focus_requested: bool,
}

/// The moderation dialog on screen, if any.
pub(super) enum Moderation {
    Kick { target: ClientId, name: String, from_server: bool, reason: String, focused: bool },
    Ban { target: ClientId, name: String, reason: String, length: Option<u64>, include_ip: bool, focused: bool },
}

/// The ban lengths to pick from: (label, seconds; `None` for good).
const BAN_LENGTHS: [(Option<u64>, &str); 4] =
    [(Some(3600), "1 hour"), (Some(86_400), "1 day"), (Some(7 * 86_400), "1 week"), (None, "Permanent")];

/// A poke someone sent us, waiting to be acknowledged.
pub(super) struct PokeAlert {
    pub(super) from: String,
    pub(super) message: String,
    pub(super) time: String,
}

fn reason_field(ui: &mut egui::Ui, reason: &mut String, focused: &mut bool) {
    let edit = ui.add(text_input(reason).hint_text("Reason (optional)").char_limit(MAX_REASON_LEN));
    if !*focused {
        edit.request_focus();
        *focused = true;
    }
}

impl App {
    pub(super) fn on_kicked(&mut self, id: ClientId, by: ClientId, from_server: bool, reason: &str) {
        let Some(view) = &self.view else { return };
        let name = |id| view.nickname(id).unwrap_or("Someone").to_owned();
        let (target, actor) = (name(id), name(by));
        let Some(channel) = view.clients.get(&id).map(|c| c.channel) else { return };
        let here = view.my_channel() == Some(channel);
        let my_id = view.my_id;
        let why = if reason.is_empty() { String::new() } else { format!(": {reason}") };
        let place = if from_server {
            "the server".to_owned()
        } else if here {
            "the channel".to_owned()
        } else {
            self.channel_name(channel)
        };
        if id == my_id {
            let line = format!("{actor} kicked you from {place}{why}");
            if from_server {
                self.kicked = Some(line.clone());
            }
            self.push(usize::from(!from_server), LineKind::Error, line);
            return;
        }
        let line = format!("{target} was kicked from {place} by {actor}{why}");
        if here {
            self.push(1, LineKind::System, line.clone());
        }
        if from_server || (!here && by == my_id) {
            self.push(0, LineKind::System, line);
        }
    }

    pub(super) fn on_poked(&mut self, from: ClientId, message: String) {
        let from = self.view.as_ref().and_then(|v| v.nickname(from)).unwrap_or("Someone").to_owned();
        let line =
            if message.is_empty() { format!("{from} poked you") } else { format!("{from} poked you: {message}") };
        self.push(0, LineKind::System, line);
        self.sound(Sound::Poke);
        let time = chrono::Local::now().format("%H:%M").to_string();
        self.pokes.push_back(PokeAlert { from, message, time });
        self.attention = true;
    }

    pub(super) fn open_kick(&mut self, target: ClientId, from_server: bool) {
        let Some(name) = self.view.as_ref().and_then(|v| v.nickname(target)) else { return };
        let name = name.to_owned();
        self.moderation = Some(Moderation::Kick { target, name, from_server, reason: String::new(), focused: false });
    }

    pub(super) fn open_ban(&mut self, target: ClientId) {
        let Some(name) = self.view.as_ref().and_then(|v| v.nickname(target)) else { return };
        let name = name.to_owned();
        let length = BAN_LENGTHS[1].0;
        self.moderation =
            Some(Moderation::Ban { target, name, reason: String::new(), length, include_ip: true, focused: false });
    }

    pub(super) fn open_poke(&mut self, target: ClientId) {
        let Some(name) = self.view.as_ref().and_then(|v| v.nickname(target)) else { return };
        self.poke_form =
            Some(PokeForm { target, name: name.to_owned(), message: String::new(), focus_requested: false });
    }

    pub(super) fn poke_dialog(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.poke_form else { return };
        let (mut send, mut cancel) = (false, false);
        let modal = theme::modal("poke").show(ctx, |ui| {
            ui.set_width(340.0);
            dialog_header(ui, &format!("Poke {}", form.name), "They get a popup and a sound.");
            let edit = ui
                .add(text_input(&mut form.message).hint_text("Message (optional)").char_limit(t7_proto::MAX_POKE_LEN));
            if !form.focus_requested {
                edit.request_focus();
                form.focus_requested = true;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                send = true;
            }
            dialog_footer(ui, |ui| {
                if ui.add(Btn::primary("Poke")).clicked() {
                    send = true;
                }
                if ui.add(Btn::secondary("Cancel")).clicked() {
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

    pub(super) fn moderation_dialog(&mut self, ctx: &egui::Context) {
        // A dialog about someone who has left closes by itself.
        let online = |id: &ClientId| self.view.as_ref().is_some_and(|v| v.clients.contains_key(id));
        let gone = match &self.moderation {
            Some(Moderation::Kick { target, .. } | Moderation::Ban { target, .. }) => !online(target),
            None => false,
        };
        if gone {
            self.moderation = None;
        }
        let (Some(dialog), Some(view)) = (&mut self.moderation, &self.view) else { return };
        let default_name = default_channel_name(view);
        let mut send: Option<C2S> = None;
        let mut cancel = false;
        let modal = theme::modal("moderation").show(ctx, |ui| {
            ui.set_width(360.0);
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            match dialog {
                Moderation::Kick { target, name, from_server, reason, focused } => {
                    let (title, note) = if *from_server {
                        (format!("Kick {name} from the server"), "They can connect again right away.".to_owned())
                    } else {
                        (
                            format!("Kick {name} from the channel"),
                            format!("They move to {default_name}, or the first open channel if it's full or locked."),
                        )
                    };
                    dialog_header(ui, &title, &note);
                    reason_field(ui, reason, focused);
                    dialog_footer(ui, |ui| {
                        if ui.add(Btn::danger("Kick")).clicked() || enter {
                            let reason = reason.trim().to_owned();
                            send = Some(C2S::Kick { target: *target, from_server: *from_server, reason });
                        }
                        if ui.add(Btn::secondary("Cancel")).clicked() {
                            cancel = true;
                        }
                    });
                }
                Moderation::Ban { target, name, reason, length, include_ip, focused } => {
                    dialog_header(
                        ui,
                        &format!("Ban {name}"),
                        "They are kicked off the server and can't connect again until the ban ends.",
                    );
                    reason_field(ui, reason, focused);
                    section(ui, "Length");
                    theme::segmented(ui, length, &BAN_LENGTHS);
                    ui.add_space(8.0);
                    theme::switch(ui, include_ip, "Also ban their IP address").on_hover_text(
                        "Also kicks off and keeps out everyone else at their address, under any identity",
                    );
                    dialog_footer(ui, |ui| {
                        if ui.add(Btn::danger("Ban")).clicked() || enter {
                            let reason = reason.trim().to_owned();
                            send = Some(C2S::Ban {
                                target: *target,
                                duration_secs: *length,
                                reason,
                                include_ip: *include_ip,
                            });
                        }
                        if ui.add(Btn::secondary("Cancel")).clicked() {
                            cancel = true;
                        }
                    });
                }
            }
        });
        if let Some(msg) = send {
            self.net.send(msg);
            self.moderation = None;
        } else if cancel || modal.should_close() {
            self.moderation = None;
        }
    }

    pub(super) fn poke_alert(&mut self, ctx: &egui::Context) {
        let Some(alert) = self.pokes.front() else { return };
        let more = self.pokes.len() - 1;
        let mut ok = false;
        let modal = theme::modal("poked").show(ctx, |ui| {
            ui.set_width(340.0);
            dialog_header(ui, &format!("{} poked you", alert.from), &alert.time);
            if !alert.message.is_empty() {
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(Stroke::new(1.0, theme::BORDER))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&alert.message).size(14.0).color(theme::TEXT));
                    });
            }
            dialog_footer(ui, |ui| {
                let label = if more > 0 { format!("OK ({more} more)") } else { "OK".to_owned() };
                if ui.add(Btn::primary(&label)).clicked() {
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
