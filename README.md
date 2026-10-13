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
 USB/line-in ───┼──▶ PipeWire ──▶ master × gain ──────┤
 LAN clients ───┤   (mix)         (sink volume)       ├──▶ 🔊 speakers
 RTP receivers ─┘         ▲                          │
              ┌───────────┴───────────┐              │
              │  null sink + monitor ├──▶ libpipewire ──▶ 🔊 the browser tab
              └───────────────────────┘   capture        PCM over the same
                                               stream       WebSocket
                          ▲
                          └── bluetooth-mixer (Rust) ── web UI on :8420
```

## Features

| | |
|---|---|
| Inputs | Bluetooth A2DP sources, USB / line-in, LAN PulseAudio clients, RTP receivers |
| Mixing | Per-input fader, mute and solo (solo mutes every other input) |
| Output | Pick any sink, master volume, output gain (0–400%), pairing state, one-click connect |
| Web output | Send the mix to the browser itself instead of a speaker/headset, with a live level meter |
| Hardware | ALSA card profile switching for USB audio interfaces |
| Footprint | ~1.2 MB binary, ~15 MB RSS, one `wpctl status` poll per 900 ms |

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
                 pulseaudio-utils
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

Everything PipeWire mixes automatically, and the selected output sink is the single
point where master and gain are applied.

### Network / app sources

Two options, both zero-configuration:

* **Any PulseAudio client on the LAN** already works. On the sending machine set
  `PULSE_SERVER=<pi-ip>:4713` (`export PULSE_SERVER=…` or
  `mpv --ao=pulse` / `ffplay -f pulse`) and it shows up as an input here.
* **RTP receivers** (the *Network / app sources* panel) load PipeWire's
  `module-rtp-recv`, so a phone or laptop can push RTP audio straight to the Pi.
  Pair it with `parec | pw-cat`-style senders or `rtpapd`.

### Pairing

Pairing only works because this process registers itself as a BlueZ pairing
agent. BlueZ asks the default agent to confirm the passkey the handset
displays; with nothing registered the attempt dies with
`AuthenticationCanceled` and the phone just says "cannot connect".
`bluetoothctl --agent` does not help — its KeyboardDisplay agent prompts on a
terminal and silently rejects when there is no TTY, which is exactly the
situation for a service. So `src/agent.rs` speaks `org.bluez.Agent1` itself
and confirms automatically.

**That gives up the protection the passkey exists for**: anyone nearby can
pair with the Pi without a human confirming the digits. It is the price of
pairing from a web UI. If that trade is unacceptable, drop the auto-confirm in
`request_confirmation` / `display_passkey` and surface the code in the UI
instead.

Pair from the handset, not from this page: tap the Pi in your phone's
Bluetooth settings and accept the code. The *Pair* button works too, but most
phones only run the secure-simple-pairing exchange as the initiator.

## How it works

| File | Role |
|---|---|
| `src/pw.rs` | Wraps `wpctl` and `pactl`; parses `wpctl status`; volume-scale conversion |
| `src/webaudio.rs` | Null sink + native libpipewire capture, broadcast to every browser |
| `src/agent.rs` | BlueZ `org.bluez.Agent1` pairing agent — without it, pairing cannot complete |
| `src/bt.rs` | Wraps `bluetoothctl` |
| `src/state.rs` | 900 ms discovery poll, solo/mute enforcement, master/gain, persistence |
| `src/web.rs` | axum: one HTML page, one WebSocket, JSON protocol |
| `src/static/index.html` | The whole UI — no build step, no framework |

The output level is the product of two controls: **Master** (0–100%) and
**Gain** (0–400%). The poll loop drives the selected sink to `master x gain`,
so gain above 100% pushes past unity — there is no brick-wall limiter behind
it, so expect clipping on loud material. Settings live in
`~/.config/bluetooth-mixer/state.json`.

PipeWire stores volumes on a cubic scale, so everything here is specified in
linear amplitude and converted at the `wpctl` boundary (`pw::to_pw`). That is
what keeps a slider reading 50% actually half as loud instead of -18 dB.

### Web output

**Use `https://<pi-ip>:8421`, not `http://…:8420`.** The certificate is
self-signed, so a browser warns once per device — accept it. This matters:
browsers only expose `AudioWorklet` in a secure context, and `AudioWorklet`
runs playback on the audio thread. The `http://` port still works, but falls
back to `ScriptProcessorNode`, which runs on the **main thread** where it
competes with WebSocket message delivery and page rendering. It starves under
load, and a starved ring buffer is audible as the audio cutting out.

`--no-tls` disables the HTTPS listener; `--tls-port PORT` moves it.

HTTP on 8420 is still the right address for everything else.


*Route to web* switches the mix onto a PulseAudio null sink and makes it the
system default, so nothing comes out of the speakers or headset. The sink's
monitor ports carry exactly the mixed signal. A native libpipewire capture
stream (`btmixer-web-capture`, on its own thread) attaches to those ports and
hands the server raw s16le PCM, which goes out as binary WebSocket frames
alongside the JSON state pushes. On the page an AudioWorklet ring buffer
feeds the audio graph, and the level meter is computed from the same samples.
The status line shows the live buffer depth, plus `underruns` and `ms dropped`
counters if either ever becomes non-zero — if you hear crackle, read those. Toggling the mode off unloads the null sink
and puts your previous output back.

There is no DSP of our own: PipeWire mixes and attenuates, this project only
drives its CLI.

### HTTP API

`GET /` is the UI, `GET /api/state` is the current state as JSON, and `/ws`
pushes the same document every ~900 ms. Commands travel over the socket as
JSON text; captured audio arrives on the same socket as binary frames:

```json
{"cmd":"vol","id":42,"vol":0.8}      {"cmd":"mute","id":42,"on":true}
{"cmd":"solo","id":42,"on":true}     {"cmd":"output","id":51}
{"cmd":"master","vol":0.7}           {"cmd":"gain","vol":1.25}
{"cmd":"web","on":true}              {"cmd":"bt_action","action":"connect","addr":"AA:BB:.."}
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `wpctl: program not found` | PipeWire isn't running for this user: `systemctl --user status pipewire wireplumber` |
| Phone won't appear as an input | Pair **and connect** it; bluez only exposes an A2DP source node while connected. Verify with `pactl list short sources` |
| Nothing loads in the UI | Check `journalctl --user -u bluetooth-mixer -f` and that port 8420 (or 8421) is reachable |
| Phone says "cannot connect" | No agent is registered: look for `bluetooth agent registered` in the service log |
| Pairing hangs | The handset never showed its code. Pair from the phone's own Bluetooth settings |
| `rustc is not supported by ... zbus` | zbus 5 needs rustc 1.87+; use rustup rather than the distro cargo |
| Web output drops out or crackles | Use `https://…:8421`, not `http://…:8420` — the HTTP port cannot use AudioWorklet |
| "This browser" refuses to play | Open the HTTPS URL and accept the self-signed certificate warning |
| Web output stuck on "buffering…" | The capture cannot reach the null sink: check `pactl list short sinks` shows `btmixer_web` and `wpctl status` lists a `btmixer-web-capture` stream |
| Web output silent but meter moving | The browser tab is muted or the system output is muted |
| Web output choppy | Too much Wi-Fi congestion: 48 kHz stereo PCM is ~1.5 Mbit/s sustained |

Run `cargo test` to exercise the `wpctl status` parser against the PipeWire
0.3 (WirePlumber 0.4/0.5) and PipeWire 1.x output layouts.