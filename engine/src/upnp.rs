//! DLNA / UPnP AV — the protocol nearly every wired speaker speaks.
//!
//! * **discovery**  SSDP `M-SEARCH`, then fetch and parse `GetDescription.xml`
//! * **control**    SOAP over HTTP: `AVTransport` (transport) and
//!                  `RenderingControl` (volume/mute)
//! * **status**     GENA `SUBSCRIBE` — the renderer POSTs a `NOTIFY` with a
//!                  `LastChange` document whenever its state changes, which we
//!                  route through [`EventHub`]; polling is only the fallback
//!
//! Audio is never touched: the renderer is handed a gateway URL and pulls the
//! original file itself, so a FLAC arrives bit-identical.

use crate::hub::{Bus, EventHub};
use crate::model::{Caps, Device, Protocol, Status, Track, TransportState, device_id};
use crate::ssdp::{self, SsdpHit};
use crate::traits::{Adapter, ConnectCtx, Sink};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const AVTRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
/// How often position is polled even while GENA events are flowing.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// One `<service>` from a device description.
#[derive(Clone, Debug)]
pub struct UpnpService {
    pub service_type: String,
    pub control_url: String,
    pub event_sub_url: Option<String>,
    pub service_id: String,
}

/// The parts of a device description we need.
#[derive(Clone, Debug, Default)]
pub struct UpnpDesc {
    pub friendly_name: String,
    pub manufacturer: String,
    pub model: String,
    pub udn: String,
    pub services: Vec<UpnpService>,
}

impl UpnpDesc {
    #[must_use]
    pub fn service(&self, t: &str) -> Option<&UpnpService> {
        self.services.iter().find(|s| s.service_type == t)
    }
}

/// Parse a UPnP device description document.
///
/// Hand-rolled over the event stream rather than a DOM: these documents are a
/// few kB and we need four scalars plus a repeated `<service>` block.
#[must_use]
pub fn parse_description(base: &str, xml_text: &str) -> Option<UpnpDesc> {
    use xml::reader::{EventReader, XmlEvent};

    let mut out = UpnpDesc::default();
    let mut path: Vec<String> = Vec::new();
    let mut svc: Option<UpnpService> = None;

    for e in EventReader::new(xml_text.as_bytes()) {
        match e {
            Ok(XmlEvent::StartElement { name, .. }) => {
                let local = name.local_name.clone();
                if local == "service" {
                    svc = Some(UpnpService {
                        service_type: String::new(),
                        control_url: String::new(),
                        event_sub_url: None,
                        service_id: String::new(),
                    });
                }
                path.push(local);
            }
            Ok(XmlEvent::EndElement { name }) => {
                if name.local_name == "service" && let Some(s) = svc.take() {
                    out.services.push(s);
                }
                path.pop();
            }
            Ok(XmlEvent::Characters(t)) | Ok(XmlEvent::CData(t)) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    continue;
                }
                match path.last().map(String::as_str) {
                    Some("friendlyName") => out.friendly_name = t.clone(),
                    Some("manufacturer") => out.manufacturer = t.clone(),
                    Some("modelName") => out.model = t.clone(),
                    Some("UDN") => out.udn = t.clone(),
                    _ => {}
                }
                if let Some(s) = svc.as_mut() {
                    match path.last().map(String::as_str) {
                        Some("serviceType") => s.service_type = t.clone(),
                        Some("controlURL") => s.control_url = ssdp::resolve_url(base, &t),
                        Some("eventSubURL") => s.event_sub_url = Some(ssdp::resolve_url(base, &t)),
                        Some("serviceId") => s.service_id = t.clone(),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if out.udn.is_empty() || out.services.is_empty() {
        return None;
    }
    Some(out)
}

/// Turn one SSDP answer into a [`Device`], or `None` if it is not a renderer.
fn device_from_hit(http: &ureq::Agent, hit: &SsdpHit, id_protocol: Protocol) -> Option<Device> {
    let xml_text = ssdp::get_description(http, &hit.location).ok()?;
    let desc = parse_description(&hit.location, &xml_text)?;
    let av = desc.service(AVTRANSPORT)?;
    let rc = desc.service(RENDERING_CONTROL);
    let udn = desc.udn.trim_start_matches("uuid:").to_string();
    let name = if desc.friendly_name.is_empty() {
        desc.model.clone()
    } else {
        desc.friendly_name.clone()
    };

    let mut extra = std::collections::BTreeMap::new();
    extra.insert("location".into(), hit.location.clone());
    extra.insert("udn".into(), udn.clone());
    extra.insert("av_control".into(), av.control_url.clone());
    if let Some(u) = &av.event_sub_url {
        extra.insert("av_event".into(), u.clone());
    }
    if let Some(r) = rc {
        extra.insert("rc_control".into(), r.control_url.clone());
        if let Some(u) = &r.event_sub_url {
            extra.insert("rc_event".into(), u.clone());
        }
    }

    let mut caps = Caps::default();
    caps.volume = rc.is_some();
    caps.mute = rc.is_some();
    caps.lossless_passthrough = true;
    caps.status_push = av.event_sub_url.is_some();

    Some(Device {
        id: device_id(id_protocol, &udn),
        protocol: id_protocol,
        name,
        model: if desc.model.is_empty() { None } else { Some(desc.model.clone()) },
        manufacturer: if desc.manufacturer.is_empty() { None } else { Some(desc.manufacturer) },
        addr: hit.from,
        port: port_of(&hit.location, hit.from),
        caps,
        extra,
    })
}

fn port_of(location: &str, fallback: IpAddr) -> u16 {
    let rest = location.split("://").nth(1).unwrap_or(location);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or_else(|| {
        let _ = fallback;
        80
    })
}

/// Shared discovery for UPnP and the QPlay compatibility layer.
///
/// `only` filters on the parsed description — this is how QPlay picks the
/// devices that advertise Tencent's private service.
pub fn discover_upnp(
    ctx: &ConnectCtx,
    timeout: Duration,
    extra_st: &[&str],
    only: Option<&(dyn Fn(&UpnpDesc) -> bool + Send + Sync)>,
    as_protocol: Protocol,
) -> Result<Vec<Device>> {
    let mut sts = vec![ssdp::ST_RENDERING_CONTROL, ssdp::ST_MEDIA_RENDERER];
    sts.extend_from_slice(extra_st);
    let hits = ssdp::search(ctx.local_addr, &sts, timeout.as_secs().max(2))?;

    let http = ctx.http.clone();
    let devices: Vec<Device> = std::thread::scope(|scope| {
        let handles: Vec<_> = hits
            .iter()
            .map(|hit| {
                let http = http.clone();
                scope.spawn(move || {
                    let xml_text = ssdp::get_description(&http, &hit.location).ok()?;
                    let desc = parse_description(&hit.location, &xml_text)?;
                    if let Some(f) = only
                        && !f(&desc)
                    {
                        return None;
                    }
                    device_from_description(hit, desc, as_protocol)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });

    // A device answering two searches yields two identical ids; keep one.
    let mut seen = std::collections::HashSet::new();
    Ok(devices.into_iter().filter(|d| seen.insert(d.id.clone())).collect())
}

fn device_from_description(hit: &SsdpHit, desc: UpnpDesc, as_protocol: Protocol) -> Option<Device> {
    let av = desc.service(AVTRANSPORT)?;
    let rc = desc.service(RENDERING_CONTROL);
    let udn = desc.udn.trim_start_matches("uuid:").to_string();
    let name = if desc.friendly_name.is_empty() { desc.model.clone() } else { desc.friendly_name.clone() };
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("location".into(), hit.location.clone());
    extra.insert("udn".into(), udn.clone());
    extra.insert("av_control".into(), av.control_url.clone());
    if let Some(u) = &av.event_sub_url {
        extra.insert("av_event".into(), u.clone());
    }
    if let Some(r) = rc {
        extra.insert("rc_control".into(), r.control_url.clone());
        if let Some(u) = &r.event_sub_url {
            extra.insert("rc_event".into(), u.clone());
        }
    }
    let mut caps = Caps::default();
    caps.volume = rc.is_some();
    caps.mute = rc.is_some();
    caps.lossless_passthrough = true;
    caps.status_push = av.event_sub_url.is_some();
    Some(Device {
        id: device_id(as_protocol, &udn),
        protocol: as_protocol,
        name,
        model: if desc.model.is_empty() { None } else { Some(desc.model) },
        manufacturer: if desc.manufacturer.is_empty() { None } else { Some(desc.manufacturer) },
        addr: hit.from,
        port: port_of(&hit.location, hit.from),
        caps,
        extra,
    })
}

/// The DLNA/UPnP adapter.
pub struct UpnpAdapter;

impl Adapter for UpnpAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::Upnp
    }

    fn discover(&self, ctx: &ConnectCtx, timeout: Duration) -> Result<Vec<Device>> {
        discover_upnp(ctx, timeout, &[], None, Protocol::Upnp)
    }
}

impl UpnpAdapter {
    /// Open a session. `extra_headers` is how the QPlay layer tags its requests.
    pub fn open(
        ctx: &ConnectCtx,
        dev: &Device,
        hub: EventHub,
        bus: Bus,
        extra_headers: Vec<(String, String)>,
    ) -> Result<UpnpSink> {
        let av = UpnpService {
            service_type: AVTRANSPORT.to_string(),
            control_url: dev.extra("av_control").context("device has no av_control")?.to_string(),
            event_sub_url: dev.extra("av_event").map(str::to_string),
            service_id: "urn:upnp-org:serviceId:AVTransport".to_string(),
        };
        let rc = dev.extra("rc_control").map(|u| UpnpService {
            service_type: RENDERING_CONTROL.to_string(),
            control_url: u.to_string(),
            event_sub_url: dev.extra("rc_event").map(str::to_string),
            service_id: "urn:upnp-org:serviceId:RenderingControl".to_string(),
        });
        let mut sink = UpnpSink {
            dev: dev.clone(),
            http: ctx.http.clone(),
            av,
            rc,
            sid: None,
            hub,
            bus,
            status: Arc::new(Mutex::new(Status::default())),
            evt_path: format!("upnp-{}", dev.extra("udn").unwrap_or(&dev.id)),
            callback_base: ctx.callback_base.clone(),
            extra_headers,
            current: None,
            url: None,
            last_poll: None,
        };
        sink.subscribe()?;
        Ok(sink)
    }
}

/// A live UPnP session.
pub struct UpnpSink {
    dev: Device,
    http: ureq::Agent,
    av: UpnpService,
    rc: Option<UpnpService>,
    /// `(SID, when it was taken)` — renewed at half the timeout.
    sid: Option<(String, Instant)>,
    hub: EventHub,
    bus: Bus,
    status: Arc<Mutex<Status>>,
    evt_path: String,
    callback_base: Option<String>,
    extra_headers: Vec<(String, String)>,
    current: Option<Track>,
    url: Option<String>,
    /// Last time we polled the device, so GENA events keep us instant while a
    /// slow poll still tracks position.
    last_poll: Option<Instant>,
}

/// Send a raw GENA request (`SUBSCRIBE` / `UNSUBSCRIBE`) and return the `SID`
/// header if the device grants one.
///
/// Hand-rolled over `TcpStream`: these verbs are not part of `ureq`'s builder
/// API, and a generated request gets rejected by renderers that accept the
/// plain one.
pub fn event_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<Option<String>> {
    let (host, addr, path) = split_url(url)?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\nContent-Length: 0\r\n\r\n");

    let mut stream = TcpStream::connect(addr).with_context(|| format!("connect {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(req.as_bytes())?;

    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    let text = String::from_utf8_lossy(&reply);
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or_default().to_string();
    if !status.contains(" 2") {
        bail!("GENA {method}: {status}");
    }
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.eq_ignore_ascii_case("sid") {
            return Ok(Some(v.trim().to_string()));
        }
    }
    Ok(None)
}

/// `http://host:port/path` → `("host:port", SocketAddr, "/path")`.
fn split_url(url: &str) -> Result<(String, SocketAddr, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, "/"));
    let path = format!("/{path}");
    let (host, port) = authority.split_once(':').unwrap_or((authority, "80"));
    let port: u16 = port.parse().unwrap_or(80);
    let ip: IpAddr = host
        .parse()
        .or_else(|_| -> Result<IpAddr> {
            use std::net::ToSocketAddrs;
            let resolved = (host, port).to_socket_addrs()?.next().context("no address")?;
            Ok(resolved.ip())
        })
        .with_context(|| format!("resolving {host}"))?;
    Ok((authority.to_string(), SocketAddr::new(ip, port), path))
}

/// Build the SOAP envelope for one action.
///
/// NOTE: no `\` line continuations in the literal below. A raw string
/// (`r#"…"#`) does not process escapes, so `\` + newline would survive into the
/// payload as a literal backslash — and a stray `\` right after the XML
/// declaration makes the body malformed, which renderers answer with HTTP 400.
/// (This actually happened; the tests below keep it from happening again.)
pub fn soap_body(service_type: &str, action: &str, args: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:{action} xmlns:u="{service_type}">{args}</u:{action}></s:Body></s:Envelope>"#
    )
}

impl UpnpSink {
    /// POST one SOAP action and return the response body.
    fn soap(&self, svc: &UpnpService, action: &str, args: &str) -> Result<String> {
        let body = soap_body(&svc.service_type, action, args);
        let mut req = self
            .http
            .post(&svc.control_url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPACTION", &format!("\"{}#{}\"", svc.service_type, action));
        for (k, v) in &self.extra_headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let mut resp = req.send(body).with_context(|| format!("SOAP {action}"))?;
        let text = resp.body_mut().read_to_string()?;
        if let Some(desc) = ssdp::xml_text(&text, "errorDescription").first() {
            bail!("{action}: {desc}");
        }
        Ok(text)
    }

    fn av(&self, action: &str, args: &str) -> Result<String> {
        self.soap(&self.av, action, args)
    }

    /// GENA subscription, so the renderer pushes state changes to us.
    fn subscribe(&mut self) -> Result<()> {
        let Some(event_url) = self.av.event_sub_url.clone() else {
            return Ok(()); // no event URL: we poll instead
        };
        let Some(base) = self.callback_base.clone() else {
            return Ok(());
        };
        let callback = format!("<{base}/{}/>", self.evt_path);
        let sid = self.event_request(
            "SUBSCRIBE",
            &event_url,
            &[
                ("CALLBACK", callback.as_str()),
                ("NT", "upnp:event"),
                ("TIMEOUT", "Second-600"),
            ],
        );
        let sid = match sid {
            Ok(s) => s,
            Err(e) => {
                log::warn!("upnp: subscribe to {} failed: {e}", self.dev.name);
                return Ok(()); // not fatal — polling still works
            }
        };
        let Some(sid) = sid else { return Ok(()) };
        let status = Arc::clone(&self.status);
        let bus = self.bus.clone();
        let id = self.dev.id.clone();
        self.hub.register(
            &self.evt_path,
            Arc::new(move |body: &str| {
                if let Some(s) = parse_last_change(body) {
                    if let Ok(mut cur) = status.lock() {
                        *cur = s;
                    }
                    bus.push(crate::model::Event::Status { id: id.clone(), status: s });
                }
            }),
        );
        self.sid = Some((sid, Instant::now()));
        Ok(())
    }

    fn unsubscribe(&mut self) {
        if let Some((sid, _)) = self.sid.take()
            && let Some(url) = self.av.event_sub_url.clone()
        {
            let _ = self.event_request("UNSUBSCRIBE", &url, &[("SID", sid.as_str())]);
        }
        self.hub.unregister(&self.evt_path);
    }

    /// GENA `SUBSCRIBE`/`UNSUBSCRIBE`.
    ///
    /// Sent over a bare `TcpStream` on purpose. `ureq` has no builder for these
    /// verbs and rejects them when assembled by hand, whereas renderers expect
    /// the plain request (verified against a Denon Home 350); a raw socket is
    /// both simpler and exactly what the device wants.
    fn event_request(&self, method: &str, url: &str, headers: &[(&str, &str)]) -> Result<Option<String>> {
        event_request(method, url, headers)
    }

    fn transport_state(&self) -> Result<TransportState> {
        let text = self.av("GetTransportInfo", "<InstanceID>0</InstanceID>")?;
        let state = ssdp::xml_text(&text, "CurrentTransportState")
            .first()
            .cloned()
            .unwrap_or_default();
        Ok(match state.as_str() {
            "PLAYING" => TransportState::Playing,
            "PAUSED_PLAYBACK" => TransportState::Paused,
            "STOPPED" => TransportState::Stopped,
            "TRANSITIONING" | "NO_MEDIA_PRESENT" => TransportState::Buffering,
            _ => TransportState::Idle,
        })
    }
}

impl Sink for UpnpSink {
    fn protocol(&self) -> Protocol {
        Protocol::Upnp
    }

    fn load(&mut self, track: &Track, url: &str) -> Result<()> {
        let metadata = htmlescape::encode_minimal(&didl(track, url));
        let args = format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI>\
             <CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
            htmlescape::encode_minimal(url)
        );
        self.av("SetAVTransportURI", &args)?;
        self.current = Some(track.clone());
        self.url = Some(url.to_string());
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        self.av("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")?;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Playing;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.av("Pause", "<InstanceID>0</InstanceID>")?;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Paused;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.av("Stop", "<InstanceID>0</InstanceID>")?;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Stopped;
        }
        Ok(())
    }

    fn seek(&mut self, position_ms: u64) -> Result<()> {
        self.av(
            "Seek",
            &format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{}</Target>", hms(position_ms)),
        )?;
        Ok(())
    }

    fn set_volume(&mut self, volume: u8) -> Result<()> {
        let Some(rc) = self.rc.clone() else { bail!("renderer exposes no RenderingControl") };
        self.soap(&rc, "SetVolume", &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{}</DesiredVolume>", volume.min(100)))?;
        if let Ok(mut s) = self.status.lock() {
            s.volume = Some(volume);
        }
        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<()> {
        let Some(rc) = self.rc.clone() else { bail!("renderer exposes no RenderingControl") };
        let v = if muted { "1" } else { "0" };
        self.soap(&rc, "SetMute", &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{v}</DesiredMute>"))?;
        if let Ok(mut s) = self.status.lock() {
            s.muted = muted;
        }
        Ok(())
    }

    fn status(&mut self) -> Result<Status> {
        let mut s = *self.status.lock().unwrap_or_else(|e| e.into_inner());
        // GENA gives us state changes instantly, but it says nothing about
        // position, so keep polling at a slow cadence either way. Without this
        // the reported position would freeze at whatever GENA last told us.
        let stale = self.last_poll.is_none_or(|at| at.elapsed() >= POLL_EVERY);
        if stale || !self.hub.has(&self.evt_path) || s.state == TransportState::Idle {
            s.state = self.transport_state().unwrap_or(TransportState::Idle);
            if let Ok(text) = self.av("GetPositionInfo", "<InstanceID>0</InstanceID>") {
                s.duration_ms = ssdp::xml_text(&text, "TrackDuration").first().map(|t| parse_hms(t)).unwrap_or(0);
                s.position_ms = ssdp::xml_text(&text, "RelTime").first().map(|t| parse_hms(t)).unwrap_or(0);
            }
            if let Some(rc) = self.rc.clone()
                && let Ok(text) = self.soap(&rc, "GetVolume", "<InstanceID>0</InstanceID><Channel>Master</Channel>")
                && let Some(v) = ssdp::xml_text(&text, "CurrentVolume").first()
            {
                s.volume = v.parse::<u8>().ok();
            }
            self.last_poll = Some(Instant::now());
            *self.status.lock().unwrap_or_else(|e| e.into_inner()) = s;
        }
        if let Some(t) = &self.current {
            s.duration_ms = s.duration_ms.max(t.duration_ms);
        }
        Ok(s)
    }

    fn caps(&self) -> Caps {
        self.dev.caps
    }

    fn tick(&mut self) -> Result<()> {
        // Renew at half the timeout, the way the spec asks.
        if let Some((sid, at)) = &self.sid
            && at.elapsed() > Duration::from_secs(300)
        {
            let url = self.av.event_sub_url.clone().unwrap_or_default();
            if !url.is_empty() {
                let _ = self.event_request(
                    "SUBSCRIBE",
                    &url,
                    &[("SID", sid.as_str()), ("TIMEOUT", "Second-600")],
                );
            }
            self.sid = Some((sid.clone(), Instant::now()));
        }
        Ok(())
    }
}

impl Drop for UpnpSink {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Build the DIDL-Lite metadata a renderer uses for its now-playing display.
#[must_use]
pub fn didl(track: &Track, url: &str) -> String {
    let pn = if track.mime() == "audio/flac" { "FLAC" } else { "MP3" };
    format!(
        // Same caveat as in `soap()`: raw strings keep a trailing `\` verbatim.
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
<item id="{id}" parentID="0" restricted="1">
<dc:title>{title}</dc:title><upnp:artist>{artist}</upnp:artist>
<upnp:album>{album}</upnp:album><upnp:albumArtURI>{cover}</upnp:albumArtURI>
<upnp:class>object.item.audioItem.musicTrack</upnp:class>
<res protocolInfo="http-get:*:{mime}:DLNA.ORG_PN={pn};DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000" duration="{dur}">{url}</res></item></DIDL-Lite>"#,
        id = track.id,
        title = htmlescape::encode_minimal(&track.title),
        artist = htmlescape::encode_minimal(&track.artist),
        album = htmlescape::encode_minimal(&track.album),
        cover = htmlescape::encode_minimal(track.cover.as_deref().unwrap_or("")),
        mime = track.mime(),
        dur = track.didl_duration(),
        url = htmlescape::encode_minimal(url),
    )
}

/// Pull transport state / volume out of a GENA `LastChange` document.
fn parse_last_change(body: &str) -> Option<Status> {
    let mut s = Status::default();
    let state = ssdp::xml_text(body, "TransportState").first().cloned().unwrap_or_default();
    s.state = match state.as_str() {
        "PLAYING" => TransportState::Playing,
        "PAUSED_PLAYBACK" => TransportState::Paused,
        "STOPPED" => TransportState::Stopped,
        "TRANSITIONING" => TransportState::Buffering,
        _ => TransportState::Idle,
    };
    if let Some(v) = ssdp::xml_text(body, "Volume").first() {
        s.volume = v.parse::<u8>().ok();
    }
    if let Some(m) = ssdp::xml_text(body, "Mute").first() {
        s.muted = m == "1" || m.eq_ignore_ascii_case("true");
    }
    // Only trust it if we actually recognised something.
    if matches!(s.state, TransportState::Idle) && s.volume.is_none() {
        return None;
    }
    Some(s)
}

/// `H:MM:SS[.mmm]` → milliseconds.
#[must_use]
pub fn parse_hms(s: &str) -> u64 {
    let mut it = s.split(':');
    let h: u64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    let m: u64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    let sec: f64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0.0);
    h * 3_600_000 + m * 60_000 + (sec * 1000.0) as u64
}

/// Milliseconds → `H:MM:SS`.
#[must_use]
pub fn hms(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:01}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
}

/// Silence the unused-import warning when `device_from_hit` is compiled out.
#[allow(dead_code)]
fn _assert_device_from_hit_used() -> Option<fn(&ureq::Agent, &SsdpHit, Protocol) -> Option<Device>> {
    Some(device_from_hit)
}

#[cfg(test)]
mod tests {
    use super::{didl, hms, soap_body, split_url};
    use crate::model::{Quality, Track};

    /// A stray `\` from a raw-string line continuation once made every SOAP
    /// body malformed (renderers answered HTTP 400). Parse the payload so that
    /// class of bug cannot come back silently.
    fn assert_well_formed(xml: &str, what: &str) {
        assert!(!xml.contains('\\'), "{what} contains a literal backslash: {xml:?}");
        let mut reader = xml::reader::EventReader::from_str(xml);
        loop {
            match reader.next() {
                Ok(xml::reader::XmlEvent::EndDocument) => break,
                Ok(_) => {}
                Err(e) => panic!("{what} is not well-formed XML: {e}\n{xml}"),
            }
        }
    }

    #[test]
    fn soap_envelope_is_well_formed_xml() {
        let body = soap_body(
            "urn:schemas-upnp-org:service:AVTransport:1",
            "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>",
        );
        assert_well_formed(&body, "SOAP body");
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("u:Play"));
        assert!(body.contains("urn:schemas-upnp-org:service:AVTransport:1"));
    }

    #[test]
    fn didl_metadata_is_well_formed_xml() {
        let track = Track {
            id: 42,
            title: "A&B <Live>".into(),
            artist: "某人".into(),
            album: "“专辑”".into(),
            duration_ms: 325_868,
            cover: None,
            quality: Quality::Lossless,
        };
        let d = didl(&track, "http://192.168.1.5:9100/t/42.flac?q=lossless");
        assert_well_formed(&d, "DIDL-Lite");
        assert!(d.contains("DLNA.ORG_PN=FLAC"), "lossless must advertise FLAC: {d}");
        // Escaped before being embedded, so the ampersand never breaks the XML.
        assert!(d.contains("A&amp;B"));
    }

    #[test]
    fn gena_url_splits_into_authority_and_path() {
        let (host, addr, path) = split_url("http://192.168.1.5:60006/upnp/event/AVT").unwrap();
        assert_eq!(host, "192.168.1.5:60006");
        assert_eq!(addr.port(), 60006);
        assert_eq!(path, "/upnp/event/AVT");

        let (_, addr, path) = split_url("http://10.0.0.2/evt/x").unwrap();
        assert_eq!(addr.port(), 80, "no port must fall back to 80");
        assert_eq!(path, "/evt/x");
    }

    #[test]
    fn duration_formats_as_didl() {
        assert_eq!(hms(0), "0:00:00");
        assert_eq!(hms(325_868), "0:05:25");
        assert_eq!(hms(3_725_000), "1:02:05");
    }
}
