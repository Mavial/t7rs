pub struct Nv12<'a> {
    pub y: &'a [u8],
    pub uv: &'a [u8],
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

const SHIFT: i32 = 13;
const fn fixed(x: f64) -> i32 {
    (x * (1 << SHIFT) as f64 + 0.5) as i32
}
// Y spans 16..=235 and U, V span 16..=240; BT.709 has Kr = 0.2126, Kb = 0.0722.
const Y: i32 = fixed(255.0 / 219.0);
const RV: i32 = fixed(1.5748 * 255.0 / 224.0);
const GU: i32 = fixed(0.187324 * 255.0 / 224.0);
const GV: i32 = fixed(0.468124 * 255.0 / 224.0);
const BU: i32 = fixed(1.8556 * 255.0 / 224.0);
const ROUND: i32 = 1 << (SHIFT - 1);

pub fn to_rgb(src: &Nv12, out: &mut [u32]) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        unsafe { to_rgb_avx2(src, out) };
        return;
    }
    rows(src, out);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn to_rgb_avx2(src: &Nv12, out: &mut [u32]) {
    rows(src, out);
}

#[allow(clippy::inline_always, reason = "AVX2 code is only generated for a body inlined into to_rgb_avx2")]
#[inline(always)]
fn rows(src: &Nv12, out: &mut [u32]) {
    let width = src.width;
    for (row, out) in out.chunks_exact_mut(width).take(src.height).enumerate() {
        let luma = &src.y[row * src.stride..][..width];
        let chroma = &src.uv[row / 2 * src.stride..][..width.next_multiple_of(2)];
        let (chroma, _) = chroma.as_chunks::<2>();
        let (out_pairs, out_last) = out.as_chunks_mut::<2>();
        let (luma_pairs, luma_last) = luma.as_chunks::<2>();
        for ((out, luma), chroma) in out_pairs.iter_mut().zip(luma_pairs).zip(chroma) {
            let tint = tint(*chroma);
            *out = [pixel(luma[0], tint), pixel(luma[1], tint)];
        }
        if let ([out], [luma], Some(&chroma)) = (out_last, luma_last, chroma.last()) {
            *out = pixel(*luma, tint(chroma));
        }
    }
}

fn tint([cb, cr]: [u8; 2]) -> [i32; 3] {
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    [RV * cr, -GU * cb - GV * cr, BU * cb]
}

fn pixel(luma: u8, [red, green, blue]: [i32; 3]) -> u32 {
    let base = (i32::from(luma) - 16) * Y + ROUND;
    let channel = |c: i32| ((base + c) >> SHIFT).clamp(0, 255) as u32;
    channel(red) << 16 | channel(green) << 8 | channel(blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(w: usize, h: usize, y: &[u8], uv: &[u8]) -> Vec<u32> {
        let mut out = vec![0; w * h];
        to_rgb(&Nv12 { y, uv, stride: w, width: w, height: h }, &mut out);
        out
    }

    fn close(a: u32, b: u32) -> bool {
        (0..3).all(|i| ((a >> (8 * i)) as u8).abs_diff((b >> (8 * i)) as u8) <= 1)
    }

    #[test]
    fn studio_swing_primaries_become_full_range_colors() {
        let cases = [
            ((16, 128, 128), 0x000000),
            ((235, 128, 128), 0xffffff),
            ((63, 102, 240), 0xff0000),
            ((173, 42, 26), 0x00ff00),
            ((32, 240, 118), 0x0000ff),
        ];
        for ((y, u, v), rgb) in cases {
            let got = convert(2, 2, &[y; 4], &[u, v])[0];
            assert!(close(got, rgb), "Y {y} U {u} V {v}: {got:06x}, want {rgb:06x}");
        }
    }

    #[test]
    fn each_two_by_two_block_shares_one_chroma_sample() {
        let stride = 5;
        let y = [100, 100, 63, 63, 0, 100, 100, 63, 63, 0];
        let uv = [128, 128, 102, 240, 0];
        let mut out = vec![0; 8];
        to_rgb(&Nv12 { y: &y, uv: &uv, stride, width: 4, height: 2 }, &mut out);
        let grey = convert(2, 2, &[100; 4], &[128, 128])[0];
        let red = convert(2, 2, &[63; 4], &[102, 240])[0];
        assert_eq!(out, [grey, grey, red, red, grey, grey, red, red]);
    }

    #[test]
    fn odd_widths_use_the_last_chroma_sample() {
        let out = convert(3, 1, &[63, 63, 63], &[128, 128, 102, 240]);
        assert!(close(out[2], 0xff0000), "{:06x}", out[2]);
    }

    /// `cargo test --release -p t7-client nv12_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run in release"]
    fn nv12_speed() {
        let (w, h) = (1920, 1080);
        let y: Vec<u8> = (0..w * h).map(|i| (i * 7 % 220 + 16) as u8).collect();
        let uv: Vec<u8> = (0..w * h / 2).map(|i| (i * 13 % 224 + 16) as u8).collect();
        let mut out = vec![0; w * h];
        let src = Nv12 { y: &y, uv: &uv, stride: w, width: w, height: h };
        let runs = 200;
        let start = std::time::Instant::now();
        for _ in 0..runs {
            to_rgb(&src, &mut out);
        }
        let per_frame = start.elapsed() / runs;
        println!(
            "1080p NV12 -> RGB: {per_frame:?} per frame, {:.1}% of one core at 60 fps",
            per_frame.as_secs_f64() * 6000.0
        );
        assert!(out.iter().any(|&p| p != 0));
    }
}
