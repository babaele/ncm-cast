//! AirPlay 1 / RAOP.
//!
//! **Scope, stated plainly:** AirPlay *2* is not implementable by a third
//! party — it needs Apple's private pairing flow and, for hardware targets, MFi
//! authentication. What this module implements is **AirPlay 1 (RAOP)**, which
//! every pre-2018 AirPort Express, most AirPlay 1 speakers and a fair number of
//! AV receivers still accept, and which needs no pairing at all.
//!
//! Flow: mDNS `_raop._tcp` → RTSP (`OPTIONS`/`ANNOUNCE`/`SETUP`/`RECORD`) →
//! RTP/UDP carrying ALAC. The AES key is wrapped with the device's RSA public
//! key (OAEP-SHA1) from the TXT record.
//!
//! **Quality:** RAOP tops out at 16-bit / 44.1 kHz ALAC, so unlike the
//! pull-based protocols this one does re-encode — ALAC is lossless, but it is
//! not a bit-exact passthrough, hence `lossless_passthrough: false`.

use crate::hub::Bus;
use crate::mdns::{self, SvcInstance};
use crate::model::{Caps, Device, Event, Protocol, Status, Track, TransportState, device_id};
use crate::netease::NeteaseClient;
use crate::traits::{Adapter, ConnectCtx, Sink};
use alac_encoder::{AlacEncoder, FormatDescription};
use anyhow::{Context, Result, anyhow, bail};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// `_raop._tcp.local.`
pub const RAOP_SERVICE: &str = "_raop._tcp.local.";
/// The dynamic payload type Apple uses for ALAC in RAOP.
const RTP_PT_ALAC: u8 = 96;
/// Samples per ALAC packet. 352 is what every real sender uses.
const FRAMES_PER_PACKET: usize = 352;
/// RAOP runs at CD rate.
const TARGET_RATE: u32 = 44_100;

/// The AirPlay 1 adapter.
pub struct AirPlayAdapter;

impl Adapter for AirPlayAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::AirPlay
    }

    fn discover(&self, _ctx: &ConnectCtx, timeout: Duration) -> Result<Vec<Device>> {
        let found = mdns::query(RAOP_SERVICE, timeout)?;
        Ok(found.into_iter().filter_map(instance_to_device).collect())
    }
}

fn instance_to_device(s: SvcInstance) -> Option<Device> {
    let addr = s.addr()?;
    let mut extra = std::collections::BTreeMap::new();
    for (k, v) in &s.txt {
        extra.insert(format!("txt.{k}"), v.clone());
    }
    extra.insert("host".into(), s.host.clone());
    extra.insert("fqdn".into(), s.fqdn.clone());
    let model = s.txt.get("am").or_else(|| s.txt.get("md")).cloned();
    let caps = Caps {
        pause: true,
        seek: false,
        // AirPlay 1 volume is a `SET_PARAMETER` away and is in dB, not percent.
        volume: true,
        mute: false,
        native_group: false,
        lossless_passthrough: false,
        status_push: false,
    };
    Some(Device {
        id: device_id(Protocol::AirPlay, &format!("{}@{}", s.name, s.host)),
        protocol: Protocol::AirPlay,
        name: s.name,
        model,
        manufacturer: Some("Apple AirPlay".to_string()),
        addr,
        port: s.port,
        caps,
        extra,
    })
}

/// One RAOP session.
pub struct AirPlaySink {
    dev: Device,
    bus: Bus,
    client: NeteaseClient,
    /// Our own LAN address — the device streams RTP back to it.
    local: IpAddr,
    /// RTSP control connection
    rtsp: Option<std::net::TcpStream>,
    session_id: String,
    client_instance: String,
    session_cookie: Option<String>,
    server_port: u16,
    udp: Option<UdpSocket>,
    aes_key: [u8; 16],
    aes_iv: [u8; 16],
    seq: u16,
    rtp_time: u32,
    ssrc: u32,
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    track: Option<Track>,
    url: Option<String>,
    /// Where a paused session resumes, in samples.
    resume_samples: u64,
    cseq: u32,
}

impl AirPlayAdapter {
    /// Open a session (no network traffic yet beyond the initial RTSP socket).
    pub fn open(ctx: &ConnectCtx, dev: &Device, bus: Bus, client: NeteaseClient) -> Result<AirPlaySink> {
        let rtsp = std::net::TcpStream::connect((dev.addr, dev.port))
            .with_context(|| format!("RAOP connect {}:{}", dev.addr, dev.port))?;
        rtsp.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(AirPlaySink {
            dev: dev.clone(),
            bus,
            client,
            local: ctx.local_addr,
            rtsp: Some(rtsp),
            session_id: random_hex(16),
            client_instance: random_hex(16),
            session_cookie: None,
            server_port: 0,
            udp: None,
            aes_key: random_bytes(),
            aes_iv: random_bytes(),
            seq: 0,
            rtp_time: 0,
            ssrc: u32::from_be_bytes(random_bytes::<4>()),
            status: Arc::new(Mutex::new(Status::default())),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            track: None,
            url: None,
            resume_samples: 0,
            cseq: 0,
        })
    }
}

impl AirPlaySink {
    fn rtsp(&self) -> Result<&std::net::TcpStream> {
        self.rtsp.as_ref().context("RAOP session closed")
    }

    /// Send one RTSP request and read the whole response head.
    fn rtsp_request(&mut self, method: &str, uri: &str, headers: &[(&str, String)], body: Option<&str>) -> Result<(u16, Vec<(String, String)>)> {
        self.cseq += 1;
        let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        req.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
        if let Some(cookie) = &self.session_cookie {
            req.push_str(&format!("Session: {cookie}\r\n"));
        }
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }
        use std::io::Write;
        let mut sock = self.rtsp()?;
        sock.write_all(req.as_bytes())?;
        sock.flush()?;

        // Read headers until the blank line.
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match sock.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    buf.push(byte[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        let mut lines = text.lines();
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let mut out = Vec::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        Ok((status, out))
    }

    fn get(&mut self, h: &[(String, String)], k: &str) -> Option<String> {
        h.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone())
    }

    /// `OPTIONS` → `ANNOUNCE` → `SETUP`, then remember the server's RTP port.
    fn setup(&mut self, local: IpAddr) -> Result<()> {
        let ip = match local {
            IpAddr::V4(v) => v,
            IpAddr::V6(_) => bail!("RAOP needs an IPv4 source address"),
        };
        self.rtsp_request("OPTIONS", "*", &[], None)?;

        let sdp = self.build_sdp(&ip)?;
        let (st, _) = self.rtsp_request(
            "ANNOUNCE",
            &format!("rtsp://{}/{}", ip, self.session_id),
            &[("Content-Type", "application/sdp".to_string())],
            Some(&sdp),
        )?;
        if st != 200 {
            bail!("RAOP ANNOUNCE rejected: {st}");
        }

        let (st, h) = self.rtsp_request(
            "SETUP",
            &format!("rtsp://{}/{}", ip, self.session_id),
            &[(
                "Transport",
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record".to_string(),
            )],
            None,
        )?;
        if st != 200 {
            bail!("RAOP SETUP rejected: {st}");
        }
        let transport = self.get(&h, "transport").unwrap_or_default();
        self.server_port = transport
            .split(';')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == "server_port")
            .and_then(|(_, v)| v.parse().ok())
            .context("RAOP SETUP answer has no server_port")?;
        self.session_cookie = self.get(&h, "session");
        let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        udp.connect(SocketAddr::new(self.dev.addr, self.server_port))?;
        self.udp = Some(udp);
        Ok(())
    }

    /// Build the SDP body, including the RSA-wrapped AES key.
    fn build_sdp(&self, local: &Ipv4Addr) -> Result<String> {
        let wrapped = self.dev.extra("txt.pk").filter(|s| !s.is_empty()).and_then(|pk| rsa_wrap(pk, &self.aes_key).ok());
        let Some(wrapped) = wrapped else {
            bail!("device advertises no usable RSA public key (txt.pk) — cannot encrypt the stream");
        };
        let key_b64 = base64_encode(&wrapped);
        let iv_b64 = base64_encode(&self.aes_iv);
        Ok(format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 {local}\r\n\
             s=NCM Cast\r\n\
             c=IN IP4 {local}\r\n\
             t=0 0\r\n\
             m=audio 0 RTP/AVP {pt}\r\n\
             a=rtpmap:{pt} AppleLossless\r\n\
             a=fmtp:{pt} {frames} 0 {bits} 40 10 14 {ch} 255 0 0 {rate}\r\n\
             a=rsaaeskey:{key_b64}\r\n\
             a=aesiv:{iv_b64}\r\n",
            pt = RTP_PT_ALAC,
            frames = FRAMES_PER_PACKET,
            bits = 16,
            ch = 2,
            rate = TARGET_RATE,
        ))
    }

    /// Start the RTP stream.
    fn start_stream(&mut self, local: IpAddr) -> Result<()> {
        if self.udp.is_none() {
            self.setup(local)?;
        }
        let ip = match local {
            IpAddr::V4(v) => v,
            IpAddr::V6(_) => bail!("RAOP needs an IPv4 source address"),
        };
        let (st, _) = self.rtsp_request(
            "RECORD",
            &format!("rtsp://{}/{}", ip, self.session_id),
            &[
                ("Range", "ntp=0-".to_string()),
                ("RTP-Info", format!("seq={};rtptime={}", self.seq, self.rtp_time)),
            ],
            None,
        )?;
        if st != 200 {
            bail!("RAOP RECORD rejected: {st}");
        }
        Ok(())
    }

    /// One RTP packet with the ALAC payload encrypted by AES-128-CBC.
    ///
    /// The streaming thread builds its packets inline (it owns cloned state),
    /// but this is the same framing and is handy for callers that drive RAOP
    /// themselves.
    #[allow(dead_code)]
    fn send_packet(&mut self, alac: &[u8], marker: bool) -> Result<()> {
        let ct = crate::crypto::aes128_cbc_encrypt(&self.aes_key, &self.aes_iv, alac);
        let mut pkt = Vec::with_capacity(12 + ct.len());
        pkt.push(0x80);
        pkt.push(if marker { 0x80 | RTP_PT_ALAC } else { RTP_PT_ALAC });
        pkt.extend_from_slice(&self.seq.to_be_bytes());
        pkt.extend_from_slice(&self.rtp_time.to_be_bytes());
        pkt.extend_from_slice(&self.ssrc.to_be_bytes());
        pkt.extend_from_slice(&ct);
        if let Some(udp) = &self.udp {
            udp.send(&pkt)?;
        }
        self.seq = self.seq.wrapping_add(1);
        self.rtp_time = self.rtp_time.wrapping_add(FRAMES_PER_PACKET as u32);
        Ok(())
    }
}

impl Sink for AirPlaySink {
    fn protocol(&self) -> Protocol {
        Protocol::AirPlay
    }

    fn load(&mut self, track: &Track, url: &str) -> Result<()> {
        // A different track invalidates any running stream.
        self.stop_stream()?;
        self.track = Some(track.clone());
        self.url = Some(url.to_string());
        self.resume_samples = 0;
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        let Some(track) = self.track.clone() else { bail!("nothing loaded") };
        let local = self.local;
        self.stop.store(false, Ordering::SeqCst);
        self.start_stream(local)?;

        let client = self.client.clone();
        let stop = Arc::clone(&self.stop);
        let status = Arc::clone(&self.status);
        let bus = self.bus.clone();
        let id = self.dev.id.clone();
        let resume = self.resume_samples;
        let url = self.url.clone().unwrap_or_default();

        let udp = self.udp.as_ref().map(|u| u.try_clone()).transpose()?;
        let key = self.aes_key;
        let iv = self.aes_iv;
        let ssrc = self.ssrc;
        let mut seq = self.seq;
        let mut rtp_time = self.rtp_time;
        let dev_addr = self.dev.addr;
        let server_port = self.server_port;

        let handle = thread::Builder::new().name("raop-stream".into()).spawn(move || {
            if let Err(e) = stream_loop(&client, &track, &url, udp.as_ref(), dev_addr, server_port, key, iv, ssrc, &mut seq, &mut rtp_time, resume, &stop, &status, &bus, &id) {
                log::error!("airplay: {e:#}");
                bus.push(Event::Error { id: Some(id.clone()), message: format!("{e:#}") });
            }
        })?;
        self.worker = Some(handle);
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Playing;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        // Remember where we are, then tear the RTP stream down.
        if let Ok(s) = self.status.lock() {
            self.resume_samples = s.position_ms as u64 * TARGET_RATE as u64 / 1000;
        }
        self.stop_stream()?;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Paused;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.resume_samples = 0;
        self.stop_stream()?;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Stopped;
            s.position_ms = 0;
        }
        Ok(())
    }

    fn set_volume(&mut self, volume: u8) -> Result<()> {
        // RAOP volume is dBFS: -144 (silent) … 0 (full scale).
        let v = if volume == 0 { -144.0 } else { -30.0 + (volume as f32 / 100.0) * 30.0 };
        let body = format!("volume: {v:.2}\r\n");
        let (st, _) = self.rtsp_request(
            "SET_PARAMETER",
            &format!("rtsp://{}/{}", self.local, self.session_id),
            &[("Content-Type", "text/parameters".to_string())],
            Some(&body),
        )?;
        if st != 200 {
            bail!("RAOP SET_PARAMETER rejected: {st}");
        }
        if let Ok(mut s) = self.status.lock() {
            s.volume = Some(volume);
        }
        Ok(())
    }

    fn status(&mut self) -> Result<Status> {
        let s = *self.status.lock().unwrap_or_else(|e| e.into_inner());
        Ok(s)
    }

    fn caps(&self) -> Caps {
        self.dev.caps
    }

    fn tick(&mut self) -> Result<()> {
        // RAOP has no subscription to renew; RTP flowing is the keep-alive.
        Ok(())
    }
}

impl AirPlaySink {
    /// Stop the worker and send TEARDOWN.
    fn stop_stream(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        if self.udp.is_some() {
            let _ = self.rtsp_request("TEARDOWN", &format!("rtsp://{}/{}", self.local, self.session_id), &[], None);
        }
        self.udp = None;
        self.session_cookie = None;
        Ok(())
    }
}

impl Drop for AirPlaySink {
    fn drop(&mut self) {
        let _ = self.stop_stream();
    }
}

/// Decode → resample → ALAC → encrypt → RTP, paced in real time.
#[allow(clippy::too_many_arguments)]
fn stream_loop(
    client: &NeteaseClient,
    track: &Track,
    url: &str,
    udp: Option<&UdpSocket>,
    dev_addr: IpAddr,
    server_port: u16,
    key: [u8; 16],
    iv: [u8; 16],
    ssrc: u32,
    seq: &mut u16,
    rtp_time: &mut u32,
    resume_samples: u64,
    stop: &AtomicBool,
    status: &Arc<Mutex<Status>>,
    bus: &Bus,
    id: &str,
) -> Result<()> {
    // AirPlay pushes, so unlike the pull protocols we go to the CDN directly
    // instead of looping the bytes back through our own gateway.
    let song = client.song_url(track.id, track.quality)?.context("no playable URL for this track")?;
    let _ = url;
    let cdn_url = song.url.clone();
    let resp = client.open_cdn(&cdn_url, None, false)?;
    let body = resp.into_body().into_reader();
    let Some(udp) = udp else { bail!("no RTP socket") };
    let dest = SocketAddr::new(dev_addr, server_port);

    if song.ext != "flac" {
        bail!("AirPlay 1 needs a FLAC source (got {}); use a DLNA/HEOS target for MP3", song.ext);
    }

    let mut decoder = claxon::FlacReader::new(body).context("cannot decode the FLAC stream")?;
    let (src_rate, channels, bits) = {
        let si = decoder.streaminfo();
        (si.sample_rate, si.channels.max(1) as usize, si.bits_per_sample.max(8))
    };

    let input = FormatDescription::pcm::<i16>(TARGET_RATE as f64, 2);
    let output = FormatDescription::alac(TARGET_RATE as f64, FRAMES_PER_PACKET as u32, 2);
    let mut encoder = AlacEncoder::new(&output);
    let mut packet_buf = vec![0u8; output.max_packet_size()];

    let mut pcm: Vec<i16> = Vec::with_capacity(FRAMES_PER_PACKET * 2 * 4);
    let mut resample_acc: u64 = 0;
    let mut skipped = 0u64;
    let mut sent_samples = resume_samples;
    let started = Instant::now();
    let mut sent_any = false;

    let mut frames = decoder.blocks();
    let mut buffer: Vec<i32> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let block = match frames.read_next_or_eof(buffer) {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => return Err(anyhow!("FLAC decode: {e}")),
        };
        let n = block.len() as usize;
        // Copy the channel planes out before handing the buffer back for reuse.
        let left_ch = block.channel(0).to_vec();
        let right_ch = if channels >= 2 { block.channel(1).to_vec() } else { block.channel(0).to_vec() };
        buffer = block.into_buffer();
        for i in 0..n {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let left = to_i16(left_ch[i], bits);
            let right = if channels >= 2 { to_i16(right_ch[i], bits) } else { left };
            if skipped < resume_samples {
                skipped += 1;
                continue;
            }
            if src_rate != TARGET_RATE {
                push_resampled(&mut pcm, left, right, src_rate, &mut resample_acc);
            } else {
                pcm.push(left);
                pcm.push(right);
            }
            while pcm.len() >= FRAMES_PER_PACKET * 2 {
                let chunk: Vec<u8> = pcm
                    .drain(..FRAMES_PER_PACKET * 2)
                    .flat_map(|s| s.to_le_bytes())
                    .collect();
                let size = encoder.encode(&input, &chunk, &mut packet_buf);
                let ct = crate::crypto::aes128_cbc_encrypt(&key, &iv, &packet_buf[..size]);

                let mut pkt = Vec::with_capacity(12 + ct.len());
                pkt.push(0x80);
                pkt.push(if sent_any { RTP_PT_ALAC } else { 0x80 | RTP_PT_ALAC });
                pkt.extend_from_slice(&seq.to_be_bytes());
                pkt.extend_from_slice(&rtp_time.to_be_bytes());
                pkt.extend_from_slice(&ssrc.to_be_bytes());
                pkt.extend_from_slice(&ct);
                udp.send_to(&pkt, dest)?;
                *seq = seq.wrapping_add(1);
                *rtp_time = rtp_time.wrapping_add(FRAMES_PER_PACKET as u32);
                sent_any = true;
                sent_samples += FRAMES_PER_PACKET as u64;

                if let Ok(mut s) = status.lock() {
                    s.position_ms = sent_samples * 1000 / TARGET_RATE as u64;
                    s.duration_ms = track.duration_ms;
                }
                bus.push(Event::Status {
                    id: id.to_string(),
                    status: *status.lock().unwrap_or_else(|e| e.into_inner()),
                });

                // Pace to real time; ALAC packets are ~8 ms of audio.
                let target = Duration::from_nanos(sent_samples * 1_000_000_000 / TARGET_RATE as u64);
                let elapsed = started.elapsed();
                if target > elapsed {
                    std::thread::sleep(target - elapsed);
                }
            }
        }
    }
    if !stop.load(Ordering::SeqCst) {
        bus.push(Event::EndOfTrack { id: id.to_string() });
    }
    Ok(())
}

/// Nearest-neighbour rate conversion of a stereo pair into `pcm`.
///
/// AirPlay 1 is fixed at 44.1 kHz while NetEase FLAC is usually 44.1 or 48 kHz,
/// so this is inaudible in practice and avoids a resampler dependency.
fn push_resampled(pcm: &mut Vec<i16>, left: i16, right: i16, src_rate: u32, acc: &mut u64) {
    *acc += TARGET_RATE as u64;
    while *acc >= src_rate as u64 {
        *acc -= src_rate as u64;
        pcm.push(left);
        pcm.push(right);
    }
}

fn to_i16(sample: i32, bits: u32) -> i16 {
    if bits >= 16 {
        // Truncate the low bits; 24-bit sources lose nothing audible.
        let shift = bits - 16;
        #[allow(clippy::cast_possible_truncation)]
        let v = (sample >> shift) as i16;
        v
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let v = (sample << (16 - bits)) as i16;
        v
    }
}

/// Wrap the AES key with the device's RSA public key (PKCS#1 DER in `txt.pk`).
fn rsa_wrap(pk_b64: &str, aes_key: &[u8; 16]) -> Result<Vec<u8>> {
    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD.decode(pk_b64.trim())?;
    // Most devices publish PKCS#1 DER; a few wrap the same key in SPKI instead.
    let pubkey: rsa::RsaPublicKey = match rsa::pkcs1::DecodeRsaPublicKey::from_pkcs1_der(&der) {
        Ok(k) => k,
        Err(_) => rsa::pkcs8::DecodePublicKey::from_public_key_der(&der)
            .map_err(|e| anyhow!("device RSA public key is not decodable: {e}"))?,
    };
    let padding = rsa::Oaep::new::<sha1::Sha1>();
    let mut rng = rsa::rand_core::OsRng;
    Ok(pubkey.encrypt(&mut rng, padding, aes_key)?)
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    for b in &mut out {
        *b = fastrand::u8(..);
    }
    out
}

fn random_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:x}", fastrand::u8(..) % 16)).collect()
}
