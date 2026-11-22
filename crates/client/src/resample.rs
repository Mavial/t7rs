pub struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        Self { step: f64::from(from_hz) / f64::from(to_hz), pos: 0.0, prev: 0.0 }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.run(input, usize::MAX, out);
    }

    pub fn input_needed(&self, frames: usize) -> usize {
        if frames == 0 {
            return 0;
        }
        (self.pos + (frames - 1) as f64 * self.step).floor() as usize + 1
    }

    pub fn pull(&mut self, input: &[f32], frames: usize, out: &mut Vec<f32>) {
        debug_assert_eq!(input.len(), self.input_needed(frames));
        self.run(input, frames, out);
    }

    fn run(&mut self, input: &[f32], max_out: usize, out: &mut Vec<f32>) {
        let at = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        let mut produced = 0;
        while produced < max_out {
            let i = self.pos.floor() as usize;
            if i + 1 > input.len() {
                break;
            }
            let frac = (self.pos - i as f64) as f32;
            out.push(at(i) + (at(i + 1) - at(i)) * frac);
            self.pos += self.step;
            produced += 1;
        }
        let consumed = (self.pos.floor() as usize).min(input.len());
        self.prev = at(consumed);
        self.pos -= consumed as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin()).collect()
    }

    fn crossings(x: &[f32], rate: u32) -> f32 {
        let c = x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        c as f32 / (x.len() as f32 / rate as f32) / 2.0
    }

    #[test]
    fn push_converts_length_and_keeps_pitch() {
        for (from, to) in [(44_100, 48_000), (48_000, 44_100), (48_000, 48_000), (16_000, 48_000)] {
            let input = tone(440.0, from, from as usize);
            let mut rs = Resampler::new(from, to);
            let mut out = Vec::new();
            for chunk in input.chunks(441) {
                rs.push(chunk, &mut out);
            }
            let expected = to as f32;
            assert!((out.len() as f32 - expected).abs() <= 2.0, "{from}->{to}: {} samples", out.len());
            let hz = crossings(&out, to);
            assert!((hz - 440.0).abs() < 2.0, "{from}->{to}: {hz} Hz");
        }
    }

    #[test]
    fn pull_produces_exact_counts_and_constant_stays_constant() {
        for (from, to) in [(48_000, 44_100), (48_000, 96_000), (48_000, 48_000)] {
            let mut rs = Resampler::new(from, to);
            let mut out = Vec::new();
            let mut total_in = 0;
            for frames in [1, 7, 441, 480, 512, 1024] {
                let need = rs.input_needed(frames);
                total_in += need;
                let before = out.len();
                rs.pull(&vec![0.5; need], frames, &mut out);
                assert_eq!(out.len() - before, frames);
            }
            assert!(out[2..].iter().all(|&v| (v - 0.5).abs() < 1e-6));
            let ratio = total_in as f64 / out.len() as f64;
            assert!((ratio - f64::from(from) / f64::from(to)).abs() < 0.01, "{from}->{to}: {ratio}");
        }
    }
}
