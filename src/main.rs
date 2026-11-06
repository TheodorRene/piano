//! piano: a sampled grand piano played on the computer keyboard, built with gpui.
//!
//! The home row plays the white keys and the row above the black ones (GarageBand's musical
//! typing); Space is the sustain pedal. `piano render …` plays scripted performances through
//! the same engine offline and reports on them (see `render.rs`).

mod analysis;
mod audio;
mod bank;
mod dsp;
mod engine;
mod keys;
mod render;
mod settings;

/// Counts allocations and frees on the current thread, so tests can hold the audio path to
/// none.
#[cfg(test)]
mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub struct Counting;

    fn bump() {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump();
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            bump();
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            bump();
            unsafe { System.realloc(ptr, layout, size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    pub fn allocations() -> usize {
        COUNT.with(|c| c.get())
    }
}

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, relative, rgb, size, App, Application, Bounds, FocusHandle, FontWeight, Hsla, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseUpEvent, Subscription, TitlebarOptions, Window, WindowBounds,
    WindowOptions,
};

use audio::Output;
use bank::Bank;
use engine::{Engine, Meters, Msg};
use keys::{is_black, is_nordic, note_name, typed_offset, typing_label, BASES, TYPING_TOP};
use settings::Settings;

const TEXT_FONT: &str = "Noto Sans";
/// Numbers, note names and symbols; monospaced digits don't jitter.
const MONO_FONT: &str = "JetBrainsMono Nerd Font";

// From the Coral terminal palette, as in viz and guitar.
const BG: u32 = 0x0d0d0d;
const OK: u32 = 0x8faf7a;
const WARN: u32 = 0xd9b08c;
const DANGER: u32 = 0xe0645a;

/// The piano's keys, A0 to C8.
const LOWEST: i32 = 21;
const HIGHEST: i32 = 108;
/// Velocities for the number keys 1–9, ppp to fff.
const VELOCITY_STEPS: [u8; 9] = [16, 30, 44, 58, 72, 86, 100, 114, 127];
/// How often the window redraws while nothing moves: keeps meters and ringing keys current.
const TICK: Duration = Duration::from_millis(33);

fn white(a: f32) -> Hsla {
    Hsla { h: 0.0, s: 0.0, l: 1.0, a }
}

fn hex(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

/// A dynamic marking for a velocity.
fn dynamic(v: u8) -> &'static str {
    match v {
        0..=23 => "ppp",
        24..=37 => "pp",
        38..=51 => "p",
        52..=65 => "mp",
        66..=79 => "mf",
        80..=93 => "f",
        94..=110 => "ff",
        _ => "fff",
    }
}

/// What a test presses: keys by gpui's name, `^name` to let go (`PIANO_TEST_PRESS`, since the
/// headless sway has no keyboard).
fn parse_scripted(s: &str) -> Option<(f64, String)> {
    let (at, key) = s.split_once(':')?;
    Some((at.parse().ok()?, key.to_string()))
}

struct Piano {
    focus: FocusHandle,
    output: Output,
    meters: Arc<Meters>,
    bank: Arc<Bank>,
    settings: Settings,
    /// Keys held down: gpui's key name and the note it started (so an octave change while
    /// holding still lets go of the right note).
    held: Vec<(String, i32)>,
    /// The note the mouse is holding.
    mouse_note: Option<i32>,
    pedal: bool,
    /// The level meter: the output's peak, falling back slowly.
    level: f32,
    /// A small random generator for the strike variation.
    rng: u64,
    scripted: Vec<(f64, String)>,
    started: Instant,
    _activation: Option<Subscription>,
}

impl Piano {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            if this.update(cx, |p, cx| p.tick(cx)).is_err() {
                break;
            }
        })
        .detach();
        let settings = Settings::load();
        let bank = Bank::open_default();
        bank.want_velocity(settings.velocity, settings.humanize + 8);
        let engine = Engine::new(bank.clone());
        let meters = engine.meters.clone();
        let mute = std::env::var_os("PIANO_MUTE").is_some();
        let mut output = Output::new(engine, !mute);
        output.send(Msg::Reverb(settings.reverb));
        let mut scripted: Vec<(f64, String)> =
            std::env::var("PIANO_TEST_PRESS").unwrap_or_default().split_whitespace().filter_map(parse_scripted).collect();
        scripted.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Lost focus means lost key-ups: let everything go rather than leave notes stuck.
        let activation = cx.observe_window_activation(window, |p, window, cx| {
            if !window.is_window_active() {
                p.let_go_of_everything();
                cx.notify();
            }
        });
        Self {
            focus: cx.focus_handle(),
            output,
            meters,
            bank,
            settings,
            held: Vec::new(),
            mouse_note: None,
            pedal: false,
            level: 0.0,
            rng: 0x9e37_79b9_7f4a_7c15 ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64),
            scripted,
            started: Instant::now(),
            _activation: Some(activation),
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        while self.scripted.last().is_some_and(|(at, _)| self.started.elapsed().as_secs_f64() >= *at) {
            let key = self.scripted.pop().expect("checked").1;
            match key.strip_prefix('^') {
                Some(up) => self.key_up(up),
                None => {
                    self.key_down(&key, None, false, false);
                }
            }
        }
        let peak = f32::from_bits(self.meters.peak.load(Ordering::Relaxed));
        self.level = peak.max(self.level * 0.8);
        cx.notify();
    }

    fn velocity(&mut self) -> u8 {
        // xorshift, ± humanize.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let h = self.settings.humanize as i32;
        let jitter = if h > 0 { (self.rng % (2 * h as u64 + 1)) as i32 - h } else { 0 };
        (self.settings.velocity as i32 + jitter).clamp(1, 127) as u8
    }

    fn note_on(&mut self, note: i32) {
        if (LOWEST..=HIGHEST).contains(&note) {
            let velocity = self.velocity();
            self.output.send(Msg::NoteOn { note: note as u8, velocity, at: Instant::now() });
        }
    }

    fn note_off(&mut self, note: i32) {
        // Another key (or the mouse) may still hold the same note.
        let still = self.held.iter().any(|&(_, n)| n == note) || self.mouse_note == Some(note);
        if !still && (LOWEST..=HIGHEST).contains(&note) {
            self.output.send(Msg::NoteOff { note: note as u8 });
        }
    }

    fn set_pedal(&mut self, down: bool) {
        if self.pedal != down {
            self.pedal = down;
            self.output.send(Msg::Pedal(down));
        }
    }

    fn let_go_of_everything(&mut self) {
        let notes: Vec<i32> = self.held.drain(..).map(|(_, n)| n).chain(self.mouse_note.take()).collect();
        for n in notes {
            self.note_off(n);
        }
        self.set_pedal(false);
    }

    fn set_velocity(&mut self, v: i32) {
        self.settings.velocity = v.clamp(1, 127) as u8;
        self.settings.save();
        self.bank.want_velocity(self.settings.velocity, self.settings.humanize + 8);
    }

    fn shift_octave(&mut self, by: i32) {
        self.settings.base = (self.settings.base + 12 * by).clamp(*BASES.start(), *BASES.end());
        self.settings.save();
    }

    /// A key pressed: by gpui's name, the character it types, the layout, and whether it's the
    /// key repeating. Returns whether it did anything.
    fn key_down(&mut self, key: &str, typed: Option<&str>, nordic: bool, held: bool) -> bool {
        // Holding a key down doesn't play it (or anything else) again.
        if held {
            return true;
        }
        match key {
            "space" => self.set_pedal(true),
            "z" => self.shift_octave(-1),
            "x" => self.shift_octave(1),
            "up" => self.set_velocity(self.settings.velocity as i32 + 6),
            "down" => self.set_velocity(self.settings.velocity as i32 - 6),
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                let i: usize = key.parse().expect("digit");
                self.set_velocity(VELOCITY_STEPS[i - 1] as i32);
            }
            "r" => {
                self.settings.reverb = !self.settings.reverb;
                self.settings.save();
                self.output.send(Msg::Reverb(self.settings.reverb));
            }
            "escape" => {
                self.let_go_of_everything();
                self.output.send(Msg::AllOff);
            }
            _ => match typed_offset(key, typed, nordic) {
                Some(offset) => {
                    let note = self.settings.base + offset;
                    self.held.push((key.to_string(), note));
                    self.note_on(note);
                }
                None => return false,
            },
        }
        true
    }

    fn key_up(&mut self, key: &str) {
        if key == "space" {
            self.set_pedal(false);
        } else if let Some(i) = self.held.iter().position(|(k, _)| k == key) {
            let (_, note) = self.held.remove(i);
            self.note_off(note);
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return;
        }
        let nordic = is_nordic(cx.keyboard_layout().name());
        if self.key_down(&ev.keystroke.key, ev.keystroke.key_char.as_deref(), nordic, ev.is_held) {
            cx.notify();
        }
    }

    fn on_key_up(&mut self, ev: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.key_up(&ev.keystroke.key);
        cx.notify();
    }

    fn is_held(&self, note: i32) -> bool {
        self.held.iter().any(|&(_, n)| n == note) || self.mouse_note == Some(note)
    }

    fn header(&self) -> impl IntoElement {
        let s = &self.settings;
        let pill = |label: &'static str, value: String, color: Hsla, lit: bool| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(7.))
                .rounded(px(9.))
                .bg(if lit { color.opacity(0.18) } else { white(0.05) })
                .border_1()
                .border_color(if lit { color.opacity(0.6) } else { white(0.0) })
                .text_size(px(14.))
                .child(div().text_color(white(0.5)).child(label))
                .child(div().font_family(MONO_FONT).font_weight(FontWeight::SEMIBOLD).text_color(if lit { color } else { white(0.9) }).child(value))
        };
        let top = s.base + TYPING_TOP;
        let range = format!("{}–{}", note_name(s.base.max(LOWEST)), note_name(top.min(HIGHEST)));
        let velocity = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .py(px(7.))
            .rounded(px(9.))
            .bg(white(0.05))
            .text_size(px(14.))
            .child(div().text_color(white(0.5)).child("Velocity"))
            .child(
                div()
                    .w(px(120.))
                    .h(px(6.))
                    .rounded_full()
                    .bg(white(0.1))
                    .child(div().h_full().rounded_full().w(relative(s.velocity as f32 / 127.0)).bg(hex(OK, 0.9))),
            )
            .child(
                div()
                    .w(px(64.))
                    .font_family(MONO_FONT)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{:>3} {}", s.velocity, dynamic(s.velocity))),
            );
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).mr(px(14.)).child("Piano"))
            .child(pill("Keys", range, hex(OK, 1.0), false))
            .child(velocity)
            .child(pill("Pedal", if self.pedal { "down".into() } else { "up".into() }, hex(WARN, 1.0), self.pedal))
            .child(pill("Reverb", if s.reverb { "on".into() } else { "off".into() }, hex(OK, 1.0), s.reverb))
            .child(div().flex_1())
            .child(self.meter())
    }

    /// The output level, with the limiter's work shown in red.
    fn meter(&self) -> impl IntoElement {
        let m = &self.meters;
        let peak = self.level;
        let reduction = f32::from_bits(m.reduction.load(Ordering::Relaxed));
        let db = analysis::db(peak);
        let fraction = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let color = match db {
            d if d > -3.0 || reduction < 0.97 => DANGER,
            d if d > -12.0 => WARN,
            _ => OK,
        };
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(12.))
            .text_color(white(0.4))
            .child("Level")
            .child(
                div()
                    .w(px(110.))
                    .h(px(6.))
                    .rounded_full()
                    .bg(white(0.08))
                    .child(div().h_full().rounded_full().w(relative(fraction)).bg(hex(color, 0.9))),
            )
    }

    /// The keyboard around the keys the letters reach: a fifth below to an octave above,
    /// with each key's letter. Held keys light green; keys still ringing (pedal, or letting
    /// go) glow faintly.
    fn keyboard(&self, nordic: bool, cx: &Context<Self>) -> impl IntoElement {
        let base = self.settings.base;
        let (from, to) = ((base - 5).max(LOWEST), (base + TYPING_TOP + 5).min(HIGHEST));
        // Start and end on white keys.
        let from = if is_black(from) { from - 1 } else { from };
        let to = if is_black(to) { to + 1 } else { to };
        let whites: Vec<i32> = (from..=to).filter(|&m| !is_black(m)).collect();
        let n = whites.len() as f32;
        let ringing = |m: i32| self.meters.is_ringing(m as u8);
        let fill_of = |m: i32, black: bool| {
            let reachable = (0..=TYPING_TOP).contains(&(m - base));
            match () {
                _ if self.is_held(m) => hex(OK, 1.0),
                _ if ringing(m) => if black { hex(OK, 0.45) } else { white(0.86).blend(hex(OK, 0.35)) },
                _ if black => hex(0x1d1d1d, 1.0),
                _ if reachable => white(0.88),
                _ => white(0.55),
            }
        };
        let letter = |m: i32, black: bool| {
            typing_label(m - base, nordic).map(|l| {
                div()
                    .mb(px(6.))
                    .text_size(px(15.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if black { white(0.8) } else { hex(BG, 0.8) })
                    .child(l)
            })
        };
        let press = |m: i32| {
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.mouse_note = Some(m);
                this.note_on(m);
                cx.notify();
            })
        };
        let white_keys = whites.iter().map(|&m| {
            div()
                .id(("white-key", m as usize))
                .flex_1()
                .h_full()
                .rounded_bl(px(6.))
                .rounded_br(px(6.))
                .bg(fill_of(m, false))
                .cursor_pointer()
                .flex()
                .flex_col()
                .justify_end()
                .items_center()
                .pb(px(8.))
                .children(letter(m, false))
                .when(m.rem_euclid(12) == 0, |d| {
                    d.child(div().text_size(px(11.)).font_family(MONO_FONT).text_color(hex(BG, 0.55)).child(note_name(m)))
                })
                .on_mouse_down(MouseButton::Left, press(m))
        });
        let black_keys = (from..=to).filter(|&m| is_black(m)).map(|m| {
            // Centred on the line between the white keys either side.
            let whites_below = whites.iter().filter(|&&w| w < m).count() as f32;
            div()
                .id(("black-key", m as usize))
                .absolute()
                .top_0()
                .left(relative((whites_below - 0.3) / n))
                .w(relative(0.6 / n))
                .h(relative(0.6))
                .rounded_bl(px(5.))
                .rounded_br(px(5.))
                .border_1()
                .border_color(white(0.1))
                .bg(fill_of(m, true))
                .cursor_pointer()
                .occlude()
                .flex()
                .flex_col()
                .justify_end()
                .items_center()
                .pb(px(6.))
                .children(letter(m, true))
                .on_mouse_down(MouseButton::Left, press(m))
        });
        div()
            .relative()
            .w_full()
            .flex_1()
            .min_h(px(160.))
            .max_h(px(360.))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if let Some(m) = this.mouse_note.take() {
                        this.note_off(m);
                        cx.notify();
                    }
                }),
            )
            .child(div().size_full().flex().gap(px(3.)).children(white_keys))
            .children(black_keys)
    }

    /// All 88 keys, small, with the letters' reach marked: where on the piano you are.
    fn overview(&self) -> impl IntoElement {
        let base = self.settings.base;
        let whites: Vec<i32> = (LOWEST..=HIGHEST).filter(|&m| !is_black(m)).collect();
        let n = whites.len() as f32;
        let fill_of = |m: i32, black: bool| match () {
            _ if self.is_held(m) => hex(OK, 1.0),
            _ if self.meters.is_ringing(m as u8) => hex(OK, 0.5),
            _ if black => hex(0x1d1d1d, 1.0),
            _ => white(0.7),
        };
        let x_of = |m: i32| whites.iter().filter(|&&w| w < m).count() as f32;
        let (lo, hi) = (base.max(LOWEST), (base + TYPING_TOP).min(HIGHEST));
        let reach_from = x_of(lo);
        let reach_to = x_of(hi) + if is_black(hi) { 0.0 } else { 1.0 };
        let whites_el = whites.iter().map(|&m| div().flex_1().h_full().rounded_b(px(2.)).bg(fill_of(m, false)));
        let blacks = (LOWEST..=HIGHEST).filter(|&m| is_black(m)).map(|m| {
            div()
                .absolute()
                .top_0()
                .left(relative((x_of(m) - 0.32) / n))
                .w(relative(0.64 / n))
                .h(relative(0.6))
                .rounded_b(px(2.))
                .bg(fill_of(m, true))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(34.))
                    .child(div().size_full().flex().gap(px(1.)).children(whites_el))
                    .children(blacks),
            )
            .child(
                div().relative().w_full().h(px(4.)).child(
                    div()
                        .absolute()
                        .top_0()
                        .h_full()
                        .rounded_full()
                        .bg(hex(OK, 0.8))
                        .left(relative(reach_from / n))
                        .w(relative((reach_to - reach_from) / n)),
                ),
            )
    }

    fn footer(&self) -> impl IntoElement {
        let m = &self.meters;
        let (loaded, total) = self.bank.progress();
        let latency = m.latency_us.load(Ordering::Relaxed) as f32 / 1000.0;
        let load = m.load_permille.load(Ordering::Relaxed) as f32 / 10.0;
        let voices = m.voices.load(Ordering::Relaxed);
        let status = if let Some(e) = self.bank.error() {
            div().text_color(hex(DANGER, 1.0)).child(e)
        } else if loaded < total {
            div().text_color(hex(WARN, 1.0)).child(format!("Loading samples {loaded}/{total}"))
        } else {
            div().font_family(MONO_FONT).child(format!("{voices:>2} voices · {load:.0}% CPU · key → engine {latency:.1} ms"))
        };
        div()
            .flex()
            .justify_between()
            .text_size(px(12.))
            .text_color(white(0.35))
            .child("Space pedal · Z X octave · ↑ ↓ or 1–9 velocity · R reverb · Esc all off")
            .child(status)
    }
}

impl Render for Piano {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let nordic = is_nordic(cx.keyboard_layout().name());
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_key_up(cx.listener(Self::on_key_up))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(18.))
            .p(px(22.))
            .bg(rgb(BG))
            .font_family(TEXT_FONT)
            .text_color(white(0.92))
            .child(self.header())
            .child(div().flex_1().flex().flex_col().justify_center().child(self.keyboard(nordic, cx)))
            .child(self.overview())
            .child(self.footer())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("render") {
        std::process::exit(render::main(&args[1..]));
    }
    // Tests open it under another app id, so a sway rule can keep it off the user's screen.
    let app_id = std::env::var("PIANO_APP_ID").unwrap_or_else(|_| "piano".into());
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.), px(560.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Piano".into()), ..Default::default() }),
                app_id: Some(app_id),
                window_min_size: Some(size(px(760.), px(360.))),
                ..Default::default()
            },
            |window, cx| {
                let piano = cx.new(|cx| Piano::new(window, cx));
                window.focus(&piano.read(cx).focus);
                piano
            },
        )
        .unwrap();
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamics_and_scripted_presses() {
        assert_eq!((dynamic(16), dynamic(80), dynamic(127)), ("ppp", "f", "fff"));
        assert_eq!(parse_scripted("1.5:a"), Some((1.5, "a".into())));
        assert_eq!(parse_scripted("2:^space"), Some((2.0, "^space".into())));
    }
}
