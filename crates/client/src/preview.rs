//! Stills of shared screens, for the shelf over the chat. A still is one
//! keyframe, decoded once on a thread of its own and scaled down to the size
//! it is drawn at; the decoder ends with the thread.

use std::sync::mpsc::Sender;
use std::time::Instant;

use egui::{Color32, ColorImage};
use t7_proto::ClientId;

use crate::net::PreviewSink;
use crate::screen::scale::{Area, Scaler};
use crate::screen::{AccessUnit, Decoder, Picture};

/// A still of someone's screen, as it comes back from its thread.
pub type Still = (ClientId, ColorImage);

/// Decodes the keyframe it gets into a `size`-pixel still of `sharer`'s
/// screen, sends it to `stills`, and wakes the UI.
pub fn sink(sharer: ClientId, size: [usize; 2], stills: Sender<Still>, ctx: egui::Context) -> PreviewSink {
    Box::new(move |au| {
        let decode = move || {
            let started = Instant::now();
            match still(&au, size) {
                Ok(image) => {
                    let (bytes, took) = (au.data.len(), started.elapsed());
                    tracing::debug!(sharer, bytes, ?took, "screen preview");
                    if stills.send((sharer, image)).is_ok() {
                        ctx.request_repaint();
                    }
                }
                Err(e) => tracing::warn!("screen preview: {e}"),
            }
        };
        if let Err(e) = std::thread::Builder::new().name("screen preview".into()).spawn(decode) {
            tracing::warn!("screen preview thread: {e}");
        }
    })
}

fn still(au: &AccessUnit, size: [usize; 2]) -> Result<ColorImage, String> {
    let picture = Decoder::new()?.decode(au)?.ok_or("the keyframe made no picture")?;
    Ok(thumbnail(&picture, size))
}

/// `picture` scaled to fit `size`, centered, with black bars where its shape differs.
fn thumbnail(picture: &Picture, size: [usize; 2]) -> ColorImage {
    let [width, height] = size;
    let mut pixels = vec![0; width * height];
    Scaler::default().draw(picture, &mut pixels, width, Area::whole(width, height));
    let rgb = |p: u32| {
        let [b, g, r, _] = p.to_le_bytes();
        Color32::from_rgb(r, g, b)
    };
    ColorImage::new(size, pixels.into_iter().map(rgb).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, pixel: u32) -> Picture {
        Picture { width, height, pixels: vec![pixel; (width * height) as usize] }
    }

    #[test]
    fn a_wide_screen_fills_the_still() {
        let still = thumbnail(&picture(1920, 1080, 0x0012_3456), [176, 99]);
        assert_eq!(still.size, [176, 99]);
        assert!(still.pixels.iter().all(|&p| p == Color32::from_rgb(0x12, 0x34, 0x56)));
    }

    #[test]
    fn a_square_screen_gets_black_bars_at_the_sides() {
        let still = thumbnail(&picture(1000, 1000, 0x00FF_FFFF), [320, 180]);
        let row = &still.pixels[..320];
        let white = row.iter().filter(|&&p| p == Color32::WHITE).count();
        assert_eq!(white, 180);
        assert_eq!(row[69], Color32::BLACK);
        assert_eq!(row[70], Color32::WHITE);
        assert_eq!(row[250], Color32::BLACK);
    }
}
