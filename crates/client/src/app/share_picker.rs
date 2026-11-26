use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::{FontId, Rect, Response, Sense, TextureHandle, TextureOptions, Ui, pos2, vec2};

use super::widgets::{menu_label, truncated};
use crate::icons::{self, Icon};
use crate::screen::{PickerSources, Source, SourceInfo};
use crate::theme;

pub(super) enum Picker {
    Loading(Receiver<PickerSources>),
    Ready { screens: Vec<SourceInfo>, apps: Vec<AppRow> },
    Failed,
}

pub(super) struct AppRow {
    name: String,
    windows: Vec<SourceInfo>,
    icon: Option<TextureHandle>,
    expanded: bool,
}

impl Picker {
    pub(super) fn open(ctx: &egui::Context, load: fn() -> PickerSources) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if tx.send(load()).is_ok() {
                ctx.request_repaint();
            }
        });
        Self::Loading(rx)
    }

    /// Takes the list once the background thread has it. True on the frame it
    /// arrives, when the menu has to be measured again at its full height.
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> bool {
        let Self::Loading(rx) = self else { return false };
        match rx.try_recv() {
            Ok(sources) => {
                let apps = sources
                    .apps
                    .into_iter()
                    .map(|app| AppRow {
                        icon: app.icon.map(|image| ctx.load_texture("share app icon", image, TextureOptions::LINEAR)),
                        name: app.name,
                        windows: app.windows,
                        expanded: false,
                    })
                    .collect();
                *self = Self::Ready { screens: sources.screens, apps };
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                *self = Self::Failed;
                true
            }
        }
    }

    pub(super) fn show(&mut self, ui: &mut Ui) -> Option<Source> {
        if let Self::Loading(_) = self {
            ui.weak("Finding screens and apps…");
            return None;
        }
        let Self::Ready { screens, apps } = self else {
            ui.weak("Couldn't list screens and apps. Reopen to try again.");
            return None;
        };
        egui::ScrollArea::vertical()
            .max_height(380.0)
            .show(ui, |ui| {
                if !screens.is_empty() {
                    menu_label(ui, "Screens");
                    for screen in screens {
                        if row(ui, &screen.name, "", Leading::Icon(Icon::Screen), None).clicked() {
                            return Some(screen.source);
                        }
                    }
                }
                menu_label(ui, "Apps");
                if apps.is_empty() {
                    ui.weak("Open or restore an app window to share it.");
                }
                for app in apps {
                    let leading = app.icon.as_ref().map_or(Leading::Icon(Icon::Window), Leading::Image);
                    if let [window] = app.windows.as_slice() {
                        let title = if window.name == app.name { "" } else { &window.name };
                        if row(ui, &app.name, title, leading, None).clicked() {
                            return Some(window.source);
                        }
                        continue;
                    }
                    let detail = format!("{} windows", app.windows.len());
                    if row(ui, &app.name, &detail, leading, Some(app.expanded)).clicked() {
                        app.expanded = !app.expanded;
                    }
                    if app.expanded {
                        for window in &app.windows {
                            if row(ui, &window.name, "", Leading::Indent, None).clicked() {
                                return Some(window.source);
                            }
                        }
                    }
                }
                None
            })
            .inner
    }
}

/// What sits left of a row's text. `Indent` lines a window up under its app's name.
enum Leading<'a> {
    Image(&'a TextureHandle),
    Icon(Icon),
    Indent,
}

fn row(ui: &mut Ui, name: &str, detail: &str, leading: Leading<'_>, expanded: Option<bool>) -> Response {
    let height = match (&leading, detail.is_empty()) {
        (Leading::Indent, _) => 28.0,
        (_, true) => 32.0,
        (_, false) => 42.0,
    };
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let p = ui.painter();
    if response.hovered() || response.has_focus() {
        p.rect_filled(rect, theme::RADIUS, theme::SURFACE_HOVER);
    }
    let icon = Rect::from_center_size(pos2(rect.left() + 19.0, rect.center().y), vec2(24.0, 24.0));
    match leading {
        Leading::Image(image) => egui::Image::new(image).paint_at(ui, icon),
        Leading::Icon(fallback) => icons::paint(p, icon.shrink(2.0), fallback, theme::ACCENT_TEXT),
        Leading::Indent => {}
    }
    let x = icon.right() + 8.0;
    let width = rect.right() - x - if expanded.is_some() { 26.0 } else { 7.0 };
    let title = truncated(ui, name, FontId::proportional(13.0), theme::TEXT, width);
    let mut cut = title.elided;
    let y = if detail.is_empty() { rect.center().y - title.size().y / 2.0 } else { rect.top() + 4.0 };
    p.galley(pos2(x, y), title, theme::TEXT);
    if !detail.is_empty() {
        let sub = truncated(ui, detail, FontId::proportional(11.5), theme::TEXT_DIM, width);
        cut |= sub.elided;
        p.galley(pos2(x, rect.bottom() - sub.size().y - 4.0), sub, theme::TEXT_DIM);
    }
    if let Some(open) = expanded {
        let arrow = Rect::from_center_size(pos2(rect.right() - 12.0, rect.center().y), vec2(12.0, 12.0));
        icons::paint(p, arrow, if open { Icon::ChevronDown } else { Icon::ChevronRight }, theme::TEXT_DIM);
    }
    let label = if detail.is_empty() {
        name.to_owned()
    } else {
        format!(
            "{name}
{detail}"
        )
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    // The tooltip only finishes text the row had to cut short.
    if cut { response.on_hover_text(label) } else { response }
}
