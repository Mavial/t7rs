//! The T7 mark: the in-app logo and the window icon.

use egui::{Color32, CornerRadius, Rect, pos2};

include!("brand_mark.rs");

/// Paints the mark into `rect`, snapped to whole pixels so block edges stay crisp.
pub fn paint(p: &egui::Painter, rect: Rect) {
    let unit = rect.width() / GRID as f32;
    let ppp = p.pixels_per_point();
    let snap = |v: f32| (v * ppp).round() / ppp;
    for k in MARK {
        let min = pos2(snap(rect.left() + f32::from(k.x) * unit), snap(rect.top() + f32::from(k.y) * unit));
        let max = pos2(snap(rect.left() + f32::from(k.x + k.w) * unit), snap(rect.top() + f32::from(k.y + k.h) * unit));
        let [r, g, b] = [(k.rgb >> 16) as u8, (k.rgb >> 8) as u8, k.rgb as u8];
        p.rect_filled(Rect::from_min_max(min, max), CornerRadius::ZERO, Color32::from_rgb(r, g, b));
    }
}

pub fn window_icon() -> Option<winit::window::Icon> {
    const SIZE: u32 = 64;
    winit::window::Icon::from_rgba(rasterize(SIZE), SIZE, SIZE).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// assets/icon.svg (README, docs) and MARK (app, window, .exe) must be the same picture.
    #[test]
    fn svg_matches_mark() {
        let svg = include_str!("../../../assets/icon.svg");
        let attr = |tag: &str, name: &str| -> String {
            let start = tag.find(&format!(" {name}=\"")).unwrap() + name.len() + 3;
            tag[start..].split('"').next().unwrap().to_owned()
        };
        let rects: Vec<(u8, u8, u8, u8, u32)> = svg
            .split("<rect")
            .skip(1)
            .map(|t| {
                let n = |k| attr(t, k).parse().unwrap();
                (n("x"), n("y"), n("width"), n("height"), u32::from_str_radix(&attr(t, "fill")[1..], 16).unwrap())
            })
            .collect();
        let mark: Vec<_> = MARK.iter().map(|k| (k.x, k.y, k.w, k.h, k.rgb)).collect();
        assert_eq!(rects, mark);
    }

    #[test]
    fn mark_fits_the_grid() {
        for k in MARK {
            assert!(u32::from(k.x) + u32::from(k.w) <= GRID && u32::from(k.y) + u32::from(k.h) <= GRID);
        }
    }
}
