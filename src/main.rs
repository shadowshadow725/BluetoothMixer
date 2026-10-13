//! bluetooth-mixer - a web-controlled Bluetooth audio mixer.
//!
//! PipeWire + WirePlumber do all the audio work (A2DP, USB/line-in, the actual
//! mixing and the filter chain). This process is only a control plane: it polls
//! the WirePlumber CLI, exposes it over HTTP/WebSocket and drives it back.

mod agent;
mod bt;
mod net;
mod pw;
mod state;
mod tls;
mod web;
mod webaudio;

use anyhow::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut port: u16 = 8420;
    let mut tls_port: u16 = tls::TLS_PORT;
    let mut tls = true;
    let mut host = "0.0.0.0".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" | "-p" => port = args.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--tls-port" => tls_port = args.next().and_then(|s| s.parse().ok()).unwrap_or(tls_port),
            "--no-tls" => tls = false,
            "--host" => host = args.next().unwrap_or(host),
            "--help" | "-h" => {
                println!(
                    "bluetooth-mixer [--host ADDR] [--port PORT] [--tls-port PORT] [--no-tls]\n\
                     \n  HTTP  default 0.0.0.0:8420\n  HTTPS default 0.0.0.0:8421\n\
                     \n  Use https:// for web output: AudioWorklet needs a secure context."
                );
                return Ok(());
            }
            _ => {}
        }
    }

    // rustls 0.23 has no implicit provider; install one or every handshake
    // fails.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        eprintln!("warning: a TLS crypto provider was already installed");
    }

    let app = Arc::new(state::App::load());
    tokio::spawn(app.clone().run());

    // Without a registered agent BlueZ cannot complete pairing at all, so do
    // this up front rather than waiting for someone to click Pair.
    match agent::register().await {
        Ok(()) => println!("bluetooth agent registered"),
        Err(e) => eprintln!("warning: bluetooth agent not registered: {e}"),
    }

    // The audio UI wants AudioWorklet, which browsers only expose over HTTPS.
    // This gets its own thread and runtime on purpose: driven from the shared
    // one it wedges the accept loop, which takes the HTTP listener down too.
    if tls {
        let app2 = app.clone();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Runtime::new() else {
                eprintln!("warning: could not create a runtime for HTTPS");
                return;
            };
            if let Err(e) = rt.block_on(tls::serve(app2, tls_port)) {
                eprintln!("warning: https listener failed: {e:#}");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    println!("bluetooth-mixer -> http://{host}:{port}");
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
