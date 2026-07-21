//! BLE *client* side of the Nearby Weave socket, for sending to a phone whose
//! WiFi is off.
//!
//! This is the mirror of `blea_recv_win.rs`. There we are the GATT server: the
//! phone writes to our client-tx characteristic and we notify on server-tx.
//! Here the phone is the server, so we write to *its* client-tx and subscribe to
//! *its* server-tx. The characteristic UUIDs are the same; only the direction
//! changes.
//!
//! Unlike the receiver this uses btleplug rather than WinRT directly. btleplug
//! already backs `BleListener`, does GATT client work on both Windows and Linux,
//! and needs far less ceremony than `BluetoothLEDevice` - so the send path gets
//! Linux support for free, which the receive path still lacks.
//!
//! The result is a `DuplexStream` carrying exactly the `[u32 len][OfflineFrame]`
//! byte stream that `OutboundRequest` expects, so the whole Nearby stack rides
//! on it unchanged - the same trick that made the receive path work.

use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::stream::StreamExt;
use rand::distr::Alphanumeric;
use rand::Rng;
use uuid::{uuid, Uuid};

const INNER_NAME: &str = "BleSender";

/// Nearby's copresence service. The phone advertises this when it is
/// discoverable as a receiver.
pub const COPRESENCE_SERVICE_UUID: Uuid = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");

/// client -> server. We write here.
const BLE_SOCKET_CLIENT_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000101");
/// server -> client. We subscribe here.
const BLE_SOCKET_SERVER_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000102");

/// SHA-256("NearbySharing")[:3], the multiplex service id hash.
const NEARBY_SHARING_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];

/// Largest Weave packet we will accept, and the most we ask for. 509 = the
/// 512-byte ATT MTU ceiling minus the 3-byte ATT header, which is what the
/// phone offers us when it is the client.
const MAX_PACKET_SIZE: u16 = 509;

/// Sanity bound on a frame length read off the duplex, matching the rest of the
/// stack. Guards against a garbage length turning into a huge allocation.
const SANE_FRAME_LENGTH: usize = 5 * 1024 * 1024;

/// The multiplex ConnectionRequest the peer expects before any Nearby data.
///
/// Byte-for-byte the frame the phone sends us when it is the client, captured
/// from a real transfer:
///
/// ```text
/// 00 00 00                  multiplex control prefix
/// 08 01                     field 1 varint = 1
/// 12 1f                     field 2, length 31
///    0a 03 fc 9f 5e         service_id_hash = SHA-256("NearbySharing")[:3]
///    10 02                  field 2 varint = 2
///    1a 16 <22 bytes>       field 3, a random session token
/// ```
fn build_multiplex_connection_request() -> Vec<u8> {
    let token: Vec<u8> = rand::rng().sample_iter(Alphanumeric).take(22).collect();

    let mut inner = Vec::new();
    inner.push(0x0a);
    inner.push(0x03);
    inner.extend_from_slice(&NEARBY_SHARING_HASH);
    inner.extend_from_slice(&[0x10, 0x02]);
    inner.push(0x1a);
    inner.push(token.len() as u8);
    inner.extend_from_slice(&token);

    let mut out = Vec::new();
    out.extend_from_slice(&[0x00, 0x00, 0x00]);
    out.extend_from_slice(&[0x08, 0x01]);
    out.push(0x12);
    out.push(inner.len() as u8);
    out.extend_from_slice(&inner);
    out
}

/// Open a Weave socket to the peer discovery found at this address.
///
/// Deliberately does *not* scan. These are random resolvable private addresses
/// that rotate, so re-finding one by address is unreliable by design, and a
/// second concurrent scan on a fresh adapter doesn't reliably repopulate
/// `peripherals()` - every attempt failed with "no longer advertising". The
/// handle discovery already holds is the thing that stays valid.
pub async fn open_ble_by_address(
    address: &str,
) -> Result<tokio::io::DuplexStream, anyhow::Error> {
    let peripheral = super::discovered_peripheral(address).ok_or_else(|| {
        anyhow::anyhow!(
            "BLE peer {address} was not found by discovery this session; rescan and try again"
        )
    })?;

    open(peripheral).await
}

fn find_characteristic(peripheral: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == uuid)
}

/// Connect to `peripheral`, complete the Weave handshake as the client, and
/// return a stream carrying the Nearby protocol.
///
/// The returned stream is the *near* end; hand it to `OutboundRequest`.
pub async fn open(peripheral: Peripheral) -> Result<tokio::io::DuplexStream, anyhow::Error> {
    if !peripheral.is_connected().await? {
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

    // Weave ConnectionRequest: [0x80][version_min u16][version_max u16][max_packet u16].
    // Counter 0 - control and data share one counter per direction, which is
    // the rule the receive path had to learn the hard way.
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

    // Wait for ConnectionConfirm: [0x81][version u16][packet_size u16].
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

    // `near` goes to OutboundRequest; the pumps share `far`, so it is split -
    // one task only reads it, the other only writes it.
    //
    // Deliberately small. At 256 KB, OutboundRequest wrote an entire 2.37 MB
    // file into the buffer in three seconds and reported State::Finished while
    // the BLE link was still draining it at 20 KB/s. Nothing then answered the
    // phone's keepalives, so 30s later it timed out and closed the connection
    // mid-drain - the phone received only the part that had made it through, and
    // the app claimed success. 16 KB is under a second of link time, so writes
    // block until the bytes are genuinely on their way and "finished" means
    // finished.
    let (near, far) = tokio::io::duplex(16 * 1024);
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    // The service hash the peer actually uses, learned from its first data
    // frame.
    //
    // It is not always NearbySharing's fc9f5e: a Pixel 10 Pro answered on
    // 3b4447, and another device on b7ef32. Assuming fc9f5e meant every reply
    // was discarded as "multiplex control", so OutboundRequest saw nothing at
    // all, ended in State::Initial and the UI showed nothing happening. Frames
    // are still trivially separable - control is prefixed 00 00 00, anything
    // else is a service hash - so take the peer's hash as authoritative and
    // send on it.
    let peer_hash = std::sync::Arc::new(std::sync::Mutex::new(NEARBY_SHARING_HASH));
    let rx_hash = peer_hash.clone();

    // Cleared when the peer closes, so the sender stops instead of writing into
    // a dead connection.
    //
    // The two pumps were independent: when the peer closed, the inbound pump
    // ended but the outbound one stayed blocked in a write-with-response that
    // will never be acknowledged. OutboundRequest then blocked on a full duplex
    // and the transfer hung with no error - and the UI's cancel could not reach
    // it either.
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let rx_alive = alive.clone();

    // Outbound: OutboundRequest -> duplex -> [hash][len][frame] -> Weave packets.
    let tx_peripheral = peripheral.clone();
    let tx_char = client_tx.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        // Counter 0 was the ConnectionRequest, so data starts at 1.
        let mut counter: u8 = 1;
        let max_payload = (packet_size as usize).saturating_sub(1).max(1);

        // The multiplex layer expects its ConnectionRequest before any data.
        let mux = build_multiplex_connection_request();
        if let Err(e) =
            send_weave(&tx_peripheral, &tx_char, &mux, &mut counter, max_payload).await
        {
            warn!("{INNER_NAME}: failed to send multiplex ConnectionRequest: {e}");
            return;
        }

        // One frame at a time, and nothing is read until the previous frame is
        // fully on the wire.
        //
        // Reading ahead into an unbounded buffer defeats the point: with 8 KB
        // reads accumulating until a complete frame arrived, and OutboundRequest
        // using 512 KB payload chunks, the whole 2.37 MB file drained into
        // `pending` in two seconds. The transfer reported Finished while BLE was
        // still sending, the phone timed out 30s later, and it received only
        // part of the file. Shrinking the duplex didn't help - the backlog had
        // simply moved from the duplex into `pending`.
        loop {
            let mut len_buf = [0u8; 4];
            if far_rd.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == 0 || len > SANE_FRAME_LENGTH {
                warn!("{INNER_NAME}: refusing insane outbound frame length {len}");
                break;
            }

            if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                warn!("{INNER_NAME}: peer is gone, abandoning the rest of the transfer");
                break;
            }

            let mut frame = vec![0u8; len];
            if far_rd.read_exact(&mut frame).await.is_err() {
                break;
            }

            // Send on whatever hash the peer is using, not our own guess.
            let hash = peer_hash
                .lock()
                .map(|h| *h)
                .unwrap_or(NEARBY_SHARING_HASH);
            let mut msg = Vec::with_capacity(3 + 4 + len);
            msg.extend_from_slice(&hash);
            msg.extend_from_slice(&len_buf);
            msg.extend_from_slice(&frame);

            if let Err(e) =
                send_weave(&tx_peripheral, &tx_char, &msg, &mut counter, max_payload).await
            {
                warn!("{INNER_NAME}: weave send failed: {e}");
                return;
            }
        }
        info!("{INNER_NAME}: outbound pump ended");
    });

    // Inbound: Weave packets -> reassemble -> strip the hash -> duplex.
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut acc: Vec<u8> = Vec::new();
        // Did the peer ever say anything? A Quick Share receiver answers our
        // multiplex ConnectionRequest with a data frame. A peer that completes
        // the Weave handshake and then closes without a word is advertising
        // 0xFEF3 as general Nearby presence but has no receiver session behind
        // it - i.e. it is not on the receiving screen. That is not a protocol
        // failure and should not be reported as one.
        let mut heard_from_peer = false;

        while let Some(n) = notifications.next().await {
            if n.uuid != BLE_SOCKET_SERVER_TX_UUID || n.value.is_empty() {
                continue;
            }
            let header = n.value[0];
            if header & 0x80 != 0 {
                // Control. Command 2 is Error, which means the peer has given up.
                if header & 0x0f == 2 {
                    // Weave has no graceful-close command, so a completed
                    // transfer ends with an Error from the peer just as a failed
                    // one does - observed immediately after State::Finished.
                    // Not a fault on its own; the transfer state says whether
                    // anything went wrong.
                    info!("{INNER_NAME}: peer closed the Weave connection");
                    break;
                }
                continue;
            }

            let first = header & 0x08 != 0;
            let last = header & 0x04 != 0;
            if first {
                acc.clear();
            }
            acc.extend_from_slice(&n.value[1..]);
            if !last {
                continue;
            }

            let msg = std::mem::take(&mut acc);
            if msg.len() <= 3 {
                continue;
            }

            // `00 00 00` = multiplex control, which the stack above must not
            // see. Any other 3-byte prefix is a service hash, and the frame
            // behind it is the [u32 len][OfflineFrame] we want - whatever hash
            // the peer chose.
            if msg[..3] == [0x00, 0x00, 0x00] {
                debug!(
                    "{INNER_NAME}: multiplex control {:02x?}",
                    &msg[..msg.len().min(24)]
                );
                continue;
            }

            if let Ok(mut h) = rx_hash.lock() {
                if *h != msg[..3] {
                    let learned: [u8; 3] = [msg[0], msg[1], msg[2]];
                    info!(
                        "{INNER_NAME}: peer is using service hash {:02x?} (was {:02x?})",
                        learned, *h
                    );
                    *h = learned;
                }
            }

            heard_from_peer = true;
            if far_wr.write_all(&msg[3..]).await.is_err() {
                break;
            }
        }
        // Stop the sender: it is otherwise blocked on an acknowledgement that
        // can never arrive.
        rx_alive.store(false, std::sync::atomic::Ordering::Relaxed);

        if heard_from_peer {
            info!("{INNER_NAME}: inbound pump ended");
        } else {
            warn!(
                "{INNER_NAME}: peer accepted the Weave connection then closed it without a word - \
                 it is advertising Nearby but is not on the Quick Share receiving screen"
            );
        }
    });

    Ok(near)
}

/// Fragment `bytes` into Weave data packets and write them to the peer.
///
/// Header: `(counter << 4) | (first << 3) | (last << 2)`, with the counter
/// shared across control and data and wrapping mod 8.
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
        // WithResponse, not WithoutResponse.
        //
        // A write command is fire-and-forget: the Windows stack accepts every
        // packet immediately and queues it, so send_weave returned instantly
        // and the radio lagged minutes behind. That made the transfer report
        // Finished in one second while the phone was still receiving, and when
        // OutboundRequest finished nothing answered the phone's keepalives, so
        // it timed out at 30s and kept only the part that had arrived.
        //
        // A write request waits for the peer's ATT acknowledgement, which is the
        // only real flow control available here - every buffer upstream of this
        // was a symptom, not the cause.
        // Bounded as a backstop. A write-with-response to a peer that has gone
        // away can otherwise wait indefinitely, which is how a dead connection
        // turned into a hung transfer that even cancel could not reach.
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
