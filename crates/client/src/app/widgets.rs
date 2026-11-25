//! Small widgets shared by the panels and dialogs.

use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, vec2};

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
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, color);
    resp.on_hover_cursor(egui::CursorIcon::NotAllowed).on_hover_text(tip);
}

pub(super) fn menu_button(ui: &mut egui::Ui, icon: Icon) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let lit = open || resp.hovered();
    if lit {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::SURFACE_HOVER);
    }
    let color = if lit { theme::TEXT } else { theme::TEXT_DIM };
    icons::paint(ui.painter(), egui::Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), icon, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub(super) fn truncated(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(0.0));
    ui.painter().layout_job(job)
}

pub(super) struct Item<'a> {
    icon: Icon,
    icon_color: Color32,
    label: &'a str,
    right: Option<&'a str>,
    danger: bool,
    submenu: bool,
}

impl<'a> Item<'a> {
    pub(super) fn new(icon: Icon, label: &'a str) -> Self {
        Self { icon, icon_color: theme::TEXT_FAINT, label, right: None, danger: false, submenu: false }
    }

    pub(super) fn right(mut self, text: &'a str) -> Self {
        self.right = Some(text);
        self
    }

    pub(super) fn icon_color(mut self, color: Color32) -> Self {
        self.icon_color = color;
        self
    }

    pub(super) fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub(super) fn submenu(mut self) -> Self {
        self.submenu = true;
        self
    }
}

impl egui::Widget for Item<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        const PAD: f32 = 8.0;
        const ICON: f32 = 14.0;
        let galley = ui.painter().layout_no_wrap(self.label.to_owned(), FontId::proportional(13.0), theme::TEXT);
        let right = self
            .right
            .map(|r| ui.painter().layout_no_wrap(r.to_owned(), FontId::proportional(11.5), theme::TEXT_FAINT));
        let chevron = if self.submenu { 16.0 } else { 0.0 };
        let right_w = right.as_ref().map_or(0.0, |g| g.size().x + 20.0) + chevron;
        let width = PAD + ICON + PAD + galley.size().x + right_w + PAD;
        // Justified in menus, so the highlight spans the whole menu.
        let resp = ui.allocate_response(vec2(width, 28.0), Sense::click());
        let rect = resp.rect;
        let enabled = resp.enabled();
        let p = ui.painter();
        if enabled && resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(5), theme::SURFACE_HOVER);
        }
        let (text, icon_color) = match (enabled, self.danger) {
            (false, _) => (theme::TEXT_FAINT, theme::TEXT_FAINT),
            (true, true) => (theme::RED, theme::RED),
            (true, false) => (theme::TEXT, self.icon_color),
        };
        let center = egui::pos2(rect.left() + PAD + ICON / 2.0, rect.center().y);
        icons::paint(p, egui::Rect::from_center_size(center, vec2(ICON, ICON)), self.icon, icon_color);
        let text_pos = egui::pos2(rect.left() + PAD * 2.0 + ICON, rect.center().y - galley.size().y / 2.0);
        p.galley_with_override_text_color(text_pos, galley, text);
        let mut right_edge = rect.right() - PAD;
        if self.submenu {
            let r = egui::Rect::from_center_size(egui::pos2(right_edge - 6.0, rect.center().y), vec2(12.0, 12.0));
            icons::paint(p, r, Icon::ChevronRight, theme::TEXT_FAINT);
            right_edge -= chevron;
        }
        if let Some(g) = right {
            let pos = egui::pos2(right_edge - g.size().x, rect.center().y - g.size().y / 2.0);
            p.galley(pos, g, theme::TEXT_FAINT);
        }
        resp
    }
}

pub(super) fn menu_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(RichText::new(text.to_uppercase()).size(10.5).strong().color(theme::TEXT_FAINT));
    });
    ui.add_space(2.0);
}

pub(super) fn menu_style(ui: &mut egui::Ui, min_width: f32) {
    ui.set_min_width(min_width);
    ui.spacing_mut().item_spacing.y = 0.0;
}

pub(super) fn menu_separator(ui: &mut egui::Ui) {
    ui.add(egui::Separator::default().spacing(9.0));
}

/// An icon toggle button, tinted with `on_color` while on (muted, deafened...).
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
    value_box(ui, value, range, unit);
}

pub(super) fn value_box(ui: &mut egui::Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, unit: Unit) {
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

pub(super) fn row_hint(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.add_space(ROW_LABEL_W);
        ui.vertical(|ui| hint(ui, text));
    });
}

pub(super) fn described_switch(ui: &mut egui::Ui, on: &mut bool, label: &str, note: &str) -> egui::Response {
    let resp = theme::switch(ui, on, label);
    ui.add_space(-6.0);
    hint(ui, note);
    ui.add_space(6.0);
    resp
}

pub(super) fn placeholder(text: &str) -> RichText {
    RichText::new(text).color(theme::TEXT_FAINT)
}

pub(super) fn field(ui: &mut egui::Ui, label: &str, edit: egui::TextEdit<'_>) -> egui::Response {
    field_label(ui, label);
    let resp = ui.add(edit);
    ui.add_space(8.0);
    resp
}

pub(super) fn list_row(ui: &mut egui::Ui, label: &str, note: &str, picked: bool, sense: Sense) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), sense);
    let p = ui.painter();
    let fill = if picked {
        theme::SURFACE_ACTIVE
    } else if resp.hovered() {
        theme::SURFACE_HOVER
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    let color = if picked || resp.hovered() { theme::TEXT } else { theme::TEXT_DIM };
    p.text(rect.left_center() + vec2(10.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(13.5), color);
    let note_pos = rect.right_center() - vec2(10.0, 0.0);
    p.text(note_pos, Align2::RIGHT_CENTER, note, FontId::proportional(11.5), theme::TEXT_FAINT);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A dialog's title, and a line explaining it unless `note` is empty.
pub(super) fn dialog_header(ui: &mut egui::Ui, title: &str, note: &str) {
    ui.label(RichText::new(title).size(17.0).strong().color(theme::TEXT));
    dialog_note(ui, note);
}

pub(super) fn person_header(ui: &mut egui::Ui, name: &str, title: &str, note: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
        paint_avatar(ui.painter(), rect.center(), 14.0, name);
        ui.label(RichText::new(title).size(17.0).strong().color(theme::TEXT));
    });
    dialog_note(ui, note);
}

fn dialog_note(ui: &mut egui::Ui, note: &str) {
    if !note.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(note).size(13.0).color(theme::TEXT_DIM));
    }
    ui.add_space(14.0);
}

/// The right-aligned button row that ends a dialog: add the main action first.
pub(super) fn dialog_footer(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(18.0);
    ui.with_layout(Layout::right_to_left(Align::Min), add);
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
