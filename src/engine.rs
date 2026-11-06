//! The realtime piano engine: voices playing samples from the `Bank`, the dampers and the
//! sustain pedal, then the mix bus (reverb, limiter).
//!
//! Everything is allocated in `Engine::new`; `drain`, `handle` and `render` never allocate or
//! lock (a test counts allocations to hold them to that), so they're safe on the audio
//! thread. Samples are read through the bank's `OnceLock`s, which a loader thread fills.
//!
//! How it behaves like a piano:
//! - A key down strikes the note; notes overlap freely, up to `POLYPHONY` voices.
//! - A key up drops the damper: the sound fades over the note's damper time (0.5 s to −60 dB;
//!   F6 has half a damper, and from F♯6 up there are none, so those ring on), with the
//!   recorded sound of the damper meeting the strings and the key and hammer returning.
//! - The pedal holds the dampers off. Lifting it damps every note no longer held; pressing
//!   it again while notes are still dying catches them (repedaling). Notes struck with the
//!   pedal down get the sample set's pedal resonance.
//! - Striking a note that still rings fades the old sound under the new attack.
//! - Past the polyphony limit the quietest voice fades out quickly; nothing is ever cut.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::bank::{Bank, Kind, Play, SampleData};
use crate::dsp::{Limiter, Reverb};

pub const RATE: u32 = 48_000;
/// Frames rendered at a time (PipeWire's quantum is asked to be 256 too).
pub const BLOCK: usize = 256;
/// Voices that sound at once before the quietest is let go. Pedalled passages ring many
/// strings, and resonance, release and noise samples count too.
pub const POLYPHONY: usize = 128;
/// Extra slots for voices fading out after being stolen, so a steal never has to cut.
const FADE_SLOTS: usize = 32;
/// A stolen voice fades out over this long (s): fast, but no click.
const STEAL_FADE: f32 = 0.010;
/// A note struck again while it still rings: the old sound fades over this long (s) under
/// the new strike.
const RESTRIKE_FADE: f32 = 0.25;
/// Release and noise samples quieter than this (−60 dB) aren't worth a voice.
const MIN_NOISE_GAIN: f32 = 1e-3;
/// Below this envelope (−80 dB) a dying voice is done.
const SILENT: f32 = 1e-4;
/// Overall gain: headroom, so a loud chord rarely needs the limiter.
const MASTER: f32 = 0.5;
/// How much of the dry sound goes to the reverb, and how loud it comes back.
const REVERB_SEND: f32 = 0.5;
const REVERB_WET: f32 = 0.22;

/// Messages from the UI.
#[derive(Clone, Copy, Debug)]
pub enum Msg {
    NoteOn { note: u8, velocity: u8, at: Instant },
    NoteOff { note: u8 },
    Pedal(bool),
    Reverb(bool),
    /// Everything stops (fast fades), the pedal comes up.
    AllOff,
}

/// What the engine tells the UI, through atomics.
#[derive(Default)]
pub struct Meters {
    /// Anything audible (voices, or the reverb's tail): the output must keep running.
    pub sounding: AtomicBool,
    pub voices: AtomicUsize,
    /// Peak of the last block (f32 bits).
    pub peak: AtomicU32,
    /// The limiter's lowest gain in the last block (f32 bits; 1 is no limiting).
    pub reduction: AtomicU32,
    /// Microseconds from the last key event to the audio thread picking it up.
    pub latency_us: AtomicU32,
    /// Render time over the block's duration, ‰.
    pub load_permille: AtomicU32,
    /// Notes that are sounding, a bit each (MIDI 0–127).
    pub ringing: [AtomicU64; 2],
}

impl Meters {
    pub fn is_ringing(&self, note: u8) -> bool {
        let word = self.ringing[(note / 64) as usize].load(Ordering::Relaxed);
        word & (1 << (note % 64)) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    /// The key is down.
    Held,
    /// The key is up but the pedal holds the damper off.
    Sustained,
    /// Dying away on its own: the damper is on (`damp` < 1), or it's a one-shot sample.
    Released,
    /// Stolen, struck again, or all-off: a short linear fade.
    Fading,
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    on: bool,
    note: u8,
    kind: Kind,
    sample: u32,
    /// Read position in the sample (frames) and step per output frame.
    pos: f64,
    step: f64,
    gain: f32,
    env: f32,
    /// Per-frame envelope rise while it fades in (the resonance), 0 when it's there.
    rise: f32,
    state: State,
    /// Per-frame envelope factor once the damper is on.
    damp: f32,
    /// Per-frame envelope decrement while fading.
    fade: f32,
    /// When it started (frames), for stealing.
    born: u64,
    /// The last frame it added, for the click probe.
    last: f32,
}

const IDLE_VOICE: Voice = Voice {
    on: false,
    note: 0,
    kind: Kind::Note,
    sample: 0,
    pos: 0.0,
    step: 1.0,
    gain: 0.0,
    env: 0.0,
    rise: 0.0,
    state: State::Fading,
    damp: 1.0,
    fade: 0.0,
    born: 0,
    last: 0.0,
};

impl Voice {
    /// A struck string (or its resonance), as opposed to a one-shot noise.
    fn is_string(&self) -> bool {
        matches!(self.kind, Kind::Note | Kind::Resonance)
    }
}

/// Counters for the offline report and the tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub steals: u64,
    pub restrikes: u64,
    pub max_voices: usize,
}

/// What the click probe saw: the loudest a voice was when it stopped or started (each
/// should be near silence: a voice ends by fading and starts at the sample's quiet onset).
#[derive(Clone, Copy, Debug, Default)]
pub struct Probe {
    pub worst_cut: f32,
    pub worst_start: f32,
}

pub struct Engine {
    bank: Arc<Bank>,
    pub meters: Arc<Meters>,
    voices: Vec<Voice>,
    /// Keys down, by note.
    keys: [bool; 128],
    /// The velocity each note was last struck with, and when (frames), for its release.
    struck: [u8; 128],
    struck_at: [u64; 128],
    pedal: bool,
    reverb_on: bool,
    reverb: Reverb,
    limiter: Limiter,
    mix: [[f32; BLOCK]; 2],
    /// Frames rendered so far.
    clock: u64,
    /// Frames since any voice sounded, for `Meters::sounding`.
    quiet_frames: u64,
    rate: f32,
    /// Play the release, hammer and pedal samples (off for checking clicks, whose real
    /// thumps would hide one).
    pub noises: bool,
    pub stats: Stats,
    pub probe: Probe,
}

impl Engine {
    pub fn new(bank: Arc<Bank>) -> Self {
        let rate = RATE as f32;
        Self {
            bank,
            meters: Arc::new(Meters::default()),
            voices: vec![IDLE_VOICE; POLYPHONY + FADE_SLOTS],
            keys: [false; 128],
            struck: [0; 128],
            struck_at: [0; 128],
            pedal: false,
            reverb_on: true,
            reverb: Reverb::new(rate),
            limiter: Limiter::new(rate),
            mix: [[0.0; BLOCK]; 2],
            clock: 0,
            quiet_frames: u64::MAX / 2,
            rate,
            noises: true,
            stats: Stats::default(),
            probe: Probe::default(),
        }
    }

    /// Takes every queued message.
    pub fn drain(&mut self, queue: &mut rtrb::Consumer<Msg>) {
        while let Ok(msg) = queue.pop() {
            if let Msg::NoteOn { at, .. } = msg {
                let us = at.elapsed().as_micros().min(u32::MAX as u128) as u32;
                self.meters.latency_us.store(us, Ordering::Relaxed);
            }
            self.handle(msg);
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::NoteOn { note, velocity, .. } => self.note_on(note.min(127), velocity),
            Msg::NoteOff { note } => self.note_off(note.min(127)),
            Msg::Pedal(down) => self.pedal(down),
            Msg::Reverb(on) => {
                if on && !self.reverb_on {
                    self.reverb.clear();
                }
                self.reverb_on = on;
            }
            Msg::AllOff => {
                self.keys = [false; 128];
                self.pedal = false;
                let rate = self.rate;
                for v in self.voices.iter_mut().filter(|v| v.on) {
                    start_fade(v, STEAL_FADE, rate);
                }
            }
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        let velocity = velocity.clamp(2, 127);
        // Struck again while ringing: the old sound gives way to the new strike.
        let mut restruck = false;
        let rate = self.rate;
        for v in self.voices.iter_mut().filter(|v| v.on && v.note == note && v.is_string()) {
            restruck |= v.state != State::Fading;
            start_fade(v, RESTRIKE_FADE, rate);
        }
        if restruck {
            self.stats.restrikes += 1;
        }
        self.keys[note as usize] = true;
        self.struck[note as usize] = velocity;
        self.struck_at[note as usize] = self.clock;
        if let Some(play) = self.bank.attack(note, velocity) {
            self.start_voice(note, Kind::Note, play);
        }
        if self.pedal {
            for play in self.bank.resonance(note, velocity).into_iter().flatten() {
                self.start_voice(note, Kind::Resonance, play);
            }
        }
    }

    fn note_off(&mut self, note: u8) {
        if !std::mem::replace(&mut self.keys[note as usize], false) {
            return;
        }
        // The key and hammer coming back up, pedal or not.
        let held = self.held_secs(note);
        if self.noises {
            if let Some(play) = self.bank.hammer(note, self.struck[note as usize], held) {
                self.start_voice(note, Kind::Hammer, play);
            }
        }
        if self.pedal {
            for v in self.voices.iter_mut().filter(|v| v.on && v.note == note && v.state == State::Held) {
                v.state = State::Sustained;
            }
        } else {
            self.damp(note);
        }
    }

    fn held_secs(&self, note: u8) -> f32 {
        (self.clock - self.struck_at[note as usize]) as f32 / self.rate
    }

    /// The damper falls on a note's strings: its voices fade, and the strings' sound as the
    /// felt meets them plays, quieter the longer the note rang. Notes without a damper ring on.
    fn damp(&mut self, note: u8) {
        let damper = self.bank.damper_time(note);
        if !damper.is_finite() {
            for v in self.voices.iter_mut().filter(|v| v.on && v.note == note && v.is_string() && v.state != State::Fading) {
                v.state = State::Released;
                v.damp = 1.0;
            }
            return;
        }
        let damp = (-6.9 / (damper * self.rate)).exp();
        let mut damped = false;
        for v in self.voices.iter_mut().filter(|v| v.on && v.note == note && v.is_string()) {
            if matches!(v.state, State::Held | State::Sustained) {
                v.state = State::Released;
                v.damp = damp;
                damped = true;
            }
        }
        if damped && self.noises {
            if let Some(play) = self.bank.release(note, self.struck[note as usize], self.held_secs(note)) {
                self.start_voice(note, Kind::Release, play);
            }
        }
    }

    fn pedal(&mut self, down: bool) {
        if self.pedal == down {
            return;
        }
        self.pedal = down;
        if self.noises {
            if let Some(play) = self.bank.pedal_noise(down) {
                self.start_voice(0, Kind::Noise, play);
            }
        }
        if down {
            // Repedaling: the dampers lift off strings still dying, which ring on.
            for v in self.voices.iter_mut().filter(|v| v.on && v.is_string() && v.state == State::Released && v.damp < 1.0) {
                v.state = State::Sustained;
            }
        } else {
            for note in 0..128u8 {
                if !self.keys[note as usize] && self.voices.iter().any(|v| v.on && v.note == note && v.state == State::Sustained) {
                    self.damp(note);
                }
            }
        }
    }

    fn start_voice(&mut self, note: u8, kind: Kind, play: Play) {
        let Some(data) = self.bank.sample(play.sample) else { return };
        if !matches!(kind, Kind::Note | Kind::Resonance) {
            // Noises are extras: never worth stealing a string for, nor playing when they'd
            // be inaudible (a release after a long-held note has decayed by `rt_decay`).
            let full = self.voices.iter().filter(|v| v.on).count() >= POLYPHONY;
            if full || play.gain < MIN_NOISE_GAIN {
                return;
            }
        }
        let start = data.start;
        let step = play.ratio * data.rate as f64 / self.rate as f64;
        let first = (data.pcm[(start + 1) * 2] as f32).abs().max((data.pcm[(start + 1) * 2 + 1] as f32).abs()) * data.scale;
        let rise = if play.attack > 0.0 { 1.0 / (play.attack * self.rate) } else { 0.0 };
        let env = if rise > 0.0 { 0.0 } else { 1.0 };
        self.probe.worst_start = self.probe.worst_start.max(first * play.gain * env * MASTER);
        let slot = self.free_slot();
        self.voices[slot] = Voice {
            on: true,
            note,
            kind,
            sample: play.sample,
            pos: start as f64,
            step,
            gain: play.gain,
            env,
            rise,
            state: if matches!(kind, Kind::Note | Kind::Resonance) { State::Held } else { State::Released },
            damp: 1.0,
            fade: 0.0,
            born: self.clock,
            last: 0.0,
        };
    }

    /// A slot for a new voice. Past the polyphony limit the quietest voice starts fading
    /// quickly (a slow fade, such as a restruck note's, is hurried first; then released
    /// voices before sustained before held, resonance and noises before strings, quieter
    /// and older first), so there's always a spare slot while it finishes.
    fn free_slot(&mut self) -> usize {
        // Fading voices count too: they still take time to finish.
        let sounding = self.voices.iter().filter(|v| v.on).count();
        if sounding >= POLYPHONY {
            let clock = self.clock as f32;
            let quick = STEAL_FADE * self.rate;
            let rank = |v: &Voice| {
                let held = match v.state {
                    State::Fading => -1.0,
                    State::Released => 0.0,
                    State::Sustained => 1.0,
                    State::Held => 2.0,
                };
                let string = if v.kind == Kind::Note { 1.0 } else { 0.0 };
                let age = ((clock - v.born as f32) / RATE as f32 / 10.0).min(1.0);
                held * 8.0 + string * 4.0 + (v.env * v.gain).min(1.0) * 2.0 - age
            };
            let rate = self.rate;
            // Not voices already fading out at least as fast as a steal would.
            let candidate = |v: &&mut Voice| v.on && !(v.state == State::Fading && v.env <= v.fade * quick);
            if let Some(victim) = self.voices.iter_mut().filter(candidate).min_by(|a, b| rank(a).total_cmp(&rank(b))) {
                start_fade(victim, STEAL_FADE, rate);
                self.stats.steals += 1;
            }
        }
        if let Some(i) = self.voices.iter().position(|v| !v.on) {
            return i;
        }
        // Every slot busy (only when many steals land within one fade): take the quietest.
        let i = (0..self.voices.len()).min_by(|&a, &b| self.voices[a].last.total_cmp(&self.voices[b].last)).expect("voices");
        self.probe.worst_cut = self.probe.worst_cut.max(self.voices[i].last * MASTER);
        i
    }

    /// Renders interleaved stereo into `out`.
    pub fn render(&mut self, out: &mut [f32]) {
        let frames = out.len() / 2;
        self.render_with(frames, |i, l, r| {
            out[2 * i] = l;
            out[2 * i + 1] = r;
        });
    }

    /// Renders `frames` frames, handing each to `put(index, left, right)`.
    /// Samples the soft clipper has had to bend so far (the limiter couldn't catch them).
    pub fn soft_clipped(&self) -> u64 {
        self.limiter.clipped
    }

    pub fn render_with(&mut self, frames: usize, mut put: impl FnMut(usize, f32, f32)) {
        let started = Instant::now();
        let mut done = 0;
        let mut peak = 0.0f32;
        while done < frames {
            let n = (frames - done).min(BLOCK);
            self.render_block(n);
            for i in 0..n {
                let (l, r) = (self.mix[0][i], self.mix[1][i]);
                peak = peak.max(l.abs()).max(r.abs());
                put(done + i, l, r);
            }
            done += n;
        }
        let m = &self.meters;
        m.peak.store(peak.to_bits(), Ordering::Relaxed);
        m.reduction.store(self.limiter.take_reduction().to_bits(), Ordering::Relaxed);
        let budget = frames as f64 / self.rate as f64;
        let load = started.elapsed().as_secs_f64() / budget.max(1e-9);
        m.load_permille.store((load * 1000.0) as u32, Ordering::Relaxed);
    }

    fn render_block(&mut self, n: usize) {
        let [mix_l, mix_r] = &mut self.mix;
        mix_l[..n].fill(0.0);
        mix_r[..n].fill(0.0);
        let bank = &*self.bank;
        let mut ringing = [0u64; 2];
        let mut active = 0;
        for v in self.voices.iter_mut().filter(|v| v.on) {
            let Some(data) = bank.sample(v.sample) else {
                v.on = false;
                continue;
            };
            play(v, data, &mut mix_l[..n], &mut mix_r[..n]);
            if v.on {
                active += 1;
                if v.kind == Kind::Note && v.state != State::Fading {
                    ringing[(v.note / 64) as usize] |= 1 << (v.note % 64);
                }
            } else {
                self.probe.worst_cut = self.probe.worst_cut.max(v.last * MASTER);
            }
        }
        self.stats.max_voices = self.stats.max_voices.max(active);
        for i in 0..n {
            let (mut l, mut r) = (mix_l[i] * MASTER, mix_r[i] * MASTER);
            if self.reverb_on {
                let (wl, wr) = self.reverb.run((l + r) * 0.5 * REVERB_SEND);
                l += wl * REVERB_WET;
                r += wr * REVERB_WET;
            }
            let (l, r) = self.limiter.run(l, r);
            mix_l[i] = l;
            mix_r[i] = r;
        }
        self.clock += n as u64;
        // The reverb's tail rings on for a couple of seconds after the last voice.
        self.quiet_frames = if active > 0 { 0 } else { self.quiet_frames.saturating_add(n as u64) };
        let m = &self.meters;
        m.sounding.store(self.quiet_frames < 3 * RATE as u64, Ordering::Relaxed);
        m.voices.store(active, Ordering::Relaxed);
        m.ringing[0].store(ringing[0], Ordering::Relaxed);
        m.ringing[1].store(ringing[1], Ordering::Relaxed);
    }
}

fn start_fade(v: &mut Voice, secs: f32, rate: f32) {
    let step = v.env.max(1e-6) / (secs * rate);
    if v.state != State::Fading {
        v.state = State::Fading;
        v.fade = step;
    } else {
        // Already fading: never slower than asked.
        v.fade = v.fade.max(step);
    }
}

/// Adds one voice's next `out.len()` frames, reading its sample with 4-point cubic Hermite
/// interpolation, and moves its envelope along.
#[inline]
fn play(v: &mut Voice, data: &SampleData, out_l: &mut [f32], out_r: &mut [f32]) {
    let end = data.frames as f64;
    let pcm = &data.pcm;
    let scale = data.scale * v.gain;
    for (l, r) in out_l.iter_mut().zip(out_r.iter_mut()) {
        if v.pos >= end {
            v.on = false;
            return;
        }
        match v.state {
            State::Held | State::Sustained => {}
            State::Released => {
                v.env *= v.damp;
                if v.env < SILENT {
                    v.on = false;
                    return;
                }
            }
            State::Fading => {
                v.env -= v.fade;
                if v.env <= 0.0 {
                    v.on = false;
                    v.last = 0.0;
                    return;
                }
            }
        }
        if v.rise > 0.0 {
            v.env += v.rise;
            if v.env >= 1.0 {
                v.env = 1.0;
                v.rise = 0.0;
            }
        }
        let i = v.pos as usize;
        let t = (v.pos - i as f64) as f32;
        // `pcm` has one frame of silence before the sample and three after, so i − 1 … i + 2
        // are always there: frame k sits at (k + 1) · 2.
        let base = i * 2;
        let at = |k: usize| (pcm[base + k * 2] as f32, pcm[base + k * 2 + 1] as f32);
        let (a, b, c, d) = (at(0), at(1), at(2), at(3));
        let g = scale * v.env;
        let (x, y) = (hermite(a.0, b.0, c.0, d.0, t) * g, hermite(a.1, b.1, c.1, d.1, t) * g);
        *l += x;
        *r += y;
        v.last = x.abs().max(y.abs());
        v.pos += v.step;
    }
}

#[inline]
fn hermite(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let c1 = 0.5 * (x1 - xm1);
    let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
    let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
    ((c3 * t + c2) * t + c1) * t + x0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{self, db};
    use crate::bank::synthetic;
    use crate::render::{self, Act};

    fn engine() -> Engine {
        let mut e = Engine::new(synthetic::bank(3.0));
        e.handle(Msg::Reverb(false));
        e
    }

    fn on(note: u8, velocity: u8) -> Msg {
        Msg::NoteOn { note, velocity, at: Instant::now() }
    }

    fn block(e: &mut Engine, secs: f64) -> Vec<f32> {
        let mut out = vec![0.0; (secs * RATE as f64) as usize * 2];
        e.render(&mut out);
        out
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn rendering_never_allocates() {
        let mut e = engine();
        let (mut tx, mut rx) = rtrb::RingBuffer::new(64);
        let mut out = vec![0.0; BLOCK * 2];
        e.render(&mut out);
        let before = crate::alloc_count::allocations();
        for k in 0..400u32 {
            let note = 21 + (k * 7 % 88) as u8;
            let _ = tx.push(on(note, 90));
            let _ = tx.push(Msg::Pedal(k % 50 < 25));
            if k % 3 == 0 {
                let _ = tx.push(Msg::NoteOff { note });
            }
            e.drain(&mut rx);
            e.render(&mut out);
        }
        e.handle(Msg::AllOff);
        e.render(&mut out);
        assert_eq!(crate::alloc_count::allocations(), before, "the audio path allocated (or freed)");
        assert!(e.stats.steals > 0, "the polyphony limit was reached");
    }

    #[test]
    fn notes_overlap_and_the_damper_ends_them_smoothly() {
        let mut e = engine();
        e.handle(on(60, 80));
        block(&mut e, 0.2);
        e.handle(on(64, 80));
        block(&mut e, 0.2);
        assert!(e.meters.is_ringing(60) && e.meters.is_ringing(64), "C and E ring together");
        e.handle(Msg::NoteOff { note: 60 });
        e.handle(Msg::NoteOff { note: 64 });
        let tail = block(&mut e, 1.0);
        assert!(e.voices.iter().all(|v| !v.on || v.kind != Kind::Note), "damped within a second");
        // The damper's fade is smooth: no rougher just after it than just before.
        let mut whole = vec![0.0; 0];
        let mut e = engine();
        e.noises = false;
        e.handle(on(60, 80));
        whole.extend(block(&mut e, 0.5));
        e.handle(Msg::NoteOff { note: 60 });
        whole.extend(block(&mut e, 0.5));
        let score = analysis::click_score(&analysis::roughness(&whole, 48), 48, RATE as usize / 2, 2400);
        assert!(score < 3.0, "release click {score:.1} dB");
        assert!(peak(&tail[tail.len() - 2000..]) < 1e-3);
    }

    #[test]
    fn the_pedal_holds_notes_and_repedaling_catches_them() {
        let mut e = engine();
        e.handle(Msg::Pedal(true));
        e.handle(on(60, 80));
        block(&mut e, 0.1);
        e.handle(Msg::NoteOff { note: 60 });
        block(&mut e, 1.0);
        assert!(e.meters.is_ringing(60), "held by the pedal");
        e.handle(Msg::Pedal(false));
        block(&mut e, 0.05);
        let damped = e.voices.iter().find(|v| v.on && v.kind == Kind::Note).map(|v| v.env).unwrap();
        assert!(damped < 0.9, "pedal up damps it");
        // Pedal down again before it's gone: it rings on from there.
        e.handle(Msg::Pedal(true));
        block(&mut e, 0.5);
        let caught = e.voices.iter().find(|v| v.on && v.kind == Kind::Note).map(|v| v.env).unwrap();
        assert!(caught > 0.2 && (caught - damped).abs() < 0.2, "{damped} → {caught}");
        // A key still held when the pedal comes up keeps sounding.
        e.handle(on(67, 80));
        block(&mut e, 0.1);
        e.handle(Msg::Pedal(false));
        block(&mut e, 1.0);
        assert!(e.meters.is_ringing(67) && !e.meters.is_ringing(60));
    }

    #[test]
    fn keys_without_dampers_ring_on() {
        let mut e = engine();
        e.handle(on(96, 80));
        block(&mut e, 0.1);
        e.handle(Msg::NoteOff { note: 96 });
        block(&mut e, 1.0);
        assert!(e.meters.is_ringing(96), "C7 has no damper");
    }

    #[test]
    fn striking_a_ringing_note_again_crossfades_without_a_click() {
        let mut e = engine();
        e.noises = false;
        let mut out = Vec::new();
        e.handle(on(57, 100));
        out.extend(block(&mut e, 0.3));
        for _ in 0..6 {
            e.handle(on(57, 100));
            out.extend(block(&mut e, 0.08));
        }
        assert!(e.stats.restrikes == 6);
        let notes = e.voices.iter().filter(|v| v.on && v.kind == Kind::Note).count();
        assert!(notes <= 4, "old strikes fade out ({notes} left)");
        assert!(e.probe.worst_cut < 1e-4, "no voice stopped short: {}", db(e.probe.worst_cut));
    }

    #[test]
    fn past_the_polyphony_limit_voices_fade_rather_than_cut() {
        let mut e = engine();
        e.noises = false;
        e.handle(Msg::Pedal(true));
        for k in 0..300u32 {
            e.handle(on(21 + (k % 88) as u8, 100));
            block(&mut e, 0.004);
        }
        assert!(e.stats.steals > 100);
        assert!(e.voices.iter().filter(|v| v.on).count() <= POLYPHONY + FADE_SLOTS);
        assert!(e.probe.worst_cut < 1e-3, "a voice was cut at {:.0} dBFS", db(e.probe.worst_cut));
    }

    #[test]
    fn resampled_notes_play_in_tune() {
        // The synthetic bank samples every third key; the others are resampled by ±1 semitone.
        for note in [57u8, 58, 59, 60, 61, 81, 82, 83] {
            let mut e = engine();
            e.handle(on(note, 80));
            let out = block(&mut e, 1.2);
            let mono: Vec<f32> = out.chunks(2).skip(4800).map(|f| f[0]).collect();
            let target = analysis::midi_hz(note as i32);
            let (hz, _) = analysis::pitch(&mono, RATE, target).unwrap();
            assert!(analysis::cents(hz, target).abs() < 0.5, "{note}: {:+.2}¢", analysis::cents(hz, target));
        }
    }

    #[test]
    fn a_key_sounds_within_one_block() {
        let bank = synthetic::bank(1.0);
        for offset in [0.0, 0.001, 0.0049] {
            let mut e = Engine::new(bank.clone());
            let at = 0.1 + offset;
            let r = render::run(&mut e, &vec![(at, Act::On(60, 80))], at + 0.1);
            let first = r.audio.chunks(2).position(|f| f[0].abs() > 1e-3).unwrap();
            let waited = first as f64 / RATE as f64 - at;
            assert!(waited <= BLOCK as f64 / RATE as f64 + 0.001, "{waited}");
        }
    }

    #[test]
    fn a_full_block_of_voices_renders_well_inside_the_budget() {
        let mut e = engine();
        e.handle(Msg::Pedal(true));
        for n in 0..POLYPHONY as u8 {
            e.handle(on(21 + n % 88, 100));
        }
        let mut out = vec![0.0; BLOCK * 2];
        let started = Instant::now();
        let blocks = 200;
        for _ in 0..blocks {
            e.render(&mut out);
        }
        let per_block = started.elapsed().as_secs_f64() / blocks as f64;
        let budget = BLOCK as f64 / RATE as f64;
        // Every key held (the synthetic bank has no resonance voices), plus restrike fades.
        assert!(e.meters.voices.load(Ordering::Relaxed) >= 88);
        // Generous, since tests run side by side: in release it's a few per cent.
        assert!(per_block < budget * 0.5, "{:.0} µs of {:.0}", per_block * 1e6, budget * 1e6);
    }

    #[test]
    fn loud_chords_are_limited_not_clipped() {
        let mut e = Engine::new(synthetic::bank(2.0));
        for n in [36u8, 43, 48, 52, 55, 60, 64, 67, 72, 76, 79, 84] {
            e.handle(on(n, 127));
        }
        let out = block(&mut e, 1.0);
        let lv = analysis::levels(&out);
        assert!(lv.peak < 1.0 && lv.clipped == 0, "{lv:?}");
    }

    /// The real samples, loaded once for all the tests that use them.
    fn real_bank() -> Option<Arc<Bank>> {
        static BANK: std::sync::OnceLock<Option<Arc<Bank>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| {
            std::env::var_os("PIANO_SAMPLES")?;
            Some(Bank::open_env_blocking().expect("PIANO_SAMPLES is set but the bank doesn't load"))
        })
        .clone()
    }

    #[test]
    fn the_real_piano_plays_every_script_cleanly() {
        let Some(bank) = real_bank() else {
            eprintln!("PIANO_SAMPLES not set: skipped");
            return;
        };
        for name in render::SCRIPTS {
            let (script, secs) = render::builtin(name).unwrap();
            let (_, f) = render::examine(&bank, &script, secs);
            assert_eq!(f.clipped, 0, "{name}: clipped");
            assert!(f.worst_click_db <= 6.0, "{name}: click {:+.1} dB at {:.2} s", f.worst_click_db, f.worst_click_at);
            assert!(f.worst_cut_db <= -60.0, "{name}: a voice stopped at {:.0} dBFS", f.worst_cut_db);
            assert!(f.block_max_us < render::budget_us() * 0.5, "{name}: a block took {:.0} µs", f.block_max_us);
        }
    }

    #[test]
    fn the_real_piano_is_in_tune_and_sounds_at_once() {
        let Some(bank) = real_bank() else {
            eprintln!("PIANO_SAMPLES not set: skipped");
            return;
        };
        let p = render::pitches(&bank, 80);
        assert_eq!(p.len(), 88);
        // A stretch-tuned piano: within 25¢ of equal temperament from A1 to C7, and following
        // a smooth curve (a resampling or mapping slip would be 100¢ off, or jump).
        for w in p.windows(3).filter(|w| (33..=96).contains(&w[1].0)) {
            let (n, c, _) = w[1];
            assert!(c.abs() < 25.0, "{n}: {c:+.1}¢");
            let off = c - 0.5 * (w[0].1 + w[2].1);
            assert!(off.abs() < 6.0, "{n}: {off:+.1}¢ off its neighbours");
        }
        let lat = render::latencies(&bank);
        assert!(lat.iter().all(|&ms| ms <= BLOCK as f64 / RATE as f64 * 1000.0 + 1.0), "{lat:?}");
    }
}
