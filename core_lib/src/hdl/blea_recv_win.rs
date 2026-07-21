//! Windows BLE *receiver* advertiser (issue #425).
//!
//! Advertises a **GATT service** under 0xFEF3 carrying the Nearby Connections
//! fast advertisement as its service data, so a phone doing BLE-only discovery
//! (WiFi off) lists this machine as a target.
//!
//! **Why a GATT service provider and not a plain advertisement publisher.**
//! Every real Quick Share receiver (neighbour phones, Google's own Windows app)
//! advertises `isPrivateGatt=true` in the phone's logcat - a connectable
//! GATT-backed advertisement, not a beacon. We spent a while broadcasting via
//! `BluetoothLEAdvertisementPublisher` (Microsoft: "mainly used to create
//! beacons"); it reached status Started but the phone's *receiver* discovery
//! never surfaced us as a ShareTarget, legacy or extended. `GattServiceProvider`
//! is the connectable/discoverable mechanism the phone actually looks for, and
//! it also gives us the GATT server needed later to serve the device name.

use tokio_util::sync::CancellationToken;
use windows::core::{IInspectable, GUID};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicProperties, GattLocalCharacteristic, GattLocalCharacteristicParameters,
    GattReadRequestedEventArgs, GattServiceProvider, GattServiceProviderAdvertisingParameters,
    GattWriteOption, GattWriteRequestedEventArgs,
};
use windows::Foundation::{AsyncOperationCompletedHandler, TypedEventHandler};
use windows::Storage::Streams::{DataReader, DataWriter};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::ble_receiver;

const INNER_NAME: &str = "BleReceiverAdvertiser";

// Copresence service 0000FEF3-0000-1000-8000-00805F9B34FB.
const COPRESENCE_SERVICE_GUID: GUID = GUID::from_u128(0x0000_FEF3_0000_1000_8000_00805F9B34FB);

// Per-slot advertisement characteristic 00000000-0000-3000-8000-000000000000
// (slot 0). A peer that finds our advertisement connects and reads this to get
// the full BleAdvertisement - the `isPrivateGatt` / `rxAdvertisement` flow.
const ADV_SLOT0_CHARACTERISTIC_GUID: GUID =
    GUID::from_u128(0x0000_0000_0000_3000_8000_000000000000);

// Nearby's BLE socket ("MultiplexBleSocketImpl"). After reading our
// advertisement the phone connects and looks for these; without the client-tx
// one it logs `missing client tx characteristic ...0101` and aborts with
// ESTABLISH_GATT_CONNECTION_FAILED.
//   client tx = phone -> us   (phone writes)
//   server tx = us -> phone   (we notify)
// Only ...0101 is confirmed from logcat; ...0102 is the obvious counterpart in
// the same scheme. The phone names whatever is still missing, so iterate on it.
const BLE_SOCKET_CLIENT_TX_GUID: GUID =
    GUID::from_u128(0x0000_0100_0004_1000_8000_001a11000101);
const BLE_SOCKET_SERVER_TX_GUID: GUID =
    GUID::from_u128(0x0000_0100_0004_1000_8000_001a11000102);

fn adv_status_name(s: i32) -> &'static str {
    match s {
        0 => "Created",
        1 => "Stopped",
        2 => "Started",
        3 => "Aborted",
        4 => "StartedWithoutAllAdvertisementData",
        _ => "Unknown",
    }
}

pub struct BleReceiverAdvertiser {
    endpoint_id: [u8; 4],
    device_type: u8,
    name: String,
    sender: tokio::sync::broadcast::Sender<crate::channel::ChannelMessage>,
}

impl BleReceiverAdvertiser {
    pub fn new(
        endpoint_id: [u8; 4],
        device_type: u8,
        name: String,
        sender: tokio::sync::broadcast::Sender<crate::channel::ChannelMessage>,
    ) -> Self {
        Self {
            endpoint_id,
            device_type,
            name,
            sender,
        }
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        // Two forms, two channels:
        //  - fast: compact, for the advertisement packet's service data.
        //  - full: served over GATT when a peer connects. Carries the device
        //    name, which is what the phone needs to build a listable
        //    ShareTarget. Serving the *fast* form here was why the phone read us
        //    successfully and still never listed us.
        let advertisement =
            ble_receiver::build_fast_receiver_advertisement(&self.endpoint_id, self.device_type);
        let full_advertisement = ble_receiver::build_full_receiver_advertisement(
            &self.endpoint_id,
            self.device_type,
            &self.name,
        );

        // Bridge between the WinRT/GATT world and the async Nearby stack.
        //  inbound : write handler -> (strip fc9f5e) -> duplex -> InboundRequest
        //  outbound: InboundRequest -> duplex -> (prepend fc9f5e) -> notify
        // The GATT objects aren't Send, so notifying has to happen back on the
        // thread that owns them; bytes travel over channels instead.
        // One bridge *per Weave connection*, not per process.
        //
        // These were built once at startup, so when an InboundRequest ended -
        // for any reason at all - the bridge died with it and every later BLE
        // connection was answered at the Weave level and then dropped on the
        // floor. The phone reconnected, got a ConnectionConfirm, and talked to
        // nobody. One failure poisoned BLE receive until the app was restarted,
        // which is exactly the "restart only buys one more attempt" behaviour we
        // kept hitting. It also leaked handshake state between connections: a
        // fresh peer's ClientInit arrived at a request still in
        // SentUkeyServerInit and failed as "message_type(1) != ClientFinish".
        //
        // The write handler therefore cannot hold a fixed sender; it writes into
        // whichever session is current.
        let inbound_slot: std::sync::Arc<
            std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let down_slot: std::sync::Arc<
            std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let handler_inbound = inbound_slot.clone();
        let handler_down = down_slot.clone();
        // Signalled by the write handler when a Weave ConnectionRequest arrives.
        let (new_session_tx, mut new_session_rx) =
            tokio::sync::mpsc::unbounded_channel::<()>();

        let (outbound_tx, mut outbound_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        // Time spent *inside* the WinRT write callback, accumulated rather than
        // logged per packet. This is the measurement that decides whether the
        // ~25 ms/packet we observe is our own processing (ours to fix) or the
        // connection interval (the central's choice, not exposed to a WinRT
        // GATT server). Two atomics cost nothing next to a 509-byte BLE write.
        let handler_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let handler_calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pump_us = handler_us.clone();
        let pump_calls = handler_calls.clone();

        // Signals that the phone tore the BLE link down. Without it the pump
        // sits waiting for bytes that can never arrive, `InboundRequest` never
        // returns, and the UI shows a transfer stuck at whatever percentage it
        // reached - which is exactly what a dropped link looked like from the
        // app: the phone says "sent", we say nothing at all.
        // Carries the upgraded WiFi socket from the bandwidth-upgrade listener
        // back to the pump, which then splices it in place of the Weave
        // transport. TCP framing is byte-for-byte what `InboundRequest` already
        // reads off the duplex, so the swap needs no translation and the
        // encrypted stream - keys, sequence numbers - continues untouched.
        // Separate from the socket itself: the socket arrives at
        // CLIENT_INTRODUCTION_ACK, but the stream may not move until
        // SAFE_TO_CLOSE_PRIOR_CHANNEL has gone out over BLE.

        // Outbound Weave packets not yet confirmed by the peer.
        //
        // SAFE_TO_CLOSE_PRIOR_CHANNEL is the last thing we send over BLE, and
        // the peer will not touch the new socket until it *receives* it. We were
        // switching as soon as it was queued, but indications on this link take
        // 10-30s to confirm - so the peer sat waiting for a frame still in our
        // queue while the switch timer ran out and the transfer died with
        // "nothing on the upgraded socket within 20s".
        let tx_pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pump_pending = tx_pending.clone();

        let session_sender = self.sender.clone();
        let session_outbound_tx = outbound_tx.clone();

        // Set when a session ends, so the advertiser drops the peer's GATT
        // connection.
        //
        // The phone stays connected to our GATT server after a transfer -
        // `subscribers = 1` never falls back to 0 - and while it does, our own
        // attempt to connect *to* it as a client contends for the same link.
        // Measured symmetrically: a receive then breaks the next send, a send
        // breaks the next receive, and only restarting the app clears it.
        // Disconnecting our client side (blea_send) covers one half; this is the
        // other.
        let recycle_adv = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let session_recycle = recycle_adv.clone();

        // Supervisor: build a fresh session for every Weave connection.
        tokio::spawn(async move {
        while new_session_rx.recv().await.is_some() {
            info!("{INNER_NAME}: new Weave connection, starting a fresh session");

            let (inbound_tx, mut inbound_rx) =
                tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let (down_tx, mut down_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let (upgrade_tx, mut upgrade_rx) =
                tokio::sync::mpsc::unbounded_channel::<tokio::net::TcpStream>();
            let (switch_tx, mut switch_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let (mut ours, theirs) = tokio::io::duplex(256 * 1024);

            // Route the write handler at this session. The previous one's
            // receiver is dropped with it, so anything still in flight for the
            // old session is discarded rather than mixed into this one.
            if let Ok(mut slot) = inbound_slot.lock() {
                *slot = Some(inbound_tx);
            }
            if let Ok(mut slot) = down_slot.lock() {
                *slot = Some(down_tx);
            }

            let outbound_tx = session_outbound_tx.clone();
            let pump_us = pump_us.clone();
            let pump_calls = pump_calls.clone();
            let pump_pending = pump_pending.clone();
            let msg_sender = session_sender.clone();
            let ui_sender = session_sender.clone();
            let end_recycle = session_recycle.clone();

        tokio::spawn(async move {
            let mut request =
                super::InboundRequest::new(theirs, "ble".to_string(), msg_sender);
            request.set_upgrade_sink(upgrade_tx, switch_tx);

            // `handle()` services exactly **one** frame and returns; the caller
            // owns the loop (see TcpServer). Calling it once ends the task after
            // the ConnectionRequest, which drops our end of the duplex - the
            // pump then sees EOF and every later Weave message is discarded.
            loop {
                match request.handle().await {
                    Ok(()) => {}
                    Err(e) => {
                        match e.downcast_ref() {
                            Some(crate::errors::AppError::NotAnError) => {
                                info!("{INNER_NAME}: BLE session closed normally");
                            }
                            // Report the state machine's position too: the same
                            // error text means different things at different
                            // points in the handshake.
                            _ => {
                                warn!(
                                    "{INNER_NAME}: BLE InboundRequest ended in state {:?}: {e}",
                                    request.state.state
                                );
                                // Tell the UI. The TCP path does this in
                                // manager.rs; without it a failed BLE transfer
                                // leaves its card frozen mid-progress forever,
                                // which reads as a hang rather than a failure.
                                if request.state.state != crate::hdl::State::Finished {
                                    let _ = ui_sender.send(crate::channel::ChannelMessage {
                                        id: "ble".to_string(),
                                        direction: crate::channel::ChannelDirection::LibToFront,
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
            // Let the peer finish its side before dropping the link.
            //
            // Recycling immediately cut the connection while the phone was still
            // completing the transfer: the PC had the whole file and the phone
            // sat on "sending" forever, because it never saw the end of the
            // exchange. Our last frames are indications that can take seconds to
            // confirm on this link, so the wait has to cover that rather than
            // just the round trip.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // Drop the peer's GATT connection, so the next transfer - in either
            // direction - starts from a clean link.
            end_recycle.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        // Pump: reassembled Weave messages in, framed replies out.
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut pending = Vec::new();
            // Throughput accounting. Lives here rather than in the WinRT write
            // callback so measuring the receive path doesn't slow it down -
            // which is the exact mistake that made transfers fail.
            let mut rx_bytes: u64 = 0;
            let mut rx_frames: u64 = 0;
            let mut win_bytes: u64 = 0;
            let mut last_report = std::time::Instant::now();
            // Held between CLIENT_INTRODUCTION_ACK and SAFE_TO_CLOSE_PRIOR_CHANNEL.
            let mut upgraded: Option<tokio::net::TcpStream> = None;
            loop {
                tokio::select! {
                    Some(msg) = inbound_rx.recv() => {
                        // `fc 9f 5e` = NearbySharing data; the remainder is
                        // exactly the [u32 len][OfflineFrame] the TCP path reads.
                        // `00 00 00` = multiplex control, not for InboundRequest.
                        if msg.len() > 3 && msg[..3] == [0xfc, 0x9f, 0x5e] {
                            // Sanity-check the framing: the [u32 len][frame] must
                            // consume the message exactly. Any mismatch desyncs
                            // the stream and InboundRequest blocks forever on a
                            // garbage length, so surface it loudly.
                            let body = &msg[3..];
                            if body.len() >= 4 {
                                let declared = u32::from_be_bytes(
                                    [body[0], body[1], body[2], body[3]],
                                ) as usize;
                                if declared + 4 != body.len() {
                                    warn!(
                                        "{INNER_NAME}: FRAMING MISMATCH - declared {declared} + 4 \
                                         != body {} (msg {} B). Trailing {} B: {:02x?}",
                                        body.len(),
                                        msg.len(),
                                        body.len().saturating_sub(declared + 4),
                                        &body[(declared + 4).min(body.len())..],
                                    );
                                } else {
                                    trace!("{INNER_NAME}: -> InboundRequest {} B frame", declared);
                                }
                            }
                            rx_frames += 1;
                            rx_bytes += body.len() as u64;
                            win_bytes += body.len() as u64;
                            let elapsed = last_report.elapsed();
                            if elapsed >= std::time::Duration::from_secs(5) {
                                use std::sync::atomic::Ordering::Relaxed;
                                let calls = pump_calls.swap(0, Relaxed);
                                let us = pump_us.swap(0, Relaxed);
                                // `in handler` is the share of wall-clock time we
                                // actually spend processing a write. If it is a
                                // few percent, the gap between packets is the
                                // link (connection interval) and no amount of
                                // optimising our code will help.
                                let mean_us = if calls > 0 { us / calls } else { 0 };
                                let busy = us as f64 / elapsed.as_micros() as f64 * 100.0;
                                info!(
                                    "{INNER_NAME}: BLE rx {:.1} KB/s ({rx_frames} frames, \
                                     {:.0} KB total) - handler {mean_us} us mean over \
                                     {calls} writes, {busy:.1}% busy",
                                    win_bytes as f64 / 1024.0 / elapsed.as_secs_f64(),
                                    rx_bytes as f64 / 1024.0,
                                );
                                win_bytes = 0;
                                last_report = std::time::Instant::now();
                            }
                            if ours.write_all(body).await.is_err() {
                                break;
                            }
                        } else {
                            info!("{INNER_NAME}: multiplex control {:02x?}", &msg[..msg.len().min(24)]);
                        }
                    }
                    n = ours.read_buf(&mut pending) => {
                        match n {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                // Split the outgoing byte stream back into whole
                                // [u32 len][frame] messages so each can carry the
                                // service hash.
                                while pending.len() >= 4 {
                                    let len = u32::from_be_bytes(
                                        [pending[0], pending[1], pending[2], pending[3]],
                                    ) as usize;
                                    if pending.len() < 4 + len {
                                        break;
                                    }
                                    let mut out = Vec::with_capacity(3 + 4 + len);
                                    out.extend_from_slice(&[0xfc, 0x9f, 0x5e]);
                                    out.extend_from_slice(&pending[..4 + len]);
                                    pending.drain(..4 + len);
                                    if outbound_tx.send(out).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Some(sock) = upgrade_rx.recv() => {
                        // Arrives at CLIENT_INTRODUCTION_ACK. Hold it: the phone
                        // still has LAST_WRITE_TO_PRIOR_CHANNEL to send over BLE
                        // and waits for our SAFE_TO_CLOSE_PRIOR_CHANNEL before
                        // it will use this socket. Switching here tore the BLE
                        // bridge down under that exchange and the phone
                        // cancelled the transfer.
                        info!("{INNER_NAME}: upgraded socket ready, holding for SAFE_TO_CLOSE");
                        upgraded = Some(sock);
                    }
                    Some(_) = switch_rx.recv() => {
                        // SAFE_TO_CLOSE_PRIOR_CHANNEL has gone out; the old
                        // channel is finished with.
                        match upgraded.take() {
                            Some(mut sock) => {
                                // First flush anything InboundRequest has
                                // written but we have not pumped yet.
                                //
                                // SAFE_TO_CLOSE goes into the duplex, and the
                                // switch is signalled on a *separate* channel
                                // immediately afterwards - so the signal can
                                // beat the bytes here. On a LAN upgrade, where
                                // the whole exchange takes under a second, it
                                // does: `prior channel drained` fired in the
                                // same second as the connection because nothing
                                // had been queued yet, we broke out of the pump,
                                // and SAFE_TO_CLOSE was left unsent in the
                                // duplex. The peer then waited forever for a
                                // frame we still held.
                                loop {
                                    let mut buf = [0u8; 8192];
                                    match tokio::time::timeout(
                                        std::time::Duration::from_millis(200),
                                        ours.read(&mut buf),
                                    )
                                    .await
                                    {
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
                                    if outbound_tx.send(out).is_err() {
                                        break;
                                    }
                                }

                                // Then wait for it to actually reach the peer.
                                // It is only queued at this point, and on a
                                // congested BLE link that is tens of seconds
                                // apart - the peer will not use the new socket
                                // until it has it.
                                // Let the sender thread pick the queue up before
                                // asking whether it is empty: it publishes
                                // `tx_pending` once per 50ms loop, so checking
                                // immediately would read a stale 0 and "drain"
                                // instantly - the same mistake in miniature.
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                                // Give it a moment to go out, but do not wait on
                                // confirmation.
                                //
                                // Waiting for the indication to be *confirmed*
                                // stalls forever on the LAN path: once the peer
                                // has the new socket it stops servicing BLE, so
                                // that confirmation may never come. It is also
                                // unnecessary - the sender thread drains its
                                // queue independently of this pump, so once
                                // SAFE_TO_CLOSE is queued it goes out whether we
                                // are still here or not. Queueing it was the
                                // part that was actually missing.
                                let drain_deadline = std::time::Instant::now()
                                    + std::time::Duration::from_secs(3);
                                while pump_pending.load(std::sync::atomic::Ordering::Relaxed) > 0
                                    && std::time::Instant::now() < drain_deadline
                                {
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                }
                                info!(
                                    "{INNER_NAME}: prior channel released ({} packet(s) still \
                                     queued, the sender thread will finish them)",
                                    pump_pending.load(std::sync::atomic::Ordering::Relaxed)
                                );

                                // Splice rather than translate: what the phone
                                // writes here is already [u32 len][frame],
                                // exactly what the far side of the duplex reads.
                                // Only the Weave and multiplex wrappers were ever
                                // BLE-specific, so the encrypted stream - keys,
                                // sequence numbers - carries straight over.
                                info!(
                                    "{INNER_NAME}: switching the stream onto the upgraded socket"
                                );

                                // The peer has occasionally completed the whole
                                // upgrade handshake and then sent nothing at
                                // all. `copy_bidirectional` waits forever on
                                // that, so the transfer hangs at whatever
                                // percentage it reached with not one line in the
                                // log - indistinguishable from a crash. Require
                                // the first byte within 20s; a peer that
                                // switched mediums and meant it starts
                                // immediately.
                                let mut first = [0u8; 8192];
                                let n = match tokio::time::timeout(
                                    std::time::Duration::from_secs(45),
                                    sock.read(&mut first),
                                )
                                .await
                                {
                                    Ok(Ok(0)) => {
                                        warn!(
                                            "{INNER_NAME}: peer closed the upgraded socket without \
                                             sending anything"
                                        );
                                        break;
                                    }
                                    Ok(Ok(n)) => n,
                                    Ok(Err(e)) => {
                                        warn!("{INNER_NAME}: upgraded socket failed: {e}");
                                        break;
                                    }
                                    Err(_) => {
                                        warn!(
                                            "{INNER_NAME}: nothing on the upgraded socket within \
                                             45s of the prior channel draining"
                                        );
                                        break;
                                    }
                                };
                                if ours.write_all(&first[..n]).await.is_err() {
                                    break;
                                }

                                match tokio::io::copy_bidirectional(&mut sock, &mut ours).await {
                                    Ok((from_phone, to_phone)) => info!(
                                        "{INNER_NAME}: upgraded socket closed ({from_phone} B in, \
                                         {to_phone} B out)"
                                    ),
                                    // The phone slams the socket shut the
                                    // moment the transfer completes rather than
                                    // closing it cleanly, so a reset here is the
                                    // normal ending, not a fault.
                                    Err(e)
                                        if matches!(
                                            e.kind(),
                                            std::io::ErrorKind::ConnectionReset
                                                | std::io::ErrorKind::ConnectionAborted
                                        ) =>
                                    {
                                        info!(
                                            "{INNER_NAME}: phone closed the upgraded socket ({e})"
                                        );
                                    }
                                    Err(e) => warn!("{INNER_NAME}: upgraded socket failed: {e}"),
                                }
                                break;
                            }
                            None => warn!(
                                "{INNER_NAME}: asked to switch but no upgraded socket arrived"
                            ),
                        }
                    }
                    _ = down_rx.recv() => {
                        warn!("{INNER_NAME}: BLE link dropped, tearing the bridge down");
                        break;
                    }
                    else => break,
                }
            }
            info!("{INNER_NAME}: bridge pump exited");
        });
        }
        });

        tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            // Create the GATT service provider for 0xFEF3.
            let create_result =
                GattServiceProvider::CreateAsync(COPRESENCE_SERVICE_GUID)?.get()?;
            let error = create_result.Error()?;
            if error.0 != 0 {
                return Err(anyhow::anyhow!(
                    "GattServiceProvider::CreateAsync failed: BluetoothError {}",
                    error.0
                ));
            }
            let provider = create_result.ServiceProvider()?;

            // Serve the full advertisement from the slot-0 characteristic. The
            // phone connects after seeing our advert and reads this; with no
            // characteristic it finds an empty service and gives up, which is
            // why we were never listed.
            let char_params = GattLocalCharacteristicParameters::new()?;
            char_params.SetCharacteristicProperties(GattCharacteristicProperties::Read)?;
            let char_result = provider
                .Service()?
                .CreateCharacteristicAsync(ADV_SLOT0_CHARACTERISTIC_GUID, &char_params)?
                .get()?;
            let characteristic = char_result.Characteristic()?;

            let adv_for_read = full_advertisement.clone();
            characteristic.ReadRequested(&TypedEventHandler::<
                GattLocalCharacteristic,
                GattReadRequestedEventArgs,
            >::new(move |_sender, args| {
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                // Must take a deferral: fetching the request is async and WinRT
                // will otherwise consider the event handled with no response.
                let deferral = args.GetDeferral()?;
                let request = args.GetRequestAsync()?.get()?;
                let writer = DataWriter::new()?;
                writer.WriteBytes(&adv_for_read)?;
                request.RespondWithValue(&writer.DetachBuffer()?)?;
                deferral.Complete()?;
                info!("*** {INNER_NAME}: served advertisement over GATT read ***");
                Ok(())
            }))?;

            // Connectable: puts the 0xFEF3 service UUID on-air so the phone's
            // receiver scan finds us. A GATT service must be connectable to
            // advertise at all (non-connectable -> status 3 Aborted). Our 26-byte
            // fast advertisement won't also fit the packet (-> status 4,
            // StartedWithoutAllAdvertisementData), so it is NOT delivered inline;
            // the phone fetches the advertisement/metadata over the GATT
            // connection instead (isPrivateGatt / rxAdvertisement). That GATT
            // characteristic is the next piece to build - for now this just gets
            // the UUID discoverable so we can confirm the phone finds us.
            let params = GattServiceProviderAdvertisingParameters::new()?;
            params.SetIsConnectable(true)?;
            params.SetIsDiscoverable(false)?;

            let writer = DataWriter::new()?;
            writer.WriteBytes(&advertisement)?;
            params.SetServiceData(&writer.DetachBuffer()?)?;

            // BLE socket: server-tx (us -> phone, via notify). Created first so
            // the client-tx write handler can reply on it.
            let stx_params = GattLocalCharacteristicParameters::new()?;
            // Notify *and* Indicate. Declaring Notify alone made NotifyValueAsync
            // return an empty result list - delivered to nobody - even with
            // SubscribedClients() == 1, i.e. the phone had subscribed for
            // indications and there was no matching subscriber to notify.
            // Notify *and* Indicate, and the Indicate is not optional.
            //
            // Tried and measured (2026-07-20): declaring Notify alone gives
            // `SubscribedClients() == 1` but `delivery statuses []` - the phone
            // subscribes for indications only, so notifications reach nobody. It
            // then answers with a Weave Error (0x92) and reconnects in a loop,
            // and the accept prompt never appears. So the ATT-confirmation
            // timeout that indications carry cannot be dodged this way; it has
            // to be lived with. Do not "simplify" this to Notify.
            stx_params.SetCharacteristicProperties(
                GattCharacteristicProperties::Notify | GattCharacteristicProperties::Indicate,
            )?;
            let stx_result = provider
                .Service()?
                .CreateCharacteristicAsync(BLE_SOCKET_SERVER_TX_GUID, &stx_params)?
                .get()?;
            let server_tx = stx_result.Characteristic()?;

            // Log subscribe/unsubscribe so a stalled transfer can be told apart
            // from a dropped link. Without this, "phone tore down the BLE
            // connection" and "phone stayed connected but stopped sending" look
            // identical from here: both are just silence in the log.
            server_tx.SubscribedClientsChanged(&TypedEventHandler::<
                GattLocalCharacteristic,
                IInspectable,
            >::new(move |sender, _| {
                if let Some(s) = sender.as_ref() {
                    let n = s.SubscribedClients().and_then(|c| c.Size()).unwrap_or(0);
                    if n == 0 {
                        warn!("{INNER_NAME}: phone unsubscribed - BLE link dropped");
                        // Unblock the current session so the transfer fails
                        // visibly instead of hanging.
                        if let Ok(slot) = handler_down.lock() {
                            if let Some(tx) = slot.as_ref() {
                                let _ = tx.send(());
                            }
                        }
                    } else {
                        info!("{INNER_NAME}: server-tx subscribers = {n}");
                    }
                }
                Ok(())
            }))?;

            // BLE socket: client-tx (phone writes to us). Log every write - this
            // is how we learn the socket framing, since the protocol above it is
            // undocumented.
            let ctx_params = GattLocalCharacteristicParameters::new()?;
            ctx_params.SetCharacteristicProperties(
                GattCharacteristicProperties::Write
                    | GattCharacteristicProperties::WriteWithoutResponse,
            )?;
            let ctx_result = provider
                .Service()?
                .CreateCharacteristicAsync(BLE_SOCKET_CLIENT_TX_GUID, &ctx_params)?
                .get()?;
            let client_tx = ctx_result.Characteristic()?;

            // Reassembly state for multi-packet Weave messages.
            //
            // WinRT dispatches `WriteRequested` on threadpool threads, so two
            // handlers run concurrently and reassemble in whatever order they
            // win the lock - even though BLE delivered the packets in order.
            // That is not theoretical: a capture showed ctr=2 (last fragment)
            // processed before ctr=1 (first fragment), which emitted a
            // tail-only message, then flushed ctr=1's head as a bogus
            // "multiplex control" frame. One D2D message was destroyed, the
            // sequence number jumped 103 -> 104 and the session died mid-file.
            //
            // So don't trust arrival order - trust the counter, which is
            // sequential mod 8. Park each packet in its slot and only consume
            // the one we're expecting next.
            struct WeaveRx {
                acc: Vec<u8>,
                expected: u8,
                pending: [Option<Vec<u8>>; 8],
            }
            let rx_buf = std::sync::Arc::new(std::sync::Mutex::new(WeaveRx {
                acc: Vec::new(),
                expected: 0,
                pending: Default::default(),
            }));
            // The write handler needs its own handle: `server_tx` itself stays
            // with the outbound Weave sender on the owning thread.
            let confirm_tx = server_tx.clone();

            // ONE Weave counter per direction, shared by control *and* data
            // packets. The phone's own stream is the proof: ConnectionRequest
            // 0x80 (ctr 0), data ctr 1,2,3,4, then Error 0xd2 (ctr 5) - its
            // sixth packet. So our ConnectionConfirm consumes counter 0 and the
            // first data packet must be counter 1. Keeping a separate data
            // counter that also started at 0 made us send counter 0 twice,
            // which is exactly what the phone answered with a Weave Error.
            let tx_counter = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
            let confirm_counter = tx_counter.clone();
            client_tx.WriteRequested(&TypedEventHandler::<
                GattLocalCharacteristic,
                GattWriteRequestedEventArgs,
            >::new(move |_sender, args| {
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let t_enter = std::time::Instant::now();
                let deferral = args.GetDeferral()?;
                let request = args.GetRequestAsync()?.get()?;
                let reader = DataReader::FromBuffer(&request.Value()?)?;
                let len = reader.UnconsumedBufferLength()? as usize;
                let mut buf = vec![0u8; len];
                reader.ReadBytes(&mut buf)?;
                if request.Option()? == GattWriteOption::WriteWithResponse {
                    request.Respond()?;
                }
                deferral.Complete()?;

                // Weave framing. Header byte: bit7 = control(1)/data(0).
                // Data packets: bits 6-4 = counter, bit3 = first fragment,
                // bit2 = last fragment. Reassemble until the last fragment, then
                // log the whole message so the multiplex layer above can be
                // decoded (truncated logs are why it was still opaque).
                if len == 0 {
                    // A zero-length write carries no Weave header. It has to be
                    // rejected *before* the branches below, both of which index
                    // buf[0]: a panic here cannot unwind across the WinRT
                    // callback boundary, so it aborts the process, and Windows
                    // reports that abort as "buffer overrun" with nothing at all
                    // in the log.
                    debug!("{INNER_NAME}: ignoring zero-length GATT write");
                } else if buf[0] & 0x80 == 0 {
                    let counter = ((buf[0] >> 4) & 0x07) as usize;
                    if let Ok(mut st) = rx_buf.lock() {
                        if st.pending[counter].is_some() {
                            // Eight outstanding packets means the counter wrapped
                            // onto an unconsumed slot: we've lost a packet and the
                            // stream can't be recovered by waiting.
                            warn!(
                                "{INNER_NAME}: weave slot {counter} already occupied - \
                                 lost a packet, stream is desynced"
                            );
                        }
                        let out_of_order = counter as u8 != st.expected;
                        if out_of_order {
                            info!(
                                "{INNER_NAME}: weave pkt ctr={counter} arrived early \
                                 (expecting {}), holding",
                                st.expected
                            );
                        }
                        st.pending[counter] = Some(std::mem::take(&mut buf));

                        // Drain every packet that is now contiguous.
                        while let Some(pkt) = {
                            let exp = st.expected as usize;
                            st.pending[exp].take()
                        } {
                            st.expected = (st.expected + 1) & 0x07;
                            let header = pkt[0];
                            let ctr = (header >> 4) & 0x07;
                            let first = header & 0x08 != 0;
                            let last = header & 0x04 != 0;
                            // Two sanity checks on reassembly. Both are silent
                            // in normal operation and both would have named the
                            // out-of-order bug immediately instead of surfacing
                            // as a D2D sequence-number error several frames
                            // later. An intermittent "Missing required fields"
                            // during the handshake is exactly what a stale or
                            // misordered packet spliced into a message looks
                            // like, so make that visible rather than inferred.
                            if first && !st.acc.is_empty() {
                                warn!(
                                    "{INNER_NAME}: REASSEMBLY - new message (ctr={ctr}) starts \
                                     with {} B still accumulated; previous message lost its tail",
                                    st.acc.len()
                                );
                            }
                            if !first && st.acc.is_empty() {
                                warn!(
                                    "{INNER_NAME}: REASSEMBLY - continuation packet (ctr={ctr}, \
                                     last={last}) with nothing accumulated; its head is missing"
                                );
                            }
                            if first {
                                st.acc.clear();
                            }
                            st.acc.extend_from_slice(&pkt[1..]);
                            // Per-packet logging runs *inside* the WinRT write
                            // callback, so its cost is paid on the thread that
                            // has to drain the BLE receive queue. At ~35
                            // packets/s with a coloured stdout writer and a file
                            // writer it throttled us below the phone's send
                            // rate, the phone's socket queue backed up, and a
                            // 1.9 MB photo arrived 60% complete while the phone
                            // reported success. Keep the detail at `trace`.
                            trace!(
                                "{INNER_NAME}: weave data pkt ctr={ctr} first={first} \
                                 last={last} +{} B (acc {} B)",
                                pkt.len() - 1,
                                st.acc.len()
                            );
                            if last {
                                let msg = std::mem::take(&mut st.acc);
                                trace!("*** {INNER_NAME}: weave MESSAGE {} B ***", msg.len());
                                // Never swallow this. A closed pump means the
                                // Nearby side is gone and the phone is talking to
                                // nobody - silently dropping here reads exactly
                                // like "the message never arrived", which is a
                                // much harder bug.
                                let sent = handler_inbound
                                    .lock()
                                    .ok()
                                    .and_then(|s| s.as_ref().map(|tx| tx.send(msg)));
                                match sent {
                                    Some(Ok(())) => {}
                                    Some(Err(e)) => warn!(
                                        "{INNER_NAME}: session is gone, dropping {} B Weave message",
                                        e.0.len()
                                    ),
                                    None => warn!(
                                        "{INNER_NAME}: no session yet, dropping a Weave message"
                                    ),
                                }
                            }
                        }
                    }
                } else {
                    // Control packets share the counter space with data, so they
                    // set where the data sequence resumes. Seeding from the
                    // ConnectionRequest is what makes `expected` correct without
                    // hardcoding "data starts at 1".
                    if let Ok(mut st) = rx_buf.lock() {
                        st.expected = (((buf[0] >> 4) & 0x07) + 1) & 0x07;
                        // A fresh connection: nothing from the last one belongs
                        // in this one's reassembly.
                        st.acc.clear();
                        st.pending = Default::default();
                    }

                    // Command 0 = ConnectionRequest, i.e. a new Weave
                    // connection. Build it a session of its own.
                    if buf[0] & 0x0f == 0 {
                        let _ = new_session_tx.send(());
                    }
                    info!(
                        "*** {INNER_NAME}: weave control {len} B: ctr={} cmd={} ({}) {:02x?} ***",
                        (buf[0] >> 4) & 0x07,
                        buf[0] & 0x0f,
                        match buf[0] & 0x0f {
                            0 => "ConnectionRequest",
                            1 => "ConnectionConfirm",
                            2 => "Error",
                            _ => "unknown",
                        },
                        &buf[..buf.len().min(32)]
                    );
                }

                // uWeave control packet: bit 7 set, low nibble = command.
                // Command 0 = Connection Request:
                //   [0x80][version_min u16][version_max u16][max_packet_size u16]
                // The phone sent 80 00 01 00 01 01 fd = v1..v1, 509-byte packets,
                // then waits for our Connection Confirm (command 1):
                //   [0x81][version u16][packet_size u16]
                // Without it the phone reports
                // GATT_SWITCH_TO_DATA_TRANSFERRING_FAILED [TIMEOUT].
                // Guard on `buf.len()`, never on `len`. `len` is the length of
                // the *original* write, but the data branch above moves `buf`
                // out into the reorder slot - so for every data packet of 7+
                // bytes `len >= 7` is still true while `buf` is empty, and
                // `buf[0]` panics. In a WinRT callback that panic can't unwind,
                // so it aborts as STATUS_STACK_BUFFER_OVERRUN.
                if buf.len() >= 7 && buf[0] == 0x80 {
                    let ver_max = u16::from_be_bytes([buf[3], buf[4]]);
                    let their_max = u16::from_be_bytes([buf[5], buf[6]]);
                    let version = ver_max.min(1);
                    let packet_size = their_max.min(509);
                    // Header: bit7 = control, bits 6-4 = counter, low nibble =
                    // command (1 = ConnectionConfirm). Draws from the shared
                    // per-direction counter so the data packets that follow
                    // continue the sequence instead of restarting it.
                    let ctr = confirm_counter
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        & 0x07;
                    let confirm = [
                        0x80 | (ctr << 4) | 0x01,
                        (version >> 8) as u8,
                        version as u8,
                        (packet_size >> 8) as u8,
                        packet_size as u8,
                    ];
                    let subscribers = confirm_tx
                        .SubscribedClients()
                        .and_then(|c| c.Size())
                        .unwrap_or(0);
                    let w = DataWriter::new()?;
                    w.WriteBytes(&confirm)?;
                    let buffer = w.DetachBuffer()?;
                    // Do NOT block on .get() here - this runs on a WinRT event
                    // handler thread and waiting on the async op can deadlock.
                    // Log failures rather than `?`-ing out, which previously made
                    // the confirm vanish with no trace.
                    // Report delivery via a completion handler. Blocking on
                    // .get() inside this WinRT callback is illegal (0x8000000E),
                    // and a worker thread is out because IBuffer isn't Send - but
                    // a completion callback costs neither.
                    match confirm_tx.NotifyValueAsync(&buffer) {
                        Ok(op) => {
                            let op_for_log = op.clone();
                            let _ = op.SetCompleted(&AsyncOperationCompletedHandler::new(
                                move |_, _| {
                                    let mut statuses = Vec::new();
                                    if let Ok(results) = op_for_log.GetResults() {
                                        for i in 0..results.Size().unwrap_or(0) {
                                            if let Ok(r) = results.GetAt(i) {
                                                statuses
                                                    .push(r.Status().map(|s| s.0).unwrap_or(-1));
                                            }
                                        }
                                    }
                                    info!(
                                        "*** {INNER_NAME}: ConnectionConfirm delivery statuses \
                                         {statuses:?} (0 = Success) ***"
                                    );
                                    Ok(())
                                },
                            ));
                            info!(
                                "*** {INNER_NAME}: sent Weave ConnectionConfirm (v{version}, \
                                 {packet_size} B, {subscribers} subscriber(s)) ***"
                            );
                        }
                        Err(e) => warn!("{INNER_NAME}: NotifyValueAsync failed: {e}"),
                    }
                }
                handler_us.fetch_add(
                    t_enter.elapsed().as_micros() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                handler_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }))?;

            provider.StartAdvertisingWithParameters(&params)?;
            info!(
                "{INNER_NAME}: GATT service advertising started ({} B service data) under 0xFEF3",
                advertisement.len()
            );

            // Outbound Weave: split whole [hash][len][frame] messages from the
            // bridge into (packet_size - 1) chunks with header
            // (counter<<4) | (first<<3) | (last<<2), and *queue* them.
            //
            // Queueing rather than firing immediately is the point. Every one of
            // these is an indication, and an indication unconfirmed for 30 s
            // obliges the stack to drop the ACL. Under inbound congestion
            // confirmations were taking 8-10 s each, so firing them all left a
            // growing backlog whose oldest entry eventually crossed 30 s:
            // measured `ctr=5 confirmed after 37175 ms, statuses [1]` followed
            // immediately by the link dropping. With one in flight, the ATT
            // timer only ever sees a single confirmation latency; the rest wait
            // in our queue where no timer is running.
            let frame_weave = |bytes: &[u8], out: &mut std::collections::VecDeque<Vec<u8>>| {
                let max_payload = 508usize; // negotiated 509 minus the header byte
                let chunks: Vec<&[u8]> = bytes.chunks(max_payload).collect();
                let n = chunks.len();
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let first = i == 0;
                    let last = i + 1 == n;
                    let ctr = tx_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0x07;
                    let header = (ctr << 4)
                        | if first { 0x08 } else { 0 }
                        | if last { 0x04 } else { 0 };

                    let mut pkt = Vec::with_capacity(1 + chunk.len());
                    pkt.push(header);
                    pkt.extend_from_slice(chunk);
                    out.push_back(pkt);
                }
                debug!(
                    "{INNER_NAME}: weave queued {} B as {n} packet(s), next ctr={}",
                    bytes.len(),
                    tx_counter.load(std::sync::atomic::Ordering::Relaxed) & 0x07
                );
            };

            let mut tx_queue: std::collections::VecDeque<Vec<u8>> =
                std::collections::VecDeque::new();
            let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

            let mut last_status: i32 = -1;
            while !ctk.is_cancelled() {
                while let Ok(msg) = outbound_rx.try_recv() {
                    frame_weave(&msg, &mut tx_queue);
                }

                // Deliberately no queue cap here. `frame_weave` assigns the
                // Weave counter when it queues a packet, so dropping a queued
                // one leaves a gap in our outbound sequence - and the phone
                // validates that sequence (it answered an earlier counter bug
                // with Weave Error 0x92). Trading a timeout for a protocol
                // violation is not a fix. The rate of keepalive responses is
                // limited at the source instead, in inbound.rs, where we
                // actually know which frames are redundant.
                if tx_queue.len() > 4 {
                    warn!(
                        "{INNER_NAME}: outbound backing up - {} packets queued; the link is not \
                         draining our indications",
                        tx_queue.len()
                    );
                }

                // Publish what is still owed to the peer, so the bridge can wait
                // for SAFE_TO_CLOSE to actually land before switching mediums.
                tx_pending.store(
                    tx_queue.len()
                        + usize::from(in_flight.load(std::sync::atomic::Ordering::Relaxed)),
                    std::sync::atomic::Ordering::Relaxed,
                );

                // At most one indication outstanding. The completion handler
                // clears the flag, so the next goes out only once the phone has
                // confirmed the previous one.
                if !in_flight.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(pkt) = tx_queue.pop_front() {
                        let ctr = (pkt[0] >> 4) & 0x07;
                        let queued = tx_queue.len();
                        let send = || -> Result<(), anyhow::Error> {
                            let w = DataWriter::new()?;
                            w.WriteBytes(&pkt)?;
                            match server_tx.NotifyValueAsync(&w.DetachBuffer()?) {
                                Ok(op) => {
                                    in_flight
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                    let flag = in_flight.clone();
                                    let op_for_log = op.clone();
                                    let sent_at = std::time::Instant::now();
                                    let _ = op.SetCompleted(
                                        &AsyncOperationCompletedHandler::new(move |_, _| {
                                            let mut statuses = Vec::new();
                                            if let Ok(results) = op_for_log.GetResults() {
                                                for i in 0..results.Size().unwrap_or(0) {
                                                    if let Ok(r) = results.GetAt(i) {
                                                        statuses.push(
                                                            r.Status().map(|s| s.0).unwrap_or(-1),
                                                        );
                                                    }
                                                }
                                            }
                                            let ms = sent_at.elapsed().as_millis();
                                            // 30 s is the ATT timeout; anything
                                            // approaching it means the link is
                                            // about to be dropped under us.
                                            if statuses.iter().any(|s| *s != 0) || ms > 10_000 {
                                                warn!(
                                                    "{INNER_NAME}: indication ctr={ctr} confirmed \
                                                     after {ms} ms, statuses {statuses:?} \
                                                     (0 = Success), {queued} still queued"
                                                );
                                            } else {
                                                debug!(
                                                    "{INNER_NAME}: indication ctr={ctr} confirmed \
                                                     in {ms} ms ({queued} queued)"
                                                );
                                            }
                                            flag.store(
                                                false,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                            Ok(())
                                        }),
                                    );
                                }
                                Err(e) => warn!("{INNER_NAME}: NotifyValueAsync failed: {e}"),
                            }
                            Ok(())
                        };
                        if let Err(e) = send() {
                            warn!("{INNER_NAME}: weave send failed: {e}");
                        }
                    }
                }
                // A session ended: stop and restart advertising, which drops the
                // peer's GATT connection. Without this the phone stays connected
                // after a transfer and the *other* direction cannot get the link.
                if recycle_adv.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    let _ = provider.StopAdvertising();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    match provider.StartAdvertisingWithParameters(&params) {
                        Ok(()) => {
                            // Did it actually drop? If subscribers is still 1
                            // the recycle did not do what it is here to do, and
                            // the next transfer in either direction will fight
                            // for the link.
                            let subs = server_tx
                                .SubscribedClients()
                                .and_then(|c| c.Size())
                                .unwrap_or(0);
                            info!(
                                "{INNER_NAME}: recycled the advertisement; subscribers now {subs}"
                            );
                        }
                        Err(e) => warn!("{INNER_NAME}: could not restart advertising: {e}"),
                    }
                    last_status = -1;
                }

                if let Ok(status) = provider.AdvertisementStatus() {
                    if status.0 != last_status {
                        last_status = status.0;
                        info!(
                            "{INNER_NAME}: advertisement status = {} ({})",
                            last_status,
                            adv_status_name(last_status)
                        );
                    }
                }
                // 50ms, not 250ms: this loop now also paces outbound
                // indications, and at 250ms the handshake burst (~8 frames)
                // would take two seconds to get out.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            info!("{INNER_NAME}: tracker cancelled, stopping advertiser");
            let _ = provider.StopAdvertising();
            Ok(())
        })
        .await??;

        Ok(())
    }
}
