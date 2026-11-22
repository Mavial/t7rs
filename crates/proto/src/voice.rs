//! Voice datagrams (UDP, same port as the TCP control connection).
//!
//! Fixed little-endian binary layout, parsed without allocation:
//!
//! ```text
//! client -> server   [kind u8][client_id u32][token u64][body]
//!   PING   body = nonce u64
//!   AUDIO  body = seq u32, opus payload
//! server -> client   [kind u8][body]
//!   PONG   body = nonce u64
//!   AUDIO  body = from u32, seq u32, opus payload
//! ```
//!
//! The token comes from the `Welcome` message and proves the sender owns the
//! client id. Audio is 48 kHz mono Opus in 20 ms frames.

use crate::ClientId;

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 960;
/// Keeps datagrams under typical path MTUs.
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_OPUS_PAYLOAD: usize = MAX_DATAGRAM - 13;

const PING: u8 = 0x01;
const AUDIO: u8 = 0x02;
const PONG: u8 = 0x81;
const AUDIO_OUT: u8 = 0x82;
const CLIENT_HEADER: usize = 1 + 4 + 8;

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

pub fn encode_to_server(id: ClientId, token: u64, msg: ToServer<'_>, out: &mut Vec<u8>) {
    out.clear();
    let kind = match msg {
        ToServer::Ping { .. } => PING,
        ToServer::Audio { .. } => AUDIO,
    };
    out.push(kind);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&token.to_le_bytes());
    match msg {
        ToServer::Ping { nonce } => out.extend_from_slice(&nonce.to_le_bytes()),
        ToServer::Audio { seq, payload } => {
            out.extend_from_slice(&seq.to_le_bytes());
            out.extend_from_slice(payload);
        }
    }
}

/// Returns `(client_id, token, message)`, or `None` for anything malformed.
pub fn parse_to_server(buf: &[u8]) -> Option<(ClientId, u64, ToServer<'_>)> {
    let (&kind, rest) = buf.split_first()?;
    let id = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    let token = u64::from_le_bytes(rest.get(4..12)?.try_into().ok()?);
    let body = &buf[CLIENT_HEADER..];
    let msg = match kind {
        PING => ToServer::Ping { nonce: u64::from_le_bytes(body.try_into().ok()?) },
        AUDIO => {
            let seq = u32::from_le_bytes(body.get(..4)?.try_into().ok()?);
            let payload = &body[4..];
            if payload.is_empty() || payload.len() > MAX_OPUS_PAYLOAD {
                return None;
            }
            ToServer::Audio { seq, payload }
        }
        _ => return None,
    };
    Some((id, token, msg))
}

pub fn encode_to_client(msg: ToClient<'_>, out: &mut Vec<u8>) {
    out.clear();
    match msg {
        ToClient::Pong { nonce } => {
            out.push(PONG);
            out.extend_from_slice(&nonce.to_le_bytes());
        }
        ToClient::Audio { from, seq, payload } => {
            out.push(AUDIO_OUT);
            out.extend_from_slice(&from.to_le_bytes());
            out.extend_from_slice(&seq.to_le_bytes());
            out.extend_from_slice(payload);
        }
    }
}

pub fn parse_to_client(buf: &[u8]) -> Option<ToClient<'_>> {
    let (&kind, body) = buf.split_first()?;
    match kind {
        PONG => Some(ToClient::Pong { nonce: u64::from_le_bytes(body.try_into().ok()?) }),
        AUDIO_OUT => {
            let from = u32::from_le_bytes(body.get(..4)?.try_into().ok()?);
            let seq = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?);
            let payload = &body[8..];
            (!payload.is_empty() && payload.len() <= MAX_OPUS_PAYLOAD)
                .then_some(ToClient::Audio { from, seq, payload })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let mut buf = Vec::new();
        for msg in [ToServer::Ping { nonce: 7 }, ToServer::Audio { seq: 9, payload: &[1, 2, 3] }] {
            encode_to_server(42, 0xdead_beef, msg, &mut buf);
            assert_eq!(parse_to_server(&buf), Some((42, 0xdead_beef, msg)));
        }
        for msg in [ToClient::Pong { nonce: 7 }, ToClient::Audio { from: 3, seq: 9, payload: &[4, 5] }] {
            encode_to_client(msg, &mut buf);
            assert_eq!(parse_to_client(&buf), Some(msg));
        }
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_to_server(&[]), None);
        assert_eq!(parse_to_server(&[AUDIO, 1, 0, 0, 0]), None);
        let mut buf = Vec::new();
        encode_to_server(1, 2, ToServer::Audio { seq: 0, payload: &[1] }, &mut buf);
        buf.truncate(CLIENT_HEADER + 4); // empty payload
        assert_eq!(parse_to_server(&buf), None);
        assert_eq!(parse_to_client(&[0x99, 0, 0]), None);
        encode_to_server(1, 2, ToServer::Audio { seq: 0, payload: &[0; MAX_OPUS_PAYLOAD + 1] }, &mut buf);
        assert_eq!(parse_to_server(&buf), None);
    }
}
