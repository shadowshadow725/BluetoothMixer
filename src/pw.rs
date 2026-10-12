//! Thin wrappers around the audio stack CLIs that ship with PipeWire/WirePlumber.
//! No audio code lives here: PipeWire does the mixing, we only drive its CLI.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

async fn run(prog: &str, args: &[String], timeout: u64) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(timeout),
        Command::new(prog).args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow!("{prog} {} timed out", args.join(" ")))??;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if err.is_empty() { stdout } else { err };
        bail!("{prog} {}: {detail}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn wpctl(args: &[&str]) -> Result<String> {
    run(
        "wpctl",
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        8,
    )
    .await
}

pub async fn pwctl(args: &[String]) -> Result<String> {
    run("pwctl", args, 8).await
}

pub async fn pactl(args: &[String]) -> Result<String> {
    run("pactl", args, 8).await
}

/// `pw-cli -m json 0 create-object <props> <params>` — used to spawn the EQ/limiter
/// filter-chain node. PipeWire/WirePlumber then auto-links it to the target output.
pub async fn create_object(props: Value) -> Result<String> {
    let out = run(
        "pw-cli",
        &[
            "-m".into(),
            "json".into(),
            "0".into(),
            "create-object".into(),
            props.to_string(),
            "{}".into(),
        ],
        8,
    )
    .await?;
    // pw-cli prints the created object; pull the new id out when we can.
    for line in out.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            return Ok(id.to_string());
        }
    }
    Ok(String::new())
}

pub async fn set_volume(id: u32, vol: f32) -> Result<()> {
    wpctl(&[
        "set-volume",
        &id.to_string(),
        &format!("{:.4}", vol.clamp(0.0, 1.0)),
    ])
    .await
    .map(|_| ())
}

pub async fn set_mute(id: u32, on: bool) -> Result<()> {
    wpctl(&["set-mute", &id.to_string(), if on { "1" } else { "0" }])
        .await
        .map(|_| ())
}

pub async fn set_default(id: u32) -> Result<()> {
    wpctl(&["set-default", &id.to_string()]).await.map(|_| ())
}

pub async fn set_profile(card: u32, profile: &str) -> Result<()> {
    wpctl(&["set-profile", &card.to_string(), profile])
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// wpctl status parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: u32,
    /// Description as shown by PipeWire ("Built-in Audio Analog Stereo").
    pub name: String,
    pub kind: String, // "sink" | "source" | "stream"
    pub vol: Option<f64>,
    pub muted: bool,
    pub default: bool,
    pub bluetooth: bool,
}

const BOX: &[char] = &[
    '\u{2500}', '\u{2502}', '\u{251C}', '\u{2514}', '\u{250C}', '\u{2524}', '\u{2534}', ' ',
];

/// Parses `wpctl status` without pulling in a regex crate. The tree drawing
/// characters are stripped, the enclosing section keyword decides the node kind.
pub fn parse_status(out: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut kind = String::from("other");

    for line in out.lines() {
        let t = line.trim();

        // Section headers look like "Sinks:", "Stream Outputs:", "Input Devices:".
        if t.ends_with(':') {
            let low = t.to_ascii_lowercase();
            if low.contains("stream") {
                kind = "stream".into();
            } else if low.contains("sink") || low.contains("output") {
                kind = "sink".into();
            } else if low.contains("source") || low.contains("input") {
                kind = "source".into();
            }
            continue;
        }

        let mut s = t.trim_start_matches(BOX);
        let mut default = false;
        if let Some(r) = s.strip_prefix('*') {
            default = true;
            s = r.trim_start_matches(BOX);
        }
        let Some((id_str, rest)) = s.split_once(". ") else {
            continue;
        };
        let id_str = id_str.trim();
        if id_str.is_empty() || !id_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(id) = id_str.parse::<u32>() else {
            continue;
        };

        let raw = rest;
        let muted = raw.contains("[MUTED]");
        let mut name = raw.to_string();
        let mut vol = None;
        if let Some(p) = name.rfind(" vol: ") {
            vol = name[p + 6..]
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok());
            name.truncate(p);
        }
        if let Some(p) = name.find(" [MUTED]") {
            name.truncate(p);
        }
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        let low = name.to_ascii_lowercase();
        nodes.push(Node {
            id,
            bluetooth: low.contains("a2dp") || low.contains("bluez"),
            // WirePlumber names every playback stream "Stream"; in the 0.4
            // layout they live under a plain "Clients:" header.
            kind: if low == "stream" {
                "stream".into()
            } else {
                kind.clone()
            },
            name: name.to_string(),
            vol,
            muted,
            default,
        });
    }
    nodes
}

/// `wpctl inspect <id>` — authoritative metadata (node.name, media.class).
pub async fn inspect(id: u32) -> Result<Value> {
    let out = wpctl(&["inspect", &id.to_string()]).await?;
    serde_json::from_str(&out).map_err(Into::into)
}

/// Pull a property out of an `wpctl inspect` blob, tolerating layout differences.
pub fn prop(v: &Value, key: &str) -> Option<String> {
    v.pointer("/info/props")?
        .get(key)
        .or_else(|| v.pointer("/props")?.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// ALSA cards + selectable profiles, via the PulseAudio compatibility layer that
/// ships with pipewire-pulse. Optional: the UI hides this panel when absent.
pub async fn cards() -> Result<Value> {
    let out = pactl(&["list".into(), "cards".into()]).await?;
    let v: Value = serde_json::from_str(&out)?;
    let mut list = vec![];
    for c in v.as_array().into_iter().flatten() {
        let profiles: Vec<String> = c
            .pointer("/profiles")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        list.push(json!({
            "id": c.get("index").and_then(Value::as_u64),
            "name": c.get("name").and_then(Value::as_str).unwrap_or_default(),
            "profile": c.pointer("/active_profile/name").and_then(Value::as_str).unwrap_or_default(),
            "profiles": profiles,
        }));
    }
    Ok(Value::Array(list))
}

// ---------------------------------------------------------------------------
// filter-chain (EQ + limiter)
// ---------------------------------------------------------------------------

pub const FILTER_NAME: &str = "BluetoothMixerFilter";
pub const FILTER_DESC: &str = "Bluetooth Mixer EQ/Limiter";

/// PipeWire `filter-chain` graph. A high-pass roll-off plus a 4-band tone
/// control and an optional LADSPA brick-wall limiter from `swh-plugins`
/// (`fast_lookahead_limiter_1913`).
pub fn filter_graph(eq: &[f32; 4], limiter: bool) -> Value {
    let mut graph = json!({
        "biquad.bands": [
            { "type": "highpass",  "frequency": 30,   "Q": 0.707 },
            { "type": "lowshelf",  "frequency": 120,  "Q": 0.707, "gain": eq[0] },
            { "type": "peaking",   "frequency": 500,  "Q": 1.0,   "gain": eq[1] },
            { "type": "peaking",   "frequency": 3000, "Q": 1.0,   "gain": eq[2] },
            { "type": "highshelf", "frequency": 8000, "Q": 0.707, "gain": eq[3] }
        ],
        "biquad.gain0": 0
    });
    if limiter {
        graph["ladspa.fast_lookahead_limiter_1913"] = json!({
            "media.class": "ladspa",
            "plugin": "fast_lookahead_limiter_1913",
            "label": "Limiter"
        });
    }
    graph
}

pub async fn create_filter(target_node_name: Option<&str>, graph: &Value) -> Result<u32> {
    let mut props = json!({
        "factory.name": "filter-chain",
        "node.name": FILTER_NAME,
        "node.description": FILTER_DESC,
        "media.class": "Audio/Filter",
        "node.group": "filter",
        "filter.graph": graph.to_string(),
        "monitor.channel-volumes": "true",
        "monitor.volume": "true"
    });
    if let Some(target) = target_node_name {
        props["target.object"] = json!("4294967295"); // SPA_OBJECT_ID_ANY
        props["target.node.name"] = json!(target);
    }
    let raw = create_object(props).await?;
    Ok(raw.parse().unwrap_or(0))
}

pub async fn update_filter(id: u32, graph: &Value) -> Result<()> {
    pwctl(&[
        "set-param".into(),
        id.to_string(),
        "filter.graph".into(),
        graph.to_string(),
    ])
    .await
    .map(|_| ())
}

pub async fn destroy_filter(id: u32) -> Result<()> {
    pwctl(&["destroy".into(), id.to_string()]).await.map(|_| ())
}
#[cfg(test)]
mod tests {
    use super::*;

    const WP4: &str = "\
PipeWire 'pipewire-0' [0.3.65, root@pi, cookie:12345]
 \u{2514}\u{2500} Clients:
 \u{250C}\u{2500} 31. Audio Player
 \u{2502} \u{251C}\u{2500} 33. Stream vol: 0.80
 \u{2502} \u{2514}\u{2500} 34. Screen
 \u{2514}\u{2500} Devices:
   \u{251C}\u{2500} Audio Adapter (Built-in Audio Analog Stereo) [2 Devices]
   \u{2502} \u{251C}\u{2500} Devices:
   \u{2502} \u{2502} \u{2514}\u{2500} Audio Analog Stereo
   \u{2502} \u{2502}   \u{251C}\u{2500} Sinks:
   \u{2502} \u{2502}   \u{2502} \u{2514}\u{2500} *   26. Built-in Audio Analog Stereo vol: 0.40 [MUTED]
   \u{2502} \u{2502}   \u{2514}\u{2500} Sources:
   \u{2502} \u{2502}     \u{2514}\u{2500} *   27. Built-in Audio Analog Stereo.stereo vol: 1.00
   \u{2514}\u{2500} Audio Adapter (2 Ports)
     \u{251C}\u{2500} Sinks:
     \u{2502} \u{2514}\u{2500} 41. Speaker
     \u{2514}\u{2500} Sources:
       \u{2514}\u{2500} 42. Microphone
";

    const WP5: &str = "\
PipeWire 'pipewire-0' [0.3.85, user@pi, cookie:999]
 \u{2514}\u{2500} Stream Outputs:
     \u{2514}\u{2500} 30. Stream Output vol: 0.90
 \u{2514}\u{2500} Stream Inputs:
     \u{2514}\u{2500} 31. Stream Input
 \u{2514}\u{2500} Output Devices:
     \u{251C}\u{2500} *   50. my_BT_Headphones_A2DP_Sink vol: 0.65
     \u{2514}\u{2500} 51. Built-in Audio Analog Stereo vol: 0.40
 \u{2514}\u{2500} Input Devices:
     \u{251C}\u{2500} *   52. Phone_A2DP_Source vol: 0.75
     \u{2514}\u{2500} 53. Built-in Audio Analog Stereo.stereo vol: 1.00
";

    #[test]
    fn parses_wireplumber_04_layout() {
        let n = parse_status(WP4);
        let find = |id: u32| n.iter().find(|x| x.id == id).unwrap();

        assert_eq!(n.len(), 7);
        assert_eq!(find(33).kind, "stream");
        assert_eq!(find(33).vol, Some(0.80));
        assert_eq!(find(26).kind, "sink");
        assert!(find(26).default && find(26).muted && find(26).vol == Some(0.40));
        assert_eq!(find(26).name, "Built-in Audio Analog Stereo");
        assert_eq!(find(27).kind, "source");
        assert_eq!(find(42).kind, "source");
    }

    #[test]
    fn parses_wireplumber_05_layout() {
        let n = parse_status(WP5);
        let find = |id: u32| n.iter().find(|x| x.id == id).unwrap();

        assert_eq!(n.len(), 6);
        assert_eq!(find(50).kind, "sink");
        assert!(find(50).default && find(50).bluetooth);
        assert_eq!(find(50).name, "my_BT_Headphones_A2DP_Sink");
        assert_eq!(find(51).vol, Some(0.40));
        assert_eq!(find(52).kind, "source");
        assert!(find(52).bluetooth);
        assert_eq!(find(30).kind, "stream");
    }

    #[test]
    fn filter_graph_shape() {
        let g = filter_graph(&[1.0, -2.0, 3.0, -4.0], true);
        let bands = &g["biquad.bands"];
        assert_eq!(bands.as_array().unwrap().len(), 5);
        assert_eq!(bands[1]["type"], "lowshelf");
        assert_eq!(bands[4]["gain"], -4.0);
        assert!(g.get("ladspa.fast_lookahead_limiter_1913").is_some());
        assert!(filter_graph(&[0.0; 4], false)
            .get("ladspa.fast_lookahead_limiter_1913")
            .is_none());
    }
}
