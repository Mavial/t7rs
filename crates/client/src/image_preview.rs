//! Constrain image fetch, decoding and resize before pixels reach egui.

use std::io::{Cursor, Read};
use std::net::IpAddr;
use std::time::Duration;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageReader, Limits};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

pub const MAX_ENCODED: u64 = 8 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_DECODED: u64 = 64 * 1024 * 1024;
const PREVIEW_SIZE: [u32; 2] = [512, 320];

pub fn is_image_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        ["png", "jpg", "jpeg", "gif", "webp"].iter().any(|image| ext.eq_ignore_ascii_case(image))
    })
}

pub fn link(word: &str) -> Option<Uri> {
    let trimmed = word.trim_start_matches(['<', '(', '[']).trim_end_matches(['>', ')', ']', '.', ',', '!', '?']);
    let url: Uri = trimmed.parse().ok()?;
    if !url.scheme_str().is_some_and(|scheme| matches!(scheme, "http" | "https"))
        || url.authority().is_none_or(|a| a.as_str().contains('@'))
    {
        return None;
    }
    Some(url)
}

pub fn fetch(url: &Uri) -> Result<Option<egui::ColorImage>, String> {
    let config = Config::builder()
        .proxy(None)
        .max_redirects(3)
        .timeout_global(Some(Duration::from_secs(10)))
        .timeout_resolve(Some(Duration::from_secs(4)))
        .build();
    let agent = ureq::Agent::with_parts(config, DefaultConnector::default(), PublicResolver);
    let mut response =
        agent.get(url.clone()).header("Accept", "image/png,image/jpeg,image/gif,image/webp").call().map_err(error)?;
    if response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|value| value.starts_with("text/"))
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    response.body_mut().as_reader().take(MAX_ENCODED + 1).read_to_end(&mut bytes).map_err(error)?;
    if !is_image_name(url.path()) && image::guess_format(&bytes).is_err() {
        return Ok(None);
    }
    decode(&bytes).map(Some)
}

#[derive(Debug)]
struct PublicResolver;

impl Resolver for PublicResolver {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if !uri.scheme_str().is_some_and(|s| matches!(s, "http" | "https"))
            || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        {
            return Err(ureq::Error::HostNotFound);
        }
        let mut safe = self.empty();
        for &address in &DefaultResolver::default().resolve(uri, config, timeout)? {
            if public_address(address.ip()) {
                safe.push(address);
            }
        }
        if safe.is_empty() { Err(ureq::Error::HostNotFound) } else { Ok(safe) }
    }
}

fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_documentation()
                || matches!(a, 0 | 240..=255 | 224..=239)
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && matches!(b, 18 | 19))
                || (a == 192 && b == 0 && c == 0))
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            a & 0xe000 == 0x2000 && a != 0x2002 && !(a == 0x2001 && matches!(b, 0 | 0xdb8))
        }
    }
}

pub fn decode(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    if bytes.len() as u64 > MAX_ENCODED {
        return Err("Image is too large to preview".into());
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(error)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED);
    reader.limits(limits);
    let image = reader.decode().map_err(error)?;
    let image = if image.width() > PREVIEW_SIZE[0] || image.height() > PREVIEW_SIZE[1] {
        image.thumbnail(PREVIEW_SIZE[0], PREVIEW_SIZE[1])
    } else {
        image
    }
    .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

pub fn clipboard_png(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let [width, height] = image.size;
    if width == 0 || height == 0 || width.max(height) > MAX_DIMENSION as usize {
        return Err("Clipboard images can be up to 4096 × 4096 pixels".into());
    }
    let rgba: Vec<_> = image.pixels.iter().flat_map(egui::Color32::to_srgba_unmultiplied).collect();
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(&rgba, width as u32, height as u32, ExtendedColorType::Rgba8)
        .map_err(error)?;
    if bytes.len() as u64 > MAX_ENCODED {
        return Err("Clipboard image is too large. Save it to disk and attach it as a file".into());
    }
    Ok(bytes)
}

fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
