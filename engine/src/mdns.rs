//! A deliberately small mDNS / DNS-SD querier.
//!
//! AirPlay (`_raop._tcp`) and QPlay (`_qplay._tcp`) are only announced over
//! mDNS, and pulling in a full responder stack for a handful of queries is not
//! worth it: this sends one PTR query, reads whatever comes back for a moment,
//! and then issues follow-up queries for the records a device left out.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// One advertised service instance.
#[derive(Clone, Debug)]
pub struct SvcInstance {
    /// Human readable instance name, e.g. `Kitchen`.
    pub name: String,
    /// Full service name as advertised, e.g. `Kitchen._raop._tcp.local.`.
    pub fqdn: String,
    /// Target hostname, e.g. `AirPort-Express.local.`.
    pub host: String,
    pub port: u16,
    pub addrs: Vec<IpAddr>,
    /// Decoded TXT record (`key=value`, bare keys get an empty value).
    pub txt: BTreeMap<String, String>,
}

impl SvcInstance {
    #[must_use]
    pub fn txt(&self, k: &str) -> Option<&str> {
        self.txt.get(k).map(String::as_str)
    }

    #[must_use]
    pub fn addr(&self) -> Option<IpAddr> {
        self.addrs.iter().find(|a| a.is_ipv4()).copied()
    }

    /// Strip `.<service>.local.` from an instance name.
    #[must_use]
    pub fn short_name(fqdn: &str) -> String {
        fqdn.split('.').next().unwrap_or(fqdn).replace("\\032", " ")
    }
}

/// Ask for one service type, e.g. `_raop._tcp.local.`
pub fn query(service: &str, timeout: Duration) -> Result<Vec<SvcInstance>> {
    let socket = open_socket()?;
    let dest: SocketAddr = SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT);

    // Round 1: who advertises this service?
    send_query(&socket, dest, service, 12 /* PTR */)?;
    let mut store = RecordStore::default();
    collect(&socket, timeout, &mut store);

    let mut targets: Vec<String> = store
        .ptr_targets(service)
        .into_iter()
        .collect();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // Round 2: fill in whatever the additional section did not carry. Most
    // devices send everything at once; the ones that do not get a second pass.
    let mut need_more = false;
    for t in &targets {
        if store.records(t, 33 /* SRV */).is_empty() || store.records(t, 16 /* TXT */).is_empty() {
            send_query(&socket, dest, t, 255 /* ANY */)?;
            need_more = true;
        }
    }
    if need_more {
        collect(&socket, (timeout / 2).max(Duration::from_millis(400)), &mut store);
    }

    // Round 3: resolve host names we still have no address for.
    let hosts: Vec<String> = targets
        .iter()
        .filter_map(|t| store.srv(t))
        .map(|(_port, host)| host)
        .filter(|h| store.records(h, 1 /* A */).is_empty())
        .collect();
    for h in hosts {
        send_query(&socket, dest, &h, 1)?;
    }
    if !targets.is_empty() {
        collect(&socket, (timeout / 2).max(Duration::from_millis(400)), &mut store);
    }

    let mut out = Vec::new();
    for t in targets {
        let Some((port, host)) = store.srv(&t) else { continue };
        let addrs = store
            .records(&host, 1)
            .iter()
            .filter_map(|r| r.as_ipv4())
            .map(IpAddr::V4)
            .collect::<Vec<_>>();
        out.push(SvcInstance {
            name: SvcInstance::short_name(&t),
            fqdn: t.clone(),
            host,
            port,
            addrs,
            txt: store.txt(&t),
        });
    }
    Ok(out)
}

fn open_socket() -> Result<UdpSocket> {
    // 5353 with reuse lets us see unicast answers addressed to the mDNS port;
    // if something already owns it we still get multicast traffic on an
    // ephemeral port.
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    s.set_reuse_address(true)?;
    #[cfg(not(windows))]
    s.set_reuse_port(true).ok();
    s.set_multicast_ttl_v4(255).ok();
    s.set_multicast_loop_v4(true).ok();
    s.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), MDNS_PORT).into())
        .or_else(|_| s.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).into()))?;
    s.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED).ok();
    s.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(s.into())
}

fn send_query(s: &UdpSocket, dest: SocketAddr, name: &str, qtype: u16) -> Result<()> {
    let mut msg = vec![0u8; 12];
    msg[4] = 0;
    msg[5] = 1; // QDCOUNT = 1
    encode_name(&mut msg, name);
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&[0, 1]); // QCLASS = IN
    s.send_to(&msg, dest).with_context(|| format!("mDNS query {name}"))?;
    Ok(())
}

fn encode_name(msg: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0);
}

fn collect(socket: &UdpSocket, for_time: Duration, store: &mut RecordStore) {
    let deadline = Instant::now() + for_time;
    let mut buf = [0u8; 9000];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => store.absorb(&buf[..n]),
            Err(_) => {}
        }
    }
}

/// A parsed resource record.
#[derive(Clone, Debug)]
struct Record {
    rtype: u16,
    rdata: Vec<u8>,
}

impl Record {
    fn as_ipv4(&self) -> Option<Ipv4Addr> {
        if self.rdata.len() == 4 {
            Some(Ipv4Addr::new(self.rdata[0], self.rdata[1], self.rdata[2], self.rdata[3]))
        } else {
            None
        }
    }
}

/// Everything we heard, keyed by `(owner name, record type)`.
#[derive(Default)]
struct RecordStore {
    map: BTreeMap<(String, u16), Vec<Record>>,
}

impl RecordStore {
    fn insert(&mut self, owner: String, rec: Record) {
        self.map.entry((owner.to_ascii_lowercase(), rec.rtype)).or_default().push(rec);
    }

    fn records(&self, owner: &str, rtype: u16) -> Vec<Record> {
        self.map
            .get(&(owner.to_ascii_lowercase(), rtype))
            .cloned()
            .unwrap_or_default()
    }

    fn ptr_targets(&self, service: &str) -> Vec<String> {
        self.records(service, 12)
            .iter()
            .filter_map(|r| read_name(&r.rdata, 0).ok().map(|(n, _)| n))
            .collect()
    }

    fn srv(&self, target: &str) -> Option<(u16, String)> {
        self.records(target, 33).first().and_then(|r| {
            if r.rdata.len() < 6 {
                return None;
            }
            let port = u16::from_be_bytes([r.rdata[4], r.rdata[5]]);
            read_name(&r.rdata, 6).ok().map(|(h, _)| (port, h))
        })
    }

    fn txt(&self, target: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for r in self.records(target, 16) {
            let mut i = 0usize;
            while i < r.rdata.len() {
                let len = r.rdata[i] as usize;
                i += 1;
                if i + len > r.rdata.len() {
                    break;
                }
                let s = String::from_utf8_lossy(&r.rdata[i..i + len]).to_string();
                i += len;
                match s.split_once('=') {
                    Some((k, v)) => {
                        out.insert(k.to_string(), v.to_string());
                    }
                    None => {
                        out.insert(s, String::new());
                    }
                }
            }
        }
        out
    }

    /// Parse one whole mDNS packet, including its additional records.
    fn absorb(&mut self, buf: &[u8]) {
        if buf.len() < 12 {
            return;
        }
        let qd = u16::from_be_bytes([buf[4], buf[5]]) as usize;
        let an = u16::from_be_bytes([buf[6], buf[7]]) as usize;
        let ns = u16::from_be_bytes([buf[8], buf[9]]) as usize;
        let ar = u16::from_be_bytes([buf[10], buf[11]]) as usize;

        let mut off = 12usize;
        for _ in 0..qd {
            match read_name(buf, off) {
                Ok((_, next)) => {
                    off = next + 4; // QTYPE + QCLASS
                }
                Err(_) => return,
            }
        }
        let total = an + ns + ar;
        for _ in 0..total {
            match self.absorb_record(buf, off) {
                Ok(next) => off = next,
                Err(_) => return,
            }
        }
    }

    fn absorb_record(&mut self, buf: &[u8], off: usize) -> Result<usize> {
        let (owner, mut p) = read_name(buf, off)?;
        if p + 10 > buf.len() {
            anyhow::bail!("truncated record header");
        }
        let rtype = u16::from_be_bytes([buf[p], buf[p + 1]]);
        let rdlen = u16::from_be_bytes([buf[p + 8], buf[p + 9]]) as usize;
        p += 10;
        if p + rdlen > buf.len() {
            anyhow::bail!("truncated rdata");
        }
        self.insert(owner, Record { rtype, rdata: buf[p..p + rdlen].to_vec() });
        Ok(p + rdlen)
    }
}

/// Read a possibly-compressed DNS name starting at `off`.
fn read_name(buf: &[u8], off: usize) -> Result<(String, usize)> {
    let mut parts: Vec<String> = Vec::new();
    let mut p = off;
    let mut end = off;
    let mut hops = 0usize;
    loop {
        if p >= buf.len() {
            anyhow::bail!("name out of bounds");
        }
        let len = buf[p];
        if len & 0xC0 == 0xC0 {
            if p + 1 >= buf.len() {
                anyhow::bail!("bad pointer");
            }
            let ptr = (u16::from_be_bytes([buf[p], buf[p + 1]]) & 0x3FFF) as usize;
            if end == off {
                end = p + 2;
            }
            hops += 1;
            if hops > 16 {
                anyhow::bail!("compression loop");
            }
            p = ptr;
            continue;
        }
        if len == 0 {
            if end == off {
                end = p + 1;
            }
            break;
        }
        p += 1;
        if p + len as usize > buf.len() {
            anyhow::bail!("label out of bounds");
        }
        parts.push(String::from_utf8_lossy(&buf[p..p + len as usize]).to_string());
        p += len as usize;
    }
    Ok((format!("{}.", parts.join(".")), end))
}
