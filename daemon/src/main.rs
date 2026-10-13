//! `ncm-castd` — the cast daemon a third-party NetEase Cloud Music player
//! talks to.
//!
//! ```text
//! ncm-castd [--port 9100] [--cookie-file <path>]
//! ```
//!
//! It owns one HTTP port that serves the JSON API, the event long-poll, the
//! UPnP callback sink and the audio gateway.

mod rpc;

use anyhow::{Context, Result};
use cast_engine::engine::CastEngine;
use cast_engine::netease::NeteaseClient;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn default_cookie_file() -> PathBuf {
    // Next to the executable is good enough for a local daemon and avoids
    // pulling in a `dirs` dependency.
    let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    p.pop();
    p.push("ncm-cookie.txt");
    p
}

fn main() -> Result<()> {
    let mut port: u16 = 9100;
    let mut cookie_file = default_cookie_file();
    let mut host: Option<std::net::IpAddr> = None;
    let mut direct = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .context("--port needs a number")?;
            }
            "--host" => {
                let raw = args.next().context("--host needs an IP address")?;
                host = Some(raw.parse().with_context(|| format!("--host: bad IP {raw}"))?);
            }
            "--cookie-file" => {
                cookie_file = PathBuf::from(args.next().context("--cookie-file needs a path")?);
            }
            "--direct" => {
                direct = true;
            }
            "--help" | "-h" => {
                println!(
                    "ncm-castd [--port 9100] [--host <lan-ip>] [--direct] [--cookie-file <path>]\n\
                     \n\
                       --port          HTTP port for the JSON API and the audio gateway\n\
                       --host          LAN address handed to renderers (auto-detected\n\
                                       otherwise; set it when the machine has several NICs)\n\
                       --direct        hand renderers the NetEase CDN URL instead of our\n\
                                       own gateway URL. Use it when the renderer cannot\n\
                                       open a connection back to this machine (firewall,\n\
                                       360/火绒 LAN stealth, router AP isolation) — the\n\
                                       symptom is everything succeeding except the audio.\n\
                     \n\
                     Interfaces, if you want to know what we picked:\n\
                       GET /api/info      GET /api/devices   GET /api/events?cursor=0"
                );
                return Ok(());
            }
            other => {
                return Err(anyhow::anyhow!("unknown argument: {other}"));
            }
        }
    }

    init_log();

    let cookie = NeteaseClient::load_cookie(&cookie_file);
    if cookie.is_empty() {
        println!("no cookie yet — use GET /api/netease/qr then /api/netease/qr/poll to log in");
    }
    let mut engine = CastEngine::start(cookie, port, host)?;
    if direct {
        engine.set_direct(true);
    }
    println!(
        "ncm-castd: LAN {} : {port} — renderers fetch from {}",
        engine.local_addr(),
        if engine.is_direct() { "NetEase CDN (direct mode)" } else { "this machine (gateway)" }
    );
    rpc::serve(Arc::new(Mutex::new(engine)), port, cookie_file)
}

/// Minimal logger: `RUST_LOG=debug ncm-castd` and friends.
fn init_log() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let level = match filter.as_str() {
        "error" => log::Level::Error,
        "warn" => log::Level::Warn,
        "debug" | "trace" => log::Level::Debug,
        _ => log::Level::Info,
    };
    let _ = log::set_logger(&SIMPLE_LOGGER);
    log::set_max_level(level.to_level_filter());
}

static SIMPLE_LOGGER: SimpleLogger = SimpleLogger;

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, _m: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        println!("[{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}
