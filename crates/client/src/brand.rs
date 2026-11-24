//! The T7 mark: the in-app logo and the window icon.

use egui::{Color32, CornerRadius, Rect, Stroke, pos2};

include!("brand_mark.rs");

pub fn paint(p: &egui::Painter, rect: Rect) {
    let unit = rect.width() / GRID as f32;
    let at = |(x, y): (u8, u8)| pos2(rect.left() + f32::from(x) * unit, rect.top() + f32::from(y) * unit);
    for k in MARK {
        let rgb = k.rgb();
        let color = Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        match *k {
            Shape::Rect { x, y, w, h, radius, .. } => {
                let shape = Rect::from_min_max(at((x, y)), at((x + w, y + h)));
                let radius = (f32::from(radius) * unit).round().min(255.0) as u8;
                p.rect_filled(shape, CornerRadius::same(radius), color);
            }
            Shape::Line { a, b, width, .. } => {
                let (a, b, w) = (at(a), at(b), f32::from(width) * unit);
                p.line_segment([a, b], Stroke::new(w, color));
                p.circle_filled(a, w / 2.0, color);
                p.circle_filled(b, w / 2.0, color);
            }
        }
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
        let from_svg: Vec<String> = svg
            .split('<')
            .filter_map(|t| {
                let a = |names: &[&str]| names.iter().map(|n| attr(t, n)).collect::<Vec<_>>().join(" ");
                if t.starts_with("rect ") {
                    Some(format!("rect {}", a(&["x", "y", "width", "height", "rx", "fill"])))
                } else if t.starts_with("line ") {
                    Some(format!("line {}", a(&["x1", "y1", "x2", "y2", "stroke-width", "stroke"])))
                } else {
                    None
                }
            })
            .collect();
        let from_mark: Vec<String> = MARK
            .iter()
            .map(|k| match *k {
                Shape::Rect { x, y, w, h, radius, rgb } => format!("rect {x} {y} {w} {h} {radius} #{rgb:06x}"),
                Shape::Line { a, b, width, rgb } => {
                    format!("line {} {} {} {} {width} #{rgb:06x}", a.0, a.1, b.0, b.1)
                }
            })
            .collect();
        assert_eq!(from_svg, from_mark);
    }

    #[test]
    fn mark_fits_the_grid() {
        let grid = GRID as f32;
        for k in MARK {
            match *k {
                Shape::Rect { x, y, w, h, radius, .. } => {
                    assert!(u32::from(x) + u32::from(w) <= GRID && u32::from(y) + u32::from(h) <= GRID);
                    assert!(2 * radius <= w.min(h));
                }
                Shape::Line { a, b, width, .. } => {
                    let half = f32::from(width) / 2.0;
                    for (x, y) in [a, b] {
                        let (x, y) = (f32::from(x), f32::from(y));
                        assert!(x - half >= 0.0 && x + half <= grid && y - half >= 0.0 && y + half <= grid);
                    }
                }
            }
        }
    }
}
