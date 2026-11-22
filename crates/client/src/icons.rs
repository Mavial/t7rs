//! Tiny vector icons (outline style) painted directly — no font or image
//! assets needed and razor sharp at any DPI. Coordinates use a 24×24 grid.

use std::f32::consts::PI;

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, pos2};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Speaker,
    Mic,
    MicOff,
    MicLocked,
    Headphones,
    HeadphonesOff,
    Moon,
    ChevronRight,
    Sliders,
    Lock,
    Shield,
    Star,
    Dots,
    ChevronDown,
    Eye,
    Check,
    Cross,
    Dash,
    Crown,
    SpeakerPlus,
}

struct Pen<'a> {
    p: &'a Painter,
    rect: Rect,
    stroke: Stroke,
}

impl Pen<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        let s = self.rect.width() / 24.0;
        pos2(self.rect.left() + x * s, self.rect.top() + y * s)
    }

    fn poly(&self, pts: &[(f32, f32)], closed: bool) {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        let shape = if closed { Shape::closed_line(pts, self.stroke) } else { Shape::line(pts, self.stroke) };
        self.p.add(shape);
    }

    fn arc_points(cx: f32, cy: f32, r: f32, from_deg: f32, to_deg: f32) -> Vec<(f32, f32)> {
        let steps = ((to_deg - from_deg).abs() / 10.0).ceil().max(2.0) as usize;
        (0..=steps)
            .map(|i| {
                let a = (from_deg + (to_deg - from_deg) * i as f32 / steps as f32) * PI / 180.0;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    }

    fn arc(&self, cx: f32, cy: f32, r: f32, from_deg: f32, to_deg: f32) {
        self.poly(&Self::arc_points(cx, cy, r, from_deg, to_deg), false);
    }

    fn line(&self, a: (f32, f32), b: (f32, f32)) {
        self.poly(&[a, b], false);
    }

    /// Rounded rectangle outline.
    fn rrect(&self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) {
        let mut pts = Vec::new();
        pts.extend(Self::arc_points(x1 - r, y0 + r, r, -90.0, 0.0));
        pts.extend(Self::arc_points(x1 - r, y1 - r, r, 0.0, 90.0));
        pts.extend(Self::arc_points(x0 + r, y1 - r, r, 90.0, 180.0));
        pts.extend(Self::arc_points(x0 + r, y0 + r, r, 180.0, 270.0));
        self.poly(&pts, true);
    }
}

pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let width = (rect.width() / 24.0 * 2.0).clamp(1.2, 2.2);
    let pen = Pen { p, rect, stroke: Stroke::new(width, color) };
    match icon {
        Icon::Speaker => {
            pen.poly(&[(3.0, 9.0), (7.0, 9.0), (12.0, 5.0), (12.0, 19.0), (7.0, 15.0), (3.0, 15.0)], true);
            pen.arc(12.0, 12.0, 4.5, -45.0, 45.0);
            pen.arc(12.0, 12.0, 8.5, -45.0, 45.0);
        }
        Icon::Mic | Icon::MicOff => {
            pen.rrect(9.0, 2.0, 15.0, 15.0, 3.0);
            pen.arc(12.0, 11.0, 7.0, 0.0, 180.0);
            pen.line((12.0, 18.0), (12.0, 22.0));
            if icon == Icon::MicOff {
                pen.line((3.0, 3.0), (21.0, 21.0));
            }
        }
        Icon::Headphones | Icon::HeadphonesOff => {
            pen.arc(12.0, 13.0, 9.0, 180.0, 360.0);
            pen.line((3.0, 13.0), (3.0, 18.0));
            pen.line((21.0, 13.0), (21.0, 18.0));
            pen.rrect(3.0, 14.0, 8.0, 21.0, 2.0);
            pen.rrect(16.0, 14.0, 21.0, 21.0, 2.0);
            if icon == Icon::HeadphonesOff {
                pen.line((2.0, 2.0), (22.0, 22.0));
            }
        }
        Icon::Sliders => {
            for (y, knob) in [(6.0, 15.0), (12.0, 8.0), (18.0, 17.0)] {
                pen.line((3.0, y), (knob - 2.5, y));
                pen.line((knob + 2.5, y), (21.0, y));
                pen.poly(&Pen::arc_points(knob, y, 2.5, 0.0, 360.0), true);
            }
        }
        Icon::MicLocked => {
            pen.rrect(5.0, 2.0, 10.5, 12.0, 2.75);
            pen.arc(7.75, 9.0, 5.0, 0.0, 180.0);
            pen.line((7.75, 14.0), (7.75, 20.0));
            pen.rrect(14.0, 14.5, 22.0, 21.5, 1.5);
            pen.arc(18.0, 14.5, 2.6, 180.0, 360.0);
        }
        Icon::Lock => {
            pen.rrect(5.0, 11.0, 19.0, 21.0, 2.0);
            pen.arc(12.0, 11.0, 4.0, 180.0, 360.0);
        }
        Icon::Shield => pen.poly(
            &[(12.0, 2.5), (20.0, 5.5), (20.0, 11.0), (17.5, 17.0), (12.0, 21.5), (6.5, 17.0), (4.0, 11.0), (4.0, 5.5)],
            true,
        ),
        Icon::Star => {
            let points: Vec<(f32, f32)> = (0..10)
                .map(|i| {
                    let r = if i % 2 == 0 { 10.0 } else { 4.2 };
                    let a = (-90.0 + i as f32 * 36.0) * PI / 180.0;
                    (12.0 + r * a.cos(), 12.8 + r * a.sin())
                })
                .collect();
            pen.poly(&points, true);
        }
        Icon::Dots => {
            for x in [5.0, 12.0, 19.0] {
                pen.poly(&Pen::arc_points(x, 12.0, 1.5, 0.0, 360.0), true);
            }
        }
        Icon::ChevronRight => pen.poly(&[(9.0, 6.0), (15.0, 12.0), (9.0, 18.0)], false),
        Icon::ChevronDown => pen.poly(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)], false),
        Icon::Eye => {
            // Two lids meeting at the corners, and the pupil.
            let mut lids = Pen::arc_points(12.0, 21.0, 12.0, 225.0, 315.0);
            lids.extend(Pen::arc_points(12.0, 4.0, 12.0, 45.0, 135.0));
            pen.poly(&lids, true);
            pen.poly(&Pen::arc_points(12.0, 12.5, 2.8, 0.0, 360.0), true);
        }
        Icon::Check => pen.poly(&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)], false),
        Icon::Cross => {
            pen.line((6.5, 6.5), (17.5, 17.5));
            pen.line((17.5, 6.5), (6.5, 17.5));
        }
        Icon::Dash => pen.line((6.5, 12.0), (17.5, 12.0)),
        Icon::SpeakerPlus => {
            pen.poly(&[(2.0, 9.0), (6.0, 9.0), (11.0, 5.0), (11.0, 19.0), (6.0, 15.0), (2.0, 15.0)], true);
            pen.line((14.5, 12.0), (21.5, 12.0));
            pen.line((18.0, 8.5), (18.0, 15.5));
        }
        Icon::Crown => pen
            .poly(&[(3.0, 18.5), (3.0, 7.5), (8.0, 12.0), (12.0, 5.0), (16.0, 12.0), (21.0, 7.5), (21.0, 18.5)], true),
        Icon::Moon => {
            // Crescent: big circle arc from (21,12) round to (12,3), back along a smaller arc.
            let mut pts = Pen::arc_points(12.0, 12.0, 9.0, 0.0, 270.0);
            pts.extend(Pen::arc_points(16.5, 7.5, 6.36, 225.0, 45.0));
            pen.poly(&pts, true);
        }
    }
}
