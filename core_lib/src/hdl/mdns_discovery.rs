use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::qr::QrSession;
use crate::utils::{is_not_self_ip, parse_endpoint_info};
use crate::DeviceType;

/// How long to wait for a candidate address to accept a connection before
/// moving to the next one. An unreachable candidate would otherwise block on
/// the OS TCP timeout (seconds) and delay discovery on every resolve.
const CONNECT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// True for IPv6 link-local addresses (fe80::/10). Connecting to one requires a
/// scope id identifying the interface, which we don't have here, so such a
/// candidate can never connect and is skipped rather than waited on.
/// (`Ipv6Addr::is_unicast_link_local` is still unstable, so check the prefix.)
fn is_ipv6_link_local(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct EndpointInfo {
    pub fullname: String,
    pub id: String,
    pub name: Option<String>,
    pub ip: Option<String>,
    pub port: Option<String>,
    pub rtype: Option<DeviceType>,
    pub present: Option<bool>,
    /// Set when this peer was found by scanning our QR code. Scanning is itself
    /// the user choosing us, so the sender connects without further prompting.
    pub qr_match: Option<bool>,
}

pub struct MDnsDiscovery {
    daemon: ServiceDaemon,
    sender: broadcast::Sender<EndpointInfo>,
    /// When set, a *hidden* peer that scanned this session's QR code is also
    /// reported, using the name recovered from its QR TLV. Without a session,
    /// hidden peers are skipped: we can neither identify nor name them.
    qr_session: Option<QrSession>,
}

impl MDnsDiscovery {
    pub fn new(sender: broadcast::Sender<EndpointInfo>) -> Result<Self, anyhow::Error> {
        let daemon = ServiceDaemon::new()?;

        Ok(Self {
            daemon,
            sender,
            qr_session: None,
        })
    }

    /// Also look for the peer that scans `session`'s QR code.
    pub fn with_qr_session(mut self, session: QrSession) -> Self {
        self.qr_session = Some(session);
        self
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("MDnsDiscovery: service starting");

        let service_type = "_FC9F5ED42C8A._tcp.local.";
        let receiver = self.daemon.browse(service_type)?;

        // Map with fullname as key and EndpointInfo as value
        let mut cache: HashMap<String, EndpointInfo> = HashMap::new();

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("MDnsDiscovery: tracker cancelled, breaking");
                    break;
                }
                r = receiver.recv_async() => {
                    match r {
                        Ok(event) => {
                            match event {
                                ServiceEvent::ServiceResolved(info) => {
                                    let port = info.get_port();

                                    // Quick Share publishes an ordered set of address
                                    // candidates (IPv6 first, then IPv4). Consider every
                                    // address the peer advertised rather than only the
                                    // first IPv4 one, so we can reach IPv6-only peers and
                                    // fall back when a multi-homed peer's first address
                                    // isn't reachable. "Self IPs" are filtered out.
                                    let mut candidates: Vec<IpAddr> = info
                                        .get_addresses()
                                        .iter()
                                        .copied()
                                        .filter(|ip| !is_ipv6_link_local(ip))
                                        .collect();
                                    candidates.retain(is_not_self_ip);
                                    candidates.sort_by_key(|ip| u8::from(ip.is_ipv4()));
                                    if candidates.is_empty() {
                                        continue;
                                    }

                                    // Decode the "n" text properties
                                    let n = match info.get_property("n") {
                                        Some(_n) => _n,
                                        None => continue,
                                    };

                                    let record = match parse_endpoint_info(n.val_str()) {
                                        Ok(r) => r,
                                        Err(_) => continue
                                    };

                                    // A peer that scanned our QR is recognised whether it
                                    // is visible or hidden; only a hidden one *depends*
                                    // on it, since that's the sole way to name it.
                                    let scanned = self
                                        .qr_session
                                        .as_ref()
                                        .and_then(|s| s.match_endpoint(&record));

                                    let dn = match (record.device_name.clone(), &scanned) {
                                        (Some(name), _) => name,
                                        (None, Some(name)) => {
                                            info!("ServiceResolved: hidden peer scanned our QR code: {name:?}");
                                            name.clone()
                                        }
                                        (None, None) => continue,
                                    };
                                    let dt = record.device_type.clone();
                                    let qr_match = scanned.is_some();

                                    let fullname = info.get_fullname().to_string();

                                    // Try each candidate in order; the first address that
                                    // accepts a connection wins.
                                    for ip in candidates {
                                        let addr = SocketAddr::new(ip, port);
                                        match tokio::time::timeout(
                                            CONNECT_PROBE_TIMEOUT,
                                            TcpStream::connect(addr),
                                        )
                                        .await
                                        {
                                            Ok(Ok(_)) => {}
                                            _ => {
                                                trace!("ServiceResolved: {addr} unreachable, trying next candidate");
                                                continue;
                                            }
                                        }

                                        // The frontend builds its target as `ip + ":" + port`,
                                        // so an IPv6 literal has to carry its brackets.
                                        let ip_str = match ip {
                                            IpAddr::V6(v6) => format!("[{v6}]"),
                                            IpAddr::V4(v4) => v4.to_string(),
                                        };

                                        let ei = EndpointInfo {
                                            fullname: fullname.clone(),
                                            id: addr.to_string(),
                                            name: Some(dn.clone()),
                                            ip: Some(ip_str),
                                            port: Some(port.to_string()),
                                            rtype: Some(dt.clone()),
                                            present: Some(true),
                                            qr_match: Some(qr_match),
                                        };
                                        info!("ServiceResolved: Resolved a new service: {:?}", ei);
                                        cache.insert(fullname.clone(), ei.clone());
                                        let _ = self.sender.send(ei);
                                        break;
                                    }
                                }
                                ServiceEvent::ServiceRemoved(_, fullname) => {
                                    trace!("ServiceRemoved: checking if should remove {}", fullname);
                                    // Only remove if it has not been seen in the last cleanup_threshold
                                    let should_remove = cache.get(&fullname).map(|ei| ei.id.clone());

                                    if let Some(id) = should_remove {
                                        info!("ServiceRemoved: Remove a previous service: {}", fullname);
                                        cache.remove(&fullname);
                                        let _ = self.sender.send(EndpointInfo {
                                            id,
                                            ..Default::default()
                                        });
                                    }
                                }
                                ServiceEvent::SearchStarted(_) | ServiceEvent::SearchStopped(_) => {}
                                _ => {}
                            }
                        },
                        Err(err) => {
                            // The mDNS browse channel is closed/disconnected
                            // (e.g. the daemon shut down after a sleep or
                            // network change). This error is terminal and
                            // non-recoverable, so continuing to loop would
                            // busy-spin and flood the log/disk (see #268).
                            // Stop the discovery task instead.
                            error!("MDnsDiscovery: browse channel closed, stopping discovery: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        // Cleanly stop browsing and shut the daemon down so its background
        // thread doesn't keep trying to deliver events to the now-dropped
        // receiver, which floods the log with "sending on a closed channel".
        let _ = self.daemon.stop_browse(service_type);
        // Keep the shutdown receiver and await it so the daemon can deliver its
        // shutdown-complete response instead of logging "failed to send
        // response of shutdown: sending on a closed channel".
        if let Ok(receiver) = self.daemon.shutdown() {
            let _ = receiver.recv_async().await;
        }

        Ok(())
    }
}
