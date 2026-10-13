//! NetEase Cloud Music API client — no signing, no external service.
//!
//! NetEase retired its `weapi` (AES-CBC + RSA) and `eapi` (AES-ECB + MD5)
//! channels server-side: signed requests now answer `200 OK` with an empty
//! body. The current web player therefore uses plain unsigned `/api/...`
//! endpoints, and so do we.
//!
//! | endpoint                              | method | purpose                     |
//! |---------------------------------------|--------|-----------------------------|
//! | `/api/cloudsearch/pc`                 | GET    | keyword search              |
//! | `/api/v6/playlist/detail`             | GET    | playlist metadata+trackIds  |
//! | `/api/v3/song/detail`                 | GET    | full song list              |
//! | `/api/song/enhance/player/url/v1`     | GET    | song id → playable URL      |
//! | `/api/w/nuser/account/get`            | GET    | logged-in user              |
//! | `/api/user/playlist`                  | GET    | user's playlists            |
//! | `/api/login/qrcode/unikey`            | POST   | start QR login              |
//! | `/api/web/qrcode/get`                 | POST   | render the QR as JPEG       |
//! | `/api/login/qrcode/client/login`      | POST   | poll QR login result        |
//!
//! Everything is parsed from `serde_json::Value` on purpose: these responses
//! nest differently per account state, and a strict struct would turn every
//! NetEase tweak into a hard failure.

use crate::model::{Quality, Track};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::Duration;
use ureq::Agent;

/// How long a resolved CDN URL stays usable (NetEase signs them, ~20 min).
pub const URL_TTL: Duration = Duration::from_secs(300);

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
pub const CDN_USER_AGENT: &str = BROWSER_UA;
pub const CDN_REFERER: &str = "https://music.163.com/";

const QR_TARGET_PREFIX: &str = "http://music.163.com/login?codekey=";

/// A resolved, playable CDN URL.
#[derive(Clone, Debug, Serialize)]
pub struct SongUrl {
    pub url: String,
    /// bitrate in bits/s as reported by NetEase
    pub br: u32,
    pub size: u64,
    /// `flac`, `mp3`, ... as reported by NetEase
    pub ext: String,
}

/// A user playlist, as shown in the sidebar.
#[derive(Clone, Debug, Serialize)]
pub struct Playlist {
    pub id: u64,
    pub name: String,
    pub track_count: u64,
    pub cover: Option<String>,
}

/// Outcome of one QR login poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QrPoll {
    /// 801 — nobody scanned yet
    Waiting,
    /// 802 — the QR expired
    Expired,
    /// 803 — scanned, waiting for confirmation on the phone
    Scanned,
    /// 800 — done; the cookie is in [`QrPoll::Success`]-carrying response
    Success,
}

/// A rendered login QR.
pub struct QrImage {
    pub unikey: String,
    /// Raw bytes — NetEase returns JPEG here, despite the old `qrcode/generate`
    /// endpoint having returned a PNG data URI.
    pub image_bytes: Vec<u8>,
}

/// The client. Cheap to clone; holds one shared `ureq` agent.
#[derive(Clone)]
pub struct NeteaseClient {
    cookie: String,
    http: Agent,
}

impl NeteaseClient {
    #[must_use]
    pub fn new(cookie: impl Into<String>) -> Self {
        Self { cookie: cookie.into(), http: crate::traits::http_agent() }
    }

    /// Read a cookie from disk (empty string when the file is missing).
    #[must_use]
    pub fn load_cookie(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default().trim().to_string()
    }

    pub fn save_cookie(path: &Path, cookie: &str) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).ok();
        }
        fs::write(path, cookie).with_context(|| format!("writing {}", path.display()))
    }

    #[must_use]
    pub fn cookie(&self) -> &str {
        &self.cookie
    }

    /// Rebuild the client around a fresh cookie (used right after a QR login).
    #[must_use]
    pub fn with_cookie(&self, cookie: impl Into<String>) -> Self {
        Self::new(cookie.into())
    }

    fn csrf(&self) -> String {
        self.cookie
            .split(';')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| k.trim() == "__csrf")
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    }

    /// `os=pc` unlocks the desktop responses (notably lossless URLs).
    fn effective_cookie(&self) -> String {
        if self.cookie.is_empty() {
            "os=pc".to_string()
        } else if self.cookie.contains("os=") {
            self.cookie.clone()
        } else {
            format!("{}; os=pc", self.cookie)
        }
    }

    fn get(&self, path: &str) -> Result<String> {
        let url = format!("https://music.163.com{path}");
        let mut resp = self
            .http
            .get(&url)
            .header("User-Agent", BROWSER_UA)
            .header("Referer", CDN_REFERER)
            .header("Cookie", &self.effective_cookie())
            .call()
            .with_context(|| format!("GET {path}"))?;
        let body = resp.body_mut().read_to_string()?;
        if body.trim().is_empty() {
            return Err(anyhow!("NetEase answered {path} with an empty body"));
        }
        Ok(body)
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<String> {
        let url = format!("https://music.163.com{path}");
        let mut resp = self
            .http
            .post(&url)
            .header("User-Agent", BROWSER_UA)
            .header("Referer", CDN_REFERER)
            .header("Cookie", &self.effective_cookie())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send_form(form.iter().copied())
            .with_context(|| format!("POST {path}"))?;
        let body = resp.body_mut().read_to_string()?;
        if body.trim().is_empty() {
            return Err(anyhow!("NetEase answered {path} with an empty body"));
        }
        Ok(body)
    }

    fn json(&self, path: &str) -> Result<Value> {
        let body = self.get(path)?;
        serde_json::from_str(&body).with_context(|| format!("decoding {path}"))
    }

    // ---------------- catalogue ----------------

    /// Keyword search, songs only.
    pub fn search(&self, keyword: &str, limit: u32, quality: Quality) -> Result<Vec<Track>> {
        let s = urlencode(keyword);
        let v = self.json(&format!(
            "/api/cloudsearch/pc?s={s}&type=1&limit={limit}&offset=0"
        ))?;
        let songs = v
            .pointer("/result/songs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(songs.iter().filter_map(|s| song_to_track(s, quality)).collect())
    }

    /// Every track of a playlist.
    ///
    /// `v6/playlist/detail` only embeds the first ~10 tracks; the rest are in
    /// `trackIds` and have to be fetched separately, which is what the second
    /// branch does.
    pub fn playlist_tracks(&self, id: u64, quality: Quality) -> Result<Vec<Track>> {
        let v = self.json(&format!("/api/v6/playlist/detail?id={id}"))?;
        let pl = v.pointer("/playlist").or_else(|| v.pointer("/result/playlist"));
        let Some(pl) = pl else {
            return Err(anyhow!("playlist {id}: no playlist object in response"));
        };
        let track_count = pl.get("trackCount").and_then(Value::as_u64).unwrap_or(0);
        let embedded = pl.get("tracks").and_then(Value::as_array).cloned().unwrap_or_default();
        if embedded.len() as u64 >= track_count && !embedded.is_empty() {
            return Ok(embedded.iter().filter_map(|s| song_to_track(s, quality)).collect());
        }
        let ids: Vec<u64> = pl
            .get("trackIds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|t| t.get("id").and_then(Value::as_u64)).collect())
            .unwrap_or_default();
        self.songs_detail(&ids, quality)
    }

    /// Bulk song lookup. The `c` parameter is mandatory — without it NetEase
    /// answers with an empty song list.
    pub fn songs_detail(&self, ids: &[u64], quality: Quality) -> Result<Vec<Track>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(400) {
            let id_json = serde_json::to_string(chunk)?;
            let c: Vec<String> = chunk.iter().map(|i| format!("{{\"id\":{i}}}")).collect();
            let c_json = format!("[{}]", c.join(","));
            let path = format!(
                "/api/v3/song/detail?ids={}&c={}",
                urlencode(&id_json),
                urlencode(&c_json)
            );
            let v = self.json(&path)?;
            if let Some(songs) = v.get("songs").and_then(Value::as_array) {
                out.extend(songs.iter().filter_map(|s| song_to_track(s, quality)));
            }
        }
        Ok(out)
    }

    /// The playlists of the logged-in user (created + subscribed).
    pub fn user_playlists(&self) -> Result<Vec<Playlist>> {
        let uid = self.user_account()?.map(|(id, _)| id).unwrap_or(0);
        let v = self.json(&format!("/api/user/playlist?uid={uid}&limit=1000&offset=0"))?;
        let lists = v.get("playlist").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(lists
            .iter()
            .filter_map(|p| {
                Some(Playlist {
                    id: p.get("id")?.as_u64()?,
                    name: p.get("name")?.as_str().unwrap_or("?").to_string(),
                    track_count: p.get("trackCount")?.as_u64().unwrap_or(0),
                    cover: p.get("coverImgUrl").and_then(Value::as_str).map(str::to_string),
                })
            })
            .collect())
    }

    /// `(user id, nickname)` of whoever owns the cookie.
    pub fn user_account(&self) -> Result<Option<(u64, String)>> {
        let v = self.json("/api/w/nuser/account/get")?;
        if let (Some(id), Some(name)) = (
            v.pointer("/profile/userId").and_then(Value::as_u64),
            v.pointer("/profile/nickname").and_then(Value::as_str),
        ) {
            return Ok(Some((id, name.to_string())));
        }
        if let (Some(id), Some(name)) = (
            v.pointer("/account/id").and_then(Value::as_u64),
            v.pointer("/account/userName").and_then(Value::as_str),
        ) {
            return Ok(Some((id, name.to_string())));
        }
        Ok(None)
    }

    // ---------------- playback ----------------

    /// Open a CDN stream, forwarding an optional `Range` header.
    ///
    /// The CDN refuses requests that do not look like they come from the web
    /// player, hence the browser UA and referer.
    pub fn open_cdn(
        &self,
        url: &str,
        range: Option<&str>,
        head_only: bool,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        // `ureq` has no generic `request(method, url)`, so pick the verb here.
        let req = if head_only { self.http.head(url) } else { self.http.get(url) };
        let req = req.header("User-Agent", CDN_USER_AGENT).header("Referer", CDN_REFERER);
        let req = match range {
            Some(r) => req.header("Range", r),
            None => req,
        };
        req.call().with_context(|| format!("CDN {url}"))
    }

    /// Resolve one song id to a playable URL, walking the quality chain until
    /// NetEase actually grants one.
    pub fn song_url(&self, id: u64, quality: Quality) -> Result<Option<SongUrl>> {
        for level in quality.chain() {
            let path = format!(
                "/api/song/enhance/player/url/v1?ids=[{id}]&level={level}&encodeType=&csrf_token={}",
                self.csrf()
            );
            let Ok(v) = self.json(&path) else { continue };
            let Some(data) = v.get("data").and_then(Value::as_array) else { continue };
            for d in data {
                let url = d.get("url").and_then(Value::as_str).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                let ext = d
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some(if *level == "lossless" { "flac" } else { "mp3" }))
                    .unwrap_or("mp3")
                    .to_ascii_lowercase();
                return Ok(Some(SongUrl {
                    url: url.to_string(),
                    br: d.get("br").and_then(Value::as_u64).unwrap_or(0) as u32,
                    size: d.get("size").and_then(Value::as_u64).unwrap_or(0),
                    ext,
                }));
            }
        }
        Ok(None)
    }

    // ---------------- QR login ----------------

    /// Step 1 — ask for a login key.
    ///
    /// NetEase risk-controls this endpoint per source IP. `-462` means the IP
    /// itself is flagged (a bare `curl` with the right UA and Referer gets the
    /// same answer), so it is worth saying so plainly instead of dumping JSON.
    pub fn qr_unikey(&self) -> Result<String> {
        let body = self.post_form("/api/login/qrcode/unikey", &[("type", "1")])?;
        let v: Value = serde_json::from_str(&body)?;
        if let Some(key) = v.get("unikey").and_then(Value::as_str) {
            return Ok(key.to_string());
        }
        let code = v.get("code").and_then(Value::as_i64);
        if code == Some(-462) {
            let tip = v
                .get("data")
                .and_then(|d| d.get("blockText"))
                .and_then(Value::as_str)
                .unwrap_or("检测到您的网络环境存在风险");
            bail!(
                "网易风控拦截了二维码登录（{tip}）。这是按出口 IP 判定的，\
                 与本程序无关：换网络 / 关代理后重试，或直接用播放器里已登录的账号\
                 （前端会调用 /api/netease/cookie 把现有 cookie 交给本服务）。"
            );
        }
        Err(anyhow!("no unikey in {body}"))
    }

    /// Step 2 — render the key as an image (JPEG in practice).
    pub fn qr_image_bytes(&self, unikey: &str) -> Result<Vec<u8>> {
        let body = self.post_form(
            "/api/web/qrcode/get",
            &[("url", &format!("{QR_TARGET_PREFIX}{unikey}&size=300"))],
        )?;
        let v: Value = serde_json::from_str(&body)?;
        let Some(img) = v.get("qrcodeImageUrl").and_then(Value::as_str) else {
            return Err(anyhow!("no qrcodeImageUrl in {body}"));
        };
        let mut resp = self.http.get(img).call().with_context(|| format!("GET {img}"))?;
        Ok(resp.body_mut().read_to_vec()?)
    }

    /// Steps 1+2 in one call.
    pub fn qr_generate(&self) -> Result<QrImage> {
        let unikey = self.qr_unikey()?;
        let image_bytes = self.qr_image_bytes(&unikey)?;
        Ok(QrImage { unikey, image_bytes })
    }

    /// Step 3 — poll. Returns the cookie on success.
    pub fn qr_poll(&self, unikey: &str) -> Result<(QrPoll, Option<String>)> {
        let body = self.post_form(
            "/api/login/qrcode/client/login",
            &[("key", unikey), ("type", "1")],
        )?;
        let v: Value = serde_json::from_str(&body)?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let poll = match code {
            800 => QrPoll::Success,
            801 => QrPoll::Waiting,
            802 => QrPoll::Expired,
            803 => QrPoll::Scanned,
            _ => return Err(anyhow!("unexpected QR poll response: {body}")),
        };
        let cookie = if poll == QrPoll::Success {
            v.get("cookie").and_then(Value::as_str).map(str::to_string)
        } else {
            None
        };
        Ok((poll, cookie))
    }
}

/// Normalise one raw song object into a [`Track`].
fn song_to_track(s: &Value, quality: Quality) -> Option<Track> {
    let id = s.get("id")?.as_u64()?;
    let name = s.get("name")?.as_str().unwrap_or("?").to_string();
    let artists = s
        .get("artists")
        .or_else(|| s.get("ar"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let album = s
        .get("album")
        .or_else(|| s.get("al"))
        .and_then(|al| al.get("name").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let cover = s
        .get("album")
        .or_else(|| s.get("al"))
        .and_then(|al| al.get("picUrl").and_then(Value::as_str))
        .map(str::to_string);
    let duration_ms = s.get("duration").or_else(|| s.get("dt")).and_then(Value::as_u64).unwrap_or(0);
    Some(Track { id, title: name, artist: artists, album, duration_ms, cover, quality })
}

/// Percent-encode a UTF-8 string for a query parameter.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
