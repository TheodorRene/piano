//! The mix bus: a gentle limiter with a soft-clip safety stage, and a small reverb. Both are
//! allocated up front, so the audio thread never allocates.

/// A stereo-linked peak limiter: no look-ahead (no added latency), a fast attack and a slow
/// release, then a soft knee above `KNEE` that only the sharpest peaks reach, so the output
/// never clips hard.
pub struct Limiter {
    threshold: f32,
    gain: f32,
    attack: f32,
    release: f32,
    /// The lowest gain since the last `take_reduction`, for the report and the meter.
    min_gain: f32,
    /// Samples the soft clipper bent (over the knee) so far.
    pub clipped: u64,
}

/// Above this the soft clipper starts bending the wave towards `CEILING`, which it never
/// reaches, so the output stays under full scale.
const KNEE: f32 = 0.85;
const CEILING: f32 = 0.98;

impl Limiter {
    pub fn new(rate: f32) -> Self {
        Self {
            // −2 dBFS.
            threshold: 0.794,
            gain: 1.0,
            attack: 1.0 - (-1.0 / (0.0003 * rate)).exp(),
            release: 1.0 - (-1.0 / (0.25 * rate)).exp(),
            min_gain: 1.0,
            clipped: 0,
        }
    }

    #[inline]
    pub fn run(&mut self, l: f32, r: f32) -> (f32, f32) {
        let peak = l.abs().max(r.abs());
        let target = if peak > self.threshold { self.threshold / peak } else { 1.0 };
        let k = if target < self.gain { self.attack } else { self.release };
        self.gain += (target - self.gain) * k;
        self.min_gain = self.min_gain.min(self.gain);
        let (l, r) = (l * self.gain, r * self.gain);
        if l.abs().max(r.abs()) > KNEE {
            self.clipped += 1;
        }
        (soft_clip(l), soft_clip(r))
    }

    /// The deepest gain reduction (a gain ≤ 1) since the last call.
    pub fn take_reduction(&mut self) -> f32 {
        std::mem::replace(&mut self.min_gain, 1.0)
    }
}

/// Linear up to `KNEE`, then a tanh curve that approaches ±`CEILING`, with a matched slope
/// at the knee so there's no kink.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let room = CEILING - KNEE;
        (KNEE + room * ((a - KNEE) / room).tanh()).copysign(x)
    }
}

/// A feedback delay network reverb: eight delay lines mixed by a Householder matrix, each
/// with a one-pole low-pass in its loop so highs die sooner, as in a real room. A short
/// pre-delay keeps the attack dry. Meant to be subtle: a small warm room.
pub struct Reverb {
    lines: [Vec<f32>; 8],
    pos: [usize; 8],
    damp: [f32; 8],
    feedback: [f32; 8],
    predelay: Vec<f32>,
    pre_pos: usize,
    /// A gentle high-pass on the input, so the bass doesn't boom.
    hp_state: f32,
    hp_k: f32,
    lp_k: f32,
}

/// Delay lengths at 48 kHz (prime, between 23 and 67 ms).
const LENGTHS: [usize; 8] = [1109, 1361, 1597, 1847, 2129, 2447, 2797, 3203];
/// Decay time (to −60 dB), s.
const T60: f32 = 1.5;

impl Reverb {
    pub fn new(rate: f32) -> Self {
        let scale = rate / 48000.0;
        let lines: [Vec<f32>; 8] = std::array::from_fn(|i| vec![0.0; ((LENGTHS[i] as f32 * scale) as usize).max(1)]);
        let feedback = std::array::from_fn(|i| {
            let secs = lines[i].len() as f32 / rate;
            10f32.powf(-3.0 * secs / T60)
        });
        Self {
            lines,
            pos: [0; 8],
            damp: [0.0; 8],
            feedback,
            predelay: vec![0.0; (0.012 * rate) as usize],
            pre_pos: 0,
            hp_state: 0.0,
            hp_k: 1.0 - (-std::f32::consts::TAU * 180.0 / rate).exp(),
            lp_k: 1.0 - (-std::f32::consts::TAU * 5000.0 / rate).exp(),
        }
    }

    /// One frame in (mono send), one stereo frame of reverb out.
    #[inline]
    pub fn run(&mut self, input: f32) -> (f32, f32) {
        self.hp_state += (input - self.hp_state) * self.hp_k;
        let x = input - self.hp_state;
        let delayed = std::mem::replace(&mut self.predelay[self.pre_pos], x);
        self.pre_pos = (self.pre_pos + 1) % self.predelay.len();

        let mut outs = [0.0f32; 8];
        for (i, o) in outs.iter_mut().enumerate() {
            let line = &self.lines[i];
            let y = line[self.pos[i]];
            self.damp[i] += (y - self.damp[i]) * self.lp_k;
            *o = self.damp[i];
        }
        // Householder: y − (2/N)·Σy keeps the network lossless before the feedback gains.
        let sum: f32 = outs.iter().sum::<f32>() * (2.0 / 8.0);
        for (i, &o) in outs.iter().enumerate() {
            let back = (o - sum) * self.feedback[i] + delayed * 0.35;
            let len = self.lines[i].len();
            self.lines[i][self.pos[i]] = back;
            self.pos[i] = (self.pos[i] + 1) % len;
        }
        let l = outs[0] - outs[2] + outs[4] - outs[6] + 0.5 * (outs[1] + outs[5]);
        let r = outs[1] - outs[3] + outs[5] - outs[7] + 0.5 * (outs[2] + outs[6]);
        (l * 0.5, r * 0.5)
    }

    pub fn clear(&mut self) {
        for l in &mut self.lines {
            l.fill(0.0);
        }
        self.predelay.fill(0.0);
        self.damp = [0.0; 8];
        self.hp_state = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_smooth_and_bounded() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(10.0) < 1.0 && soft_clip(-10.0) > -1.0);
        // Continuous with a matched slope at the knee.
        let e = 1e-4;
        let slope = (soft_clip(KNEE + e) - soft_clip(KNEE)) / e;
        assert!((slope - 1.0).abs() < 0.01, "{slope}");
    }

    #[test]
    fn limiter_holds_loud_input_under_full_scale() {
        let mut lim = Limiter::new(48000.0);
        let mut peak = 0.0f32;
        for n in 0..48000 {
            let x = 3.0 * (n as f32 * 0.05).sin();
            let (l, r) = lim.run(x, x);
            peak = peak.max(l.abs()).max(r.abs());
        }
        assert!(peak < 1.0, "{peak}");
        assert!(lim.take_reduction() < 0.4);
    }

    #[test]
    fn reverb_decays_and_stays_finite() {
        let mut rev = Reverb::new(48000.0);
        let energy = |from: usize, to: usize, rev: &mut Reverb| {
            (from..to).map(|n| {
                let (l, r) = rev.run(if n == 0 { 1.0 } else { 0.0 });
                assert!(l.is_finite() && r.is_finite());
                l * l + r * r
            }).sum::<f32>()
        };
        let early = energy(0, 24000, &mut rev);
        let _ = energy(24000, 96000, &mut rev);
        let late = energy(96000, 120000, &mut rev);
        // 2 s past the impulse (more than T60) it's at least 60 dB down on the first half second.
        assert!(late < early * 1e-6, "{early} {late}");
    }
}
