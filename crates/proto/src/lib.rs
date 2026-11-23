//! Wire protocol shared by the T7 server and client.
//!
//! Every message travels over TLS 1.3 on TCP (see [`tls`]) as a frame: a
//! little-endian `u32` payload length followed by a [postcard]-encoded
//! message. Postcard is a compact varint-based binary format, so typical
//! control messages are only a few bytes on the wire and (de)serialisation is
//! extremely cheap.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod identity;
pub mod model;
pub mod perms;
pub mod tls;
pub mod update;
pub mod voice;

/// This build's protocol. New messages are only ever appended to the enums
/// below, so older clients keep working for everything they know about.
pub const PROTOCOL_VERSION: u16 = 6;
/// Oldest client protocol a server still accepts.
pub const MIN_PROTOCOL_VERSION: u16 = 6;
/// Default port of a T7 server (TCP for control, UDP for voice).
pub const DEFAULT_PORT: u16 = 9988;
/// Upper bound for a single frame. Protects both sides from memory abuse.
pub const MAX_FRAME_LEN: usize = 256 * 1024;

pub const MAX_NICKNAME_LEN: usize = 32;
pub const MAX_TEXT_LEN: usize = 2000;
pub const MAX_POKE_LEN: usize = 100;
/// For kick and ban reasons.
pub const MAX_REASON_LEN: usize = 100;
pub const MAX_CHANNEL_NAME_LEN: usize = 40;
pub const MAX_CHANNEL_TOPIC_LEN: usize = 200;
/// Channels nest at most this deep (top level is depth 1).
pub const MAX_CHANNEL_DEPTH: usize = 3;
pub const MAX_CHANNELS: usize = 200;
pub const MAX_ROLE_NAME_LEN: usize = 32;
/// Roles a server may have, besides Everyone.
pub const MAX_ROLES: usize = 32;

/// How often the client sends a ping, and how long the server waits for
/// any traffic before considering the connection dead.
pub const PING_INTERVAL_SECS: u64 = 5;
pub const IDLE_TIMEOUT_SECS: u64 = 30;

pub type ClientId = u32;
pub type ChannelId = u32;
pub type BanId = u32;

pub use identity::{Identity, IdentityProof};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ClientStatus {
    /// Microphone muted by the user.
    pub muted: bool,
    /// Speakers/headphones muted by the user.
    pub deafened: bool,
    pub away: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub welcome: String,
    pub max_clients: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: ChannelId,
    pub parent: Option<ChannelId>,
    pub name: String,
    pub topic: String,
    /// `None` means unlimited.
    pub max_clients: Option<u32>,
    /// Sort key among siblings (lower first).
    pub order: u32,
    /// Deleted automatically once nobody is in it (or in its sub-channels).
    pub temporary: bool,
    pub has_password: bool,
    /// Where new clients land.
    pub default: bool,
    /// Who owns a temporary channel: they moderate it, and may edit and
    /// delete it. Its creator, then whoever has been in it longest.
    pub owner: Option<ClientId>,
    /// They hold the Moderator role in this channel and every channel below it.
    pub moderators: Vec<Identity>,
    pub kind: ChannelKind,
    /// What it sets for each role, over the channel above it. Only clients
    /// that may edit permissions get them.
    pub overrides: Vec<perms::Override>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    #[default]
    Voice,
    /// Groups the channels under it, at the top level. Nobody joins it.
    Category,
    /// Joining it makes you a temporary room of your own under it, which you
    /// own. Nobody stays in it.
    Lobby,
}

impl ChannelKind {
    pub fn levels_including_rooms(self) -> usize {
        if self == ChannelKind::Lobby { 2 } else { 1 }
    }
}

/// The editable part of a channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSettings {
    pub name: String,
    pub topic: String,
    pub max_clients: Option<u32>,
    pub password: PasswordChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PasswordChange {
    Keep,
    Remove,
    Set(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: ClientId,
    pub nickname: String,
    pub channel: ChannelId,
    pub status: ClientStatus,
    /// Their highest role, shown next to their name: "Owner" for the owner,
    /// `None` with only Everyone.
    pub role: Option<String>,
    /// The same on every connection with the same key; two connections may share one.
    pub identity: Identity,
    pub speech: Speech,
}

/// Whether the server passes someone's voice on, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Speech {
    #[default]
    Allowed,
    /// They may not speak in the channel they are in.
    NotHere,
    /// A moderator muted them on the server, until one unmutes them or the
    /// server restarts.
    ServerMuted,
}

/// A ban on the server: the identity, and the IP address if that was banned too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanInfo {
    pub id: BanId,
    pub identity: Identity,
    pub ip: Option<IpAddr>,
    /// Their nickname when they were banned.
    pub nickname: String,
    pub reason: String,
    /// Who banned them, by nickname.
    pub banned_by: String,
    pub created_unix: u64,
    /// `None`: permanent.
    pub expires_unix: Option<u64>,
}

/// Where a text message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextTarget {
    /// Everyone on the server.
    Server,
    /// Everyone in the sender's current channel.
    Channel,
    /// A single client (private message).
    Client(ClientId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaveReason {
    Disconnected,
    TimedOut,
    Kicked,
}

/// Client → server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum C2S {
    /// Must be the first message on a new connection.
    Hello {
        protocol: u16,
        nickname: String,
        password: Option<String>,
        identity: IdentityProof,
    },
    /// `password` is needed for password-protected channels, unless they may bypass it.
    JoinChannel {
        channel: ChannelId,
        password: Option<String>,
    },
    SendText {
        target: TextTarget,
        text: String,
    },
    SetStatus(ClientStatus),
    Ping {
        nonce: u64,
    },
    Goodbye,
    /// Get someone's attention: a popup and a sound on their side (protocol 3).
    Poke {
        target: ClientId,
        message: String,
    },
    /// The creator moves into the new channel, if it is a voice channel.
    CreateChannel {
        parent: Option<ChannelId>,
        settings: ChannelSettings,
        temporary: bool,
        kind: ChannelKind,
    },
    EditChannel {
        id: ChannelId,
        settings: ChannelSettings,
    },
    /// `position` is the index among the new siblings the client sees, not
    /// counting the moved channel.
    MoveChannel {
        id: ChannelId,
        parent: Option<ChannelId>,
        position: u32,
    },
    /// People inside move out the way a kick from the channel moves them.
    DeleteChannel {
        id: ChannelId,
    },
    SetDefaultChannel {
        id: ChannelId,
    },
    /// Become the owner with the claim token the server printed. It works once.
    ClaimOwnership {
        token: String,
    },
    SetModerator {
        channel: ChannelId,
        identity: Identity,
        moderator: bool,
    },
    /// Out of their channel to where a new client would land, or off the server.
    Kick {
        target: ClientId,
        from_server: bool,
        reason: String,
    },
    /// Bans their identity, and their IP address with `include_ip`, for
    /// `duration_secs` (`None`: for good), and kicks off everyone it covers.
    Ban {
        target: ClientId,
        duration_secs: Option<u64>,
        reason: String,
        include_ip: bool,
    },
    /// Answered with [`S2C::Bans`].
    ListBans,
    /// Answered with the new [`S2C::Bans`].
    Unban {
        id: BanId,
    },
    /// Answered, like every change to the roles, with [`S2C::Roles`].
    ChangeRoles(perms::RoleChange),
    /// Replaces what `channel` allows and denies the role, for a role below
    /// your own where you may manage permissions. Setting nothing removes it.
    SetOverride {
        channel: ChannelId,
        role: perms::RoleId,
        allow: perms::Perms,
        deny: perms::Perms,
    },
    /// Mutes someone on the server, or lifts it, if you may mute people where
    /// they are and their role is below yours. It sticks to their identity
    /// across reconnects.
    ServerMute {
        target: ClientId,
        muted: bool,
    },
    /// Moves someone into `channel`, if you may move people out of where they
    /// are and into `channel`, their role is below yours, and they may see
    /// it. A full channel, or one with a password, needs your Bypass.
    MoveMember {
        target: ClientId,
        channel: ChannelId,
    },
}

/// Server → client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum S2C {
    /// Successful handshake: full snapshot of the server state.
    Welcome {
        your_id: ClientId,
        /// Seals this client's voice datagrams, both ways.
        voice_key: voice::VoiceKey,
        server: ServerInfo,
        /// The channels this client sees, and the people in them.
        channels: Vec<ChannelInfo>,
        clients: Vec<ClientInfo>,
        permissions: perms::Permissions,
    },
    /// Handshake refused; the server closes the connection afterwards.
    /// Servers also send it, outside TLS, to clients too old for TLS.
    Rejected {
        reason: String,
    },
    ClientJoined(ClientInfo),
    ClientLeft {
        id: ClientId,
        reason: LeaveReason,
    },
    ClientMoved {
        id: ClientId,
        channel: ChannelId,
    },
    ClientStatus {
        id: ClientId,
        status: ClientStatus,
    },
    Text {
        from: ClientId,
        /// `Channel` text carries the channel it was sent in, so clients can
        /// keep history straight even if they move channels quickly.
        target: TextTarget,
        channel: ChannelId,
        text: String,
    },
    /// A non-fatal error caused by the last request (e.g. channel full).
    Error {
        message: String,
    },
    Pong {
        nonce: u64,
    },
    /// Someone poked you (protocol 3).
    Poked {
        from: ClientId,
        message: String,
    },
    /// Every channel this client sees, sent after any change to them.
    Channels(Vec<ChannelInfo>),
    ClientRole {
        id: ClientId,
        role: Option<String>,
    },
    /// Sent to everyone before the kicked client is moved to the default
    /// channel, or disconnected with [`LeaveReason::Kicked`].
    ClientKicked {
        id: ClientId,
        by: ClientId,
        from_server: bool,
        reason: String,
    },
    /// The ban list, for someone who may ban and asked.
    Bans(Vec<BanInfo>),
    /// What this client may do, sent whenever that changes.
    Permissions(perms::Permissions),
    /// Every role and who holds them, for clients that may edit roles or
    /// channel permissions, sent whenever they change.
    Roles(perms::Roles),
    /// Someone already connected came into view: they moved into a channel
    /// this client sees, or it now sees theirs.
    ClientShown(ClientInfo),
    /// Someone went out of view, the other way around. They are still connected.
    ClientHidden {
        id: ClientId,
    },
    /// Whether the server passes someone's voice on changed.
    ClientSpeech {
        id: ClientId,
        speech: Speech,
    },
}

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    TooLarge(usize),
    Decode(postcard::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "io error: {e}"),
            FrameError::TooLarge(n) => write!(f, "frame too large ({n} bytes)"),
            FrameError::Decode(e) => write!(f, "malformed message: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Encodes a message into a complete frame (length prefix + payload).
///
/// The result can be shared (e.g. via `Arc<[u8]>`) and written to many
/// sockets, so a broadcast is serialised exactly once.
pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut buf = vec![0u8; 4];
    let mut buf = postcard::to_extend(msg, std::mem::take(&mut buf)).expect("serialising into a Vec cannot fail");
    let len = (buf.len() - 4) as u32;
    buf[..4].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Reads one frame and decodes it. `scratch` is reused between calls to
/// avoid an allocation per message.
pub async fn read_frame<R, T>(reader: &mut R, scratch: &mut Vec<u8>) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len = [0u8; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    scratch.resize(len, 0);
    reader.read_exact(scratch).await?;
    postcard::from_bytes(scratch).map_err(FrameError::Decode)
}

/// Writes one frame and flushes it, so it doesn't wait in a TLS or write buffer.
pub async fn write_frame<W, T>(writer: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    writer.write_all(&encode_frame(msg)).await?;
    writer.flush().await?;
    Ok(())
}

/// Normalises and validates a nickname. Returns the cleaned name.
pub fn validate_nickname(raw: &str) -> Result<String, &'static str> {
    let name: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    if name.is_empty() {
        return Err("nickname must not be empty");
    }
    if name.chars().count() > MAX_NICKNAME_LEN {
        return Err("nickname is too long");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let msg = C2S::Hello {
            protocol: PROTOCOL_VERSION,
            nickname: "alice".into(),
            password: None,
            identity: identity::IdentityKey::from_seed([3; 32]).prove(&[4; 32]),
        };
        let frame = encode_frame(&msg);
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize, frame.len() - 4);
        let mut cursor = std::io::Cursor::new(frame);
        let mut scratch = Vec::new();
        let decoded: C2S = read_frame(&mut cursor, &mut scratch).await.unwrap();
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn rejects_oversized_frames() {
        let mut data = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes().to_vec();
        data.extend_from_slice(&[0; 8]);
        let mut cursor = std::io::Cursor::new(data);
        let res: Result<C2S, _> = read_frame(&mut cursor, &mut Vec::new()).await;
        assert!(matches!(res, Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn nickname_validation() {
        assert_eq!(validate_nickname("  bob \n").unwrap(), "bob");
        assert!(validate_nickname("   ").is_err());
        assert!(validate_nickname(&"x".repeat(MAX_NICKNAME_LEN + 1)).is_err());
    }
}
