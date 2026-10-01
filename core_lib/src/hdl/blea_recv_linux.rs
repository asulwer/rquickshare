//! Linux BLE receiver advertiser using BlueZ via `bluer`.
//!
//! Advertises a connectable GATT service under UUID 0xFEF3 (Copresence) carrying the
//! Nearby Connections fast advertisement in its service data, and serves the full
//! advertisement (with device name and endpoint ID) over characteristic slot 0.
//! Also hosts the uWeave socket characteristics for BLE transfer and bandwidth upgrade.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluer::{
    adv::Advertisement,
    gatt::local::{
        Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
        CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, Service,
    },
};
use futures::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use uuid::{uuid, Uuid};

use super::ble_receiver;
use crate::channel::ChannelMessage;

const INNER_NAME: &str = "BleReceiverAdvertiser";

// Copresence service 0000FEF3-0000-1000-8000-00805F9B34FB.
const COPRESENCE_SERVICE_UUID: Uuid = uuid!("0000fef3-0000-1000-8000-00805f9b34fb");

// Per-slot advertisement characteristic 00000000-0000-3000-8000-000000000000 (slot 0).
// A peer that finds our advertisement connects and reads this to get the full
// BleAdvertisement containing the device name and endpoint info.
const ADV_SLOT0_CHARACTERISTIC_UUID: Uuid = uuid!("00000000-0000-3000-8000-000000000000");

// Nearby's BLE socket characteristics ("MultiplexBleSocketImpl"):
//   client tx = phone -> us   (phone writes)
//   server tx = us -> phone   (we notify/indicate)
const BLE_SOCKET_CLIENT_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000101");
const BLE_SOCKET_SERVER_TX_UUID: Uuid = uuid!("00000100-0004-1000-8000-001a11000102");

pub struct BleReceiverAdvertiser {
    endpoint_id: [u8; 4],
    device_type: u8,
    name: String,
    sender: tokio::sync::broadcast::Sender<ChannelMessage>,
}

impl BleReceiverAdvertiser {
    pub fn new(
        endpoint_id: [u8; 4],
        device_type: u8,
        name: String,
        sender: tokio::sync::broadcast::Sender<ChannelMessage>,
    ) -> Self {
        Self {
            endpoint_id,
            device_type,
            name,
            sender,
        }
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;
        info!(
            "{INNER_NAME}: advertising on Bluetooth adapter {} ({})",
            adapter.name(),
            adapter.address().await?
        );

        // Only the full form is needed. The compact "fast" form would go in the
        // advertisement packet's service data, but it doesn't fit alongside the
        // service UUID in a legacy packet - Windows hits the same limit (status
        // 4, StartedWithoutAllAdvertisementData). The phone finds us by the
        // 0xFEF3 UUID and reads this over GATT instead.
        let full_advertisement = ble_receiver::build_full_receiver_advertisement(
            &self.endpoint_id,
            self.device_type,
            &self.name,
        );

        // Channels and state shared with GATT handlers
        let inbound_slot: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(None));
        let down_slot: Arc<Mutex<Option<mpsc::UnboundedSender<()>>>> = Arc::new(Mutex::new(None));

        // Signalled when a Weave ConnectionRequest arrives from a phone
        let (new_session_tx, mut new_session_rx) = mpsc::unbounded_channel::<()>();

        // Outbound Weave sender channel currently registered by the active GATT subscriber
        let current_sub_tx: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(None));

        let tx_pending = Arc::new(AtomicUsize::new(0));
        let tx_drained = Arc::new(Notify::new());

        // Reassembly state for multi-packet incoming Weave messages
        struct WeaveRx {
            acc: Vec<u8>,
            expected: u8,
            pending: [Option<Vec<u8>>; 8],
        }
        let rx_buf = Arc::new(Mutex::new(WeaveRx {
            acc: Vec::new(),
            expected: 0,
            pending: Default::default(),
        }));

        // Outbound Weave packet sequence counter (0..7)
        let tx_counter = Arc::new(AtomicU8::new(0));

        let send_pkt: Arc<dyn Fn(Vec<u8>) + Send + Sync> = {
            let current_sub_tx = current_sub_tx.clone();
            let tx_pending = tx_pending.clone();
            Arc::new(move |pkt: Vec<u8>| {
                tx_pending.fetch_add(1, Ordering::Relaxed);
                if let Ok(slot) = current_sub_tx.lock() {
                    if let Some(tx) = slot.as_ref() {
                        if tx.send(pkt).is_err() {
                            warn!("{INNER_NAME}: failed to queue BLE packet; subscriber gone");
                            tx_pending.fetch_sub(1, Ordering::Relaxed);
                        }
                    } else {
                        warn!("{INNER_NAME}: no active subscriber to send BLE packet to");
                        tx_pending.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            })
        };

        // --- Build local GATT application ---
        let app = Application {
            services: vec![Service {
                uuid: COPRESENCE_SERVICE_UUID,
                primary: true,
                characteristics: vec![
                    // Slot 0 characteristic: full advertisement
                    Characteristic {
                        uuid: ADV_SLOT0_CHARACTERISTIC_UUID,
                        read: Some(CharacteristicRead {
                            read: true,
                            fun: Box::new({
                                let adv = full_advertisement.clone();
                                move |req| {
                                    let adv = adv.clone();
                                    async move {
                                        info!(
                                            "*** {INNER_NAME}: served advertisement over GATT read (offset={}) ***",
                                            req.offset
                                        );
                                        let offset = req.offset as usize;
                                        if offset >= adv.len() {
                                            Ok(Vec::new())
                                        } else {
                                            Ok(adv[offset..].to_vec())
                                        }
                                    }
                                    .boxed()
                                }
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    // Server TX: us -> phone (indications / notifications)
                    Characteristic {
                        uuid: BLE_SOCKET_SERVER_TX_UUID,
                        notify: Some(CharacteristicNotify {
                            notify: true,
                            indicate: true,
                            method: CharacteristicNotifyMethod::Fun(Box::new({
                                let current_sub_tx = current_sub_tx.clone();
                                let tx_pending = tx_pending.clone();
                                let tx_drained = tx_drained.clone();
                                let down_slot = down_slot.clone();
                                move |mut notifier| {
                                    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                                    if let Ok(mut slot) = current_sub_tx.lock() {
                                        *slot = Some(sub_tx);
                                    }
                                    let current_sub_tx = current_sub_tx.clone();
                                    let tx_pending = tx_pending.clone();
                                    let tx_drained = tx_drained.clone();
                                    let down_slot = down_slot.clone();
                                    async move {
                                        info!(
                                            "{INNER_NAME}: phone subscribed to Server TX (confirming={})",
                                            notifier.confirming()
                                        );
                                        loop {
                                            tokio::select! {
                                                _ = notifier.stopped() => {
                                                    warn!("{INNER_NAME}: phone unsubscribed - BLE link dropped");
                                                    if let Ok(guard) = down_slot.lock() {
                                                        if let Some(tx) = guard.as_ref() {
                                                            let _ = tx.send(());
                                                        }
                                                    }
                                                    break;
                                                }
                                                Some(pkt) = sub_rx.recv() => {
                                                    let res = notifier.notify(pkt).await;
                                                    let prev = tx_pending.fetch_sub(1, Ordering::Relaxed);
                                                    if prev <= 1 {
                                                        tx_drained.notify_waiters();
                                                    }
                                                    if let Err(e) = res {
                                                        warn!("{INNER_NAME}: notifier.notify failed: {e}");
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                        if let Ok(mut slot) = current_sub_tx.lock() {
                                            *slot = None;
                                        }
                                        info!("{INNER_NAME}: server-tx notification loop ended");
                                    }
                                    .boxed()
                                }
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    // Client TX: phone -> us (writes)
                    Characteristic {
                        uuid: BLE_SOCKET_CLIENT_TX_UUID,
                        write: Some(CharacteristicWrite {
                            write: true,
                            write_without_response: true,
                            method: CharacteristicWriteMethod::Fun(Box::new({
                                let rx_buf = rx_buf.clone();
                                let tx_counter = tx_counter.clone();
                                let send_pkt = send_pkt.clone();
                                let new_session_tx = new_session_tx.clone();
                                let inbound_slot = inbound_slot.clone();
                                move |mut buf, _req| {
                                    let rx_buf = rx_buf.clone();
                                    let tx_counter = tx_counter.clone();
                                    let send_pkt = send_pkt.clone();
                                    let new_session_tx = new_session_tx.clone();
                                    let inbound_slot = inbound_slot.clone();
                                    async move {
                                        if buf.is_empty() {
                                            return Ok(());
                                        }

                                        // Weave framing: bit 7: 0 = data, 1 = control
                                        if buf[0] & 0x80 == 0 {
                                            let counter = ((buf[0] >> 4) & 0x07) as usize;
                                            let mut ready_msgs = Vec::new();
                                            if let Ok(mut st) = rx_buf.lock() {
                                                st.pending[counter] = Some(std::mem::take(&mut buf));

                                                while let Some(pkt) = {
                                                    let exp = st.expected as usize;
                                                    st.pending[exp].take()
                                                } {
                                                    st.expected = (st.expected + 1) & 0x07;
                                                    let header = pkt[0];
                                                    let first = header & 0x08 != 0;
                                                    let last = header & 0x04 != 0;

                                                    if first {
                                                        st.acc.clear();
                                                    }
                                                    st.acc.extend_from_slice(&pkt[1..]);
                                                    if last {
                                                        let msg = std::mem::take(&mut st.acc);
                                                        ready_msgs.push(msg);
                                                    }
                                                }
                                            }

                                            for msg in ready_msgs {
                                                if let Ok(guard) = inbound_slot.lock() {
                                                    if let Some(tx) = guard.as_ref() {
                                                        let _ = tx.send(msg);
                                                    }
                                                }
                                            }
                                        } else {
                                            // Control packet
                                            if let Ok(mut st) = rx_buf.lock() {
                                                st.expected = (((buf[0] >> 4) & 0x07) + 1) & 0x07;
                                                st.acc.clear();
                                                st.pending = Default::default();
                                            }

                                            let cmd = buf[0] & 0x0f;
                                            // Command 0 = ConnectionRequest: [0x80, ver_min(2), ver_max(2), max_pkt(2)]
                                            if cmd == 0 {
                                                info!("*** {INNER_NAME}: received Weave ConnectionRequest ***");
                                                let ver_max = if buf.len() >= 5 {
                                                    u16::from_be_bytes([buf[3], buf[4]])
                                                } else {
                                                    1
                                                };
                                                let their_max = if buf.len() >= 7 {
                                                    u16::from_be_bytes([buf[5], buf[6]])
                                                } else {
                                                    509
                                                };
                                                let version = ver_max.min(1);
                                                let packet_size = their_max.min(509);

                                                tx_counter.store(0, Ordering::Relaxed);
                                                if let Ok(mut st) = rx_buf.lock() {
                                                    st.acc.clear();
                                                    st.expected = 1;
                                                    st.pending = Default::default();
                                                }

                                                let ctr = tx_counter.fetch_add(1, Ordering::Relaxed) & 0x07;
                                                let confirm = vec![
                                                    0x80 | (ctr << 4) | 0x01,
                                                    (version >> 8) as u8,
                                                    version as u8,
                                                    (packet_size >> 8) as u8,
                                                    packet_size as u8,
                                                ];
                                                info!(
                                                    "*** {INNER_NAME}: sending ConnectionConfirm (v{version}, {packet_size} B) ***"
                                                );
                                                send_pkt(confirm);
                                                let _ = new_session_tx.send(());
                                            } else {
                                                info!("{INNER_NAME}: weave control cmd={cmd} {:02x?}", &buf);
                                            }
                                        }

                                        Ok(())
                                    }
                                    .boxed()
                                }
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let app_handle = adapter.serve_gatt_application(app).await?;
        info!("{INNER_NAME}: GATT application registered successfully");

        // --- Advertise connectable 0xFEF3 service ---
        // A legacy BLE advertising packet is capped at 31 bytes total.
        // The 0xFEF3 Service UUID (4 B) + Flags (3 B) = 7 B, easily fitting in
        // a standard legacy ADV_IND packet (37 B would trigger BlueZ to force
        // Bluetooth 5 Extended Advertising which Android's legacy scanner cannot see).
        // Android discovers the receiver by seeing the 0xFEF3 Service UUID on air,
        // connects, and reads the full advertisement (name + endpoint) over GATT slot 0.
        let le_advertisement = Advertisement {
            advertisement_type: bluer::adv::Type::Peripheral,
            service_uuids: vec![COPRESENCE_SERVICE_UUID].into_iter().collect(),
            discoverable: Some(true),
            ..Default::default()
        };
        let mut adv_handle = Some(adapter.advertise(le_advertisement.clone()).await?);
        info!("{INNER_NAME}: legacy connectable GATT service advertising started under 0xFEF3");

        // --- Supervisor: spawn a fresh Nearby session for each Weave connection ---
        let session_sender = self.sender.clone();
        let session_inbound_slot = inbound_slot.clone();
        let session_down_slot = down_slot.clone();
        let session_tx_counter = tx_counter.clone();
        let session_send_pkt = send_pkt.clone();
        let session_tx_pending = tx_pending.clone();
        let session_tx_drained = tx_drained.clone();

        tokio::spawn(async move {
            while new_session_rx.recv().await.is_some() {
                info!("{INNER_NAME}: new Weave connection, starting fresh session");

                let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let (down_tx, mut down_rx) = mpsc::unbounded_channel::<()>();
                let (upgrade_tx, mut upgrade_rx) =
                    mpsc::unbounded_channel::<tokio::net::TcpStream>();
                let (switch_tx, mut switch_rx) = mpsc::unbounded_channel::<()>();
                let (mut ours, theirs) = tokio::io::duplex(256 * 1024);

                *session_inbound_slot.lock().unwrap() = Some(inbound_tx);
                *session_down_slot.lock().unwrap() = Some(down_tx);

                let msg_sender = session_sender.clone();
                let ui_sender = session_sender.clone();

                // 1. Spawn InboundRequest worker
                tokio::spawn(async move {
                    let _pause = crate::hdl::DiscoveryPause::new();
                    let mut request =
                        super::InboundRequest::new(theirs, "ble".to_string(), msg_sender);
                    request.set_upgrade_sink(upgrade_tx, switch_tx);

                    loop {
                        match request.handle().await {
                            Ok(()) => {}
                            Err(e) => {
                                match e.downcast_ref() {
                                    Some(crate::errors::AppError::NotAnError) => {
                                        info!("{INNER_NAME}: BLE session closed normally");
                                    }
                                    _ => {
                                        warn!(
                                            "{INNER_NAME}: BLE InboundRequest ended in state {:?}: {e}",
                                            request.state.state
                                        );
                                        if request.state.state != crate::hdl::State::Finished {
                                            let _ = ui_sender.send(ChannelMessage {
                                                id: "ble".to_string(),
                                                direction:
                                                    crate::channel::ChannelDirection::LibToFront,
                                                state: Some(crate::hdl::State::Disconnected),
                                                ..Default::default()
                                            });
                                        }
                                    }
                                }
                                break;
                            }
                        }
                    }
                });

                // 2. Spawn pump: pipes between duplex and Weave BLE framing
                let pump_tx_counter = session_tx_counter.clone();
                let pump_send_pkt = session_send_pkt.clone();
                let pump_pending = session_tx_pending.clone();
                let pump_drained = session_tx_drained.clone();

                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut upgraded: Option<tokio::net::TcpStream> = None;

                    loop {
                        tokio::select! {
                            Some(msg) = inbound_rx.recv() => {
                                if msg.len() > 3 && msg[..3] == [0xfc, 0x9f, 0x5e] {
                                    let body = &msg[3..];
                                    if ours.write_all(body).await.is_err() {
                                        break;
                                    }
                                } else {
                                    debug!("{INNER_NAME}: multiplex control {:02x?}", &msg[..msg.len().min(24)]);
                                }
                            }
                            n = ours.read_buf(&mut pending) => {
                                match n {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {
                                        while pending.len() >= 4 {
                                            let len = u32::from_be_bytes([
                                                pending[0], pending[1], pending[2], pending[3],
                                            ]) as usize;
                                            if pending.len() < 4 + len {
                                                break;
                                            }
                                            let mut out = Vec::with_capacity(3 + 4 + len);
                                            out.extend_from_slice(&[0xfc, 0x9f, 0x5e]);
                                            out.extend_from_slice(&pending[..4 + len]);
                                            pending.drain(..4 + len);

                                            frame_weave(&out, &pump_tx_counter, &pump_send_pkt);
                                        }
                                    }
                                }
                            }
                            Some(sock) = upgrade_rx.recv() => {
                                info!("{INNER_NAME}: upgraded socket ready, holding for SAFE_TO_CLOSE");
                                upgraded = Some(sock);
                            }
                            Some(_) = switch_rx.recv() => {
                                match upgraded.take() {
                                    Some(mut sock) => {
                                        // Flush remaining bytes from duplex
                                        loop {
                                            let mut buf = [0u8; 8192];
                                            match tokio::time::timeout(Duration::from_millis(200), ours.read(&mut buf)).await {
                                                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                                Ok(Ok(n)) => pending.extend_from_slice(&buf[..n]),
                                            }
                                        }
                                        while pending.len() >= 4 {
                                            let len = u32::from_be_bytes([
                                                pending[0], pending[1], pending[2], pending[3],
                                            ]) as usize;
                                            if pending.len() < 4 + len {
                                                break;
                                            }
                                            let mut out = Vec::with_capacity(3 + 4 + len);
                                            out.extend_from_slice(&[0xfc, 0x9f, 0x5e]);
                                            out.extend_from_slice(&pending[..4 + len]);
                                            pending.drain(..4 + len);
                                            frame_weave(&out, &pump_tx_counter, &pump_send_pkt);
                                        }

                                        // Close the multiplex service channel
                                        let disconnect = vec![
                                            0x00, 0x00, 0x00, 0x08, 0x02, 0x1a, 0x05, 0x0a, 0x03, 0xfc, 0x9f, 0x5e,
                                        ];
                                        pump_send_pkt(disconnect);

                                        // Wait for queued packets to drain
                                        let drained = pump_drained.notified();
                                        tokio::pin!(drained);
                                        drained.as_mut().enable();
                                        if pump_pending.load(Ordering::Relaxed) > 0 {
                                            let _ = tokio::time::timeout(Duration::from_secs(3), drained).await;
                                        }

                                        info!("{INNER_NAME}: prior channel released, switching to upgraded socket");

                                        let mut first = [0u8; 8192];
                                        let n = match tokio::time::timeout(Duration::from_secs(45), sock.read(&mut first)).await {
                                            Ok(Ok(0)) => {
                                                warn!("{INNER_NAME}: peer closed upgraded socket without sending");
                                                break;
                                            }
                                            Ok(Ok(n)) => n,
                                            Ok(Err(e)) => {
                                                warn!("{INNER_NAME}: upgraded socket failed: {e}");
                                                break;
                                            }
                                            Err(_) => {
                                                warn!("{INNER_NAME}: nothing on upgraded socket within 45s");
                                                break;
                                            }
                                        };
                                        if ours.write_all(&first[..n]).await.is_err() {
                                            break;
                                        }

                                        match tokio::io::copy_bidirectional(&mut sock, &mut ours).await {
                                            Ok((from_phone, to_phone)) => {
                                                info!("{INNER_NAME}: upgraded socket closed ({from_phone} B in, {to_phone} B out)");
                                            }
                                            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {
                                                info!("{INNER_NAME}: phone closed upgraded socket ({e})");
                                            }
                                            Err(e) => warn!("{INNER_NAME}: upgraded socket copy error: {e}"),
                                        }
                                        break;
                                    }
                                    None => warn!("{INNER_NAME}: asked to switch but no upgraded socket arrived"),
                                }
                            }
                            _ = down_rx.recv() => {
                                warn!("{INNER_NAME}: BLE link dropped, tearing bridge down");
                                break;
                            }
                            else => break,
                        }
                    }
                    info!("{INNER_NAME}: bridge pump exited");
                });
            }
        });

        // --- Keep advertiser running and handle discovery pause ---
        let mut adv_paused = false;
        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: cancelled, stopping BLE receiver advertiser");
                    drop(adv_handle);
                    drop(app_handle);
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    let want_paused = crate::hdl::ble_send_in_progress();
                    if want_paused != adv_paused {
                        if want_paused {
                            drop(adv_handle.take());
                            info!("{INNER_NAME}: outbound BLE send in progress, pausing advertisement");
                        } else {
                            match adapter.advertise(le_advertisement.clone()).await {
                                Ok(h) => {
                                    adv_handle = Some(h);
                                    info!("{INNER_NAME}: outbound BLE send finished, resumed advertisement");
                                }
                                Err(e) => warn!("{INNER_NAME}: could not resume advertisement: {e}"),
                            }
                        }
                        adv_paused = want_paused;
                    }
                }
            }
        }

        Ok(())
    }
}

/// Slices `bytes` into chunks of at most 508 bytes, adds a Weave data header,
/// and queues each chunk via `send_pkt`.
fn frame_weave(bytes: &[u8], tx_counter: &AtomicU8, send_pkt: &Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
    let max_payload = 508usize;
    let chunks: Vec<&[u8]> = bytes.chunks(max_payload).collect();
    let n = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let first = i == 0;
        let last = i + 1 == n;
        let ctr = tx_counter.fetch_add(1, Ordering::Relaxed) & 0x07;
        let header = (ctr << 4) | if first { 0x08 } else { 0 } | if last { 0x04 } else { 0 };

        let mut pkt = Vec::with_capacity(1 + chunk.len());
        pkt.push(header);
        pkt.extend_from_slice(chunk);
        send_pkt(pkt);
    }
}
