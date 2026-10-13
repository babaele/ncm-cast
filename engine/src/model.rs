//! Shared data model for every cast protocol.
//!
//! A [`Device`] is what discovery produces; a [`Sink`] (see [`crate::traits`])
//! is what playback control talks to; [`Status`]/[`Event`] are what the host
//! application (an Electron/Vue player) receives back.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;

/// Stable, protocol-scoped identifier, e.g. `upnp:uuid:9a1b...`,
/// `heos:192.168.1.20:1`, `airplay:Kitchen._raop`, `qplay:uuid:...`.
pub type DeviceId = String;

/// The push protocol a device speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// DLNA / UPnP AV (AVTransport + RenderingControl)
    Upnp,
    /// Denon HEOS / Marantz (Telnet CLI on port 1255)
    Heos,
    /// AirPlay 1 / RAOP (RTSP + RTP/ALAC)
    AirPlay,
    /// QPlay compatibility layer (UPnP + Tencent private headers)
    QPlay,
}

impl Protocol {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Upnp => "upnp",
            Protocol::Heos => "heos",
            Protocol::AirPlay => "airplay",
            Protocol::QPlay => "qplay",
        }
    }

    /// True when the protocol can stream an untouched audio file (no decode).
    ///
    /// AirPlay 1 always terminates the stream in software, so it can never be
    /// a byte-for-byte passthrough.
    #[must_use]
    pub fn is_passthrough(self) -> bool {
        matches!(self, Protocol::Upnp | Protocol::Heos | Protocol::QPlay)
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a device is able to do. Drives which controls the UI shows.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Caps {
    pub pause: bool,
    pub seek: bool,
    pub volume: bool,
    pub mute: bool,
    /// The protocol can group devices itself (HEOS `set_group`).
    pub native_group: bool,
    /// Lossless audio reaches the device unmodified (FLAC/ALAC passthrough).
    pub lossless_passthrough: bool,
    /// The device pushes status changes to us (GENA / HEOS events).
    pub status_push: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            pause: true,
            seek: true,
            volume: true,
            mute: true,
            native_group: false,
            lossless_passthrough: true,
            status_push: false,
        }
    }
}

/// A discovered renderer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub protocol: Protocol,
    pub name: String,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub addr: IpAddr,
    pub port: u16,
    #[serde(default)]
    pub caps: Caps,
    /// Protocol-specific knobs kept as text so they survive JSON round-trips:
    /// `location`, `udn`, `control_url`, `pid`, `gid`, `pk`, `txt.*`, ...
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl Device {
    #[must_use]
    pub fn extra(&self, k: &str) -> Option<&str> {
        self.extra.get(k).map(String::as_str)
    }
}

/// NetEase audio quality tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// 128 kbps MP3 — playable without login
    Standard,
    /// 320 kbps MP3
    Exhigh,
    /// FLAC 16/44.1 (default)
    #[default]
    Lossless,
    /// Hi-Res (>48 kHz / 24 bit), needs VIP
    Hires,
}

impl Quality {
    /// The `level=` value of `/api/song/enhance/player/url/v1`.
    #[must_use]
    pub fn level(self) -> &'static str {
        match self {
            Quality::Standard => "standard",
            Quality::Exhigh => "exhigh",
            Quality::Lossless => "lossless",
            Quality::Hires => "lossless",
        }
    }

    /// Fallback chain, best first. Hi-Res and lossless share one level, so the
    /// chain only differs below it.
    #[must_use]
    pub fn chain(self) -> &'static [&'static str] {
        match self {
            Quality::Standard => &["standard"],
            Quality::Exhigh => &["exhigh", "higher", "standard"],
            Quality::Lossless | Quality::Hires => &["lossless", "exhigh", "higher", "standard"],
        }
    }
}

/// One track of a NetEase playlist, ready to be pushed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    /// NetEase song id
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// 0 when the API did not report one
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub cover: Option<String>,
    /// Requested quality; downgraded automatically when unavailable
    #[serde(default)]
    pub quality: Quality,
}

impl Track {
    /// File-extension hint used in the gateway URL so renderers that sniff the
    /// URL get the right content type.
    #[must_use]
    pub fn ext(&self) -> &'static str {
        match self.quality {
            Quality::Lossless | Quality::Hires => "flac",
            _ => "mp3",
        }
    }

    /// `DLNA.ORG_PN` / MIME guess for DIDL-Lite `protocolInfo`.
    #[must_use]
    pub fn mime(&self) -> &'static str {
        match self.quality {
            Quality::Lossless | Quality::Hires => "audio/flac",
            _ => "audio/mpeg",
        }
    }

    /// `H:MM:SS.mmm` as required by DIDL-Lite `res@duration`.
    #[must_use]
    pub fn didl_duration(&self) -> String {
        let total = self.duration_ms / 1000;
        format!("{:01}:{:02}:{:02}.000", total / 3600, (total / 60) % 60, total % 60)
    }
}

/// Transport state, unified across protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Idle,
    Buffering,
    Playing,
    Paused,
    Stopped,
    Error,
}

/// A status snapshot, unified across protocols.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Status {
    pub state: TransportState,
    /// Milliseconds into the current track
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub volume: Option<u8>,
    #[serde(default)]
    pub muted: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            state: TransportState::Idle,
            position_ms: 0,
            duration_ms: 0,
            volume: None,
            muted: false,
        }
    }
}

/// Something the host application cares about.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Struct variants throughout: the enum is `#[serde(tag = "type")]`-tagged,
    /// and a newtype variant would only serialize if its inner type were a map.
    DeviceFound { device: Device },
    DeviceLost { id: DeviceId },
    Status { id: DeviceId, status: Status },
    TrackChanged { id: DeviceId, track: Option<Track> },
    EndOfTrack { id: DeviceId },
    Error { id: Option<DeviceId>, message: String },
    Log { level: &'static str, message: String },
}

/// Build a device id that is stable across discovery cycles.
#[must_use]
pub fn device_id(protocol: Protocol, unique: &str) -> DeviceId {
    format!("{}:{}", protocol.as_str(), unique)
}
