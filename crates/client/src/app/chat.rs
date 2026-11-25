//! The main pane on a server: a header with where you are and the chat tabs,
//! a strip while someone in your channel shares their screen, the chat, and
//! the box you type in.

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, CornerRadius, FontId, Margin, Rect, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::{C2S, ChannelKind, ClientId, TextTarget};

use super::sidebar::HEADER_H;
use super::widgets::{paint_avatar, truncated};
use super::{App, Conn, sharing};
use crate::icons::{self, Icon};
use crate::theme::{self, Btn};

const MAX_LINES: usize = 1000;
const GROUP_GAP: TimeDelta = TimeDelta::minutes(5);
const TEXT_X: f32 = 44.0;

pub(super) enum LineKind {
    System,
    Error,
    Msg { from: ClientId, name: String },
}

pub(super) struct Line {
    pub(super) at: NaiveDateTime,
    pub(super) time: String,
    pub(super) kind: LineKind,
    pub(super) text: String,
}

impl Line {
    pub(super) fn new(at: NaiveDateTime, kind: LineKind, text: String) -> Self {
        Self { at, time: at.format("%H:%M").to_string(), kind, text }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum TabKind {
    Server,
    Channel,
    Private(ClientId),
}

pub(super) struct Tab {
    pub(super) kind: TabKind,
    pub(super) title: String,
    pub(super) lines: Vec<Line>,
    pub(super) unread: u32,
}

impl Tab {
    fn label(&self) -> String {
        match self.kind {
            TabKind::Server => "Server".to_owned(),
            TabKind::Channel => format!("# {}", self.title),
            TabKind::Private(_) => format!("@ {}", self.title),
        }
    }
}

pub(super) fn base_tabs() -> Vec<Tab> {
    vec![
        Tab { kind: TabKind::Server, title: "Server".into(), lines: Vec::new(), unread: 0 },
        Tab { kind: TabKind::Channel, title: "Channel".into(), lines: Vec::new(), unread: 0 },
    ]
}

pub(super) enum Entry<'a> {
    Day(NaiveDate),
    Said { name: &'a str, lines: &'a [Line] },
    Note(&'a Line),
}

pub(super) fn entries(lines: &[Line]) -> impl Iterator<Item = Entry<'_>> {
    let mut i = 0;
    let mut day = None;
    std::iter::from_fn(move || {
        let line = lines.get(i)?;
        let date = line.at.date();
        if day != Some(date) {
            day = Some(date);
            return Some(Entry::Day(date));
        }
        let LineKind::Msg { from, name } = &line.kind else {
            i += 1;
            return Some(Entry::Note(line));
        };
        let start = i;
        i += 1;
        while let Some(next) = lines.get(i)
            && matches!(&next.kind, LineKind::Msg { from: f, .. } if f == from)
            && next.at.date() == date
            && next.at - lines[i - 1].at < GROUP_GAP
        {
            i += 1;
        }
        Some(Entry::Said { name, lines: &lines[start..i] })
    })
}

fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    match (today - day).num_days() {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => day.format("%A, %-d %B").to_string(),
    }
}

fn tab_width(ui: &egui::Ui, tab: &Tab, active: bool) -> f32 {
    let label = ui.painter().layout_no_wrap(tab.label(), FontId::proportional(12.5), theme::TEXT);
    let pill = (tab.unread > 0).then(|| unread_pill(ui, tab.unread).size().x + 10.0);
    let close = active && matches!(tab.kind, TabKind::Private(_));
    20.0 + label.size().x + pill.map_or(0.0, |w| w.max(15.0) + 6.0) + if close { 17.0 } else { 0.0 }
}

fn unread_pill(ui: &egui::Ui, unread: u32) -> std::sync::Arc<egui::Galley> {
    let text = if unread > 99 { "99+".to_owned() } else { unread.to_string() };
    ui.painter().layout_no_wrap(text, FontId::proportional(10.5), theme::ON_ACCENT)
}

fn tab_button(ui: &mut egui::Ui, tab: &Tab, active: bool) -> (egui::Response, bool) {
    let width = tab_width(ui, tab, active);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 28.0), Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter();
    if active {
        p.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::SURFACE_ACTIVE);
    } else if hovered {
        p.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::SURFACE_HOVER);
    }
    let color = if active || hovered { theme::TEXT } else { theme::TEXT_DIM };
    let label = p.layout_no_wrap(tab.label(), FontId::proportional(12.5), color);
    let mut x = rect.left() + 10.0;
    let label_w = label.size().x;
    p.galley(egui::pos2(x, rect.center().y - label.size().y / 2.0), label, color);
    x += label_w;
    if tab.unread > 0 {
        let count = unread_pill(ui, tab.unread);
        let pill_w = (count.size().x + 10.0).max(15.0);
        let pill = Rect::from_min_size(egui::pos2(x + 6.0, rect.center().y - 7.5), vec2(pill_w, 15.0));
        p.rect_filled(pill, CornerRadius::same(8), theme::ACCENT);
        p.galley(pill.center() - count.size() / 2.0, count, theme::ON_ACCENT);
        x = pill.right();
    }
    let mut closed = false;
    if active && matches!(tab.kind, TabKind::Private(_)) {
        let close = Rect::from_center_size(egui::pos2(x + 11.0, rect.center().y), vec2(16.0, 16.0));
        let close_resp = ui.interact(close, resp.id.with("close"), Sense::click());
        let color = if close_resp.hovered() { theme::TEXT } else { theme::TEXT_FAINT };
        icons::paint(ui.painter(), close.shrink(2.5), Icon::Cross, color);
        closed = close_resp.on_hover_text("Close").clicked();
    }
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), closed)
}

fn kbd(ui: &mut egui::Ui, key: &str) {
    let galley = ui.painter().layout_no_wrap(key.to_owned(), FontId::monospace(11.0), theme::TEXT_FAINT);
    let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(8.0, 2.0), Sense::hover());
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, theme::BORDER_STRONG),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, theme::TEXT_FAINT);
}

impl App {
    pub(super) fn open_private(&mut self, id: ClientId) {
        let idx = self.private_tab(id);
        self.active_tab = idx;
        self.tabs[idx].unread = 0;
        self.focus_input = true;
    }

    fn send_chat(&mut self) {
        let text = self.input.trim().to_owned();
        if text.is_empty() || !matches!(self.conn, Conn::Connected { .. }) {
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

    pub(super) fn push(&mut self, tab: usize, kind: LineKind, text: impl Into<String>) {
        let t = &mut self.tabs[tab];
        t.lines.push(Line::new(chrono::Local::now().naive_local(), kind, text.into()));
        if t.lines.len() > MAX_LINES {
            t.lines.drain(..t.lines.len() - MAX_LINES);
        }
        if tab != self.active_tab {
            t.unread += 1;
        }
    }

    pub(super) fn private_tab(&mut self, id: ClientId) -> usize {
        if let Some(i) = self.tabs.iter().position(|t| t.kind == TabKind::Private(id)) {
            return i;
        }
        let title = self.view.as_ref().and_then(|v| v.nickname(id)).unwrap_or("unknown").to_owned();
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

    pub(super) fn sync_channel_tab_title(&mut self) {
        if let Some(ch) = self.view.as_ref().and_then(ServerView::my_channel) {
            self.tabs[1].title = self.channel_name(ch);
        }
    }

    pub(super) fn main_pane(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG)).show(ui, |ui| {
            if self.shown_view().is_none() {
                egui::Frame::new().inner_margin(Margin::symmetric(16, 12)).show(ui, |ui| self.empty_state(ui));
                return;
            }
            self.pane_header(ui);
            self.live_strip(ui);
            self.tabs[self.active_tab].unread = 0;
            self.composer(ui);
            self.chat_log(ui);
        });
    }

    fn pane_header(&mut self, ui: &mut egui::Ui) {
        let (mut open, mut close) = (None, None);
        let frame = egui::Frame::new().inner_margin(Margin { left: 20, right: 12, top: 0, bottom: 0 });
        egui::Panel::top("pane-header").frame(frame).exact_size(HEADER_H).show(ui, |ui| {
            let gap = 2.0;
            let tabs_w: f32 =
                self.tabs.iter().enumerate().map(|(i, t)| tab_width(ui, t, i == self.active_tab) + gap).sum();
            let full = ui.available_width();
            let tabs_room = tabs_w.min((full - 140.0).max(0.0));
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (title, _) = ui.allocate_exact_size(vec2(full - tabs_room, 28.0), Sense::hover());
                self.paint_title(ui, title.with_max_x(title.right() - 16.0));
                let mut tabs = |ui: &mut egui::Ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let (resp, closed) = tab_button(ui, tab, i == self.active_tab);
                        if resp.clicked() {
                            open = Some(i);
                        }
                        if matches!(tab.kind, TabKind::Private(_)) {
                            if closed || resp.middle_clicked() {
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
                };
                if tabs_room < tabs_w {
                    let scroll = egui::ScrollArea::horizontal().max_width(tabs_room);
                    scroll.scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
                        ui.horizontal(&mut tabs);
                    });
                } else {
                    tabs(ui);
                }
            });
        });
        if let Some(i) = open {
            self.active_tab = i;
            self.focus_input = true;
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn paint_title(&self, ui: &egui::Ui, rect: Rect) {
        let Some(view) = self.shown_view() else { return };
        let p = ui.painter();
        let y = rect.center().y;
        let (name, aside) = match self.tabs[self.active_tab].kind {
            TabKind::Private(id) => {
                let title = &self.tabs[self.active_tab].title;
                paint_avatar(p, egui::pos2(rect.left() + 11.0, y), 11.0, title);
                (title.clone(), whereabouts(view, id))
            }
            TabKind::Server | TabKind::Channel => {
                let ch = view.my_channel().and_then(|id| view.channels.get(&id));
                let lobby = ch.is_some_and(|c| c.kind == ChannelKind::Lobby);
                let icon = if lobby { Icon::SpeakerPlus } else { Icon::Speaker };
                let icon_rect = Rect::from_center_size(egui::pos2(rect.left() + 8.0, y), vec2(16.0, 16.0));
                icons::paint(p, icon_rect, icon, theme::ACCENT_TEXT);
                (self.tabs[1].title.clone(), ch.map_or_else(String::new, |c| c.topic.clone()))
            }
        };
        let x = rect.left() + 26.0;
        let name = truncated(ui, &name, FontId::proportional(14.0), theme::TEXT, rect.right() - x);
        let name_w = name.size().x;
        p.galley(egui::pos2(x, y - name.size().y / 2.0), name, theme::TEXT);
        let aside_x = x + name_w + 10.0;
        if !aside.is_empty() && rect.right() - aside_x > 40.0 {
            let aside = truncated(ui, &aside, FontId::proportional(12.5), theme::TEXT_FAINT, rect.right() - aside_x);
            p.galley(egui::pos2(aside_x, y - aside.size().y / 2.0), aside, theme::TEXT_FAINT);
        }
    }

    fn live_strip(&mut self, ui: &mut egui::Ui) {
        let Some(view) = &self.view else { return };
        let Some(channel) = view.my_channel() else { return };
        let sharers: Vec<(ClientId, String)> = (view.clients_in(channel).into_iter())
            .filter(|c| c.sharing && c.id != view.my_id)
            .map(|c| (c.id, c.nickname.clone()))
            .collect();
        if sharers.is_empty() {
            return;
        }
        let watched = self.watching.as_ref().map(|w| w.sharer);
        let mut picked = None;
        let frame = egui::Frame::new().inner_margin(Margin { left: 20, right: 12, top: 6, bottom: 6 });
        egui::Panel::top("live").frame(frame).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (id, name) in &sharers {
                let watching = watched == Some(*id);
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
                let p = ui.painter();
                let y = rect.center().y;
                paint_avatar(p, egui::pos2(rect.left() + 11.0, y), 11.0, name);
                let mut x = rect.left() + 32.0;
                if !watching {
                    x = sharing::live_badge(p, egui::pos2(x, y)).right() + 8.0;
                }
                let (before, after) = if watching {
                    ("You're watching ", " in its own window")
                } else {
                    ("", " is sharing their screen")
                };
                let mut job = LayoutJob::default();
                let fmt = |color| TextFormat { font_id: FontId::proportional(12.5), color, ..Default::default() };
                job.append(before, 0.0, fmt(theme::TEXT_DIM));
                job.append(name, 0.0, fmt(theme::TEXT));
                job.append(after, 0.0, fmt(theme::TEXT_DIM));
                let button =
                    if watching { Btn::secondary("Stop watching").small() } else { Btn::primary("Watch").small() };
                job.wrap = egui::text::TextWrapping::truncate_at_width(rect.right() - x - 120.0);
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(egui::pos2(x, y - galley.size().y / 2.0), galley, theme::TEXT);
                let slot = Rect::from_min_max(egui::pos2(rect.right() - 120.0, rect.top()), rect.right_bottom());
                let resp = ui.put(slot, |ui: &mut egui::Ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.add(button)).inner
                });
                if resp.clicked() {
                    picked = Some((*id, watching));
                }
            }
        });
        match picked {
            Some((_, true)) => self.stop_watching(),
            Some((id, false)) => self.watch(id),
            None => {}
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let tab = &self.tabs[self.active_tab];
        let hint = match tab.kind {
            TabKind::Server => "Message everyone on the server".to_owned(),
            TabKind::Channel => format!("Message #{}", tab.title),
            TabKind::Private(_) => format!("Message {}", tab.title),
        };
        let frame = egui::Frame::new().inner_margin(Margin { left: 20, right: 20, top: 4, bottom: 16 });
        if !matches!(self.conn, Conn::Connected { .. }) {
            egui::Panel::bottom("composer").frame(frame).show_separator_line(false).show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::hover());
                let p = ui.painter();
                p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
                let text_pos = egui::pos2(rect.left() + 12.0, rect.center().y);
                p.text(
                    text_pos,
                    Align2::LEFT_CENTER,
                    "You're not connected",
                    FontId::proportional(14.0),
                    theme::TEXT_FAINT,
                );
            });
            return;
        }
        egui::Panel::bottom("composer").frame(frame).show_separator_line(false).show(ui, |ui| {
            let field = egui::Frame::new()
                .fill(theme::SURFACE)
                .corner_radius(CornerRadius::same(8))
                .stroke(Stroke::new(1.0, theme::BORDER))
                .inner_margin(Margin { left: 12, right: 10, top: 3, bottom: 3 });
            field.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut self.input)
                        .hint_text(RichText::new(hint).color(theme::TEXT_FAINT))
                        .frame(egui::Frame::NONE)
                        .desired_width(ui.available_width() - 52.0)
                        .min_size(vec2(0.0, 34.0))
                        .align(Align2::LEFT_CENTER)
                        .char_limit(t7_proto::MAX_TEXT_LEN)
                        .font(FontId::proportional(14.0));
                    let resp = ui.add(edit);
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.send_chat();
                        self.focus_input = true;
                    }
                    if std::mem::take(&mut self.focus_input) {
                        resp.request_focus();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| kbd(ui, "Enter"));
                });
            });
        });
    }

    fn chat_log(&self, ui: &mut egui::Ui) {
        let tab = &self.tabs[self.active_tab];
        let spoken = tab.lines.iter().any(|l| !matches!(l.kind, LineKind::System));
        if !spoken && tab.kind != TabKind::Server {
            let (title, note) = match tab.kind {
                TabKind::Private(_) => (
                    format!("No messages with {} yet.", tab.title),
                    format!("Only {} sees what you write here.", tab.title),
                ),
                _ => (
                    format!("No messages in #{} yet.", tab.title),
                    "Messages here are seen by everyone in the channel.".to_owned(),
                ),
            };
            return empty_chat(ui, &title, &note);
        }
        let today = chrono::Local::now().date_naive();
        let scroll = egui::ScrollArea::vertical().id_salt(tab.kind).auto_shrink(false).stick_to_bottom(true);
        scroll.show(ui, |ui| {
            egui::Frame::new().inner_margin(Margin { left: 20, right: 20, top: 8, bottom: 8 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for entry in entries(&tab.lines) {
                    match entry {
                        Entry::Day(day) => day_separator(ui, &day_label(day, today)),
                        Entry::Said { name, lines } => said(ui, name, lines),
                        Entry::Note(line) => note(ui, line),
                    }
                }
            });
        });
    }
}

fn whereabouts(view: &ServerView, id: ClientId) -> String {
    let Some(c) = view.clients.get(&id) else { return "Offline".to_owned() };
    let channel = view.channels.get(&c.channel).map_or("?", |ch| ch.name.as_str());
    let mut parts = vec![format!("In {channel}")];
    if c.status.away {
        parts.push("away".to_owned());
    }
    if c.sharing {
        parts.push("sharing their screen".to_owned());
    }
    parts.join(" · ")
}

fn day_separator(ui: &mut egui::Ui, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    let p = ui.painter();
    let text = p.text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(11.0), theme::TEXT_FAINT);
    let stroke = Stroke::new(1.0, theme::BORDER);
    p.hline(rect.left()..=text.left() - 10.0, rect.center().y, stroke);
    p.hline(text.right() + 10.0..=rect.right(), rect.center().y, stroke);
}

fn said(ui: &mut egui::Ui, name: &str, lines: &[Line]) {
    let width = ui.available_width();
    let text_w = width - TEXT_X;
    let color = theme::avatar_color(name);
    let p = ui.painter();
    let header = p.layout_no_wrap(name.to_owned(), FontId::proportional(14.0), color);
    let time = p.layout_no_wrap(lines[0].time.clone(), FontId::proportional(11.0), theme::TEXT_FAINT);
    let texts: Vec<_> =
        lines.iter().map(|l| p.layout(l.text.clone(), FontId::proportional(14.0), theme::TEXT, text_w)).collect();
    let height = header.size().y + 2.0 + texts.iter().map(|g| g.size().y + 2.0).sum::<f32>();
    let (rect, _) = ui.allocate_exact_size(vec2(width, height + 12.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    let top = rect.top() + 6.0;
    paint_avatar(p, egui::pos2(rect.left() + 16.0, top + 16.0), 16.0, name);
    let x = rect.left() + TEXT_X;
    let header_h = header.size().y;
    let time_pos = egui::pos2(x + header.size().x + 8.0, top + header_h - time.size().y - 1.0);
    p.galley(egui::pos2(x, top), header, color);
    p.galley(time_pos, time, theme::TEXT_FAINT);
    let mut y = top + header_h + 2.0;
    for text in texts {
        let h = text.size().y;
        p.galley(egui::pos2(x, y), text, theme::TEXT);
        y += h + 2.0;
    }
}

fn note(ui: &mut egui::Ui, line: &Line) {
    let color = if matches!(line.kind, LineKind::Error) { theme::RED } else { theme::TEXT_FAINT };
    let mut job = LayoutJob::default();
    let fmt = TextFormat { font_id: FontId::proportional(12.5), color, ..Default::default() };
    job.append(&line.text, 0.0, fmt.clone());
    job.append(" · ", 0.0, fmt.clone());
    job.append(&line.time, 0.0, fmt);
    job.wrap.max_width = ui.available_width() - TEXT_X;
    let galley = ui.painter().layout_job(job);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), galley.size().y + 6.0), Sense::hover());
    ui.painter().galley(egui::pos2(rect.left() + TEXT_X, rect.top() + 3.0), galley, color);
}

fn empty_chat(ui: &mut egui::Ui, title: &str, note: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.38).max(20.0));
        let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
        icons::paint(ui.painter(), rect, Icon::Chat, theme::TEXT_FAINT);
        ui.add_space(4.0);
        ui.label(RichText::new(title).size(13.5).color(theme::TEXT_DIM));
        ui.label(RichText::new(note).size(12.0).color(theme::TEXT_FAINT));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(hour, minute, 0).unwrap()
    }

    fn msg(from: ClientId, name: &str) -> LineKind {
        LineKind::Msg { from, name: name.into() }
    }

    fn shape(lines: &[Line]) -> Vec<(&'static str, usize)> {
        entries(lines)
            .map(|e| match e {
                Entry::Day(_) => ("day", 0),
                Entry::Said { lines, .. } => ("said", lines.len()),
                Entry::Note(_) => ("note", 1),
            })
            .collect()
    }

    #[test]
    fn groups_one_persons_messages_in_a_row() {
        let lines = [
            Line::new(at(24, 20, 14), LineKind::System, "You joined Games".into()),
            Line::new(at(24, 20, 14), msg(2, "Marta"), "hey!".into()),
            Line::new(at(24, 20, 15), msg(3, "Kenji"), "my mic is acting up".into()),
            Line::new(at(24, 20, 15), msg(3, "Kenji"), "can someone check?".into()),
            Line::new(at(24, 20, 16), LineKind::Error, "Couldn't open the microphone".into()),
            Line::new(at(24, 20, 17), msg(3, "Kenji"), "back".into()),
        ];
        assert_eq!(shape(&lines), [("day", 0), ("note", 1), ("said", 1), ("said", 2), ("note", 1), ("said", 1)]);
    }

    #[test]
    fn a_pause_or_a_new_day_starts_a_new_group() {
        let lines = [
            Line::new(at(23, 23, 50), msg(2, "Marta"), "late".into()),
            Line::new(at(23, 23, 52), msg(2, "Marta"), "still here".into()),
            Line::new(at(23, 23, 58), msg(2, "Marta"), "after a pause".into()),
            Line::new(at(24, 0, 1), msg(2, "Marta"), "past midnight".into()),
        ];
        assert_eq!(shape(&lines), [("day", 0), ("said", 2), ("said", 1), ("day", 0), ("said", 1)]);
    }

    #[test]
    fn days_read_as_people_say_them() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(day_label(today.pred_opt().unwrap(), today), "Yesterday");
        assert_eq!(day_label(NaiveDate::from_ymd_opt(2026, 11, 20).unwrap(), today), "Friday, 20 November");
    }
}
