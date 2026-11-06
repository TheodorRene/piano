//! Measuring rendered audio, for the offline report and the tests: levels and clipping,
//! clicks, pitch, decay and release times; and writing WAV files.

use std::io::Write;
use std::path::Path;

use realfft::RealFftPlanner;

/// Peak and RMS of interleaved stereo, and how many samples hit full scale.
#[derive(Clone, Copy, Debug, Default)]
pub struct Levels {
    pub peak: f32,
    pub rms: f32,
    /// Samples at or over 0.999 (full scale, after the limiter).
    pub clipped: usize,
}

pub fn levels(stereo: &[f32]) -> Levels {
    let mut l = Levels::default();
    let mut sum = 0.0f64;
    for &x in stereo {
        l.peak = l.peak.max(x.abs());
        sum += (x as f64) * (x as f64);
        if x.abs() >= 0.999 {
            l.clipped += 1;
        }
    }
    l.rms = (sum / stereo.len().max(1) as f64).sqrt() as f32;
    l
}

pub fn db(x: f32) -> f32 {
    20.0 * x.max(1e-12).log10()
}

/// The largest jump between neighbouring samples of either channel.
pub fn max_jump(stereo: &[f32]) -> f32 {
    let mut m = 0.0f32;
    for ch in 0..2 {
        let mut last = 0.0;
        for f in stereo.as_chunks::<2>().0 {
            m = m.max((f[ch] - last).abs());
            last = f[ch];
        }
    }
    m
}

/// "Roughness": the RMS of the second difference (x[n] − 2x[n−1] + x[n−2]) of each channel
/// over windows of `win` frames, the larger channel. A click (a step or a kink in the wave)
/// shows up as a spike in it, while a smooth fade only lowers it.
pub fn roughness(stereo: &[f32], win: usize) -> Vec<f32> {
    let frames = stereo.len() / 2;
    let mut out = Vec::with_capacity(frames / win + 1);
    let mut acc = [0.0f64; 2];
    let mut prev = [[0.0f32; 2]; 2];
    for (n, f) in stereo.as_chunks::<2>().0.iter().enumerate() {
        for ch in 0..2 {
            let d2 = f[ch] - 2.0 * prev[ch][0] + prev[ch][1];
            acc[ch] += (d2 as f64) * (d2 as f64);
            prev[ch] = [f[ch], prev[ch][0]];
        }
        if (n + 1) % win == 0 || n + 1 == frames {
            out.push((acc[0].max(acc[1]) / win as f64).sqrt() as f32);
            acc = [0.0; 2];
        }
    }
    out
}

/// How much rougher (dB) the sound gets just after `at` (a frame) than it was before: the
/// worst 1 ms window in the `after` frames following, against the median 1 ms window in the
/// 40 ms before. A release or a voice steal that fades smoothly scores ≤ 0 dB; a click scores
/// well above (a step of 0.01 in a quiet passage is +30 dB or more).
pub fn click_score(rough: &[f32], win: usize, at: usize, after: usize) -> f32 {
    let i = at / win;
    let before_from = i.saturating_sub(40 * 48 / win);
    if i <= before_from + 2 || i >= rough.len() {
        return 0.0;
    }
    let mut before: Vec<f32> = rough[before_from..i].to_vec();
    before.sort_by(f32::total_cmp);
    let base = before[before.len() / 2].max(1e-7);
    let end = (i + after.div_ceil(win) + 1).min(rough.len());
    let worst = rough[i..end].iter().copied().fold(0.0f32, f32::max);
    db(worst / base)
}

/// The envelope: RMS of the mono mix over windows of `win` frames.
pub fn envelope(stereo: &[f32], win: usize) -> Vec<f32> {
    stereo
        .chunks(2 * win)
        .map(|c| {
            let s: f64 = c.as_chunks::<2>().0.iter().map(|f| (0.5 * (f[0] + f[1])) as f64).map(|m| m * m).sum();
            (s / (c.len() / 2).max(1) as f64).sqrt() as f32
        })
        .collect()
}

/// Seconds from `from` (a frame) until the envelope has fallen `drop_db` below its level
/// there (or its peak after it, if that's higher); None if it never does.
pub fn time_to_fall(env: &[f32], win: usize, rate: u32, from: usize, drop_db: f32) -> Option<f64> {
    let i = from / win;
    let start = *env.get(i)?;
    let reference = env[i..env.len().min(i + 10)].iter().copied().fold(start, f32::max);
    let target = reference * 10f32.powf(-drop_db / 20.0);
    env[i..].iter().position(|&e| e <= target).map(|k| (k * win) as f64 / rate as f64)
}

/// The pitch of a single note (mono), Hz, measured near the `expected` fundamental: a long
/// Hann-windowed FFT, zero-padded, with a parabola through the peak. Piano bass notes have a
/// weak fundamental, so below 130 Hz it takes the strongest of the first four partials and
/// divides (the strings' stiffness makes upper partials a little sharp: under 1¢ for these).
pub fn pitch(mono: &[f32], rate: u32, expected: f64) -> Option<(f64, usize)> {
    let n = mono.len().min(1 << 16);
    if n < 4096 {
        return None;
    }
    let size = (n * 4).next_power_of_two();
    let mut buf = vec![0.0f32; size];
    for (i, (b, &x)) in buf.iter_mut().zip(mono).enumerate() {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
        *b = x * w as f32;
    }
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(size);
    let mut spec = fft.make_output_vec();
    fft.process(&mut buf, &mut spec).ok()?;
    let mag: Vec<f64> = spec.iter().map(|c| (c.norm() as f64).max(1e-20)).collect();
    let bin_hz = rate as f64 / size as f64;
    let partials = if expected < 130.0 { 4 } else { 1 };
    let mut best: Option<(f64, f64, usize)> = None;
    for k in 1..=partials {
        let centre = expected * k as f64;
        // ±60 cents around the partial.
        let lo = ((centre * 2f64.powf(-0.05) / bin_hz) as usize).max(1);
        let hi = ((centre * 2f64.powf(0.05) / bin_hz) as usize).min(mag.len() - 2);
        if lo >= hi {
            continue;
        }
        let i = (lo..=hi).max_by(|&a, &b| mag[a].total_cmp(&mag[b]))?;
        let (a, b, c) = (mag[i - 1].ln(), mag[i].ln(), mag[i + 1].ln());
        let shift = 0.5 * (a - c) / (a - 2.0 * b + c);
        let hz = (i as f64 + shift) * bin_hz / k as f64;
        if best.is_none_or(|(_, m, _)| mag[i] > m) {
            best = Some((hz, mag[i], k));
        }
    }
    best.map(|(hz, _, k)| (hz, k))
}

pub fn cents(hz: f64, target: f64) -> f64 {
    1200.0 * (hz / target).log2()
}

pub fn midi_hz(midi: i32) -> f64 {
    440.0 * 2f64.powf((midi - 69) as f64 / 12.0)
}

/// Writes interleaved stereo as a 32-bit float WAV.
pub fn write_wav(path: &Path, stereo: &[f32], rate: u32) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data = (stereo.len() * 4) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?; // IEEE float
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 8).to_le_bytes())?;
    f.write_all(&8u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data.to_le_bytes())?;
    for x in stereo {
        f.write_all(&x.to_le_bytes())?;
    }
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, secs: f64, amp: f32) -> Vec<f32> {
        (0..(secs * 48000.0) as usize)
            .flat_map(|n| {
                let x = amp * (std::f64::consts::TAU * hz * n as f64 / 48000.0).sin() as f32;
                [x, x]
            })
            .collect()
    }

    #[test]
    fn pitch_reads_a_sine_to_a_tenth_of_a_cent() {
        for hz in [27.5, 98.0, 261.63, 1046.5, 4186.0] {
            let mono: Vec<f32> = sine(hz, 1.5, 0.5).chunks(2).map(|f| f[0]).collect();
            let (got, _) = pitch(&mono, 48000, hz * 1.01).unwrap();
            assert!(cents(got, hz).abs() < 0.1, "{hz}: {got}");
        }
    }

    #[test]
    fn clicks_score_high_and_fades_low() {
        let mut x = sine(440.0, 1.0, 0.1);
        // A smooth 50 ms fade from 0.5 s.
        let mut faded = x.clone();
        for (n, f) in faded.as_chunks_mut::<2>().0.iter_mut().enumerate().skip(24000) {
            let g = (1.0 - (n - 24000) as f32 / 2400.0).max(0.0);
            f[0] *= g;
            f[1] *= g;
        }
        // A hard cut just after 0.5 s (off a zero crossing).
        for f in x.as_chunks_mut::<2>().0.iter_mut().skip(24100) {
            f[0] = 0.0;
            f[1] = 0.0;
        }
        let score = |s: &[f32]| click_score(&roughness(s, 48), 48, 24000, 1200);
        assert!(score(&faded) < 1.0, "fade: {}", score(&faded));
        assert!(score(&x) > 12.0, "cut: {}", score(&x));
    }
}
