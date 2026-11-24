// The T7 mark as data, shared by the client (`brand.rs`) and `build.rs`,
// which `include!` it. Plain std only: build.rs compiles it on its own.
// Keep in sync with assets/icon.svg.

pub enum Shape {
    Rect { x: u8, y: u8, w: u8, h: u8, radius: u8, rgb: u32 },
    Line { a: (u8, u8), b: (u8, u8), width: u8, rgb: u32 },
}

pub const GRID: u32 = 64;

const fn rect(x: u8, y: u8, w: u8, h: u8, radius: u8, rgb: u32) -> Shape {
    Shape::Rect { x, y, w, h, radius, rgb }
}

const fn line(a: (u8, u8), b: (u8, u8), width: u8, rgb: u32) -> Shape {
    Shape::Line { a, b, width, rgb }
}

pub const MARK: &[Shape] = &[
    // Tile: border, then fill.
    rect(0, 0, 64, 64, 14, 0x3a3a3a),
    rect(3, 3, 58, 58, 11, 0x161616),
    rect(9, 28, 8, 12, 4, 0xededed),
    rect(21, 21, 8, 26, 4, 0xededed),
    line((39, 20), (52, 20), 8, 0x6ab4ea),
    line((52, 20), (41, 44), 8, 0x6ab4ea),
];

impl Shape {
    pub fn rgb(&self) -> u32 {
        match *self {
            Shape::Rect { rgb, .. } | Shape::Line { rgb, .. } => rgb,
        }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        match *self {
            Shape::Rect { x, y, w, h, radius, .. } => {
                let (hw, hh, r) = (f32::from(w) / 2.0, f32::from(h) / 2.0, f32::from(radius));
                let qx = (px - f32::from(x) - hw).abs() - hw + r;
                let qy = (py - f32::from(y) - hh).abs() - hh + r;
                let outside = qx.max(0.0).hypot(qy.max(0.0));
                outside + qx.max(qy).min(0.0) <= r
            }
            Shape::Line { a, b, width, .. } => {
                let (ax, ay) = (f32::from(a.0), f32::from(a.1));
                let (dx, dy) = (f32::from(b.0) - ax, f32::from(b.1) - ay);
                let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                (px - ax - t * dx).hypot(py - ay - t * dy) <= f32::from(width) / 2.0
            }
        }
    }
}

/// Rasterizes the mark to `size`×`size` straight RGBA, 8×8 samples per pixel.
pub fn rasterize(size: u32) -> Vec<u8> {
    const SS: u32 = 8;
    let unit = GRID as f32 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut bl, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let gx = (px as f32 + (sx as f32 + 0.5) / SS as f32) * unit;
                    let gy = (py as f32 + (sy as f32 + 0.5) / SS as f32) * unit;
                    if let Some(k) = MARK.iter().rev().find(|k| k.contains(gx, gy)) {
                        let rgb = k.rgb();
                        r += rgb >> 16 & 0xff;
                        g += rgb >> 8 & 0xff;
                        bl += rgb & 0xff;
                        a += 1;
                    }
                }
            }
            // Color averaged over covered samples; alpha is the coverage.
            let avg = |sum: u32| sum.checked_div(a).unwrap_or(0) as u8;
            out.extend_from_slice(&[avg(r), avg(g), avg(bl), (a * 255 / (SS * SS)) as u8]);
        }
    }
    out
}
