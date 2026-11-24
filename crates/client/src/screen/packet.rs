use std::collections::VecDeque;
use std::time::{Duration, Instant};

use t7_proto::voice::{MAX_ACCESS_UNIT, MAX_VIDEO_PAYLOAD};

use super::AccessUnit;

pub const FRAME_PACE: Duration = Duration::from_millis(16);
const MAX_BURST: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPart<'a> {
    pub frame: u32,
    pub part: u16,
    pub parts: u16,
    pub key: bool,
    pub payload: &'a [u8],
}

pub fn packetize(au: &AccessUnit) -> impl Iterator<Item = VideoPart<'_>> {
    debug_assert!(au.data.len() <= MAX_ACCESS_UNIT);
    let parts = au.data.len().div_ceil(MAX_VIDEO_PAYLOAD) as u16;
    au.data.chunks(MAX_VIDEO_PAYLOAD).enumerate().map(move |(i, payload)| VideoPart {
        frame: au.frame,
        part: i as u16,
        parts,
        key: au.key,
        payload,
    })
}

/// Spreads each access unit's parts evenly over [`FRAME_PACE`] from when it
/// arrives, so a keyframe doesn't leave in one burst. A newer access unit
/// makes the rest of the older ones due at once.
#[derive(Default)]
pub struct Pacer {
    queue: VecDeque<Sending>,
}

struct Sending {
    au: AccessUnit,
    parts: usize,
    sent: usize,
    since: Instant,
}

impl Pacer {
    /// Queues `au`. `false`, and nothing queued, if it is over [`MAX_ACCESS_UNIT`] bytes.
    pub fn push(&mut self, au: AccessUnit, now: Instant) -> bool {
        if au.data.len() > MAX_ACCESS_UNIT {
            return false;
        }
        let parts = au.data.len().div_ceil(MAX_VIDEO_PAYLOAD);
        if parts > 0 {
            self.queue.push_back(Sending { au, parts, sent: 0, since: now });
        }
        true
    }

    pub fn next_at(&self) -> Option<Instant> {
        let first = self.queue.front()?;
        Some(if self.queue.len() > 1 { first.since } else { first.due_at(first.sent) })
    }

    /// Hands `send` the parts due by `now` in order, at most [`MAX_BURST`] of them.
    pub fn send_due(&mut self, now: Instant, mut send: impl FnMut(VideoPart<'_>)) {
        let mut budget = MAX_BURST;
        while budget > 0 {
            let superseded = self.queue.len() > 1;
            let Some(s) = self.queue.front_mut() else { return };
            let due = if superseded { s.parts } else { s.due_by(now) };
            let n = due.saturating_sub(s.sent).min(budget);
            packetize(&s.au).skip(s.sent).take(n).for_each(&mut send);
            s.sent += n;
            budget -= n;
            if s.sent < s.parts {
                return;
            }
            self.queue.pop_front();
        }
    }
}

impl Sending {
    fn due_at(&self, i: usize) -> Instant {
        let nanos = (FRAME_PACE.as_nanos() * i as u128).div_ceil(self.parts as u128);
        self.since + Duration::from_nanos(nanos as u64)
    }

    fn due_by(&self, now: Instant) -> usize {
        let elapsed = now.saturating_duration_since(self.since).as_nanos();
        ((elapsed * self.parts as u128 / FRAME_PACE.as_nanos()) as usize + 1).min(self.parts)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Assembled {
    /// The next access unit to decode: a keyframe, or the frame right after
    /// the last one handed out.
    Frame(AccessUnit),
    /// A frame was lost, so the ones after it can't be decoded. Nothing comes
    /// out until a keyframe does: ask the sharer for one.
    NeedKeyframe,
}

/// Puts one sharer's access units back together from their parts, which may
/// arrive in any order, twice, or not at all.
#[derive(Default)]
pub struct Assembler {
    pending: Option<Pending>,
    newest: Option<u32>,
    decodable: Option<u32>,
}

struct Pending {
    frame: u32,
    key: bool,
    parts: Vec<Option<Vec<u8>>>,
    missing: usize,
}

impl Assembler {
    pub fn push(&mut self, part: VideoPart<'_>) -> Option<Assembled> {
        if self.newest.is_some_and(|n| part.frame <= n) {
            return None;
        }
        let mut lost = false;
        match &self.pending {
            Some(p) if p.frame > part.frame => return None,
            Some(p) if p.frame < part.frame => {
                self.newest = Some(p.frame);
                self.decodable = None;
                self.pending = None;
                lost = true;
            }
            _ => {}
        }
        let lost = lost.then_some(Assembled::NeedKeyframe);
        let p = self.pending.get_or_insert_with(|| Pending {
            frame: part.frame,
            key: part.key,
            parts: vec![None; usize::from(part.parts)],
            missing: usize::from(part.parts),
        });
        if p.key != part.key || p.parts.len() != usize::from(part.parts) {
            return lost;
        }
        let Some(slot @ None) = p.parts.get_mut(usize::from(part.part)) else { return lost };
        *slot = Some(part.payload.to_vec());
        p.missing -= 1;
        if p.missing > 0 {
            return lost;
        }
        let p = self.pending.take().expect("just completed");
        Some(self.finish(p))
    }

    pub fn wait_for_keyframe(&mut self) {
        self.decodable = None;
    }

    pub fn waiting_for_keyframe(&self) -> bool {
        self.decodable.is_none()
    }

    fn finish(&mut self, p: Pending) -> Assembled {
        self.newest = Some(p.frame);
        let follows = self.decodable.and_then(|d| d.checked_add(1)) == Some(p.frame);
        if p.key || follows {
            self.decodable = Some(p.frame);
            let data = p.parts.into_iter().flatten().collect::<Vec<_>>().concat();
            Assembled::Frame(AccessUnit { frame: p.frame, key: p.key, data })
        } else {
            self.decodable = None;
            Assembled::NeedKeyframe
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(frame: u32, key: bool, len: usize) -> AccessUnit {
        AccessUnit { frame, key, data: (0..len).map(|i| (i * 7 + frame as usize) as u8).collect() }
    }

    fn deliver(asm: &mut Assembler, au: &AccessUnit, parts: &[usize]) -> Vec<Assembled> {
        let all: Vec<VideoPart<'_>> = packetize(au).collect();
        parts.iter().filter_map(|&i| asm.push(all[i])).collect()
    }

    fn whole(asm: &mut Assembler, au: &AccessUnit) -> Vec<Assembled> {
        let n = packetize(au).count();
        deliver(asm, au, &(0..n).collect::<Vec<_>>())
    }

    #[test]
    fn parts_fill_datagrams_and_rejoin() {
        let unit = au(3, true, 2 * MAX_VIDEO_PAYLOAD + 1);
        let parts: Vec<VideoPart<'_>> = packetize(&unit).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().enumerate().all(|(i, p)| p.part as usize == i && p.parts == 3 && p.frame == 3 && p.key));
        assert_eq!(
            parts.iter().map(|p| p.payload.len()).collect::<Vec<_>>(),
            [MAX_VIDEO_PAYLOAD, MAX_VIDEO_PAYLOAD, 1]
        );
        assert_eq!(packetize(&au(0, false, MAX_VIDEO_PAYLOAD)).count(), 1);
        assert_eq!(whole(&mut Assembler::default(), &unit), [Assembled::Frame(unit)]);
    }

    #[test]
    fn reordered_and_duplicated_parts_make_one_frame() {
        let mut asm = Assembler::default();
        let unit = au(0, true, 4 * MAX_VIDEO_PAYLOAD);
        assert_eq!(deliver(&mut asm, &unit, &[2, 0, 2, 3, 0]), []);
        assert_eq!(deliver(&mut asm, &unit, &[1]), [Assembled::Frame(unit.clone())]);
        assert_eq!(deliver(&mut asm, &unit, &[1, 3]), [], "late duplicates");
    }

    #[test]
    fn a_lost_part_holds_frames_until_a_keyframe() {
        let mut asm = Assembler::default();
        let (key, next) = (au(0, true, 3 * MAX_VIDEO_PAYLOAD), au(1, false, 3 * MAX_VIDEO_PAYLOAD));
        assert_eq!(whole(&mut asm, &key), [Assembled::Frame(key)]);
        assert_eq!(deliver(&mut asm, &next, &[0, 2]), []);
        let after = au(2, false, MAX_VIDEO_PAYLOAD + 1);
        let lost_then_undecodable = [Assembled::NeedKeyframe, Assembled::NeedKeyframe];
        assert_eq!(whole(&mut asm, &after), lost_then_undecodable);
        assert_eq!(deliver(&mut asm, &next, &[1]), [], "too late");
        assert_eq!(whole(&mut asm, &au(3, false, 10)), [Assembled::NeedKeyframe]);
        let (key, next) = (au(4, true, 10), au(5, false, 10));
        assert_eq!(whole(&mut asm, &key), [Assembled::Frame(key)]);
        assert_eq!(whole(&mut asm, &next), [Assembled::Frame(next)]);
    }

    #[test]
    fn a_wholly_lost_frame_holds_the_next() {
        let mut asm = Assembler::default();
        let (key, next) = (au(0, true, 10), au(1, false, 10));
        assert_eq!(whole(&mut asm, &key), [Assembled::Frame(key)]);
        assert_eq!(whole(&mut asm, &next), [Assembled::Frame(next)]);
        assert_eq!(whole(&mut asm, &au(3, false, 10)), [Assembled::NeedKeyframe]);
    }

    #[test]
    fn nothing_comes_out_before_the_first_keyframe() {
        let mut asm = Assembler::default();
        assert_eq!(whole(&mut asm, &au(7, false, 10)), [Assembled::NeedKeyframe]);
        let key = au(8, true, MAX_VIDEO_PAYLOAD + 1);
        assert_eq!(whole(&mut asm, &key), [Assembled::Frame(key)]);
    }

    #[test]
    fn parts_that_disagree_with_their_frame_are_dropped() {
        let mut asm = Assembler::default();
        let unit = au(0, true, 2 * MAX_VIDEO_PAYLOAD);
        let first = packetize(&unit).next().unwrap();
        assert_eq!(asm.push(first), None);
        assert_eq!(asm.push(VideoPart { part: 1, parts: 5, ..first }), None, "another part count");
        assert_eq!(asm.push(VideoPart { part: 1, key: false, ..first }), None, "not a keyframe after all");
        assert_eq!(asm.push(VideoPart { part: 9, parts: 2, ..first }), None, "past the last part");
        assert_eq!(deliver(&mut asm, &unit, &[1]), [Assembled::Frame(unit)]);
    }

    fn sends(pacer: &mut Pacer, t0: Instant, at: &[u64]) -> Vec<usize> {
        at.iter()
            .map(|&ms| {
                let mut n = 0;
                pacer.send_due(t0 + Duration::from_millis(ms), |_| n += 1);
                n
            })
            .collect()
    }

    #[test]
    fn a_frame_leaves_spread_over_the_frame_interval() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        assert!(pacer.push(au(0, false, 16 * MAX_VIDEO_PAYLOAD), t0));
        assert_eq!(pacer.next_at(), Some(t0));
        assert_eq!(sends(&mut pacer, t0, &[0, 0, 7, 15]), [1, 0, 7, 8]);
        assert_eq!(pacer.next_at(), None, "all 16 sent by 15 ms");
        assert!(!pacer.push(au(1, true, MAX_ACCESS_UNIT + 1), t0), "too large to number");
        assert_eq!(pacer.next_at(), None);
    }

    #[test]
    fn a_newer_frame_sends_the_rest_of_the_older_at_once() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        let (older, newer) = (au(0, true, 16 * MAX_VIDEO_PAYLOAD), au(1, false, 16 * MAX_VIDEO_PAYLOAD));
        pacer.push(older.clone(), t0);
        assert_eq!(sends(&mut pacer, t0, &[0]), [1]);
        pacer.push(newer.clone(), t0 + Duration::from_millis(1));
        assert_eq!(pacer.next_at(), Some(t0), "the older is due at once");
        let mut out = Vec::new();
        pacer.send_due(t0 + Duration::from_millis(1), |p| out.push((p.frame, p.part)));
        let rest_of_older = (1..16).map(|i| (0, i));
        assert_eq!(out, rest_of_older.chain([(1, 0)]).collect::<Vec<_>>());
    }

    #[test]
    fn a_large_frame_leaves_a_burst_at_a_time_in_order() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        let unit = au(0, true, MAX_ACCESS_UNIT);
        pacer.push(unit.clone(), t0);
        let late = t0 + FRAME_PACE;
        let mut asm = Assembler::default();
        let mut out = Vec::new();
        let mut bursts = 0;
        while pacer.next_at().is_some_and(|at| at <= late) {
            pacer.send_due(late, |p| out.extend(asm.push(p)));
            bursts += 1;
        }
        assert_eq!(bursts, MAX_ACCESS_UNIT.div_ceil(MAX_VIDEO_PAYLOAD).div_ceil(MAX_BURST));
        assert_eq!(out, [Assembled::Frame(unit)]);
    }
}
