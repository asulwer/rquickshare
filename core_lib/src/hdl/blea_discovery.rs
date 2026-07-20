//! Discover phones that are advertising as Quick Share *receivers* over BLE, so
//! we can send to one whose WiFi is off.
//!
//! `BleListener` already scans, but for a different purpose and a different
//! service: it watches `0xFE2C` to notice that *someone nearby is sharing* and
//! raises a one-bit signal. This one watches `0xFEF3` - the copresence service a
//! discoverable receiver advertises, the same one we advertise in
//! `blea_recv_win` - and surfaces each peer as an `EndpointInfo` the UI can list
//! and the send path can connect to.
//!
//! Addressing: a BLE peer has no IP, so it is reported as `ip = "ble"` and
//! `port = <bluetooth address>`. The frontend builds its target as
//! `ip + ":" + port`, giving `ble:<address>`, which `TcpServer::connect`
//! recognises and routes down the BLE path instead of `TcpStream::connect`.

use std::collections::HashMap;

use anyhow::anyhow;
use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager};
use futures::stream::StreamExt;
use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::EndpointInfo;
use crate::utils::DeviceType;

const INNER_NAME: &str = "BleDiscovery";

/// Peripherals we have discovered this session, by address.
///
/// The send path must not re-find a peer by scanning again: BLE addresses here
/// are random resolvable private addresses that rotate, so by the time the user
/// picks a target its address may already be stale - and a second concurrent
/// scan on a fresh adapter doesn't reliably repopulate `peripherals()` anyway.
/// Every attempt failed with "no longer advertising" for exactly that reason.
/// Hold the handle discovery already has instead.
static DISCOVERED: once_cell::sync::Lazy<
    std::sync::Mutex<HashMap<String, btleplug::platform::Peripheral>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

/// Every address a given device name has been seen at this session.
///
/// BLE addresses here are resolvable private addresses and **rotate** - one
/// Pixel produced three in two minutes. Each rotation looks like a new device,
/// which is why the same phone appeared in the list repeatedly, and why picking
/// an older entry failed with "Not connected": that address no longer exists.
/// The device *name* is the stable identity, so remember which addresses belong
/// to it and repoint all of them at the newest peripheral.
static NAME_ADDRESSES: once_cell::sync::Lazy<std::sync::Mutex<HashMap<String, Vec<String>>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

/// Record a peripheral under `name`, and repoint every address previously seen
/// for that name at it, so a stale entry in the UI still connects.
fn remember_peer(name: &str, address: &str, peripheral: &btleplug::platform::Peripheral) {
    let addresses = {
        let Ok(mut names) = NAME_ADDRESSES.lock() else {
            return;
        };
        let entry = names.entry(name.to_string()).or_default();
        if !entry.iter().any(|a| a == address) {
            entry.push(address.to_string());
        }
        entry.clone()
    };

    if let Ok(mut found) = DISCOVERED.lock() {
        for a in addresses {
            found.insert(a, peripheral.clone());
        }
    }
}

/// True if this name has already been listed, i.e. this is a rotated address for
/// a device the user can already see.
fn already_listed(name: &str) -> bool {
    NAME_ADDRESSES
        .lock()
        .map(|n| n.contains_key(name))
        .unwrap_or(false)
}

/// The peripheral discovery found at this address, if it is still known.
pub fn discovered_peripheral(address: &str) -> Option<btleplug::platform::Peripheral> {
    DISCOVERED
        .lock()
        .ok()
        .and_then(|m| m.get(address).cloned())
}

/// The copresence service a discoverable Quick Share receiver advertises.
const SERVICE_UUID_COPRESENCE: Uuid = super::COPRESENCE_SERVICE_UUID;

/// Slot 0 of the copresence service: the peer's full BleAdvertisement.
const ADV_SLOT0_UUID: Uuid = uuid::uuid!("00000000-0000-3000-8000-000000000000");

/// Connect briefly, read the peer's slot-0 advertisement, and pull the device
/// name and type out of it. `None` if it isn't a Quick Share receiver.
///
/// The connection is dropped afterwards: the send path reconnects when the user
/// actually picks this target. Bounded, because a device that accepts a
/// connection and then never answers would otherwise stall discovery.
async fn read_peer_advertisement(
    peripheral: &btleplug::platform::Peripheral,
) -> Option<(String, u8)> {
    let work = async {
        if !peripheral.is_connected().await.ok()? {
            peripheral.connect().await.ok()?;
        }
        peripheral.discover_services().await.ok()?;
        let slot0 = peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == ADV_SLOT0_UUID)?;
        let value = peripheral.read(&slot0).await.ok()?;
        super::parse_full_advertisement(&value)
    };

    let result = tokio::time::timeout(std::time::Duration::from_secs(8), work)
        .await
        .ok()
        .flatten();

    // Don't hold connections to every device in range.
    let _ = peripheral.disconnect().await;
    result
}

/// Names already reported over mDNS this session.
///
/// A phone with WiFi on is found by *both* transports and would otherwise be
/// listed twice, with no way for the user to tell which entry is which. LAN wins
/// when it is available: it is orders of magnitude faster than BLE's ~20 KB/s,
/// so a BLE entry for a peer we can already reach over the network is never the
/// one to pick.
static LAN_NAMES: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashSet<String>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Record a peer found over mDNS, so BLE discovery can suppress its duplicate.
pub fn note_lan_peer(name: &str) {
    if let Ok(mut s) = LAN_NAMES.lock() {
        s.insert(name.to_string());
    }
}

/// Forget the LAN peers seen so far. Called when discovery restarts, so a peer
/// that has since left the network isn't suppressed forever.
pub fn clear_lan_peers() {
    if let Ok(mut s) = LAN_NAMES.lock() {
        s.clear();
    }
    // Also forget which addresses belonged to which device: a new scan should
    // list what is actually in range now, not what was here an hour ago.
    if let Ok(mut n) = NAME_ADDRESSES.lock() {
        n.clear();
    }
    if let Ok(mut d) = DISCOVERED.lock() {
        d.clear();
    }
}

fn known_on_lan(name: &str) -> bool {
    LAN_NAMES.lock().map(|s| s.contains(name)).unwrap_or(false)
}

pub struct BleDiscovery {
    adapter: Adapter,
    sender: Sender<EndpointInfo>,
}

impl BleDiscovery {
    pub async fn new(sender: Sender<EndpointInfo>) -> Result<Self, anyhow::Error> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        if adapters.is_empty() {
            return Err(anyhow!("no bluetooth adapter"));
        }

        Ok(Self {
            adapter: adapters[0].clone(),
            sender,
        })
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        let mut events = self.adapter.events().await?;
        // Unfiltered. btleplug's ScanFilter is best-effort and the Windows
        // backend largely ignores it, so filtering at the adapter risks getting
        // nothing at all while looking like "no peer is advertising". Match on
        // the UUID below instead, where we can also log what we *did* see.
        self.adapter.start_scan(ScanFilter::default()).await?;

        // Report each peer once. BLE advertisements repeat several times a
        // second, and a phone using a rotating random address would otherwise
        // pile up duplicates in the UI.
        let mut seen: HashMap<String, ()> = HashMap::new();

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                Some(e) = events.next() => {
                    let mut advertised: Option<Vec<u8>> = None;
                    let id = match e {
                        CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                            if let Some(data) = service_data.get(&SERVICE_UUID_COPRESENCE) {
                                // The bytes, not just the key. Logging only the
                                // UUID meant a parse failure was indistinguishable
                                // from an empty payload, and cost a test cycle.
                                trace!(
                                    "{INNER_NAME}: 0xFEF3 data from {id:?}: {:02x?}",
                                    data
                                );
                                advertised = Some(data.clone());
                            }
                            // Log every advertiser's services once we know we're
                            // seeing traffic at all. "No peer advertising" and
                            // "we never get the event" look identical without
                            // this, and cost a test cycle each time.
                            trace!(
                                "{INNER_NAME}: service data from {id:?}: {:?}",
                                service_data.keys().collect::<Vec<_>>()
                            );
                            if !service_data.contains_key(&SERVICE_UUID_COPRESENCE) {
                                continue;
                            }
                            id
                        }
                        // Windows often reports the UUID without the payload,
                        // because the advertisement hits the same 31-byte limit
                        // that strips our own (status 4). The UUID alone is
                        // enough to know it is worth connecting to.
                        CentralEvent::ServicesAdvertisement { id, services } => {
                            trace!("{INNER_NAME}: services from {id:?}: {services:?}");
                            if !services.contains(&SERVICE_UUID_COPRESENCE) {
                                continue;
                            }
                            id
                        }
                        _ => continue,
                    };

                    let peripheral = match self.adapter.peripheral(&id).await {
                        Ok(p) => p,
                        Err(e) => {
                            debug!("{INNER_NAME}: no peripheral for {id:?}: {e}");
                            continue;
                        }
                    };

                    let address = peripheral.address().to_string();
                    if seen.insert(address.clone(), ()).is_some() {
                        continue;
                    }

                    // Identify from the advertisement first, and only fall back
                    // to a GATT read.
                    //
                    // Reading slot 0 was the wrong default: *we* serve it over
                    // GATT because Windows drops our 26-byte payload from the
                    // 31-byte advertisement (status 4), but Android fits the
                    // fast form inline and need not expose the characteristic at
                    // all. Connecting to 11 devices and failing to read any was
                    // that assumption, not 11 devices being uninteresting.
                    //
                    // 0xFEF3 is a shared copresence service, so a peer whose
                    // advertisement doesn't parse as NearbySharing isn't a
                    // target - one answered our multiplex request with service
                    // hash b7ef32 rather than fc9f5e.
                    let parsed = advertised
                        .as_deref()
                        .and_then(super::parse_peer_advertisement);

                    let (name, device_type) = match parsed {
                        // Full form: the name came with it.
                        Some(p) if p.name.is_some() => (p.name.unwrap(), p.device_type),
                        // Fast form: no room for a name, so ask for it over
                        // GATT and fall back to the endpoint id if the peer has
                        // no slot 0 to read.
                        Some(p) => match read_peer_advertisement(&peripheral).await {
                            Some(v) => v,
                            None => (
                                format!(
                                    "Quick Share device ({})",
                                    String::from_utf8_lossy(&p.endpoint_id)
                                ),
                                p.device_type,
                            ),
                        },
                        None => {
                            debug!(
                                "{INNER_NAME}: {address} advertises 0xFEF3 but not NearbySharing, \
                                 skipping"
                            );
                            continue;
                        }
                    };

                    // Already reachable over the network - don't offer a second,
                    // far slower way to reach the same phone.
                    if known_on_lan(&name) {
                        debug!(
                            "{INNER_NAME}: {name} is already on the LAN, not listing it over BLE"
                        );
                        continue;
                    }

                    // A rotated address for a device already in the list. Point
                    // its old addresses at this peripheral so the entry the user
                    // is looking at still works, but don't list it again.
                    if already_listed(&name) {
                        remember_peer(&name, &address, &peripheral);
                        debug!("{INNER_NAME}: {name} reappeared at {address}, refreshed");
                        continue;
                    }
                    remember_peer(&name, &address, &peripheral);

                    let ei = EndpointInfo {
                        fullname: address.clone(),
                        id: address.clone(),
                        name: Some(name),
                        ip: Some("ble".to_string()),
                        port: Some(address.clone()),
                        rtype: Some(DeviceType::from_raw_value(device_type)),
                        present: Some(true),
                        qr_match: Some(false),
                    };
                    info!("{INNER_NAME}: discovered BLE receiver: {ei:?}");
                    let _ = self.sender.send(ei);
                }
            }
        }

        let _ = self.adapter.stop_scan().await;
        Ok(())
    }
}
