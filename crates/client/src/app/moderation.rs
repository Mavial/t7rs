use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Align2, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::{C2S, ChannelId, ClientId, MAX_REASON_LEN};

use super::chat::LineKind;
use super::sidebar::HEADER_H;
use super::widgets::{
    described_switch, dialog_footer, field, icon_button, paint_avatar, person_header, placeholder, section, text_input,
};
use super::{App, Lost, default_channel_name};
use crate::icons::Icon;
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

struct BanLength {
    duration_secs: Option<u64>,
    label: &'static str,
}

const BAN_LENGTHS: [BanLength; 4] = [
    BanLength { duration_secs: Some(3600), label: "1 hour" },
    BanLength { duration_secs: Some(86_400), label: "1 day" },
    BanLength { duration_secs: Some(7 * 86_400), label: "1 week" },
    BanLength { duration_secs: None, label: "Permanent" },
];

fn ban_button(length: Option<u64>) -> String {
    match BAN_LENGTHS.iter().find(|l| l.duration_secs == length) {
        Some(BanLength { duration_secs: Some(_), label }) => format!("Ban for {label}"),
        _ => "Ban permanently".to_owned(),
    }
}

pub(super) struct PokeAlert {
    pub(super) from: ClientId,
    pub(super) name: String,
    pub(super) message: String,
    pub(super) time: String,
}

const POKES_SHOWN: usize = 3;

struct CardTitle {
    newest_poker: String,
    rest: String,
}

fn card_title(pokes: &[PokeAlert]) -> CardTitle {
    let mut names: Vec<&str> = Vec::new();
    for p in pokes.iter().rev() {
        if !names.contains(&p.name.as_str()) {
            names.push(&p.name);
        }
    }
    let rest = match (names.len(), pokes.len()) {
        (1, 1) => " poked you".to_owned(),
        (1, n) => format!(" poked you {n} times"),
        (2, _) => format!(" and {} poked you", names[1]),
        (n, _) => format!(" and {} others poked you", n - 1),
    };
    CardTitle { newest_poker: names[0].to_owned(), rest }
}

fn channel_to_join(view: &ServerView, poker: ClientId) -> Option<(ChannelId, String)> {
    let channel = view.clients.get(&poker)?.channel;
    let name = &view.channels.get(&channel)?.name;
    (view.my_channel() != Some(channel)).then(|| (channel, name.clone()))
}

enum CardAction {
    Dismiss,
    Reply(ClientId),
    Join(ChannelId),
}

fn reason_field(ui: &mut egui::Ui, reason: &mut String, focused: &mut bool) {
    let edit = text_input(reason).hint_text(placeholder("Optional, they see it")).char_limit(MAX_REASON_LEN);
    let edit = field(ui, "Reason", edit);
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
                let quoted = if reason.is_empty() { String::new() } else { format!(": \"{reason}\"") };
                let detail = format!("by {actor}{quoted}");
                self.kicked = Some(Lost { headline: "Kicked from the server".into(), detail });
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
        let name = self.view.as_ref().and_then(|v| v.nickname(from)).unwrap_or("Someone").to_owned();
        let line =
            if message.is_empty() { format!("{name} poked you") } else { format!("{name} poked you: {message}") };
        self.push(0, LineKind::System, line);
        self.sound(Sound::Poke);
        let time = chrono::Local::now().format("%H:%M").to_string();
        self.pokes.push_back(PokeAlert { from, name, message, time });
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
        let length = BAN_LENGTHS[1].duration_secs;
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
            ui.set_width(400.0);
            person_header(ui, &form.name, &format!("Poke {}", form.name), "They get a notification and a sound.");
            let edit =
                text_input(&mut form.message).hint_text(placeholder("Optional")).char_limit(t7_proto::MAX_POKE_LEN);
            let edit = field(ui, "Message", edit);
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
            ui.set_width(440.0);
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            match dialog {
                Moderation::Kick { target, name, from_server, reason, focused } => {
                    let (title, note) = if *from_server {
                        (format!("Kick {name} from the server"), "They can connect again right away.".to_owned())
                    } else {
                        let channel = view.clients.get(target).and_then(|c| view.channels.get(&c.channel));
                        let place = channel.map_or_else(|| "the channel".to_owned(), |c| c.name.clone());
                        (
                            format!("Kick {name} from {place}"),
                            format!(
                                "They move to {default_name}, or to the first open channel if it's full or locked."
                            ),
                        )
                    };
                    person_header(ui, name, &title, &note);
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
                    person_header(
                        ui,
                        name,
                        &format!("Ban {name}"),
                        "They are kicked off the server and can't connect again until the ban ends.",
                    );
                    reason_field(ui, reason, focused);
                    section(ui, "Length");
                    let lengths = BAN_LENGTHS.map(|l| (l.duration_secs, l.label));
                    theme::segmented(ui, length, &lengths);
                    ui.add_space(14.0);
                    described_switch(
                        ui,
                        include_ip,
                        "Also ban their IP address",
                        "Stops them making a new identity. Can also block others on the same network.",
                    );
                    dialog_footer(ui, |ui| {
                        if ui.add(Btn::danger(&ban_button(*length))).clicked() || enter {
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

    pub(super) fn poke_card(&mut self, ctx: &egui::Context) {
        let pokes = &*self.pokes.make_contiguous();
        let Some(newest) = pokes.last() else { return };
        let online = self.view.as_ref().is_some_and(|v| v.clients.contains_key(&newest.from));
        let join = self.view.as_ref().and_then(|v| channel_to_join(v, newest.from));
        let title = card_title(pokes);
        let several = pokes.len() > 1;
        let mut action = None;
        let frame = egui::Frame::new()
            .fill(theme::PANEL)
            .stroke(Stroke::new(1.0, theme::BORDER_STRONG))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(14))
            .shadow(ctx.global_style().visuals.popup_shadow);
        egui::Area::new(egui::Id::unique("poke-card"))
            .order(egui::Order::Middle)
            .anchor(Align2::RIGHT_TOP, vec2(-16.0, HEADER_H + 12.0))
            .fade_in(false)
            .show(ctx, |ui| {
                frame.show(ui, |ui| {
                    ui.set_width(300.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                        paint_avatar(ui.painter(), rect.center(), 14.0, &newest.name);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.set_width(ui.available_width() - 40.0);
                            let mut job = LayoutJob::default();
                            let fmt =
                                |color| TextFormat { font_id: FontId::proportional(13.5), color, ..Default::default() };
                            job.append(&title.newest_poker, 0.0, fmt(theme::TEXT));
                            job.append(&title.rest, 0.0, fmt(theme::TEXT_DIM));
                            ui.label(job);
                            ui.label(RichText::new(&newest.time).size(11.5).color(theme::TEXT_FAINT));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            if icon_button(ui, Icon::Cross, "Dismiss").clicked() {
                                action = Some(CardAction::Dismiss);
                            }
                        });
                    });
                    let shown = pokes.iter().rev().take(POKES_SHOWN);
                    for poke in shown.filter(|p| several || !p.message.is_empty()) {
                        ui.add_space(6.0);
                        poke_line(ui, poke, several);
                    }
                    let older = pokes.len().saturating_sub(POKES_SHOWN);
                    if older > 0 {
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("and {older} earlier")).size(11.5).color(theme::TEXT_FAINT));
                    }
                    if online {
                        ui.add_space(10.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some((channel, name)) = &join
                                && ui.add(Btn::primary(&format!("Join {name}")).small()).clicked()
                            {
                                action = Some(CardAction::Join(*channel));
                            }
                            if ui.add(Btn::secondary("Reply").small()).clicked() {
                                action = Some(CardAction::Reply(newest.from));
                            }
                        });
                    }
                });
            });
        let Some(action) = action else { return };
        self.pokes.clear();
        match action {
            CardAction::Dismiss => {}
            CardAction::Reply(id) => self.open_private(id),
            CardAction::Join(channel) => self.join_channel(channel),
        }
    }
}

fn poke_line(ui: &mut egui::Ui, poke: &PokeAlert, named: bool) {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            if named {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(&poke.name).size(12.5).color(theme::TEXT));
                    ui.label(RichText::new(&poke.time).size(11.5).color(theme::TEXT_FAINT));
                });
            }
            if !poke.message.is_empty() {
                ui.label(RichText::new(&poke.message).color(theme::TEXT));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poke(from: ClientId, name: &str) -> PokeAlert {
        PokeAlert { from, name: name.into(), message: String::new(), time: "20:21".into() }
    }

    #[test]
    fn the_card_names_the_newest_poker_and_counts_the_rest() {
        let title = |pokes: &[PokeAlert]| {
            let title = card_title(pokes);
            title.newest_poker + &title.rest
        };
        assert_eq!(title(&[poke(2, "Marta")]), "Marta poked you");
        assert_eq!(title(&[poke(2, "Marta"), poke(2, "Marta")]), "Marta poked you 2 times");
        assert_eq!(title(&[poke(3, "Kenji"), poke(2, "Marta")]), "Marta and Kenji poked you");
        let three = [poke(4, "Ola"), poke(3, "Kenji"), poke(2, "Marta"), poke(3, "Kenji")];
        assert_eq!(title(&three), "Kenji and 2 others poked you");
    }

    #[test]
    fn the_ban_button_says_how_long() {
        assert_eq!(ban_button(Some(86_400)), "Ban for 1 day");
        assert_eq!(ban_button(None), "Ban permanently");
    }
}
