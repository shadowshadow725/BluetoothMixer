//! Optional RTP-over-LAN sources, backed by PipeWire's `module-rtp-recv`
//! through the pipewire-pulse compatibility loader. Plain PulseAudio clients on
//! the LAN need no setup at all - they show up as input streams automatically.

use anyhow::{anyhow, Result};

pub async fn add(name: &str, session: u32) -> Result<u32> {
    let out = crate::pw::pactl(&[
        "load-module".into(),
        "module-rtp-recv".into(),
        format!("sink_name=btmixer_{name}"),
        format!("session={session}"),
    ])
    .await?;
    out.split_whitespace()
        .last()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("rtp-recv did not return a module id"))
}

pub async fn remove(id: u32) -> Result<()> {
    crate::pw::pactl(&["unload-module".into(), id.to_string()]).await?;
    Ok(())
}

pub async fn alive(id: u32) -> bool {
    crate::pw::pactl(&["list".into(), "short".into(), "modules".into()])
        .await
        .map(|s| {
            s.lines()
                .any(|l| l.split_whitespace().next() == Some(&id.to_string()))
        })
        .unwrap_or(false)
}
