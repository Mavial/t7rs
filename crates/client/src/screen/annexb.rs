//! Annex B H.264: NAL units behind `00 00 01` start codes.

const IDR: u8 = 5;
const SPS: u8 = 7;
const PPS: u8 = 8;

/// The NAL units in `data`, each without its start code.
pub fn units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = start_code(data).map_or(&[][..], |at| &data[at + 3..]);
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (unit, next) = match start_code(rest) {
            Some(at) => (&rest[..at], &rest[at + 3..]),
            None => (rest, &[][..]),
        };
        rest = next;
        // A four-byte start code's leading zero trails the unit before it.
        let end = unit.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        Some(&unit[..end])
    })
    .filter(|unit| !unit.is_empty())
}

fn start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|w| w == [0, 0, 1])
}

fn kind(unit: &[u8]) -> u8 {
    unit[0] & 0x1f
}

/// Decodable on its own: holds an IDR slice.
pub fn is_key(data: &[u8]) -> bool {
    units(data).any(|u| kind(u) == IDR)
}

/// The newest SPS and PPS, so every keyframe can carry them and a viewer that
/// joins mid-share can start at any keyframe.
#[derive(Default)]
pub struct ParamSets(Vec<u8>);

impl ParamSets {
    /// Remembers the parameter sets in `au`, or puts the remembered ones in
    /// front of a keyframe that has none.
    pub fn complete(&mut self, au: &mut Vec<u8>) {
        let mut sets = Vec::new();
        for unit in units(au).filter(|u| matches!(kind(u), SPS | PPS)) {
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(unit);
        }
        if !sets.is_empty() {
            self.0 = sets;
        } else if is_key(au) {
            au.splice(0..0, self.0.iter().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_units_on_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let units: Vec<&[u8]> = units(&data).collect();
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
        assert!(is_key(&data));
        assert!(!is_key(&[0, 0, 1, 0x41, 9]));
    }

    #[test]
    fn keyframes_without_parameter_sets_get_the_last_ones() {
        let mut sets = ParamSets::default();
        let mut first = vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
        let original = first.clone();
        sets.complete(&mut first);
        assert_eq!(first, original);

        let mut delta = vec![0, 0, 0, 1, 0x41, 4];
        sets.complete(&mut delta);
        assert_eq!(delta, [0, 0, 0, 1, 0x41, 4]);

        let mut bare_key = vec![0, 0, 0, 1, 0x65, 5];
        sets.complete(&mut bare_key);
        assert_eq!(bare_key, [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 5]);
    }
}
