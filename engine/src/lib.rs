//! `cast-engine` — the music-push engine behind a third-party NetEase Cloud
//! Music player.
//!
//! Four protocols behind one interface (see [`traits`]):
//!
//! | protocol | discovery      | control                    | status            | lossless |
//! |----------|----------------|----------------------------|-------------------|----------|
//! | UPnP/DLNA| SSDP           | SOAP AVTransport/Rendering | GENA `NOTIFY`     | yes, byte-exact |
//! | HEOS     | SSDP           | Telnet 1255                | `event/*` push    | yes, byte-exact |
//! | AirPlay 1| mDNS `_raop`   | RTSP + RTP/ALAC            | polled            | ALAC 16/44.1 |
//! | QPlay    | mDNS + SSDP    | UPnP + private headers     | GENA `NOTIFY`     | yes, byte-exact |
//!
//! Audio reaches pull-based devices through [`gateway`], which relays the
//! NetEase CDN bytes untouched — that is what makes "无损推送" real rather than
//! marketing.
//!
//! ```no_run
//! use cast_engine::engine::CastEngine;
//! use cast_engine::model::{Quality, Track};
//!
//! let mut engine = CastEngine::start(String::new(), 9100)?;
//! let (found, _lost) = engine.refresh(None);
//! let ids: Vec<_> = found.iter().map(|d| d.id.clone()).collect();
//! let track = Track {
//!     id: 1815628781,
//!     title: "起风了".into(),
//!     artist: "买辣椒也用券".into(),
//!     album: "起风了".into(),
//!     duration_ms: 325_000,
//!     cover: None,
//!     quality: Quality::Lossless,
//! };
//! engine.play_track(&ids, &track)?;
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod airplay;
pub mod crypto;
pub mod engine;
pub mod gateway;
pub mod group;
pub mod heos;
pub mod hub;
pub mod mdns;
pub mod model;
pub mod netease;
pub mod qplay;
pub mod registry;
pub mod ssdp;
pub mod traits;
pub mod upnp;

pub use engine::CastEngine;
pub use hub::{Bus, EventHub};
pub use model::{Device, DeviceId, Event, Protocol, Quality, Status, Track, TransportState};
pub use traits::{Adapter, Sink};
