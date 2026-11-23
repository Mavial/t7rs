//! Chat tabs: the server, your channel, and private conversations.

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::model::ServerView;
use t7_proto::{C2S, ClientId, TextTarget};

use super::{App, Conn};
use crate::theme;

const MAX_LINES: usize = 1000;

pub(super) enum LineKind {
    System,
    Error,
    Msg { from: ClientId, name: String },
}

pub(super) struct Line {
    pub(super) time: String,
    pub(super) kind: LineKind,
    pub(super) text: String,
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

pub(super) fn base_tabs() -> Vec<Tab> {
    vec![
        Tab { kind: TabKind::Server, title: "Server".into(), lines: Vec::new(), unread: 0 },
        Tab { kind: TabKind::Channel, title: "Channel".into(), lines: Vec::new(), unread: 0 },
    ]
}

/// One chat line as a single wrapped text layout: cheap to lay out and render.
fn layout_line(line: &Line, my_id: ClientId, width: f32) -> LayoutJob {
    use egui::text::TextFormat;
    let fmt =
        |size: f32, color: Color32| TextFormat { font_id: FontId::proportional(size), color, ..Default::default() };
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

/// A chat tab, with its unread count in a pill after the title.
fn tab_button(ui: &mut egui::Ui, label: &str, active: bool, unread: u32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(13.0), theme::TEXT);
    let count = (unread > 0).then(|| {
        let text = if unread > 99 { "99+".to_owned() } else { unread.to_string() };
        ui.painter().layout_no_wrap(text, FontId::proportional(10.5), theme::ON_ACCENT)
    });
    let pill_w = count.as_ref().map_or(0.0, |c| (c.size().x + 10.0).max(17.0));
    let width = galley.size().x + 24.0 + if count.is_some() { pill_w + 6.0 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 28.0), Sense::click());
    let p = ui.painter();
    let hovered = resp.hovered();
    if active {
        p.rect(
            rect,
            CornerRadius::same(theme::RADIUS),
            theme::SURFACE_HOVER,
            Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );
    } else if hovered {
        p.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::SURFACE);
    }
    let color = if active || hovered { theme::TEXT } else { theme::TEXT_DIM };
    let text_pos = egui::pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0);
    let text_right = text_pos.x + galley.size().x;
    p.galley_with_override_text_color(text_pos, galley, color);
    if let Some(count) = count {
        let pill = egui::Rect::from_min_size(egui::pos2(text_right + 6.0, rect.center().y - 8.0), vec2(pill_w, 16.0));
        p.rect_filled(pill, CornerRadius::same(8), theme::ACCENT);
        p.galley(pill.center() - count.size() / 2.0, count, theme::ON_ACCENT);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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

    pub(super) fn chat(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(16, 12));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if self.view.is_none() {
                self.empty_state(ui);
                return;
            }

            // Tabs
            let mut close = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for i in 0..self.tabs.len() {
                    let tab = &self.tabs[i];
                    let label = match tab.kind {
                        TabKind::Server => "Server".to_owned(),
                        TabKind::Channel => format!("# {}", tab.title),
                        TabKind::Private(_) => format!("@ {}", tab.title),
                    };
                    let resp = tab_button(ui, &label, i == self.active_tab, tab.unread);
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

            ui.add_space(2.0);
            ui.separator();
            ui.add_space(2.0);

            // Composer pinned to the bottom, messages fill the rest.
            let kind = self.tabs[self.active_tab].kind;
            let hint = match kind {
                TabKind::Server => "Message everyone on the server".to_owned(),
                TabKind::Channel => format!("Message #{}", self.tabs[1].title),
                TabKind::Private(_) => format!("Message @{}", self.tabs[self.active_tab].title),
            };
            egui::Panel::bottom("composer")
                .show_separator_line(false)
                .frame(egui::Frame::new().inner_margin(Margin { top: 10, ..Default::default() }))
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .corner_radius(CornerRadius::same(8))
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .inner_margin(Margin::symmetric(12, 3))
                        .show(ui, |ui| {
                            let edit = egui::TextEdit::singleline(&mut self.input)
                                .hint_text(RichText::new(hint).color(theme::TEXT_FAINT))
                                .frame(egui::Frame::NONE)
                                .desired_width(f32::INFINITY)
                                .min_size(vec2(0.0, 32.0))
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
                        });
                });

            let view = self.view.as_ref().expect("checked above");
            let tab = &self.tabs[self.active_tab];
            egui::ScrollArea::vertical().id_salt(kind).auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                for line in &tab.lines {
                    ui.label(layout_line(line, view.my_id, ui.available_width()));
                }
            });
        });
    }
}
