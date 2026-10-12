# Bluetooth Mixer

A web UI for mixing audio from **many devices into one Bluetooth headset** on a
Raspberry Pi or other single-board computer.

There is almost no bespoke audio code here. **PipeWire + WirePlumber already do
the mixing**: they already turn any A2DP phone, USB interface and LAN client into
nodes, and already mix every input into every output. This project is a thin
control plane written in Rust (~600 lines) that drives the official WirePlumber
CLI and renders a page.

```
 phones ──A2DP──┐                                    ┌──A2DP──▶ 🎧
 USB/line-in ───┼──▶ PipeWire ──▶ filter-chain ───────┤
 LAN clients ───┤   (mix)         (EQ + limiter)     └──▶ 🔊 speakers
 RTP receivers ─┘         ▲
                          └── bluetooth-mixer (Rust) ── web UI on :8420
```

## Features

| | |
|---|---|
| Inputs | Bluetooth A2DP sources, USB / line-in, LAN PulseAudio clients, RTP receivers |
| Mixing | Per-input fader, mute and solo (solo mutes every other input) |
| Output | Pick any sink, master volume, pairing state, one-click connect |
| Tone | 4-band EQ (±12 dB) + LADSPA brick-wall limiter on a PipeWire `filter-chain` |
| Hardware | ALSA card profile switching for USB audio interfaces |
| Footprint | ~1.3 MB binary, ~15 MB RSS, one `wpctl status` poll per 900 ms |

## Install

On Raspberry Pi OS Bookworm/Trixie or any Debian-based distro with PipeWire:

```bash
git clone <this-repo> && cd BluetoothMixer
chmod +x install.sh && ./install.sh
```

Then open `http://<pi-ip>:8420` from any phone or laptop on the same network.

To run manually instead:

```bash
sudo apt install pipewire wireplumber wireplumber-pulse bluez bluez-tools \
                 pulseaudio-utils swh-plugins
cargo build --release
./target/release/bluetooth-mixer --port 8420
```

## Usage

1. **Pair a phone** – *Bluetooth devices* → *Scan* → *Pair* → *Connect*. PipeWire
   creates an `A2DP_Source` node and it appears as an input. Do this once per phone.
2. **Plug in a USB interface / line-in** – it appears as an input automatically.
   Pick the profile in *Hardware inputs* (e.g. `Analog Stereo Duplex`) to get a
   capture stream.
3. **Pair your headphones** and pick them in *Output*.
4. Balance with the per-input faders; `S` solos one input, `M` mutes it.

Everything PipeWire mixes automatically: the EQ/limiter node is inserted on the
selected output, so all inputs pass through it.

### Network / app sources

Two options, both zero-configuration:

* **Any PulseAudio client on the LAN** already works. On the sending machine set
  `PULSE_SERVER=<pi-ip>:4713` (`export PULSE_SERVER=…` or
  `mpv --ao=pulse` / `ffplay -f pulse`) and it shows up as an input here.
* **RTP receivers** (the *Network / app sources* panel) load PipeWire's
  `module-rtp-recv`, so a phone or laptop can push RTP audio straight to the Pi.
  Pair it with `parec | pw-cat`-style senders or `rtpapd`.

## How it works

| File | Role |
|---|---|
| `src/pw.rs` | Wraps `wpctl`, `pwctl`, `pactl`, `pw-cli`; parses `wpctl status` |
| `src/bt.rs` | Wraps `bluetoothctl` |
| `src/state.rs` | 900 ms discovery poll, solo/mute enforcement, filter sync, persistence |
| `src/web.rs` | axum: one HTML page, one WebSocket, JSON protocol |
| `src/static/index.html` | The whole UI — no build step, no framework |

The EQ/limiter is a PipeWire `filter-chain` node created with
`pw-cli create-object` and reconfigured in place with `pwctl set-param
filter.graph '{…}'`. Settings live in
`~/.config/bluetooth-mixer/state.json`.

### HTTP API

`GET /` is the UI, `GET /api/state` is the current state as JSON, and `/ws`
pushes the same document every ~900 ms. Commands travel over the socket:

```json
{"cmd":"vol","id":42,"vol":0.8}      {"cmd":"mute","id":42,"on":true}
{"cmd":"solo","id":42,"on":true}     {"cmd":"output","id":51}
{"cmd":"master","vol":0.7}           {"cmd":"eq","band":2,"gain":-4.5}
{"cmd":"limiter","on":true}          {"cmd":"bt_action","action":"connect","addr":"AA:BB:.."}
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `wpctl: program not found` | PipeWire isn't running for this user: `systemctl --user status pipewire wireplumber` |
| Phone won't appear as an input | Pair **and connect** it; bluez only exposes an A2DP source node while connected. Verify with `pactl list short sources` |
| *EQ/limiter unavailable* in the header | `pw-cli` needs `libpipewire-0.3-modules`; install it, then restart the service |
| Limiter toggle has no effect | LADSPA plugin missing: `sudo apt install swh-plugins` |
| Headphones disappear after switching output | The filter node is rebuilt automatically; wait one poll cycle (~1 s) |
| Nothing loads in the UI | Check `journalctl --user -u bluetooth-mixer -f` and that port 8420 is reachable |

Run `cargo test` to exercise the `wpctl status` parser against both
WirePlumber 0.4 and 0.5 output layouts.