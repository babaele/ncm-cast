//! The local HTTP audio gateway — how lossless push actually works.
//!
//! Every protocol that can *pull* (UPnP/DLNA, HEOS, QPlay) is pointed at
//! `http://<lan-ip>:<port>/t/<song-id>.<ext>`. On each request the song id is
//! resolved to a signed NetEase CDN URL and the upstream bytes are piped
//! straight through: **no decoding, no re-encoding, no sample-rate or bit-depth
//! conversion**. A 24/96 FLAC arrives at the renderer bit-identical to what
//! NetEase serves, which is the only honest definition of "无损推送".
//!
//! `Range` is forwarded upstream so renderers can seek, and `HEAD` is answered
//! with headers only — several renderers probe the length before playing.

use crate::netease::{NeteaseClient, SongUrl, URL_TTL};
use crate::model::{Quality, Track};
use anyhow::Result;
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::thread;
use std::time::Instant;
use tiny_http_dh::{Header, Method, Request, Response, Server, StatusCode};

/// Path prefix for tracks. `/netease/` is kept as an alias so existing
/// swyh-rs URLs keep working.
pub const TRACK_PREFIX: &str = "/t/";
pub const TRACK_PREFIX_LEGACY: &str = "/netease/";

/// Resolved CDN URLs with the instant they were resolved.
///
/// NetEase signs its URLs and they expire in roughly 20 minutes, so the cache
/// window is short enough to stay valid and long enough that a renderer's
/// `HEAD` + `GET` pair only costs one API call.
static URL_CACHE: LazyLock<RwLock<HashMap<(u64, Quality), (Instant, SongUrl)>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Forget a cached URL (upstream refused it).
pub fn invalidate(song_id: u64) {
    if let Ok(mut c) = URL_CACHE.write() {
        c.retain(|(id, _), _| *id != song_id);
    }
}

/// The audio gateway.
///
/// Two ways to use it: [`Gateway::start`] runs its own HTTP server (handy when
/// the gateway lives in a separate process), or embed it in a front-end that
/// already owns a port and forward `/t/...` requests to [`Gateway::handle`] —
/// which is what `ncm-castd` does, so everything sits behind one port.
#[derive(Clone)]
pub struct Gateway {
    pub local_addr: IpAddr,
    pub port: u16,
    /// Shared, because `Gateway` is `Clone` and the daemon keeps its own copy:
    /// a QR login must update **every** clone, or `/t/...` would keep resolving
    /// URLs with the old (empty) cookie and silently fall back to low quality.
    client: Arc<Mutex<NeteaseClient>>,
    shutdown: Arc<Mutex<bool>>,
}

impl Gateway {
    /// Describe a gateway without binding anything.
    #[must_use]
    pub fn new(local_addr: IpAddr, port: u16, client: NeteaseClient) -> Self {
        Self {
            local_addr,
            port,
            client: Arc::new(Mutex::new(client)),
            shutdown: Arc::new(Mutex::new(false)),
        }
    }

    /// Bind and spawn the serving thread.
    pub fn start(local_addr: IpAddr, port: u16, client: NeteaseClient) -> Result<Self> {
        let server = Server::http((local_addr, port))
            .map_err(|e| anyhow::anyhow!("gateway: cannot bind {local_addr}:{port}: {e}"))?;
        let real_port = server.server_addr().to_ip().map(|s| s.port()).unwrap_or(port);
        let gw = Self {
            local_addr,
            port: real_port,
            client: Arc::new(Mutex::new(client)),
            shutdown: Arc::new(Mutex::new(false)),
        };
        let flag = Arc::clone(&gw.shutdown);
        let embedded = gw.clone();
        thread::Builder::new()
            .name("cast-gateway".into())
            .spawn(move || {
                while !*flag.lock().unwrap_or_else(|e| e.into_inner()) {
                    match server.recv_timeout(std::time::Duration::from_millis(500)) {
                        Ok(Some(rq)) => embedded.handle(rq),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            })?;
        Ok(gw)
    }

    /// Serve one request. Consumes `rq`.
    pub fn handle(&self, rq: Request) {
        handle(rq, &self.client())
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.local_addr, self.port)
    }

    /// The URL a renderer should be pointed at.
    #[must_use]
    pub fn url_for(&self, track: &Track) -> String {
        format!(
            "{}{}{}.{}?q={}",
            self.base_url(),
            TRACK_PREFIX,
            track.id,
            track.ext(),
            track.quality.level()
        )
    }

    /// Resolve (and cache) the CDN URL for a track.
    pub fn resolve(&self, track: &Track) -> Option<SongUrl> {
        resolve_cached(&self.client(), track.id, track.quality)
    }

    /// Resolve the CDN URL for a track without consulting the cache.
    ///
    /// Direct mode hands this URL straight to the renderer, which fetches it
    /// from NetEase itself — there is no daemon in the middle to detect a
    /// failed fetch and re-relay a fresh one, so a cached (possibly expired)
    /// URL would just leave the renderer stuck on a dead link. Always resolve
    /// fresh there; gateway mode keeps using the cache since the daemon
    /// re-relays and invalidates on failure.
    pub fn resolve_fresh(&self, track: &Track) -> Option<SongUrl> {
        match self.client().song_url(track.id, track.quality) {
            Ok(s) => s,
            Err(e) => {
                log::error!("netease: {e:#}");
                None
            }
        }
    }

    /// A snapshot of the current client. Taking it by value keeps the lock
    /// off the (long) relay path.
    fn client(&self) -> NeteaseClient {
        self.client.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Swap in a client with a fresh cookie (after a QR login).
    /// Visible to every clone of this `Gateway`.
    pub fn set_client(&self, client: NeteaseClient) {
        if let Ok(mut c) = self.client.lock() {
            *c = client;
        }
    }

    pub fn shutdown(&self) {
        if let Ok(mut f) = self.shutdown.lock() {
            *f = true;
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn handle(rq: Request, client: &NeteaseClient) {
    let path = rq.url().to_string();
    let (path, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
    let song_id = parse_track_path(path);
    let Some(id) = song_id else {
        respond_error(rq, 404, "expected /t/<song-id>.<ext>");
        return;
    };
    let head_only = matches!(*rq.method(), Method::Head);
    if !head_only && !matches!(*rq.method(), Method::Get) {
        respond_error(rq, 405, "only GET and HEAD are supported");
        return;
    }
    let quality = parse_quality(query);
    let Some(song) = resolve_cached(client, id, quality) else {
        respond_error(rq, 404, &format!("no playable url for song {id}"));
        return;
    };
    let range = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("range"))
        .map(|h| h.value.as_str().to_string());
    relay(rq, client, &song, range.as_deref(), head_only);
}

/// `/t/1815628781.flac` → `1815628781`. A trailing extension is a hint only;
/// anything else after the digits is rejected so a stray query string can never
/// silently select a different song.
#[must_use]
pub fn parse_track_path(path: &str) -> Option<u64> {
    let rest = path.strip_prefix(TRACK_PREFIX).or_else(|| path.strip_prefix(TRACK_PREFIX_LEGACY))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let tail = &rest[digits.len()..];
    if !tail.is_empty() && !tail.starts_with('.') {
        return None;
    }
    if tail.contains(['/', '\\']) {
        return None;
    }
    digits.parse().ok()
}

fn parse_quality(query: &str) -> Quality {
    for (k, v) in query.split('&').filter_map(|p| p.split_once('=')) {
        if k == "q" {
            return match v {
                "standard" => Quality::Standard,
                "exhigh" | "higher" => Quality::Exhigh,
                "lossless" => Quality::Lossless,
                "hires" => Quality::Hires,
                _ => Quality::Lossless,
            };
        }
    }
    Quality::Lossless
}

fn resolve_cached(client: &NeteaseClient, id: u64, quality: Quality) -> Option<SongUrl> {
    if let Ok(cache) = URL_CACHE.read()
        && let Some((at, song)) = cache.get(&(id, quality))
        && at.elapsed() < URL_TTL
    {
        return Some(song.clone());
    }
    match client.song_url(id, quality) {
        Ok(Some(song)) => {
            if let Ok(mut cache) = URL_CACHE.write() {
                cache.insert((id, quality), (Instant::now(), song.clone()));
            }
            Some(song)
        }
        Ok(None) => None,
        Err(e) => {
            log::error!("netease: {e:#}");
            None
        }
    }
}

/// Pipe upstream bytes through unmodified.
fn relay(rq: Request, client: &NeteaseClient, song: &SongUrl, range: Option<&str>, head_only: bool) {
    let resp = match client.open_cdn(&song.url, range, head_only) {
        Ok(r) => r,
        Err(e) => {
            log::error!("netease cdn: {e:#}");
            invalidate(song_id_of(&rq));
            respond_error(rq, 502, &format!("{e:#}"));
            return;
        }
    };
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        log::error!("netease cdn: HTTP {status}");
        invalidate(song_id_of(&rq));
        respond_error(rq, 502, &format!("NetEase CDN returned HTTP {status}"));
        return;
    }

    let content_type = header_str(&resp, "content-type").unwrap_or_else(|| mime_for(&song.ext));
    // The upstream length is authoritative **because it covers a Range**: on a
    // 206 it is the slice length, whereas `song.size` is the whole file, so
    // preferring `song.size` would declare more bytes than we actually send.
    // Only fall back to `song.size` for a full-body response with no upstream
    // length — some HTTP clients drop `Content-Length` on HEAD, and a renderer
    // that probes and gets no length will refuse to play (or loop).
    let upstream_len = header_str(&resp, "content-length").and_then(|s| s.parse::<usize>().ok());
    let content_length = match (upstream_len, range) {
        (Some(l), _) => Some(l),
        // A Range request whose upstream hid its length: we cannot know the
        // slice size without parsing Content-Range, so stay honest and omit it.
        (None, Some(_)) => None,
        (None, None) => (song.size > 0).then_some(song.size as usize),
    };
    let content_range = header_str(&resp, "content-range");

    let mut headers = Vec::new();
    push(&mut headers, "Content-Type", &content_type);
    // A real, seekable file with a real length — advertising ranges cannot send
    // a renderer into a request loop, unlike on a live stream.
    push(&mut headers, "Accept-Ranges", "bytes");
    push(&mut headers, "Cache-Control", "no-cache");
    if let Some(cr) = &content_range {
        push(&mut headers, "Content-Range", cr);
    }

    // tiny_http switches to chunked (and drops `Content-Length`) once the body
    // is >= 32 KiB, which is every real music file. Renderers that want a known
    // length to seek or to compute duration misbehave without it, so raise the
    // threshold: when we know the length we always send it.
    let result = if head_only {
        // Headers only, but declaring the **real** length: renderers routinely
        // probe with HEAD to decide whether they can play the file at all, and
        // a missing/zero `Content-Length` makes some of them give up (or loop).
        // tiny_http writes the header itself when `data_length` is `Some`, and
        // skips the body for HEAD, so an empty reader is the right payload here.
        rq.respond(
            Response::new(StatusCode(status), headers, io::empty(), content_length, None)
                .with_chunked_threshold(usize::MAX),
        )
    } else {
        let reader = resp.into_body().into_reader();
        rq.respond(
            Response::new(StatusCode(status), headers, reader, content_length, None)
                .with_chunked_threshold(usize::MAX),
        )
    };
    if let Err(e) = result {
        log::error!("gateway: sending response failed: {e}");
    }
}

fn song_id_of(rq: &Request) -> u64 {
    let path = rq.url().to_string();
    let (path, _) = path.split_once('?').unwrap_or((path.as_str(), ""));
    parse_track_path(path).unwrap_or(0)
}

fn header_str(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

#[must_use]
pub fn mime_for(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "flac" => "audio/flac".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "m4a" | "aac" => "audio/mp4".to_string(),
        "wav" => "audio/wav".to_string(),
        other => format!("audio/{other}"),
    }
}

pub fn push(headers: &mut Vec<Header>, name: &str, value: &str) {
    if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
        headers.push(h);
    }
}

fn respond_error(rq: Request, code: u16, msg: &str) {
    let _ = rq.respond(Response::from_string(msg).with_status_code(StatusCode(code)));
}
