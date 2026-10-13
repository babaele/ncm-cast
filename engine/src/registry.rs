//! The discovery registry: runs every adapter and tracks device lifetimes.

use crate::airplay::AirPlayAdapter;
use crate::heos::HeosAdapter;
use crate::model::{Device, DeviceId, Event};
use crate::qplay::QPlayAdapter;
use crate::traits::{Adapter, ConnectCtx};
use crate::upnp::UpnpAdapter;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A device that has not answered discovery for this long is considered gone.
const GRACE: Duration = Duration::from_secs(90);

pub struct Registry {
    adapters: Vec<Box<dyn Adapter>>,
    devices: HashMap<DeviceId, Device>,
    seen: HashMap<DeviceId, Instant>,
}

impl Registry {
    /// All four protocols, in the order they are searched.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            adapters: vec![
                Box::new(UpnpAdapter),
                Box::new(HeosAdapter),
                Box::new(AirPlayAdapter),
                Box::new(QPlayAdapter),
            ],
            devices: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    /// Run one discovery sweep across every protocol.
    ///
    /// One adapter failing never aborts the sweep — a HEOS-less network is not
    /// an error, and neither is an mDNS query that gets no answer.
    pub fn refresh(&mut self, ctx: &ConnectCtx, timeout: Duration, bus: &crate::hub::Bus) -> (Vec<Device>, Vec<DeviceId>) {
        let now = Instant::now();
        let mut added = Vec::new();
        for a in &self.adapters {
            match a.discover(ctx, timeout) {
                Ok(found) => {
                    for d in found {
                        self.seen.insert(d.id.clone(), now);
                        if self.devices.insert(d.id.clone(), d.clone()).is_none() {
                            added.push(d.clone());
                            bus.push(Event::DeviceFound { device: d });
                        }
                    }
                }
                Err(e) => {
                    log::debug!("{}: discovery failed: {e:#}", a.protocol());
                    bus.push(Event::Log {
                        level: "debug",
                        message: format!("{}: {e:#}", a.protocol()),
                    });
                }
            }
        }
        let lost: Vec<DeviceId> = self
            .seen
            .iter()
            .filter(|(_, at)| now.duration_since(**at) > GRACE)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &lost {
            self.seen.remove(id);
            self.devices.remove(id);
            bus.push(Event::DeviceLost { id: id.clone() });
        }
        (added, lost)
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Device> {
        self.devices.get(id).cloned()
    }

    #[must_use]
    pub fn list(&self) -> Vec<Device> {
        let mut v: Vec<Device> = self.devices.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Drop a device immediately (it answered our control call with an error).
    pub fn forget(&mut self, id: &str) {
        self.devices.remove(id);
        self.seen.remove(id);
    }
}
