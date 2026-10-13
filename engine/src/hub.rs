//! Plumbing for asynchronous callbacks, in both directions.
//!
//! * [`EventHub`] — inbound: devices POST to us (UPnP GENA `NOTIFY`). The HTTP
//!   front-end looks the path up here and hands the body to the closure.
//! * [`Bus`] — outbound: engine → host application (SSE / IPC).

use crate::model::Event;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Handler for one HTTP callback path.
pub type EventHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// Registry of callback paths the HTTP front-end should route to us.
#[derive(Clone, Default)]
pub struct EventHub {
    map: Arc<Mutex<HashMap<String, EventHandler>>>,
}

impl EventHub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `path` (relative, no leading slash). Returns the full relative
    /// URL the device should be given.
    pub fn register(&self, path: &str, cb: EventHandler) -> String {
        if let Ok(mut m) = self.map.lock() {
            m.insert(path.to_string(), cb);
        }
        path.to_string()
    }

    pub fn unregister(&self, path: &str) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(path);
        }
    }

    /// Deliver a body to the handler registered for `path`.
    pub fn dispatch(&self, path: &str, body: &str) -> bool {
        let cb = match self.map.lock() {
            Ok(m) => m.get(path).cloned(),
            Err(_) => None,
        };
        match cb {
            Some(cb) => {
                cb(body);
                true
            }
            None => false,
        }
    }

    #[must_use]
    pub fn has(&self, path: &str) -> bool {
        self.map.lock().map(|m| m.contains_key(path)).unwrap_or(false)
    }
}

/// A tiny multi-consumer event queue with blocking reads.
///
/// Deliberately not a `SyncSender` fan-out: the daemon has exactly one SSE
/// reader per client and it is simpler (and loss-free) to give each of them a
/// cursor into a shared, capped log.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<BusInner>,
}

struct BusInner {
    log: Mutex<VecDeque<(u64, Event)>>,
    cv: Condvar,
    next: Mutex<u64>,
}

impl Bus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(BusInner {
                log: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                next: Mutex::new(0),
            }),
        }
    }

    pub fn push(&self, e: Event) {
        let seq = {
            let mut n = self.inner.next.lock().unwrap_or_else(|p| p.into_inner());
            *n += 1;
            *n
        };
        if let Ok(mut log) = self.inner.log.lock() {
            log.push_back((seq, e));
            // Keep enough history for a client that reconnects to catch up.
            while log.len() > 512 {
                log.pop_front();
            }
        }
        self.inner.cv.notify_all();
    }

    /// Everything after `cursor`. Returns the new cursor.
    pub fn since(&self, cursor: u64) -> (u64, Vec<Event>) {
        let Ok(log) = self.inner.log.lock() else { return (cursor, Vec::new()) };
        let events: Vec<Event> = log.iter().filter(|(s, _)| *s > cursor).map(|(_, e)| e.clone()).collect();
        let newest = log.back().map(|(s, _)| *s).unwrap_or(cursor);
        (newest, events)
    }

    /// Long-poll: block until an event newer than `cursor` arrives, or until
    /// `timeout` elapses. Returns `(new cursor, events)`.
    ///
    /// This is what a `GET /api/events?cursor=N` front-end uses; it behaves
    /// like SSE without needing a chunked HTTP writer.
    pub fn wait_since(&self, cursor: u64, timeout: Duration) -> (u64, Vec<Event>) {
        let (now_cursor, events) = self.since(cursor);
        if !events.is_empty() {
            return (now_cursor, events);
        }
        let deadline = Instant::now() + timeout;
        let guard = self.inner.log.lock().unwrap_or_else(|p| p.into_inner());
        self.inner.cv.wait_timeout(guard, timeout).ok();
        let (c, events) = self.since(cursor);
        // Return immediately once we are out of time, even if nothing came in,
        // so the caller can keep the connection alive with an empty reply.
        if events.is_empty() && Instant::now() >= deadline {
            return (c, Vec::new());
        }
        (c, events)
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}
