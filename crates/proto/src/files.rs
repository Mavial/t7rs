//! Protocol 10 attachment metadata and messages. File bytes travel on a
//! separate TLS connection from chat and are never sent in a control event.

use serde::{Deserialize, Serialize};

use crate::{ChannelId, Identity};

pub const VERSION: u16 = 10;
pub const CHUNK_BYTES: usize = 128 * 1024;
pub const GB: u64 = 1_000_000_000;
pub const DEFAULT_LIMIT: u64 = 4 * GB;
// TOML stores signed 64-bit integers.
pub const MAX_LIMIT: u64 = i64::MAX as u64;
pub const NAME_BYTES: usize = 240;
pub const PAGE_SIZE: usize = 24;

pub type FileId = [u8; 16];
pub type SessionKey = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cursor {
    pub posted_ms: u64,
    pub id: FileId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub key: Cursor,
    pub author: Identity,
    pub nickname: String,
    pub name: String,
    pub size: u64,
    pub caption: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelFile {
    pub channel: ChannelId,
    pub attachment: Attachment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Subscribe,
    List { request: u64, channel: Option<ChannelId>, before: Option<Cursor>, query: String },
    SetLimit(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Ready { limit: u64, key: SessionKey },
    Limit(u64),
    Access { readable: Vec<ChannelId>, can_post_here: bool },
    Page { request: u64, files: Vec<ChannelFile>, next: Option<Cursor> },
    Posted(ChannelFile),
    Error { request: Option<u64>, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Upload { channel: ChannelId, name: String, size: u64, caption: String },
    Download { id: FileId },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Upload {
    Chunk(Vec<u8>),
    Finish,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Ready { size: u64 },
    Chunk(Vec<u8>),
    Done,
    Error(String),
    Finalizing,
}

/// A portable display name and safe default for the native Save dialog.
/// Storage paths always use the opaque ID.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > NAME_BYTES
        || name.starts_with(' ')
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| {
            c.is_control()
                || "/\\:<>|\"?*".contains(c)
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        && !["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
        })
}

impl Attachment {
    pub fn matches_query(&self, channel_name: &str, lowercase_query: &str) -> bool {
        lowercase_query.is_empty()
            || [&*self.name, &*self.nickname, channel_name]
                .iter()
                .any(|text| text.to_lowercase().contains(lowercase_query))
    }
}
