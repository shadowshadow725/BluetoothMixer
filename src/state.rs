//! Application state: discovery poll loop, command handling, EQ/limiter sync
//! and persistence. PipeWire owns the audio graph; this file only nudges it.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;
use axum::extract::ws::Message;

use crate::{bt, net, pw, webaudio};

const POLL: Duration = Duration::from_millis(900);
const BT_TTL: Duration = Duration::from_secs(3);
const CARD_TTL: Duration = Duration::from_secs(20);
const NET_TTL: Duration = Duration::from_secs(8);
const SCAN_TIME: Duration = Duration::from_secs(11);

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Cfg {
    /// Output fader, 0.0 – 1.0, as set by the user. `None` until first run,
    /// at which point the sink's current volume is adopted rather than
    /// stomping on whatever the system was already using.
    pub master: Option<f32>,
    /// Output gain multiplier applied on top of `master`, 0.0 – 2.0.
    /// 1.0 is unity; above that the sink volume goes past 100%.
    pub gain: f32,
    /// Description of the preferred output sink (survives reconnects/reboots).
    pub output: Option<String>,
    /// Route the mix into the browser instead of a speaker/headset.
    pub web: bool,
    /// Per-input mute intent, keyed by node description.
    pub mute: BTreeMap<String, bool>,
    pub net: Vec<NetSource>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            master: None,
            gain: 1.0,
            output: None,
            web: false,
            mute: BTreeMap::new(),
            net: vec![],
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NetSource {
    pub name: String,
    pub session: u32,
    #[serde(default)]
    pub module_id: Option<u32>,
}

pub struct App {
    cfg: Mutex<Cfg>,
    solo: Mutex<Option<u32>>,
    clients: Mutex<Vec<UnboundedSender<Message>>>,
    /// Fan-out of captured PCM frames to every connected browser.
    audio: broadcast::Sender<Vec<u8>>,
    /// Audio frames lost because a client could not keep up. Surfaced in the
    /// state document because it is otherwise completely silent corruption.
    pub audio_dropped: Arc<AtomicU64>,
    /// Handle for the running libpipewire capture, so switching modes can
    /// stop it.
    capture: Mutex<Option<webaudio::Capture>>,
    /// Module id of the null sink we created, so only we unload it.
    web_sink: Mutex<Option<u32>>,
    /// Whether the stale-sink sweep has already run this process.
    web_clean: Mutex<bool>,
    /// Negotiated A2DP codec for the running Bluetooth stream, cached because
    /// reading it means a `wpctl inspect` per lookup.
    bt_codec: Mutex<(String, Instant)>,
    /// mtime of the running binary, so the UI can prove which build is live.
    pub build: String,
    last: Mutex<Option<Value>>,
    bt_at: Mutex<Instant>,
    bt_cache: Mutex<Value>,
    cards_at: Mutex<Instant>,
    cards_cache: Mutex<Value>,
    net_at: Mutex<Instant>,
    net_cache: Mutex<Value>,
    scan_until: Mutex<Instant>,
    version: Mutex<String>,
}

impl App {
    pub fn load() -> Self {
        let path = cfg_path();
        let cfg = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Cfg>(&s).ok())
            .unwrap_or_default();
        // Cache timestamps start out expired so the first poll fills everything in.
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        // Generous: at 10 ms per frame this is several seconds of slack, so a
        // brief main-thread stall on the page cannot cost us audio.
        let (audio, _) = broadcast::channel(512);
        let app = Self {
            cfg: Mutex::new(cfg),
            solo: Mutex::new(None),
            clients: Mutex::new(vec![]),
            audio,
            audio_dropped: Arc::new(AtomicU64::new(0)),
            capture: Mutex::new(None),
            web_sink: Mutex::new(None),
            web_clean: Mutex::new(false),
            bt_codec: Mutex::new((String::new(), stale)),
            build: build_stamp(),
            last: Mutex::new(None),
            bt_at: Mutex::new(stale),
            bt_cache: Mutex::new(Value::Null),
            cards_at: Mutex::new(stale),
            cards_cache: Mutex::new(Value::Array(vec![])),
            net_at: Mutex::new(stale),
            net_cache: Mutex::new(Value::Array(vec![])),
            scan_until: Mutex::new(stale),
            version: Mutex::new(String::new()),
        };
        if let Err(e) = app.save() {
            eprintln!("warning: cannot persist config to {}: {e}", path.display());
        }
        app
    }

    pub fn save(&self) -> Result<()> {
        let path = cfg_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let cfg = self.cfg.lock().unwrap().clone();
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)?;
        Ok(())
    }

    pub fn last_state(&self) -> Value {
        self.last.lock().unwrap().clone().unwrap_or(json!({}))
    }

    pub fn add_client(&self, tx: UnboundedSender<Message>) {
        self.clients.lock().unwrap().push(tx);
    }

    pub fn drop_client(&self, tx: &UnboundedSender<Message>) {
        self.clients.lock().unwrap().retain(|c| !c.same_channel(tx));
    }

    pub fn subscribe_audio(&self) -> broadcast::Receiver<Vec<u8>> {
        self.audio.subscribe()
    }

    // -- poll loop ---------------------------------------------------------

    pub async fn run(self: std::sync::Arc<Self>) {
        let mut ticker = tokio::time::interval(POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = self.build().await;
            *self.last.lock().unwrap() = Some(state.clone());
            let text = Message::Text(serde_json::to_string(&state).unwrap_or_default());
            for tx in self.clients.lock().unwrap().clone() {
                let _ = tx.send(text.clone());
            }
        }
    }

    // -- discovery ---------------------------------------------------------

    async fn nodes(&self) -> (Vec<pw::Node>, Option<String>) {
        match pw::wpctl(&["status"]).await {
            // `wpctl --version` does not exist; the banner carries the version.
            Ok(s) => {
                if self.version.lock().unwrap().is_empty() {
                    if let Some(v) = pw::parse_version(&s) {
                        *self.version.lock().unwrap() = v;
                    }
                }
                (pw::parse_status(&s), None)
            }
            Err(e) => (vec![], Some(e.to_string())),
        }
    }

    fn selected_output<'a>(&self, nodes: &'a [pw::Node]) -> Option<&'a pw::Node> {
        let cfg = self.cfg.lock().unwrap();
        let sinks: Vec<&pw::Node> = nodes.iter().filter(|n| n.kind == "sink").collect();
        // While web mode is on the browser is the output, regardless of what
        // the system default happens to be.
        if cfg.web {
            if let Some(w) = sinks.iter().find(|n| n.name == webaudio::SINK_DESC) {
                return Some(w);
            }
        }
        if sinks.is_empty() {
            return None;
        }
        cfg.output
            .as_deref()
            .and_then(|w| sinks.iter().find(|n| n.name == w).copied())
            .or_else(|| sinks.iter().find(|n| n.default).copied())
            .or(Some(sinks[0]))
    }

    /// Create the null sink and start the libpipewire capture when web mode is
    /// on; tear both down when it is off. Idempotent, so it is safe to call
    /// every poll.
    ///
    /// `nodes` is needed to put the previous sink back when web mode ends.
    /// No std lock is ever held across an await.
    async fn sync_web(&self, nodes: &[pw::Node]) -> Option<String> {
        let on = self.cfg.lock().unwrap().web;

        if !on {
            let running = self.capture.lock().unwrap().take();
            if let Some(mut c) = running {
                c.stop();
            }
            let owned = self.web_sink.lock().unwrap().take();
            if let Some(id) = owned {
                let restore = self.cfg.lock().unwrap().output.clone();
                // Put the system default back before the sink disappears.
                if let Some(name) = restore {
                    if let Some(n) = nodes.iter().find(|n| n.kind == "sink" && n.name == name) {
                        let _ = pw::set_default(n.id).await;
                    }
                }
                webaudio::drop_sink(id).await;
            }
            // A run that was killed while web mode was on leaves the null sink
            // behind; sweep it up once rather than on every poll.
            let swept = {
                let mut g = self.web_clean.lock().unwrap();
                let already = *g;
                *g = true;
                already
            };
            if !swept {
                if let Some(id) = webaudio::sink_module().await {
                    webaudio::drop_sink(id).await;
                }
            }
            return None;
        }

        let need_sink = self.web_sink.lock().unwrap().is_none();
        if need_sink {
            match webaudio::ensure_sink().await {
                Ok(id) => *self.web_sink.lock().unwrap() = Some(id),
                Err(e) => return Some(format!("web output unavailable: {e}")),
            }
            // Clients pick their sink from the system default, so the null
            // sink has to become it for the mix to arrive.
            let _ = pw::pactl(&["set-default-sink".into(), webaudio::SINK_NAME.into()]).await;
        }

        let need_capture = self.capture.lock().unwrap().is_none();
        if need_capture {
            let h = webaudio::spawn_capture(self.audio.clone(), self.audio_dropped.clone());
            *self.capture.lock().unwrap() = Some(h);
        }
        None
    }

    async fn bluetooth(&self) -> Value {
        {
            let mut at = self.bt_at.lock().unwrap();
            if at.elapsed() < BT_TTL {
                return self.bt_cache.lock().unwrap().clone();
            }
            *at = Instant::now();
        }
        let v = match bt::adapter().await {
            Ok(a) => {
                // Skip the per-device queries entirely while the radio is off.
                let powered = a.powered;
                let devices = if powered {
                    bt::devices()
                        .await
                        .map(|d| serde_json::to_value(d).unwrap_or_default())
                } else {
                    Ok(Value::Array(vec![]))
                };
                json!({
                    "adapter": a,
                    "devices": devices.unwrap_or_else(|e| json!({ "error": e.to_string() })),
                })
            }
            Err(e) => json!({ "error": e.to_string() }),
        };
        *self.bt_cache.lock().unwrap() = v.clone();
        v
    }

    async fn cards(&self) -> Value {
        {
            let mut at = self.cards_at.lock().unwrap();
            if at.elapsed() < CARD_TTL {
                return self.cards_cache.lock().unwrap().clone();
            }
            *at = Instant::now();
        }
        let v = pw::cards().await.unwrap_or(Value::Array(vec![]));
        *self.cards_cache.lock().unwrap() = v.clone();
        v
    }

    /// Keeps every configured RTP receiver loaded, re-instantiating any that died.
    async fn net_sources(&self) -> Value {
        {
            let mut at = self.net_at.lock().unwrap();
            if at.elapsed() < NET_TTL {
                return self.net_cache.lock().unwrap().clone();
            }
            *at = Instant::now();
        }
        let mut list = self.cfg.lock().unwrap().net.clone();
        for s in &mut list {
            let ok = match s.module_id {
                Some(id) => net::alive(id).await,
                None => false,
            };
            if !ok {
                match net::add(&s.name, s.session).await {
                    Ok(id) => s.module_id = Some(id),
                    Err(e) => {
                        let v = json!({ "error": e.to_string() });
                        *self.net_cache.lock().unwrap() = v.clone();
                        return v;
                    }
                }
            }
        }
        self.cfg.lock().unwrap().net = list.clone();
        let _ = self.save();
        let v = serde_json::to_value(list).unwrap_or(Value::Array(vec![]));
        *self.net_cache.lock().unwrap() = v.clone();
        v
    }

    /// Drive the selected sink to `master * gain`. Called from the poll loop so
    /// the two controls stay composable and survive an output swap.
    ///
    /// This is the *only* place the mix level is applied: the PCM we ship to
    /// the browser is raw, so exempting a sink here would silently drop the
    /// user's gain entirely rather than prevent it being applied twice.
    async fn apply_master(&self, out: Option<&pw::Node>) {
        let Some(out) = out else { return };
        let adopted = {
            let mut cfg = self.cfg.lock().unwrap();
            if cfg.master.is_none() {
                // Store the linear amplitude, not PipeWire's cubic value.
                cfg.master = out.vol.map(|v| pw::from_pw(v as f32));
                true
            } else {
                false
            }
        };
        if adopted {
            let _ = self.save();
        }
        let (master, gain) = {
            let cfg = self.cfg.lock().unwrap();
            (cfg.master.unwrap_or(1.0), cfg.gain)
        };
        let want = (master * gain).clamp(0.0, pw::MAX_GAIN);
        // out.vol is PipeWire's cubic value; compare in linear amplitude.
        if out.vol.is_some_and(|v| (pw::from_pw(v as f32) - want).abs() < 0.005) {
            return;
        }
        let _ = pw::set_volume(out.id, want).await;
    }

    /// Which codec did BlueZ actually negotiate with the phone?
    ///
    /// The phone proposes and BlueZ accepts — there is no D-Bus property or
    /// method anywhere in BlueZ to pin a codec from this side — so this is
    /// read-only, purely so the UI can tell you what you are actually running.
    async fn bluetooth_codec(&self, nodes: &[pw::Node]) -> String {
        const TTL: Duration = Duration::from_secs(5);
        {
            let g = self.bt_codec.lock().unwrap();
            if g.1.elapsed() < TTL {
                return g.0.clone();
            }
        }
        let stream = nodes
            .iter()
            .find(|n| n.kind == "stream" && n.name.starts_with("bluez_input"));
        let codec = match stream {
            Some(n) => pw::inspect(n.id)
                .await
                .ok()
                .and_then(|v| pw::prop(&v, "api.bluez5.codec"))
                .unwrap_or_default(),
            None => String::new(),
        };
        *self.bt_codec.lock().unwrap() = (codec.clone(), Instant::now());
        codec
    }

    /// Keep the graph's mute flags in sync with solo / user intent.
    async fn apply_mutes(&self, inputs: &[pw::Node]) {
        let solo = *self.solo.lock().unwrap();
        let pending: Vec<(u32, bool)> = {
            let cfg = self.cfg.lock().unwrap();
            inputs
                .iter()
                .filter_map(|n| {
                    // A soloed input is always audible, whatever its mute says.
                    let want = if solo == Some(n.id) {
                        false
                    } else {
                        cfg.mute.get(&n.name).copied().unwrap_or(false) || solo.is_some()
                    };
                    (n.muted != want).then_some((n.id, want))
                })
                .collect()
        };
        for (id, want) in pending {
            let _ = pw::set_mute(id, want).await;
        }
    }

    fn wireplumber_version(&self) -> String {
        self.version.lock().unwrap().clone()
    }

    pub async fn build(&self) -> Value {
        let (nodes, pw_err) = self.nodes().await;

        // Network I/O first: never hold a std mutex across an await.
        // Bluetooth pairing works even when PipeWire is down, so it is always polled.
        let bluetooth = self.bluetooth().await;
        let cards = self.cards().await;
        let net = self.net_sources().await;
        let bt_codec = self.bluetooth_codec(&nodes).await;
        let note = self.sync_web(&nodes).await;

        let outputs: Vec<pw::Node> = nodes.iter().filter(|n| n.kind == "sink").cloned().collect();
        let inputs: Vec<pw::Node> = nodes
            .iter()
            .filter(|n| n.kind == "source" || n.kind == "stream")
            .cloned()
            .collect();
        self.apply_mutes(&inputs).await;
        let selected = self.selected_output(&nodes);
        self.apply_master(selected).await;

        let solo = *self.solo.lock().unwrap();
        let cfg = self.cfg.lock().unwrap();
        let jnode = |n: &pw::Node| {
            json!({
                "id": n.id,
                "name": n.name,
                "vol": n.vol.map(|v| pw::from_pw(v as f32)),
                "muted": n.muted || cfg.mute.get(&n.name).copied().unwrap_or(false),
                "default": n.default,
                "bluetooth": n.bluetooth,
            })
        };

        json!({
            "ok": pw_err.is_none(),
            "error": pw_err,
            "version": self.wireplumber_version(),
            "build": self.build.clone(),
            "outputs": outputs.iter().map(&jnode).collect::<Vec<_>>(),
            "selected": selected.map(|n| n.id),
            "inputs": inputs.iter().map(|n| {
                let mut v = jnode(n);
                v["solo"] = json!(solo == Some(n.id));
                v
            }).collect::<Vec<_>>(),
            "solo": solo,
            "master": cfg.master.unwrap_or(1.0),
            "gain": cfg.gain,
            "web": cfg.web,
            "web_clients": self.clients.lock().unwrap().len(),
            "audio_dropped": self
                .audio_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            "note": note,
            "bluetooth": bluetooth,
            "bt_codec": bt_codec,
            "cards": cards,
            "net": net,
            "scanning": self.scan_until.lock().unwrap().elapsed() < SCAN_TIME,
        })
    }

    // -- commands ----------------------------------------------------------

    pub async fn command(&self, cmd: &Value) -> Result<String> {
        let c = |k: &str| {
            cmd.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let n = |k: &str| cmd.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
        let f = |k: &str| cmd.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let b = |k: &str| cmd.get(k).and_then(Value::as_bool).unwrap_or(false);

        match c("cmd").as_str() {
            "vol" => pw::set_volume(n("id"), f("vol")).await?,
            "mute" => {
                let name = self.name_of(n("id")).await;
                self.cfg.lock().unwrap().mute.insert(name, b("on"));
                pw::set_mute(n("id"), b("on")).await?;
                self.save()?;
            }
            "solo" => {
                let mut cur = self.solo.lock().unwrap();
                *cur = if !b("on") {
                    cur.filter(|&x| x != n("id"))
                } else if *cur == Some(n("id")) {
                    None
                } else {
                    Some(n("id"))
                };
            }
            "master" => {
                self.cfg.lock().unwrap().master = Some(f("vol").clamp(0.0, 1.0));
                self.save()?;
            }
            "gain" => {
                self.cfg.lock().unwrap().gain = f("vol").clamp(0.0, pw::MAX_GAIN);
                self.save()?;
            }
            "web" => {
                self.cfg.lock().unwrap().web = b("on");
                self.save()?;
            }
            "output" => {
                let (nodes, _) = self.nodes().await;
                let name = nodes
                    .iter()
                    .find(|x| x.kind == "sink" && x.id == n("id"))
                    .map(|x| x.name.clone())
                    .ok_or_else(|| anyhow!("output not found"))?;
                self.cfg.lock().unwrap().output = Some(name);
                self.save()?;
                pw::set_default(n("id")).await?;
            }
            "profile" => pw::set_profile(n("card"), &c("profile")).await?,
            "bt_power" => {
                let action = if b("on") { "power on" } else { "power off" };
                let out = bt::act(action, "").await;
                return Ok(tail(&out));
            }
            "bt_scan" => {
                *self.scan_until.lock().unwrap() = Instant::now() + SCAN_TIME;
                tokio::spawn(async {
                    let _ = bt::act("scan", "").await;
                });
            }
            "bt_action" => {
                let action = c("action");
                let out = bt::act(&action, &c("addr")).await;
                let msg = tail(&out);
                // Several bluetoothctl actions print nothing on success, so
                // without this the user gets no confirmation at all.
                return Ok(if msg.is_empty() {
                    format!("{action} ok")
                } else {
                    msg
                });
            }
            "net_add" => {
                let name = if c("name").is_empty() {
                    "Network".into()
                } else {
                    c("name")
                };
                let session = cmd.get("session").and_then(Value::as_u64).unwrap_or(0) as u32;
                let id = net::add(&name, session).await?;
                self.cfg.lock().unwrap().net.push(NetSource {
                    name,
                    session,
                    module_id: Some(id),
                });
                self.save()?;
                *self.net_at.lock().unwrap() = Instant::now() - NET_TTL * 2;
            }
            "net_remove" => {
                let idx = n("index") as usize;
                let id = self
                    .cfg
                    .lock()
                    .unwrap()
                    .net
                    .get(idx)
                    .and_then(|s| s.module_id);
                if let Some(id) = id {
                    let _ = net::remove(id).await;
                }
                self.cfg.lock().unwrap().net.remove(idx);
                self.save()?;
                *self.net_at.lock().unwrap() = Instant::now() - NET_TTL * 2;
            }
            other => return Err(anyhow!("unknown command `{other}`")),
        }
        Ok(String::new())
    }

    async fn name_of(&self, id: u32) -> String {
        let (nodes, _) = self.nodes().await;
        nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| format!("node{id}"))
    }
}

fn tail(s: &str) -> String {
    s.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Build time taken from the binary's own mtime. Deploying is easy to get out
/// of step with a rebuild, so the UI shows this rather than leaving us both
/// guessing which build is actually running.
fn build_stamp() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "unknown".into();
    };
    let Ok(md) = std::fs::metadata(&exe) else {
        return "unknown".into();
    };
    let Ok(t) = md.modified() else {
        return "unknown".into();
    };
    let d: chrono_stub::DateTime = t.into();
    d.0
}

/// Minimal UTC formatting so we do not pull in a date-time crate.
mod chrono_stub {
    use std::time::{SystemTime, UNIX_EPOCH};
    pub struct DateTime(pub String);
    impl From<SystemTime> for DateTime {
        fn from(t: SystemTime) -> Self {
            let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            // civil-from-days, so we can print YYYY-MM-DD HH:MM:SS in UTC
            let days = (secs / 86_400) as i64;
            let rem = secs % 86_400;
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z.rem_euclid(146_097);
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            DateTime(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60
            ))
        }
    }
}

fn cfg_path() -> PathBuf {
    let base = std::env::var("BLUETOOTH_MIXER_CONFIG")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from))
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("bluetooth-mixer").join("state.json")
}
