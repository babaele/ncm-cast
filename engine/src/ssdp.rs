//! SSDP (UPnP) discovery, shared by the UPnP, HEOS and QPlay adapters.
//!
//! One M-SEARCH sweep per search target, all sent from a single socket that is
//! bound to the interface we want answers on. The multicast TTL is kept low (2)
//! because nothing we care about lives more than one hop away.

use anyhow::Result;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub const SSDP_MULTICAST: (IpAddr, u16) = (IpAddr::V4(std::net::Ipv4Addr::new(239, 255, 255, 250)), 1900);

/// Search targets we care about.
pub const ST_RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
pub const ST_MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const ST_HEOS: &str = "urn:schemas-denon-com:device:ACT-Denon:1";

/// One SSDP response, with all of its headers kept.
#[derive(Clone, Debug)]
pub struct SsdpHit {
    /// The `LOCATION` header — URL of the device description XML.
    pub location: String,
    /// Where the answer came from (the device's own address).
    pub from: IpAddr,
    /// The search target this answer is for.
    pub st: String,
    /// `USN`, used as a stable identity for UPnP devices.
    pub usn: String,
    /// Lowercased header map, for protocol-specific extras.
    pub headers: HashMap<String, String>,
}

/// Adapter names that never carry a renderer we can talk to.
///
/// VMware, VirtualBox, Hyper-V, WSL, Tailscale and friends all hand out private
/// IPv4 addresses, and `get_if_addrs()` returns them in OS order — so taking the
/// first private address blindly regularly produced `192.168.44.1` (a VMware
/// host-only adapter) instead of the real LAN address. The renderer was then
/// told to fetch `http://192.168.44.1:9100/...`, which of course it cannot
/// reach; it just sits in `TRANSITIONING` forever.
const VIRTUAL_IFACES: &[&str] = &[
    "vmware",
    "vmnet",
    "virtualbox",
    "vbox",
    "hyper-v",
    "vethernet",
    "wsl",
    "tailscale",
    "zerotier",
    "radmin",
    "hamachi",
    "tap-",
    "tap ",
    "openvpn",
    "npcap",
    "loopback",
    "isatap",
    "teredo",
    "6to4",
];

/// Names that mark a real physical link, used as a positive signal.
const PHYSICAL_HINTS: &[&str] = &["wlan", "wi-fi", "wifi", "wireless", "eth", "以太网", "无线"];

/// Score one interface: higher is a better candidate for the LAN address.
fn score_iface(name: &str, is_p2p: bool) -> i32 {
    let lower = name.to_ascii_lowercase();
    if VIRTUAL_IFACES.iter().any(|v| lower.contains(v)) {
        return -100;
    }
    let mut s = 0;
    if PHYSICAL_HINTS.iter().any(|v| lower.contains(v)) {
        s += 10;
    }
    if is_p2p {
        s -= 5;
    }
    s
}

/// Pick the LAN address to bind to and to hand out in URLs.
///
/// Every candidate is scored (see [`score_iface`]) and the best private IPv4
/// wins; virtual adapters are excluded outright, and Wi-Fi/Ethernet are
/// preferred over anything else. Falls back to any non-loopback address.
#[must_use]
pub fn pick_local_addr() -> Option<IpAddr> {
    let addrs = if_addrs::get_if_addrs().ok()?;
    let mut best: Option<(i32, IpAddr)> = None;
    let mut fallback = None;
    for a in addrs {
        match a.ip() {
            IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                if is_private_v4(&v4.octets()) {
                    let s = score_iface(&a.name, a.is_p2p);
                    if best.as_ref().is_none_or(|(bs, _)| s > *bs) {
                        best = Some((s, IpAddr::V4(v4)));
                    }
                } else {
                    fallback.get_or_insert(IpAddr::V4(v4));
                }
            }
            IpAddr::V6(v6) if !v6.is_loopback() && fallback.is_none() => {
                fallback = Some(IpAddr::V6(v6));
            }
            _ => {}
        }
    }
    best.map(|(_, ip)| ip).or(fallback)
}

fn is_private_v4(o: &[u8; 4]) -> bool {
    matches!(o[0], 10 | 172 | 192)
}

fn build_search(st: &str, mx: u8) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {}:{}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx}\r\n\
         ST: {st}\r\n\
         \r\n",
        SSDP_MULTICAST.0, SSDP_MULTICAST.1
    )
}

/// Run one M-SEARCH sweep for every `sts`, collecting answers for `secs`.
///
/// Answers are de-duplicated by `(LOCATION, ST)`: renderers routinely answer
/// the same search more than once, and a device that is both an AV renderer and
/// an HEOS player answers two different searches.
pub fn search(local_addr: IpAddr, sts: &[&str], secs: u64) -> Result<Vec<SsdpHit>> {
    let bind: SocketAddr = SocketAddr::new(local_addr, 0);
    let domain = match local_addr {
        IpAddr::V4(_) => Domain::IPV4,
        IpAddr::V6(_) => Domain::IPV6,
    };
    let sock2 = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    sock2.set_reuse_address(true).ok();
    sock2.bind(&bind.into())?;
    sock2.set_broadcast(true).ok();
    sock2.set_multicast_ttl_v4(2).ok();
    if let IpAddr::V4(v4) = local_addr {
        sock2.set_multicast_if_v4(&v4).ok();
    }
    let socket: UdpSocket = sock2.into();
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;

    let dest: SocketAddr = SSDP_MULTICAST.into();
    for st in sts {
        let _ = socket.send_to(build_search(st, 2).as_bytes(), dest);
        // Staggering the searches avoids one device answering all of them in a
        // single burst, which some cheap renderers drop.
        std::thread::sleep(Duration::from_millis(60));
    }

    let deadline = Instant::now() + Duration::from_secs(secs.max(1));
    let mut hits: Vec<SsdpHit> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut buf = [0u8; 4096];

    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(hit) = parse_answer(&text, from.ip())
                    && seen.insert((hit.location.clone(), hit.st.clone()))
                {
                    hits.push(hit);
                }
            }
            Err(_) => { /* read timeout — loop re-checks the deadline */ }
        }
    }
    Ok(hits)
}

/// Parse an SSDP `HTTP/1.1 200 OK` answer.
fn parse_answer(text: &str, from: IpAddr) -> Option<SsdpHit> {
    let mut lines = text.lines();
    let status = lines.next().unwrap_or("").trim().to_ascii_uppercase();
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return None;
    }
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let location = headers.get("location")?.clone();
    if location.is_empty() {
        return None;
    }
    Some(SsdpHit {
        location,
        from,
        st: headers.get("st").cloned().unwrap_or_default(),
        usn: headers.get("usn").cloned().unwrap_or_default(),
        headers,
    })
}

/// Fetch a device description document.
pub fn get_description(http: &ureq::Agent, location: &str) -> Result<String> {
    let mut resp = http.get(location).call()?;
    Ok(resp.body_mut().read_to_string()?)
}

/// Resolve a possibly-relative URL found inside a description document.
///
/// UPnP `controlURL`/`eventSubURL`/`SCPDURL` are frequently relative, and a few
/// devices get the leading slash wrong. Everything is resolved against the
/// `LOCATION` we fetched the document from.
#[must_use]
pub fn resolve_url(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let Some(scheme_end) = base.find("://") else {
        return rel.to_string();
    };
    let scheme = &base[..scheme_end];
    let rest = &base[scheme_end + 3..];
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    let path = if let Some(p) = rel.strip_prefix('/') {
        p.to_string()
    } else {
        // relative to the *directory* of base, not to base itself
        let base_dir = match base.rfind('/') {
            Some(i) if i > scheme_end + 3 => &base[..i + 1],
            _ => "/",
        };
        format!("{}{}", &base_dir[scheme_end + 3..], rel)
    };
    format!("{scheme}://{authority}/{path}")
}

/// Pull the text of every element with the given local name out of an XML
/// document, in document order. Used for the handful of fields we need from
/// device descriptions; it is deliberately not a general XML model.
#[must_use]
pub fn xml_text(xml: &str, local_name: &str) -> Vec<String> {
    use xml::reader::{EventReader, XmlEvent};
    let mut out = Vec::new();
    let mut depth: Vec<String> = Vec::new();
    for e in EventReader::new(xml.as_bytes()) {
        match e {
            Ok(XmlEvent::StartElement { name, .. }) => depth.push(name.local_name),
            Ok(XmlEvent::EndElement { .. }) => {
                depth.pop();
            }
            Ok(XmlEvent::Characters(t)) | Ok(XmlEvent::CData(t)) => {
                if depth.last().map(String::as_str) == Some(local_name) {
                    out.push(t);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::score_iface;

    /// A VMware host-only adapter (`192.168.44.1`) once beat the real WLAN
    /// address, so renderers were handed a URL on a network they cannot see.
    #[test]
    fn virtual_adapters_lose_to_physical_ones() {
        assert!(score_iface("VMware Network Adapter VMnet1", false) < 0);
        assert!(score_iface("vEthernet (WSL)", false) < 0);
        assert!(score_iface("Tailscale", false) < 0);
        assert!(score_iface("以太网", false) > 0);
        assert!(score_iface("WLAN", false) > 0);
        assert!(score_iface("wlan0", false) > score_iface("以太网", false) - 1);
    }

    #[test]
    fn physical_names_beat_unknown_names() {
        assert!(score_iface("WLAN", false) > score_iface("adapter-7", false));
        // Point-to-point links (VPNs) rank below a plain physical link.
        assert!(score_iface("WLAN", false) > score_iface("WLAN", true));
    }
}
