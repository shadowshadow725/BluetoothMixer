//! "Play in this browser" output: pipe the mixed audio to the web UI instead of
//! a speaker or a Bluetooth headset.
//!
//! A PulseAudio null sink gives the mix somewhere to go, and that sink's
//! monitor ports carry exactly the signal we want to ship to the browser. We
//! attach a native libpipewire capture stream to those monitor ports and push
//! raw s16le PCM down a WebSocket, which goes straight into an AudioWorklet
//! on the other end — no encoding, no container.
//!
//! This used to be `parec` reading the sink's Pulse monitor source, but that
//! route carried ~1.9 s of latency that no Pulse knob could reach.

use anyhow::{anyhow, Result};
use pipewire as pw;
use pw::spa;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Name the capture node advertises in `pw-dump`.
pub const NODE_NAME: &str = "btmixer-web-capture";

/// Format of the PCM we push over the WebSocket. The browser decodes these
/// bytes as little-endian signed 16-bit samples, interleaved.
pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// If the socket cannot keep up for this long we stop dropping audio and
/// back off: fill the gap with silence rather than clipping. TCP has already
/// retransmitted by this point, so the data is coming, just late — and late
/// audio is worse than a short gap.

/// One WebSocket binary frame. 20 ms is 1920 frames, so the browser gets 50
/// messages a second rather than 100. Every message is a main-thread task on
/// the `ScriptProcessorNode` path, and halving the event rate measurably
/// reduces the stalls that starve it. Going finer costs more than it buys.
pub const CHUNK_MS: usize = 20;

pub const SINK_NAME: &str = "btmixer_web";
/// `pactl load-module` splits its arguments on whitespace and has no quoting,
/// so this has to be a single token; the UI prettifies it.
pub const SINK_DESC: &str = "WebOutput";

/// Bytes per streamed frame.
pub fn chunk_bytes() -> usize {
    RATE as usize / 1000 * CHUNK_MS as usize * CHANNELS as usize * 2
}

/// Module id of the null sink, if one is currently loaded.
pub async fn sink_module() -> Option<u32> {
    let out = crate::pw::pactl(&["list".into(), "short".into(), "modules".into()])
        .await
        .ok()?;
    out.lines()
        .filter(|l| l.split_whitespace().nth(1) == Some("module-null-sink"))
        .find_map(|l| {
            let id = l.split_whitespace().next()?.parse().ok()?;
            l.contains(SINK_NAME).then_some(id)
        })
}

/// Create the null sink (idempotent) and return its module id.
pub async fn ensure_sink() -> Result<u32> {
    if let Some(id) = sink_module().await {
        return Ok(id);
    }
    let out = crate::pw::pactl(&[
        "load-module".into(),
        "module-null-sink".into(),
        format!("sink_name={SINK_NAME}"),
        format!("sink_properties=device.description={SINK_DESC}"),
    ])
    .await?;
    out.split_whitespace()
        .last()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("null-sink did not return a module id"))
}

pub async fn drop_sink(id: u32) {
    let _ = crate::pw::pactl(&["unload-module".into(), id.to_string()]).await;
}

/// Attach a native libpipewire capture to the null sink's monitor ports and
/// pump it into `tx` as fixed-size frames.
pub fn spawn_capture(tx: tokio::sync::broadcast::Sender<Vec<u8>>) -> Capture {
    let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let stop = Arc::new(AtomicBool::new(false));

    // The PipeWire main loop is a blocking, non-Send-friendly object, so it
    // gets its own OS thread. The real-time process callback only copies bytes
    // out of the buffer and hands them over; it never blocks or allocates a
    // queue, so it cannot stall the audio graph.
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name(NODE_NAME.to_string())
            .spawn(move || capture_thread(raw_tx, stop))
            .ok()
    };
    if thread.is_none() {
        eprintln!("web output: could not start the {NODE_NAME} thread");
    }

    let pump = tokio::spawn(async move {
        if let Err(e) = pump_loop(raw_rx, tx).await {
            eprintln!("web output: capture stopped: {e}");
        }
    });

    Capture {
        stop,
        pump,
        _thread: thread,
    }
}

/// Handle to a running capture. Dropping it does *not* stop the capture; call
/// [`Capture::stop`] when web mode goes away.
pub struct Capture {
    stop: Arc<AtomicBool>,
    pump: tokio::task::JoinHandle<()>,
    /// Detached on stop: the loop quits within one tick of the flag being set,
    /// and nothing needs its result.
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.pump.abort();
        self._thread.take();
    }
}

/// Owns the PipeWire main loop, core and capture stream. Runs to completion on
/// the calling thread; returns once [`AtomicBool`] flips to `true`.
fn capture_thread(tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>, stop: Arc<AtomicBool>) {
    if let Err(e) = capture_thread_inner(tx, stop) {
        eprintln!("web output: capture stopped: {e}");
    }
}

fn capture_thread_inner(
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let ml = pw::main_loop::MainLoopBox::new(None)?;
    let ctx = pw::context::ContextBox::new(ml.loop_(), None)?;
    let core = ctx.connect(None)?;

    // Deliberately no `media.class`: WirePlumber treats a stream that arrives
    // with one as fully formed and never gives it ports, which leaves it
    // suspended forever. Omitting it is what pw-cat does.
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        // Without this WirePlumber names the stream after the application and
        // `wpctl status` lists our own capture among the mixer inputs.
        *pw::keys::MEDIA_NAME => NODE_NAME,
        *pw::keys::APP_NAME => "bluetooth-mixer",
        *pw::keys::NODE_NAME => NODE_NAME,
        *pw::keys::NODE_DESCRIPTION => "BluetoothMixer web capture",
        // Attach to the monitor ports of the sink rather than a source node.
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        *pw::keys::TARGET_OBJECT => SINK_NAME,
        *pw::keys::AUDIO_FORMAT => "S16LE",
        *pw::keys::AUDIO_RATE => RATE.to_string(),
        *pw::keys::AUDIO_CHANNELS => CHANNELS.to_string(),
    };

    let stream = pw::stream::StreamBox::new(&core, "capture", props)?;

    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(|_s, _d, old, new| {
            if old != new {
                eprintln!("web output: capture {NODE_NAME} is {new:?}");
            }
        })
        .process(move |stream, _| {
            // Real-time thread: dequeue, copy out, drop (which requeues the
            // buffer). Nothing here can block or wait on a receiver.
            let Some(mut buf) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buf.datas_mut();
            let Some(plane) = datas.first_mut() else {
                return;
            };
            let stride = plane.chunk().stride().max(0) as usize;
            let chunk = plane.chunk();
            let start = chunk.offset() as usize;
            let want = chunk.size() as usize;
            let Some(src) = plane.data() else {
                return;
            };
            let start = start.min(src.len());
            let end = start.saturating_add(want).min(src.len());
            if end <= start {
                return;
            }
            let mut out = Vec::with_capacity(end - start);
            if stride == 0 || stride >= end - start {
                out.extend_from_slice(&src[start..end]);
            } else {
                // Stride shorter than the chunk: the plane holds a sub-frame,
                // so copy row by row.
                let mut off = start;
                while off + stride <= end {
                    out.extend_from_slice(&src[off..off + stride]);
                    off += stride;
                }
            }
            // Dropping `buf` returns it to the stream's pool.
            drop(buf);
            if !out.is_empty() {
                // Unbounded queue, so this cannot block the real-time thread.
                let _ = tx.send(out);
            }
        })
        .register()?;

    // Ask for exactly the PCM we ship to the browser, so the negotiated format
    // can never silently drift from what the AudioWorklet expects.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(RATE);
    audio_info.set_channels(CHANNELS as u32);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&bytes).unwrap()];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // The stream is driven by the graph, but a sink that has gone idle can
    // stop feeding us; poll the stop flag so the thread always exits.
    // `MainLoop` is a repr(transparent) wrapper over the same pointer the box
    // hands out, and both live on this stack frame.
    let loop_: &'static pw::main_loop::MainLoop = unsafe { &*(ml.as_raw_ptr() as *const _) };
    let tick = Duration::from_millis(50);
    let timer = loop_.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            loop_.quit();
        }
    });
    let _ = timer.update_timer(Some(tick), Some(tick));

    ml.run();
    Ok(())
}

/// Frame and pace the raw PCM stream into exact `chunk_bytes()` slices.
async fn pump_loop(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    tx: tokio::sync::broadcast::Sender<Vec<u8>>,
) -> Result<()> {
    let size = chunk_bytes();
    let slot = Duration::from_millis(CHUNK_MS as u64);
    let mut carry: Vec<u8> = Vec::new();
    let mut next_due = tokio::time::Instant::now();

    while let Some(mut data) = rx.recv().await {
        carry.append(&mut data);
        while carry.len() >= size {
            // Hold every chunk to a fixed 20 ms slot on the wall clock, and
            // re-anchor whenever we have fallen behind.
            let now = tokio::time::Instant::now();
            if next_due <= now {
                next_due = now + slot;
            } else {
                tokio::time::sleep_until(next_due).await;
                next_due += slot;
            }
            let chunk: Vec<u8> = carry.drain(..size).collect();
            let _ = tx.send(chunk);
        }
    }
    Err(anyhow!("capture stream closed"))
}
