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
    GattReadRequestedEventArgs, GattServiceProvider, GattServiceProviderAdvertisementStatus,
    GattServiceProviderAdvertisementStatusChangedEventArgs,
    GattServiceProviderAdvertisingParameters, GattWriteOption, GattWriteRequestedEventArgs,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_future::AsyncOperationCompletedHandler;

use super::ble_receiver;

const INNER_NAME: &str = "BleReceiverAdvertiser";

// Copresence service 0000FEF3-0000-1000-8000-00805F9B34FB.
const COPRESENCE_SERVICE_GUID: GUID = GUID::from_u128(0x0000_FEF3_0000_1000_8000_0080_5F9B_34FB);

// Per-slot advertisement characteristic 00000000-0000-3000-8000-000000000000
// (slot 0). A peer that finds our advertisement connects and reads this to get
// the full BleAdvertisement - the `isPrivateGatt` / `rxAdvertisement` flow.
const ADV_SLOT0_CHARACTERISTIC_GUID: GUID =
    GUID::from_u128(0x0000_0000_0000_3000_8000_0000_0000_0000);

// Nearby's BLE socket ("MultiplexBleSocketImpl"). After reading our
// advertisement the phone connects and looks for these; without the client-tx
// one it logs `missing client tx characteristic ...0101` and aborts with
// ESTABLISH_GATT_CONNECTION_FAILED.
//   client tx = phone -> us   (phone writes)
//   server tx = us -> phone   (we notify)
// Only ...0101 is confirmed from logcat; ...0102 is the obvious counterpart in
// the same scheme. The phone names whatever is still missing, so iterate on it.
const BLE_SOCKET_CLIENT_TX_GUID: GUID = GUID::from_u128(0x0000_0100_0004_1000_8000_001a_1100_0101);
const BLE_SOCKET_SERVER_TX_GUID: GUID = GUID::from_u128(0x0000_0100_0004_1000_8000_001a_1100_0102);

/// Send a Weave ConnectionConfirm notification, retrying on non-Success delivery.
///
/// The first connection to a freshly advertised receiver fails intermittently
/// and a re-send works - the long-standing "first BLE connection is flaky" bug.
/// The receive-side signature: our ConnectionConfirm comes back delivery status
/// `Unreachable` (1) even though the phone has subscribed (measured: the failing
/// first connection logged delivery `[1]`, the phone's own retry a moment later
/// logged `[0]`), and the phone drops the link about a second later having never
/// received it.
///
/// The cause is context, not the link: this notification is sent from inside the
/// GATT write handler that just received the ConnectionRequest, so the ATT layer
/// is still completing that write when we notify. Retrying from the delivery
/// completion callback runs *after* that transaction, which is why it gets
/// through - and why the phone's later retry always worked.
///
/// The same 5 bytes are re-sent, keeping the Weave counter the phone never saw.
/// Retries end with the session: once a newer one has replaced it, or its phone
/// has unsubscribed - the phone dropping the link is what ends a failed one.
fn notify_confirm(
    confirm_tx: GattLocalCharacteristic,
    confirm: [u8; 5],
    device: String,
    active: std::sync::Arc<std::sync::Mutex<ActivePeer>>,
    generation: u64,
) {
    let buffer = match (|| {
        let w = DataWriter::new()?;
        w.WriteBytes(&confirm)?;
        w.DetachBuffer()
    })() {
        Ok(b) => b,
        Err(e) => {
            warn!("{INNER_NAME}: could not build ConnectionConfirm buffer: {e}");
            return;
        }
    };

    let tx_for_cb = confirm_tx.clone();
    let device_for_cb = device.clone();
    let sent = notify_device(&confirm_tx, &buffer, Some(&device), move |statuses| {
        let delivered = !statuses.is_empty() && statuses.iter().all(|s| *s == 0);
        let current = active
            .lock()
            .is_ok_and(|a| a.generation() == Some(generation));
        if delivered {
            info!(
                "*** {INNER_NAME}: ConnectionConfirm delivery statuses {statuses:?} \
                 (0 = Success) ***"
            );
        } else if current && subscribed_client(&tx_for_cb, &device_for_cb).is_some() {
            warn!("*** {INNER_NAME}: ConnectionConfirm not delivered ({statuses:?}); retrying ***");
            notify_confirm(
                tx_for_cb.clone(),
                confirm,
                device_for_cb.clone(),
                active.clone(),
                generation,
            );
        } else {
            warn!(
                "*** {INNER_NAME}: ConnectionConfirm not delivered ({statuses:?}) and the \
                 session is over; not retrying ***"
            );
        }
    });
    if let Err(e) = sent {
        warn!("{INNER_NAME}: NotifyValueAsync failed: {e}");
    }
}

use super::ble_receiver::ActivePeer;

/// The Bluetooth device id behind a GATT session, for telling peers apart.
fn session_device(
    session: windows::core::Result<
        windows::Devices::Bluetooth::GenericAttributeProfile::GattSession,
    >,
) -> Option<String> {
    Some(session.ok()?.DeviceId().ok()?.Id().ok()?.to_string_lossy())
}

/// Send `buffer` as an indication to `device`, and call `done` with the
/// delivery statuses (0 = Success) once it completes.
///
/// Only the session's device gets it. Notifying every subscriber sent this
/// session's Weave packets to any other device subscribed at the time, which
/// is garbage to it. When the device can't be found among the subscribers -
/// it has just gone - this falls back to all of them, as before.
fn notify_device(
    characteristic: &GattLocalCharacteristic,
    buffer: &windows::Storage::Streams::IBuffer,
    device: Option<&str>,
    done: impl Fn(Vec<i32>) + Send + 'static,
) -> windows::core::Result<()> {
    if let Some(client) = device.and_then(|d| subscribed_client(characteristic, d)) {
        let op = characteristic.NotifyValueForSubscribedClientAsync(buffer, &client)?;
        let result = op.clone();
        op.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
            let status = result
                .GetResults()
                .and_then(|r| r.Status())
                .map(|s| s.0)
                .unwrap_or(-1);
            done(vec![status]);
            Ok(())
        }))
    } else {
        let op = characteristic.NotifyValueAsync(buffer)?;
        let results = op.clone();
        op.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
            let mut statuses = Vec::new();
            if let Ok(results) = results.GetResults() {
                for i in 0..results.Size().unwrap_or(0) {
                    if let Ok(r) = results.GetAt(i) {
                        statuses.push(r.Status().map(|s| s.0).unwrap_or(-1));
                    }
                }
            }
            done(statuses);
            Ok(())
        }))
    }
}

/// The subscribed client of `characteristic` that is `device`, if subscribed.
fn subscribed_client(
    characteristic: &GattLocalCharacteristic,
    device: &str,
) -> Option<windows::Devices::Bluetooth::GenericAttributeProfile::GattSubscribedClient> {
    let clients = characteristic.SubscribedClients().ok()?;
    (0..clients.Size().ok()?)
        .filter_map(|i| clients.GetAt(i).ok())
        .find(|c| session_device(c.Session()).as_deref() == Some(device))
}

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
    /// Hidden means not discoverable over Bluetooth either: the advertisement
    /// is paused while hidden, except under a phone that is already connected.
    visibility: tokio::sync::watch::Receiver<crate::hdl::Visibility>,
}

impl BleReceiverAdvertiser {
    pub fn new(
        endpoint_id: [u8; 4],
        device_type: u8,
        name: String,
        sender: tokio::sync::broadcast::Sender<crate::channel::ChannelMessage>,
        visibility: tokio::sync::watch::Receiver<crate::hdl::Visibility>,
    ) -> Self {
        Self {
            endpoint_id,
            device_type,
            name,
            sender,
            visibility,
        }
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        // Two pieces, two channels:
        //  - header: the advertisement packet's service data. Says there is an
        //    advertisement to read over GATT and carries its hash.
        //  - full: served over GATT when a peer connects. Carries the device
        //    name, which is what the phone needs to build a listable
        //    ShareTarget. Serving the *fast* form here was why the phone read us
        //    successfully and still never listed us.
        let full_advertisement = ble_receiver::build_full_receiver_advertisement(
            &self.endpoint_id,
            self.device_type,
            &self.name,
        );
        let header = ble_receiver::build_advertisement_header(&full_advertisement);

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
        // Cancelled when the current session's phone drops the link.
        let down_slot: std::sync::Arc<std::sync::Mutex<Option<CancellationToken>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let handler_inbound = inbound_slot.clone();
        let handler_down = down_slot.clone();
        // The device the current session belongs to, and that session's
        // generation. The GATT server takes writes from any device in range
        // without pairing, and the reassembly state and counters are shared, so
        // any device that wrote a ConnectionRequest used to reset them and take
        // over - killing a transfer in progress, even when it was only a second
        // phone looking for Quick Share targets. Only the device that opened the
        // session may write to it now; see `ActivePeer`.
        let active = std::sync::Arc::new(std::sync::Mutex::new(ActivePeer::default()));
        let session_active = active.clone();
        let handler_active = active.clone();
        let subscribers_active = active.clone();
        let outbound_active = active.clone();
        // Signalled by the write handler when a Weave ConnectionRequest arrives,
        // with the generation of the session to start.
        let (new_session_tx, mut new_session_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();

        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

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
        //
        // The pump counts a message the moment it queues it, and the sender
        // thread swaps that for its packets and counts each one off as its
        // indication completes. This used to be a copy the sender refreshed on
        // its 50 ms tick, so right after the pump queued something it still
        // read 0 and the waits below didn't wait.
        let outstanding = ble_receiver::Outstanding::new();
        let pump_outstanding = outstanding.clone();
        // The Weave packet size the last ConnectionConfirm agreed.
        let agreed_size = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(509));
        let confirm_size = agreed_size.clone();

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

        // Wakes the thread that owns the GATT objects whenever something it
        // acts on may have changed: an indication completed, a session ended,
        // the advertisement status or the subscribers changed, or a session
        // moved to its upgraded socket. It acts on the current state each time,
        // so a wake that finds nothing to do costs nothing.
        let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let session_wake = wake_tx.clone();

        // The generation of a session that has handed its stream to the
        // upgraded socket. Its remaining Weave packets go out without waiting
        // on each other's confirmations: once the phone has joined the new
        // network it stops confirming BLE indications, and the multiplex
        // channel close queued behind an unconfirmed one would reach it far too
        // late. 0 = none; generations start at 1.
        let flush_generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let session_flush = flush_generation.clone();

        let session_ctk = ctk.clone();

        // Supervisor: build a fresh session for every Weave connection.
        tokio::spawn(async move {
            while let Some(generation) = new_session_rx.recv().await {
                info!("{INNER_NAME}: new Weave connection, starting a fresh session");
                let end_active = session_active.clone();

                let (inbound_tx, mut inbound_rx) =
                    tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                let link_down = CancellationToken::new();
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
                    *slot = Some(link_down.clone());
                }

                let outbound_tx = session_outbound_tx.clone();
                let pump_us = pump_us.clone();
                let pump_calls = pump_calls.clone();
                let pump_outstanding = pump_outstanding.clone();
                let msg_sender = session_sender.clone();
                let ui_sender = session_sender.clone();
                let end_recycle = session_recycle.clone();
                // The session task waits for its own last frames to be confirmed
                // before releasing the link - see the recycle below.
                let end_outstanding = pump_outstanding.clone();
                let end_link_down = link_down.clone();
                let end_wake = session_wake.clone();
                let end_ctk = session_ctk.clone();
                let pump_wake = session_wake.clone();
                let pump_flush = session_flush.clone();
                let pump_ctk = session_ctk.clone();

                tokio::spawn(async move {
                    // Off the radio while we receive: a concurrent BLE scan starves the
                    // connection. Released when this session ends, whatever ends it.
                    let _pause = crate::hdl::DiscoveryPause::new();

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
                    // exchange. That was patched with a flat 5s guess; the exact
                    // condition is "our final frames have been confirmed", which is
                    // precisely what the drained signal means. Wait for the event, not
                    // the clock - it releases the link as soon as the peer has really
                    // caught up, and waits longer than 5s if this link is slow.
                    //
                    // Every indication ends in a completion, confirmed or failed at
                    // the stack's own ATT timeout, so this resolves without a clock of
                    // ours. A dropped link ends it at once: nothing more will be
                    // confirmed there.
                    tokio::select! {
                        _ = end_outstanding.drained() => {}
                        _ = end_link_down.cancelled() => info!(
                            "{INNER_NAME}: the link dropped with {} packet(s) unconfirmed",
                            end_outstanding.count()
                        ),
                        _ = end_ctk.cancelled() => {}
                    }
                    // Free the server for the next device, unless a newer
                    // session already took it.
                    let current = end_active
                        .lock()
                        .map(|mut active| active.end(generation))
                        .unwrap_or(false);
                    // Drop the peer's GATT connection, so the next transfer - in either
                    // direction - starts from a clean link. Not when a newer session
                    // replaced this one, e.g. the phone retrying a flaky first connect:
                    // recycling then dropped the retry's link.
                    if current {
                        end_recycle.store(true, std::sync::atomic::Ordering::Relaxed);
                        if end_wake.send(()).is_err() {
                            debug!("{INNER_NAME}: the advertiser has stopped");
                        }
                    }
                });

                // Pump: reassembled Weave messages in, framed replies out.
                tokio::spawn(async move {
                    use futures::FutureExt;
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
                    // Counted before it can reach the sender thread, so a waiter
                    // never sees 0 while a message sits in the channel.
                    let queue = |out: Vec<u8>| {
                        pump_outstanding.add(1);
                        let sent = outbound_tx.send(out).is_ok();
                        if !sent {
                            pump_outstanding.done(1);
                        }
                        sent
                    };
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
                                        let mean_us = us.checked_div(calls).unwrap_or(0);
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
                                            if !queue(out) {
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
                                        //
                                        // InboundRequest finishes writing SAFE_TO_CLOSE
                                        // into the duplex before it signals, so the bytes
                                        // are already there: take what is readable now
                                        // and stop at the first read that would wait.
                                        loop {
                                            let mut buf = [0u8; 8192];
                                            match ours.read(&mut buf).now_or_never() {
                                                Some(Ok(n)) if n > 0 => {
                                                    pending.extend_from_slice(&buf[..n])
                                                }
                                                _ => break,
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
                                            if !queue(out) {
                                                break;
                                            }
                                        }

                                        // Close the multiplex service channel, the same
                                        // way the send path does - this direction was
                                        // missing it.
                                        //
                                        // The phone can finish the whole upgrade
                                        // handshake and then send nothing over the
                                        // hotspot: measured here as "nothing on the
                                        // upgraded socket within 45s", with the phone
                                        // having connected and introduced itself over
                                        // that very socket, so it is reachable and simply
                                        // not sending. Its sender waits for the BLE
                                        // service channel to close before committing to
                                        // the new medium - the mirror of the reader
                                        // behaviour that stranded PC->phone until we
                                        // closed the channel there (see blea_send.rs).
                                        //
                                        // We host the GATT server, so we close the
                                        // channel by *notifying* the DISCONNECT rather
                                        // than writing it: queue it on outbound_tx now so
                                        // it rides out in the same drain as SAFE_TO_CLOSE,
                                        // before the switch. 00 00 00 = multiplex
                                        // control; 08 02 1a 05 0a 03 <hash> = disconnect
                                        // the named service; fc 9f 5e is this session's
                                        // service hash, the one used for data framing
                                        // above.
                                        let disconnect = vec![
                                            0x00, 0x00, 0x00, 0x08, 0x02, 0x1a, 0x05, 0x0a, 0x03, 0xfc,
                                            0x9f, 0x5e,
                                        ];
                                        if !queue(disconnect) {
                                            warn!(
                                                "{INNER_NAME}: could not queue the multiplex channel close"
                                            );
                                        } else {
                                            info!(
                                                "{INNER_NAME}: queued multiplex channel close for \
                                                 [fc, 9f, 5e]"
                                            );
                                        }

                                        // Then wait for it to go out, but not for it to
                                        // be *confirmed*.
                                        //
                                        // Waiting for confirmation stalls forever on the
                                        // LAN path: once the peer has the new socket it
                                        // stops servicing BLE, so that confirmation may
                                        // never come. So the sender stops waiting on
                                        // confirmations for this session and hands the
                                        // rest of its queue to the stack, counting each
                                        // packet off as it goes; `drained` then means
                                        // "out of our hands". A dropped link ends the
                                        // wait too: nothing more goes out there.
                                        pump_flush.store(generation, std::sync::atomic::Ordering::Relaxed);
                                        if pump_wake.send(()).is_err() {
                                            debug!("{INNER_NAME}: the advertiser has stopped");
                                        }
                                        tokio::select! {
                                            _ = pump_outstanding.drained() => {}
                                            _ = link_down.cancelled() => {}
                                            _ = pump_ctk.cancelled() => break,
                                        }
                                        info!(
                                            "{INNER_NAME}: prior channel released ({} packet(s) still \
                                             queued)",
                                            pump_outstanding.count()
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

                                        // There used to be a 45s limit on the phone's
                                        // first byte here, from when it finished the
                                        // upgrade handshake and then sent nothing. That
                                        // was the multiplex channel close above going
                                        // missing. A phone that gives up now closes the
                                        // socket, which ends the copy like any other
                                        // ending, and the byte counts below show whether
                                        // it ever sent anything.
                                        let copied = tokio::select! {
                                            r = tokio::io::copy_bidirectional(&mut sock, &mut ours) => r,
                                            _ = pump_ctk.cancelled() => break,
                                        };
                                        match copied {
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
                            _ = link_down.cancelled() => {
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

        let mut visibility = self.visibility.clone();
        let subscribers_wake = wake_tx.clone();
        let status_wake = wake_tx.clone();
        let completion_wake = wake_tx;
        tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let runtime = tokio::runtime::Handle::current();

            // With Bluetooth off, CreateAsync fails (RadioNotAvailable), and
            // this task used to end there: a PC started before Bluetooth was
            // turned on stayed undiscoverable over BLE until the app restarted.
            // Wait for the radio first; once advertising, the loop below
            // handles it going off and on again.
            if !crate::hdl::bluetooth_is_on() {
                info!("{INNER_NAME}: Bluetooth is off; advertising once it is turned on");
            }
            let on = runtime.block_on(async {
                tokio::select! {
                    _ = crate::hdl::bluetooth_on() => true,
                    _ = ctk.cancelled() => false,
                }
            });
            if !on {
                return Ok(());
            }

            // Create the GATT service provider for 0xFEF3.
            let create_result =
                GattServiceProvider::CreateAsync(COPRESENCE_SERVICE_GUID)?.join()?;
            let error = create_result.Error()?;
            if error.0 != 0 {
                return Err(anyhow::anyhow!(
                    "GattServiceProvider::CreateAsync failed: BluetoothError {}",
                    error.0
                ));
            }
            let provider = create_result.ServiceProvider()?;
            // The loop below reads the status itself; this only tells it when
            // to look.
            provider.AdvertisementStatusChanged(&TypedEventHandler::<
                GattServiceProvider,
                GattServiceProviderAdvertisementStatusChangedEventArgs,
            >::new(move |_, _| {
                if status_wake.send(()).is_err() {
                    debug!("{INNER_NAME}: the advertiser has stopped");
                }
                Ok(())
            }))?;

            // Serve the full advertisement from the slot-0 characteristic. The
            // phone connects after seeing our advert and reads this; with no
            // characteristic it finds an empty service and gives up, which is
            // why we were never listed.
            let char_params = GattLocalCharacteristicParameters::new()?;
            char_params.SetCharacteristicProperties(GattCharacteristicProperties::Read)?;
            let char_result = provider
                .Service()?
                .CreateCharacteristicAsync(ADV_SLOT0_CHARACTERISTIC_GUID, &char_params)?
                .join()?;
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
                let request = args.GetRequestAsync()?.join()?;
                let writer = DataWriter::new()?;
                writer.WriteBytes(&adv_for_read)?;
                request.RespondWithValue(&writer.DetachBuffer()?)?;
                deferral.Complete()?;
                info!("*** {INNER_NAME}: served advertisement over GATT read ***");
                Ok(())
            }))?;

            // Connectable: a GATT service must be connectable to advertise at
            // all (non-connectable -> status 3 Aborted). The service data is the
            // 17-byte advertisement header, which tells the phone's scan to read
            // the full advertisement over GATT (isPrivateGatt / rxAdvertisement).
            // The 26-byte fast advertisement that used to go here was dropped by
            // Windows, and with only the bare UUID on air the phone almost never
            // read us. The header does reach the phone - its log shows "Received
            // advertisement header ... numSlots=1" and a read within seconds -
            // although Windows still reports status 4
            // (StartedWithoutAllAdvertisementData) for whatever else it leaves out.
            let params = GattServiceProviderAdvertisingParameters::new()?;
            params.SetIsConnectable(true)?;
            params.SetIsDiscoverable(false)?;

            let writer = DataWriter::new()?;
            writer.WriteBytes(&header)?;
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
                .join()?;
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
                    info!("{INNER_NAME}: server-tx subscribers = {n}");
                    // The session's own device leaving is what ends it, not the
                    // count reaching zero: with a second device subscribed, the
                    // count never would.
                    let owner = subscribers_active
                        .lock()
                        .ok()
                        .and_then(|a| a.device().map(str::to_string));
                    if owner.is_some_and(|d| subscribed_client(s, &d).is_none()) {
                        warn!("{INNER_NAME}: the session's phone unsubscribed - BLE link dropped");
                        // Unblock the current session so the transfer fails
                        // visibly instead of hanging.
                        if let Ok(slot) = handler_down.lock() {
                            if let Some(link_down) = slot.as_ref() {
                                link_down.cancel();
                            }
                        }
                    }
                }
                // Hidden pauses the advertisement only with no phone
                // subscribed, and a recycle waits for the phone to go.
                if subscribers_wake.send(()).is_err() {
                    debug!("{INNER_NAME}: the advertiser has stopped");
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
                .join()?;
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
                let request = args.GetRequestAsync()?.join()?;
                let reader = DataReader::FromBuffer(&request.Value()?)?;
                let len = reader.UnconsumedBufferLength()? as usize;
                let mut buf = vec![0u8; len];
                reader.ReadBytes(&mut buf)?;
                if request.Option()? == GattWriteOption::WriteWithResponse {
                    request.Respond()?;
                }
                deferral.Complete()?;

                // Whose write this is. A ConnectionRequest may start a session;
                // anything else must come from the device whose session it is.
                // See `ActivePeer`.
                let device = session_device(args.Session()).unwrap_or_default();
                let mut started: Option<u64> = None;
                if let Some(&head) = buf.first() {
                    let is_request = head & 0x80 != 0 && head & 0x0f == 0;
                    let allowed = match handler_active.lock() {
                        Ok(mut active) if is_request => {
                            let owner_subscribed = active
                                .device()
                                .is_some_and(|d| subscribed_client(&confirm_tx, d).is_some());
                            started = active.start(&device, owner_subscribed);
                            started.is_some()
                        }
                        Ok(active) => active.owns(&device),
                        Err(_) => false,
                    };
                    if !allowed {
                        if is_request {
                            info!(
                                "{INNER_NAME}: ignoring a ConnectionRequest from {device}: another \
                                 device's session is active"
                            );
                        } else {
                            trace!("{INNER_NAME}: ignoring a write from {device}, not this session's");
                        }
                        return Ok(());
                    }
                }

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
                            if st.acc.len() > super::ble_receiver::MAX_WEAVE_MESSAGE {
                                warn!(
                                    "{INNER_NAME}: REASSEMBLY - message passed {} B without ending; dropping it",
                                    super::ble_receiver::MAX_WEAVE_MESSAGE
                                );
                                st.acc.clear();
                                continue;
                            }
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
                    if let Some(generation) = started {
                        if new_session_tx.send(generation).is_err() {
                            warn!("{INNER_NAME}: the session supervisor is gone");
                        }
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
                    confirm_size.store(packet_size, std::sync::atomic::Ordering::Relaxed);

                    // A ConnectionRequest starts a fresh Weave connection, so
                    // reset both counters here. This handler and its state live
                    // for the advertiser's whole lifetime, not per connection -
                    // so the second transfer of a session inherited the first's
                    // counters. Our ConnectionConfirm then went out on a stale
                    // outbound counter and the phone answered with a Weave Error
                    // before any accept prompt (measured: its own log shows the
                    // Weave socket erroring during UKEY2, ending in EOFException),
                    // while our inbound reorder still expected the prior stream's
                    // next counter and would have stalled the phone's data. The
                    // phone's ConnectionRequest is its counter 0, so ours
                    // restarts at 0 and the next inbound packet we expect is 1.
                    confirm_counter.store(0, std::sync::atomic::Ordering::Relaxed);
                    if let Ok(mut st) = rx_buf.lock() {
                        st.acc.clear();
                        st.expected = 1;
                        st.pending = Default::default();
                    }

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
                    let link_max = subscribed_client(&confirm_tx, &device)
                        .and_then(|c| c.MaxNotificationSize().ok());
                    info!(
                        "*** {INNER_NAME}: sending Weave ConnectionConfirm (v{version}, \
                         {packet_size} B, link notifications up to {link_max:?} B, \
                         {subscribers} subscriber(s)) ***"
                    );
                    // Retries on non-Success delivery - see notify_confirm. Do
                    // NOT block on .join() here: this runs on a WinRT write-event
                    // handler thread, where waiting on the async op is illegal
                    // (0x8000000E) and can deadlock. The retry is driven entirely
                    // by the delivery-completion callback.
                    if let Some(generation) = started {
                        notify_confirm(
                            confirm_tx.clone(),
                            confirm,
                            device.clone(),
                            handler_active.clone(),
                            generation,
                        );
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
                header.len()
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
            //
            // Packets are cut to the size the ConnectionConfirm agreed, capped by
            // the largest notification the session's phone can take on this
            // link. Its count of outstanding packets replaces the message's.
            let frame_weave = |bytes: &[u8], out: &mut std::collections::VecDeque<Vec<u8>>| {
                let link_max = outbound_active
                    .lock()
                    .ok()
                    .and_then(|a| a.device().and_then(|d| subscribed_client(&server_tx, d)))
                    .and_then(|c| c.MaxNotificationSize().ok());
                let size = ble_receiver::send_packet_size(
                    agreed_size.load(std::sync::atomic::Ordering::Relaxed),
                    link_max,
                );
                let packets = ble_receiver::weave_packets(bytes, size, &tx_counter);
                outstanding.add(packets.len());
                outstanding.done(1);
                debug!(
                    "{INNER_NAME}: weave queued {} B as {} packet(s) of up to {size} B, next ctr={}",
                    bytes.len(),
                    packets.len(),
                    tx_counter.load(std::sync::atomic::Ordering::Relaxed) & 0x07
                );
                out.extend(packets);
            };

            let mut tx_queue: std::collections::VecDeque<Vec<u8>> =
                std::collections::VecDeque::new();
            // The id of the indication in flight, 0 for none. An id rather than
            // a flag: once a switch to the upgraded socket gives up on one, its
            // late completion must not clear the next one's slot and let two
            // out at once.
            let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let mut next_indication: u64 = 0;

            let mut sends = crate::hdl::ble_sends();
            // Set while a recycle waits for the lingering phone to go.
            let mut recycling: Option<tokio::time::Instant> = None;
            let mut last_status: i32 = -1;
            let mut adv_paused = false;
            loop {
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

                // Stop waiting on confirmations once the session has moved to
                // its upgraded socket.
                //
                // Once the phone joins the hotspot it quits confirming our
                // indications, so the gate would otherwise sit on the
                // outstanding one for the full 30s ATT timeout - and the
                // switch-critical multiplex channel-close, queued behind it,
                // would reach the phone far too late to matter (measured: the
                // transfer then died at "nothing on the upgraded socket within
                // 45s"). This used to be a 5s limit on every indication; the
                // switch is the event it stood in for. The rest of this
                // session's packets go straight to the stack, in order.
                let flush = flush_generation.load(std::sync::atomic::Ordering::Relaxed);
                let flushing = flush != 0
                    && outbound_active
                        .lock()
                        .is_ok_and(|a| a.generation() == Some(flush));
                if flushing {
                    let current = in_flight.load(std::sync::atomic::Ordering::Relaxed);
                    if current != 0
                        && in_flight
                            .compare_exchange(
                                current,
                                0,
                                std::sync::atomic::Ordering::Relaxed,
                                std::sync::atomic::Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        info!(
                            "{INNER_NAME}: switched to the upgraded socket; no longer waiting on \
                             the unconfirmed indication"
                        );
                        outstanding.done(1);
                    }
                }

                // At most one indication outstanding, except after the switch.
                // The completion handler clears the slot, so the next goes out
                // only once the phone has confirmed the previous one - or once
                // the switch gives up on it. Whichever of the two clears it
                // counts the packet off.
                loop {
                    if !flushing && in_flight.load(std::sync::atomic::Ordering::Relaxed) != 0 {
                        break;
                    }
                    let Some(pkt) = tx_queue.pop_front() else {
                        break;
                    };
                    let gated = !flushing;
                    {
                        let ctr = (pkt[0] >> 4) & 0x07;
                        let queued = tx_queue.len();
                        next_indication += 1;
                        let id = next_indication;
                        let send = || -> Result<(), anyhow::Error> {
                            let w = DataWriter::new()?;
                            w.WriteBytes(&pkt)?;
                            let device = outbound_active
                                .lock()
                                .ok()
                                .and_then(|a| a.device().map(str::to_string));
                            // Taken before sending: the completion can run
                            // before the call returns.
                            if gated {
                                in_flight.store(id, std::sync::atomic::Ordering::Relaxed);
                            }
                            let slot = in_flight.clone();
                            let confirmed = outstanding.clone();
                            let wake = completion_wake.clone();
                            let sent_at = std::time::Instant::now();
                            let sent = notify_device(
                                &server_tx,
                                &w.DetachBuffer()?,
                                device.as_deref(),
                                move |statuses| {
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
                                    // Only if this indication still holds the
                                    // slot; a late one leaves the next alone.
                                    if gated
                                        && slot
                                            .compare_exchange(
                                                id,
                                                0,
                                                std::sync::atomic::Ordering::Relaxed,
                                                std::sync::atomic::Ordering::Relaxed,
                                            )
                                            .is_ok()
                                    {
                                        confirmed.done(1);
                                    }
                                    // The next packet can go now.
                                    if wake.send(()).is_err() {
                                        debug!("{INNER_NAME}: the advertiser has stopped");
                                    }
                                },
                            );
                            sent?;
                            Ok(())
                        };
                        match send() {
                            // After the switch a packet is done with once the
                            // stack has it.
                            Ok(()) => {
                                if !gated {
                                    outstanding.done(1);
                                }
                            }
                            // It never went out, so no completion will count
                            // it off.
                            Err(e) => {
                                warn!("{INNER_NAME}: weave send failed: {e}");
                                if gated {
                                    in_flight.store(0, std::sync::atomic::Ordering::Relaxed);
                                }
                                outstanding.done(1);
                            }
                        }
                    }
                }
                // Stop advertising while we are sending over BLE, and resume
                // when the send ends.
                //
                // The counterpart to pausing the scan, and for the same reason:
                // discovery and a transfer contend for one radio. Advertising
                // costs more than throughput though - it costs the handshake.
                // A phone that sees our advertisement tries to fetch the full
                // version over GATT, and its own log says what that does to a
                // connection it is already serving:
                //
                //   Received advertisement header BleAdvertisementHeader { .. }
                //   Try to fetch advertisement from a possible IOS target.
                //   Not allow Gatt actions during scanning.
                //   BleGattServerProviderV1: Passing wrong device!
                //   Receive completion for unexpected operation: SEND_NOTIFICATION
                //   BleSocketOutputStream failed to write data / status 133
                //
                // The phone's GATT server got a notification completion it could
                // not attribute, gave up on the Weave socket and tore it down -
                // measured mid-UKEY2, so the transfer died in the handshake with
                // no prompt shown. We were advertising *at* the peer we were
                // mid-handshake with. Nothing needs to discover us while we are
                // busy; the session that matters is already established.
                //
                // Outbound BLE sends only - see `AdvertisePause`. Stopping the
                // advertisement drops connected GATT clients, so doing this on
                // an inbound transfer would tear down the session it is meant
                // to protect.
                //
                // Hidden pauses it too, so a hidden PC isn't discoverable over
                // Bluetooth while mDNS has already gone quiet - but only with no
                // phone subscribed, for the same reason: a receive in progress
                // keeps the advertisement, and it stops once that phone leaves.
                let hidden =
                    *visibility.borrow_and_update() == crate::hdl::Visibility::Invisible;
                let subscribed = server_tx
                    .SubscribedClients()
                    .and_then(|c| c.Size())
                    .unwrap_or(0)
                    > 0;
                let sending = *sends.borrow_and_update() > 0;
                let want_paused = sending || (hidden && !subscribed);
                if want_paused != adv_paused {
                    if want_paused {
                        if let Err(e) = provider.StopAdvertising() {
                            warn!("{INNER_NAME}: could not pause the advertisement: {e}");
                        }
                        if sending {
                            info!("{INNER_NAME}: BLE send in progress, pausing the advertisement");
                        } else {
                            info!("{INNER_NAME}: hidden, pausing the advertisement");
                        }
                    } else {
                        match provider.StartAdvertisingWithParameters(&params) {
                            Ok(()) => {
                                info!("{INNER_NAME}: resuming the advertisement")
                            }
                            Err(e) => {
                                warn!("{INNER_NAME}: could not resume advertising: {e}")
                            }
                        }
                    }
                    adv_paused = want_paused;
                    last_status = -1;
                    // A pause takes over from a recycle in progress, and the
                    // resume restarts the advertisement.
                    recycling = None;
                }

                // A session ended: stop and restart advertising, which drops the
                // peer's GATT connection. Without this the phone stays connected
                // after a transfer and the *other* direction cannot get the link.
                //
                // Skipped while paused - there is nothing to recycle, and
                // restarting here would undo the pause above.
                // Also skipped once a new session has started: the phone that
                // owns it is on this link, and recycling would drop it.
                let session_open = outbound_active
                    .lock()
                    .map(|a| a.device().is_some())
                    .unwrap_or(false);
                if recycle_adv.swap(false, std::sync::atomic::Ordering::Relaxed)
                    && !adv_paused
                    && !session_open
                {
                    // Recycle only if the phone is *still* on the GATT link.
                    //
                    // The recycle exists for one job: drop a phone that stayed
                    // connected after a transfer, so the next transfer - in
                    // either direction - starts from a clean link. But
                    // StopAdvertising is a blunt instrument: it also removes us
                    // from the phone's discovery, and the phone logs
                    // onEndpointLost and stops listing us in the share sheet.
                    //
                    // When the phone closed the session itself - the normal case,
                    // and what the log shows ("BLE session closed normally",
                    // subscribers 0) - there is nothing to drop. Recycling anyway
                    // dropped nothing and only made us disappear from the chooser
                    // for the next transfer: issue #17. So look first, and leave
                    // the advertisement untouched when no subscriber lingers.
                    let subs = server_tx
                        .SubscribedClients()
                        .and_then(|c| c.Size())
                        .unwrap_or(0);
                    if subs == 0 {
                        info!(
                            "{INNER_NAME}: session ended with no lingering subscriber; \
                             keeping the advertisement up so the phone keeps listing us"
                        );
                    } else {
                        if let Err(e) = provider.StopAdvertising() {
                            warn!("{INNER_NAME}: could not stop advertising to recycle: {e}");
                        }
                        // Advertise again once the phone is off the link. That
                        // used to be a flat 500 ms sleep on this thread; the
                        // phone leaving shows up as the subscribers changing.
                        // The 2 s limit stands in for that event, should Windows
                        // ever keep a client through StopAdvertising: staying
                        // off the air would hide us from every phone.
                        recycling =
                            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
                    }
                }
                if let Some(deadline) = recycling {
                    let subs = server_tx
                        .SubscribedClients()
                        .and_then(|c| c.Size())
                        .unwrap_or(0);
                    let stopped = provider
                        .AdvertisementStatus()
                        .is_ok_and(|s| s == GattServiceProviderAdvertisementStatus::Stopped);
                    if subs == 0 || stopped || tokio::time::Instant::now() >= deadline {
                        recycling = None;
                        match provider.StartAdvertisingWithParameters(&params) {
                            // Did it actually drop? If subscribers is still 1
                            // the recycle did not do what it is here to do, and
                            // the next transfer in either direction will fight
                            // for the link.
                            Ok(()) => info!(
                                "{INNER_NAME}: recycled to drop a lingering subscriber; \
                                 subscribers now {subs}"
                            ),
                            Err(e) => warn!("{INNER_NAME}: could not restart advertising: {e}"),
                        }
                        last_status = -1;
                    }
                }

                if let Ok(status) = provider.AdvertisementStatus() {
                    if status.0 != last_status {
                        last_status = status.0;
                        info!(
                            "{INNER_NAME}: advertisement status = {} ({})",
                            last_status,
                            adv_status_name(last_status)
                        );

                        // Turning Bluetooth off aborts the advertisement, and it
                        // used to stay aborted until the app restarted: the PC
                        // stopped being discoverable over BLE for good. Aborted
                        // also shows briefly at every start, so act only when
                        // the radio really is off: wait for it to come back on,
                        // then advertise again.
                        if status == GattServiceProviderAdvertisementStatus::Aborted
                            && !adv_paused
                            && !crate::hdl::bluetooth_is_on()
                        {
                            info!(
                                "{INNER_NAME}: Bluetooth turned off; advertising again once it is back on"
                            );
                            let back = runtime.block_on(async {
                                tokio::select! {
                                    _ = crate::hdl::bluetooth_turned_on() => true,
                                    _ = ctk.cancelled() => false,
                                }
                            });
                            if !back {
                                break;
                            }
                            // Windows usually resumes a GATT service's advertisement
                            // by itself once the radio is back - measured: status 4
                            // a second after Bluetooth came on - and then refuses a
                            // second start with E_ILLEGAL_METHOD_CALL. Starting it
                            // here covers the times it does not.
                            match provider.StartAdvertisingWithParameters(&params) {
                                Ok(()) => info!("{INNER_NAME}: Bluetooth back on, advertising again"),
                                Err(e)
                                    if e.code()
                                        == windows::Win32::Foundation::E_ILLEGAL_METHOD_CALL =>
                                {
                                    info!(
                                        "{INNER_NAME}: Bluetooth back on; Windows resumed the advertisement"
                                    )
                                }
                                Err(e) => warn!("{INNER_NAME}: could not advertise again: {e}"),
                            }
                            last_status = -1;
                        }
                    }
                }

                // Sleep until something changes. This used to wake every 50 ms,
                // which also capped outbound at about 20 indications a second;
                // now the next packet goes the moment the last is confirmed.
                let deadline = recycling.unwrap_or_else(tokio::time::Instant::now);
                let next = runtime.block_on(async {
                    tokio::select! {
                        _ = ctk.cancelled() => None,
                        Some(msg) = outbound_rx.recv() => Some(Some(msg)),
                        Some(()) = wake_rx.recv() => Some(None),
                        Ok(()) = sends.changed() => Some(None),
                        Ok(()) = visibility.changed() => Some(None),
                        _ = tokio::time::sleep_until(deadline), if recycling.is_some() => Some(None),
                    }
                });
                match next {
                    None => break,
                    Some(Some(msg)) => frame_weave(&msg, &mut tx_queue),
                    Some(None) => {}
                }
            }

            info!("{INNER_NAME}: tracker cancelled, stopping advertiser");
            if let Err(e) = provider.StopAdvertising() {
                warn!("{INNER_NAME}: could not stop advertising: {e}");
            }
            Ok(())
        })
        .await??;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the receiver on the machine's real Bluetooth adapter and flips
    /// visibility; read the log for "hidden, pausing" and "resuming". Run by hand:
    /// `cargo test -p rqs_lib hidden_pauses_the_advertisement -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a Bluetooth adapter; check the log"]
    async fn hidden_pauses_the_advertisement() {
        use crate::hdl::Visibility;

        let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
        let (visibility, watcher) = tokio::sync::watch::channel(Visibility::Visible);
        let (sender, _) = tokio::sync::broadcast::channel(8);
        let receiver =
            BleReceiverAdvertiser::new([1, 2, 3, 4], 3, "rqs-test".to_string(), sender, watcher);
        let ctk = CancellationToken::new();
        let run = tokio::spawn({
            let ctk = ctk.clone();
            async move { receiver.run(ctk).await }
        });

        // The receiver acts on each change as it happens; the gaps only keep
        // the steps apart in the log, and give Windows time to report status.
        let settle = std::time::Duration::from_secs(2);
        tokio::time::sleep(settle).await;
        if visibility.send(Visibility::Invisible).is_err() {
            panic!("the receiver stopped early: {:?}", run.await);
        }
        tokio::time::sleep(settle).await;
        visibility.send(Visibility::Visible).unwrap();
        tokio::time::sleep(settle).await;

        ctk.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("the receiver did not stop on cancel")
            .unwrap()
            .unwrap();
    }
}
