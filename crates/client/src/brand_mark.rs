pub struct Block {
    pub x: u8,
    pub y: u8,
    pub w: u8,
    pub h: u8,
    pub rgb: u32,
}

pub const GRID: u32 = 64;

const fn b(x: u8, y: u8, w: u8, h: u8, rgb: u32) -> Block {
    Block { x, y, w, h, rgb }
}

pub const MARK: &[Block] = &[
    b(8, 0, 48, 64, 0x3a3a3a),
    b(4, 4, 56, 56, 0x3a3a3a),
    b(0, 8, 64, 48, 0x3a3a3a),
    b(8, 4, 48, 56, 0x161616),
    b(4, 8, 56, 48, 0x161616),
    b(8, 16, 24, 8, 0x2580c3),
    b(16, 24, 8, 24, 0x2580c3),
    b(36, 16, 20, 8, 0xededed),
    b(48, 24, 8, 8, 0xededed),
    b(44, 32, 8, 8, 0xededed),
    b(40, 40, 8, 8, 0xededed),
];

pub fn rasterize(size: u32) -> Vec<u8> {
    const SS: u32 = 8;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut bl, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let gx = (px * SS + sx) * 2 + 1;
                    let gy = (py * SS + sy) * 2 + 1;
                    let scale = size * SS * 2;
                    let hit = MARK.iter().rev().find(|k| {
                        let inside = |v: u32, lo: u8, len: u8| {
                            v * GRID >= u32::from(lo) * scale && v * GRID < (u32::from(lo) + u32::from(len)) * scale
                        };
                        inside(gx, k.x, k.w) && inside(gy, k.y, k.h)
                    });
                    if let Some(k) = hit {
                        r += k.rgb >> 16 & 0xff;
                        g += k.rgb >> 8 & 0xff;
                        bl += k.rgb & 0xff;
                        a += 1;
                    }
                }
            }
            let avg = |sum: u32| sum.checked_div(a).unwrap_or(0) as u8;
            out.extend_from_slice(&[avg(r), avg(g), avg(bl), (a * 255 / (SS * SS)) as u8]);
        }
    }
    out
}
