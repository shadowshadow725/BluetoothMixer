//! Thin wrappers around the audio stack CLIs that ship with PipeWire/WirePlumber.
//! No audio code lives here: PipeWire does the mixing, we only drive its CLI.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

/// Upper bound for the output gain, as a linear amplitude multiplier.
pub const MAX_GAIN: f32 = 4.0;

/// PipeWire keeps volumes on a cubic scale: the number `wpctl` shows and
/// accepts is the cube root of the real amplitude, so 0.5 is -18 dB rather
/// than -6 dB. Every control in this app is specified in linear amplitude and
/// converted at this boundary, so a slider reading 50% really is half as loud.
pub fn to_pw(linear: f32) -> f32 {
    linear.clamp(0.0, MAX_GAIN).cbrt()
}

/// Inverse of [`to_pw`], for reporting a node's real level back to the UI.
pub fn from_pw(cubic: f32) -> f32 {
    cubic.powi(3)
}

pub async fn set_volume(id: u32, linear: f32) -> Result<()> {
    wpctl(&[
        "set-volume",
        &id.to_string(),
        &format!("{:.4}", to_pw(linear)),
    ])
    .await
    .map(|_| ())
}

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

pub async fn pactl(args: &[String]) -> Result<String> {
    run("pactl", args, 8).await
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

/// Read a node's current level in linear amplitude. `wpctl status` omits the
/// volume for some source nodes, so this is the fallback when a fader has to
/// be derived from the graph itself.
pub async fn get_volume(id: u32) -> Option<f32> {
    let out = wpctl(&["get-volume", &id.to_string()]).await.ok()?;
    let token = out.split_whitespace().nth(1)?;
    token.parse::<f32>().ok().map(from_pw)
}

// ---------------------------------------------------------------------------
// Per-source panning
// ---------------------------------------------------------------------------

/// A node as the PulseAudio compatibility layer sees it. `wpctl` and
/// `pactl` use different ids for the same object, so the map is keyed by the
/// PipeWire object id that `wpctl status` prints; that is what the UI sends
/// back in commands.
#[derive(Debug, Clone)]
pub struct PulseNode {
    /// Pulse index used by `pactl set-sink-input-volume` / `set-source-volume`.
    pub index: u32,
    /// `true` for an `Audio/Source` (A2DP input, mic, virtual source).
    pub source: bool,
    /// Channel names in wire order (`front-left`, `front-right`, …).
    pub channels: Vec<String>,
    /// Loudest channel in linear amplitude. This is the level the fader
    /// represents: panning only attenuates the far side, so the near channel
    /// (the peak) stays at the user's chosen level. Needed for playback
    /// streams because `wpctl get-volume` reports 0 for them.
    pub peak: Option<f32>,
}

impl PulseNode {
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }
}

/// List playback streams and sources, keyed by PipeWire node id.
///
/// Only the Pulse compatibility layer can set individual channel volumes
/// without doing raw PipeWire param round-trips. Nodes that are not exposed
/// there are absent from the map; callers fall back to `wpctl`.
pub async fn pulse_nodes() -> Result<HashMap<u32, PulseNode>> {
    let mut map = HashMap::new();
    // Sink inputs first, sources second: a virtual source may also have an
    // internal playback stream, and its source volume is the one that feeds
    // the graph.
    for (list, source) in [("sink-inputs", false), ("sources", true)] {
        let out = pactl(&[
            "-f".into(),
            "json".into(),
            "list".into(),
            list.into(),
        ])
        .await?;
        let v: Value = serde_json::from_str(&out)?;
        for item in v.as_array().into_iter().flatten() {
            let Some(index) = item.get("index").and_then(Value::as_u64) else {
                continue;
            };
            let Some(id) = item
                .pointer("/properties/object.id")
                .and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .or_else(|| v.as_u64().map(|n| n as u32))
                })
            else {
                continue;
            };
            let channels = item
                .get("channel_map")
                .and_then(Value::as_str)
                .map(parse_channel_map)
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    item.get("sample_specification")
                        .and_then(Value::as_str)
                        .and_then(parse_spec_channels)
                })
                .unwrap_or_default();
            map.insert(
                id,
                PulseNode {
                    index: index as u32,
                    source,
                    channels,
                    peak: parse_peak_volume(item),
                },
            );
        }
    }
    Ok(map)
}

/// Highest channel level from a `pactl -f json` object. Pulse volume values
/// are cubic like `wpctl`, with 65536 = unity, so they convert with
/// [`from_pw`] after normalising.
fn parse_peak_volume(item: &Value) -> Option<f32> {
    item.get("volume")
        .and_then(Value::as_object)?
        .values()
        .filter_map(|v| v.get("value").and_then(Value::as_f64))
        .map(|raw| from_pw((raw / 65536.0) as f32))
        .reduce(f32::max)
}

fn parse_channel_map(s: &str) -> Vec<String> {
    s.split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Fallback for older `pactl` output that has no `channel_map`: read the
/// channel count out of `s16le 2ch 48000Hz`.
fn parse_spec_channels(s: &str) -> Option<Vec<String>> {
    let n = s
        .split_whitespace()
        .find_map(|w| w.strip_suffix("ch").and_then(|d| d.parse::<usize>().ok()))?;
    Some(match n {
        0 => vec![],
        1 => vec!["mono".into()],
        2 => vec!["front-left".into(), "front-right".into()],
        _ => (0..n).map(|i| format!("ch{i}")).collect(),
    })
}

/// Set a node's channel levels in linear amplitude. Stereo (and the front
/// pair of a multichannel node) is panned; every other channel stays at
/// `base`, so a source never loses channels. Returns whether the pan was
/// actually applied (false for mono or nodes invisible to PulseAudio).
pub async fn set_node_gains(
    id: u32,
    left: f32,
    right: f32,
    base: f32,
    nodes: &HashMap<u32, PulseNode>,
) -> Result<bool> {
    let Some(node) = nodes.get(&id) else {
        set_volume(id, base).await?;
        return Ok(false);
    };
    if node.channel_count() < 2 {
        set_volume(id, base).await?;
        return Ok(false);
    }

    let mut args = vec![
        if node.source {
            "set-source-volume"
        } else {
            "set-sink-input-volume"
        }
        .to_string(),
        node.index.to_string(),
    ];
    args.extend(node.channels.iter().map(|c| {
        let v = match c.as_str() {
            "front-left" | "fl" | "left" => left,
            "front-right" | "fr" | "right" => right,
            _ => base,
        };
        format!("{:.4}", v.clamp(0.0, 10.0))
    }));

    if pactl(&args).await.is_ok() {
        return Ok(true);
    }
    // Keep the fader working even if the per-channel call is rejected.
    set_volume(id, base).await?;
    Ok(false)
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

/// Splits a node description into (name, volume, muted).
///
/// PipeWire 0.3 printed `Name vol: 0.40 [MUTED]`; 1.0 moved everything into one
/// trailing bracket: `Name  [vol: 0.33 MUTED]`, or just a scope tag such as
/// `[alsa]` / `[v4l2]`. Both have to be stripped so the name stays stable — it
/// is the key used for the persisted output selection and mute map.
fn split_status(rest: &str) -> (String, Option<f64>, bool) {
    let muted = rest.contains("MUTED");
    let mut name = rest.trim().to_string();
    let mut vol = None;

    if let Some(open) = name.rfind('[') {
        if name.ends_with(']') {
            let inner = name[open + 1..name.len() - 1].trim().to_string();
            name.truncate(open);
            if let Some(v) = inner.strip_prefix("vol:") {
                vol = v.split_whitespace().next().and_then(|s| s.parse().ok());
            }
        }
    }

    if vol.is_none() {
        if let Some(p) = name.rfind(" vol: ") {
            vol = name[p + 6..]
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok());
            name.truncate(p);
        }
    }

    let name = name.replace(" [MUTED]", "");
    (name.trim().to_string(), vol, muted)
}

/// PipeWire version from the `wpctl status` banner, e.g.
/// `PipeWire 'pipewire-0' [1.4.2, andy@pi, cookie:2845416046]` -> `1.4.2`.
pub fn parse_version(out: &str) -> Option<String> {
    let line = out.lines().next()?;
    let open = line.find('[')? + 1;
    let rest = &line[open..];
    let end = rest.find(',').unwrap_or(rest.len());
    let v = rest[..end].trim();
    (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')).then(|| v.to_string())
}

/// Parses `wpctl status` without pulling in a regex crate. The tree drawing
/// characters are stripped, the enclosing section keyword decides the node kind.
pub fn parse_status(out: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut kind = String::from("other");
    // PipeWire >= 1.0 groups nodes under top-level `Audio` / `Video` / …
    // headings. Only Audio matters; Video would otherwise leak camera and
    // video-codec nodes into the mixer as phantom inputs.
    let mut audio = true;

    for line in out.lines() {
        // A top-level heading is a bare word in column 0, with no tree drawing
        // characters in front of it and no trailing colon.
        if !line.trim().is_empty()
            && !line.starts_with([' ', '\u{2502}', '\u{251C}', '\u{2514}'])
            && !line.ends_with(':')
            && !line.contains(|c: char| c.is_whitespace() || !c.is_ascii_alphanumeric())
        {
            audio = line.trim() == "Audio";
            kind = "other".into();
            continue;
        }

        let t = line.trim();

        // Section headers look like "Sinks:", "Stream Outputs:", "Input Devices:".
        if t.ends_with(':') {
            let low = t.to_ascii_lowercase();
            kind = if low.contains("stream") {
                "stream".into()
            } else if low.contains("sink") || low.contains("output") {
                "sink".into()
            } else if low.contains("source") || low.contains("input") {
                "source".into()
            } else {
                // "Clients:", "Devices:", "Filters:" … must not inherit the
                // previous section's kind.
                "other".into()
            };
            if !audio {
                kind = "other".into();
            }
            continue;
        }
        if !audio {
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

        // `wpctl status` lists each stream followed by its link endpoints,
        // e.g. "output_FL > alsa_output...:playback_FL". Those are graph
        // internals, not audio streams, and muting one can sever the path
        // (our own capture stream sits in this list too).
        if rest.contains(" > ")
            || rest.contains(" < ")
            || rest.starts_with(crate::webaudio::NODE_NAME)
            || rest.starts_with(crate::webaudio::APP_NAME)
        {
            continue;
        }

        let (name, vol, muted) = split_status(rest);
        // Bare port lines belonging to a stream ("monitor_FR", "input_FL")
        // carry no arrows, but they are not mixer inputs either.
        if name.split('_').next().is_some_and(|d| {
            matches!(d, "input" | "output" | "monitor")
        }) {
            continue;
        }
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
            name,
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

/// Pull a property out of a `wpctl inspect` blob, tolerating layout differences.
pub fn prop(v: &Value, key: &str) -> Option<String> {
    v.pointer("/info/props")?
        .get(key)
        .or_else(|| v.pointer("/props")?.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// ALSA cards + selectable profiles, via the PulseAudio compatibility layer that
/// ships with pipewire-pulse. Optional: the UI hides this panel when absent.
///
/// `pactl` only emits JSON when asked with `-f json`; the default output is
/// human-readable text. In JSON the profile list is an object keyed by profile
/// name and `active_profile` is a bare string, not `{"name": …}`.
pub async fn cards() -> Result<Value> {
    let out = pactl(&["-f".into(), "json".into(), "list".into(), "cards".into()]).await?;
    let v: Value = serde_json::from_str(&out)?;
    let mut list = vec![];
    for c in v.as_array().into_iter().flatten() {
        let mut profiles: Vec<String> = c
            .get("profiles")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        // Keep the useless "off" entry at the bottom of the dropdown.
        profiles.sort_by(|a, b| (a == "off").cmp(&(b == "off")).then_with(|| a.cmp(b)));
        list.push(json!({
            "id": c.get("index").and_then(Value::as_u64),
            "name": c.get("name").and_then(Value::as_str).unwrap_or_default(),
            "profile": c.get("active_profile").and_then(Value::as_str).unwrap_or_default(),
            "profiles": profiles,
        }));
    }
    Ok(Value::Array(list))
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

    /// Verbatim `wpctl status` from Raspberry Pi OS Trixie: PipeWire 1.4.2 with
    /// WirePlumber 0.5.8. PipeWire 1.0 flattened the tree into `Audio` /
    /// `Video` top-level sections and moved volume into a trailing `[…]` block.
    /// Verbatim PipeWire 1.4 output with a Bluetooth stream active. The
    /// indented "output_FL > sink:playback_FL" lines are graph links, not
    /// audio, and must not show up as mixer inputs.
    const PWA2DP: &str = "\
PipeWire 'pipewire-0' [1.4.2, andy@raspberrypi, cookie:1]
Audio
 ├─ Devices:
 │      48. Built-in Audio                      [alsa]
 │     107. 6217010097's Redmi K50 Pro          [bluez5]
 │
 ├─ Sinks:
 │  *  105. WebOutput                           [vol: 0.40]
 │
 └─ Streams:
       111. bluez_input.04_10_6B_FA_5A_D7.2
              90. output_FL       > WebOutput:playback_FL	[active]
             116. output_FR       > WebOutput:playback_FR	[active]
       112. btmixer-web-capture
             105. monitor_FR
             101. input_FR        < WebOutput:monitor_FR	[active]
             108. monitor_FL
              99. input_FL        < WebOutput:monitor_FL	[active]
       113. bluetooth-mixer
             106. monitor_FR
             102. input_FR        < WebOutput:monitor_FR	[active]
";
    const PW10: &str = "\
PipeWire 'pipewire-0' [1.4.2, andy@raspberrypi, cookie:2845416046]
 └─ Clients:
        33. WirePlumber                         [1.4.2, andy@raspberrypi, pid:994]

Audio
 ├─ Devices:
 │      48. Built-in Audio                      [alsa]
 │
 ├─ Sinks:
 │  *   57. Built-in Audio Stereo               [vol: 0.33 MUTED]
 │
 ├─ Sources:
 │      71. Built-in Audio Stereo              [vol: 1.00]
 │
 ├─ Filters:
 │
 └─ Streams:

Video
 ├─ Devices:
 │      35. rpi-hevc-dec                        [v4l2]
 │
 ├─ Sinks:
 │  *   72. bcm2835-isp                         [v4l2]
 │
 ├─ Sources:
 │  *   71. bcm2835-isp (V4L2)
 │
 ├─ Filters:
 │
 └─ Streams:

Settings
 └─ Default Configured Devices:
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
    fn parses_pipewire_10_layout() {
        let n = parse_status(PW10);
        let find = |id: u32| n.iter().find(|x| x.id == id).unwrap();

        assert_eq!(find(57).kind, "sink");
        assert!(find(57).default && find(57).muted);
        assert_eq!(find(57).vol, Some(0.33));
        assert_eq!(find(57).name, "Built-in Audio Stereo");
        assert_eq!(find(71).kind, "source");
        assert_eq!(find(71).vol, Some(1.00));
        assert!(!find(71).muted);

        // Video nodes must never reach the mixer, and ALSA devices are not
        // sinks or sources either.
        assert!(n.iter().all(|x| x.kind != "video"));
        assert_eq!(find(48).kind, "other");
        for id in [35, 72] {
            assert!(n.iter().find(|x| x.id == id).is_none(), "video node leaked");
        }

        // Only the two audio sink/source lines are sink/source nodes.
        assert_eq!(n.iter().filter(|x| x.kind == "sink").count(), 1);
        assert_eq!(n.iter().filter(|x| x.kind == "source").count(), 1);
    }

    /// Only the real A2DP stream is a mixer input; graph link endpoints and
    /// our own capture process are not, and muting those severs the audio path.
    #[test]
    fn ignores_graph_links_and_own_capture() {
        let n = parse_status(PWA2DP);
        let streams: Vec<&str> = n.iter().filter(|x| x.kind == "stream").map(|x| x.name.as_str()).collect();
        assert_eq!(streams, vec!["bluez_input.04_10_6B_FA_5A_D7.2"],
                   "only the real playback stream should be an input");
        assert!(n.iter().all(|x| !x.name.contains('>') && !x.name.contains('<')));
        assert!(!n.iter().any(|x| x.name.starts_with(crate::webaudio::NODE_NAME)));
        assert!(!n.iter().any(|x| x.name.starts_with(crate::webaudio::APP_NAME)));
    }

    #[test]
    fn parses_peak_volume_from_pulse_json() {
        let item = serde_json::json!({
            "volume": {
                "front-left": {"value": 0},
                "front-right": {"value": 60838}
            }
        });
        let peak = parse_peak_volume(&item).unwrap();
        assert!((peak - 0.8).abs() < 0.001, "peak was {peak}");
        assert_eq!(parse_peak_volume(&serde_json::json!({})), None);
        assert_eq!(
            parse_peak_volume(&serde_json::json!({"volume": {}})),
            None
        );
    }

    #[test]
    fn parses_channel_maps_for_panning() {
        assert_eq!(
            parse_channel_map("front-left,front-right"),
            vec!["front-left", "front-right"]
        );
        assert_eq!(
            parse_spec_channels("s16le 2ch 48000Hz"),
            Some(vec!["front-left".into(), "front-right".into()])
        );
        assert_eq!(
            parse_spec_channels("float32le 1ch 48000Hz"),
            Some(vec!["mono".into()])
        );
        assert_eq!(parse_spec_channels("s16le 48000Hz"), None);
    }

    #[test]
    fn extracts_version_from_banner() {
        assert_eq!(parse_version(PW10).as_deref(), Some("1.4.2"));
        assert_eq!(parse_version(WP5).as_deref(), Some("0.3.85"));
        assert_eq!(parse_version(WP4).as_deref(), Some("0.3.65"));
        assert_eq!(parse_version("no banner here"), None);
    }

    /// The persisted output name must stay stable across volume and mute
    /// changes, otherwise the selection can never be found again.
    #[test]
    fn node_name_is_stable_across_volume_and_mute() {
        let mut n = parse_status(PW10);
        let before = n.iter().find(|x| x.id == 57).unwrap().name.clone();
        let bumped = PW10.replace("[vol: 0.33 MUTED]", "[vol: 0.90]");
        n = parse_status(&bumped);
        assert_eq!(n.iter().find(|x| x.id == 57).unwrap().name, before);
    }

}
