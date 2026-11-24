//! The bar across the top (server, connection, updates) and the one along the
//! bottom (you, your status, voice settings).

use std::time::Duration;

use egui::{Align, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, vec2};
use t7_proto::Speech;

use super::chat::LineKind;
use super::widgets::{avatar, icon_button, logo, mic_held, toggle};
use super::{App, Conn};
use crate::icons::Icon;
use crate::theme;
use crate::theme::Btn;

impl App {
    fn restart_to_update(&mut self) {
        let Some(exe) = self.updater.install() else { return };
        // Same arguments, so a command-line connection reconnects.
        match std::process::Command::new(&exe).args(std::env::args().skip(1)).spawn() {
            Ok(_) => self.quit = true,
            Err(e) => self.push(0, LineKind::Error, format!("Updated, but couldn't restart T7: {e}. Start it again.")),
        }
    }

    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(Margin::symmetric(12, 8))
            .stroke(Stroke::new(1.0, theme::BORDER));
        egui::Panel::top("top").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                logo(ui, 24.0);
                ui.add_space(2.0);
                let name = self.view.as_ref().map_or("T7", |v| v.info.name.as_str());
                ui.label(RichText::new(name).size(15.0).strong().color(theme::TEXT));

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    match self.conn {
                        Conn::Connected { .. } => {
                            if ui.add(Btn::secondary("Disconnect")).clicked() {
                                self.disconnect();
                            }
                        }
                        Conn::Connecting { .. } => {
                            if ui.add(Btn::secondary("Cancel")).clicked() {
                                self.disconnect();
                            }
                        }
                        Conn::Disconnected => {
                            if ui.add(Btn::primary("Connect")).on_hover_text("Ctrl+S").clicked() {
                                self.open_connect_form();
                            }
                        }
                    }
                    self.status_pill(ui);
                    match self.updater.status() {
                        crate::update::Status::Ready(version) => {
                            let label = format!("Restart to update to {version}");
                            if ui
                                .add(Btn::primary(&label))
                                .on_hover_text("The update is downloaded and verified")
                                .clicked()
                            {
                                self.restart_to_update();
                            }
                        }
                        crate::update::Status::Downloading(version) => {
                            ui.label(
                                RichText::new(format!("Downloading {version}…")).size(12.5).color(theme::TEXT_FAINT),
                            );
                        }
                        _ => {}
                    }
                });
            });
        });
    }

    fn status_pill(&self, ui: &mut egui::Ui) {
        let (color, text) = match &self.conn {
            Conn::Connected { .. } if self.voice.blocked() => (theme::AMBER, "Connected · no voice".to_owned()),
            Conn::Connected { .. } => {
                let ping = match self.voice.rtt().or(self.ping) {
                    Some(p) if p < Duration::from_millis(1) => " · <1 ms".to_owned(),
                    Some(p) => format!(" · {} ms", p.as_millis()),
                    None => String::new(),
                };
                (theme::GREEN, format!("Connected{ping}"))
            }
            Conn::Connecting { address, .. } => (theme::AMBER, format!("Connecting to {address}…")),
            Conn::Disconnected => (theme::TEXT_FAINT, "Not connected".to_owned()),
        };
        // Painted to the exact height of the buttons beside it.
        let galley = ui.painter().layout_no_wrap(text, FontId::proportional(12.5), theme::TEXT_DIM);
        let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 34.0, theme::CONTROL_H), Sense::hover());
        let p = ui.painter();
        p.rect(
            rect,
            CornerRadius::same(255),
            theme::SURFACE,
            Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );
        let dot = egui::pos2(rect.left() + 15.0, rect.center().y);
        p.circle_filled(dot, 6.0, color.gamma_multiply(0.2));
        p.circle_filled(dot, 3.5, color);
        p.galley(egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0), galley, theme::TEXT_DIM);
        if self.voice.blocked() {
            resp.on_hover_text(
                "Voice packets aren't getting through. The server's UDP port (same number as \
                 the TCP port, 9988 by default) is probably blocked by a firewall.",
            );
        }
    }

    pub(super) fn self_bar(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(theme::BG)
            .inner_margin(Margin::symmetric(12, 8))
            .stroke(Stroke::new(1.0, theme::BORDER));
        egui::Panel::bottom("self").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let nick = self
                    .view
                    .as_ref()
                    .and_then(|v| v.me())
                    .map_or_else(|| self.settings.nickname.clone(), |c| c.nickname.clone());
                avatar(ui, &nick, 30.0, self.voice.shared.transmitting());
                ui.add_space(2.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(RichText::new(&nick).size(13.5).strong().color(theme::TEXT));
                    let s = self.settings.status;
                    let (txt, col) = if !matches!(self.conn, Conn::Connected { .. }) {
                        ("Offline", theme::TEXT_FAINT)
                    } else if self.my_speech() == Speech::ServerMuted {
                        ("Muted on the server", theme::RED)
                    } else if self.my_speech() == Speech::NotHere {
                        ("Can't speak here", theme::AMBER)
                    } else if s.away {
                        ("Away", theme::AMBER)
                    } else if s.deafened {
                        ("Sound off", theme::RED)
                    } else if s.muted {
                        ("Microphone muted", theme::RED)
                    } else {
                        ("Online", theme::GREEN)
                    };
                    ui.label(RichText::new(txt).size(11.5).color(col));
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if icon_button(ui, Icon::Sliders, "Voice settings").clicked() {
                        self.open_voice_settings();
                    }
                    // A hairline between the settings and the status toggles.
                    let (r, _) = ui.allocate_exact_size(vec2(9.0, 18.0), Sense::hover());
                    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, theme::BORDER_STRONG));
                    let mut s = self.settings.status;
                    let speech = self.my_speech();
                    let changed = toggle(ui, &mut s.away, Icon::Moon, Icon::Moon, theme::AMBER, "Away")
                        | toggle(
                            ui,
                            &mut s.deafened,
                            Icon::Headphones,
                            Icon::HeadphonesOff,
                            theme::RED,
                            "Turn sound off (Ctrl+Shift+D)",
                        )
                        | match speech {
                            Speech::Allowed => toggle(
                                ui,
                                &mut s.muted,
                                Icon::Mic,
                                Icon::MicOff,
                                theme::RED,
                                "Mute microphone (Ctrl+Shift+M)",
                            ),
                            Speech::NotHere => {
                                let tip = "You can't speak in this channel";
                                mic_held(ui, Icon::MicOff, theme::TEXT_FAINT, tip);
                                false
                            }
                            Speech::ServerMuted => {
                                let tip = "A moderator muted you on the server. Only a moderator can unmute you.";
                                mic_held(ui, Icon::MicLocked, theme::RED, tip);
                                false
                            }
                        };
                    if changed {
                        self.set_status(s);
                    }
                    let (r, _) = ui.allocate_exact_size(vec2(9.0, 18.0), Sense::hover());
                    ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, theme::BORDER_STRONG));
                    self.share_button(ui);
                });
            });
        });
    }
}
