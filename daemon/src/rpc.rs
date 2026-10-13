//! The HTTP front-end the host player (Electron/Vue, or anything that can
//! `fetch`) talks to on the LAN port.
//!
//! One port, four things behind it:
//!
//! * `/api/*`               — discovery, sessions, playback, groups, NetEase
//! * `/api/events?cursor=N` — long-poll of the engine event bus
//! * `/t/<id>.<ext>`        — the audio gateway renderers pull from
//! * `/evt/<path>`          — where devices POST callbacks (UPnP GENA `NOTIFY`)
//!
//! CORS is wide open on purpose: this is a local daemon driven by a local UI.

use anyhow::anyhow;
use cast_engine::engine::CastEngine;
use cast_engine::gateway::Gateway;
use cast_engine::hub::{Bus, EventHub};
use cast_engine::model::{DeviceId, Track};
use cast_engine::netease::NeteaseClient;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http_dh::{Header, Method, Request, Response, Server, StatusCode};

/// How long `/api/events` holds the request open before answering empty.
const LONG_POLL: Duration = Duration::from_secs(25);

#[derive(Deserialize)]
struct PlayReq {
    ids: Vec<DeviceId>,
    track: Track,
    /// Per-request override of the engine's delivery mode. `true` hands the
    /// renderer the NetEase CDN URL instead of our gateway URL — use it when
    /// the renderer cannot open a connection back to this host.
    #[serde(default)]
    direct: Option<bool>,
}
#[derive(Deserialize)]
struct QueueReq {
    ids: Vec<DeviceId>,
    tracks: Vec<Track>,
    #[serde(default)]
    direct: Option<bool>,
}
#[derive(Deserialize)]
struct DirectReq {
    direct: bool,
}
#[derive(Deserialize)]
struct IdsReq {
    ids: Vec<DeviceId>,
}
#[derive(Deserialize)]
struct IdReq {
    id: DeviceId,
}
#[derive(Deserialize)]
struct VolumeReq {
    id: DeviceId,
    volume: u8,
}
#[derive(Deserialize)]
struct MuteReq {
    id: DeviceId,
    muted: bool,
}
#[derive(Deserialize)]
struct SeekReq {
    id: DeviceId,
    position_ms: u64,
}
#[derive(Deserialize)]
struct CookieReq {
    cookie: String,
}

#[derive(Serialize)]
struct Reply<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn ok<T: Serialize>(data: T) -> String {
    serde_json::to_string(&Reply { ok: true, data: Some(data), error: None }).unwrap_or_else(|_| "{}".into())
}

fn fail<T: Serialize>(e: impl std::fmt::Display) -> String {
    serde_json::to_string(&Reply::<T> { ok: false, data: None, error: Some(e.to_string()) })
        .unwrap_or_else(|_| "{}".into())
}

/// Cloned handles, so no engine lock is held while a request is served.
struct Ctx {
    engine: Arc<Mutex<CastEngine>>,
    gateway: Gateway,
    hub: EventHub,
    bus: Bus,
    client: NeteaseClient,
    port: u16,
}

pub fn serve(engine: Arc<Mutex<CastEngine>>, port: u16, cookie_file: PathBuf) -> anyhow::Result<()> {
    let (local, gateway, hub, bus, client) = {
        let e = engine.lock().unwrap_or_else(|p| p.into_inner());
        (e.local_addr(), e.gateway.clone(), e.hub.clone(), e.bus.clone(), e.client.clone())
    };
    // Bind every interface, not just `local`: the renderers reach us on the
    // LAN address, while the player UI talks to 127.0.0.1. Both work.
    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow!("cannot bind 0.0.0.0:{port}: {e}"))?;
    println!("ncm-castd listening on http://127.0.0.1:{port} (LAN: http://{local}:{port})");

    let ctx = Arc::new(Ctx { engine, gateway, hub, bus, client, port });
    let cookie_path = Arc::new(cookie_file);

    // Housekeeping: keep-alives, subscription renewals, group drift correction.
    let ticker = Arc::clone(&ctx);
    std::thread::Builder::new().name("cast-tick".into()).spawn(move || loop {
        ticker.engine.lock().unwrap_or_else(|p| p.into_inner()).tick();
        std::thread::sleep(Duration::from_secs(1));
    })?;

    for rq in server.incoming_requests() {
        let ctx = Arc::clone(&ctx);
        let cookie_path = Arc::clone(&cookie_path);
        // One thread per request: a renderer pulling a 40 MB FLAC must not
        // block the UI's status polling.
        std::thread::spawn(move || handle(rq, &ctx, &cookie_path));
    }
    Ok(())
}

fn handle(mut rq: Request, ctx: &Ctx, cookie_path: &Path) {
    let method = rq.method().clone();
    let url = rq.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));

    if method == Method::Options {
        let _ = rq.respond(cors(Response::from_string("")));
        return;
    }
    // Devices POST their callbacks here.
    if let Some(rest) = path.strip_prefix("/evt/") {
        let body = body_of(&mut rq);
        ctx.hub.dispatch(rest, &body);
        let _ = rq.respond(cors(Response::from_string("ok")));
        return;
    }
    // Audio: hand the request straight to the gateway, which consumes it.
    if path.starts_with("/t/") || path.starts_with("/netease/") {
        ctx.gateway.handle(rq);
        return;
    }

    let body = body_of(&mut rq);
    let (code, text) = match route(&method, path, query, &body, ctx, cookie_path) {
        Ok((c, t)) => (c, t),
        Err((c, m)) => (c, fail::<()>(m)),
    };
    let _ = rq.respond(
        cors(Response::from_string(text).with_status_code(StatusCode(code)))
            .with_header(header("Content-Type", "application/json")),
    );
}

fn body_of(rq: &mut Request) -> String {
    let mut s = String::new();
    let _ = rq.as_reader().take(8 * 1024 * 1024).read_to_string(&mut s);
    s
}

fn cors<R: Read>(r: Response<R>) -> Response<R> {
    r.with_header(header("Access-Control-Allow-Origin", "*"))
        .with_header(header("Access-Control-Allow-Headers", "content-type"))
        .with_header(header("Access-Control-Allow-Methods", "GET,POST,OPTIONS"))
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(b"X-Empty", b"").expect("valid header"))
}

fn q(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Ok(hi), Ok(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

/// Run `f` against the engine, mapping any error to a 500.
fn with_engine<T: Serialize>(
    ctx: &Ctx,
    f: impl FnOnce(&mut CastEngine) -> anyhow::Result<T>,
) -> Result<(u16, String), (u16, String)> {
    let mut e = ctx.engine.lock().unwrap_or_else(|p| p.into_inner());
    match f(&mut e) {
        Ok(v) => Ok((200, ok(v))),
        Err(e) => Err((500, format!("{e:#}"))),
    }
}

#[allow(clippy::too_many_lines)]
fn route(
    method: &Method,
    path: &str,
    query: &str,
    body: &str,
    ctx: &Ctx,
    cookie_path: &Path,
) -> Result<(u16, String), (u16, String)> {
    let post = matches!(method, Method::Post);
    match (post, path) {
        // ---------------- devices ----------------
        (false, "/api/devices") => Ok((200, ok(ctx.engine.lock().unwrap_or_else(|p| p.into_inner()).devices()))),
        (false, "/api/direct") => {
            Ok((200, ok(ctx.engine.lock().unwrap_or_else(|p| p.into_inner()).is_direct())))
        }
        (false, "/api/info") => {
            let e = ctx.engine.lock().unwrap_or_else(|p| p.into_inner());
            #[derive(Serialize)]
            struct Info<'a> {
                lan: String,
                port: u16,
                direct: bool,
                gateway: &'a str,
            }
            Ok((
                200,
                ok(Info {
                    lan: e.local_addr().to_string(),
                    port: ctx.port,
                    direct: e.is_direct(),
                    gateway: if e.is_direct() { "netease-cdn" } else { "local" },
                }),
            ))
        }
        (true, "/api/refresh") => with_engine(ctx, |e| {
            let (added, lost) = e.refresh(None);
            Ok(serde_json::json!({ "added": added, "lost": lost }))
        }),
        (true, "/api/connect") => {
            let r: IdReq = parse(body)?;
            with_engine(ctx, |e| e.connect(&r.id).map(|_| true))
        }
        (true, "/api/disconnect") => {
            let r: IdReq = parse(body)?;
            with_engine(ctx, |e| {
                e.disconnect(&r.id);
                Ok(true)
            })
        }

        // ---------------- playback ----------------
        (true, "/api/play") => {
            let r: PlayReq = parse(body)?;
            with_engine(ctx, |e| {
                if let Some(d) = r.direct {
                    e.set_direct(d);
                }
                e.play_track(&r.ids, &r.track).map(|_| true)
            })
        }
        (true, "/api/queue") => {
            let r: QueueReq = parse(body)?;
            with_engine(ctx, |e| {
                if let Some(d) = r.direct {
                    e.set_direct(d);
                }
                e.play_queue(&r.ids, r.tracks).map(|_| true)
            })
        }
        // Switch how URLs are handed out (gateway vs. NetEase CDN). See
        // `CastEngine::set_direct` for why this exists at all.
        (true, "/api/direct") => {
            let r: DirectReq = parse(body)?;
            with_engine(ctx, |e| {
                e.set_direct(r.direct);
                Ok(e.is_direct())
            })
        }
        (true, "/api/next") => {
            let r: IdsReq = parse(body)?;
            with_engine(ctx, |e| e.next(&r.ids).map(|_| true))
        }
        (true, "/api/pause") => {
            let r: IdReq = parse(body)?;
            with_engine(ctx, |e| e.pause(&r.id).map(|_| true))
        }
        (true, "/api/resume") => {
            let r: IdReq = parse(body)?;
            with_engine(ctx, |e| e.resume(&r.id).map(|_| true))
        }
        (true, "/api/stop") => {
            let r: IdReq = parse(body)?;
            with_engine(ctx, |e| e.stop(&r.id).map(|_| true))
        }
        (true, "/api/volume") => {
            let r: VolumeReq = parse(body)?;
            with_engine(ctx, |e| e.set_volume(&r.id, r.volume).map(|_| true))
        }
        (true, "/api/mute") => {
            let r: MuteReq = parse(body)?;
            with_engine(ctx, |e| e.set_mute(&r.id, r.muted).map(|_| true))
        }
        // Seek the renderer to an absolute position. The host player's progress
        // bar drags map straight onto this; protocols without `Sink::seek`
        // (HEOS/AirPlay) answer 500 and the UI keeps the bar display-only.
        (true, "/api/seek") => {
            let r: SeekReq = parse(body)?;
            with_engine(ctx, |e| e.seek(&r.id, r.position_ms).map(|_| true))
        }
        (false, "/api/status") => {
            let id = q(query, "id").ok_or_else(|| (400, "id is required".to_string()))?;
            with_engine(ctx, |e| e.status(&id))
        }

        // ---------------- groups ----------------
        (true, "/api/group") => {
            let r: IdsReq = parse(body)?;
            with_engine(ctx, |e| e.group(&r.ids))
        }
        (true, "/api/ungroup") => with_engine(ctx, |e| e.ungroup().map(|_| true)),

        // ---------------- events ----------------
        (false, "/api/events") => {
            let cursor = q(query, "cursor").and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
            let (next, events) = ctx.bus.wait_since(cursor, LONG_POLL);
            Ok((200, ok(serde_json::json!({ "cursor": next, "events": events }))))
        }

        // ---------------- NetEase ----------------
        (false, "/api/netease/qr") => match ctx.client.qr_generate() {
            Ok(qr) => Ok((200, ok(serde_json::json!({
                "unikey": qr.unikey,
                "mime": "image/jpeg",
                "image": base64_encode(&qr.image_bytes),
            })))),
            Err(e) => Err((502, format!("{e:#}"))),
        },
        (false, "/api/netease/qr/poll") => {
            let key = q(query, "key").ok_or_else(|| (400, "key is required".to_string()))?;
            match ctx.client.qr_poll(&key) {
                Ok((poll, cookie)) => {
                    if let Some(c) = &cookie {
                        let _ = NeteaseClient::save_cookie(cookie_path, c);
                        // Rebuild both clients: the engine's and the gateway's.
                        ctx.engine.lock().unwrap_or_else(|p| p.into_inner()).set_cookie(c.clone());
                    }
                    Ok((200, ok(serde_json::json!({
                        "status": format!("{poll:?}").to_lowercase(),
                        "cookie": cookie.is_some(),
                    }))))
                }
                Err(e) => Err((502, format!("{e:#}"))),
            }
        }
        (false, "/api/netease/playlists") => match ctx.client.user_playlists() {
            Ok(l) => Ok((200, ok(l))),
            Err(e) => Err((502, format!("{e:#}"))),
        },
        (false, "/api/netease/playlist") => {
            let id = q(query, "id")
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| (400, "id is required".to_string()))?;
            let quality = q(query, "quality").unwrap_or_else(|| "lossless".into());
            let quality = serde_json::from_str::<cast_engine::model::Quality>(&format!("\"{quality}\""))
                .unwrap_or_default();
            match ctx.client.playlist_tracks(id, quality) {
                Ok(t) => Ok((200, ok(t))),
                Err(e) => Err((502, format!("{e:#}"))),
            }
        }
        (false, "/api/netease/search") => {
            let kw = q(query, "kw").unwrap_or_default();
            let limit = q(query, "limit").and_then(|s| s.parse().ok()).unwrap_or(30);
            match ctx.client.search(&kw, limit, cast_engine::model::Quality::Lossless) {
                Ok(t) => Ok((200, ok(t))),
                Err(e) => Err((502, format!("{e:#}"))),
            }
        }
        (true, "/api/netease/cookie") => {
            let r: CookieReq = parse(body)?;
            NeteaseClient::save_cookie(cookie_path, &r.cookie)
                .map_err(|e| (500, format!("{e:#}")))?;
            ctx.engine.lock().unwrap_or_else(|p| p.into_inner()).set_cookie(r.cookie);
            Ok((200, ok(true)))
        }

        _ => Err((404, format!("no route for {method} {path}"))),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, (u16, String)> {
    serde_json::from_str(body).map_err(|e| (400, format!("bad request body: {e}")))
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}
