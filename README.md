# piano

A sampled grand piano you play on the laptop keyboard: a native [gpui](https://gpui.rs)
window, with sound through PipeWire.

- **Keys** — GarageBand-style musical typing: the home row A S D F G H J K L Ø Æ ' plays
  the white keys from C3 up to G4, the row above (W E, T Y U, O P, ¨) the black keys. On the
  US layout the keys after L are ; ' \ and ]. Each key on screen shows its letter for the
  layout in use (Norwegian or English). Z / X move it an octave down or up (C0 to C7).
- **Sustain pedal** — hold Space.
- **Velocity** — a laptop keyboard can't tell how hard you strike, so you set it: ↑ / ↓ in
  steps, or 1–9 for ppp to fff. Each strike varies a little (±4) so repeated notes don't
  sound mechanical.
- **Reverb** — a subtle small room, R to turn it on or off.

The screen shows a keyboard around the keys your letters reach (held keys green, keys
still ringing tinted), the whole 88 keys below with your reach marked, and the octave,
velocity, pedal, reverb and output level above.

## The sound

The samples are the **Accurate-Salamander Grand Piano** (v6.2a): Alexander Holm's
Salamander Grand Piano, a Yamaha C5 recorded at 48 kHz/24-bit with 16 velocity layers,
remastered by Chisato Yamauchi (noise removed, the tone of each key evened out, and every
note retuned by a concert piano tuner). The piano plays it as a real one behaves:

- Notes overlap freely (up to 128 voices; past that the quietest fades out quickly).
- Letting go of a key drops its damper: the note fades in about half a second, with the
  recorded sound of the felt meeting the strings and the key returning. As on a grand,
  F6 has only half a damper and the keys above have none, so they ring on.
- With the pedal down the dampers stay off; lifting it damps everything you're no longer
  holding, and pressing it again quickly catches notes that are still dying away. Notes
  struck with the pedal down get the sample set's pedal resonance, and the pedal makes its
  own soft noise.
- Striking a note that's still ringing fades the old sound under the new strike.
- Samples are resampled with cubic interpolation; there's headroom and a gentle limiter, so
  even fff chords don't clip.

A key reaches the sound engine within one 5 ms audio block (2.5 ms on average); PipeWire
adds its 256-sample quantum and the sound card's buffer.

## Run

```sh
nix run                        # from this directory
nix profile install .          # or install it: `piano`, plus a launcher entry
```

The first build downloads the samples (a 784 MB zip from archive.org) into the Nix store;
the app reads them straight from the zip, loading the velocity layers it needs in the
background (about 500 MB of memory at one velocity; 1.3 GB if every layer is used).

The launcher entry (`share/applications/piano.desktop`) is in `~/.nix-profile/share`; if
your launcher doesn't look there, symlink it into `~/.local/share/applications`.

## macOS

**Not supported yet.** It's built and tested on Linux only, and three parts are
Linux-specific:

- **Sound output** (`src/audio.rs`) is a PipeWire stream. macOS needs a CoreAudio output
  instead, e.g. through the [cpal](https://crates.io/crates/cpal) crate. The engine itself is
  portable: `Engine::render_with` fills any buffer it's handed, so the new output only has to
  call it from cpal's callback, drain the same `rtrb` queue and keep the "start on the first
  key" behaviour. `pipewire` would then be a Linux-only dependency
  (`[target.'cfg(target_os = "linux")'.dependencies]`).
- **The window toolkit** is built with gpui's Wayland backend only (`default-features =
  false, features = ["wayland"]`). On macOS gpui uses Metal; its build compiles the shaders
  with `xcrun metal`, which needs the full Xcode, or enable gpui's `runtime_shaders` feature
  to compile them at startup instead. gpui needs the `font-kit` feature there.
- **The Nix flake** patches Vulkan and Wayland into the binary's rpath, points at Mesa's
  Vulkan drivers, and links PipeWire; all of that would have to be Linux-only
  (`lib.optionals stdenv.isLinux`), and the launcher entry is a Linux `.desktop` file.

Once those are done, running it on a Mac would be: install Rust (and Xcode, unless
`runtime_shaders`), download the sample zip (the URL is in `flake.nix`), and run
`PIANO_SAMPLES=/path/to/AccurateSalamanderGrandPianoV6.2a_48khz24bit_flac.zip cargo run
--release`. The key mapping should carry over, but check the Ø Æ labels: they're chosen
when the layout's name contains "Norwegian" (`keys::is_nordic`), and what gpui reports as
the layout name on macOS is untested. Once it builds, the offline renderer
(`piano render`) and `cargo test` exercise the engine without the window or audio, so
they're the first things to run there.

## Keys

| Key | |
|---|---|
| A S D F G H J K L Ø Æ ' | white keys (US: … L ; ' \\) |
| W E T Y U O P ¨ | black keys (US: … P ]) |
| Space | sustain pedal (hold) |
| Z / X | octave down / up |
| ↑ / ↓ | velocity ±6 |
| 1–9 | velocity ppp to fff |
| R | reverb on / off |
| Esc | all notes off |

Velocity, octave and reverb are kept in `~/.local/state/piano/settings.json`.

## Credits and licence

The piano samples are licensed under
[Creative Commons Attribution 3.0](http://creativecommons.org/licenses/by/3.0/):

- **Salamander Grand Piano V3** by Alexander Holm (Yamaha C5),
  <https://archive.org/details/SalamanderGrandPianoV3>, CC-BY 3.0.
- **Accurate-Salamander Grand Piano V6.2a** by Chisato Yamauchi, with retuning by Hiroharu
  Narikawa, <https://www.ir.isas.jaxa.jp/~cyamauch/AccurateSalamander/>, a modified version
  of the above under the same licence (its README says CC-BY 3.0; the archive.org item,
  <https://archive.org/details/httpswww.ir.isas.jaxa.jpcyamauchaccuratesalamander>, lists
  CC-BY 4.0).

They're downloaded at build time and not part of this repository.
