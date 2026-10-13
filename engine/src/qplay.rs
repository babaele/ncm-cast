//! QPlay — Tencent's protocol behind QQ Music / 音箱 that support it.
//!
//! **What this is, and what it is not.** Real QPlay is a proprietary protocol:
//! it needs Tencent's SDK under NDA and a device that passed certification at
//! the authorised test lab. A third party cannot implement it.
//!
//! What *is* public is that QPlay devices are, at their core, UPnP AV renderers
//! that expect a small set of private headers on the control requests. This
//! module therefore discovers QPlay devices (mDNS `_qplay._tcp` plus SSDP
//! descriptions that advertise Tencent's service) and drives them with the
//! standard [`crate::upnp`] sink carrying those headers. Devices that insist on
//! a certified controller will refuse; devices that only check for the header
//! work fine.
//!
//! Treat this as a compatibility layer, and expect exactly that level of
//! reliability.

use crate::hub::{Bus, EventHub};
use crate::mdns;
use crate::model::{Device, Protocol};
use crate::ssdp;
use crate::traits::{Adapter, ConnectCtx};
use crate::upnp::{self, UpnpDesc, UpnpSink};
use anyhow::Result;
use std::time::Duration;

/// mDNS service type QPlay devices advertise.
pub const QPLAY_MDNS: &str = "_qplay._tcp.local.";
/// Service type seen inside some QPlay device descriptions.
pub const QPLAY_SERVICE: &str = "urn:schemas-tencent-com:service:QPlay:1";

/// The extra headers QPlay expects on every control request.
pub const HEADERS: [(&str, &str); 2] = [("QPlay", "1"), ("X-QPlay-Method", "SetAVTransportURI")];

pub struct QPlayAdapter;

impl Adapter for QPlayAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::QPlay
    }

    fn discover(&self, ctx: &ConnectCtx, timeout: Duration) -> Result<Vec<Device>> {
        // 1. every UPnP renderer, so we have control URLs
        let all = upnp::discover_upnp(ctx, timeout, &[], None, Protocol::QPlay)?;

        // 2. who actually claims QPlay?
        let mut qplay_addrs: Vec<String> = Vec::new();
        if let Ok(instances) = mdns::query(QPLAY_MDNS, timeout) {
            for i in instances {
                if let Some(a) = i.addr() {
                    qplay_addrs.push(a.to_string());
                }
                qplay_addrs.push(i.host.to_ascii_lowercase());
            }
        }

        let mut out = Vec::new();
        for mut d in all {
            let claims = d.model.as_deref().unwrap_or("").to_ascii_lowercase().contains("qplay")
                || d.manufacturer.as_deref().unwrap_or("").to_ascii_lowercase().contains("qplay")
                || d.name.to_ascii_lowercase().contains("qplay")
                || d.extra.values().any(|v| v.to_ascii_lowercase().contains("qplay"))
                || qplay_addrs.iter().any(|a| *a == d.addr.to_string());
            if !claims {
                continue;
            }
            d.protocol = Protocol::QPlay;
            d.id = format!("qplay:{}", d.extra.get("udn").cloned().unwrap_or_else(|| d.id.clone()));
            d.extra.insert("qplay".into(), "1".into());
            // QPlay renderers are driven with plain UPnP plus the private
            // headers, so nothing else about the device changes.
            out.push(d);
        }
        Ok(out)
    }
}

impl QPlayAdapter {
    /// Open a session that tags every SOAP request with the QPlay headers.
    pub fn open(ctx: &ConnectCtx, dev: &Device, hub: EventHub, bus: Bus) -> Result<UpnpSink> {
        let extra = HEADERS.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        upnp::UpnpAdapter::open(ctx, dev, hub, bus, extra)
    }
}

/// True when a device description advertises Tencent's QPlay service.
#[must_use]
pub fn is_qplay_desc(d: &UpnpDesc) -> bool {
    d.services.iter().any(|s| s.service_type.contains("tencent") || s.service_id.contains("QPlay"))
        || d.model.to_ascii_lowercase().contains("qplay")
        || d.manufacturer.to_ascii_lowercase().contains("qplay")
}

/// Convenience: SSDP-only QPlay detection, for callers that do not want mDNS.
pub fn discover_ssdp(ctx: &ConnectCtx, timeout: Duration) -> Result<Vec<Device>> {
    upnp::discover_upnp(ctx, timeout, &[ssdp::ST_MEDIA_RENDERER], Some(&is_qplay_desc), Protocol::QPlay)
}
