//! Visual identity: neutral greys with a warm peach accent, after
//! opencode's OC-2 desktop theme (anomalyco/opencode, `oc-2.json`, dark).

use egui::{self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
pub const SURFACE: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x23, 0x23, 0x23);
pub const SURFACE_ACTIVE: Color32 = Color32::from_rgb(0x2e, 0x2e, 0x2e);
pub const BORDER: Color32 = Color32::from_rgb(0x28, 0x28, 0x28);
const BORDER_STRONG: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);

pub const TEXT: Color32 = Color32::from_rgb(0xed, 0xed, 0xed);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xa0, 0xa0, 0xa0);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x70, 0x70, 0x70);

pub const ACCENT: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x83);
/// Text and icons drawn on an `ACCENT` fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0xff, 0xc1, 0xa4);
pub const ACCENT_BG: Color32 = Color32::from_rgba_premultiplied(0x2a, 0x1a, 0x10, 0xa0);

pub const GREEN: Color32 = Color32::from_rgb(0x49, 0xc9, 0x70);
pub const AMBER: Color32 = Color32::from_rgb(0xf6, 0xc2, 0x51);
pub const RED: Color32 = Color32::from_rgb(0xf1, 0x48, 0x4f);

pub const RADIUS: u8 = 6;

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 6.0);
        s.button_padding = egui::vec2(12.0, 6.0);
        s.interact_size.y = 30.0;
        s.window_margin = Margin::same(20);
        s.menu_margin = Margin::same(6);
        s.scroll.floating = true;
        s.scroll.bar_width = 6.0;

        style.animation_time = 0.12;
        style.interaction.selectable_labels = false;

        let v: &mut Visuals = &mut style.visuals;
        *v = Visuals::dark();
        v.override_text_color = None;
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.window_shadow = Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
        v.menu_corner_radius = CornerRadius::same(RADIUS);
        v.extreme_bg_color = BG;
        v.faint_bg_color = SURFACE;
        v.text_edit_bg_color = Some(SURFACE);
        v.hyperlink_color = ACCENT_TEXT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);
        v.error_fg_color = RED;
        v.warn_fg_color = AMBER;
        v.indent_has_left_vline = false;

        let r = CornerRadius::same(RADIUS);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = r;

        w.inactive.bg_fill = SURFACE;
        w.inactive.weak_bg_fill = SURFACE;
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;

        w.hovered.bg_fill = SURFACE_HOVER;
        w.hovered.weak_bg_fill = SURFACE_HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;

        w.active.bg_fill = SURFACE_ACTIVE;
        w.active.weak_bg_fill = SURFACE_ACTIVE;
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.5, TEXT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;

        w.open = w.active;
    });
}

/// Stable, pleasant color per nickname for avatars.
pub fn avatar_color(name: &str) -> Color32 {
    // OC-2 hues, mid-dark so white initials stay readable.
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

/// A primary (accent-filled) button.
pub fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(ON_ACCENT).strong())
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .corner_radius(RADIUS)
}
