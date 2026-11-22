//! Visual identity: a calm dark theme with a violet accent.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Visuals,
};

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x0f, 0x13);
pub const PANEL: Color32 = Color32::from_rgb(0x13, 0x15, 0x1a);
pub const SURFACE: Color32 = Color32::from_rgb(0x1a, 0x1d, 0x24);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x22, 0x26, 0x30);
pub const SURFACE_ACTIVE: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x3b);
pub const BORDER: Color32 = Color32::from_rgb(0x24, 0x28, 0x31);

pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xef);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0x9f, 0xae);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x60, 0x65, 0x74);

pub const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x3a, 0xed);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0xc4, 0xb5, 0xfd);
pub const ACCENT_BG: Color32 = Color32::from_rgba_premultiplied(0x1f, 0x10, 0x3d, 0xa0);

pub const GREEN: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
pub const AMBER: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
pub const RED: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);

pub const RADIUS: u8 = 8;

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
        v.window_corner_radius = CornerRadius::same(14);
        v.window_shadow = Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
        v.menu_corner_radius = CornerRadius::same(RADIUS);
        v.extreme_bg_color = BG;
        v.faint_bg_color = SURFACE;
        v.text_edit_bg_color = Some(SURFACE);
        v.hyperlink_color = ACCENT_TEXT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
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
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x34, 0x39, 0x46));
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
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0x8b, 0x5c, 0xf6),
        Color32::from_rgb(0x3b, 0x82, 0xf6),
        Color32::from_rgb(0x06, 0xb6, 0xd4),
        Color32::from_rgb(0x10, 0xb9, 0x81),
        Color32::from_rgb(0x84, 0xcc, 0x16),
        Color32::from_rgb(0xf5, 0x9e, 0x0b),
        Color32::from_rgb(0xf4, 0x3f, 0x5e),
        Color32::from_rgb(0xec, 0x48, 0x99),
    ];
    // FNV-1a: tiny and stable across runs and platforms.
    let hash = name
        .to_lowercase()
        .bytes()
        .fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    PALETTE[hash as usize % PALETTE.len()]
}

/// A primary (accent-filled) button.
pub fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(Color32::WHITE).strong())
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .corner_radius(RADIUS)
}
