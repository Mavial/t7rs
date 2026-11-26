use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, FontId, PopupCloseBehavior, RectAlign, RichText, Sense, vec2};
use t7_proto::model::ServerView;
use t7_proto::{C2S, ClientId, MAX_WATCHED, Speech};

use super::chat::LineKind;
use super::share_picker::Picker;
use super::widgets::{icon_button, mic_held, toggle};
use super::{App, Conn};
use crate::icons::Icon;
use crate::screen::{Quality, Share, Source};
use crate::theme;
use crate::watch::{self, Frames, Stream};

pub(super) const QUALITIES: [(Quality, &str); 3] =
    [(Quality::P720_30, "720p 30"), (Quality::P1080_30, "1080p 30"), (Quality::P1080_60, "1080p 60")];

pub(super) struct Sharing {
    share: Share,
    state: ShareState,
}

#[derive(Clone, Copy)]
enum ShareState {
    Requested,
    Live,
}

impl Sharing {
    fn ended_by_server(&self, shared: Option<bool>) -> bool {
        matches!((shared, self.state), (None, _) | (Some(false), ShareState::Live))
    }
}

pub(super) struct Watching {
    pub(super) sharer: ClientId,
    pub(super) frames: Arc<Frames>,
}

fn cannot_share(app: &App) -> Option<&'static str> {
    if !cfg!(windows) {
        Some("Screen sharing is Windows-only for now")
    } else if !matches!(app.conn, Conn::Connected { .. }) {
        Some("Connect to a server to share your screen")
    } else if app.sharing.is_none() && app.my_speech() != Speech::Allowed {
        Some("You can't share your screen in this channel")
    } else {
        None
    }
}

impl App {
    pub fn screens(&self) -> Vec<Stream> {
        let name = |id| self.view.as_ref().and_then(|v| v.nickname(id)).unwrap_or("Someone").to_owned();
        self.watching
            .iter()
            .map(|w| Stream { sharer: w.sharer, name: name(w.sharer), frames: w.frames.clone() })
            .collect()
    }

    pub fn stop_watching(&mut self, sharer: ClientId) {
        if let Some(i) = self.watching.iter().position(|w| w.sharer == sharer) {
            self.watching.remove(i);
            self.net.unwatch(sharer);
        }
    }

    pub fn stop_watching_all(&mut self) {
        for w in std::mem::take(&mut self.watching) {
            self.net.unwatch(w.sharer);
        }
    }

    pub(super) fn watch(&mut self, sharer: ClientId) {
        let Some(view) = &self.view else { return };
        if sharer == view.my_id || self.watching.iter().any(|w| w.sharer == sharer) {
            return;
        }
        let name = view.nickname(sharer).unwrap_or("Someone").to_owned();
        if self.watching.len() >= MAX_WATCHED {
            let tab = self.active_tab;
            let line = format!("You can watch {MAX_WATCHED} screens at once. Close one to watch {name}'s.");
            return self.push(tab, LineKind::System, line);
        }
        match watch::start(self.screen_wake.clone()) {
            Ok((frames, sink)) => {
                self.net.watch(sharer, sink);
                self.watching.push(Watching { sharer, frames });
            }
            Err(e) => {
                let tab = self.active_tab;
                self.push(tab, LineKind::Error, format!("Can't show {name}'s screen: {e}"));
            }
        }
    }

    fn start_sharing(&mut self, source: Source, ctx: &egui::Context) {
        let video = self.net.video_sink();
        let ctx = ctx.clone();
        match Share::start(source, self.settings.screen_quality, move |au| video(au), move || ctx.request_repaint()) {
            Ok(share) => {
                tracing::info!(encoder = share.encoder(), "sharing the screen");
                self.net.send(C2S::SetSharing(true));
                self.sharing = Some(Sharing { share, state: ShareState::Requested });
            }
            Err(e) => {
                let tab = self.active_tab;
                self.push(tab, LineKind::Error, format!("Couldn't share your screen: {e}"));
            }
        }
    }

    fn stop_sharing(&mut self) {
        let Some(sharing) = self.sharing.take() else { return };
        self.net.send(C2S::SetSharing(false));
        let stats = sharing.share.take_stats();
        let latency = stats.latency_total.checked_div(stats.encoded).unwrap_or_default();
        let (captured, encoded, bytes, max) = (stats.captured, stats.encoded, stats.bytes, stats.latency_max);
        let sent = self.net.take_video_sent();
        let (parts, socket_refused, too_large, stale, skipped) =
            (sent.parts, sent.socket_refused, sent.too_large, sent.stale, sent.skipped);
        tracing::info!(
            captured,
            encoded,
            bytes,
            ?latency,
            ?max,
            parts,
            socket_refused,
            too_large,
            stale,
            skipped,
            "stopped sharing the screen"
        );
    }

    pub(super) fn send_keyframe(&self) {
        if let Some(sharing) = &self.sharing {
            sharing.share.request_keyframe();
        }
    }

    pub(super) fn sync_screens(&mut self) {
        let shared = self.view.as_ref().and_then(ServerView::me).map(|c| c.sharing);
        if let Some(sharing) = &mut self.sharing {
            if shared == Some(true) {
                sharing.state = ShareState::Live;
            }
            if sharing.share.ended() {
                self.stop_sharing();
                let tab = self.active_tab;
                self.push(tab, LineKind::System, "Stopped sharing: the shared window or screen is gone");
            } else if sharing.ended_by_server(shared) {
                self.stop_sharing();
            }
        }
        let still_there = |id| {
            let v = self.view.as_ref();
            v.is_some_and(|v| v.clients.get(&id).is_some_and(|c| c.sharing && Some(c.channel) == v.my_channel()))
        };
        let gone: Vec<ClientId> = self.watching.iter().map(|w| w.sharer).filter(|&id| !still_there(id)).collect();
        for sharer in gone {
            self.stop_watching(sharer);
        }
    }

    pub(super) fn share_button(&mut self, ui: &mut egui::Ui) {
        if let Some(tip) = cannot_share(self) {
            self.share_picker = None;
            return mic_held(ui, Icon::Screen, theme::TEXT_FAINT, tip);
        }
        if self.sharing.is_some() {
            let mut on = true;
            if toggle(ui, &mut on, Icon::Screen, Icon::ScreenStop, theme::RED, "Stop sharing your screen") {
                self.stop_sharing();
            }
            return;
        }
        let resp = icon_button(ui, Icon::Screen, "Share your screen");
        let mut picked = None;
        let arrived = self.share_picker.as_mut().is_some_and(|picker| picker.poll(ui.ctx()));
        let popup = egui::Popup::menu(&resp)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .align(RectAlign::TOP_END)
            .sizing_pass(arrived)
            .show(|ui| {
                ui.set_width(320.0);
                ui.strong("Share your screen");
                ui.label(RichText::new("Quality").size(11.5).color(theme::TEXT_FAINT));
                theme::segmented(ui, &mut self.settings.screen_quality, &QUALITIES);
                let picker = self.share_picker.get_or_insert_with(|| Picker::open(ui.ctx(), self.share_sources));
                picked = picker.show(ui);
                if picked.is_some() {
                    ui.close();
                }
            });
        if popup.is_none() || !egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp)) {
            self.share_picker = None;
        }
        if let Some(source) = picked {
            self.start_sharing(source, ui.ctx());
        }
    }
}

/// LIVE in white on red, or WATCHING in red outline once you watch it.
pub(super) fn live_badge(p: &egui::Painter, at: egui::Pos2, watching: bool) -> egui::Rect {
    let (text, color) = if watching { ("WATCHING", theme::RED) } else { ("LIVE", Color32::WHITE) };
    let galley = p.layout_no_wrap(text.into(), FontId::proportional(9.5), color);
    let rect = egui::Rect::from_min_size(at - vec2(0.0, 7.0), vec2(galley.size().x + 8.0, 14.0));
    if watching {
        p.rect_stroke(rect, CornerRadius::same(3), egui::Stroke::new(1.0, theme::RED), egui::StrokeKind::Inside);
    } else {
        p.rect_filled(rect, CornerRadius::same(3), theme::RED);
    }
    p.galley(Align2::CENTER_CENTER.anchor_size(rect.center(), galley.size()).min, galley, color);
    rect
}

pub(super) fn live_badge_clicked(ui: &egui::Ui, rect: egui::Rect, id: egui::Id, tip: &str, watchable: bool) -> bool {
    let sense = if watchable { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense).on_hover_text(tip);
    let resp = if watchable { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    resp.clicked()
}
