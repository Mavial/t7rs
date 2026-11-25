//! The main pane on a server: a header with where you are and the chat tabs,
//! the shelf while people in your channel share their screens, the chat, and
//! the box you type in.

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, CornerRadius, FontId, Margin, Rect, RichText, Sense, Stroke, vec2};
use t7_proto::files::ChannelFile;
use t7_proto::model::ServerView;
use t7_proto::{C2S, ChannelKind, ClientId, TextTarget};

use super::files::Files;
use super::markdown::Body;
use super::sidebar::HEADER_H;
use super::widgets::{paint_avatar, truncated};
use super::{App, Conn};
use crate::icons::{self, Icon};
use crate::theme::{self, Btn};

pub(super) const MAX_LINES: usize = 1000;
const GROUP_GAP: TimeDelta = TimeDelta::minutes(5);
const TEXT_X: f32 = 44.0;

pub(super) enum LineKind {
    System,
    Error,
    Msg { from: ClientId, name: String },
    File(ChannelFile),
}

pub(super) struct Line {
    pub(super) at: NaiveDateTime,
    pub(super) time: String,
    pub(super) kind: LineKind,
    body: Body,
}

impl Line {
    pub(super) fn new(at: NaiveDateTime, kind: LineKind, text: String) -> Self {
        let body = match &kind {
            LineKind::Msg { .. } => Body::parse(text),
            LineKind::File(file) => Body::parse(file.attachment.caption.clone()),
            _ => Body::plain(text),
        };
        Self { at, time: at.format("%H:%M").to_string(), kind, body }
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
    File(&'a ChannelFile, &'a str, &'a Body),
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
            return Some(match &line.kind {
                LineKind::File(file) => Entry::File(file, &line.time, &line.body),
                _ => Entry::Note(line),
            });
        };
        let start = i;
        i += 1;
        while let Some(next) = lines.get(i)
            && matches!(&next.kind, LineKind::Msg { from: f, .. } if f == from)
            && lines[i - 1].body.link.is_none()
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

struct TabButton {
    resp: egui::Response,
    close_clicked: bool,
}

fn tab_button(ui: &mut egui::Ui, tab: &Tab, active: bool) -> TabButton {
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
    TabButton { resp: resp.on_hover_cursor(egui::CursorIcon::PointingHand), close_clicked: closed }
}

fn send_button(ui: &mut egui::Ui, modifiers: egui::Modifiers) -> egui::Response {
    let (_, rect) = ui.allocate_space(vec2(32.0, 32.0));
    let response = ui.interact(rect, egui::Id::unique("chat-send"), Sense::click());
    let fill = if !response.enabled() {
        theme::SURFACE_ACTIVE
    } else if response.is_pointer_button_down_on() {
        theme::ACCENT_PRESSED
    } else if response.hovered() || response.has_focus() {
        theme::ACCENT_HOVER
    } else {
        theme::ACCENT
    };
    ui.painter().circle_filled(rect.center(), 14.0, fill);
    let ink = if response.enabled() { theme::ON_ACCENT } else { theme::TEXT_FAINT };
    icons::paint(ui.painter(), rect.shrink(7.0), Icon::ArrowUp, ink);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), "Send"));
    let shortcut = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(modifiers, egui::Key::Enter));
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Send ({shortcut})"))
}

impl App {
    pub(crate) fn prepare_chat_paste(&self, ctx: &egui::Context, events: &mut [egui::Event]) {
        let id = egui::Id::unique("chat-composer");
        for event in events {
            if let egui::Event::Paste(text) = event
                && ctx.memory(|m| m.has_focus(id))
                && !egui::TextEdit::load_state(ctx, id).and_then(|state| state.cursor.char_range()).is_some_and(
                    |range| super::markdown::in_code(&self.input, range.as_sorted_char_range().start.into()),
                )
                && let Some(markdown) = super::markdown::rich_paste(text)
            {
                *text = markdown;
            }
        }
    }

    pub(super) fn open_private(&mut self, id: ClientId) {
        let idx = self.private_tab(id);
        self.active_tab = idx;
        self.tabs[idx].unread = 0;
        self.focus_input = true;
    }

    fn send_chat(&mut self) {
        if !matches!(self.conn, Conn::Connected { .. }) || self.input.chars().count() > t7_proto::MAX_TEXT_LEN {
            return;
        }
        if self.tabs[self.active_tab].kind == TabKind::Channel && self.files.has_staged() {
            if self.files.queue_uploads(&self.input, &self.net) {
                self.input.clear();
            }
            return;
        }
        if self.input.trim().is_empty() {
            return;
        }
        let target = match self.tabs[self.active_tab].kind {
            TabKind::Server => TextTarget::Server,
            TabKind::Channel => TextTarget::Channel,
            TabKind::Private(id) => TextTarget::Client(id),
        };
        self.net.send(C2S::SendText { target, text: std::mem::take(&mut self.input) });
    }

    pub(super) fn push(&mut self, tab: usize, kind: LineKind, text: impl Into<String>) {
        let t = &mut self.tabs[tab];
        t.lines.push(Line::new(chrono::Local::now().naive_local(), kind, text.into()));
        trim_lines(&mut t.lines);
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
            self.files.transfer_rows(ui);
            if self.files.library_open
                && let Some(view) = &self.view
            {
                self.files.library(ui, view, &self.net);
                return;
            }
            self.shelf(ui);
            self.tabs[self.active_tab].unread = 0;
            if self.tabs[self.active_tab].kind == TabKind::Channel
                && self.files.drawer_open
                && let Some(view) = &self.view
                && let Some(channel) = self.files.drawer(ui, view, &self.net)
            {
                self.join_channel(channel);
            }
            self.composer(ui);
            self.chat_log(ui);
        });
    }

    fn pane_header(&mut self, ui: &mut egui::Ui) {
        let (mut open, mut close) = (None, None);
        let frame = egui::Frame::new().inner_margin(Margin { left: 20, right: 12, top: 0, bottom: 0 });
        egui::Panel::top("pane-header").frame(frame).exact_size(HEADER_H).show(ui, |ui| {
            if self.files.library_open {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("All files").size(17.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(Btn::secondary("Back to chat").small()).clicked() {
                            self.files.library_open = false;
                        }
                    });
                });
                return;
            }
            let gap = 2.0;
            let tabs_w: f32 =
                self.tabs.iter().enumerate().map(|(i, t)| tab_width(ui, t, i == self.active_tab) + gap).sum();
            let full = ui.available_width();
            let file_controls = self.files.limit.is_some();
            let channel_tab = self.tabs[self.active_tab].kind == TabKind::Channel;
            let tools_room = if !file_controls {
                0.0
            } else if channel_tab {
                128.0
            } else {
                75.0
            };
            let tabs_room = tabs_w.min((full - tools_room - 125.0).max(0.0));
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (title, _) = ui.allocate_exact_size(vec2(full - tabs_room - tools_room, 28.0), Sense::hover());
                self.paint_title(ui, title.with_max_x(title.right() - 16.0));
                let mut tabs = |ui: &mut egui::Ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let TabButton { resp, close_clicked } = tab_button(ui, tab, i == self.active_tab);
                        if resp.clicked() {
                            open = Some(i);
                        }
                        if matches!(tab.kind, TabKind::Private(_)) {
                            if close_clicked || resp.middle_clicked() {
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
                if file_controls {
                    ui.add_space(7.0);
                    if channel_tab && ui.add(Btn::secondary("Files").small()).clicked() {
                        self.files.drawer_open = !self.files.drawer_open;
                    }
                    ui.add_space(5.0);
                    if ui.add(Btn::ghost("All files").small()).clicked() {
                        self.files.open_library(&self.net);
                    }
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

    fn composer(&mut self, ui: &mut egui::Ui) {
        let tab = &self.tabs[self.active_tab];
        let upload_limit = self.files.limit.filter(|_| tab.kind == TabKind::Channel);
        let files = upload_limit.is_some();
        let draft_cap = (ui.available_height() * 0.38).clamp(68.0, 240.0);
        let id = egui::Id::unique("chat-composer");
        let cursor = egui::TextEdit::load_state(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .map_or(usize::MAX, |r| r.primary.index.into());
        let writing_code = super::markdown::in_fenced_code(&self.input, cursor);
        let hint = match tab.kind {
            TabKind::Server => "Message everyone on the server".to_owned(),
            TabKind::Channel => format!("Message #{}", tab.title),
            TabKind::Private(_) => format!("Message {}", tab.title),
        };
        let connected = matches!(self.conn, Conn::Connected { .. });
        let frame = egui::Frame::new().inner_margin(Margin { left: 20, right: 20, top: 4, bottom: 16 });
        egui::Panel::bottom("composer").frame(frame).show_separator_line(false).show(ui, |ui| {
            if !connected {
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
                return;
            }
            let field = egui::Frame::new()
                .fill(theme::SURFACE)
                .corner_radius(CornerRadius::same(8))
                .stroke(Stroke::new(1.0, if writing_code { theme::BORDER_STRONG } else { theme::BORDER }))
                .inner_margin(Margin { left: 12, right: 10, top: 3, bottom: 3 });
            field.show(ui, |ui| {
                let focused = ui.memory(|m| m.has_focus(id));
                if files && focused && self.files.can_post() {
                    let images = ui.input_mut(|i| i.events.extract_if(.., |event| matches!(event, egui::Event::PasteImage(_))).collect::<Vec<_>>());
                    for event in images {
                        if let egui::Event::PasteImage(image) = event { self.files.paste(ui.ctx(), image); }
                    }
                }
                if files && self.files.has_staged() {
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .id_salt("draft-attachments")
                        .max_height(142.0)
                        .show(ui, |ui| self.files.staged_tiles(ui));
                    ui.add_space(12.0);
                }
                ui.horizontal(|ui| {
                    if let Some(limit) = upload_limit
                        && ui
                            .add_enabled(self.files.can_post(), Btn::ghost("+").small())
                            .on_hover_text(format!(
                                "Attach files or paste an image · {} per file",
                                super::files::size_label(limit)
                            ))
                            .clicked()
                    {
                        self.files.pick(ui.ctx());
                    }
                    let send_modifiers = if writing_code { egui::Modifiers::COMMAND } else { egui::Modifiers::NONE };
                    let send = focused && ui.input(|i| i.events.iter().any(|event| {
                        matches!(event, egui::Event::Key { key: egui::Key::Enter, pressed: true, repeat: false, modifiers, .. }
                            if modifiers.matches_exact(send_modifiers))
                    }));
                    let width = (ui.available_width() - 32.0 - ui.spacing().item_spacing.x).max(1.0);
                    let mut layouter = super::markdown::draft_galley;
                    let height = (layouter(ui, &self.input, width).size().y + 12.0)
                        .clamp(34.0, draft_cap);
                    let newline = if writing_code { egui::Modifiers::NONE } else { egui::Modifiers::SHIFT };
                    let edit = egui::TextEdit::multiline(&mut self.input)
                        .id(id)
                        .desired_rows(1)
                        .return_key(egui::KeyboardShortcut::new(newline, egui::Key::Enter))
                        .lock_focus(writing_code)
                        .hint_text(RichText::new(hint).color(theme::TEXT_FAINT))
                        .frame(egui::Frame::NONE)
                        .desired_width(width)
                        .min_size(vec2(0.0, 34.0))
                        .margin(Margin::symmetric(0, 6))
                        .align(Align2::LEFT_TOP)
                        .font(FontId::proportional(14.0))
                        .layouter(&mut layouter);
                    let mut output = egui::ScrollArea::vertical()
                        .id_salt("chat-draft")
                        .max_width(width)
                        .max_height(height)
                        .min_scrolled_height(height)
                        .show(ui, |ui| edit.show(ui))
                        .inner;
                    let fence_opened_this_frame = !writing_code
                        && output.response.changed()
                        && output.cursor_range.is_some_and(|range| {
                            super::markdown::in_fenced_code(&self.input, range.primary.index.into())
                        });
                    if send {
                        if fence_opened_this_frame && let Some(range) = output.cursor_range {
                            use egui::TextBuffer;
                            let mut caret = self.input.delete_selected(&range);
                            self.input.insert_text_at(&mut caret, "\n", usize::MAX);
                            output.state.cursor.set_char_range(Some(egui::text::CCursorRange::one(caret)));
                            output.state.store(ui.ctx(), id);
                        } else {
                            self.send_chat();
                        }
                        self.focus_input = true;
                    }
                    if std::mem::take(&mut self.focus_input) {
                        output.response.request_focus();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = (!self.input.trim().is_empty() || files && self.files.has_staged())
                            && !(files && self.files.preparing_image())
                            && self.input.chars().count() <= t7_proto::MAX_TEXT_LEN;
                        if ui.add_enabled(ready, |ui: &mut egui::Ui| send_button(ui, send_modifiers)).clicked() {
                            self.send_chat();
                            self.focus_input = true;
                        }
                    });
                });
                let count = self.input.chars().count();
                if count > t7_proto::MAX_TEXT_LEN {
                    ui.colored_label(theme::RED, format!("{count} / {} characters", t7_proto::MAX_TEXT_LEN));
                }
            });
        });
    }

    fn chat_log(&mut self, ui: &mut egui::Ui) {
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
                        Entry::Said { name, lines } => {
                            said(ui, name, lines, &mut self.files, &mut self.markdown_cache);
                        }
                        Entry::Note(line) => note(ui, line),
                        Entry::File(file, time, caption) => {
                            self.files.chat_post(ui, file, time, caption, &mut self.markdown_cache, &self.net);
                        }
                    }
                }
            });
        });
    }
}

pub(super) fn trim_lines(lines: &mut Vec<Line>) {
    if lines.len() > MAX_LINES {
        lines.drain(..lines.len() - MAX_LINES);
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

fn said(
    ui: &mut egui::Ui,
    name: &str,
    lines: &[Line],
    files: &mut Files,
    cache: &mut egui_commonmark::CommonMarkCache,
) {
    let width = ui.available_width();
    let color = theme::avatar_color(name);
    let p = ui.painter();
    let header = p.layout_no_wrap(name.to_owned(), FontId::proportional(14.0), color);
    let time = p.layout_no_wrap(lines[0].time.clone(), FontId::proportional(11.0), theme::TEXT_FAINT);
    ui.add_space(6.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, header.size().y + 2.0), Sense::hover());
    {
        let p = ui.painter();
        let top = rect.top();
        paint_avatar(p, egui::pos2(rect.left() + 16.0, top + 16.0), 16.0, name);
        let x = rect.left() + TEXT_X;
        let header_h = header.size().y;
        let time_pos = egui::pos2(x + header.size().x + 8.0, top + header_h - time.size().y - 1.0);
        p.galley(egui::pos2(x, top), header, color);
        p.galley(time_pos, time, theme::TEXT_FAINT);
    }
    egui::Frame::new().inner_margin(Margin { left: TEXT_X as i8, ..Margin::ZERO }).show(ui, |ui| {
        for line in lines {
            line.body.show(ui, cache);
            ui.add_space(2.0);
        }
        if let Some(url) = lines.last().and_then(|line| line.body.link.as_ref()) {
            files.linked_image(ui, url);
        }
    });
    ui.add_space(6.0);
}

fn note(ui: &mut egui::Ui, line: &Line) {
    let color = if matches!(line.kind, LineKind::Error) { theme::RED } else { theme::TEXT_FAINT };
    let mut job = LayoutJob::default();
    let fmt = TextFormat { font_id: FontId::proportional(12.5), color, ..Default::default() };
    job.append(&line.body.text, 0.0, fmt.clone());
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
                Entry::File(..) => ("file", 1),
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
