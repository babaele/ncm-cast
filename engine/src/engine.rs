//! Session management and playback — the part the host application drives.

use crate::airplay::AirPlayAdapter;
use crate::gateway::Gateway;
use crate::group::{self, Correction, Group, CORRECTION_INTERVAL_MS};
use crate::heos::HeosAdapter;
use crate::hub::{Bus, EventHub};
use crate::model::{Device, DeviceId, Event, Status, Track, TransportState};
use crate::netease::NeteaseClient;
use crate::qplay::QPlayAdapter;
use crate::registry::Registry;
use crate::traits::{ConnectCtx, Sink};
use crate::upnp::UpnpAdapter;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Default port for the RPC/HTTP front-end.
pub const DEFAULT_RPC_PORT: u16 = 9100;
/// How long one discovery sweep waits.
pub const DEFAULT_DISCOVERY_SECS: u64 = 3;

pub struct CastEngine {
    pub ctx: ConnectCtx,
    registry: Registry,
    sessions: HashMap<DeviceId, Box<dyn Sink>>,
    pub client: NeteaseClient,
    pub gateway: Gateway,
    pub hub: EventHub,
    pub bus: Bus,
    group: Option<Group>,
    /// `(track, url)` currently loaded — needed to restart a drifted member.
    current: Option<(Track, String)>,
    queue: VecDeque<Track>,
    last_correction: Instant,
    /// Hand renderers the NetEase CDN URL instead of our own gateway URL.
    ///
    /// The gateway is the better default (stable MIME type, real
    /// `Content-Length`, working `Range`, and it can attach the cookie). But it
    /// requires the renderer to open a connection **back** to this machine, and
    /// plenty of setups refuse that: Windows Firewall, 360/火绒 "LAN stealth",
    /// or a router with AP isolation. Those all look identical from here —
    /// discovery fine, control fine, no audio. Direct mode sidesteps the whole
    /// problem by letting the renderer fetch from NetEase instead of from us.
    direct: bool,
}

impl CastEngine {
    /// Bring up the gateway, the event hub and the discovery registry.
    pub fn new(local_addr: IpAddr, gateway: Gateway, client: NeteaseClient) -> Self {
        let ctx = ConnectCtx {
            http: crate::traits::http_agent(),
            local_addr,
            gateway_base: gateway.base_url(),
            callback_base: Some(format!("{}/evt", gateway.base_url())),
        };
        Self {
            ctx,
            registry: Registry::with_defaults(),
            sessions: HashMap::new(),
            client,
            gateway,
            hub: EventHub::new(),
            bus: Bus::new(),
            group: None,
            current: None,
            queue: VecDeque::new(),
            last_correction: Instant::now(),
            direct: false,
        }
    }

    /// Whether renderers are handed CDN URLs instead of gateway URLs.
    #[must_use]
    pub fn is_direct(&self) -> bool {
        self.direct
    }

    /// Switch between gateway URLs and CDN URLs.
    ///
    /// Use `true` when renderers cannot open a connection back to this host —
    /// see the note on the field for what that looks like in practice.
    pub fn set_direct(&mut self, direct: bool) {
        self.direct = direct;
    }

    /// Convenience constructor: pick the LAN address and describe the gateway
    /// **without** binding it, so the front-end can serve `/t/...` on the same
    /// port as its own API. Use [`Gateway::start`] instead when the gateway
    /// should own a port of its own.
    /// Bring the engine up.
    ///
    /// `host` overrides the LAN address we hand out in URLs. Auto-detection
    /// skips virtual adapters, but machines with several real NICs (wired +
    /// wireless, or a VPN that routes the LAN) still need the explicit form,
    /// because the renderer has to be able to reach whatever address we give it.
    pub fn start(cookie: String, port: u16, host: Option<IpAddr>) -> Result<Self> {
        let local = match host {
            Some(ip) => ip,
            None => {
                crate::ssdp::pick_local_addr().context("no non-loopback network interface")?
            }
        };
        let client = NeteaseClient::new(cookie);
        let gateway = Gateway::new(local, port, client.clone());
        Ok(Self::new(local, gateway, client))
    }

    pub fn local_addr(&self) -> IpAddr {
        self.ctx.local_addr
    }

    /// Install a new NetEase cookie everywhere (right after a QR login).
    pub fn set_cookie(&mut self, cookie: String) {
        self.client = self.client.with_cookie(cookie);
        self.gateway.set_client(self.client.clone());
    }

    // ---------------- discovery ----------------

    /// One discovery sweep. Returns `(new devices, devices that went away)`.
    pub fn refresh(&mut self, secs: Option<u64>) -> (Vec<Device>, Vec<DeviceId>) {
        let timeout = Duration::from_secs(secs.unwrap_or(DEFAULT_DISCOVERY_SECS));
        self.registry.refresh(&self.ctx, timeout, &self.bus)
    }

    #[must_use]
    pub fn devices(&self) -> Vec<Device> {
        self.registry.list()
    }

    #[must_use]
    pub fn device(&self, id: &str) -> Option<Device> {
        self.registry.get(id)
    }

    // ---------------- sessions ----------------

    /// Open a session, reusing an existing one.
    pub fn connect(&mut self, id: &str) -> Result<()> {
        if self.sessions.contains_key(id) {
            return Ok(());
        }
        let dev = self.registry.get(id).with_context(|| format!("unknown device {id}"))?;
        let hub = self.hub.clone();
        let bus = self.bus.clone();
        let sink: Box<dyn Sink> = match dev.protocol {
            crate::model::Protocol::Upnp => {
                Box::new(UpnpAdapter::open(&self.ctx, &dev, hub, bus, Vec::new())?)
            }
            crate::model::Protocol::QPlay => Box::new(QPlayAdapter::open(&self.ctx, &dev, hub, bus)?),
            crate::model::Protocol::Heos => Box::new(HeosAdapter::open(&self.ctx, &dev, bus)?),
            crate::model::Protocol::AirPlay => {
                Box::new(AirPlayAdapter::open(&self.ctx, &dev, bus, self.client.clone())?)
            }
        };
        self.sessions.insert(id.to_string(), sink);
        Ok(())
    }

    pub fn disconnect(&mut self, id: &str) {
        if let Some(mut s) = self.sessions.remove(id) {
            let _ = s.stop();
        }
    }

    #[must_use]
    pub fn connected(&self) -> Vec<DeviceId> {
        self.sessions.keys().cloned().collect()
    }

    fn sink(&mut self, id: &str) -> Result<&mut Box<dyn Sink>> {
        self.sessions.get_mut(id).with_context(|| format!("not connected to {id}"))
    }

    // ---------------- playback ----------------

    /// The URL a device should pull for this track.
    #[must_use]
    /// The URL a renderer should fetch `track` from.
    ///
    /// Gateway URL by default; the NetEase CDN URL in direct mode. In direct
    /// mode a failed lookup is an error rather than a silent fall-back: the
    /// whole point of the mode is to avoid the gateway, so quietly handing out
    /// a gateway URL produces the exact failure the user switched away from —
    /// everything succeeds, nothing plays.
    pub fn url_for(&self, track: &Track) -> Result<String> {
        if self.direct {
            let song = self
                .gateway
                .resolve_fresh(track)
                .with_context(|| format!("direct mode: no CDN URL for track {}", track.id))?;
            if song.url.is_empty() {
                bail!("direct mode: CDN returned an empty URL for track {}", track.id);
            }
            return Ok(song.url);
        }
        Ok(self.gateway.url_for(track))
    }

    /// Load and start `track` on every device in `ids`.
    ///
    /// All members are loaded first and only then started, so the gap between
    /// the first and last device hearing audio is one round of SOAP calls
    /// instead of one full track fetch.
    pub fn play_track(&mut self, ids: &[DeviceId], track: &Track) -> Result<()> {
        let url = self.url_for(track)?;
        for id in ids {
            self.connect(id)?;
        }
        for id in ids {
            let s = self.sink(id)?;
            s.load(track, &url).map_err(|e| {
                // A device that refuses the URL is usually gone; drop the
                // session so the next call re-opens it cleanly.
                self.sessions.remove(id);
                e
            })?;
        }
        for id in ids {
            if let Ok(s) = self.sink(id) {
                let _ = s.play();
            }
        }
        self.current = Some((track.clone(), url));
        self.bus.push(Event::TrackChanged {
            id: ids.first().cloned().unwrap_or_default(),
            track: Some(track.clone()),
        });
        Ok(())
    }

    /// Replace the queue and start from its head.
    pub fn play_queue(&mut self, ids: &[DeviceId], tracks: Vec<Track>) -> Result<()> {
        let mut q = VecDeque::from(tracks);
        if let Some(first) = q.pop_front() {
            self.queue = q;
            self.play_track(ids, &first)?;
        }
        Ok(())
    }

    /// Advance the queue by hand (also used when a track finishes).
    pub fn next(&mut self, ids: &[DeviceId]) -> Result<()> {
        match self.queue.pop_front() {
            Some(t) => self.play_track(ids, &t),
            None => {
                for id in ids {
                    if let Ok(s) = self.sink(id) {
                        let _ = s.stop();
                    }
                }
                Ok(())
            }
        }
    }

    pub fn pause(&mut self, id: &str) -> Result<()> {
        self.sink(id)?.pause()
    }

    pub fn resume(&mut self, id: &str) -> Result<()> {
        self.sink(id)?.play()
    }

    pub fn stop(&mut self, id: &str) -> Result<()> {
        self.sink(id)?.stop()
    }

    /// Seek the renderer to an absolute position (milliseconds). Only protocols
    /// that implement `Sink::seek` support this (UPnP/DLNA does; HEOS/AirPlay
    /// return an error, which the caller should surface as "not supported").
    pub fn seek(&mut self, id: &str, position_ms: u64) -> Result<()> {
        self.sink(id)?.seek(position_ms)
    }

    pub fn set_volume(&mut self, id: &str, v: u8) -> Result<()> {
        self.sink(id)?.set_volume(v)
    }

    pub fn set_mute(&mut self, id: &str, m: bool) -> Result<()> {
        self.sink(id)?.set_mute(m)
    }

    pub fn status(&mut self, id: &str) -> Result<Status> {
        self.sink(id)?.status()
    }

    // ---------------- groups ----------------

    /// Put `ids` into one synchronised group.
    ///
    /// Returns `true` when some protocol grouped them natively, in which case
    /// the engine leaves the devices alone from then on.
    pub fn group(&mut self, ids: &[DeviceId]) -> Result<bool> {
        if ids.len() < 2 {
            bail!("a group needs at least two devices");
        }
        for id in ids {
            self.connect(id)?;
        }
        let members: Vec<Device> = ids.iter().filter_map(|i| self.registry.get(i)).collect();
        let mut native = false;
        for id in ids {
            if let Ok(s) = self.sink(id)
                && s.join_group(&members)?
            {
                native = true;
            }
        }
        self.group = Some(Group {
            members: ids.to_vec(),
            leader: ids.first().cloned().unwrap_or_default(),
            native,
        });
        Ok(native)
    }

    pub fn ungroup(&mut self) -> Result<()> {
        if let Some(g) = self.group.take() {
            for m in &g.members {
                if let Ok(s) = self.sink(m) {
                    let _ = s.leave_group();
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn group_info(&self) -> Option<Group> {
        self.group.clone()
    }

    // ---------------- housekeeping ----------------

    /// Call roughly once a second.
    pub fn tick(&mut self) {
        // Keep-alives, subscription renewals, RAOP liveness.
        let mut dead = Vec::new();
        for (id, s) in &mut self.sessions {
            if s.tick().is_err() {
                dead.push(id.clone());
            }
        }
        for id in dead {
            self.sessions.remove(&id);
        }
        self.correct_drift();
    }

    /// Software sync for groups that are not native.
    fn correct_drift(&mut self) {
        let Some(g) = self.group.clone() else { return };
        if g.native || self.last_correction.elapsed() < Duration::from_millis(CORRECTION_INTERVAL_MS) {
            return;
        }
        self.last_correction = Instant::now();
        let leader_pos = match self.sessions.get_mut(&g.leader) {
            Some(s) => match s.status() {
                Ok(st) => st,
                Err(_) => return,
            },
            None => return,
        };
        // A track that has finished on the leader advances the whole queue.
        if leader_pos.duration_ms > 0
            && leader_pos.position_ms + 1_000 >= leader_pos.duration_ms
            && !self.queue.is_empty()
        {
            let ids = g.members.clone();
            let _ = self.next(&ids);
            return;
        }
        for m in g.followers() {
            let Some(s) = self.sessions.get_mut(m) else { continue };
            let Ok(st) = s.status() else { continue };
            match group::correction_between(leader_pos.position_ms, st.position_ms) {
                Correction::None => {}
                Correction::Seek(p) => {
                    log::debug!("sync: nudging {m} to {p} ms");
                    let _ = s.seek(p);
                }
                Correction::Restart => {
                    if let Some((t, url)) = &self.current {
                        let t = t.clone();
                        let url = url.clone();
                        if s.load(&t, &url).is_ok() && s.play().is_ok() && s.seek(leader_pos.position_ms).is_err() {
                            log::debug!("sync: {m} does not support seek; restarted from 0");
                        }
                    }
                }
            }
        }
    }

    /// True when at least one connected device is actually playing.
    pub fn is_playing(&mut self) -> bool {
        let ids: Vec<DeviceId> = self.sessions.keys().cloned().collect();
        ids.iter().any(|id| {
            self.sessions
                .get_mut(id)
                .and_then(|s| s.status().ok())
                .map(|st| is_active(&st))
                .unwrap_or(false)
        })
    }
}

/// Convenience: what a UI usually wants to show for one device.
#[must_use]
pub fn summarise(dev: &Device, st: Option<Status>) -> String {
    match st {
        Some(s) => format!("{} — {:?} {} ms", dev.name, s.state, s.position_ms),
        None => format!("{} — {}", dev.name, dev.protocol),
    }
}

/// Guard used by callers that want to act only on a live session.
#[must_use]
pub fn is_active(st: &Status) -> bool {
    matches!(st.state, TransportState::Playing | TransportState::Buffering)
}
