//! Voice datagrams (UDP, same port as the TCP control connection).
//!
//! Every datagram is sealed with ChaCha20-Poly1305 under the client's voice
//! key from `Welcome`. The cleartext header is the associated data, so it is
//! authenticated too. Fixed little-endian layout, opened in place:
//!
//! ```text
//! client -> server   [kind u8][client_id u32][counter u64][sealed body][tag 16]
//!   PING   body = nonce u64
//!   AUDIO  body = seq u32, opus payload
//! server -> client   [kind u8][counter u64][sealed body][tag 16]
//!   PONG   body = nonce u64
//!   AUDIO  body = from u32, seq u32, opus payload
//! ```
//!
//! The 12-byte nonce is the direction (0 to the server, 1 to the client),
//! three zero bytes and the sender's counter. Each sender counts up from 0;
//! each receiver drops counters it has already seen and counters more than 63
//! behind the newest one. Audio is 48 kHz mono Opus in 20 ms frames.

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};

use crate::ClientId;

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 960;
/// Keeps datagrams under typical path MTUs.
pub const MAX_DATAGRAM: usize = 1200;
/// Both directions spend 33 bytes around the payload: a 13- or 9-byte header,
/// 4 or 8 sealed bytes before the payload, and the 16-byte tag.
pub const MAX_OPUS_PAYLOAD: usize = MAX_DATAGRAM - CLIENT_HEADER - 4 - TAG_LEN;
const _: () = assert!(SERVER_HEADER + 8 + MAX_OPUS_PAYLOAD + TAG_LEN <= MAX_DATAGRAM);

/// The secret for one client's voice datagrams, sent in `Welcome` inside TLS.
pub type VoiceKey = [u8; 32];

const PING: u8 = 0x01;
const AUDIO: u8 = 0x02;
const PONG: u8 = 0x81;
const AUDIO_OUT: u8 = 0x82;
const CLIENT_HEADER: usize = 1 + 4 + 8;
const SERVER_HEADER: usize = 1 + 8;
const TAG_LEN: usize = 16;
const TO_SERVER: u8 = 0;
const TO_CLIENT: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToServer<'a> {
    Ping { nonce: u64 },
    Audio { seq: u32, payload: &'a [u8] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToClient<'a> {
    Pong { nonce: u64 },
    Audio { from: ClientId, seq: u32, payload: &'a [u8] },
}

/// The client's end of its voice link.
pub struct ClientSide {
    id: ClientId,
    link: Link,
}

impl ClientSide {
    pub fn new(id: ClientId, key: &VoiceKey) -> Self {
        Self { id, link: Link::new(key) }
    }

    pub fn seal(&mut self, msg: ToServer<'_>, out: &mut Vec<u8>) {
        let counter = self.link.next_counter();
        out.clear();
        out.push(match msg {
            ToServer::Ping { .. } => PING,
            ToServer::Audio { .. } => AUDIO,
        });
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&counter.to_le_bytes());
        match msg {
            ToServer::Ping { nonce } => out.extend_from_slice(&nonce.to_le_bytes()),
            ToServer::Audio { seq, payload } => {
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(payload);
            }
        }
        self.link.seal(TO_SERVER, counter, CLIENT_HEADER, out);
    }

    /// Decrypts in place. `None` for anything forged, replayed or malformed.
    pub fn open<'a>(&mut self, datagram: &'a mut [u8]) -> Option<ToClient<'a>> {
        let kind = *datagram.first()?;
        if !matches!(kind, PONG | AUDIO_OUT) || datagram.len() > MAX_DATAGRAM {
            return None;
        }
        let counter = u64::from_le_bytes(datagram.get(1..9)?.try_into().ok()?);
        let body = self.link.open(TO_CLIENT, counter, SERVER_HEADER, datagram)?;
        if kind == PONG {
            return Some(ToClient::Pong { nonce: u64::from_le_bytes(body.try_into().ok()?) });
        }
        let (head, payload) = body.split_at_checked(8)?;
        let from = u32::from_le_bytes(head[..4].try_into().ok()?);
        let seq = u32::from_le_bytes(head[4..].try_into().ok()?);
        valid_payload(payload).then_some(ToClient::Audio { from, seq, payload })
    }
}

/// The server's end of one client's voice link.
pub struct ServerSide {
    link: Link,
}

impl ServerSide {
    pub fn new(key: &VoiceKey) -> Self {
        Self { link: Link::new(key) }
    }

    pub fn seal(&mut self, msg: ToClient<'_>, out: &mut Vec<u8>) {
        let counter = self.link.next_counter();
        out.clear();
        match msg {
            ToClient::Pong { nonce } => {
                out.push(PONG);
                out.extend_from_slice(&counter.to_le_bytes());
                out.extend_from_slice(&nonce.to_le_bytes());
            }
            ToClient::Audio { from, seq, payload } => {
                out.push(AUDIO_OUT);
                out.extend_from_slice(&counter.to_le_bytes());
                out.extend_from_slice(&from.to_le_bytes());
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(payload);
            }
        }
        self.link.seal(TO_CLIENT, counter, SERVER_HEADER, out);
    }

    /// Decrypts in place. `None` for anything forged, replayed or malformed.
    pub fn open<'a>(&mut self, datagram: &'a mut [u8]) -> Option<ToServer<'a>> {
        let kind = *datagram.first()?;
        if !matches!(kind, PING | AUDIO) || datagram.len() > MAX_DATAGRAM {
            return None;
        }
        let counter = u64::from_le_bytes(datagram.get(5..13)?.try_into().ok()?);
        let body = self.link.open(TO_SERVER, counter, CLIENT_HEADER, datagram)?;
        if kind == PING {
            return Some(ToServer::Ping { nonce: u64::from_le_bytes(body.try_into().ok()?) });
        }
        let (seq, payload) = body.split_at_checked(4)?;
        let seq = u32::from_le_bytes(seq.try_into().ok()?);
        valid_payload(payload).then_some(ToServer::Audio { seq, payload })
    }
}

/// The client id in a datagram's cleartext header: whose key should open it.
pub fn sender(datagram: &[u8]) -> Option<ClientId> {
    Some(u32::from_le_bytes(datagram.get(1..5)?.try_into().ok()?))
}

fn valid_payload(payload: &[u8]) -> bool {
    !payload.is_empty() && payload.len() <= MAX_OPUS_PAYLOAD
}

/// Key, send counter and replay window of one end of a voice link.
struct Link {
    cipher: ChaCha20Poly1305,
    sent: u64,
    received: ReplayWindow,
}

impl Link {
    fn new(key: &VoiceKey) -> Self {
        Self { cipher: ChaCha20Poly1305::new(&(*key).into()), sent: 0, received: ReplayWindow::default() }
    }

    fn next_counter(&mut self) -> u64 {
        self.sent += 1;
        self.sent - 1
    }

    /// Encrypts `out[header..]` in place and appends the tag; `out[..header]` is the associated data.
    fn seal(&self, direction: u8, counter: u64, header: usize, out: &mut Vec<u8>) {
        let (aad, body) = out.split_at_mut(header);
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce(direction, counter), aad, body.into())
            .expect("a datagram is far below the AEAD length limit");
        out.extend_from_slice(&tag);
    }

    /// Decrypts the body in place if it authenticates and its counter is new.
    fn open<'a>(&mut self, direction: u8, counter: u64, header: usize, datagram: &'a mut [u8]) -> Option<&'a [u8]> {
        if !self.received.is_new(counter) {
            return None;
        }
        let body_len = datagram.len().checked_sub(header + TAG_LEN)?;
        let (aad, rest) = datagram.split_at_mut(header);
        let (body, tag) = rest.split_at_mut(body_len);
        let tag = Tag::try_from(&*tag).ok()?;
        self.cipher.decrypt_inout_detached(&nonce(direction, counter), aad, (&mut *body).into(), &tag).ok()?;
        self.received.insert(counter);
        Some(body)
    }
}

fn nonce(direction: u8, counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[0] = direction;
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce.into()
}

/// Which of the 64 newest counters have arrived. Older ones count as replays.
#[derive(Default)]
struct ReplayWindow {
    newest: u64,
    /// Bit `i` is set once counter `newest - i` has arrived.
    seen: u64,
}

impl ReplayWindow {
    fn is_new(&self, counter: u64) -> bool {
        match self.newest.checked_sub(counter) {
            None => true,
            Some(age) => age < 64 && self.seen & (1 << age) == 0,
        }
    }

    /// Records a counter that [`Self::is_new`] accepted.
    fn insert(&mut self, counter: u64) {
        if counter > self.newest {
            let ahead = counter - self.newest;
            self.seen = (if ahead < 64 { self.seen << ahead } else { 0 }) | 1;
            self.newest = counter;
        } else {
            self.seen |= 1 << (self.newest - counter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: VoiceKey = [7; 32];

    /// `n` pings sealed by a client, in sending order (counters 0, 1, ...).
    fn pings(n: u64) -> Vec<Vec<u8>> {
        let mut client = ClientSide::new(3, &KEY);
        (0..n)
            .map(|nonce| {
                let mut out = Vec::new();
                client.seal(ToServer::Ping { nonce }, &mut out);
                out
            })
            .collect()
    }

    /// Delivers `datagrams[i]` for each `i` in order; returns which ones opened.
    fn deliver(server: &mut ServerSide, datagrams: &[Vec<u8>], order: &[usize]) -> Vec<bool> {
        order.iter().map(|&i| server.open(&mut datagrams[i].clone()).is_some()).collect()
    }

    #[test]
    fn roundtrips_both_ways() {
        let mut client = ClientSide::new(42, &KEY);
        let mut server = ServerSide::new(&KEY);
        let mut buf = Vec::new();
        for msg in [ToServer::Ping { nonce: 7 }, ToServer::Audio { seq: 9, payload: &[1, 2, 3] }] {
            client.seal(msg, &mut buf);
            assert_eq!(sender(&buf), Some(42));
            assert_eq!(server.open(&mut buf.clone()), Some(msg));
        }
        for msg in [ToClient::Pong { nonce: 7 }, ToClient::Audio { from: 3, seq: 9, payload: &[4, 5] }] {
            server.seal(msg, &mut buf);
            assert_eq!(client.open(&mut buf.clone()), Some(msg));
        }
    }

    #[test]
    fn payload_is_not_in_the_clear() {
        let mut buf = Vec::new();
        ClientSide::new(1, &KEY).seal(ToServer::Audio { seq: 0, payload: b"secret words" }, &mut buf);
        assert!(!buf.windows(12).any(|w| w == b"secret words"));
    }

    #[test]
    fn rejects_forgeries() {
        let mut buf = Vec::new();
        ClientSide::new(1, &KEY).seal(ToServer::Audio { seq: 0, payload: &[1] }, &mut buf);
        assert_eq!(ServerSide::new(&[8; 32]).open(&mut buf.clone()), None, "wrong key");
        for byte in [1, 5, CLIENT_HEADER, buf.len() - 1] {
            let mut tampered = buf.clone();
            tampered[byte] ^= 1;
            assert_eq!(ServerSide::new(&KEY).open(&mut tampered), None, "flipped byte {byte}");
        }
        // A server-to-client datagram is not accepted by the server, nor the reverse.
        let mut pong = Vec::new();
        ServerSide::new(&KEY).seal(ToClient::Pong { nonce: 1 }, &mut pong);
        assert_eq!(ServerSide::new(&KEY).open(&mut pong.clone()), None);
        assert_eq!(ClientSide::new(1, &KEY).open(&mut buf.clone()), None);
    }

    #[test]
    fn rejects_malformed() {
        let mut server = ServerSide::new(&KEY);
        assert_eq!(server.open(&mut []), None);
        assert_eq!(server.open(&mut [AUDIO, 1, 0, 0, 0]), None);
        assert_eq!(sender(&[AUDIO, 1, 0]), None);
        let mut client = ClientSide::new(1, &KEY);
        let mut buf = Vec::new();
        client.seal(ToServer::Audio { seq: 0, payload: &[] }, &mut buf);
        assert_eq!(server.open(&mut buf), None, "empty payload");
        client.seal(ToServer::Audio { seq: 0, payload: &[0; MAX_OPUS_PAYLOAD + 1] }, &mut buf);
        assert_eq!(server.open(&mut buf), None, "oversized payload");
        client.seal(ToServer::Audio { seq: 0, payload: &[0; MAX_OPUS_PAYLOAD] }, &mut buf);
        assert_eq!(buf.len(), MAX_DATAGRAM);
        assert!(server.open(&mut buf).is_some(), "largest payload fits");
        let mut from_server = ServerSide::new(&KEY);
        from_server.seal(ToClient::Audio { from: 2, seq: 0, payload: &[0; MAX_OPUS_PAYLOAD] }, &mut buf);
        assert!(buf.len() <= MAX_DATAGRAM);
        assert!(client.open(&mut buf).is_some());
    }

    #[test]
    fn replays_are_dropped() {
        let d = pings(3);
        let mut server = ServerSide::new(&KEY);
        assert_eq!(deliver(&mut server, &d, &[0, 0, 2, 1, 1, 2]), [true, false, true, true, false, false]);
    }

    #[test]
    fn window_covers_the_64_newest_counters() {
        let d = pings(70);
        let mut server = ServerSide::new(&KEY);
        // 69 is the newest: 6 is 63 behind and still accepted, 5 is 64 behind and dropped.
        assert_eq!(deliver(&mut server, &d, &[69, 6, 5, 6, 68]), [true, true, false, false, true]);
    }

    #[test]
    fn a_jump_forward_forgets_the_old_window() {
        let d = pings(200);
        let mut server = ServerSide::new(&KEY);
        assert_eq!(
            deliver(&mut server, &d, &[0, 1, 199, 1, 135, 136, 198]),
            [true, true, true, false, false, true, true]
        );
    }

    #[test]
    fn first_counter_may_be_anything() {
        let d = pings(100);
        let mut server = ServerSide::new(&KEY);
        assert_eq!(deliver(&mut server, &d, &[99, 0, 50, 99]), [true, false, true, false]);
    }

    #[test]
    fn replay_window_edges() {
        let mut w = ReplayWindow::default();
        w.insert(u64::MAX);
        assert!(!w.is_new(u64::MAX));
        assert!(w.is_new(u64::MAX - 63));
        assert!(!w.is_new(u64::MAX - 64));
        assert!(!w.is_new(0));
    }
}
