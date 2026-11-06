//! The sample bank: the Accurate-Salamander Grand Piano, read straight from its zip.
//!
//! `sfz_live/…flat.Recommended.sfz` says which sample each key and velocity plays, with
//! per-key tuning, volume and start offset; plus the sampled releases (the strings as the
//! damper lands), the hammer/key noise on letting go, the pedal's noises, and a "pseudo pedal
//! resonance" (soft neighbouring samples tuned to the struck note while the pedal is down).
//!
//! Samples load on a background thread, as they're wanted: the layers around the velocity
//! in use first (`want_velocity`), so the piano plays within a moment of starting. Each is
//! kept as 16-bit PCM (TPDF-dithered from 24-bit, scaled to its own peak) in a `OnceLock`,
//! which the audio thread reads without locking. A sample that isn't loaded yet falls back
//! to the nearest loaded velocity layer.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

/// The SFZ used: the "live" set (no pre-onset delay) with a flat bass/treble balance.
const SFZ: &str = "sfz_live/Accurate-SalamanderGrandPiano_flat.Recommended.sfz";
/// Samples are faded out over their last this many frames, so a voice that plays to the
/// end of its sample never stops with a step.
const END_FADE: usize = 960;
/// Decoding threads.
const WORKERS: usize = 8;

/// Levels of the extra sounds against the SFZ's own (which ships with them off): dB added.
const RELEASE_DB: f32 = 0.0;
const HAMMER_DB: f32 = -3.0;
const PEDAL_DB: f32 = -3.0;
const RESONANCE_DB: f32 = 0.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A struck string.
    Note,
    /// Pedal resonance: soft, same pitch, fades in.
    Resonance,
    /// The strings as the damper lands.
    Release,
    /// The key and hammer coming back up.
    Hammer,
    /// The pedal's dampers lifting or falling.
    Noise,
}

/// A decoded sample: interleaved stereo i16 with one frame of silence before frame 0 and
/// three after the end (for the interpolator), scaled by `scale`.
pub struct SampleData {
    pub frames: usize,
    /// Its sample rate: mostly 48 kHz, but the remaster retunes some samples by their rate
    /// (F♯1 and C3 are A1 and A2 recordings at about ±3 semitones).
    pub rate: u32,
    /// Where playback starts (the SFZ's `offset`).
    pub start: usize,
    pub pcm: Box<[i16]>,
    pub scale: f32,
}

pub struct Sample {
    /// Path inside the zip.
    path: String,
    /// Where playback starts, frames (the SFZ's `offset`).
    offset: usize,
    data: OnceLock<SampleData>,
    wanted: AtomicBool,
    failed: AtomicBool,
}

#[derive(Clone, Debug)]
struct Region {
    sample: u32,
    kind: Kind,
    lokey: u8,
    hikey: u8,
    lovel: u8,
    hivel: u8,
    keycenter: f32,
    /// Cents.
    tune: f32,
    /// dB.
    volume: f32,
    /// dB per second the key was held (release samples).
    rt_decay: f32,
    /// Pedal noise: true for the pedal going down.
    pedal_down: bool,
    /// Seconds to fade 60 dB once the damper falls; infinite where there's no damper.
    release: f32,
    /// Seconds to fade in.
    attack: f32,
    /// Whether pitch follows the key.
    keytrack: bool,
}

/// What to play: a sample, at what pitch and level.
#[derive(Clone, Copy, Debug)]
pub struct Play {
    pub sample: u32,
    /// Playback rate for the note, relative to the sample's own.
    pub ratio: f64,
    pub gain: f32,
    pub attack: f32,
}

pub struct Bank {
    name: String,
    source: Option<PathBuf>,
    samples: Vec<Sample>,
    regions: Vec<Region>,
    /// amp_veltrack, 0–1.
    veltrack: f32,
    /// Region per (note, velocity), or u32::MAX.
    attack_map: Box<[[u32; 128]; 128]>,
    release_map: Box<[[u32; 128]; 128]>,
    hammer_map: [u32; 128],
    /// Up to two resonance regions per note.
    resonance_map: [[u32; 2]; 128],
    pedal_regions: Vec<u32>,
    /// Damper time per note (s to −60 dB), infinite where none.
    dampers: [f32; 128],
    loaded: AtomicUsize,
    error: Mutex<Option<String>>,
    /// Wakes the loader.
    loader: Mutex<Option<thread::Thread>>,
    /// A cheap counter for the pedal noise's coin toss.
    toss: AtomicUsize,
}

const NONE: u32 = u32::MAX;

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

impl Bank {
    /// The bank named by `PIANO_SAMPLES` (the zip, or a directory it was unpacked into),
    /// loading in the background. On error the bank is empty and `error` says why.
    pub fn open_default() -> Arc<Bank> {
        match std::env::var_os("PIANO_SAMPLES").map(PathBuf::from).ok_or_else(|| anyhow!("PIANO_SAMPLES is not set")).and_then(|p| Bank::open(&p)) {
            Ok(bank) => {
                let bank = Arc::new(bank);
                bank.want_essentials();
                Bank::start_loader(&bank);
                bank
            }
            Err(e) => {
                let bank = Bank::empty();
                *bank.error.lock().unwrap() = Some(format!("Samples: {e:#}"));
                Arc::new(bank)
            }
        }
    }

    /// The bank named by `PIANO_SAMPLES`, every sample loaded (for the renderer and tests).
    pub fn open_env_blocking() -> Result<Arc<Bank>> {
        let path = std::env::var_os("PIANO_SAMPLES").map(PathBuf::from).ok_or_else(|| anyhow!("PIANO_SAMPLES is not set (the dev shell sets it)"))?;
        let bank = Arc::new(Bank::open(&path)?);
        for s in &bank.samples {
            s.wanted.store(true, Ordering::Relaxed);
        }
        bank.load_wanted();
        if let Some(e) = bank.error() {
            bail!(e);
        }
        Ok(bank)
    }

    fn empty() -> Bank {
        Bank {
            name: String::new(),
            source: None,
            samples: Vec::new(),
            regions: Vec::new(),
            veltrack: 1.0,
            attack_map: Box::new([[NONE; 128]; 128]),
            release_map: Box::new([[NONE; 128]; 128]),
            hammer_map: [NONE; 128],
            resonance_map: [[NONE; 2]; 128],
            pedal_regions: Vec::new(),
            dampers: [0.5; 128],
            loaded: AtomicUsize::new(0),
            error: Mutex::new(None),
            loader: Mutex::new(None),
            toss: AtomicUsize::new(0),
        }
    }

    /// Reads the SFZ and lays out the regions; loads no audio.
    pub fn open(path: &Path) -> Result<Bank> {
        let text = read_entry(path, SFZ).with_context(|| format!("reading {SFZ} from {}", path.display()))?;
        let text = String::from_utf8_lossy(&text);
        let mut bank = Bank::empty();
        bank.name = "Accurate-Salamander Grand Piano v6.2a".into();
        bank.source = Some(path.to_path_buf());
        let mut ids: HashMap<String, u32> = HashMap::new();
        for ops in parse_sfz(&text) {
            let get = |k: &str| ops.get(k).map(String::as_str);
            let num = |k: &str, d: f32| get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
            if let Some(v) = get("amp_veltrack") {
                bank.veltrack = v.parse::<f32>().unwrap_or(100.0) / 100.0;
            }
            let Some(sample) = get("sample") else { continue };
            // "..\48khz24bit\021_A0v01.flac", relative to the SFZ's directory.
            let file = sample.replace('\\', "/");
            let file = file.trim_start_matches("../").to_string();
            let next = ids.len() as u32;
            let id = *ids.entry(file.clone()).or_insert(next);
            if id == next {
                let offset = num("offset", 0.0) as usize;
                bank.samples.push(Sample { path: file, offset, data: OnceLock::new(), wanted: AtomicBool::new(false), failed: AtomicBool::new(false) });
            }
            let trigger = get("trigger").unwrap_or("attack");
            let group: i32 = get("group").and_then(|g| g.parse().ok()).unwrap_or(0);
            let lokey = num("lokey", 0.0);
            let kind = match trigger {
                "release" => Kind::Release,
                "release_key" => Kind::Hammer,
                _ if lokey < 0.0 => Kind::Noise,
                _ if (300..400).contains(&group) => Kind::Resonance,
                _ => Kind::Note,
            };
            let mut volume = num("volume", 0.0) + num("group_volume", 0.0);
            volume += match kind {
                Kind::Release => RELEASE_DB,
                Kind::Hammer => HAMMER_DB,
                Kind::Noise => PEDAL_DB,
                Kind::Resonance => RESONANCE_DB,
                Kind::Note => 0.0,
            };
            let release = num("ampeg_release", 0.5);
            bank.regions.push(Region {
                sample: id,
                kind,
                lokey: lokey.max(0.0) as u8,
                hikey: num("hikey", 127.0).max(0.0) as u8,
                lovel: num("lovel", 1.0) as u8,
                hivel: num("hivel", 127.0) as u8,
                keycenter: num("pitch_keycenter", 60.0),
                tune: num("tune", 0.0),
                volume,
                rt_decay: num("rt_decay", 0.0),
                pedal_down: get("on_locc64") == Some("127"),
                release: if release >= 50.0 { f32::INFINITY } else { release },
                attack: num("ampeg_attack", 0.0),
                keytrack: get("pitch_keytrack") != Some("0"),
            });
        }
        if bank.regions.is_empty() {
            bail!("no regions in {SFZ}");
        }
        bank.build_maps();
        Ok(bank)
    }

    fn build_maps(&mut self) {
        for (i, r) in self.regions.iter().enumerate() {
            let i = i as u32;
            for note in r.lokey..=r.hikey.min(127) {
                let n = note as usize;
                match r.kind {
                    Kind::Note => {
                        for v in r.lovel..=r.hivel.min(127) {
                            self.attack_map[n][v as usize] = i;
                        }
                        self.dampers[n] = if r.release.is_finite() { r.release } else { f32::INFINITY };
                    }
                    Kind::Release => {
                        for v in r.lovel..=r.hivel.min(127) {
                            self.release_map[n][v as usize] = i;
                        }
                    }
                    Kind::Hammer => self.hammer_map[n] = i,
                    Kind::Resonance => {
                        let slot = &mut self.resonance_map[n];
                        if slot[0] == NONE {
                            slot[0] = i;
                        } else {
                            slot[1] = i;
                        }
                    }
                    Kind::Noise => {}
                }
            }
            if r.kind == Kind::Noise {
                self.pedal_regions.push(i);
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// (samples loaded, samples wanted so far).
    pub fn progress(&self) -> (usize, usize) {
        let wanted = self.samples.iter().filter(|s| s.wanted.load(Ordering::Relaxed) && !s.failed.load(Ordering::Relaxed)).count();
        (self.loaded.load(Ordering::Relaxed), wanted)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// Bytes of decoded audio held.
    pub fn memory(&self) -> usize {
        self.samples.iter().filter_map(|s| s.data.get()).map(|d| d.pcm.len() * 2).sum()
    }

    #[inline]
    pub fn sample(&self, id: u32) -> Option<&SampleData> {
        self.samples.get(id as usize)?.data.get()
    }

    /// The amplitude for a velocity: the SFZ's amp_veltrack blend of the velocity squared.
    fn velocity_gain(&self, velocity: u8) -> f32 {
        let v = velocity as f32 / 127.0;
        1.0 - self.veltrack * (1.0 - v * v)
    }

    fn play(&self, region: u32, note: u8, velocity: u8, extra_db: f32) -> Play {
        let r = &self.regions[region as usize];
        let semis = if r.keytrack { note as f32 - r.keycenter } else { 0.0 };
        let ratio = 2f64.powf((semis as f64 + r.tune as f64 / 100.0) / 12.0);
        let vel_gain = if r.kind == Kind::Noise { 1.0 } else { self.velocity_gain(velocity) };
        Play { sample: r.sample, ratio, gain: vel_gain * db_to_gain(r.volume + extra_db), attack: r.attack }
    }

    /// The note's sample at this velocity, or the nearest velocity layer that's loaded.
    /// Asks the loader for the right one if it isn't there yet.
    pub fn attack(&self, note: u8, velocity: u8) -> Option<Play> {
        let velocity = velocity.clamp(2, 127);
        let row = &self.attack_map[note as usize];
        let want = row[velocity as usize];
        if want == NONE {
            return None;
        }
        if self.loaded_region(want) {
            return Some(self.play(want, note, velocity, 0.0));
        }
        // Not loaded yet: the nearest layer that is, either way.
        self.samples[self.regions[want as usize].sample as usize].wanted.store(true, Ordering::Relaxed);
        for d in 1..127i32 {
            for v in [velocity as i32 - d, velocity as i32 + d] {
                if (2..=127).contains(&v) {
                    let r = row[v as usize];
                    if r != NONE && r != want && self.loaded_region(r) {
                        return Some(self.play(r, note, velocity, 0.0));
                    }
                }
            }
        }
        None
    }

    fn loaded_region(&self, region: u32) -> bool {
        self.samples[self.regions[region as usize].sample as usize].data.get().is_some()
    }

    /// The release sample (the strings as the damper lands) for a note let go after
    /// `held` seconds.
    pub fn release(&self, note: u8, velocity: u8, held: f32) -> Option<Play> {
        let r = self.release_map[note as usize][velocity.clamp(2, 127) as usize];
        if r == NONE || !self.loaded_region(r) {
            return None;
        }
        let decay = -self.regions[r as usize].rt_decay * held;
        Some(self.play(r, note, velocity, decay))
    }

    /// The key and hammer coming back up.
    pub fn hammer(&self, note: u8, velocity: u8, held: f32) -> Option<Play> {
        let r = self.hammer_map[note as usize];
        if r == NONE || !self.loaded_region(r) {
            return None;
        }
        let decay = -self.regions[r as usize].rt_decay * held;
        Some(self.play(r, note, velocity, decay))
    }

    /// The pedal resonance for a note struck with the pedal down: up to two soft voices.
    pub fn resonance(&self, note: u8, velocity: u8) -> [Option<Play>; 2] {
        self.resonance_map[note as usize].map(|r| (r != NONE && self.loaded_region(r)).then(|| self.play(r, note, velocity, 0.0)))
    }

    /// The pedal's noise going down or up (one of two recordings, alternately).
    pub fn pedal_noise(&self, down: bool) -> Option<Play> {
        let k = self.toss.fetch_add(1, Ordering::Relaxed);
        let mut found = [NONE; 2];
        let mut n = 0;
        for &r in &self.pedal_regions {
            if self.regions[r as usize].pedal_down == down && n < 2 {
                found[n] = r;
                n += 1;
            }
        }
        if n == 0 {
            return None;
        }
        let r = found[k % n];
        self.loaded_region(r).then(|| self.play(r, 60, 100, 0.0))
    }

    /// Seconds for a note's sound to fall 60 dB once its damper lands (infinite: none).
    pub fn damper_time(&self, note: u8) -> f32 {
        self.dampers[note as usize]
    }

    /// Marks the samples around a velocity as wanted (± `spread`), and wakes the loader.
    pub fn want_velocity(&self, velocity: u8, spread: u8) {
        let lo = velocity.saturating_sub(spread).max(2);
        let hi = velocity.saturating_add(spread).min(127);
        for note in 0..128 {
            for v in lo..=hi {
                let r = self.attack_map[note][v as usize];
                if r != NONE {
                    self.samples[self.regions[r as usize].sample as usize].wanted.store(true, Ordering::Relaxed);
                }
            }
        }
        self.wake();
    }

    /// Releases, hammer and pedal noises, and the resonance's soft layer.
    fn want_essentials(&self) {
        for r in &self.regions {
            if r.kind != Kind::Note {
                self.samples[r.sample as usize].wanted.store(true, Ordering::Relaxed);
            }
        }
    }

    fn wake(&self) {
        if let Some(t) = self.loader.lock().unwrap().as_ref() {
            t.unpark();
        }
    }

    fn start_loader(bank: &Arc<Bank>) {
        let b = bank.clone();
        let handle = thread::Builder::new()
            .name("piano-loader".into())
            .spawn(move || loop {
                b.load_wanted();
                // Also wakes now and then for samples the audio thread asked for.
                thread::park_timeout(Duration::from_millis(100));
            })
            .expect("failed to spawn the loader");
        *bank.loader.lock().unwrap() = Some(handle.thread().clone());
    }

    /// Decodes every wanted sample not yet loaded, on a few threads.
    fn load_wanted(&self) {
        let Some(source) = &self.source else { return };
        let todo: Vec<usize> = (0..self.samples.len())
            .filter(|&i| {
                let s = &self.samples[i];
                s.wanted.load(Ordering::Relaxed) && s.data.get().is_none() && !s.failed.load(Ordering::Relaxed)
            })
            .collect();
        if todo.is_empty() {
            return;
        }
        let next = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..WORKERS.min(todo.len()) {
                scope.spawn(|| {
                    let mut archive = match open_archive(source) {
                        Ok(a) => a,
                        Err(e) => {
                            *self.error.lock().unwrap() = Some(format!("Samples: {e:#}"));
                            return;
                        }
                    };
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = todo.get(k) else { break };
                        let s = &self.samples[i];
                        match decode(&mut archive, &s.path, s.offset) {
                            Ok(data) => {
                                let _ = s.data.set(data);
                                self.loaded.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                s.failed.store(true, Ordering::Relaxed);
                                *self.error.lock().unwrap() = Some(format!("{}: {e:#}", s.path));
                            }
                        }
                    }
                });
            }
        });
    }
}

enum Archive {
    Zip(zip::ZipArchive<std::fs::File>),
    Dir(PathBuf),
}

fn open_archive(path: &Path) -> Result<Archive> {
    if path.is_dir() {
        return Ok(Archive::Dir(path.to_path_buf()));
    }
    let f = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    Ok(Archive::Zip(zip::ZipArchive::new(f)?))
}

fn read_entry(path: &Path, name: &str) -> Result<Vec<u8>> {
    let mut a = open_archive(path)?;
    let mut buf = Vec::new();
    match &mut a {
        Archive::Zip(z) => {
            z.by_name(name)?.read_to_end(&mut buf)?;
        }
        Archive::Dir(d) => buf = std::fs::read(d.join(name))?,
    }
    Ok(buf)
}

fn decode(archive: &mut Archive, name: &str, offset: usize) -> Result<SampleData> {
    match archive {
        Archive::Zip(z) => decode_flac(z.by_name(name)?, name, offset),
        Archive::Dir(d) => decode_flac(std::io::BufReader::new(std::fs::File::open(d.join(name))?), name, offset),
    }
}

/// Decodes a FLAC to dithered 16-bit stereo, normalised to its peak, with the padding the
/// interpolator needs and a fade over its last `END_FADE` frames.
fn decode_flac(reader: impl Read, name: &str, offset: usize) -> Result<SampleData> {
    let mut flac = claxon::FlacReader::new(reader)?;
    let info = flac.streaminfo();
    if !(8_000..=192_000).contains(&info.sample_rate) {
        bail!("{} Hz", info.sample_rate);
    }
    let channels = info.channels as usize;
    let full = (1i64 << (info.bits_per_sample - 1)) as f32;
    let mut audio: Vec<f32> = Vec::with_capacity(info.samples.unwrap_or(0) as usize * 2);
    let mut blocks = flac.blocks();
    let mut buffer = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(buffer)? {
        for i in 0..block.duration() {
            let l = block.sample(0, i) as f32 / full;
            let r = if channels > 1 { block.sample(1, i) as f32 / full } else { l };
            audio.push(l);
            audio.push(r);
        }
        buffer = block.into_buffer();
    }
    let frames = audio.len() / 2;
    if frames < 2 * END_FADE {
        bail!("too short");
    }
    let fade_from = frames - END_FADE;
    for (k, f) in audio.as_chunks_mut::<2>().0.iter_mut().enumerate().skip(fade_from) {
        // Raised cosine to zero.
        let x = (k - fade_from) as f32 / END_FADE as f32;
        let g = 0.5 + 0.5 * (std::f32::consts::PI * x).cos();
        f[0] *= g;
        f[1] *= g;
    }
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
    let scale = peak / 32767.0;
    let mut pcm = vec![0i16; (frames + 4) * 2];
    // TPDF dither: two uniform randoms, ±1 LSB.
    let mut seed = 0x2545_f491_4f6c_dd1du64 ^ name.len() as u64;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 24) as f32
    };
    for (o, &x) in pcm[2..2 + frames * 2].iter_mut().zip(&audio) {
        let d = rand() - rand();
        *o = (x / scale + d).round().clamp(-32767.0, 32767.0) as i16;
    }
    let start = offset.min(frames - 1);
    Ok(SampleData { frames, rate: info.sample_rate, start, pcm: pcm.into_boxed_slice(), scale })
}

/// Parses SFZ text into one opcode map per region, with the `<global>`, `<master>` and
/// `<group>` opcodes it inherits. Comments, `<control>` and `<curve>` are skipped; values
/// are taken up to the next opcode (sample paths contain no spaces here).
pub fn parse_sfz(text: &str) -> Vec<HashMap<String, String>> {
    #[derive(PartialEq, Clone, Copy)]
    enum Header {
        None,
        Control,
        Global,
        Master,
        Group,
        Region,
        Curve,
    }
    let mut regions = Vec::new();
    let (mut global, mut master, mut group) = (HashMap::new(), HashMap::new(), HashMap::new());
    let mut region: Option<HashMap<String, String>> = None;
    let mut header = Header::None;
    let finish = |region: &mut Option<HashMap<String, String>>, out: &mut Vec<HashMap<String, String>>| {
        if let Some(r) = region.take() {
            out.push(r);
        }
    };
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("");
        for token in line.split_whitespace() {
            if let Some(h) = token.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
                finish(&mut region, &mut regions);
                header = match h {
                    "control" => Header::Control,
                    "global" => Header::Global,
                    "master" => Header::Master,
                    "group" => Header::Group,
                    "region" => Header::Region,
                    "curve" => Header::Curve,
                    _ => Header::None,
                };
                match header {
                    Header::Global => {
                        global.clear();
                        master.clear();
                        group.clear();
                    }
                    Header::Master => {
                        master = global.clone();
                        group.clear();
                    }
                    Header::Group => group = master.clone(),
                    Header::Region => region = Some(if group.is_empty() { master.clone() } else { group.clone() }),
                    _ => {}
                }
                continue;
            }
            let Some((k, v)) = token.split_once('=') else { continue };
            let (k, v) = (k.to_string(), v.to_string());
            match header {
                Header::Global => {
                    global.insert(k.clone(), v.clone());
                    master.insert(k.clone(), v.clone());
                    group.insert(k, v);
                }
                Header::Master => {
                    master.insert(k.clone(), v.clone());
                    group.insert(k, v);
                }
                Header::Group => {
                    group.insert(k, v);
                }
                Header::Region => {
                    if let Some(r) = region.as_mut() {
                        r.insert(k, v);
                    }
                }
                _ => {}
            }
        }
    }
    finish(&mut region, &mut regions);
    regions
}

#[cfg(test)]
pub mod synthetic {
    //! A small made-up bank for tests that don't need the real samples: decaying tones with a
    //! few partials, sampled every three keys at four velocities, and a noise burst for each
    //! release.
    use super::*;

    pub fn tone(hz: f64, secs: f64, amp: f32) -> Vec<f32> {
        let n = (secs * 48_000.0) as usize;
        (0..n)
            .flat_map(|i| {
                let t = i as f64 / 48_000.0;
                let x: f64 = [(1.0, 1.0), (2.0, 0.5), (3.0, 0.25)]
                    .iter()
                    .map(|&(k, a)| a * (std::f64::consts::TAU * hz * k * t).sin() * (-1.5 * k * t).exp())
                    .sum();
                let x = (x * amp as f64 * (t / 0.002).min(1.0)) as f32;
                [x, x * 0.9]
            })
            .collect()
    }

    fn to_data(audio: &[f32]) -> SampleData {
        let frames = audio.len() / 2;
        let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
        let scale = peak / 32767.0;
        let mut pcm = vec![0i16; (frames + 4) * 2];
        for (k, (o, &x)) in pcm[2..2 + frames * 2].iter_mut().zip(audio).enumerate() {
            let fade = ((frames - k / 2) as f32 / END_FADE as f32).min(1.0);
            *o = (x * fade / scale).round() as i16;
        }
        SampleData { frames, rate: 48_000, start: 0, pcm: pcm.into_boxed_slice(), scale }
    }

    /// `secs` long tones; `silent` makes one key (C4 = 60) play silence, for steal tests.
    pub fn bank(secs: f64) -> Arc<Bank> {
        let mut b = Bank::empty();
        b.name = "synthetic".into();
        b.veltrack = 0.9;
        let layers = [(1u8, 40u8, 0.2f32), (41, 80, 0.45), (81, 110, 0.7), (111, 127, 0.95)];
        let add = |b: &mut Bank, audio: Vec<f32>| {
            let id = b.samples.len() as u32;
            let data = to_data(&audio);
            let s = Sample { path: format!("synthetic{id}"), offset: 0, data: OnceLock::new(), wanted: AtomicBool::new(true), failed: AtomicBool::new(false) };
            let _ = s.data.set(data);
            b.samples.push(s);
            b.loaded.fetch_add(1, Ordering::Relaxed);
            id
        };
        for centre in (21..=108).step_by(3) {
            let hz = 440.0 * 2f64.powf((centre as f64 - 69.0) / 12.0);
            for &(lo, hi, amp) in &layers {
                let id = add(&mut b, tone(hz, secs, amp));
                b.regions.push(Region {
                    sample: id,
                    kind: Kind::Note,
                    lokey: (centre - 1).max(21) as u8,
                    hikey: (centre + 1).min(108) as u8,
                    lovel: lo,
                    hivel: hi,
                    keycenter: centre as f32,
                    tune: 0.0,
                    volume: 0.0,
                    rt_decay: 0.0,
                    pedal_down: false,
                    release: if centre >= 90 { f32::INFINITY } else { 0.5 },
                    attack: 0.0,
                    keytrack: true,
                });
            }
        }
        // A soft click-free release thump for every key: a decaying low tone.
        let thump = add(&mut b, tone(60.0, 0.4, 0.05));
        b.regions.push(Region {
            sample: thump,
            kind: Kind::Release,
            lokey: 21,
            hikey: 108,
            lovel: 1,
            hivel: 127,
            keycenter: 60.0,
            tune: 0.0,
            volume: -6.0,
            rt_decay: 6.0,
            pedal_down: false,
            release: 0.5,
            attack: 0.0,
            keytrack: false,
        });
        b.build_maps();
        Arc::new(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfz_regions_inherit_their_headers() {
        let text = "<global> amp_veltrack=98.5 ampeg_release=0.5\n<master> ampeg_release=5.0\n\
                    <group> tune=+2 offset=768 // comment\n<region> sample=..\\x\\a.flac lokey=22 hikey=22 lovel=2 hivel=26 pitch_keycenter=21\n\
                    <region> sample=..\\x\\b.flac lokey=23\n<master> volume=-21\n<group> group=101 trigger=release rt_decay=6\n\
                    <region> sample=..\\x\\c.flac lokey=20";
        let r = parse_sfz(text);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["tune"], "+2");
        assert_eq!(r[0]["ampeg_release"], "5.0");
        assert_eq!(r[0]["amp_veltrack"], "98.5");
        assert_eq!(r[1]["offset"], "768");
        assert_eq!(r[2]["trigger"], "release");
        assert_eq!(r[2]["volume"], "-21");
        assert_eq!(r[2]["ampeg_release"], "0.5", "a new master starts from the global");
        assert!(!r[2].contains_key("tune"));
    }

    /// The real bank's layout, if `PIANO_SAMPLES` points at it (the dev shell and the Nix
    /// build set it).
    #[test]
    fn the_real_bank_maps_every_key_and_velocity() {
        let Some(path) = std::env::var_os("PIANO_SAMPLES") else {
            eprintln!("PIANO_SAMPLES not set: skipped");
            return;
        };
        let bank = Bank::open(Path::new(&path)).unwrap();
        assert_eq!(bank.samples.len(), 641, "16 layers of 30 notes, 69 releases, 88 hammer noises, 4 pedal noises");
        assert!((bank.veltrack - 0.985).abs() < 1e-6);
        for note in 21..=108 {
            for v in 2..=127 {
                assert_ne!(bank.attack_map[note][v], NONE, "note {note} velocity {v}");
            }
            assert_ne!(bank.hammer_map[note], NONE);
        }
        assert_eq!(bank.attack_map[60][1], NONE, "velocity 1 is silent in the live set");
        assert!((bank.damper_time(60) - 0.5).abs() < 1e-6 && bank.damper_time(89) == 5.0);
        assert!(bank.damper_time(90).is_infinite() && bank.damper_time(108).is_infinite(), "no dampers from F♯6");
        assert_ne!(bank.release_map[60][80], NONE);
        assert_eq!(bank.release_map[100][80], NONE, "no release samples above D♯6's range");
        assert_ne!(bank.resonance_map[60][0], NONE);
        assert_eq!(bank.pedal_regions.len(), 4);
        // A♯0 plays A0's sample a semitone up, 2 cents sharp.
        let r = &bank.regions[bank.attack_map[22][80] as usize];
        assert_eq!((r.keycenter, r.tune), (21.0, 2.0));
    }
}
