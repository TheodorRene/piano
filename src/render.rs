//! The offline renderer: plays scripted performances through the real engine, with no
//! PipeWire, and reports on the sound. `piano render [scripts…] [-o dir]`.
//!
//! Events are timed in seconds and reach the engine the way they do live: at the start of
//! the next block after they're "sent". Each script gives a WAV (with the reverb, as heard)
//! and a section of the report: levels, clipping and the limiter; clicks; CPU time per
//! block. The report ends with every key's pitch against equal temperament, decay and
//! release times, and the key-to-sound latency, measured on single notes without reverb.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::analysis::{self, db};
use crate::bank::Bank;
use crate::engine::{Engine, Msg, BLOCK, RATE};
use crate::keys::note_name;

/// A scripted event: seconds from the start, and what happens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Act {
    On(u8, u8),
    Off(u8),
    Pedal(bool),
    Reverb(bool),
}

pub type Script = Vec<(f64, Act)>;

/// The built-in performances, by name.
pub const SCRIPTS: [&str; 8] = ["scale", "chord", "repeat", "pedal", "polyphony", "dynamics", "loud", "trill"];

const V: u8 = 80;

pub fn builtin(name: &str) -> Option<(Script, f64)> {
    let mut s = Script::new();
    let len = match name {
        // C major up and down from C4, detached quavers at 120 bpm.
        "scale" => {
            let up = [60, 62, 64, 65, 67, 69, 71, 72];
            let notes: Vec<u8> = up.iter().chain(up.iter().rev().skip(1)).copied().collect();
            for (i, &n) in notes.iter().enumerate() {
                let t = 0.2 + i as f64 * 0.25;
                s.push((t, Act::On(n, V)));
                s.push((t + 0.2, Act::Off(n)));
            }
            5.5
        }
        // A wide C major chord held 3 s, let go.
        "chord" => {
            for (i, n) in [36, 48, 55, 60, 64, 67, 72].into_iter().enumerate() {
                s.push((0.2 + i as f64 * 0.004, Act::On(n, V)));
                s.push((3.2, Act::Off(n)));
            }
            6.0
        }
        // Fast repeated notes on A4 (12 a second), then the same with the pedal down.
        "repeat" => {
            for k in 0..16 {
                let t = 0.2 + k as f64 / 12.0;
                s.push((t, Act::On(69, V)));
                s.push((t + 0.04, Act::Off(69)));
            }
            s.push((2.0, Act::Pedal(true)));
            for k in 0..16 {
                let t = 2.2 + k as f64 / 12.0;
                s.push((t, Act::On(69, V)));
                s.push((t + 0.04, Act::Off(69)));
            }
            s.push((4.5, Act::Pedal(false)));
            7.0
        }
        // A broken chord with the pedal down and the keys let go at once, then the pedal up.
        "pedal" => {
            s.push((0.1, Act::Pedal(true)));
            for (i, n) in [36, 43, 48, 52, 55, 60, 64, 67, 72].into_iter().enumerate() {
                let t = 0.2 + i as f64 * 0.15;
                s.push((t, Act::On(n, V)));
                s.push((t + 0.1, Act::Off(n)));
            }
            s.push((4.0, Act::Pedal(false)));
            6.5
        }
        // Pedal down, a glissando up all 88 keys and back down: more strikes than voices.
        "polyphony" => {
            s.push((0.05, Act::Pedal(true)));
            let notes: Vec<u8> = (21..=108).chain((21..108).rev()).collect();
            for (i, &n) in notes.iter().enumerate() {
                let t = 0.1 + i as f64 * 0.025;
                s.push((t, Act::On(n, V)));
                s.push((t + 0.02, Act::Off(n)));
            }
            s.push((5.0, Act::Pedal(false)));
            7.5
        }
        // C4 at rising velocities.
        "dynamics" => {
            for (i, v) in [8u8, 24, 40, 56, 72, 88, 104, 120, 127].into_iter().enumerate() {
                let t = 0.2 + i as f64 * 0.7;
                s.push((t, Act::On(60, v)));
                s.push((t + 0.6, Act::Off(60)));
            }
            7.0
        }
        // Both hands full, fff, repeated: what the limiter is for.
        "loud" => {
            let chord = [24, 31, 36, 43, 48, 55, 60, 64, 67, 72, 76, 79];
            for k in 0..4 {
                let t = 0.2 + k as f64 * 0.6;
                for &n in &chord {
                    s.push((t, Act::On(n, 127)));
                    s.push((t + 0.5, Act::Off(n)));
                }
            }
            4.0
        }
        // A fast trill, overlapping key presses (legato fingers).
        "trill" => {
            for k in 0..24 {
                let t = 0.2 + k as f64 * 0.07;
                let n = if k % 2 == 0 { 64 } else { 65 };
                s.push((t, Act::On(n, V)));
                s.push((t + 0.09, Act::Off(n)));
            }
            3.5
        }
        _ => return None,
    };
    s.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some((s, len))
}

/// A script from a file: one event a line, `secs on NOTE [velocity]`, `secs off NOTE`,
/// `secs pedal down|up`, `secs reverb on|off`; NOTE is MIDI or a name (C4, F#3, Bb2).
pub fn parse(text: &str) -> Result<(Script, f64), String> {
    let mut s = Script::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let w: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("line {}: {line:?}", i + 1);
        let at: f64 = w.first().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
        let act = match (w.get(1).copied(), w.get(2).copied()) {
            (Some("on"), Some(n)) => {
                let v = w.get(3).map_or(Some(V), |v| v.parse().ok()).ok_or_else(bad)?;
                Act::On(parse_note(n).ok_or_else(bad)?, v)
            }
            (Some("off"), Some(n)) => Act::Off(parse_note(n).ok_or_else(bad)?),
            (Some("pedal"), Some(d)) => Act::Pedal(d == "down" || d == "on"),
            (Some("reverb"), Some(d)) => Act::Reverb(d == "on"),
            _ => return Err(bad()),
        };
        s.push((at, act));
    }
    s.sort_by(|a, b| a.0.total_cmp(&b.0));
    let len = s.last().map_or(1.0, |e| e.0) + 3.0;
    Ok((s, len))
}

pub fn parse_note(s: &str) -> Option<u8> {
    if let Ok(m) = s.parse::<u8>() {
        return (m < 128).then_some(m);
    }
    let mut chars = s.chars();
    let pc = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let (acc, oct) = match rest.chars().next()? {
        '#' | '♯' => (1, &rest[rest.chars().next()?.len_utf8()..]),
        'b' | '♭' => (-1, &rest[rest.chars().next()?.len_utf8()..]),
        _ => (0, rest.as_str()),
    };
    let oct: i32 = oct.parse().ok()?;
    let m = (oct + 1) * 12 + pc + acc;
    (0..128).contains(&m).then_some(m as u8)
}

/// What a run produced.
pub struct Run {
    pub audio: Vec<f32>,
    /// Microseconds each block took to render.
    pub block_us: Vec<f32>,
    /// Frame each event reached the engine, and the event.
    pub applied: Vec<(usize, Act)>,
    /// Frames at which voices were stolen or struck again.
    pub steals: Vec<usize>,
    pub restrikes: Vec<usize>,
    pub max_voices: usize,
    /// The limiter's deepest gain.
    pub min_gain: f32,
    /// Frames the soft clipper bent.
    pub soft_clipped: u64,
}

/// Plays `script` for `secs` through `engine`, in blocks as live.
pub fn run(engine: &mut Engine, script: &Script, secs: f64) -> Run {
    let frames = (secs * RATE as f64) as usize;
    let mut audio = vec![0.0f32; frames * 2];
    let mut block_us = Vec::with_capacity(frames / BLOCK + 1);
    let mut applied = Vec::new();
    let (mut steals, mut restrikes) = (Vec::new(), Vec::new());
    let mut min_gain = 1.0f32;
    let mut next = 0;
    let mut at = 0;
    while at < frames {
        let (s0, r0) = (engine.stats.steals, engine.stats.restrikes);
        // Everything sent before this block starts is picked up now.
        while next < script.len() && (script[next].0 * RATE as f64) as usize <= at {
            let act = script[next].1;
            engine.handle(match act {
                Act::On(n, v) => Msg::NoteOn { note: n, velocity: v, at: Instant::now() },
                Act::Off(n) => Msg::NoteOff { note: n },
                Act::Pedal(d) => Msg::Pedal(d),
                Act::Reverb(r) => Msg::Reverb(r),
            });
            applied.push((at, act));
            next += 1;
        }
        let n = (frames - at).min(BLOCK);
        let started = Instant::now();
        engine.render(&mut audio[at * 2..(at + n) * 2]);
        block_us.push(started.elapsed().as_secs_f32() * 1e6);
        if engine.stats.steals > s0 {
            steals.push(at);
        }
        if engine.stats.restrikes > r0 {
            restrikes.push(at);
        }
        min_gain = min_gain.min(f32::from_bits(engine.meters.reduction.load(std::sync::atomic::Ordering::Relaxed)));
        at += n;
    }
    Run { audio, block_us, applied, steals, restrikes, max_voices: engine.stats.max_voices, min_gain, soft_clipped: engine.soft_clipped() }
}

/// One script's findings.
#[derive(Debug, Default)]
pub struct Findings {
    pub peak_db: f32,
    pub rms_db: f32,
    pub clipped: usize,
    /// Frames the soft clipper bent, after the limiter.
    pub soft_clipped: u64,
    pub limiter_db: f32,
    pub max_jump: f32,
    /// The roughest moment just after a damper or a steal, dB over just before (≤ 0 smooth).
    pub worst_click_db: f32,
    pub worst_click_at: f64,
    /// Loudest a voice was when it stopped (a cut, if well above silence), dBFS.
    pub worst_cut_db: f32,
    pub block_mean_us: f32,
    pub block_p99_us: f32,
    pub block_max_us: f32,
    pub max_voices: usize,
    pub steals: usize,
    pub restrikes: usize,
}

/// The realtime budget for a block, µs.
pub fn budget_us() -> f32 {
    BLOCK as f32 / RATE as f32 * 1e6
}

/// Renders a script and measures it. The click check runs on a second render without the
/// reverb, release and pedal samples, whose own (real) noises would hide a click.
pub fn examine(bank: &Arc<Bank>, script: &Script, secs: f64) -> (Run, Findings) {
    let mut engine = Engine::new(bank.clone());
    let run_heard = run(&mut engine, script, secs);

    let mut quiet = Engine::new(bank.clone());
    quiet.handle(Msg::Reverb(false));
    quiet.noises = false;
    let run_quiet = run(&mut quiet, script, secs);

    let lv = analysis::levels(&run_heard.audio);
    let mut f = Findings {
        peak_db: db(lv.peak),
        rms_db: db(lv.rms),
        clipped: lv.clipped,
        soft_clipped: run_heard.soft_clipped,
        limiter_db: db(run_heard.min_gain),
        max_jump: analysis::max_jump(&run_heard.audio),
        worst_cut_db: db(quiet.probe.worst_cut),
        max_voices: run_heard.max_voices,
        steals: run_heard.steals.len(),
        restrikes: run_heard.restrikes.len(),
        worst_click_db: f32::NEG_INFINITY,
        ..Default::default()
    };
    let mut blocks = run_heard.block_us.clone();
    blocks.sort_by(f32::total_cmp);
    f.block_mean_us = blocks.iter().sum::<f32>() / blocks.len().max(1) as f32;
    f.block_p99_us = blocks[(blocks.len() * 99 / 100).min(blocks.len() - 1)];
    f.block_max_us = *blocks.last().unwrap_or(&0.0);

    // Clicks: just after each damper (key up, pedal up), steal and restrike, unless a note
    // starts close by (its attack is meant to be sharp).
    let win = 48;
    let rough = analysis::roughness(&run_quiet.audio, win);
    let ons: Vec<usize> = run_quiet.applied.iter().filter(|(_, a)| matches!(a, Act::On(..))).map(|&(t, _)| t).collect();
    // A strike from 100 ms before to just past the window after.
    let near_on = |t: usize| ons.iter().any(|&o| o + RATE as usize / 10 > t && t + RATE as usize * 6 / 100 >= o);
    let mut checks: Vec<usize> = run_quiet
        .applied
        .iter()
        .filter(|(_, a)| matches!(a, Act::Off(_) | Act::Pedal(false)))
        .map(|&(t, _)| t)
        .collect();
    checks.extend(&run_quiet.steals);
    checks.extend(&run_quiet.restrikes);
    for t in checks.into_iter().filter(|&t| !near_on(t)) {
        let s = analysis::click_score(&rough, win, t, RATE as usize / 20);
        if s > f.worst_click_db {
            f.worst_click_db = s;
            f.worst_click_at = t as f64 / RATE as f64;
        }
    }
    (run_heard, f)
}

/// Each key alone at velocity `v`, no reverb: its pitch against equal temperament (A4 = 440)
/// in cents, and which partial it was read from.
pub fn pitches(bank: &Arc<Bank>, v: u8) -> Vec<(u8, f64, usize)> {
    (21..=108u8)
        .filter_map(|n| {
            let mut e = Engine::new(bank.clone());
            e.handle(Msg::Reverb(false));
            e.noises = false;
            let script = vec![(0.0, Act::On(n, v))];
            let r = run(&mut e, &script, 1.8);
            // From 0.3 s, past the hammer's thump.
            let mono: Vec<f32> = r.audio.as_chunks::<2>().0.iter().skip(RATE as usize * 3 / 10).map(|f| 0.5 * (f[0] + f[1])).collect();
            let target = analysis::midi_hz(n as i32);
            analysis::pitch(&mono, RATE, target).map(|(hz, k)| (n, analysis::cents(hz, target), k))
        })
        .collect()
}

/// A note, its seconds to −20 dB and −40 dB held, and to −60 dB after letting go.
pub type Decay = (u8, Option<f64>, Option<f64>, Option<f64>);

/// Decay and release of single notes: seconds held until −20 dB and −40 dB from the peak
/// (held 12 s), and seconds after the key comes up until −60 dB (held 2 s).
pub fn decays(bank: &Arc<Bank>, notes: &[u8]) -> Vec<Decay> {
    let win = 480;
    notes
        .iter()
        .map(|&n| {
            let mut e = Engine::new(bank.clone());
            e.handle(Msg::Reverb(false));
            let held = run(&mut e, &vec![(0.0, Act::On(n, V))], 12.0);
            let env = analysis::envelope(&held.audio, win);
            let d20 = analysis::time_to_fall(&env, win, RATE, 0, 20.0);
            let d40 = analysis::time_to_fall(&env, win, RATE, 0, 40.0);
            let mut e = Engine::new(bank.clone());
            e.handle(Msg::Reverb(false));
            e.noises = false;
            let rel = run(&mut e, &vec![(0.0, Act::On(n, V)), (2.0, Act::Off(n))], 6.0);
            let env = analysis::envelope(&rel.audio, win);
            let up = 2 * RATE as usize;
            let r60 = analysis::time_to_fall(&env, win, RATE, up + up % BLOCK, 60.0);
            (n, d20, d40, r60)
        })
        .collect()
}

/// Key-to-sound latency: single notes struck at odd moments (not on block boundaries), the
/// frames from the strike to the first output over −60 dBFS. Includes waiting for the next
/// block and any silence at the start of the sample.
pub fn latencies(bank: &Arc<Bank>) -> Vec<f64> {
    [60u8, 33, 84, 45, 72, 96, 28, 67]
        .iter()
        .enumerate()
        .filter_map(|(i, &n)| {
            let at = 0.1 + i as f64 * 0.0371;
            let mut e = Engine::new(bank.clone());
            let r = run(&mut e, &vec![(at, Act::On(n, V))], at + 0.2);
            let sent = (at * RATE as f64) as usize;
            let first = r.audio.as_chunks::<2>().0.iter().position(|f| f[0].abs().max(f[1].abs()) > 1e-3)?;
            Some((first.saturating_sub(sent)) as f64 / RATE as f64 * 1000.0)
        })
        .collect()
}

pub fn main(args: &[String]) -> i32 {
    let mut out = PathBuf::from("target/render");
    let mut names = Vec::new();
    let mut quick = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = PathBuf::from(it.next().expect("-o needs a directory")),
            "--quick" => quick = true,
            "-h" | "--help" => {
                eprintln!(
                    "piano render [SCRIPT…] [-o DIR] [--quick]\n\nSCRIPT is a built-in ({}) or a file;\n\
                     none plays them all. --quick skips the 88-key pitch and decay survey.",
                    SCRIPTS.join(", ")
                );
                return 0;
            }
            _ => names.push(a.clone()),
        }
    }
    if names.is_empty() {
        names = SCRIPTS.iter().map(|s| s.to_string()).collect();
    }
    let started = Instant::now();
    let bank = match Bank::open_env_blocking() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("piano: {e}");
            return 1;
        }
    };
    let load_secs = started.elapsed().as_secs_f64();
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("piano: {}: {e}", out.display());
        return 1;
    }
    let mut report = String::new();
    let _ = writeln!(report, "piano render — {} ({} samples, {:.1} s to load, {} MB)", bank.name(), bank.progress().1, load_secs, bank.memory() >> 20);
    let _ = writeln!(report, "budget per {BLOCK}-frame block: {:.0} µs\n", budget_us());
    let mut ok = true;
    for name in &names {
        let (script, secs) = match builtin(name) {
            Some(s) => s,
            None => match std::fs::read_to_string(name).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("piano: {name}: {e}");
                    return 1;
                }
            },
        };
        let (run, f) = examine(&bank, &script, secs);
        let stem = Path::new(name).file_stem().map_or(name.clone(), |s| s.to_string_lossy().into_owned());
        let wav = out.join(format!("{stem}.wav"));
        if let Err(e) = analysis::write_wav(&wav, &run.audio, RATE) {
            eprintln!("piano: {}: {e}", wav.display());
        }
        let flag = |bad: bool| if bad { "  ← !" } else { "" };
        let _ = writeln!(report, "{stem}  ({secs:.1} s, {})", wav.display());
        let _ = writeln!(
            report,
            "  level     peak {:6.1} dBFS  rms {:6.1} dBFS  clipped {}  limiter {:.1} dB  soft-clipped {} frames{}",
            f.peak_db, f.rms_db, f.clipped, f.limiter_db, f.soft_clipped, flag(f.clipped > 0)
        );
        let _ = writeln!(
            report,
            "  clicks    worst {:+.1} dB at {:.2} s after a damper/steal  ·  voice cut at {:.0} dBFS  ·  max jump {:.3}{}",
            f.worst_click_db,
            f.worst_click_at,
            f.worst_cut_db,
            f.max_jump,
            flag(f.worst_click_db > 6.0 || f.worst_cut_db > -60.0)
        );
        let _ = writeln!(
            report,
            "  cpu       block mean {:.0} µs  p99 {:.0} µs  max {:.0} µs  ({:.1}% of budget at p99)  voices ≤ {}  steals {}  restrikes {}{}",
            f.block_mean_us,
            f.block_p99_us,
            f.block_max_us,
            f.block_p99_us / budget_us() * 100.0,
            f.max_voices,
            f.steals,
            f.restrikes,
            flag(f.block_max_us > budget_us() * 0.5)
        );
        ok &= f.block_max_us <= budget_us() * 0.5;
        ok &= f.clipped == 0 && f.worst_click_db <= 6.0 && f.worst_cut_db <= -60.0;
    }

    let lat = latencies(&bank);
    let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    let max = lat.iter().copied().fold(0.0, f64::max);
    let _ = writeln!(
        report,
        "\nlatency   key → first sound out of the engine: mean {mean:.1} ms, max {max:.1} ms (≤ one {BLOCK}-frame block = {:.1} ms, plus the sample's onset);\n          then PipeWire plays the block after its {BLOCK}/{RATE} quantum and the device's buffer",
        BLOCK as f64 / RATE as f64 * 1000.0
    );

    if !quick {
        let p = pitches(&bank, V);
        let worst = p.iter().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
        // Neighbouring keys should follow a smooth tuning curve (pianos are stretch-tuned):
        // a key that jumps from its neighbours' average is a resampling or mapping error.
        // Up to C7: the top octave's tuning (by the remaster's tuner) jumps by design.
        let mut rough = (0u8, 0.0f64);
        for w in p.windows(3).filter(|w| w[1].0 <= 96) {
            let dev = w[1].1 - 0.5 * (w[0].1 + w[2].1);
            if dev.abs() > rough.1.abs() {
                rough = (w[1].0, dev);
            }
        }
        let _ = writeln!(report, "\npitch     88 keys at velocity {V}, cents from equal temperament (A4 = 440):");
        for row in p.chunks(12) {
            let line: Vec<String> = row.iter().map(|(n, c, _)| format!("{:>4} {:+5.1}", note_name(*n as i32), c)).collect();
            let _ = writeln!(report, "          {}", line.join("  "));
        }
        if let Some((n, c, _)) = worst {
            let _ = writeln!(report, "          furthest {} {:+.1}¢; least smooth (to C7) {} {:+.1}¢ off its neighbours{}", note_name(*n as i32), c, note_name(rough.0 as i32), rough.1, flag_bool(rough.1.abs() > 6.0));
        }
        let _ = writeln!(report, "\ndecay     held: s to −20 dB / −40 dB;  released after 2 s: s to −60 dB");
        for (n, d20, d40, r60) in decays(&bank, &[21, 33, 45, 57, 60, 69, 81, 88, 93, 100, 108]) {
            let s = |x: Option<f64>| x.map_or("  —  ".into(), |x| format!("{x:5.2}"));
            let _ = writeln!(report, "          {:>4}  {} / {}   release {}", note_name(n as i32), s(d20), s(d40), s(r60));
        }
    }
    print!("{report}");
    let _ = std::fs::write(out.join("report.txt"), &report);
    println!("\nreport: {}", out.join("report.txt").display());
    if ok {
        0
    } else {
        2
    }
}

fn flag_bool(bad: bool) -> &'static str {
    if bad {
        "  ← !"
    } else {
        ""
    }
}
