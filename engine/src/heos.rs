//! Denon HEOS / Marantz — Telnet CLI on port 1255.
//!
//! HEOS is the one protocol here that does **real** multi-device sync: the
//! device itself keeps a group in sample-accurate lock-step, so
//! [`Sink::join_group`] returns `Ok(true)` and the engine stops doing software
//! drift correction.
//!
//! Discovery is SSDP (`urn:schemas-denon-com:device:ACT-Denon:1`); control is
//! a plaintext `heos://` command per line, answered with one JSON object. The
//! connection also carries unsolicited `event/*` messages, which is where our
//! status callbacks come from.
//!
//! Audio is pulled from the gateway URL (`player/play_stream`), so HEOS also
//! gets the untouched original file.

use crate::hub::Bus;
use crate::model::{Caps, Device, Event, Protocol, Status, Track, TransportState, device_id};
use crate::ssdp;
use crate::traits::{Adapter, ConnectCtx, Sink};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HEOS_PORT: u16 = 1255;
/// Speakers drop an idle telnet session after a while, so keep it warm.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

/// The HEOS adapter.
pub struct HeosAdapter;

impl Adapter for HeosAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::Heos
    }

    /// One telnet connection lists *every* player on the network, so a single
    /// SSDP answer is enough to enumerate them all.
    fn discover(&self, ctx: &ConnectCtx, timeout: Duration) -> Result<Vec<Device>> {
        let hits = ssdp::search(ctx.local_addr, &[ssdp::ST_HEOS], timeout.as_secs().max(2))?;
        let mut out = Vec::new();
        for hit in hits {
            let Ok(conn) = HeosConn::connect(hit.from, Bus::new(), &device_id(Protocol::Heos, &hit.from.to_string())) else {
                continue;
            };
            let players = match conn.players() {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("heos: get_players on {} failed: {e}", hit.from);
                    continue;
                }
            };
            for p in players {
                out.push(player_to_device(&p, hit.from));
            }
            if !out.is_empty() {
                break; // one speaker already told us about all of them
            }
        }
        Ok(out)
    }
}

/// One player (a HEOS speaker, or a zone) as reported by `get_players`.
#[derive(Clone, Debug)]
pub struct HeosPlayer {
    pub pid: String,
    pub name: String,
    pub model: String,
    pub ip: IpAddr,
    pub group: Option<String>,
}

fn player_to_device(p: &HeosPlayer, _fallback_ip: IpAddr) -> Device {
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("pid".into(), p.pid.clone());
    extra.insert("ip".into(), p.ip.to_string());
    if let Some(g) = &p.group {
        extra.insert("gid".into(), g.clone());
    }
    let caps = Caps {
        pause: true,
        // The HEOS CLI has no absolute seek command for a URL stream.
        seek: false,
        volume: true,
        mute: true,
        native_group: true,
        lossless_passthrough: true,
        status_push: true,
    };
    Device {
        id: device_id(Protocol::Heos, &p.pid),
        protocol: Protocol::Heos,
        name: p.name.clone(),
        model: Some(p.model.clone()),
        manufacturer: Some("Denon".to_string()),
        addr: p.ip,
        port: HEOS_PORT,
        caps,
        extra,
    }
}

/// A telnet session with one HEOS speaker.
/// The live half of a telnet session. Swapped wholesale on reconnect so the
/// socket and its response channel never end up mismatched.
struct Link {
    writer: TcpStream,
    /// `std::sync::mpsc::Receiver` is not `Sync`, and the connection is shared
    /// through an `Arc`, so it has to sit behind a mutex.
    resp_rx: Receiver<Value>,
}

struct HeosConn {
    ip: IpAddr,
    bus: Bus,
    dev_id: String,
    link: Mutex<Link>,
    status: Arc<Mutex<Status>>,
}

impl HeosConn {
    /// Open the telnet session. `bus` receives every unsolicited event; pass a
    /// throwaway one when you only need to enumerate players.
    fn connect(ip: IpAddr, bus: Bus, dev_id: &str) -> Result<Arc<Self>> {
        let conn = Arc::new(Self {
            ip,
            bus,
            dev_id: dev_id.to_string(),
            // Replaced immediately by `open_link`; a real socket is needed for
            // the type, and `open_link` cannot fail after this point.
            link: Mutex::new(Link {
                writer: TcpStream::connect((ip, HEOS_PORT))?,
                resp_rx: channel::<Value>().1,
            }),
            status: Arc::new(Mutex::new(Status::default())),
        });
        conn.reopen()?;
        Ok(conn)
    }

    /// (Re)open the socket and restart the reader thread.
    ///
    /// Speakers drop an idle telnet session, and if the reader thread exits the
    /// sink would otherwise be wedged forever — every later command timing out.
    fn reopen(&self) -> Result<()> {
        let stream = TcpStream::connect((self.ip, HEOS_PORT))
            .with_context(|| format!("HEOS telnet {}:{}", self.ip, HEOS_PORT))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_nodelay(true)?;
        let (resp_tx, resp_rx) = channel::<Value>();

        let reader = BufReader::new(stream.try_clone()?);
        let bus = self.bus.clone();
        let status = Arc::clone(&self.status);
        let id = self.dev_id.clone();
        std::thread::Builder::new()
            .name(format!("heos-{}", id))
            .spawn(move || {
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim().to_string();
                    if line.is_empty() || !line.starts_with('{') {
                        continue;
                    }
                    let Ok(v): Result<Value, _> = serde_json::from_str(&line) else { continue };
                    let cmd = v.pointer("/heos/command").and_then(Value::as_str).unwrap_or("").to_string();
                    if cmd.starts_with("event/") {
                        let msg = v.pointer("/heos/message").and_then(Value::as_str).unwrap_or("");
                        handle_event(&bus, &status, &id, &cmd, msg);
                    } else {
                        let _ = resp_tx.send(v);
                    }
                }
            })?;

        *self.link.lock().unwrap_or_else(|e| e.into_inner()) =
            Link { writer: stream, resp_rx };
        Ok(())
    }

    /// Send one command and wait for its answer, reconnecting once if the
    /// session has gone stale.
    fn command(&self, cmd: &str, timeout: Duration) -> Result<Value> {
        let base = cmd.strip_prefix("heos://").unwrap_or(cmd);
        let base = base.split('?').next().unwrap_or(base).to_string();
        match self.try_command(cmd, &base, timeout) {
            Ok(v) => Ok(v),
            Err(e) => {
                log::warn!("HEOS {base}: {e:#} — reopening the telnet session");
                self.reopen().with_context(|| format!("HEOS reconnect to {}", self.ip))?;
                // The change-event subscription dies with the old socket.
                let _ = self.try_command(
                    "heos://system/register_for_change_events?enable=on",
                    "system/register_for_change_events",
                    Duration::from_secs(5),
                );
                self.try_command(cmd, &base, timeout)
            }
        }
    }

    /// One attempt, no retry.
    fn try_command(&self, cmd: &str, base: &str, timeout: Duration) -> Result<Value> {
        {
            let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
            link.writer.write_all(format!("{cmd}\r\n").as_bytes())?;
            link.writer.flush()?;
        }
        let deadline = Instant::now() + timeout;
        let link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        let rx = &link.resp_rx;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("HEOS {base}: timeout");
            }
            match rx.recv_timeout(remaining) {
                Ok(v) => {
                    let c = v.pointer("/heos/command").and_then(Value::as_str).unwrap_or("");
                    if c != base {
                        // A late answer to an earlier command — ignore it.
                        continue;
                    }
                    let result = v.pointer("/heos/result").and_then(Value::as_str).unwrap_or("");
                    if result != "success" {
                        let msg = v.pointer("/heos/message").and_then(Value::as_str).unwrap_or("");
                        bail!("HEOS {base}: {msg}");
                    }
                    return Ok(v);
                }
                Err(_) => bail!("HEOS {base}: timeout"),
            }
        }
    }

    fn players(&self) -> Result<Vec<HeosPlayer>> {
        let v = self.command("heos://player/get_players", Duration::from_secs(6))?;
        let Some(payload) = v.get("payload").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        Ok(payload
            .iter()
            .filter_map(|p| {
                let pid = p.get("pid").map(stringify)?;
                if pid.is_empty() {
                    return None;
                }
                let ip: IpAddr = p
                    .get("ip")
                    .map(stringify)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(self.ip);
                Some(HeosPlayer {
                    pid,
                    name: p.get("name").map(stringify).unwrap_or_else(|| "HEOS".to_string()),
                    model: p.get("model").map(stringify).unwrap_or_default(),
                    ip,
                    group: p.get("gid").map(stringify).filter(|s| !s.is_empty()),
                })
            })
            .collect())
    }
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_kv(msg: &str) -> Vec<(String, String)> {
    msg.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Translate an unsolicited `event/*` message into an engine [`Event`].
fn handle_event(bus: &Bus, status: &Arc<Mutex<Status>>, id: &str, cmd: &str, msg: &str) {
    let kv = parse_kv(msg);
    let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).unwrap_or("");
    if let Ok(mut s) = status.lock() {
        match cmd {
            "event/player_state_changed" => {
                s.state = match get("state") {
                    "play" => TransportState::Playing,
                    "pause" => TransportState::Paused,
                    "stop" => TransportState::Stopped,
                    _ => TransportState::Idle,
                };
            }
            "event/player_volume_changed" => {
                s.volume = get("level").parse::<u8>().ok();
                s.muted = get("mute") == "on";
            }
            // The only place a URL stream's position ever comes from: HEOS has
            // no query for it, it just pushes `cur_pos`/`duration` while
            // playing. Without this the UI shows a track stuck at 0:00.
            "event/player_now_playing_progress" => {
                if let Some(p) = get("cur_pos").parse::<u64>().ok() {
                    s.position_ms = p;
                }
                if let Some(d) = get("duration").parse::<u64>().ok() {
                    s.duration_ms = d;
                }
            }
            _ => {}
        }
        bus.push(Event::Status { id: id.to_string(), status: *s });
    }
}

/// A live HEOS session.
pub struct HeosSink {
    dev: Device,
    pid: String,
    /// Once grouped, commands go to the group leader's pid.
    leader: String,
    conn: Arc<HeosConn>,
    status: Arc<Mutex<Status>>,
    current: Option<Track>,
    /// Last telnet heartbeat (`system/heart_beat`).
    last_beat: Instant,
    /// What `load` was asked for but not started yet — see `play`.
    pending: Option<(Track, String)>,
    /// Whether a URL stream is live on the player right now.
    ///
    /// `play` has to know the difference: issuing `play_stream` again would
    /// restart the track from zero instead of resuming it.
    streaming: bool,
}

impl HeosAdapter {
    /// Open a session to `dev`.
    pub fn open(_ctx: &ConnectCtx, dev: &Device, bus: Bus) -> Result<HeosSink> {
        let pid = dev.extra("pid").context("HEOS device has no pid")?.to_string();
        let ip = dev.extra("ip").and_then(|s| s.parse().ok()).unwrap_or(dev.addr);
        let conn = HeosConn::connect(ip, bus.clone(), &dev.id)?;
        // Unsolicited change events are what gives us status callbacks.
        let _ = conn.command("heos://system/register_for_change_events?enable=on", Duration::from_secs(5));
        let status = conn.status.clone();
        Ok(HeosSink {
            dev: dev.clone(),
            pid: pid.clone(),
            leader: pid,
            conn,
            status,
            current: None,
            last_beat: Instant::now(),
            pending: None,
            streaming: false,
        })
    }
}

impl Sink for HeosSink {
    fn protocol(&self) -> Protocol {
        Protocol::Heos
    }

    fn load(&mut self, track: &Track, url: &str) -> Result<()> {
        // HEOS has no "queue this URL" command — `play_stream` both loads and
        // starts. Pausing right after it used to be how we staged a track, but
        // a paused URL stream is dropped outright: the player reports
        // `state=stop` and `set_play_state=play` cannot bring it back. So load
        // only remembers the track, and `play` is what actually starts it.
        self.current = Some(track.clone());
        self.pending = Some((track.clone(), url.to_string()));
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        if let Some((_track, url)) = self.pending.take() {
            self.conn.command(
                &format!(
                    "heos://player/play_stream?pid={}&url={}",
                    self.leader,
                    urlencode(&url)
                ),
                Duration::from_secs(10),
            )?;
            self.streaming = true;
        } else if self.streaming {
            // Already streaming: resume rather than restart from zero.
            self.conn.command(
                &format!("heos://player/set_play_state?pid={}&state=play", self.leader),
                Duration::from_secs(5),
            )?;
        } else {
            // Nothing queued and nothing live — leave the player as it is.
            log::debug!("HEOS {}: play() with nothing loaded", self.dev.id);
        }
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Playing;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.conn.command(
            &format!("heos://player/set_play_state?pid={}&state=pause", self.leader),
            Duration::from_secs(5),
        )?;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Paused;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.conn.command(
            &format!("heos://player/set_play_state?pid={}&state=stop", self.leader),
            Duration::from_secs(5),
        )?;
        // The stream is gone: a queued track must not be started by a later
        // `play`, and the next `play` must issue a fresh `play_stream`.
        self.pending = None;
        self.streaming = false;
        if let Ok(mut s) = self.status.lock() {
            s.state = TransportState::Stopped;
        }
        Ok(())
    }

    fn set_volume(&mut self, volume: u8) -> Result<()> {
        self.conn.command(
            &format!("heos://player/set_volume?pid={}&level={}", self.leader, volume.min(100)),
            Duration::from_secs(5),
        )?;
        if let Ok(mut s) = self.status.lock() {
            s.volume = Some(volume);
        }
        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<()> {
        let state = if muted { "on" } else { "off" };
        self.conn.command(
            &format!("heos://player/set_mute?pid={}&state={state}", self.leader),
            Duration::from_secs(5),
        )?;
        if let Ok(mut s) = self.status.lock() {
            s.muted = muted;
        }
        Ok(())
    }

    fn status(&mut self) -> Result<Status> {
        let mut s = *self.status.lock().unwrap_or_else(|e| e.into_inner());
        let v = self.conn.command(
            &format!("heos://player/get_play_state?pid={}", self.leader),
            Duration::from_secs(5),
        )?;
        if let Some(state) = v.pointer("/payload/state").and_then(Value::as_str) {
            s.state = match state {
                "play" => TransportState::Playing,
                "pause" => TransportState::Paused,
                "stop" => TransportState::Stopped,
                _ => TransportState::Idle,
            };
        }
        if let Ok(vv) = self.conn.command(
            &format!("heos://player/get_volume?pid={}", self.leader),
            Duration::from_secs(5),
        ) {
            let msg = vv.pointer("/heos/message").and_then(Value::as_str).unwrap_or("");
            for (k, val) in parse_kv(msg) {
                if k == "level" {
                    s.volume = val.parse::<u8>().ok();
                } else if k == "mute" {
                    s.muted = val == "on";
                }
            }
        }
        if let Some(t) = &self.current {
            s.duration_ms = t.duration_ms;
        }
        Ok(s)
    }

    fn caps(&self) -> Caps {
        self.dev.caps
    }

    /// Native grouping — this is HEOS's own synchronised playback.
    fn join_group(&mut self, members: &[Device]) -> Result<bool> {
        let mut pids: Vec<String> = members
            .iter()
            .filter(|d| d.protocol == Protocol::Heos)
            .filter_map(|d| d.extra("pid").map(str::to_string))
            .collect();
        if !pids.contains(&self.pid) {
            pids.insert(0, self.pid.clone());
        }
        if pids.len() < 2 {
            return Ok(false);
        }
        self.conn.command(
            &format!("heos://group/set_group?pid={}", pids.join(",")),
            Duration::from_secs(8),
        )?;
        // The first pid becomes the group leader; every command from now on
        // targets it, which is what keeps the group in lock-step.
        self.leader = pids.into_iter().next().unwrap_or_else(|| self.pid.clone());
        Ok(true)
    }

    fn leave_group(&mut self) -> Result<()> {
        self.conn.command(
            &format!("heos://group/ungroup?pid={}", self.leader),
            Duration::from_secs(8),
        )?;
        self.leader = self.pid.clone();
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        // Speakers drop an idle telnet session. `command()` reopens it on
        // failure, but a heartbeat keeps it from going stale in the first place.
        if self.last_beat.elapsed() >= HEARTBEAT_EVERY {
            self.last_beat = Instant::now();
            let _ = self.conn.command("heos://system/heart_beat", Duration::from_secs(5));
        }
        Ok(())
    }
}
