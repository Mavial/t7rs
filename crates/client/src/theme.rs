//! Visual identity: neutral greys, after opencode's OC-2 desktop theme
//! (anomalyco/opencode, `oc-2.json`, dark), with TeamSpeak 3's blue as the accent.

use egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
pub const SURFACE: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x23, 0x23, 0x23);
pub const SURFACE_ACTIVE: Color32 = Color32::from_rgb(0x2e, 0x2e, 0x2e);
pub const BORDER: Color32 = Color32::from_rgb(0x28, 0x28, 0x28);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);

pub const TEXT: Color32 = Color32::from_rgb(0xed, 0xed, 0xed);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xa0, 0xa0, 0xa0);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x70, 0x70, 0x70);

pub const ACCENT: Color32 = Color32::from_rgb(0x25, 0x80, 0xc3);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x30, 0x8f, 0xd4);
const ACCENT_PRESSED: Color32 = Color32::from_rgb(0x1e, 0x6f, 0xab);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0x6a, 0xb4, 0xea);
pub const ACCENT_BG: Color32 = Color32::from_rgba_premultiplied(0x0e, 0x22, 0x34, 0xa0);

pub const GREEN: Color32 = Color32::from_rgb(0x49, 0xc9, 0x70);
pub const AMBER: Color32 = Color32::from_rgb(0xf6, 0xc2, 0x51);
pub const RED: Color32 = Color32::from_rgb(0xf1, 0x48, 0x4f);
const RED_FILL: Color32 = Color32::from_rgb(0xd9, 0x3a, 0x41);
const RED_HOVER: Color32 = Color32::from_rgb(0xe8, 0x46, 0x4d);
const RED_PRESSED: Color32 = Color32::from_rgb(0xbf, 0x31, 0x37);

pub const RADIUS: u8 = 6;
pub const CONTROL_H: f32 = 30.0;

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 6.0);
        s.button_padding = egui::vec2(10.0, 4.0);
        s.interact_size = egui::vec2(48.0, CONTROL_H);
        s.window_margin = Margin::same(20);
        s.menu_margin = Margin::same(4);
        s.combo_height = 260.0;
        s.slider_rail_height = 4.0;
        s.scroll.floating = true;
        s.scroll.bar_width = 6.0;

        style.animation_time = 0.1;
        style.interaction.selectable_labels = false;

        let v: &mut Visuals = &mut style.visuals;
        *v = Visuals::dark();
        v.override_text_color = None;
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(12);
        v.window_shadow = Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
        v.menu_corner_radius = CornerRadius::same(8);
        v.extreme_bg_color = BG;
        v.faint_bg_color = SURFACE;
        v.text_edit_bg_color = Some(SURFACE);
        v.hyperlink_color = ACCENT_TEXT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);
        v.error_fg_color = RED;
        v.warn_fg_color = AMBER;
        v.indent_has_left_vline = false;
        v.slider_trailing_fill = true;
        v.handle_shape = egui::style::HandleShape::Circle;

        let r = CornerRadius::same(RADIUS);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = r;

        // `bg_fill` paints slider rails and checkbox boxes, `weak_bg_fill`
        // buttons, combo boxes and menu items.
        w.inactive.bg_fill = SURFACE_ACTIVE;
        w.inactive.weak_bg_fill = SURFACE;
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;

        w.hovered.bg_fill = BORDER_STRONG;
        w.hovered.weak_bg_fill = SURFACE_HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;

        w.active.bg_fill = BORDER_STRONG;
        w.active.weak_bg_fill = SURFACE_ACTIVE;
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.5, TEXT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;

        w.open = w.active;
        w.open.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    });
}

pub fn avatar_color(name: &str) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0xd1, 0x64, 0x27),
        Color32::from_rgb(0xcb, 0x9f, 0x34),
        Color32::from_rgb(0x19, 0x8b, 0x43),
        Color32::from_rgb(0x00, 0x7d, 0x9b),
        Color32::from_rgb(0x32, 0x50, 0xdf),
        Color32::from_rgb(0xb8, 0x2d, 0x35),
        Color32::from_rgb(0xc8, 0x3d, 0x8b),
        Color32::from_rgb(0x5c, 0x5c, 0x5c),
    ];
    // FNV-1a: tiny and stable across runs and platforms.
    let hash = name.to_lowercase().bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
    PALETTE[hash as usize % PALETTE.len()]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Primary,
    Secondary,
    Danger,
    Ghost,
}

/// A button painted here rather than by egui, whose fill overrides drop the
/// hover and pressed states.
pub struct Btn<'a> {
    text: &'a str,
    kind: Kind,
    small: bool,
    min_width: f32,
}

impl<'a> Btn<'a> {
    fn new(text: &'a str, kind: Kind) -> Self {
        Self { text, kind, small: false, min_width: 72.0 }
    }

    pub fn primary(text: &'a str) -> Self {
        Self::new(text, Kind::Primary)
    }

    pub fn secondary(text: &'a str) -> Self {
        Self::new(text, Kind::Secondary)
    }

    pub fn danger(text: &'a str) -> Self {
        Self::new(text, Kind::Danger)
    }

    pub fn ghost(text: &'a str) -> Self {
        Self::new(text, Kind::Ghost)
    }

    pub fn small(mut self) -> Self {
        self.small = true;
        self.min_width = 0.0;
        self
    }

    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
}

impl egui::Widget for Btn<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let (height, pad, size) = if self.small { (24.0, 10.0, 12.5) } else { (CONTROL_H, 14.0, 13.5) };
        let font = FontId::proportional(size);
        let weak = self.kind == Kind::Secondary || self.kind == Kind::Ghost;
        let galley = ui.painter().layout_no_wrap(self.text.to_owned(), font, TEXT);
        let width = (galley.size().x + pad * 2.0).max(self.min_width);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let enabled = resp.enabled();
            let (hover, down) = (enabled && resp.hovered(), enabled && resp.is_pointer_button_down_on());
            let (fill, stroke, text) = match self.kind {
                Kind::Primary => (
                    if down {
                        ACCENT_PRESSED
                    } else if hover {
                        ACCENT_HOVER
                    } else {
                        ACCENT
                    },
                    Color32::TRANSPARENT,
                    ON_ACCENT,
                ),
                Kind::Danger => (
                    if down {
                        RED_PRESSED
                    } else if hover {
                        RED_HOVER
                    } else {
                        RED_FILL
                    },
                    Color32::TRANSPARENT,
                    ON_ACCENT,
                ),
                Kind::Secondary => (
                    if down {
                        SURFACE_ACTIVE
                    } else if hover {
                        SURFACE_HOVER
                    } else {
                        SURFACE
                    },
                    if hover { BORDER_STRONG } else { BORDER },
                    TEXT,
                ),
                Kind::Ghost => (
                    if down {
                        SURFACE_ACTIVE
                    } else if hover {
                        SURFACE_HOVER
                    } else {
                        Color32::TRANSPARENT
                    },
                    Color32::TRANSPARENT,
                    if hover { TEXT } else { TEXT_DIM },
                ),
            };
            let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.4) };
            let p = ui.painter();
            let radius = CornerRadius::same(RADIUS);
            p.rect(rect, radius, fade(fill), Stroke::new(1.0, fade(stroke)), egui::StrokeKind::Inside);
            if !weak && enabled {
                let top = egui::Rect::from_min_size(
                    rect.min + egui::vec2(f32::from(RADIUS), 0.5),
                    egui::vec2(rect.width() - 2.0 * f32::from(RADIUS), 1.0),
                );
                p.rect_filled(top, 0.0, Color32::from_white_alpha(if down { 0 } else { 28 }));
            }
            let pos = rect.center() - galley.size() / 2.0;
            p.galley_with_override_text_color(pos, galley, if enabled { text } else { TEXT_FAINT });
        }
        if resp.enabled() { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
    }
}

pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let font = FontId::proportional(13.0);
    let galleys: Vec<_> =
        options.iter().map(|(_, label)| ui.painter().layout_no_wrap((*label).to_owned(), font.clone(), TEXT)).collect();
    let inset = 3.0;
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 24.0).collect();
    let total = widths.iter().sum::<f32>() + inset * 2.0;
    let (track, _) = ui.allocate_exact_size(egui::vec2(total, CONTROL_H), egui::Sense::hover());
    ui.painter().rect(
        track,
        CornerRadius::same(RADIUS + 1),
        SURFACE,
        Stroke::new(1.0, BORDER),
        egui::StrokeKind::Inside,
    );
    let mut changed = false;
    let mut x = track.left() + inset;
    for (((option, label), galley), w) in options.iter().zip(galleys).zip(widths) {
        let rect =
            egui::Rect::from_min_size(egui::pos2(x, track.top() + inset), egui::vec2(w, CONTROL_H - inset * 2.0));
        x += w;
        let resp = ui.interact(rect, ui.scope_id().with(("segment", *label)), egui::Sense::click());
        let selected = *value == *option;
        if resp.clicked() && !selected {
            *value = *option;
            changed = true;
        }
        let p = ui.painter();
        if selected {
            p.rect(
                rect,
                CornerRadius::same(RADIUS - 1),
                SURFACE_ACTIVE,
                Stroke::new(1.0, BORDER_STRONG),
                egui::StrokeKind::Inside,
            );
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(RADIUS - 1), SURFACE_HOVER);
        }
        let color = if selected || resp.hovered() { TEXT } else { TEXT_DIM };
        p.galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, color);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    changed
}

pub fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let enabled = resp.enabled();
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let text = if enabled { TEXT } else { TEXT_FAINT };
    p.text(rect.left_center(), egui::Align2::LEFT_CENTER, label, FontId::proportional(13.5), text);
    let track =
        egui::Rect::from_min_size(egui::pos2(rect.right() - 30.0, rect.center().y - 9.0), egui::vec2(30.0, 18.0));
    let off = if resp.hovered() { BORDER_STRONG } else { SURFACE_ACTIVE };
    let fill = lerp_color(off, ACCENT, t);
    p.rect_filled(track, CornerRadius::same(9), if enabled { fill } else { fill.gamma_multiply(0.4) });
    let knob = egui::pos2(track.left() + 9.0 + t * 12.0, track.center().y);
    p.circle_filled(knob, 7.0, if enabled { Color32::WHITE } else { TEXT_FAINT });
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

pub fn modal(id: &str) -> egui::Modal {
    let frame = egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(20))
        .shadow(Shadow { offset: [0, 16], blur: 48, spread: 0, color: Color32::from_black_alpha(150) });
    egui::Modal::new(egui::Id::unique(id)).frame(frame).backdrop_color(Color32::from_black_alpha(150))
}
