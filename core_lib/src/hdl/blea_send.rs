//! BLE *client* side of the Nearby Weave socket, for sending to a phone whose
//! WiFi is off.
//!
//! The mirror of `blea_recv_win.rs`. There we are the GATT server: the phone
//! writes to our client-tx characteristic and we notify on server-tx. Here the
//! phone is the server, so we write to *its* client-tx and subscribe to *its*
//! server-tx. Same UUIDs, opposite direction.
//!
//! Uses btleplug rather than WinRT: it already backs `BleListener`, needs far
//! less ceremony than `BluetoothLEDevice`, and works on Linux - so unlike the
//! receive path this is cross-platform.
//!
//! The result carries exactly the `[u32 len][OfflineFrame]` stream that
//! `OutboundRequest` expects, so the Nearby stack rides it unchanged.

use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::stream::StreamExt;
use rand::distr::Alphanumeric;
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::{uuid, Uuid};

const INNER_NAME: &str = "BleSender";

/// Nearby's copresence service, advertised by a discoverable receiver.
pub const COPRESENCE_SERVICE_UUID: Uuid = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");

/// client -> server. We write here.
const BLE_SOCKET_CLIENT_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000101");
/// server -> client. We subscribe here.
const BLE_SOCKET_SERVER_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000102");

/// SHA-256("NearbySharing")[:3].
const NEARBY_SHARING_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];

/// 512-byte ATT MTU ceiling minus the 3-byte ATT header.
const MAX_PACKET_SIZE: u16 = 509;

/// Sanity bound on a frame length, matching the rest of the stack.
const SANE_FRAME_LENGTH: usize = 5 * 1024 * 1024;

/// A BLE Weave socket plus the hooks needed to upgrade off it.
///
/// A phone acting as receiver will not offer us an upgrade - asked directly with
/// UPGRADE_PATH_REQUEST it answers nothing - so if a send is to leave BLE, *we*
/// must host the new medium and this transport must be able to adopt it.
pub struct BleChannel {
    /// Hand to `OutboundRequest`.
    pub stream: tokio::io::DuplexStream,
    /// Where the upgrade listener delivers the socket the peer connected on.
    pub upgrade_tx: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
    /// Signals that the old channel is released and the stream may move.
    pub switch_tx: tokio::sync::mpsc::UnboundedSender<()>,
}

/// The multiplex ConnectionRequest the peer expects before any Nearby data.
///
/// Byte-for-byte the frame the phone sends us when it is the client:
/// `00 00 00 | 08 01 | 12 1f { 0a 03 <hash> | 10 02 | 1a 16 <22-byte token> }`.
fn build_multiplex_connection_request() -> Vec<u8> {
    let token: Vec<u8> = rand::rng().sample_iter(Alphanumeric).take(22).collect();

    let mut inner = Vec::new();
    inner.extend_from_slice(&[0x0a, 0x03]);
    inner.extend_from_slice(&NEARBY_SHARING_HASH);
    inner.extend_from_slice(&[0x10, 0x02, 0x1a]);
    inner.push(token.len() as u8);
    inner.extend_from_slice(&token);

    let mut out = Vec::new();
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x08, 0x01, 0x12]);
    out.push(inner.len() as u8);
    out.extend_from_slice(&inner);
    out
}

fn find_characteristic(peripheral: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == uuid)
}

/// Open a Weave socket to the peer discovery found at this address.
///
/// Deliberately does *not* scan. These are resolvable private addresses that
/// rotate, so re-finding one by address is unreliable by design, and a second
/// concurrent scan on a fresh adapter does not reliably repopulate
/// `peripherals()` - every attempt failed with "no longer advertising".
pub async fn open_ble_by_address(address: &str) -> Result<BleChannel, anyhow::Error> {
    let peripheral = super::discovered_peripheral(address).ok_or_else(|| {
        anyhow::anyhow!(
            "BLE peer {address} was not found by discovery this session; rescan and try again"
        )
    })?;

    // Try the cached handle, then refresh once.
    //
    // Handles go stale: these addresses rotate every couple of minutes, and once
    // the peer has moved the cached handle fails with "Not connected" even
    // though the peer is present and healthy. Discovery only refreshes while it
    // is running, which it is not once the user has stopped scanning - so a send
    // shortly after a scan works and one a few minutes later does not.
    match open(peripheral).await {
        Ok(channel) => Ok(channel),
        Err(first) => {
            warn!("{INNER_NAME}: {first}; the handle may have gone stale, rescanning");
            let fresh = super::refresh_peripheral(address)
                .await
                .ok_or_else(|| anyhow::anyhow!("{first}"))?;
            open(fresh).await
        }
    }
}

/// Connect, complete the Weave handshake as the client, and return the channel.
pub async fn open(peripheral: Peripheral) -> Result<BleChannel, anyhow::Error> {
    // Was the link already held? If a send fails right after a receive, this
    // says whether we were contending with a connection that should have been
    // dropped - which is the standing theory for why alternating directions is
    // unreliable.
    let already = peripheral.is_connected().await.unwrap_or(false);
    info!("{INNER_NAME}: peer already connected = {already}");
    if !already {
        peripheral.connect().await?;
    }
    peripheral.discover_services().await?;

    let client_tx = find_characteristic(&peripheral, BLE_SOCKET_CLIENT_TX_UUID).ok_or_else(|| {
        anyhow::anyhow!("peer has no Nearby BLE socket client-tx characteristic (...0101)")
    })?;
    let server_tx = find_characteristic(&peripheral, BLE_SOCKET_SERVER_TX_UUID).ok_or_else(|| {
        anyhow::anyhow!("peer has no Nearby BLE socket server-tx characteristic (...0102)")
    })?;

    peripheral.subscribe(&server_tx).await?;
    let mut notifications = peripheral.notifications().await?;
    info!("{INNER_NAME}: subscribed to server-tx");

    // Weave ConnectionRequest. Counter 0 - control and data share one counter
    // per direction, which the receive path learned the hard way.
    let request = [
        0x80,
        0x00,
        0x01,
        0x00,
        0x01,
        (MAX_PACKET_SIZE >> 8) as u8,
        MAX_PACKET_SIZE as u8,
    ];
    peripheral
        .write(&client_tx, &request, WriteType::WithoutResponse)
        .await?;
    info!("{INNER_NAME}: sent Weave ConnectionRequest");

    let packet_size = loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), notifications.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for Weave ConnectionConfirm"))?
            .ok_or_else(|| anyhow::anyhow!("notification stream ended during handshake"))?;

        if n.uuid != BLE_SOCKET_SERVER_TX_UUID || n.value.is_empty() {
            continue;
        }
        let header = n.value[0];
        if header & 0x80 == 0 {
            warn!("{INNER_NAME}: data packet before ConnectionConfirm, ignoring");
            continue;
        }
        match header & 0x0f {
            1 if n.value.len() >= 5 => {
                let size = u16::from_be_bytes([n.value[3], n.value[4]]).min(MAX_PACKET_SIZE);
                info!("{INNER_NAME}: Weave ConnectionConfirm, packet size {size}");
                break size;
            }
            2 => return Err(anyhow::anyhow!("peer refused the Weave connection (Error)")),
            other => warn!("{INNER_NAME}: unexpected control command {other} during handshake"),
        }
    };

    // Small on purpose: writes must block until bytes are genuinely on their
    // way, so progress and "finished" mean something. See the git history - a
    // 256 KB buffer let OutboundRequest report Finished in one second while the
    // radio was minutes behind.
    // 16 KB, and measured to be the sweet spot (2026-07-21). Tried larger both
    // ways - 1 MB chunks and a 256 KB duplex - and both were *slower* over the
    // upgraded WiFi link, not faster. The small duplex keeps OutboundRequest
    // tightly coupled to actual send progress, so data streams smoothly in
    // TCP-sized pieces rather than in bursts; it is also what keeps "Finished"
    // honest on BLE-only transfers. Don't enlarge this without a measurement
    // showing it helps.
    let (near, far) = tokio::io::duplex(16 * 1024);

    let (upgrade_tx, mut upgrade_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::net::TcpStream>();
    let (switch_tx, mut switch_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // One task, not two.
    //
    // Reading and writing the duplex have to end up in the same place, because
    // an upgrade splices a TCP socket onto *both* halves - and two independent
    // pumps also meant that when the peer closed, the sender stayed blocked on
    // an acknowledgement that would never come, hanging the transfer with no
    // error and no way for cancel to reach it.
    tokio::spawn(async move {
        let (mut far_rd, mut far_wr) = tokio::io::split(far);

        // Counter 0 was the ConnectionRequest, so data starts at 1.
        let mut counter: u8 = 1;
        let max_payload = (packet_size as usize).saturating_sub(1).max(1);
        let mut hash = NEARBY_SHARING_HASH;
        let mut pending: Vec<u8> = Vec::new();
        let mut acc: Vec<u8> = Vec::new();
        let mut upgraded: Option<tokio::net::TcpStream> = None;
        // A Quick Share receiver answers our multiplex ConnectionRequest. One
        // that completes the handshake then closes without a word is
        // advertising Nearby presence but is not on the receiving screen.
        let mut heard_from_peer = false;

        let mux = build_multiplex_connection_request();
        if let Err(e) = send_weave(&peripheral, &client_tx, &mux, &mut counter, max_payload).await {
            warn!("{INNER_NAME}: failed to send multiplex ConnectionRequest: {e}");
            return;
        }

        loop {
            let mut buf = [0u8; 8192];
            tokio::select! {
                // Outbound. `read` is cancel-safe; `read_exact` is not, which
                // matters now that it shares a select with other branches.
                r = far_rd.read(&mut buf) => {
                    let n = match r {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    pending.extend_from_slice(&buf[..n]);

                    // Send whole frames only. Nothing is read while a frame is
                    // going out, because send_weave awaits each acknowledged
                    // write - that is the backpressure.
                    while pending.len() >= 4 {
                        let len = u32::from_be_bytes(
                            [pending[0], pending[1], pending[2], pending[3]],
                        ) as usize;
                        if len == 0 || len > SANE_FRAME_LENGTH {
                            warn!("{INNER_NAME}: refusing insane outbound frame length {len}");
                            return;
                        }
                        if pending.len() < 4 + len {
                            break;
                        }

                        let mut msg = Vec::with_capacity(3 + 4 + len);
                        msg.extend_from_slice(&hash);
                        msg.extend_from_slice(&pending[..4 + len]);
                        pending.drain(..4 + len);

                        if let Err(e) = send_weave(
                            &peripheral, &client_tx, &msg, &mut counter, max_payload,
                        ).await {
                            warn!("{INNER_NAME}: weave send failed: {e}");
                            return;
                        }
                    }
                }

                // Inbound.
                note = notifications.next() => {
                    let Some(n) = note else { break };
                    if n.uuid != BLE_SOCKET_SERVER_TX_UUID || n.value.is_empty() {
                        continue;
                    }
                    let header = n.value[0];
                    if header & 0x80 != 0 {
                        // Weave has no graceful close, so a completed transfer
                        // ends with an Error just as a failed one does.
                        if header & 0x0f == 2 {
                            info!("{INNER_NAME}: peer closed the Weave connection");
                            break;
                        }
                        continue;
                    }

                    if header & 0x08 != 0 {
                        acc.clear();
                    }
                    acc.extend_from_slice(&n.value[1..]);
                    if header & 0x04 == 0 {
                        continue;
                    }

                    let msg = std::mem::take(&mut acc);
                    if msg.len() <= 3 {
                        continue;
                    }
                    // `00 00 00` is multiplex control, which the stack above
                    // must not see. Anything else is the peer's service hash -
                    // not always fc9f5e, and per-session.
                    if msg[..3] == [0x00, 0x00, 0x00] {
                        debug!(
                            "{INNER_NAME}: multiplex control {:02x?}",
                            &msg[..msg.len().min(24)]
                        );
                        continue;
                    }
                    if hash != msg[..3] {
                        let learned: [u8; 3] = [msg[0], msg[1], msg[2]];
                        info!(
                            "{INNER_NAME}: peer is using service hash {learned:02x?} \
                             (was {hash:02x?})"
                        );
                        hash = learned;
                    }
                    heard_from_peer = true;
                    if far_wr.write_all(&msg[3..]).await.is_err() {
                        break;
                    }
                }

                // The peer connected on the medium we offered. Hold it: it will
                // not use the socket until the old channel is released.
                Some(sock) = upgrade_rx.recv() => {
                    info!("{INNER_NAME}: upgraded socket ready, holding for release");
                    upgraded = Some(sock);
                }

                // The old channel has been released.
                Some(_) = switch_rx.recv() => {
                    match upgraded.take() {
                        Some(mut sock) => {
                            info!("{INNER_NAME}: switching the stream onto the upgraded socket");

                            // Carry anything half-read across.
                            //
                            // `pending` holds bytes taken off the duplex that
                            // have not yet formed a whole frame. Dropping them
                            // here loses the middle of the stream: the peer sits
                            // waiting for the rest of a frame that no longer
                            // exists, which is a transfer that stalls partway
                            // with the sender believing it sent everything.
                            if !pending.is_empty() {
                                info!(
                                    "{INNER_NAME}: carrying {} B of partial frame onto the \
                                     upgraded socket",
                                    pending.len()
                                );
                                if let Err(e) = sock.write_all(&pending).await {
                                    warn!("{INNER_NAME}: could not carry the partial frame: {e}");
                                    return;
                                }
                                pending.clear();
                            }

                            // Do NOT close BLE here - it strands the transfer.
                            //
                            // Tried it on the theory that the phone waits for the
                            // weave socket to disconnect; the opposite happened.
                            // Closing BLE right after the switch tore the phone's
                            // weave socket down (its "onDisconnected callback"
                            // fires, then a GATT connection timeout) and it never
                            // started reading the WiFi socket - the transfer
                            // froze at exactly the bytes BLE had delivered
                            // (98 KB). The runs that completed at 6 MB/s kept BLE
                            // open. Leave it; it is dropped at pump end as usual.

                            // Rejoin the halves: TCP framing is exactly what is
                            // already flowing, so nothing needs translating.
                            let mut joined = tokio::io::join(far_rd, far_wr);
                            match tokio::io::copy_bidirectional(&mut sock, &mut joined).await {
                                Ok((from_peer, to_peer)) => info!(
                                    "{INNER_NAME}: upgraded socket closed \
                                     ({from_peer} B in, {to_peer} B out)"
                                ),
                                Err(e) => warn!("{INNER_NAME}: upgraded socket failed: {e}"),
                            }
                            return;
                        }
                        None => warn!("{INNER_NAME}: asked to switch with no upgraded socket"),
                    }
                }
            }
        }

        // Let go of the peer.
        //
        // We hold a GATT *client* connection for the whole send and never
        // released it, so after a PC -> phone transfer the phone stayed
        // connected to us - and stopped being discoverable as a receiver. The
        // symptom was the PC vanishing from the phone's list and phone -> PC
        // failing until something reset the link.
        if let Err(e) = peripheral.disconnect().await {
            debug!("{INNER_NAME}: disconnect failed: {e}");
        }

        if heard_from_peer {
            info!("{INNER_NAME}: pump ended");
        } else {
            warn!(
                "{INNER_NAME}: peer accepted the Weave connection then closed it without a word - \
                 it is advertising Nearby but is not on the Quick Share receiving screen"
            );
        }
    });

    Ok(BleChannel {
        stream: near,
        upgrade_tx,
        switch_tx,
    })
}

/// Fragment `bytes` into Weave data packets and write them to the peer.
///
/// Header: `(counter << 4) | (first << 3) | (last << 2)`, the counter shared
/// across control and data, wrapping mod 8.
async fn send_weave(
    peripheral: &Peripheral,
    characteristic: &Characteristic,
    bytes: &[u8],
    counter: &mut u8,
    max_payload: usize,
) -> Result<(), anyhow::Error> {
    let chunks: Vec<&[u8]> = bytes.chunks(max_payload).collect();
    let n = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let header = ((*counter & 0x07) << 4)
            | if i == 0 { 0x08 } else { 0 }
            | if i + 1 == n { 0x04 } else { 0 };
        *counter = counter.wrapping_add(1) & 0x07;

        let mut pkt = Vec::with_capacity(1 + chunk.len());
        pkt.push(header);
        pkt.extend_from_slice(chunk);

        // WithResponse, not WithoutResponse. A write command is fire-and-forget:
        // the stack accepts every packet instantly and queues it, so this
        // returned immediately while the radio was minutes behind. Waiting for
        // the peer's ATT acknowledgement is the only real backpressure here.
        //
        // Bounded, because a write to a peer that has gone away otherwise waits
        // forever - which turned a dead link into a hung transfer.
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            peripheral.write(characteristic, &pkt, WriteType::WithResponse),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "peer did not acknowledge a write within 20s; treating the link as dead"
                ))
            }
        }
    }
    Ok(())
}
