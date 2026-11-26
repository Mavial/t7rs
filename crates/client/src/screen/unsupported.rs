use super::{AccessUnit, Params, Picture, ShareStats, SourceInfo};

const UNSUPPORTED: &str = "screen sharing is not supported on this platform";

pub(crate) fn picker_sources() -> super::PickerSources {
    super::PickerSources::default()
}

pub fn sources() -> Vec<SourceInfo> {
    Vec::new()
}

pub enum Share {}

impl Share {
    pub fn start(
        _: super::Source,
        _: Params,
        _: Box<dyn FnMut(AccessUnit) + Send>,
        _: Box<dyn FnOnce() + Send>,
    ) -> Result<Share, String> {
        Err(UNSUPPORTED.into())
    }

    pub fn request_keyframe(&self) {
        match *self {}
    }

    pub fn ended(&self) -> bool {
        match *self {}
    }

    pub fn encoder(&self) -> &str {
        match *self {}
    }

    pub fn take_stats(&self) -> ShareStats {
        match *self {}
    }
}

pub enum Decoder {}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        Err(UNSUPPORTED.into())
    }

    pub fn decode(&mut self, _: &[u8], _: &mut Picture) -> Result<bool, String> {
        match *self {}
    }
}
