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
        let (inbound_tx, mut inbound_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (outbound_tx, mut outbound_rx) =
            tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        let (mut ours, theirs) = tokio::io::duplex(256 * 1024);
        let msg_sender = self.sender.clone();
        tokio::spawn(async move {
            let mut request =
                super::InboundRequest::new(theirs, "ble".to_string(), msg_sender);

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
                            _ => warn!("{INNER_NAME}: BLE InboundRequest ended: {e}"),
                        }
                        break;
                    }
                }
            }
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
                                info!(
                                    "{INNER_NAME}: BLE rx {:.1} KB/s ({rx_frames} frames, \
                                     {:.0} KB total)",
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
                    else => break,
                }
            }
            info!("{INNER_NAME}: bridge pump exited");
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
                                if let Err(e) = inbound_tx.send(msg) {
                                    warn!(
                                        "{INNER_NAME}: bridge is gone, dropping {} B Weave message",
                                        e.0.len()
                                    );
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
                Ok(())
            }))?;

            provider.StartAdvertisingWithParameters(&params)?;
            info!(
                "{INNER_NAME}: GATT service advertising started ({} B service data) under 0xFEF3",
                advertisement.len()
            );

            // Outbound Weave: take whole [hash][len][frame] messages from the
            // bridge, split into (packet_size - 1) chunks and notify each with
            // header (counter<<4) | (first<<3) | (last<<2).
            let send_weave = |bytes: &[u8]| -> Result<(), anyhow::Error> {
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

                    let w = DataWriter::new()?;
                    w.WriteBytes(&pkt)?;
                    let _ = server_tx.NotifyValueAsync(&w.DetachBuffer()?);
                }
                info!(
                    "{INNER_NAME}: weave sent {} B in {n} packet(s), next ctr={}",
                    bytes.len(),
                    tx_counter.load(std::sync::atomic::Ordering::Relaxed) & 0x07
                );
                Ok(())
            };

            let mut last_status: i32 = -1;
            while !ctk.is_cancelled() {
                while let Ok(msg) = outbound_rx.try_recv() {
                    if let Err(e) = send_weave(&msg) {
                        warn!("{INNER_NAME}: weave send failed: {e}");
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
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }

            info!("{INNER_NAME}: tracker cancelled, stopping advertiser");
            let _ = provider.StopAdvertising();
            Ok(())
        })
        .await??;

        Ok(())
    }
}
