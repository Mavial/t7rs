//! Screen sharing in the app: the share button and its picker, watching
//! someone's screen, and keeping both in step with the server.

use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, FontId, PopupCloseBehavior, RectAlign, RichText, Sense, vec2};
use t7_proto::model::ServerView;
use t7_proto::{C2S, ClientId, Speech};

use super::{App, Conn, LineKind, icon_button, mic_held, toggle};
use crate::icons::Icon;
use crate::screen::{Quality, Share, Source, SourceInfo};
use crate::theme;
use crate::watch::{self, Frames};

const QUALITIES: [(Quality, &str); 3] =
    [(Quality::P720_30, "720p 30"), (Quality::P1080_30, "1080p 30"), (Quality::P1080_60, "1080p 60")];

/// Our screen, while we share it.
pub(super) struct Sharing {
    share: Share,
    /// The server has said we share. Until then its view of us is stale.
    confirmed: bool,
}

/// Someone's screen we watch, shown in a window of its own.
pub(super) struct Watching {
    pub(super) sharer: ClientId,
    frames: Arc<Frames>,
}

/// Why sharing is unavailable right now, if it is.
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
    /// The screen to show in the screen window, and that window's title.
    pub fn watching(&self) -> Option<(&Arc<Frames>, String)> {
        let watching = self.watching.as_ref()?;
        let name = self.view.as_ref().and_then(|v| v.nickname(watching.sharer)).unwrap_or("Someone");
        Some((&watching.frames, format!("{name}'s screen — T7")))
    }

    pub fn stop_watching(&mut self) {
        if self.watching.take().is_some() {
            self.net.set_screen_sink(None);
            self.net.send(C2S::Watch(None));
        }
    }

    /// Watches `sharer`'s screen instead of any watched before.
    pub(super) fn watch(&mut self, sharer: ClientId) {
        let Some(view) = &self.view else { return };
        if sharer == view.my_id || self.watching.as_ref().is_some_and(|w| w.sharer == sharer) {
            return;
        }
        let name = view.nickname(sharer).unwrap_or("Someone").to_owned();
        match watch::start(sharer, self.screen_wake.clone()) {
            Ok((frames, sink)) => {
                self.net.set_screen_sink(Some(sink));
                self.net.send(C2S::Watch(Some(sharer)));
                self.watching = Some(Watching { sharer, frames });
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
        // The share ending wakes the UI, which then stops it (`sync_screens`).
        match Share::start(source, self.settings.screen_quality, move |au| video(au), move || ctx.request_repaint()) {
            Ok(share) => {
                tracing::info!(encoder = share.encoder(), "sharing the screen");
                self.net.send(C2S::SetSharing(true));
                self.sharing = Some(Sharing { share, confirmed: false });
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
        tracing::info!(captured, encoded, bytes, ?latency, ?max, "stopped sharing the screen");
    }

    pub(super) fn send_keyframe(&self) {
        if let Some(sharing) = &self.sharing {
            sharing.share.request_keyframe();
        }
    }

    /// Ends a share or a watch the server, the connection or the shared
    /// window has ended. Runs every frame, after the network events.
    pub(super) fn sync_screens(&mut self) {
        let shared = self.view.as_ref().and_then(ServerView::me).map(|c| c.sharing);
        if let Some(sharing) = &mut self.sharing {
            sharing.confirmed |= shared == Some(true);
            if sharing.share.ended() {
                self.stop_sharing();
                let tab = self.active_tab;
                self.push(tab, LineKind::System, "Stopped sharing: what you shared is gone");
            } else if shared.is_none() || (sharing.confirmed && shared == Some(false)) {
                // Disconnected, or the server stopped it because we changed channels.
                self.stop_sharing();
            }
        }
        let still_there =
            |v: &ServerView, id| v.clients.get(&id).is_some_and(|c| c.sharing && Some(c.channel) == v.my_channel());
        let watched = self.watching.as_ref().map(|w| w.sharer);
        if watched.is_some_and(|id| !self.view.as_ref().is_some_and(|v| still_there(v, id))) {
            self.stop_watching();
        }
    }

    /// The share button, next to the voice buttons: a picker of what to
    /// share, or a stop button while sharing.
    pub(super) fn share_button(&mut self, ui: &mut egui::Ui) {
        if let Some(tip) = cannot_share(self) {
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
        if resp.clicked() {
            self.share_sources = crate::screen::sources();
        }
        let mut picked = None;
        egui::Popup::menu(&resp)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .align(RectAlign::TOP_END)
            .show(|ui| {
                ui.set_width(320.0);
                ui.label(RichText::new("Quality").size(11.5).color(theme::TEXT_FAINT));
                theme::segmented(ui, &mut self.settings.screen_quality, &QUALITIES);
                let monitors = self.share_sources.iter().filter(|s| matches!(s.source, Source::Monitor(_)));
                let windows = self.share_sources.iter().filter(|s| matches!(s.source, Source::Window(_)));
                egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    for (heading, list) in [("Screens", monitors.collect::<Vec<_>>()), ("Windows", windows.collect())] {
                        if list.is_empty() {
                            continue;
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new(heading).size(11.5).color(theme::TEXT_FAINT));
                        for SourceInfo { source, name } in list {
                            if ui.add(egui::Button::new(name.as_str()).truncate()).on_hover_text(name).clicked() {
                                picked = Some(*source);
                                ui.close();
                            }
                        }
                    }
                });
            });
        if let Some(source) = picked {
            self.start_sharing(source, ui.ctx());
        }
    }
}

/// Paints the red LIVE badge with its left edge centered at `at`; its rect.
pub(super) fn live_badge(p: &egui::Painter, at: egui::Pos2) -> egui::Rect {
    let galley = p.layout_no_wrap("LIVE".into(), FontId::proportional(9.5), Color32::WHITE);
    let rect = egui::Rect::from_min_size(at - vec2(0.0, 7.0), vec2(galley.size().x + 8.0, 14.0));
    p.rect_filled(rect, CornerRadius::same(3), theme::RED);
    p.galley(Align2::CENTER_CENTER.anchor_size(rect.center(), galley.size()).min, galley, Color32::WHITE);
    rect
}

/// The badge's click area and tooltip in the channel tree; true when clicked to watch.
pub(super) fn live_badge_clicked(ui: &egui::Ui, rect: egui::Rect, id: egui::Id, tip: &str, watchable: bool) -> bool {
    let sense = if watchable { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense).on_hover_text(tip);
    let resp = if watchable { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    resp.clicked()
}
