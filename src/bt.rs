//! `bluetoothctl` (bluez) wrapper: discovery, pairing and connection state.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

async fn btc(args: &[String], timeout: u64) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(timeout),
        Command::new("bluetoothctl")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("bluetoothctl timed out"))??;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// Simple `Key: value` dump, used for `show` and `info`.
fn parse_kv(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}

fn yes(kv: &HashMap<String, String>, key: &str) -> bool {
    matches!(
        kv.get(key).map(String::as_str),
        Some("yes") | Some("true") | Some("powered")
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct Adapter {
    pub mac: String,
    pub name: String,
    pub powered: bool,
    pub pairable: bool,
    pub discoverable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub addr: String,
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// "sink" (headphones/speaker) or "source" (a phone streaming audio to us).
    pub role: String,
}

pub async fn adapter() -> Result<Adapter> {
    let list = btc(&["list".into()], 6).await.unwrap_or_default();
    let mac = list
        .lines()
        .find_map(|l| l.strip_prefix("Controller "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_default()
        .to_string();

    let kv = parse_kv(&btc(&["show".into()], 6).await.unwrap_or_default());
    Ok(Adapter {
        mac,
        name: kv.get("name").cloned().unwrap_or_default(),
        powered: yes(&kv, "powered"),
        pairable: yes(&kv, "pairable"),
        discoverable: yes(&kv, "discoverable"),
    })
}

pub async fn devices() -> Result<Vec<Device>> {
    // No `--timeout` here: it makes bluetoothctl sit and wait, which is only
    // wanted for `scan`. Plain `devices` returns immediately and would
    // otherwise stall the whole poll loop for its full timeout.
    let out = btc(&["devices".into()], 6).await?;
    let addrs: Vec<String> = out
        .lines()
        .filter_map(|l| l.strip_prefix("Device "))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect();

    let mut list = Vec::new();
    for addr in addrs {
        list.push(device_info(&addr).await);
    }
    Ok(list)
}

pub async fn device_info(addr: &str) -> Device {
    let raw = btc(&["info".into(), addr.into()], 6)
        .await
        .unwrap_or_default();
    let kv = parse_kv(&raw);

    // A device lists a dozen UUIDs and they all share one key, so the map
    // above only ever holds the last one. Scan the raw output instead, or a
    // phone's "Audio Source" gets overwritten by whatever it lists last.
    let mut role = String::new();
    for line in raw.lines() {
        let Some(uuid) = line.trim_start().strip_prefix("UUID:") else {
            continue;
        };
        let uuid = uuid.to_ascii_lowercase();
        if uuid.contains("source") {
            role = "source".into();
        } else if uuid.contains("sink") && role.is_empty() {
            role = "sink".into();
        }
    }
    Device {
        name: kv
            .get("alias")
            .or_else(|| kv.get("name"))
            .cloned()
            .unwrap_or_else(|| addr.to_string()),
        paired: yes(&kv, "paired"),
        trusted: yes(&kv, "trusted"),
        connected: yes(&kv, "connected"),
        role,
        addr: addr.to_string(),
    }
}

pub async fn act(action: &str, addr: &str) -> String {
    // Before pairing, the adapter has to be discoverable or the handset cannot
    // find us in the first place.
    if action == "pair" && !addr.is_empty() {
        let _ = btc(&["discoverable".into(), "on".into()], 6).await;
    }
    let (args, timeout): (Vec<String>, u64) = match action {
        "scan" => (
            vec!["--timeout".into(), "10".into(), "scan".into(), "on".into()],
            16,
        ),
        // `bluetoothctl power` is an error without an explicit on/off argument.
        "power on" => (vec!["power".into(), "on".into()], 8),
        "power off" => (vec!["power".into(), "off".into()], 8),
        // No `--agent` here on purpose. Our own agent (src/agent.rs) is already
        // registered as the system default and confirms the passkey; letting
        // bluetoothctl register a second one just makes it reject on our
        // behalf. The long leash is for the user tapping "accept" on the
        // handset.
        "pair" if !addr.is_empty() => (vec!["pair".into(), addr.into()], 45),
        a if addr.is_empty() => (vec![a.to_string()], 8),
        a => (vec![a.to_string(), addr.to_string()], 12),
    };
    btc(&args, timeout).await.unwrap_or_else(|e| e.to_string())
}
