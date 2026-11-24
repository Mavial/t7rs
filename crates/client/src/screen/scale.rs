//! Fits a picture into a window, its aspect ratio kept and black around it.
//! Shrinking averages every picture pixel a window pixel covers, so thin
//! text stays readable. Enlarging repeats pixels, which keeps text sharp and
//! costs little more than a copy. Weights are 8-bit fixed point, and each
//! channel is weighed in a 16-bit lane: a row's bytes at once down the
//! column, and a pixel's four channels in one `u64` along the row.

use super::Picture;

/// Draws pictures scaled to fit, keeping the weights while sizes stay the same.
#[derive(Default)]
pub struct Scaler {
    /// Picture width and height, then the size drawn at.
    sizes: (usize, usize, usize, usize),
    x: Axis,
    y: Axis,
    /// One picture row weighed down the column, a pixel's channels in the
    /// 16-bit lanes of each `u64`.
    row: Vec<u64>,
}

impl Scaler {
    /// Draws `picture` into `out`, `width` × `height` pixels in rows, as
    /// large as fits and centered.
    pub fn draw(&mut self, picture: &Picture, out: &mut [u32], width: usize, height: usize) {
        let (pw, ph) = (picture.width as usize, picture.height as usize);
        let out = &mut out[..width * height];
        if out.is_empty() {
            return;
        }
        if pw == 0 || ph == 0 || picture.pixels.len() < pw * ph {
            return out.fill(0);
        }
        let (x0, y0, w, h) = fit((pw, ph), (width, height));
        if self.sizes != (pw, ph, w, h) {
            self.sizes = (pw, ph, w, h);
            self.x = Axis::new(pw, w);
            self.y = Axis::new(ph, h);
            self.row = vec![0; pw];
        }
        out[..y0 * width].fill(0);
        out[(y0 + h) * width..].fill(0);
        for i in 0..h {
            let at = (y0 + i) * width;
            let (src_y, _) = self.y.at(i);
            // Enlarging: a row the same as the one above.
            if self.y.taps == 1 && i > 0 && self.y.at(i - 1).0 == src_y {
                out.copy_within(at - width..at, at);
                continue;
            }
            let row = &mut out[at..at + width];
            row[..x0].fill(0);
            row[x0 + w..].fill(0);
            let row = &mut row[x0..x0 + w];
            let src = &picture.pixels[src_y * pw..][..pw];
            if w == pw && self.y.taps == 1 {
                row.copy_from_slice(src);
            } else if self.x.taps == 1 && self.y.taps == 1 {
                for (o, &x) in row.iter_mut().zip(&self.x.start) {
                    *o = src[x];
                }
            } else {
                self.down_the_column(picture, i);
                self.along_the_row(row);
            }
        }
    }

    /// Weighs the picture rows that output row `i` covers into `self.row`.
    fn down_the_column(&mut self, picture: &Picture, i: usize) {
        let pw = picture.width as usize;
        let (start, weights) = self.y.at(i);
        let lanes: &mut [u16] = bytemuck::cast_slice_mut(&mut self.row);
        lanes.fill(0);
        for (k, &weight) in weights.iter().enumerate().filter(|(_, w)| **w > 0) {
            let src: &[u8] = bytemuck::cast_slice(&picture.pixels[(start + k) * pw..][..pw]);
            let weight = weight as u16;
            for (lane, &channel) in lanes.iter_mut().zip(src) {
                *lane += u16::from(channel) * weight;
            }
        }
        for lane in lanes {
            *lane = (*lane + 0x80) >> 8;
        }
    }

    fn along_the_row(&self, out: &mut [u32]) {
        // A tap count known at compile time unrolls the inner loop.
        match self.x.taps {
            2 => self.along::<2>(out),
            3 => self.along::<3>(out),
            4 => self.along::<4>(out),
            _ => self.along::<0>(out),
        }
    }

    /// `along_the_row` with `TAPS` taps, or any number with 0.
    fn along<const TAPS: usize>(&self, out: &mut [u32]) {
        let taps = if TAPS == 0 { self.x.taps } else { TAPS };
        let weights = self.x.weights.chunks_exact(taps);
        for ((o, &start), weights) in out.iter_mut().zip(&self.x.start).zip(weights) {
            let mut sum = 0;
            for (&p, &weight) in self.row[start..start + taps].iter().zip(weights) {
                sum += p * u64::from(weight);
            }
            *o = pack(sum);
        }
    }
}

/// One pixel from its four channels weighed by 256 in 16-bit lanes, rounded.
fn pack(lanes: u64) -> u32 {
    let lanes = ((lanes + 0x0080_0080_0080_0080) >> 8) & 0x00FF_00FF_00FF_00FF;
    let bytes = lanes | lanes >> 8;
    ((bytes & 0xFFFF) | ((bytes >> 16) & 0xFFFF_0000)) as u32
}

/// Where a `picture`-sized image goes in a `window` of at least 1 × 1: x, y, width, height.
fn fit(picture: (usize, usize), window: (usize, usize)) -> (usize, usize, usize, usize) {
    let scale = (window.0 as f64 / picture.0 as f64).min(window.1 as f64 / picture.1 as f64);
    let w = ((picture.0 as f64 * scale).round() as usize).clamp(1, window.0);
    let h = ((picture.1 as f64 * scale).round() as usize).clamp(1, window.1);
    ((window.0 - w) / 2, (window.1 - h) / 2, w, h)
}

/// For each output pixel along one axis, the `taps` source pixels from its
/// start that make it up and their weights, which sum to 256.
#[derive(Default)]
struct Axis {
    taps: usize,
    start: Vec<usize>,
    weights: Vec<u32>,
}

impl Axis {
    fn new(src: usize, dst: usize) -> Axis {
        let scale = src as f64 / dst as f64;
        // Each output pixel's source pixels, with how much of each it covers.
        let span = |i: usize| -> Vec<(usize, f64)> {
            if scale <= 1.0 {
                return vec![((((i as f64 + 0.5) * scale) as usize).min(src - 1), 1.0)];
            }
            let (lo, hi) = (i as f64 * scale, (i + 1) as f64 * scale);
            (lo as usize..(hi.ceil() as usize).min(src))
                .map(|j| (j, hi.min(j as f64 + 1.0) - lo.max(j as f64)))
                .collect()
        };
        let spans: Vec<_> = (0..dst).map(span).collect();
        let taps = spans.iter().map(Vec::len).max().unwrap_or(1);
        let mut axis = Axis { taps, start: Vec::with_capacity(dst), weights: vec![0; dst * taps] };
        for (span, weights) in spans.iter().zip(axis.weights.chunks_exact_mut(taps)) {
            let total: f64 = span.iter().map(|c| c.1).sum();
            let start = span[0].0.min(src - taps);
            for &(j, w) in span {
                weights[j - start] += (w / total * 256.0).round() as u32;
            }
            // Rounding can leave the sum off by a little: the heaviest takes it.
            let sum: u32 = weights.iter().sum();
            let heaviest = (0..taps).max_by_key(|&k| weights[k]).unwrap_or(0);
            weights[heaviest] = (weights[heaviest] + 256).saturating_sub(sum);
            axis.start.push(start);
        }
        axis
    }

    fn at(&self, i: usize) -> (usize, &[u32]) {
        (self.start[i], &self.weights[i * self.taps..][..self.taps])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u32) -> Picture {
        let pixels = (0..height).flat_map(|y| (0..width).map(move |x| (x, y))).map(|(x, y)| pixel(x, y)).collect();
        Picture { width, height, pixels }
    }

    fn draw(p: &Picture, width: usize, height: usize) -> Vec<u32> {
        let mut out = vec![0xDEAD_BEEF; width * height];
        Scaler::default().draw(p, &mut out, width, height);
        out
    }

    #[test]
    fn a_picture_the_size_of_the_window_is_copied() {
        let p = picture(4, 3, |x, y| x * 0x10 + y * 0x1000);
        assert_eq!(draw(&p, 4, 3), p.pixels);
    }

    #[test]
    fn a_wide_picture_is_centered_with_black_above_and_below() {
        let out = draw(&picture(4, 2, |_, _| 0x00FF_8040), 2, 3);
        assert_eq!(out, [0, 0, 0x00FF_8040, 0x00FF_8040, 0, 0]);
        let out = draw(&picture(1, 2, |_, _| 0x0012_3456), 3, 2);
        assert_eq!(out, [0, 0x0012_3456, 0, 0, 0x0012_3456, 0]);
    }

    #[test]
    fn one_color_stays_that_color_at_any_size() {
        let p = picture(1920, 1080, |_, _| 0x00C8_6432);
        for (w, h) in [(1280, 720), (853, 480), (333, 187), (2560, 1440), (3000, 1000)] {
            let (x0, y0, fw, fh) = fit((1920, 1080), (w, h));
            let out = draw(&p, w, h);
            let inside = (y0..y0 + fh).flat_map(|y| (x0..x0 + fw).map(move |x| y * w + x));
            assert!(inside.map(|i| out[i]).all(|px| px == 0x00C8_6432), "{w}x{h}");
        }
    }

    #[test]
    fn enlarging_repeats_pixels() {
        let p = picture(2, 1, |x, _| x + 1);
        assert_eq!(draw(&p, 4, 2), [1, 1, 2, 2, 1, 1, 2, 2]);
        assert_eq!(draw(&p, 6, 2), [0, 1, 1, 2, 2, 0, 0, 1, 1, 2, 2, 0]);
    }

    #[test]
    fn a_one_pixel_line_shrunk_stays_visible() {
        // White one-pixel columns every 3 pixels, shrunk to a third: every
        // output column covers one, so all come out a third of white.
        let p = picture(300, 3, |x, _| if x % 3 == 1 { 0x00FF_FFFF } else { 0 });
        let out = draw(&p, 100, 1);
        let gray = |px: u32| px.to_le_bytes()[..3].iter().all(|c| (0x54..=0x56).contains(c));
        assert!(out.iter().all(|&px| px == out[0] && gray(px)), "{:x?}", &out[..4]);
    }

    #[test]
    #[ignore = "a timing, not a check: cargo test --release -p t7-client scaling_cost -- --ignored --nocapture"]
    fn scaling_cost() {
        let p = picture(1920, 1080, |x, y| (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) & 0x00FF_FFFF);
        for (w, h) in [(1920, 1080), (1874, 1054), (1600, 900), (1280, 720), (960, 540), (2560, 1440), (3840, 2160)] {
            let mut scaler = Scaler::default();
            let mut out = vec![0; w * h];
            scaler.draw(&p, &mut out, w, h);
            let start = std::time::Instant::now();
            let runs = 60;
            for _ in 0..runs {
                scaler.draw(&p, &mut out, w, h);
            }
            println!("1920x1080 into {w}x{h}: {:.2} ms", start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs));
        }
    }
}
