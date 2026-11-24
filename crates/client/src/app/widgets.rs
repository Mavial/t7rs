//! Small widgets shared by the panels and dialogs.

use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, vec2};

use crate::icons::Icon;
use crate::{icons, theme};

pub(super) fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    crate::brand::paint(ui.painter(), rect);
}

pub(super) fn paint_avatar(p: &egui::Painter, center: egui::Pos2, radius: f32, name: &str) {
    p.circle_filled(center, radius, theme::avatar_color(name));
    let initial: String = name.chars().next().map_or_default(|c| c.to_uppercase().collect());
    p.text(center, Align2::CENTER_CENTER, initial, FontId::proportional(radius * 1.1), Color32::WHITE);
}

pub(super) fn avatar(ui: &mut egui::Ui, name: &str, size: f32, talking: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_avatar(ui.painter(), rect.center(), size / 2.0, name);
    if talking {
        ui.painter().circle_stroke(rect.center(), size / 2.0 + 2.5, Stroke::new(2.0, theme::GREEN));
    }
}

pub(super) fn icon_button(ui: &mut egui::Ui, icon: Icon, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    let (fill, color) =
        if resp.hovered() { (theme::SURFACE_HOVER, theme::TEXT) } else { (Color32::TRANSPARENT, theme::TEXT_DIM) };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

pub(super) fn mic_held(ui: &mut egui::Ui, icon: Icon, color: Color32, tip: &str) {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::SURFACE);
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, color);
    resp.on_hover_text(tip);
}

pub(super) fn toggle(
    ui: &mut egui::Ui,
    on: &mut bool,
    icon: Icon,
    icon_on: Icon,
    on_color: Color32,
    tip: &str,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    let (fill, color, icon) = match (*on, resp.hovered()) {
        (true, hovered) => (on_color.gamma_multiply(if hovered { 0.26 } else { 0.18 }), on_color, icon_on),
        (false, true) => (theme::SURFACE_HOVER, theme::TEXT, icon),
        (false, false) => (Color32::TRANSPARENT, theme::TEXT_DIM, icon),
    };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, color);
    let clicked = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked();
    if clicked {
        *on = !*on;
    }
    clicked
}

/// Starts a group of settings: a small caps label with room above it.
pub(super) fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title.to_uppercase()).size(11.0).strong().color(theme::TEXT_FAINT));
    ui.add_space(2.0);
}

/// Width of the label column in settings rows.
const ROW_LABEL_W: f32 = 104.0;
/// Width of the number box after a slider.
pub(super) const SLIDER_VALUE_W: f32 = 60.0;

/// A settings row: the label in a fixed column so every control starts at the same x.
pub(super) fn row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let color = if ui.is_enabled() { theme::TEXT_DIM } else { theme::TEXT_FAINT };
        let (rect, _) = ui.allocate_exact_size(vec2(ROW_LABEL_W, theme::CONTROL_H), Sense::hover());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, label, FontId::proportional(13.0), color);
        add(ui)
    })
    .inner
}

#[derive(Clone, Copy)]
pub(super) enum Unit {
    Db,
    /// Stored as a factor, shown as a percentage.
    Percent,
}

/// A slider filling the rest of a row, its value in an editable box at the end.
pub(super) fn slider(ui: &mut egui::Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, unit: Unit) {
    let width = ui.available_width() - SLIDER_VALUE_W - ui.spacing().item_spacing.x;
    ui.scope(|ui| {
        // egui sizes the knob from the control height; this keeps it small.
        ui.spacing_mut().interact_size.y = 16.0;
        ui.spacing_mut().slider_width = width;
        ui.add(egui::Slider::new(value, range.clone()).show_value(false));
    });
    let speed = (range.end() - range.start()) / 200.0;
    let drag = egui::DragValue::new(value).range(range).speed(speed);
    let drag = match unit {
        Unit::Db => drag.suffix(" dB").fixed_decimals(0),
        Unit::Percent => drag
            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
            .custom_parser(|s| s.trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)),
    };
    ui.add_sized(vec2(SLIDER_VALUE_W, theme::CONTROL_H), drag);
}

/// A faint explanatory line under a control.
pub(super) fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(theme::TEXT_FAINT));
}

/// A dialog's title, and a line explaining it unless `note` is empty.
pub(super) fn dialog_header(ui: &mut egui::Ui, title: &str, note: &str) {
    ui.label(RichText::new(title).size(17.0).strong().color(theme::TEXT));
    if !note.is_empty() {
        ui.add_space(2.0);
        ui.label(RichText::new(note).size(12.5).color(theme::TEXT_DIM));
    }
    ui.add_space(14.0);
}

/// The right-aligned button row that ends a dialog: add the main action first.
pub(super) fn dialog_footer(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(18.0);
    ui.with_layout(Layout::right_to_left(Align::Center), add);
}

pub(super) fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::TEXT_DIM));
    ui.add_space(-2.0);
}

/// A single-line text field, as tall as a button.
pub(super) fn text_input(value: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(value)
        .desired_width(f32::INFINITY)
        .min_size(vec2(0.0, theme::CONTROL_H))
        .align(Align2::LEFT_CENTER)
        .margin(Margin::symmetric(10, 0))
}

pub(super) fn field(ui: &mut egui::Ui, label: &str, value: &mut String, password: bool) -> egui::Response {
    field_label(ui, label);
    let resp = ui.add(text_input(value).password(password));
    ui.add_space(8.0);
    resp
}
