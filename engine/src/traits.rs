//! The two traits every push protocol implements.
//!
//! [`Adapter`] = *discovery* + *session creation* (stateless, cheap to keep).
//! [`Sink`]    = *one live connection to one device* (stateful, owns sockets).

use crate::model::{Caps, Device, Protocol, Status, Track};
use std::net::IpAddr;
use std::time::Duration;

/// Everything a [`Sink`] needs from the engine when it is opened.
#[derive(Clone, Debug)]
pub struct ConnectCtx {
    /// Shared HTTP client with sane timeouts.
    pub http: ureq::Agent,
    /// The LAN address the device must reach us on (for the gateway and for
    /// UPnP GENA callbacks).
    pub local_addr: IpAddr,
    /// Base URL of the local byte-passthrough gateway, e.g. `http://192.168.1.5:9100`.
    pub gateway_base: String,
    /// Where the device should send asynchronous events, e.g.
    /// `http://192.168.1.5:9100/evt`. `None` disables push callbacks and the
    /// engine falls back to polling.
    pub callback_base: Option<String>,
}

impl ConnectCtx {
    /// `http://<local_addr>:<port>/evt`
    #[must_use]
    pub fn callback_url(&self, path: &str) -> Option<String> {
        self.callback_base
            .as_ref()
            .map(|b| format!("{b}/{path}"))
    }
}

/// Discovers renderers of one protocol and opens sessions to them.
pub trait Adapter: Send + Sync {
    fn protocol(&self) -> Protocol;

    /// One discovery sweep. May return devices already seen; the engine
    /// de-duplicates by [`Device::id`].
    fn discover(&self, ctx: &ConnectCtx, timeout: Duration) -> anyhow::Result<Vec<Device>>;
}

/// A live session with one renderer.
///
/// Every method is fallible because a network renderer can disappear between
/// two calls; a caller should treat `Err` as "session is gone" and re-open.
pub trait Sink: Send {
    fn protocol(&self) -> Protocol;

    /// Hand a track to the device without starting it (UPnP `SetAVTransportURI`).
    /// `url` is the gateway URL, i.e. what the device will actually pull.
    fn load(&mut self, track: &Track, url: &str) -> anyhow::Result<()>;

    fn play(&mut self) -> anyhow::Result<()>;
    fn pause(&mut self) -> anyhow::Result<()>;
    fn stop(&mut self) -> anyhow::Result<()>;

    /// Seek to an absolute position. Used for drift correction inside a group.
    fn seek(&mut self, position_ms: u64) -> anyhow::Result<()> {
        let _ = position_ms;
        anyhow::bail!("{}: seek not supported", self.protocol())
    }

    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        let _ = volume;
        anyhow::bail!("{}: volume not supported", self.protocol())
    }

    fn set_mute(&mut self, muted: bool) -> anyhow::Result<()> {
        let _ = muted;
        anyhow::bail!("{}: mute not supported", self.protocol())
    }

    /// Poll the current state. Cheap enough to be called a few times a second.
    fn status(&mut self) -> anyhow::Result<Status>;

    fn caps(&self) -> Caps;

    /// Ask the device to join a synchronised group.
    ///
    /// Returns `Ok(true)` when the protocol groups natively (HEOS) — the engine
    /// then stops doing software drift correction. `Ok(false)` means "accepted
    /// but you must sync me yourself". The default is the latter, which is
    /// always correct and never worse.
    fn join_group(&mut self, members: &[Device]) -> anyhow::Result<bool> {
        let _ = members;
        Ok(false)
    }

    fn leave_group(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called roughly once a second by the engine: renews UPnP subscriptions,
    /// sends RAOP keep-alives, re-reads HEOS events, and lets a protocol apply
    /// one drift-correction step.
    fn tick(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Build the `ureq` agent shared by discovery and control.
///
/// `ureq` leaves every timeout unset by default, so one renderer that accepts
/// the TCP connection and then stalls would block a caller forever. Bounding
/// every request is what keeps a dead device from freezing the whole engine.
#[must_use]
pub fn http_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .timeout_connect(Some(Duration::from_secs(4)))
            .build(),
    )
}
