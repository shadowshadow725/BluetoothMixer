//! A BlueZ pairing agent.
//!
//! Pairing cannot work without one: BlueZ asks whoever is registered as the
//! default agent to confirm the passkey the other device displays, and if
//! nothing answers the attempt dies with `AuthenticationCanceled` — or, from
//! the phone's side, "cannot connect". `bluetoothctl --agent` is no help,
//! because its KeyboardDisplay agent prompts on a terminal and silently
//! rejects when there is no TTY, which is exactly the situation here.
//!
//! So we speak `org.bluez.Agent1` ourselves and confirm automatically. The
//! trade-off is real and worth stating plainly: the passkey check exists to
//! stop someone nearby pairing in your name, and auto-confirming gives that
//! up. The alternative is a human at a keyboard, which a web UI on a phone
//! is not.

use anyhow::{anyhow, Context, Result};
use std::sync::OnceLock;
use zbus::interface;
use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

pub const AGENT_PATH: &str = "/org/bluetooth/bluetooth_mixer_agent";
/// KeyboardDisplay is required to receive `DisplayPasskey`, which is how a
/// modern handset asks us to confirm the six digits it is showing.
const CAPABILITY: &str = "KeyboardDisplay";

struct MixerAgent {
    /// Answer for a peer that asks *us* for a passkey. Random per pairing so
    /// the numeric comparison the peer runs is meaningful.
    passkey: u32,
}

impl MixerAgent {
    fn new() -> Self {
        Self { passkey: random_passkey() }
    }
}

/// A six-digit passkey, without pulling in an RNG crate for one number.
fn random_passkey() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    ((t ^ (std::process::id() as u128).wrapping_mul(0x9E37_79B9)) % 1_000_000) as u32
}

#[interface(name = "org.bluez.Agent1")]
impl MixerAgent {
    async fn release(&self) {
        eprintln!("bluetooth agent: released");
    }

    async fn request_pin_code(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<String> {
        eprintln!("bluetooth agent: PIN requested -> answering 0000");
        Ok("0000".into())
    }

    async fn display_pin_code(&self, _device: OwnedObjectPath, pincode: u16) -> zbus::fdo::Result<()> {
        eprintln!("bluetooth agent: PIN {pincode} displayed");
        Ok(())
    }

    async fn request_passkey(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<u32> {
        eprintln!("bluetooth agent: passkey requested -> answering {}", self.passkey);
        Ok(self.passkey)
    }

    async fn display_passkey(
        &self,
        _device: OwnedObjectPath,
        passkey: u32,
        _entered: u16,
    ) -> zbus::fdo::Result<()> {
        eprintln!("bluetooth agent: passkey {passkey} shown on the device -> confirming");
        Ok(())
    }

    async fn request_confirmation(
        &self,
        _device: OwnedObjectPath,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        eprintln!("bluetooth agent: confirm passkey {passkey} -> accepted");
        Ok(())
    }

    async fn request_authorization(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<()> {
        eprintln!("bluetooth agent: authorization requested -> accepted");
        Ok(())
    }

    async fn cancel(&self) {
        eprintln!("bluetooth agent: request cancelled by the device");
    }

    async fn authorize(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

/// Register the agent and make it the default. BlueZ may not be up yet when
/// we start, so retry until it answers.
pub async fn register() -> Result<()> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=30 {
        match try_register().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("bluetooth agent: attempt {attempt} failed: {e:#}");
                last = Some(e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(last.unwrap_or_else(|| anyhow!("bluetooth agent registration failed")))
}

/// The exported `org.bluez.Agent1` object lives on the D-Bus connection, so
/// that connection has to outlive the call that registers it.
static AGENT_CONN: OnceLock<Connection> = OnceLock::new();

async fn try_register() -> Result<()> {
    let conn = Connection::system()
        .await
        .context("connecting to the system bus")?;
    let path = OwnedObjectPath::try_from(AGENT_PATH).context("agent path")?;
    let mgr = ("org.bluez", "/org/bluez", Some("org.bluez.AgentManager1"));

    conn.object_server()
        .at(AGENT_PATH, MixerAgent::new())
        .await
        .context("exporting org.bluez.Agent1")?;

    conn.call_method(
        Some(mgr.0),
        mgr.1,
        mgr.2,
        "RegisterAgent",
        &(path.clone(), CAPABILITY),
    )
    .await
    .context("RegisterAgent")?;
    conn.call_method(
        Some(mgr.0),
        mgr.1,
        mgr.2,
        "RequestDefaultAgent",
        &path,
    )
    .await
    .context("RequestDefaultAgent")?;

    // Only pin the connection once registration actually succeeded, so a failed
    // attempt does not leave a half-live agent behind.
    AGENT_CONN
        .set(conn)
        .map_err(|_| anyhow!("bluetooth agent already registered"))?;
    Ok(())
}
