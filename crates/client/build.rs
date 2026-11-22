//! Embeds the T7 mark as the Windows executable's icon, so Explorer, shortcuts
//! and the taskbar show it before the window even opens.

use std::path::PathBuf;

#[allow(dead_code)]
mod mark {
    include!("src/brand_mark.rs");
}

fn main() {
    println!("cargo:rerun-if-changed=src/brand_mark.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("t7.ico");
    std::fs::write(&ico, ico_file(&[16, 24, 32, 48, 64, 256])).unwrap();
    let rc = out.join("t7.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/"))).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().unwrap();
}

/// A .ico holding one 32-bit BMP image per size.
fn ico_file(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_image(s)).collect();
    let mut f = Vec::new();
    f.extend_from_slice(&[0, 0, 1, 0]);
    f.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        f.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        f.extend_from_slice(&(img.len() as u32).to_le_bytes());
        f.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    images.iter().for_each(|img| f.extend_from_slice(img));
    f
}

/// BITMAPINFOHEADER, bottom-up BGRA pixels, then an all-zero AND mask
/// (the alpha channel does the masking).
fn bmp_image(size: u32) -> Vec<u8> {
    let rgba = mark::rasterize(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut f = Vec::new();
    for v in [40u32, size, size * 2] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    for _ in 0..6 {
        f.extend_from_slice(&0u32.to_le_bytes());
    }
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            f.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    f.resize(f.len() + (mask_row * size) as usize, 0);
    f
}
