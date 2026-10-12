//! Application state: discovery poll loop, command handling, EQ/limiter sync
//! and persistence. PipeWire owns the audio graph; this file only nudges it.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

use crate::{bt, net, pw};

const POLL: Duration = Duration::from_millis(900);
const BT_TTL: Duration = Duration::from_secs(3);
const CARD_TTL: Duration = Duration::from_secs(20);
const NET_TTL: Duration = Duration::from_secs(8);
const SCAN_TIME: Duration = Duration::from_secs(11);

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Cfg {
    /// Low shelf 120 Hz, mid 500 Hz, upper-mid 3 kHz, high shelf 8 kHz, in dB.
    pub eq: [f32; 4],
    pub limiter: bool,
    /// Description of the preferred output sink (survives reconnects/reboots).
    pub output: Option<String>,
    /// Per-input mute intent, keyed by node description.
    pub mute: BTreeMap<String, bool>,
    pub net: Vec<NetSource>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            eq: [0.0; 4],
            limiter: false,
            output: None,
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
    clients: Mutex<Vec<UnboundedSender<String>>>,
    applied_graph: Mutex<Option<String>>,
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
        let app = Self {
            cfg: Mutex::new(cfg),
            solo: Mutex::new(None),
            clients: Mutex::new(vec![]),
            applied_graph: Mutex::new(None),
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

    pub fn add_client(&self, tx: UnboundedSender<String>) {
        self.clients.lock().unwrap().push(tx);
    }

    pub fn drop_client(&self, tx: &UnboundedSender<String>) {
        self.clients.lock().unwrap().retain(|c| !c.same_channel(tx));
    }

    // -- poll loop ---------------------------------------------------------

    pub async fn run(self: std::sync::Arc<Self>) {
        let mut ticker = tokio::time::interval(POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if self.version.lock().unwrap().is_empty() {
                let v = pw::wpctl(&["--version"]).await.unwrap_or_default();
                *self.version.lock().unwrap() = v.trim().to_string();
            }
            let state = self.build().await;
            let text = serde_json::to_string(&state).unwrap_or_default();
            *self.last.lock().unwrap() = Some(state);
            for tx in self.clients.lock().unwrap().clone() {
                let _ = tx.send(text.clone());
            }
        }
    }

    // -- discovery ---------------------------------------------------------

    async fn nodes(&self) -> (Vec<pw::Node>, Option<String>) {
        match pw::wpctl(&["status"]).await {
            Ok(s) => (pw::parse_status(&s), None),
            Err(e) => (vec![], Some(e.to_string())),
        }
    }

    fn selected_output<'a>(&self, nodes: &'a [pw::Node]) -> Option<&'a pw::Node> {
        let sinks: Vec<&pw::Node> = nodes.iter().filter(|n| n.kind == "sink").collect();
        if sinks.is_empty() {
            return None;
        }
        let want = self.cfg.lock().unwrap().output.clone();
        want.as_deref()
            .and_then(|w| sinks.iter().find(|n| n.name == w).copied())
            .or_else(|| sinks.iter().find(|n| n.default).copied())
            .or(Some(sinks[0]))
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

    /// Push EQ/limiter changes into the filter-chain node, creating it if needed.
    async fn sync_filter(&self, nodes: &[pw::Node]) -> Option<String> {
        let (eq, limiter) = {
            let cfg = self.cfg.lock().unwrap();
            (cfg.eq, cfg.limiter)
        };
        let graph = pw::filter_graph(&eq, limiter).to_string();

        if let Some(n) = nodes.iter().find(|n| n.name == pw::FILTER_DESC) {
            if self.applied_graph.lock().unwrap().as_deref() == Some(graph.as_str()) {
                return None;
            }
            return match pw::update_filter(n.id, &Value::from(graph.clone())).await {
                Ok(()) => {
                    *self.applied_graph.lock().unwrap() = Some(graph);
                    None
                }
                Err(e) => Some(format!("EQ update failed: {e}")),
            };
        }

        // Filter is gone (reboot, output swap) - build it again.
        *self.applied_graph.lock().unwrap() = None;
        let out = self.selected_output(nodes)?;
        let target = pw::inspect(out.id)
            .await
            .ok()
            .and_then(|v| pw::prop(&v, "node.name"));
        match pw::create_filter(target.as_deref(), &Value::from(graph)).await {
            Ok(_) => Some(format!("EQ/limiter created on {}", out.name)),
            Err(e) => Some(format!("EQ/limiter unavailable: {e}")),
        }
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

        let filter_note = match pw_err {
            Some(_) => None,
            None => self.sync_filter(&nodes).await,
        };

        let outputs: Vec<pw::Node> = nodes.iter().filter(|n| n.kind == "sink").cloned().collect();
        let inputs: Vec<pw::Node> = nodes
            .iter()
            .filter(|n| n.kind == "source" || n.kind == "stream")
            .cloned()
            .collect();
        self.apply_mutes(&inputs).await;
        let selected = self.selected_output(&nodes);

        let solo = *self.solo.lock().unwrap();
        let cfg = self.cfg.lock().unwrap();
        let jnode = |n: &pw::Node| {
            json!({
                "id": n.id,
                "name": n.name,
                "vol": n.vol,
                "muted": n.muted || cfg.mute.get(&n.name).copied().unwrap_or(false),
                "default": n.default,
                "bluetooth": n.bluetooth,
            })
        };

        json!({
            "ok": pw_err.is_none(),
            "error": pw_err,
            "version": self.wireplumber_version(),
            "outputs": outputs.iter().map(&jnode).collect::<Vec<_>>(),
            "selected": selected.map(|n| n.id),
            "inputs": inputs.iter().map(|n| {
                let mut v = jnode(n);
                v["solo"] = json!(solo == Some(n.id));
                v
            }).collect::<Vec<_>>(),
            "solo": solo,
            "master": selected.and_then(|n| n.vol).unwrap_or(1.0),
            "eq": cfg.eq,
            "limiter": cfg.limiter,
            "filter_live": nodes.iter().any(|n| n.name == pw::FILTER_DESC),
            "bluetooth": bluetooth,
            "cards": cards,
            "net": net,
            "note": filter_note,
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
                let (nodes, _) = self.nodes().await;
                if let Some(o) = self.selected_output(&nodes) {
                    pw::set_volume(o.id, f("vol")).await?;
                }
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
                // Rebuild the filter so it lands on the newly selected sink.
                if let Some(f) = nodes.iter().find(|x| x.name == pw::FILTER_DESC) {
                    let _ = pw::destroy_filter(f.id).await;
                }
                *self.applied_graph.lock().unwrap() = None;
            }
            "eq" => {
                let i = n("band") as usize;
                if i < 4 {
                    self.cfg.lock().unwrap().eq[i] = f("gain").clamp(-12.0, 12.0);
                    self.save()?;
                }
            }
            "eq_reset" => {
                self.cfg.lock().unwrap().eq = [0.0; 4];
                self.save()?;
            }
            "limiter" => {
                self.cfg.lock().unwrap().limiter = b("on");
                self.save()?;
            }
            "profile" => pw::set_profile(n("card"), &c("profile")).await?,
            "bt_power" => {
                bt::act(if b("on") { "power" } else { "power-off" }, "").await;
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
                return Ok(tail(&out));
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
