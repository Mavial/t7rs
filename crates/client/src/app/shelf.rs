use std::collections::BTreeSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use egui::emath::GuiRounding;
use egui::epaint::RectShape;
use egui::{
    Align2, Color32, CornerRadius, FontId, Margin, Mesh, Rect, Sense, Shape, Stroke, StrokeKind, TextureHandle,
    TextureOptions, pos2, vec2,
};
use t7_proto::model::ServerView;
use t7_proto::{ChannelId, ClientId};

use super::widgets::{paint_avatar, truncated};
use super::{App, sharing};
use crate::icons::{self, Icon};
use crate::preview::{self, Still};
use crate::theme::{self, Btn};

const ASK_AGAIN: Duration = Duration::from_secs(2);
const STILL: egui::Vec2 = vec2(176.0, 99.0);
const CARD_PAD: f32 = 6.0;
const CARD_W: f32 = STILL.x + 2.0 * CARD_PAD;

pub(super) struct Shelf {
    pub(super) cards: Vec<Card>,
    still_tx: Sender<Still>,
    still_rx: Receiver<Still>,
    pub(super) folded: BTreeSet<ChannelId>,
    pointed: Option<ClientId>,
}

pub(super) struct Card {
    pub(super) sharer: ClientId,
    pub(super) still: Option<TextureHandle>,
    pub(super) asked: Option<Instant>,
}

impl Shelf {
    pub(super) fn new() -> Self {
        let (still_tx, still_rx) = mpsc::channel();
        Self { cards: Vec::new(), still_tx, still_rx, folded: BTreeSet::new(), pointed: None }
    }

    fn follow(&mut self, sharers: &[ClientId]) {
        self.cards.retain(|c| sharers.contains(&c.sharer));
        for &sharer in sharers {
            if !self.cards.iter().any(|c| c.sharer == sharer) {
                self.cards.push(Card { sharer, still: None, asked: None });
            }
        }
    }

    fn card(&mut self, sharer: ClientId) -> Option<&mut Card> {
        self.cards.iter_mut().find(|c| c.sharer == sharer)
    }
}

struct Shown<'a> {
    sharer: ClientId,
    name: &'a str,
    still: Option<&'a TextureHandle>,
    watched: bool,
}

#[derive(Clone, Copy)]
enum Action {
    Watch(ClientId),
    Stop(ClientId),
    Fold(bool),
}

impl App {
    pub(super) fn sync_shelf(&mut self, ctx: &egui::Context) {
        let sharers = self.sharers_here();
        self.shelf.follow(&sharers);
        while let Ok((sharer, image)) = self.shelf.still_rx.try_recv() {
            if let Some(card) = self.shelf.card(sharer) {
                card.still = Some(ctx.load_texture(format!("still {sharer}"), image, TextureOptions::LINEAR));
            }
        }
        self.ask_unasked(ctx);
    }

    fn sharers_here(&self) -> Vec<ClientId> {
        let Some(view) = &self.view else { return Vec::new() };
        let Some(channel) = view.my_channel() else { return Vec::new() };
        view.clients_in(channel).iter().filter(|c| c.sharing && c.id != view.my_id).map(|c| c.id).collect()
    }

    fn ask_unasked(&mut self, ctx: &egui::Context) {
        if self.view.as_ref().and_then(ServerView::my_channel).is_some_and(|c| self.shelf.folded.contains(&c)) {
            return;
        }
        let unasked: Vec<ClientId> = self.shelf.cards.iter().filter(|c| c.asked.is_none()).map(|c| c.sharer).collect();
        for sharer in unasked {
            self.ask_still(sharer, ctx);
        }
    }

    fn ask_still(&mut self, sharer: ClientId, ctx: &egui::Context) {
        self.shelf.card(sharer).expect("a card for each sharer asked").asked = Some(Instant::now());
        let size = still_size(ctx.pixels_per_point());
        let sink = preview::spawn_decode(sharer, size, self.shelf.still_tx.clone(), ctx.clone());
        self.net.preview(sharer, sink);
    }

    pub(super) fn shelf(&mut self, ui: &mut egui::Ui) {
        let Some(view) = &self.view else { return };
        let Some(channel) = view.my_channel() else { return };
        if self.shelf.cards.is_empty() {
            return;
        }
        let shown: Vec<Shown> = (self.shelf.cards.iter())
            .map(|c| Shown {
                sharer: c.sharer,
                name: view.nickname(c.sharer).unwrap_or("Someone"),
                still: c.still.as_ref(),
                watched: self.watching.iter().any(|w| w.sharer == c.sharer),
            })
            .collect();
        let folded = self.shelf.folded.contains(&channel);
        let channel_name = view.channels.get(&channel).map_or("", |c| c.name.as_str());
        let margin = if folded {
            Margin { left: 20, right: 12, top: 6, bottom: 6 }
        } else {
            Margin { left: 20, right: 12, top: 10, bottom: 12 }
        };
        let (action, pointed) = egui::Panel::top("shelf")
            .frame(egui::Frame::new().inner_margin(margin))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 8.0);
                let line = summary(&shown, channel_name, folded);
                match &shown[..] {
                    _ if folded => {
                        (one_line(ui, &line, Icon::ChevronRight, "Show").then_some(Action::Fold(false)), None)
                    }
                    [lone] => lone_card(ui, lone),
                    _ => {
                        let fold = one_line(ui, &line, Icon::ChevronDown, "Hide").then_some(Action::Fold(true));
                        let (action, pointed) = cards(ui, &shown);
                        (fold.or(action), pointed)
                    }
                }
            })
            .inner;
        if pointed != self.shelf.pointed {
            self.shelf.pointed = pointed;
            if let Some(sharer) = pointed
                && self.shelf.card(sharer).is_some_and(|c| c.asked.is_none_or(|at| at.elapsed() >= ASK_AGAIN))
            {
                self.ask_still(sharer, ui.ctx());
            }
        }
        match action {
            Some(Action::Watch(sharer)) => self.watch(sharer),
            Some(Action::Stop(sharer)) => self.stop_watching(sharer),
            Some(Action::Fold(true)) => {
                self.shelf.folded.insert(channel);
            }
            Some(Action::Fold(false)) => {
                self.shelf.folded.remove(&channel);
                self.ask_unasked(ui.ctx());
            }
            None => {}
        }
    }
}

fn summary(shown: &[Shown], channel: &str, folded: bool) -> String {
    let watched = shown.iter().filter(|c| c.watched).count();
    match (shown, watched) {
        ([lone], 1) => format!("You're watching {} in the Screens window", lone.name),
        ([lone], _) => format!("{} is sharing their screen", lone.name),
        (_, 0) if folded => format!("{} people are sharing", shown.len()),
        (_, 0) => format!("{} people are sharing in {channel}", shown.len()),
        (_, watched) => format!("You're watching {watched} of {}", shown.len()),
    }
}

fn one_line(ui: &mut egui::Ui, line: &str, icon: Icon, fold: &str) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let p = ui.painter();
    let y = rect.center().y;
    let x = sharing::live_badge(p, pos2(rect.left(), y), false).right() + 8.0;
    let text = truncated(ui, line, FontId::proportional(12.5), theme::TEXT_DIM, rect.right() - x - 80.0);
    p.galley(pos2(x, y - text.size().y / 2.0), text, theme::TEXT_DIM);
    fold_at(ui, rect, icon, fold)
}

fn fold_at(ui: &mut egui::Ui, row: Rect, icon: Icon, text: &str) -> bool {
    let slot = Rect::from_min_max(pos2(row.right() - 80.0, row.top()), pos2(row.right(), row.top() + 24.0));
    let resp = ui.put(slot, |ui: &mut egui::Ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| fold_button(ui, icon, text)).inner
    });
    resp.clicked()
}

fn fold_button(ui: &mut egui::Ui, icon: Icon, text: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(12.5), theme::TEXT_DIM);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, 24.0), Sense::click());
    let p = ui.painter();
    let color = if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM };
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::SURFACE_HOVER);
    }
    icons::paint(p, Rect::from_center_size(pos2(rect.left() + 14.0, rect.center().y), vec2(12.0, 12.0)), icon, color);
    p.galley_with_override_text_color(pos2(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0), galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn cards(ui: &mut egui::Ui, shown: &[Shown]) -> (Option<Action>, Option<ClientId>) {
    let (mut action, mut pointed) = (None, None);
    egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
        ui.horizontal(|ui| {
            for c in shown {
                let (picked, here) = card(ui, c);
                action = action.or(picked);
                pointed = pointed.or(here.then_some(c.sharer));
            }
        });
    });
    (action, pointed)
}

fn card(ui: &mut egui::Ui, c: &Shown) -> (Option<Action>, bool) {
    let size = vec2(CARD_W, CARD_PAD + STILL.y + 6.0 + 24.0 + CARD_PAD);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    let border = if c.watched { theme::ACCENT } else { theme::BORDER };
    p.rect(rect, CornerRadius::same(8), theme::SURFACE, Stroke::new(1.0, border), StrokeKind::Inside);
    let still = Rect::from_min_size(rect.min + vec2(CARD_PAD, CARD_PAD), STILL);
    paint_still(p, still, c.still);
    if c.watched {
        let backdrop = p.add(Shape::Noop);
        let tag = sharing::live_badge(p, still.min + vec2(6.0, 13.0), true);
        p.set(backdrop, RectShape::filled(tag, CornerRadius::same(3), Color32::from_black_alpha(230)));
    }
    let y = still.bottom() + 6.0 + 12.0;
    paint_avatar(p, pos2(rect.left() + CARD_PAD + 9.0, y), 9.0, c.name);
    let (picked, button) = if c.watched {
        (Action::Stop(c.sharer), Btn::secondary("Stop").small())
    } else {
        (Action::Watch(c.sharer), Btn::primary("Watch").small())
    };
    let slot =
        Rect::from_min_max(pos2(rect.right() - CARD_PAD - 70.0, y - 12.0), pos2(rect.right() - CARD_PAD, y + 12.0));
    let button = ui.put(slot, |ui: &mut egui::Ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.add(button)).inner
    });
    let x = rect.left() + CARD_PAD + 24.0;
    let name = truncated(ui, c.name, FontId::proportional(12.5), theme::TEXT, button.rect.left() - 6.0 - x);
    ui.painter().galley(pos2(x, y - name.size().y / 2.0), name, theme::TEXT);
    (button.clicked().then_some(picked), resp.contains_pointer())
}

fn lone_card(ui: &mut egui::Ui, c: &Shown) -> (Option<Action>, Option<ClientId>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), STILL.y), Sense::hover());
    let still = Rect::from_min_size(rect.min, STILL);
    let pointed = ui.interact(still, ui.scope_id().with(("still", c.sharer)), Sense::hover()).contains_pointer();
    let p = ui.painter();
    paint_still(p, still, c.still);
    let x = still.right() + 14.0;
    let y = rect.top() + 11.0;
    paint_avatar(p, pos2(x + 10.0, y), 10.0, c.name);
    let name = p.text(pos2(x + 26.0, y), Align2::LEFT_CENTER, c.name, FontId::proportional(13.5), theme::TEXT);
    sharing::live_badge(p, pos2(name.right() + 8.0, y), c.watched);
    let note = if c.watched { "In the Screens window" } else { "Sharing their screen" };
    p.text(pos2(x, y + 22.0), Align2::LEFT_CENTER, note, FontId::proportional(12.5), theme::TEXT_DIM);
    let (picked, button) = if c.watched {
        (Action::Stop(c.sharer), Btn::secondary("Stop watching").small())
    } else {
        (Action::Watch(c.sharer), Btn::primary("Watch").small())
    };
    let slot = Rect::from_min_size(pos2(x, rect.bottom() - 24.0), vec2(rect.right() - x - 90.0, 24.0));
    let clicked = ui
        .put(slot, |ui: &mut egui::Ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| ui.add(button)).inner
        })
        .clicked();
    let folded = fold_at(ui, rect, Icon::ChevronDown, "Hide");
    let action = if folded { Some(Action::Fold(true)) } else { clicked.then_some(picked) };
    (action, pointed.then_some(c.sharer))
}

const SPARE: usize = 1;

pub(super) fn still_size(pixels_per_point: f32) -> [usize; 2] {
    let size = (STILL * pixels_per_point).round();
    [size.x as usize + SPARE, size.y as usize + SPARE]
}

fn paint_still(p: &egui::Painter, rect: Rect, still: Option<&TextureHandle>) {
    if let Some(still) = still {
        // The CPU renderer copies a textured quad pixel for pixel, rather than
        // filtering every pixel, only when the quad stops short of the
        // texture's last row and column and more triangles follow it in its
        // mesh: hence the spare row and column, and the empty triangle.
        let ppp = p.pixels_per_point();
        let [w, h] = still.size().map(|n| n as f32);
        let spare = SPARE as f32;
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2((w - spare) / w, (h - spare) / h));
        let drawn = Rect::from_min_size(rect.min.round_to_pixels(ppp), vec2(w - spare, h - spare) / ppp);
        let mut quad = Mesh::with_texture(still.id());
        quad.add_rect_with_uv(drawn, uv, Color32::WHITE);
        quad.add_triangle(0, 0, 0);
        p.add(quad);
    } else {
        p.rect(rect, 0.0, theme::PANEL, Stroke::new(1.0, theme::BORDER), StrokeKind::Inside);
        let icon = Rect::from_center_size(rect.center(), vec2(22.0, 22.0));
        icons::paint(p, icon, Icon::Screen, theme::BORDER_STRONG);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sharers(shelf: &Shelf) -> Vec<ClientId> {
        shelf.cards.iter().map(|c| c.sharer).collect()
    }

    #[test]
    fn cards_follow_who_shares_in_the_order_they_started() {
        let mut shelf = Shelf::new();
        shelf.follow(&[4, 2]);
        shelf.cards[1].asked = Some(Instant::now());
        shelf.follow(&[1, 2, 4]);
        assert_eq!(sharers(&shelf), [4, 2, 1], "a new sharer goes last");
        shelf.follow(&[1, 2]);
        assert_eq!(sharers(&shelf), [2, 1], "one stopped");
        assert!(shelf.cards[0].asked.is_some(), "the others keep their stills");
    }
}
