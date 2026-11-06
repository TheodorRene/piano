# AGENTS.md

Notes for coding agents working on piano. The user-facing overview is in `README.md`. The
sibling projects `../guitar` (tuner and singing monitor, same user) and `../viz` are the
reference for gpui 0.2 patterns and the Nix setup; their `AGENTS.md` files have more gpui
gotchas. The keyboard handling, the PipeWire output and the headless runner come from guitar.

## Working with the user

- They play guitar and sing tenor; this is a piano to play on the laptop keyboard. **Sound
  quality and feel come first**: smooth (no clicks, no clipping), low latency, piano-like.
- **Iterate in small steps:** build one thing, verify it with the harness below, then stop
  and report. Their ideas shift once they see (and hear) things running.
- **After every change:** run the tests, then `git add -A && nix profile upgrade piano`, so
  the installed app (launcher and `piano` on the PATH) is current. Don't commit unless asked.
- **Don't disturb them:** never open a window on their screen and never play sound unasked.
  Use the offline renderer and the headless runner; tell them when it's ready to try.
- Ask before downloading anything over about 1 GB.
- **Hardware and setup:** Ubuntu 26.04, sway on Wayland, PipeWire, Intel Arc B390. Keyboard
  layouts "Norwegian (no dead keys)" and "English (US)".

## Build and test

```sh
nix develop -c cargo build --release
nix develop -c cargo test --release        # with the real samples: the dev shell sets PIANO_SAMPLES
nix develop -c cargo clippy --release --all-targets
git add -A && nix build                    # flakes only see tracked files; runs the tests too
```

### The verification harness (you can't hear it, so measure it)

- **Offline renderer:** `nix develop -c ./target/release/piano render [SCRIPT…] [-o DIR]
  [--quick]` plays scripted performances through the real engine (no PipeWire) and writes
  `DIR/<script>.wav` (32-bit float, as heard, with reverb) and `DIR/report.txt` (default
  `target/render/`). Exit code 2 if a check fails. Built-ins (`render::SCRIPTS`): `scale`,
  `chord`, `repeat` (12 notes/s, then with the pedal), `pedal`, `polyphony` (pedalled
  glissando up and down all 88 keys: steals), `dynamics`, `loud` (fff 12-note chords: the
  limiter), `trill`. A script file is one event a line: `0.5 on C4 [vel]`, `1.0 off C4`,
  `1.2 pedal down|up`, `reverb on|off`. Per script the report gives:
  - **level:** peak, RMS, samples at full scale, limiter gain reduction, frames the soft
    clipper had to bend;
  - **clicks:** a second render without reverb and release/pedal samples; after every key
    up, pedal up, steal and restrike (unless a strike is near), the roughest 1 ms (RMS of
    the second difference) against the median of the 40 ms before (≤ 6 dB passes; a real
    click is +20 dB or more). Plus the click probe: the loudest any voice was when it
    stopped (must be ≤ −60 dBFS; voices end by fading, so it's usually −110 or lower), and
    the largest sample-to-sample jump;
  - **cpu:** render time per 256-frame block (mean, p99, max) against the 5333 µs budget,
    peak voices, steals and restrikes.
  Without `--quick` it adds every key's pitch in cents from equal temperament (single
  notes, velocity 80, FFT peak; below 130 Hz from the strongest of the first four
  partials), decay times (−20/−40 dB held) and damper release times (−60 dB), and the
  **latency**: frames from a key event at an odd moment to the first output over −60 dBFS.
- **Tests** (`cargo test`) guard the same things: on a synthetic bank (always) no
  allocation or free on the audio path (a counting global allocator, `alloc_count` in
  `main.rs`), overlap and smooth damping, pedal and repedaling, no-damper keys, restrikes,
  polyphony steals without cuts, resampled pitch within 0.5¢, latency ≤ one block, CPU,
  limiting; and, when `PIANO_SAMPLES` is set (dev shell, Nix build), every built-in script
  on the real bank passes the report's checks, every key from A1 to C7 is within 25¢ and on
  a smooth tuning curve, and latency ≤ one block.
- **Screenshots without the user's screen or speakers:** `nix develop -c
  tools/gui-headless.sh -r 1400x700 -s 3 -p '1:a 1.1:d 1.2:g 1.3:space 2:^a'` runs the app in
  its own headless sway with `PIANO_MUTE=1` (the engine renders at real-time pace into
  nothing; PipeWire is never opened) and scratch settings, and saves `shot.png`. `-p` presses
  keys by gpui name at those seconds (`^name` lets go; `space` is the pedal) through
  `PIANO_TEST_PRESS`, since the headless sway has no keyboard. `-b 2:2.6` takes a burst of
  shots (`burst-NNN.png`, ~65 ms apart) instead, for anything that changes quickly; tile
  them with `nix shell nixpkgs#imagemagick -c magick montage burst-*.png -tile 3x3 sheet.png`.
  It can't show the user's Norwegian layout (no keyboard in the headless seat): the US
  labels show; `keys.rs` tests the Nordic ones.

## Architecture

One binary crate. `piano` opens the window; `piano render …` is the offline renderer.

| Module | Role |
|---|---|
| `bank.rs` | the sample set: reads `sfz_live/…flat.Recommended.sfz` from the zip (`parse_sfz`: global/master/group/region inheritance), maps (note, velocity) to regions for notes, releases (`trigger=release`), hammer noise (`release_key`), pedal noise and pedal resonance (groups 301–399); a loader thread decodes wanted samples (claxon, 8 threads) to dithered 16-bit, peak-normalised, padded for the interpolator, faded at the end; `want_velocity` asks for the layers around a velocity; unloaded layers fall back to the nearest loaded one; `synthetic::bank` for tests |
| `engine.rs` | the realtime engine: `Msg`s in, `Meters` (atomics) out; voices with Held/Sustained/Released/Fading states, damper times from the bank, pedal (repedaling), restrike crossfade, polyphony stealing by fade, 4-point Hermite resampling, then reverb and limiter. Allocation-free after `new` |
| `dsp.rs` | `Limiter` (stereo peak limiter, no look-ahead, then a soft clipper under 0.98) and `Reverb` (8-line FDN, damped, with pre-delay) |
| `audio.rs` | `Output`: the PipeWire stream (from guitar: NODE_LATENCY 256/48000, renders only `buffer.requested()`), started on the first key, paused after 20 s silent, woken by a `pw::channel`; or a silent real-time-paced thread with `PIANO_MUTE`. Events go over an `rtrb` ring |
| `render.rs` | the offline renderer and report (above) |
| `analysis.rs` | levels, roughness/click score, envelope and decay times, FFT pitch, WAV writing |
| `keys.rs` | musical typing (`TYPING_KEYS`, `typed_offset`, `typing_label`, `is_nordic`), note names |
| `settings.rs` | velocity, humanize, octave, reverb; `$XDG_STATE_HOME/piano/settings.json` |
| `main.rs` | gpui `Piano` view: keys → `Msg`s (held keys remember their note, so an octave change mid-chord lets go of the right one; lost focus lets everything go), header pills, keyboard around the reach with letters, 88-key overview, footer status; 33 ms tick redraws meters |

### Conventions

- **Colours:** the Coral palette as in guitar and viz: `OK` green (held keys, reach),
  `WARN` amber (pedal down, loading), `DANGER` red (limiting) on `#0d0d0d`.
- **Fonts:** Noto Sans for text, JetBrainsMono Nerd Font for numbers, note names, symbols.
- **gpui:** never shape text in a canvas, and draw no big filled paths (both made text
  flicker on the user's Intel Arc; the headless runner can't show it). Quads and divs, and
  a `RenderImage` for big shapes. The UI here is all divs.

## Gotchas

- **The samples:** `flake.nix` fetches the 784 MB FLAC zip with `fetchurl`; the app and
  tests find it through `PIANO_SAMPLES` (wrapper default, dev shell, build env). archive.org
  served it at ~300 KB/s in one stream but ~2 MB/s per ranged request, so it was fetched
  in 12 parallel ranges with curl, checked against archive.org's MD5, and added with
  `nix-store --add-fixed sha256 <file>` (the same store path `fetchurl` makes).
- **Sample rates aren't all 48 kHz:** the remaster retunes by sample rate (47862–48250 Hz),
  and F♯1 and C3 are A1 and A2 recordings at 40410 and 56950 Hz. The step is
  `ratio × sample rate / output rate`.
- **The SFZ's extras ship disabled** (CC20–23 = 0). Here they're on: sampled release
  (−21 dB, `rt_decay` 6–9 dB/s, pedal up only), hammer noise (−37 −3 dB, every key up),
  pedal noise (−39/−45 −3 dB), pedal resonance (groups 301–314: the struck pitch from the
  soft layer of samples 3/6/9 keys away, −6 to −18 dB, 50 ms fade-in, only when struck with
  the pedal down). Levels are `RELEASE_DB` etc. in `bank.rs`. Release and noise samples
  never steal a voice and are skipped under −60 dB.
- **Velocity:** the live SFZ is silent at velocity 1 (clamped to 2); `amp_veltrack=98.5`
  means gain = 1 − 0.985·(1 − (v/127)²) on top of the layer choice.
- **Tuning is stretched by design** (the remaster's tuner): A0 −19¢, C4 −3¢, C7 +23¢, and
  the top octave jumps to +56¢ at B7 where the C8 sample takes over. The smoothness check
  stops at C7 for that reason.
- **The sustain pedal is Space,** held. Auto-repeat (`is_held`) is ignored for every key.
- **The PipeWire output is untested by agents:** playing would reach the user's speakers.
  The engine and its timing are tested offline; the user hears it first. The stream pauses
  after 20 s of silence (not guitar's 3 s): after a pause the card may suspend, and waking
  it costs latency on the next key.
- **`ls` is aliased** (eza) in the tool shell and once hung; use `command ls`.
