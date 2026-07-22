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

/// The device name we listed this address under, if any.
fn name_for_address(address: &str) -> Option<String> {
    NAME_ADDRESSES.lock().ok().and_then(|n| {
        n.iter()
            .find(|(_, addrs)| addrs.iter().any(|a| a == address))
            .map(|(name, _)| name.clone())
    })
}

/// Scan briefly and return a *fresh* handle for whatever device was listed at
/// `address`.
///
/// Handles go stale. These are resolvable private addresses that rotate every
/// couple of minutes, and the handle we cached at discovery time stops working
/// once the peer has moved - `connect()` then fails with "Not connected" even
/// though nothing is wrong with the peer or the link. Discovery only refreshes
/// while it is running, which it is not once the user has stopped scanning.
///
/// Matching is by *name*, because that is the only stable identity here - the
/// address we were given is by definition the one that expired.
pub async fn refresh_peripheral(address: &str) -> Option<btleplug::platform::Peripheral> {
    use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
    use btleplug::platform::Manager;

    let wanted = name_for_address(address)?;
    info!("{INNER_NAME}: refreshing a stale handle for {wanted}");

    let manager = Manager::new().await.ok()?;
    let adapter = manager.adapters().await.ok()?.into_iter().next()?;
    adapter.start_scan(ScanFilter::default()).await.ok()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut found = None;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let Ok(peripherals) = adapter.peripherals().await else {
            continue;
        };
        for p in peripherals {
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            if !props.services.contains(&SERVICE_UUID_COPRESENCE) {
                continue;
            }
            // Confirm it is the same device by reading its advertisement, the
            // same way discovery identifies peers in the first place.
            if let Some((name, _)) = read_peer_advertisement(&p).await {
                if name == wanted {
                    let addr = p.address().to_string();
                    remember_peer(&name, &addr, &p);
                    info!("{INNER_NAME}: {wanted} is now at {addr}");
                    found = Some(p);
                    break;
                }
            }
        }
        if found.is_some() {
            break;
        }
    }

    let _ = adapter.stop_scan().await;
    if found.is_none() {
        warn!("{INNER_NAME}: could not find {wanted} again; it may be out of range");
    }
    found
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

/// Where to publish endpoint changes, so a peer that turns up on the LAN can
/// retract its own BLE listing. Set while `BleDiscovery` is running.
static BLE_SENDER: once_cell::sync::Lazy<std::sync::Mutex<Option<Sender<EndpointInfo>>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// Record a peer found over mDNS, and retract any BLE entry for the same device.
///
/// Event-driven supersede, replacing a fixed head start. BLE advertisements
/// arrive in about a second while an mDNS service takes longer to resolve, so
/// the old code simply *waited* 4s before listing anything over BLE, hoping mDNS
/// would win. That is a guess with a timer: too short and a LAN peer still gets
/// listed as a slow BLE target, too long and every genuinely BLE-only peer is
/// delayed for nothing.
///
/// Instead, list BLE immediately and let this retract it when the mDNS event
/// actually arrives. The user sees peers ~4s sooner, and LAN still wins whenever
/// it is available - decided by the event rather than by a deadline.
pub fn note_lan_peer(name: &str) {
    let newly_known = LAN_NAMES
        .lock()
        .map(|mut s| s.insert(name.to_string()))
        .unwrap_or(false);
    if !newly_known {
        return;
    }

    // Drop any BLE entries already shown for this device. The frontend removes
    // an endpoint sent with only its id, the same way mDNS reports a service
    // going away.
    let addresses = NAME_ADDRESSES
        .lock()
        .ok()
        .and_then(|n| n.get(name).cloned())
        .unwrap_or_default();
    if addresses.is_empty() {
        return;
    }
    if let Ok(guard) = BLE_SENDER.lock() {
        if let Some(sender) = guard.as_ref() {
            for id in addresses {
                debug!("{INNER_NAME}: {name} is on the LAN now, retracting its BLE entry {id}");
                let _ = sender.send(EndpointInfo {
                    id,
                    ..Default::default()
                });
            }
        }
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

/// How many transfers are in flight. Discovery stops scanning while any are.
///
/// A BLE scan and a BLE connection compete for the same radio: scanning through
/// a transfer cut throughput from ~20 KB/s to 3-4 KB/s, and a starved link
/// during connection setup is also the likeliest source of the first-connection
/// handshake failures. A `watch` rather than a Notify because discovery must not
/// miss the change - once scanning stops there are no BLE events to wake it, so
/// a lost wakeup would suppress scanning permanently.
static TRANSFER_COUNT: once_cell::sync::Lazy<tokio::sync::watch::Sender<usize>> =
    once_cell::sync::Lazy::new(|| tokio::sync::watch::channel(0usize).0);

/// Pauses BLE discovery for as long as it is held.
///
/// RAII on purpose: a transfer can end by completion, error, cancel or a dropped
/// task, and every one of those must release the pause. A manual
/// `transfer_finished()` would eventually be missed on some error path and leave
/// the app permanently unable to discover anything.
pub struct DiscoveryPause;

impl DiscoveryPause {
    pub fn new() -> Self {
        TRANSFER_COUNT.send_modify(|n| *n += 1);
        Self
    }
}

impl Default for DiscoveryPause {
    fn default() -> Self {
        Self::new()
    }
}

/// Outbound BLE transfers only - see `AdvertisePause`.
static BLE_SEND_COUNT: once_cell::sync::Lazy<std::sync::atomic::AtomicUsize> =
    once_cell::sync::Lazy::new(|| std::sync::atomic::AtomicUsize::new(0));

/// Pauses the BLE *receiver advertisement* while we are sending over BLE.
///
/// Deliberately separate from `DiscoveryPause` rather than sharing its counter.
/// `DiscoveryPause` is also held by inbound sessions, and stopping the
/// advertisement is exactly how we drop a peer's GATT connection (see the
/// recycle in `blea_recv_win`) - so pausing on an inbound transfer would tear
/// down the very session being paused for. This counts outbound BLE sends only,
/// where our GATT server has no session to lose.
pub struct AdvertisePause;

impl AdvertisePause {
    pub fn new() -> Self {
        BLE_SEND_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Default for AdvertisePause {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AdvertisePause {
    fn drop(&mut self) {
        BLE_SEND_COUNT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Is a BLE send running right now?
pub fn ble_send_in_progress() -> bool {
    BLE_SEND_COUNT.load(std::sync::atomic::Ordering::SeqCst) > 0
}

impl Drop for DiscoveryPause {
    fn drop(&mut self) {
        TRANSFER_COUNT.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Publish a peer, unless it is reachable over the LAN or already listed.
///
/// Free-standing so the concurrent identification tasks can call it; the shared
/// maps are global and mutex-guarded, so this is safe from any task.
fn list_peer(
    sender: &Sender<EndpointInfo>,
    name: String,
    device_type: u8,
    address: &str,
    peripheral: &btleplug::platform::Peripheral,
) {
    // Already reachable over the network - don't offer a second, far slower way
    // to reach the same phone.
    if known_on_lan(&name) {
        debug!("{INNER_NAME}: {name} is already on the LAN, not listing it over BLE");
        return;
    }

    // A rotated address for a device already in the list. Point its old
    // addresses at this peripheral so the entry the user is looking at still
    // works, but don't list it again.
    if already_listed(&name) {
        remember_peer(&name, address, peripheral);
        debug!("{INNER_NAME}: {name} reappeared at {address}, refreshed");
        return;
    }
    remember_peer(&name, address, peripheral);

    let ei = EndpointInfo {
        fullname: address.to_string(),
        id: address.to_string(),
        name: Some(name),
        ip: Some("ble".to_string()),
        port: Some(address.to_string()),
        rtype: Some(DeviceType::from_raw_value(device_type)),
        present: Some(true),
        qr_match: Some(false),
    };
    info!("{INNER_NAME}: discovered BLE receiver: {ei:?}");
    let _ = sender.send(ei);
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

        // Publish through this while we run, so `note_lan_peer` can retract a
        // BLE entry the moment mDNS resolves the same device.
        if let Ok(mut guard) = BLE_SENDER.lock() {
            *guard = Some(self.sender.clone());
        }

        // Owns the concurrent identification tasks. A JoinSet rather than bare
        // `tokio::spawn` so they are reaped as they finish and, more
        // importantly, aborted when the scan stops - a cancelled scan must not
        // leave GATT connections being opened in the background.
        let mut identifiers: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        // Watch for transfers so we can get off the radio while one runs.
        let mut transfers = TRANSFER_COUNT.subscribe();
        let mut scanning = true;
        // Peripherals an identification task currently holds open. Aborting a
        // task kills it at an await point, so its own `disconnect` never runs -
        // and a GATT connection we keep open stops the peer being discoverable
        // (measured yesterday: the PC vanished from the phone's list until the
        // link was released). Shutdown disconnects whatever is left here.
        let in_flight: std::sync::Arc<
            std::sync::Mutex<HashMap<String, btleplug::platform::Peripheral>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                // Get off the radio while a transfer is running, and back on
                // when it finishes. Scanning through a transfer starves the
                // connection - measured at 3-4 KB/s instead of ~20.
                Ok(()) = transfers.changed() => {
                    let busy = *transfers.borrow() > 0;
                    if busy && scanning {
                        info!("{INNER_NAME}: transfer in progress, pausing the scan");
                        let _ = self.adapter.stop_scan().await;
                        scanning = false;
                    } else if !busy && !scanning {
                        info!("{INNER_NAME}: transfers finished, resuming the scan");
                        if let Err(e) = self.adapter.start_scan(ScanFilter::default()).await {
                            warn!("{INNER_NAME}: could not resume scanning: {e}");
                        }
                        scanning = true;
                    }
                }
                // Reap finished identifications so the set doesn't grow for the
                // life of a long scan. Guarded because `join_next` on an empty
                // set returns immediately.
                Some(res) = identifiers.join_next(), if !identifiers.is_empty() => {
                    if let Err(e) = res {
                        if !e.is_cancelled() {
                            debug!("{INNER_NAME}: peer identification task failed: {e}");
                        }
                    }
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
                    let Some(p) = advertised
                        .as_deref()
                        .and_then(super::parse_peer_advertisement)
                    else {
                        debug!(
                            "{INNER_NAME}: {address} advertises 0xFEF3 but not NearbySharing, \
                             skipping"
                        );
                        continue;
                    };

                    match p.name {
                        // Full form: the name came with it, so publish straight
                        // away - no connection needed.
                        Some(name) => {
                            list_peer(&self.sender, name, p.device_type, &address, &peripheral)
                        }
                        // Fast form carries no name, so it has to be read over
                        // GATT. Do that concurrently: the read costs a connect
                        // and is bounded at 8s, and inline it stalled the whole
                        // event loop - one unresponsive device delayed every
                        // other peer's listing behind it.
                        None => {
                            let sender = self.sender.clone();
                            let peripheral = peripheral.clone();
                            let address = address.clone();
                            let endpoint_id = p.endpoint_id;
                            let device_type = p.device_type;
                            let in_flight = in_flight.clone();
                            if let Ok(mut m) = in_flight.lock() {
                                m.insert(address.clone(), peripheral.clone());
                            }
                            identifiers.spawn(async move {
                                let (name, device_type) =
                                    match read_peer_advertisement(&peripheral).await {
                                        Some(v) => v,
                                        None => (
                                            format!(
                                                "Quick Share device ({})",
                                                String::from_utf8_lossy(&endpoint_id)
                                            ),
                                            device_type,
                                        ),
                                    };
                                // Ran to completion, so it closed its own
                                // connection - nothing for shutdown to clean up.
                                if let Ok(mut m) = in_flight.lock() {
                                    m.remove(&address);
                                }
                                list_peer(&sender, name, device_type, &address, &peripheral);
                            });
                        }
                    }
                }
            }
        }

        // Abort any identification still in flight and wait for it to unwind,
        // so nothing is left opening GATT connections after the scan is over.
        identifiers.shutdown().await;

        // Release links the aborted tasks were holding. They died at an await,
        // so their own disconnect never ran.
        let stranded: Vec<btleplug::platform::Peripheral> = in_flight
            .lock()
            .map(|mut m| m.drain().map(|(_, p)| p).collect())
            .unwrap_or_default();
        for p in stranded {
            debug!("{INNER_NAME}: releasing a peripheral left connected by an aborted identify");
            let _ = p.disconnect().await;
        }

        let _ = self.adapter.stop_scan().await;
        // Stop retracting through a sender that is no longer being listened to.
        if let Ok(mut guard) = BLE_SENDER.lock() {
            *guard = None;
        }
        Ok(())
    }
}
