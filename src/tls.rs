//! HTTPS side of the server.
//!
//! This exists for one reason: `AudioWorklet` is only exposed in a secure
//! context, and the web output is unusable without it. `ScriptProcessorNode`
//! does work over plain HTTP, but it runs on the *main* thread, where it
//! competes with WebSocket message delivery and page rendering — and it
//! starves, which sounds like the audio cutting out.
//!
//! So we serve TLS too, using a self-signed certificate generated on first
//! run. Browsers will warn once per device; after that the page is a secure
//! context and the worklet path is used automatically.

use anyhow::{Context, Result};
use std::path::PathBuf;

pub const TLS_PORT: u16 = 8421;

fn cert_paths() -> (PathBuf, PathBuf) {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bluetooth-mixer");
    (base.join("cert.pem"), base.join("key.pem"))
}

/// Generate a self-signed certificate if we do not already have one.
///
/// The SAN list matters: browsers ignore the CN entirely, and a certificate
/// that does not cover the address you actually typed is a hard failure, not
/// a warning you can click past.
fn ensure_cert() -> Result<(PathBuf, PathBuf)> {
    let (cert, key) = cert_paths();
    if cert.exists() && key.exists() {
        return Ok((cert, key));
    }
    if let Some(d) = cert.parent() {
        std::fs::create_dir_all(d)?;
    }

    let mut names = vec!["localhost".to_string(), "raspberrypi".to_string()];
    if let Ok(ip) = std::env::var("BLUETOOTH_MIXER_IP") {
        names.push(ip);
    }
    // Best effort: pick up whatever addresses this host has.
    if let Ok(out) = std::process::Command::new("hostname").arg("-I").output() {
        for ip in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if ip.parse::<std::net::IpAddr>().is_ok() {
                names.push(ip.to_string());
            }
        }
    }

    let certified = rcgen::generate_simple_self_signed(names)
        .context("generating a self-signed certificate")?;
    std::fs::write(&cert, certified.cert.pem())?;
    std::fs::write(&key, certified.signing_key.serialize_pem())?;
    Ok((cert, key))
}

pub async fn serve(app: std::sync::Arc<crate::state::App>, port: u16) -> Result<()> {
    let (cert_path, key_path) = ensure_cert()?;
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .context("loading the TLS certificate")?;

    // axum-server wants a blocking listener; handing it a ready-made socket
    // keeps the bind failure visible here instead of inside the server task.
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
    println!("bluetooth-mixer -> https://0.0.0.0:{port} (self-signed)");

    let server = axum_server::from_tcp_rustls(listener, config)
        .context("building the HTTPS listener")?;
    server
        .serve(crate::web::router(app).into_make_service())
        .await?;
    Ok(())
}