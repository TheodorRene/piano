//! Getting the engine's sound out: a PipeWire playback stream (as in guitar's piano), or, for
//! tests and the headless runner, a silent thread that renders at the same pace.
//!
//! The engine lives on the audio thread. The UI sends it `Msg`s through a lock-free ring
//! (`rtrb`), and reads `Meters` (atomics) back. The stream starts on the first key and pauses
//! once the piano has been silent for `IDLE`, so the output can sleep; a key wakes it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

use crate::engine::{Engine, Msg, BLOCK, RATE};

/// Pause the stream after this long without sound. Longer than guitar's 3 s: after a pause
/// the sound card may suspend (WirePlumber does after 5 s), and waking it costs latency on
/// the next key, so this only pauses between sessions of playing, not between phrases.
const IDLE: Duration = Duration::from_secs(20);

pub struct Output {
    producer: rtrb::Producer<Msg>,
    wake: Option<pw::channel::Sender<()>>,
    /// The engine, until the output thread starts and takes it.
    pending: Option<(Engine, rtrb::Consumer<Msg>)>,
    audible: bool,
}

impl Output {
    /// `audible`: play through PipeWire; otherwise render silently at the same pace.
    pub fn new(engine: Engine, audible: bool) -> Self {
        let (producer, consumer) = rtrb::RingBuffer::new(1024);
        Self { producer, wake: None, pending: Some((engine, consumer)), audible }
    }

    /// Queues a message for the engine, starting or waking the output if it's asleep. Until
    /// the first key the output isn't started: other messages go straight to the engine.
    pub fn send(&mut self, msg: Msg) {
        if let Some((engine, _)) = &mut self.pending {
            if !matches!(msg, Msg::NoteOn { .. }) {
                engine.handle(msg);
                return;
            }
        }
        let _ = self.producer.push(msg);
        if let Some((engine, consumer)) = self.pending.take() {
            let audible = self.audible;
            let (wake, woken) = pw::channel::channel();
            thread::Builder::new()
                .name("piano-audio".into())
                .spawn(move || {
                    let result = if audible { run_pipewire(engine, consumer, woken) } else { run_silent(engine, consumer) };
                    if let Err(e) = result {
                        eprintln!("piano: audio output failed: {e}");
                    }
                })
                .expect("failed to spawn the audio thread");
            self.wake = Some(wake);
        } else if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
    }
}

/// Renders blocks at real-time pace into nothing: the headless runner's stand-in for the
/// sound card, so the UI sees voices and meters as it would.
fn run_silent(mut engine: Engine, mut consumer: rtrb::Consumer<Msg>) -> Result<()> {
    let mut out = vec![0.0f32; BLOCK * 2];
    let period = Duration::from_secs_f64(BLOCK as f64 / RATE as f64);
    let mut next = Instant::now();
    loop {
        engine.drain(&mut consumer);
        engine.render(&mut out);
        next += period;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
    }
}

fn run_pipewire(engine: Engine, consumer: rtrb::Consumer<Msg>, woken: pw::channel::Receiver<()>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamRc::new(
        core,
        "piano",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "piano",
            *pw::keys::APP_NAME => "piano",
            *pw::keys::AUDIO_CHANNELS => "2",
            *pw::keys::NODE_LATENCY => "256/48000",
        },
    )?;

    let meters = engine.meters.clone();
    let mut state = (engine, consumer);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let (engine, consumer) = &mut state;
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            const STRIDE: usize = 2 * 4;
            let frames = match data.data() {
                Some(bytes) => {
                    // Only what the graph asked for: filling the whole buffer adds latency.
                    let room = bytes.len() / STRIDE;
                    let frames = if requested > 0 { requested.min(room) } else { room.min(BLOCK) };
                    engine.drain(consumer);
                    // The engine renders f32 stereo; PipeWire's buffer is bytes, maybe unaligned.
                    let (frames_out, _) = bytes[..frames * STRIDE].as_chunks_mut::<STRIDE>();
                    engine.render_with(frames, |i, l, r| {
                        frames_out[i][..4].copy_from_slice(&l.to_le_bytes());
                        frames_out[i][4..].copy_from_slice(&r.to_le_bytes());
                    });
                    frames
                }
                None => 0,
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = STRIDE as _;
            *chunk.size_mut() = (STRIDE * frames) as _;
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).expect("valid format pod")];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // Pause when silent for a while; a key wakes it.
    let active = Rc::new(Cell::new(true));
    let quiet_since = Rc::new(Cell::new(None::<Instant>));
    let (idle_stream, idle_active) = (stream.clone(), active.clone());
    let timer = mainloop.loop_().add_timer(move |_| {
        if !idle_active.get() || meters.sounding.load(Ordering::Relaxed) {
            quiet_since.set(None);
            return;
        }
        let since = quiet_since.get().unwrap_or_else(Instant::now);
        quiet_since.set(Some(since));
        if since.elapsed() >= IDLE {
            let _ = idle_stream.set_active(false);
            idle_active.set(false);
        }
    });
    timer.update_timer(Some(Duration::from_millis(500)), Some(Duration::from_millis(500))).into_result()?;
    let _woken = woken.attach(mainloop.loop_(), move |()| {
        if !active.get() {
            let _ = stream.set_active(true);
            active.set(true);
        }
    });

    mainloop.run();
    Ok(())
}
