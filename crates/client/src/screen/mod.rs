//! Screen sharing: capture and encode on the sharer, decode and show on viewers.
//! Frames travel as H.264 access units over the voice link (`t7_proto::voice`).

/// One encoded H.264 frame in Annex B form, the unit the encoder emits and
/// the decoder takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// Counts up by one per frame on each share.
    pub frame: u32,
    /// Decodable on its own (IDR).
    pub key: bool,
    pub data: Vec<u8>,
}
