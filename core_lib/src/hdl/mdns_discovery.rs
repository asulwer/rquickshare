use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::qr::QrSession;
use crate::utils::{is_not_self_ip, parse_endpoint_info};
use crate::DeviceType;

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

        // Probing a peer's addresses can take as long as the OS connect timeout
        // for an unreachable one, so probes run in their own tasks and report
        // back here. `probing` keeps a peer from being probed twice at once,
        // since mDNS re-resolves the same service repeatedly, and holds what is
        // needed to abandon a probe: its generation, so a stale result is
        // recognised, and its abort handle, for a peer removed mid-probe.
        let (probe_tx, mut probe_rx) = mpsc::channel::<(String, u64, Option<EndpointInfo>)>(32);
        let mut probing: HashMap<String, (u64, tokio::task::AbortHandle)> = HashMap::new();
        let mut generation: u64 = 0;

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
                                    // `ServiceResolved` now carries a `ResolvedService`
                                    // rather than a `ServiceInfo`, and its addresses are
                                    // `ScopedIp` (an IPv6 address plus the scope_id of the
                                    // interface it was seen on) instead of bare `IpAddr`.
                                    // We route by address alone, so flatten to `IpAddr`.
                                    let mut candidates: Vec<IpAddr> = info
                                        .get_addresses()
                                        .iter()
                                        .map(|ip| ip.to_ip_addr())
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

                                    // A cached peer is probed again only when this
                                    // resolve moves it: a new address or port is
                                    // otherwise ignored until ServiceRemoved.
                                    if !needs_probe(cache.get(&fullname), &candidates, port)
                                        || probing.contains_key(&fullname)
                                    {
                                        continue;
                                    }

                                    generation += 1;
                                    let this_probe = generation;
                                    let tx = probe_tx.clone();
                                    let probe_name = fullname.clone();
                                    let task = tokio::spawn(async move {
                                        let ei = Self::probe_candidates(
                                            &probe_name, candidates, port, &dn, &dt, qr_match,
                                        )
                                        .await;
                                        // The loop is gone only when discovery stops.
                                        if tx.send((probe_name, this_probe, ei)).await.is_err() {
                                            trace!("MDnsDiscovery: probe finished after discovery stopped");
                                        }
                                    });
                                    probing.insert(fullname, (this_probe, task.abort_handle()));
                                }
                                ServiceEvent::ServiceRemoved(_, fullname) => {
                                    trace!("ServiceRemoved: checking if should remove {}", fullname);
                                    // Still being probed: drop the probe, or its result
                                    // would add back a peer that has just gone.
                                    if let Some((_, task)) = probing.remove(&fullname) {
                                        task.abort();
                                    }
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
                Some((fullname, this_probe, ei)) = probe_rx.recv() => {
                    // Only the result of the probe still on record counts; one
                    // that was abandoned (removed, or superseded) is dropped.
                    match probing.get(&fullname) {
                        Some((current, _)) if *current == this_probe => {
                            probing.remove(&fullname);
                        }
                        _ => continue,
                    }

                    // A peer none of whose addresses answered isn't cached, so the
                    // next resolve for it is probed again - it may just have been
                    // asleep.
                    let Some(ei) = ei else {
                        trace!("MDnsDiscovery: no reachable address for {fullname}");
                        continue;
                    };

                    // Let BLE discovery know this peer is reachable over the
                    // network, so it doesn't list the same phone twice.
                    #[cfg(feature = "experimental")]
                    if let Some(name) = &ei.name {
                        crate::hdl::note_lan_peer(name);
                    }

                    info!("ServiceResolved: Resolved a new service: {:?}", ei);
                    // A re-probe that found the peer somewhere else: withdraw
                    // the old entry, which the front end keys by address.
                    if let Some(old) = cache.insert(fullname, ei.clone()) {
                        if old.id != ei.id {
                            let _ = self.sender.send(EndpointInfo {
                                id: old.id,
                                ..Default::default()
                            });
                        }
                    }
                    // Before the front end hears of it, so the send it starts
                    // straight away finds the key.
                    if ei.qr_match == Some(true) {
                        if let Some(session) = &self.qr_session {
                            session.remember_peer(&ei.id);
                        }
                    }
                    let _ = self.sender.send(ei);
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

    /// Return the first of `candidates` that accepts a connection, as the
    /// `EndpointInfo` the front end can send to, or `None` if none of them does.
    ///
    /// All candidates are tried at once and the first to connect wins; the rest
    /// are abandoned when the task set drops. Trying them in turn needed a
    /// per-address timeout so an unreachable one couldn't stall the reachable
    /// one behind it. Always run from its own task: when no candidate answers,
    /// this lasts as long as the OS connect timeout.
    async fn probe_candidates(
        fullname: &str,
        candidates: Vec<IpAddr>,
        port: u16,
        device_name: &str,
        device_type: &DeviceType,
        qr_match: bool,
    ) -> Option<EndpointInfo> {
        let mut attempts = tokio::task::JoinSet::new();
        for ip in candidates {
            attempts.spawn(async move {
                let addr = SocketAddr::new(ip, port);
                TcpStream::connect(addr).await.map(|_| addr)
            });
        }

        while let Some(attempt) = attempts.join_next().await {
            let addr = match attempt {
                Ok(Ok(addr)) => addr,
                Ok(Err(e)) => {
                    trace!("ServiceResolved: a candidate for {fullname} is unreachable: {e}");
                    continue;
                }
                Err(e) => {
                    warn!("ServiceResolved: probe task failed: {e}");
                    continue;
                }
            };
            let ip = addr.ip();

            // The frontend builds its target as `ip + ":" + port`, so an IPv6
            // literal has to carry its brackets.
            let ip_str = match ip {
                IpAddr::V6(v6) => format!("[{v6}]"),
                IpAddr::V4(v4) => v4.to_string(),
            };

            return Some(EndpointInfo {
                fullname: fullname.to_string(),
                id: addr.to_string(),
                name: Some(device_name.to_string()),
                ip: Some(ip_str),
                port: Some(port.to_string()),
                rtype: Some(device_type.clone()),
                present: Some(true),
                qr_match: Some(qr_match),
            });
        }

        None
    }
}

/// Whether a resolve should be probed: always for a peer we haven't cached,
/// and for a cached one only when the address we reach it on is no longer
/// among those it advertises, or its port changed.
fn needs_probe(cached: Option<&EndpointInfo>, candidates: &[IpAddr], port: u16) -> bool {
    let Some(cached) = cached else {
        return true;
    };
    match cached.id.parse::<SocketAddr>() {
        Ok(reached) => reached.port() != port || !candidates.contains(&reached.ip()),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(id: &str) -> EndpointInfo {
        EndpointInfo {
            id: id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_cached_peer_is_probed_again_only_when_it_moves() {
        let ip: IpAddr = "192.168.1.20".parse().unwrap();
        let other: IpAddr = "192.168.1.21".parse().unwrap();
        let peer = cached("192.168.1.20:5000");

        assert!(needs_probe(None, &[ip], 5000));
        assert!(!needs_probe(Some(&peer), &[ip], 5000));
        assert!(!needs_probe(Some(&peer), &[other, ip], 5000));
        assert!(needs_probe(Some(&peer), &[ip], 5001));
        assert!(needs_probe(Some(&peer), &[other], 5000));
    }

    /// An address that never answers must not hold up one that does.
    #[tokio::test]
    async fn an_unreachable_candidate_does_not_delay_a_reachable_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // TEST-NET-1: routable-looking, never answers, so its connect hangs.
        let unreachable: IpAddr = "192.0.2.1".parse().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();

        let found = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            MDnsDiscovery::probe_candidates(
                "peer",
                vec![unreachable, local],
                port,
                "Phone",
                &DeviceType::Phone,
                false,
            ),
        )
        .await
        .expect("the unreachable candidate held up the probe")
        .expect("the reachable candidate wasn't found");
        assert_eq!(found.ip.as_deref(), Some("127.0.0.1"));
        drop(listener);
    }
}
