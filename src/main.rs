//! bluetooth-mixer - a web-controlled Bluetooth audio mixer.
//!
//! PipeWire + WirePlumber do all the audio work (A2DP, USB/line-in, the actual
//! mixing and the filter chain). This process is only a control plane: it polls
//! the WirePlumber CLI, exposes it over HTTP/WebSocket and drives it back.

mod bt;
mod net;
mod pw;
mod state;
mod web;

use anyhow::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut port: u16 = 8420;
    let mut host = "0.0.0.0".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" | "-p" => port = args.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--host" => host = args.next().unwrap_or(host),
            "--help" | "-h" => {
                println!("bluetooth-mixer [--host ADDR] [--port PORT]   (default 0.0.0.0:8420)");
                return Ok(());
            }
            _ => {}
        }
    }

    let app = Arc::new(state::App::load());
    tokio::spawn(app.clone().run());

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    println!("bluetooth-mixer -> http://{host}:{port}");
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
