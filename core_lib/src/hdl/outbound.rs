use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use p256::ecdh::diffie_hellman;
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{PublicKey, Sec1Point};
use prost::Message;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast::{Receiver, Sender};
use ts_rs::TS;

use super::info::{InternalFileInfo, TransferMetadata};
use super::{InnerState, State};
use crate::channel::{ChannelAction, ChannelDirection, ChannelMessage};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
use crate::location_nearby_connections::connection_response_frame::ResponseStatus;
use crate::location_nearby_connections::payload_transfer_frame::{
    payload_header, PacketType, PayloadChunk, PayloadHeader,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::securegcm::ukey2_alert::AlertType;
use crate::securegcm::ukey2_client_init::CipherCommitment;
use crate::securegcm::{
    ukey2_message, DeviceToDeviceMessage, GcmMetadata, Type, Ukey2Alert, Ukey2ClientFinished,
    Ukey2ClientInit, Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit,
};
use crate::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType,
    SecureMessage, SigScheme,
};
use crate::sharing_nearby::{
    file_metadata, paired_key_result_frame, text_metadata, FileMetadata, IntroductionFrame,
    TextMetadata,
};
use crate::utils::{
    decode_point, derive_d2d_keys, encode_point, gen_ecdsa_keypair, gen_random,
    to_four_digit_string, D2DKeys, DeviceType, RemoteDeviceInfo,
};
use crate::{location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
/// How long to wait, once everything is sent, for the receiver to hang up.
///
/// The receiver ends the session itself once it has written every payload.
/// Hanging up first races that: our DISCONNECTION lands while the peer is still
/// finishing the last file, and Windows Quick Share then reports "Can't complete
/// transfer" for a transfer that arrived intact. We only send it ourselves if
/// the peer has not closed within this long.
///
/// This bound stands in for the confirmation a receiver that lost the data
/// never sends: the hang-up is the event, and this is how a send it never
/// comes for ends as Disconnected instead of staying open and blocking every
/// later send to that peer. Phones hang up within a second of the last byte.
const PEER_HANG_UP_GRACE: Duration = Duration::from_secs(15);

/// Whether to pursue the send-side WiFi upgrade: joining the hotspot the phone
/// brings up when we are sending to it and its WiFi is off.
///
/// On by default - it is the whole point of the feature, and BLE at ~20 KB/s is
/// not a transport anyone wants for a real file. `RQS_NO_SEND_WIFI_UPGRADE`
/// forces the BLE-only path, which is worth having while the WiFi-Direct
/// association timing is still being characterised: it is the difference
/// between a slow transfer and no transfer if a phone behaves unexpectedly.
///
/// This gates the *announcement* as well as the join. If WIFI_HOTSPOT is never
/// in our supported mediums the phone never offers a path, so switching this off
/// disables the whole upgrade path rather than half of it.
/// False where the upgrade is not implemented, so the medium list stays honest:
/// only a build that can actually join a hotspot announces that it can.
fn send_wifi_upgrade_enabled() -> bool {
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    {
        std::env::var_os("RQS_NO_SEND_WIFI_UPGRADE").is_none()
    }
    #[cfg(not(all(feature = "experimental", target_os = "windows")))]
    {
        false
    }
}

/// Everything the bandwidth upgrade does on the new socket once it is connected,
/// shared by both media that carry it: send CLIENT_INTRODUCTION, read and verify
/// CLIENT_INTRODUCTION_ACK, then hand the socket to the transport. The bytes are
/// medium-agnostic - the phone reads the same frames whether the socket reached
/// it over WIFI_HOTSPOT or WIFI_LAN - so there is exactly one copy.
///
/// Returns `Ok` only when the peer acknowledged the introduction and the socket
/// was accepted by the transport. Any earlier failure returns `Err`, leaving
/// `join_ready` false so the caller stays on BLE.
#[cfg(all(feature = "experimental", target_os = "windows"))]
async fn introduce_over_upgraded_socket(
    mut socket: tokio::net::TcpStream,
    endpoint_id: String,
    unencrypted: bool,
    upgrade_tx: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
) -> Result<(), anyhow::Error> {
    use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
        ClientIntroduction, EventType,
    };
    use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let intro = location_nearby_connections::OfflineFrame {
        version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
        v1: Some(location_nearby_connections::V1Frame {
            r#type: Some(
                location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                    .into(),
            ),
            bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                event_type: Some(EventType::ClientIntroduction.into()),
                client_introduction: Some(ClientIntroduction {
                    endpoint_id: Some(endpoint_id),
                    supports_disabling_encryption: Some(unencrypted),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
    };
    let data = intro.encode_to_vec();
    socket.write_all(&(data.len() as u32).to_be_bytes()).await?;
    socket.write_all(&data).await?;
    socket.flush().await?;
    info!("Bandwidth upgrade: sent CLIENT_INTRODUCTION");

    // No timeout: this runs in the upgrade task, which ends when the transfer
    // does, and a peer that closes the socket ends the read. The transfer
    // carries on over BLE meanwhile.
    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > SANE_FRAME_LENGTH as usize {
        return Err(anyhow!("insane ack frame length {len}"));
    }
    let mut frame = vec![0u8; len];
    socket.read_exact(&mut frame).await?;

    // Check what came back, rather than assuming.
    //
    // This used to log "got CLIENT_INTRODUCTION_ACK" on the strength of having
    // read *some* frame, which made the log worse than useless: it asserted the
    // peer had accepted the new medium when nothing had been decoded. The peer's
    // own record of the same transfer said the opposite -
    // "physicalConnectionClosed() for medium WIFI_HOTSPOT with no corresponding
    // EstablishedConnection that was previously opened", active medium still BLE,
    // 98304 bytes received and not one of them over WiFi.
    //
    // An upgrade that is not acknowledged must fail here. Failing leaves
    // `join_ready` false, so the switch never fires and the transfer finishes
    // over BLE - slow, but a slow transfer is a transfer.
    let ack = location_nearby_connections::OfflineFrame::decode(&*frame)
        .map_err(|e| anyhow!("peer's reply on the upgraded socket was not a frame: {e}"))?;
    let event = ack
        .v1
        .as_ref()
        .and_then(|v| v.bandwidth_upgrade_negotiation.as_ref())
        .map(|bun| bun.event_type());
    if event
        != Some(
            location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::ClientIntroductionAck,
        )
    {
        return Err(anyhow!(
            "peer did not acknowledge the introduction; it replied with {event:?}"
        ));
    }
    info!("Bandwidth upgrade: got CLIENT_INTRODUCTION_ACK");

    if upgrade_tx.send(socket).is_err() {
        return Err(anyhow!("transport gone, cannot adopt the upgraded socket"));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub enum OutboundPayload {
    Files(Vec<String>),
    Text(String),
}

/// Generic over the transport so the handshake can be driven by a test over an
/// in-memory duplex pair, not just a real socket. In production `S` is always
/// `tokio::net::TcpStream`.
#[derive(Debug)]
pub struct OutboundRequest<S> {
    endpoint_id: [u8; 4],
    /// Write half only; reading happens in a task. See `frame_reader`.
    socket: tokio::io::WriteHalf<S>,
    /// Frames off the wire, still encrypted.
    ///
    /// Decoupling reads from writes is what lets a long send answer keepalives:
    /// the file loop can drain this between chunks instead of going silent for
    /// the whole transfer.
    frames: tokio::sync::mpsc::Receiver<crate::hdl::RawFrame>,
    pub state: InnerState,
    sender: Sender<ChannelMessage>,
    receiver: Receiver<ChannelMessage>,
    payload: OutboundPayload,
    /// Set when the peer scanned our QR code: signing the handshake with it
    /// lets the peer accept without asking. See `qr::handshake_signature`.
    qr_key: Option<SigningKey>,
    /// The UKEY2 auth key (the one the PIN comes from), which `qr_key` signs.
    auth_key: Option<Vec<u8>>,
    /// Where the upgrade listener delivers the socket the peer connected on,
    /// and the signal that the old channel has been released. Set by the BLE
    /// send path, which is the only transport that can adopt an upgraded socket
    /// and the only one slow enough to need to.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgrade_tx: Option<tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>>,
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    switch_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Fired by the transport once the upgraded socket is spliced in. Payload
    /// must not be written between requesting the switch and this.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    switched: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// Set once the stream has moved onto an upgraded (WiFi) socket, so the
    /// send loop stops waiting for one and proceeds.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgraded: bool,
    /// Whether the peer's upgrade offer allows running the upgraded channel
    /// without encryption (`supports_disabling_encryption`).
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgrade_unencrypted: bool,
    /// Frames go out and come in as plain `OfflineFrame`s, without the
    /// UKEY2 encryption around them.
    ///
    /// Set when the stream moves onto an upgraded channel whose offer allowed
    /// it. The phone decides that on its own: a Pixel offering
    /// `supports_disabling_encryption: true` runs the new channel as plain
    /// WIFI_HOTSPOT even when our CLIENT_INTRODUCTION says false ("ignoring wire
    /// CLIENT_INTRODUCTION.supportsDisablingEncryption=false ... using local
    /// intent true"). Encrypted frames sent there were each discarded as an
    /// "invalid OfflineFrame", so the file stopped at the bytes BLE had carried
    /// while we reported the transfer finished. Google's own client turns
    /// encryption off in the same case.
    unencrypted: bool,
    /// Set by the background join task once it has joined the peer's AP,
    /// connected, introduced itself and handed the socket to the transport - the
    /// point at which LAST_WRITE_TO_PRIOR_CHANNEL should go out. An *event*, not
    /// a deadline: it fires whenever the (variable-length) WiFi association
    /// finishes, and the send loop keeps streaming over BLE until it does.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    join_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether an upgrade attempt is already running, so a repeated offer (or a
    /// per-file nudge) doesn't start a second one. Covers both upgrade kinds:
    /// joining the peer's AP (WIFI_HOTSPOT) and a direct LAN connect
    /// (WIFI_LAN).
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    join_started: bool,
    /// The peer's AP, once the WIFI_HOTSPOT join has actually associated to it;
    /// written by the join task. Cleanup disconnects exactly this. It used to be
    /// a flag set when the join *started*, so a transfer that never joined
    /// anything still disconnected the PC's WiFi. The WIFI_LAN path never sets
    /// it: it opens a second TCP socket over the existing route.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    joined_ap: std::sync::Arc<std::sync::Mutex<Option<crate::hdl::JoinedAp>>>,
    /// Stops the upgrade task when the transfer ends, including the join's own
    /// thread, which aborting the task never reached.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    join_cancel: tokio_util::sync::CancellationToken,
    /// The background upgrade task, so cleanup can wait for it to stop - it has
    /// to have recorded what it joined before cleanup can undo it.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    join_handle: Option<tokio::task::JoinHandle<()>>,
    /// How much file data goes into one payload chunk.
    ///
    /// This sets how often the send loop comes up for air, because nothing else
    /// happens while a chunk is being written. At 512 KB over BLE that is ~25s
    /// at 20 KB/s - so keepalives went unanswered until the peer closed at its
    /// 30s timeout, and the cancel button did nothing for the same 25s. Over TCP
    /// a chunk is milliseconds and none of it matters.
    chunk_size: usize,
    /// Set once everything is sent: the point at which we stop waiting for the
    /// peer to hang up and do it ourselves. See `PEER_HANG_UP_GRACE`.
    hang_up_deadline: Option<tokio::time::Instant>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> OutboundRequest<S> {
    pub fn new(
        endpoint_id: [u8; 4],
        socket: S,
        id: String,
        sender: Sender<ChannelMessage>,
        payload: OutboundPayload,
        rdi: RemoteDeviceInfo,
    ) -> Self {
        let receiver = sender.subscribe();
        let files = match &payload {
            OutboundPayload::Files(files) => Some(files.to_owned()),
            OutboundPayload::Text(_) => None,
        };

        let (reader, writer) = tokio::io::split(socket);
        let frames = crate::hdl::spawn_frame_reader(reader, SANE_FRAME_LENGTH as usize);

        Self {
            endpoint_id,
            socket: writer,
            frames,
            state: InnerState {
                id,
                server_seq: 0,
                client_seq: 0,
                state: State::Initial,
                encryption_done: false,
                transfer_metadata: Some(TransferMetadata {
                    id: String::from(""),
                    source: Some(rdi),
                    files,
                    ..Default::default()
                }),
                ..Default::default()
            },
            sender,
            receiver,
            payload,
            qr_key: None,
            auth_key: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgrade_tx: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            switch_tx: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            switched: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgraded: false,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgrade_unencrypted: false,
            unencrypted: false,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            join_ready: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            joined_ap: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            join_cancel: tokio_util::sync::CancellationToken::new(),
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            join_started: false,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            join_handle: None,
            chunk_size: 512 * 1024,
            hang_up_deadline: None,
        }
    }

    /// Let this request upgrade off a slow transport. Only the BLE send path
    /// can adopt an upgraded socket.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    pub fn set_upgrade_sinks(
        &mut self,
        upgrade_tx: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
        switch_tx: tokio::sync::mpsc::UnboundedSender<()>,
        switched: std::sync::Arc<tokio::sync::Notify>,
    ) {
        self.upgrade_tx = Some(upgrade_tx);
        self.switch_tx = Some(switch_tx);
        self.switched = Some(switched);
    }

    /// Use smaller payload chunks, for a transport where writing one takes long
    /// enough to matter. See `chunk_size`.
    pub fn set_chunk_size(&mut self, chunk_size: usize) {
        info!("Using {chunk_size} B payload chunks");
        self.chunk_size = chunk_size;
    }

    /// The peer scanned our QR code; sign the handshake so it accepts without
    /// asking.
    pub fn set_qr_key(&mut self, key: SigningKey) {
        info!("Peer scanned our QR code: asking it to skip its accept prompt");
        self.qr_key = Some(key);
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        let r = self.handle_next().await;
        // Every error ends the session, a completed send included (the peer's
        // hang-up comes back as one); only now is it safe to drop the WiFi link
        // it may be running over. A send the peer never confirmed ends as
        // Disconnected and must not leave us on its hotspot either.
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        if r.is_err() {
            self.cleanup_upgrade().await;
        }
        r
    }

    async fn handle_next(&mut self) -> Result<(), anyhow::Error> {
        let hang_up_deadline = self.hang_up_deadline;
        tokio::select! {
            _ = tokio::time::sleep_until(hang_up_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if hang_up_deadline.is_some() =>
            {
                // Everything went out, but the receiver never confirmed it, and
                // it always does when it has the data. Measured: an upgraded
                // send whose frames the phone discarded ended exactly here,
                // with 98304 bytes on the phone and "Finished" on our screen.
                warn!("Peer did not hang up after the transfer; it did not confirm receipt");
                self.update_state(
                    |e| {
                        e.state = State::Disconnected;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            },
            i = self.receiver.recv() => {
                match i {
                    Ok(channel_msg) => {
                        if channel_msg.direction == ChannelDirection::LibToFront {
                            return Ok(());
                        }

                        if channel_msg.id != self.state.id {
                            return Ok(());
                        }

                        debug!("outbound: got: {:?}", channel_msg);
                        match channel_msg.action {
                            Some(ChannelAction::CancelTransfer) => {
                                self.update_state(
                                    |e| {
                                        e.state = State::Cancelled;
                                    },
                                    true,
                                ).await;
                                self.disconnection().await?;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            },
                            None => {
                                trace!("inbound: nothing to do")
                            },
                            _ => {}
                        }
                    }
                    Err(e) => {
                        error!("inbound: channel error: {}", e);
                    }
                }
            },
            // Both branches are channel receives, so both are cancel-safe.
            frame = self.frames.recv() => {
                match frame {
                    Some(frame_data) => self.handle_frame(frame_data).await?,
                    // Closing the connection once everything is sent is a
                    // hang-up too, just without the DISCONNECTION frame.
                    None if hang_up_deadline.is_some() => {
                        self.peer_disconnected().await;
                        return Err(anyhow!(crate::errors::AppError::NotAnError));
                    }
                    None => return Err(anyhow!(std::io::Error::from(
                        std::io::ErrorKind::UnexpectedEof
                    ))),
                }
            }
        }

        Ok(())
    }

    /// Whether `msg` is the user pressing Cancel on this transfer.
    fn is_cancel_for_us(&self, msg: &ChannelMessage) -> bool {
        msg.direction == ChannelDirection::FrontToLib
            && msg.id == self.state.id
            && msg.action == Some(ChannelAction::CancelTransfer)
    }

    /// Cancel at the user's request: report it, hang up and undo any upgrade.
    /// Always ends the transfer, quietly unless hanging up fails.
    async fn cancel_by_user(&mut self) -> Result<(), anyhow::Error> {
        info!("Cancelling the transfer at the user's request");
        self.update_state(
            |e| {
                e.state = State::Cancelled;
            },
            true,
        )
        .await;
        self.disconnection().await?;
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        self.cleanup_upgrade().await;
        Err(anyhow!(crate::errors::AppError::NotAnError))
    }

    /// Handle any frame already waiting, without blocking.
    ///
    /// Called between file chunks. The send loop writes continuously and never
    /// reads until the whole file is done, which over TCP is invisible but over
    /// BLE means ~95s of silence for a 1.9 MB file - the peer sees no KeepAlive
    /// response and closes at its 30s timeout, capping outbound at whatever
    /// fits in 30s. Draining here keeps the connection alive without stalling
    /// the send when there is nothing to read.
    async fn service_pending_frames(&mut self) -> Result<(), anyhow::Error> {
        // Honour a cancel from the UI first.
        //
        // `handle()` watches this channel, but a send never returns to
        // `handle()` until the whole file is done - so over a slow medium the
        // cancel button did nothing for minutes. Three presses were logged
        // against a transfer that was already wedged, with no effect.
        while let Ok(channel_msg) = self.receiver.try_recv() {
            if self.is_cancel_for_us(&channel_msg) {
                return self.cancel_by_user().await;
            }
        }

        loop {
            match self.frames.try_recv() {
                // Boxed to break a type-level cycle: this calls `handle_frame`,
                // which can reach `process_consent`, which contains the send
                // loop that calls back here. At runtime the frames arriving
                // mid-send are keepalives and acks rather than consent frames,
                // so it does not actually re-enter - but the compiler cannot
                // know that and needs the indirection to size the future.
                Ok(frame_data) => Box::pin(self.handle_frame(frame_data)).await?,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(anyhow!(std::io::Error::from(
                        std::io::ErrorKind::UnexpectedEof
                    )))
                }
            }
        }
    }

    /// Block until the peer sends something, then handle it.
    ///
    /// `service_pending_frames` drains what has already arrived and returns;
    /// this waits for what has not. Used while holding payload back for the
    /// medium switch, where there is a specific event we cannot proceed without
    /// and spinning on `try_recv` would burn a core waiting for it. Frames are
    /// fed by a background reader task, so awaiting here does not stop them
    /// arriving.
    ///
    /// Cancel is watched too: this wait can last as long as the peer takes to
    /// release the old channel, and the Cancel button used to do nothing for
    /// all of it.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn await_next_frame(&mut self) -> Result<(), anyhow::Error> {
        loop {
            // Both branches are channel receives, so both are cancel-safe.
            tokio::select! {
                frame = self.frames.recv() => {
                    return match frame {
                        Some(frame_data) => Box::pin(self.handle_frame(frame_data)).await,
                        None => Err(anyhow!(std::io::Error::from(
                            std::io::ErrorKind::UnexpectedEof
                        ))),
                    };
                }
                msg = self.receiver.recv() => match msg {
                    Ok(msg) if self.is_cancel_for_us(&msg) => return self.cancel_by_user().await,
                    // Anything else, including having fallen behind on the
                    // bus, leaves us waiting for the frame.
                    _ => {}
                },
            }
        }
    }

    async fn handle_frame(&mut self, frame_data: Vec<u8>) -> Result<(), anyhow::Error> {
        let current_state = &self.state;
        // Now determine what will be the request type based on current state
        match current_state.state {
            State::SentUkeyClientInit => {
                debug!("Handling State::SentUkeyClientInit frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.update_state(
                    |e| {
                        e.server_init_data = Some(frame_data);
                    },
                    false,
                )
                .await;
                self.process_ukey2_server_init(&msg).await?;

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::SentUkeyClientFinish;
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            State::SentUkeyClientFinish => {
                debug!("Handling State::SentUkeyClientFinish frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                self.process_connection_response(&frame).await?;

                // Both connection responses are out; the next frame is the
                // receiver's paired key encryption.
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::SentConnectionResponse;
                        e.server_init_data = Some(frame_data);
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            _ if self.unencrypted => {
                let offline = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                self.process_offline_frame(offline).await?;
            }
            _ => {
                debug!("Handling SecureMessage frame");
                // Print the bytes when this fails. A protobuf error here says
                // only that what arrived was not a SecureMessage; it cannot say
                // what it was instead, and the answer has twice been "something
                // from a lower layer that should never have reached us". The
                // first bytes identify it immediately - 0x0A starts a real
                // SecureMessage, 0x08 is a multiplex control body, 0x00 is a
                // length prefix read at the wrong offset.
                let smsg = match SecureMessage::decode(&*frame_data) {
                    Ok(m) => m,
                    Err(e) => {
                        // Not everything on an encrypted channel is encrypted.
                        // The peer signs off with a *plaintext* DISCONNECTION -
                        // measured as the 12 bytes
                        // 08 01 12 08 08 06 3a 04 08 00 10 00, which is
                        // OfflineFrame { V1, type: DISCONNECTION } - and trying
                        // to read that as a SecureMessage produced an alarming
                        // protobuf error for what is simply the other end
                        // hanging up.
                        if let Ok(plain) =
                            location_nearby_connections::OfflineFrame::decode(&*frame_data)
                        {
                            if plain.v1.as_ref().and_then(|v| v.r#type).map(|t| {
                                t == location_nearby_connections::v1_frame::FrameType::Disconnection
                                    as i32
                            }) == Some(true)
                            {
                                info!("Peer sent an unencrypted disconnect, ending session");
                                self.peer_disconnected().await;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            }
                        }
                        error!(
                            "Frame is not a SecureMessage ({e}); {} B beginning {:02x?}",
                            frame_data.len(),
                            &frame_data[..frame_data.len().min(32)]
                        );
                        return Err(e.into());
                    }
                };
                self.decrypt_and_process_secure_message(&smsg).await?;
            }
        }

        Ok(())
    }

    pub async fn send_connection_request(&mut self) -> Result<(), anyhow::Error> {
        let request = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionRequest.into(),
                ),
                connection_request: Some(location_nearby_connections::ConnectionRequestFrame {
                    endpoint_id: Some(String::from_utf8_lossy(&self.endpoint_id).to_string()),
                    // What the peer shows as "sharing with you", so it follows
                    // the user's chosen name rather than the raw hostname.
                    endpoint_name: Some(crate::utils::device_name().into_bytes()),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: crate::utils::device_name(),
                            device_type: DeviceType::Laptop,
                        }
                        .serialize(),
                    ),
                    // Declare what we can actually host.
                    //
                    // This claimed WIFI_LAN only, so a WIFI_HOTSPOT offer landed
                    // outside the negotiated set and the peer ignored it - which
                    // is the likeliest reason a hosted hotspot was never joined.
                    // Only claim it on the slow transport, where we really will
                    // bring one up; on TCP it would be a medium we never offer.
                    // Only claim WIFI_HOTSPOT when the send-side upgrade is
                    // enabled - otherwise we would invite the phone to negotiate
                    // a medium we then ignore, leaving it retrying.
                    mediums: if self.chunk_size < 512 * 1024 && send_wifi_upgrade_enabled() {
                        vec![Medium::WifiHotspot.into(), Medium::WifiLan.into()]
                    } else {
                        vec![Medium::WifiLan.into()]
                    },
                    // Advertise 5GHz so the peer hosts its upgrade AP there.
                    //
                    // We sent no medium_metadata at all, so the phone assumed we
                    // were 2.4GHz-only and hosted its hotspot on 2.4GHz
                    // (frequency 2422 in the offer, and "2.4GHz Mediums:
                    // [WIFI_HOTSPOT]" in its own log). That is why the phone's
                    // hotspot crawled at 2-5 MB/s while WiFi-LAN over a 5GHz
                    // router 100ft away was far faster. Telling it we do 5GHz
                    // should let it pick the fast band.
                    medium_metadata: Some(location_nearby_connections::MediumMetadata {
                        supports_5_ghz: Some(true),
                        supports_6_ghz: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_frame(request.encode_to_vec()).await?;

        Ok(())
    }

    pub async fn send_ukey2_client_init(&mut self) -> Result<(), anyhow::Error> {
        let (secret_key, public_key) = gen_ecdsa_keypair();

        let encoded_point = public_key.to_sec1_point(false);
        let x = encoded_point.x().unwrap();
        let y = encoded_point.y().unwrap();

        let pkey = GenericPublicKey {
            r#type: PublicKeyType::EcP256.into(),
            ec_p256_public_key: Some(EcP256PublicKey {
                x: encode_point(Bytes::from(x.to_vec()))?,
                y: encode_point(Bytes::from(y.to_vec()))?,
            }),
            ..Default::default()
        };

        let finish_frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientFinish.into()),
            message_data: Some(
                Ukey2ClientFinished {
                    public_key: Some(pkey.encode_to_vec()),
                }
                .encode_to_vec(),
            ),
        };

        let sha512 = Sha512::digest(finish_frame.encode_to_vec());
        let frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientInit.into()),
            message_data: Some(
                Ukey2ClientInit {
                    version: Some(1),
                    random: Some(gen_random(32)),
                    next_protocol: Some(String::from("AES_256_CBC-HMAC_SHA256")),
                    cipher_commitments: vec![CipherCommitment {
                        handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512.into()),
                        commitment: Some(sha512.to_vec()),
                    }],
                }
                .encode_to_vec(),
            ),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        self.update_state(
            |e| {
                e.state = State::SentUkeyClientInit;
                e.private_key = Some(secret_key);
                e.client_init_msg_data = Some(frame.encode_to_vec());
                e.ukey_client_finish_msg_data = Some(finish_frame.encode_to_vec());
            },
            false,
        )
        .await;

        Ok(())
    }

    async fn process_ukey2_server_init(&mut self, msg: &Ukey2Message) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ServerInit {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ServerInit",
                msg.message_type
            ));
        }

        let server_init = match Ukey2ServerInit::decode(msg.message_data()) {
            Ok(uk2si) => uk2si,
            Err(e) => {
                return Err(anyhow!("UKey2: Ukey2ClientFinished::decode: {}", e));
            }
        };

        if server_init.version() != 1 {
            self.send_ukey2_alert(AlertType::BadVersion).await?;
            return Err(anyhow!("UKey2: server_init.version != 1"));
        }

        if server_init.random().len() != 32 {
            self.send_ukey2_alert(AlertType::BadRandom).await?;
            return Err(anyhow!("UKey2: server_init.random.len != 32"));
        }

        if server_init.handshake_cipher() != Ukey2HandshakeCipher::P256Sha512 {
            self.send_ukey2_alert(AlertType::BadHandshakeCipher).await?;
            return Err(anyhow!("UKey2: handshake_cipher != P256Sha512"));
        }

        let server_public_key = match GenericPublicKey::decode(server_init.public_key()) {
            Ok(spk) => spk,
            Err(e) => {
                return Err(anyhow!("UKey2: GenericPublicKey::decode: {}", e));
            }
        };

        self.finalize_key_exchange(server_public_key).await?;
        self.send_frame(self.state.ukey_client_finish_msg_data.clone().unwrap())
            .await?;

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into(),
                ),
                connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					os_info: Some(location_nearby_connections::OsInfo {
						r#type: Some(location_nearby_connections::os_info::OsType::Linux.into())
					}),
					..Default::default()
				}),
                ..Default::default()
            }),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        Ok(())
    }

    async fn process_connection_response(
        &mut self,
        frame: &location_nearby_connections::OfflineFrame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() != location_nearby_connections::v1_frame::FrameType::ConnectionResponse
        {
            return Err(anyhow!(format!(
                "Unexpected frame type: {:?}",
                v1_frame.r#type()
            )));
        }

        if v1_frame.connection_response.is_none() {
            return Err(anyhow!("Unexpected None connection_response"));
        }

        if v1_frame.connection_response.as_ref().unwrap().response() != ResponseStatus::Accept {
            return Err(anyhow!("Connection rejected by third party"));
        }

        // Nothing encrypted goes out yet: see
        // `process_paired_key_encryption_frame`.
        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let key = self
            .state
            .recv_hmac_key
            .as_ref()
            .ok_or_else(|| anyhow!("secure message before the key exchange"))?;
        let mut hmac = HmacSha256::new_from_slice(key)?;
        hmac.update(&smsg.header_and_body);
        // Constant-time, so the comparison doesn't leak how much matched.
        hmac.verify_slice(&smsg.signature)
            .map_err(|_| anyhow!("hmac!=signature"))?;

        let header_and_body = HeaderAndBody::decode(&*smsg.header_and_body)?;

        let msg_data = header_and_body.body;
        let key = self.state.decrypt_key.as_ref().unwrap();

        let decrypted =
            crate::hdl::aes256_cbc_decrypt(key, header_and_body.header.iv(), &msg_data)?;

        let d2d_msg = DeviceToDeviceMessage::decode(&*decrypted)?;

        let seq = self.get_client_seq_inc().await;
        if d2d_msg.sequence_number() != seq {
            return Err(anyhow!(
                "Error d2d_msg.sequence_number invalid ({} vs {})",
                d2d_msg.sequence_number(),
                seq
            ));
        }

        let offline = location_nearby_connections::OfflineFrame::decode(d2d_msg.message())?;
        self.process_offline_frame(offline).await
    }

    /// Handle one frame from the peer once it is plaintext: decrypted, or read
    /// as-is from an upgraded channel the peer runs without encryption.
    async fn process_offline_frame(
        &mut self,
        offline: location_nearby_connections::OfflineFrame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = offline
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;
        match v1_frame.r#type() {
            location_nearby_connections::v1_frame::FrameType::PayloadTransfer => {
                trace!("Received FrameType::PayloadTransfer");
                let payload_transfer = v1_frame
                    .payload_transfer
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;

                let header = payload_transfer
                    .payload_header
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;
                let chunk = payload_transfer
                    .payload_chunk
                    .as_ref()
                    .ok_or_else(|| anyhow!("Missing required fields"))?;

                match header.r#type() {
                    payload_header::PayloadType::Bytes => {
                        // Once per chunk, so `trace`.
                        trace!("Processing PayloadType::Bytes");
                        let payload_id = header.id();

                        if let Some(buffer) = crate::hdl::buffer_bytes_chunk(
                            &mut self.state.payload_buffers,
                            payload_id,
                            header.total_size(),
                            chunk,
                            SANE_FRAME_LENGTH as usize,
                        )? {
                            let innner_frame = sharing_nearby::Frame::decode(buffer.as_slice())?;
                            self.process_transfer_setup(&innner_frame).await?;
                        }
                    }
                    payload_header::PayloadType::File => {
                        error!("Unhandled PayloadType::File: {:?}", header.r#type())
                    }
                    payload_header::PayloadType::Stream => {
                        error!("Unhandled PayloadType::Stream: {:?}", header.r#type())
                    }
                    payload_header::PayloadType::UnknownPayloadType => {
                        error!(
                            "Invalid PayloadType::UnknownPayloadType: {:?}",
                            header.r#type()
                        )
                    }
                }
            }
            location_nearby_connections::v1_frame::FrameType::KeepAlive => {
                trace!("Sending keepalive");
                self.send_keepalive().await?;
            }
            location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeRetry => {
                info!(
                    "Received BandwidthUpgradeRetry: {:?}",
                    v1_frame.bandwidth_upgrade_retry
                );
                // Answer with what we support.
                //
                // The peer announces its mediums and waits for ours before
                // choosing one - the receive path has done this for a while, the
                // send path never did. With no answer the phone's negotiated set
                // collapsed to WIFI_LAN alone
                // ("2.4GHz Mediums: [WIFI_LAN]"), which it then could not host
                // with WiFi off: "failed to start listening for incoming Wifi
                // connections", retrying every 32s forever. WIFI_HOTSPOT never
                // entered consideration, so our offer was never weighed.
                if let Err(e) = self.send_supported_mediums().await {
                    warn!("send_supported_mediums failed: {e}");
                }
            }
            location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation => {
                // Log only, for now. Sending over BLE is stuck at ~20 KB/s and
                // will hit the same ~1 MB indication-timeout wall the receive
                // path did, so an upgrade matters here as much as it did there -
                // but the roles are reversed. Receiving, we host the medium and
                // push UPGRADE_PATH_AVAILABLE. Sending, the *phone* is the
                // server, so it offers and we would have to join its network,
                // which on Windows means WlanConnect rather than tethering.
                //
                // Whether it offers at all, and on which medium, decides the
                // shape of that work - so record it before writing any.
                info!(
                    "Bandwidth upgrade: peer sent {:?}",
                    v1_frame.bandwidth_upgrade_negotiation
                );

                // The peer is hosting a medium and has given us the details to
                // reach it. Two shapes, and we prefer the first:
                //
                //   WIFI_LAN    - the phone is on the same LAN as us and offers a
                //                 socket at its LAN address (e.g. 192.168.1.13:
                //                 58571). We already route to it over Ethernet,
                //                 so there is no network to join - just a second
                //                 TCP connection. This is the case that used to
                //                 stall: the handler only knew about hotspot
                //                 credentials, so it ignored the LAN offer and
                //                 kept re-sending UPGRADE_PATH_REQUEST, which the
                //                 phone rejected ("failed to process
                //                 UPGRADE_PATH_REQUEST", "Upgrade in progress")
                //                 while the transfer sat on BLE forever.
                //
                //   WIFI_HOTSPOT - with WiFi off the phone brings up its own AP
                //                 (192.168.49.1, the standard Android P2P gateway)
                //                 and offers it; we associate to that AP first,
                //                 then connect. It needed to see WIFI_HOTSPOT in
                //                 the negotiated set, which it did not until we
                //                 started answering BandwidthUpgradeRetry.
                //
                // Opt-in (see the request block above). Whichever finishes first
                // wins: if the upgrade lands we switch; if the file finishes
                // first, cleanup_upgrade stops it - no size threshold, the
                // events decide.
                #[cfg(all(feature = "experimental", target_os = "windows"))]
                if let Some(info) = v1_frame
                    .bandwidth_upgrade_negotiation
                    .as_ref()
                    .filter(|bun| {
                        bun.event_type()
                            == location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::UpgradePathAvailable
                    })
                    .and_then(|bun| bun.upgrade_path_info.as_ref())
                {
                    if send_wifi_upgrade_enabled() {
                        // Agree with the peer's choice in our introduction; it
                        // applies it either way (see `unencrypted`).
                        self.upgrade_unencrypted = info.supports_disabling_encryption();
                        let lan = info.wifi_lan_socket.as_ref().and_then(|s| {
                            let ip = s.ip_address();
                            // A port that doesn't fit is a bad offer, not one
                            // to wrap round to some other port.
                            let port = u16::try_from(s.wifi_port?).ok()?;
                            (ip.len() == 4).then(|| {
                                (std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]), port)
                            })
                        });
                        if let Some((ip, port)) = lan {
                            self.spawn_lan_upgrade(ip, port);
                        } else if let Some(creds) = info.wifi_hotspot_credentials.clone() {
                            self.spawn_join(creds);
                        }
                    }
                }

                // The peer has finished with the old channel and is waiting for
                // us to release it before using the new socket. Same exchange as
                // the receive path, because hosting the medium puts us in the
                // same role regardless of who is sending the file.
                #[cfg(all(feature = "experimental", target_os = "windows"))]
                {
                    let is_last_write = v1_frame
                        .bandwidth_upgrade_negotiation
                        .as_ref()
                        .map(|bun| {
                            bun.event_type()
                                == location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::LastWriteToPriorChannel
                        })
                        .unwrap_or(false);
                    if is_last_write && !self.upgraded {
                        // The peer is done writing to the old channel. Answer on
                        // that channel, say our own last word on it, and move.
                        //
                        // We do not get a SAFE_TO_CLOSE back. As the side that
                        // *joined* the upgraded medium we are the client, and
                        // the peer treats the old channel as finished once it
                        // has our SAFE_TO_CLOSE. Waiting for one - which this
                        // code did - deadlocks: the peer waits for us to start
                        // using the new medium, we wait for a frame it will
                        // never send, and about nine seconds later it gives up
                        // and sends a plaintext DISCONNECTION
                        // (08 01 12 08 08 06 3a 04 08 00 10 00). That was
                        // measured three times out of three.
                        //
                        // SAFE_TO_CLOSE is the last thing we may put on this
                        // channel. Not one frame more.
                        //
                        // The name is literal: it tells the peer it may close
                        // the prior channel, and the peer does so immediately.
                        // A LAST_WRITE sent after it - which this code did, for
                        // protocol tidiness - lands on a channel that is already
                        // going away and comes back as
                        // "EndpointManager received an invalid OfflineFrame ...
                        // invalid tag (zero)" 119 ms after the peer logs
                        // "process BANDWIDTH_UPGRADE_NEGOTIATION frame event
                        // type:SAFE_TO_CLOSE_PRIOR_CHANNEL". The peer then stops
                        // reading, and every such transfer stalls at exactly the
                        // 98304 bytes BLE had already delivered.
                        //
                        // We owe no LAST_WRITE of our own here. As the side that
                        // joined the upgraded medium we are the client: the peer
                        // announces it is done with the old channel, we
                        // acknowledge, and the channel is finished.
                        info!(
                            "Bandwidth upgrade: peer sent LAST_WRITE_TO_PRIOR_CHANNEL; answering"
                        );
                        if let Err(e) = self.send_safe_to_close_prior_channel().await {
                            warn!("send_safe_to_close_prior_channel failed: {e}");
                        }
                        // Only move if there is something to move onto. The
                        // peer's LAST_WRITE can only follow our
                        // CLIENT_INTRODUCTION, which only happens once the join
                        // has landed, so this should always hold - but splicing
                        // onto a socket that does not exist would strand the
                        // transfer on a channel both sides have just finished
                        // with, and that is worth being explicit about.
                        if self.switch_tx.is_some()
                            && self.join_ready.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            self.switch_to_upgraded_medium().await?;
                        } else {
                            warn!(
                                "Bandwidth upgrade: peer released the prior channel before the \
                                 join was ready; staying on the current medium"
                            );
                        }
                    }

                    // The peer hosts and has released the old channel - this is
                    // the answer to the LAST_WRITE we sent after joining its AP.
                    // The switch happens here and not a moment earlier: moving
                    // the stream before the old channel is released is what hung
                    // the receive path.
                    let is_safe_to_close = v1_frame
                        .bandwidth_upgrade_negotiation
                        .as_ref()
                        .map(|bun| {
                            bun.event_type()
                                == location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::SafeToClosePriorChannel
                        })
                        .unwrap_or(false);
                    // Both sides have now said their last word on the old
                    // channel, so it is released and the stream can move. This
                    // is the switch point, and the only one.
                    // `switch_tx` is cleared when an upgrade is abandoned, so a
                    // late answer cannot splice a stream that has moved on.
                    if is_safe_to_close && !self.upgraded && self.switch_tx.is_some() {
                        info!("Bandwidth upgrade: peer released the prior channel; switching");
                        self.switch_to_upgraded_medium().await?;
                    }
                }
            }
            location_nearby_connections::v1_frame::FrameType::Disconnection => {
                // The remote device closed the session (normal after a transfer
                // completes). End cleanly rather than logging it as an error.
                info!("Received disconnect frame, ending session");
                self.peer_disconnected().await;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
            _ => {
                error!("Unhandled offline frame encrypted: {:?}", offline);
            }
        }

        Ok(())
    }

    /// The peer hung up.
    ///
    /// Once everything is sent, that is the receiver confirming it has the
    /// data, and only then is the transfer Finished. Anywhere earlier it is a
    /// failure the UI has to be told about, or it keeps showing the last step
    /// as if still working.
    async fn peer_disconnected(&mut self) {
        if matches!(
            self.state.state,
            State::Finished | State::Cancelled | State::Rejected | State::Disconnected
        ) {
            return;
        }
        let state = if self.hang_up_deadline.is_some() {
            State::Finished
        } else {
            State::Disconnected
        };
        self.update_state(
            |e| {
                e.state = state;
            },
            true,
        )
        .await;
    }

    async fn process_transfer_setup(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() == sharing_nearby::v1_frame::FrameType::Cancel {
            info!("Transfer canceled");
            self.update_state(
                |e| {
                    e.state = State::Cancelled;
                },
                true,
            )
            .await;
            self.disconnection().await?;
            return Err(anyhow!(crate::errors::AppError::NotAnError));
        }

        match self.state.state {
            State::SentConnectionResponse => {
                debug!("Processing State::SentConnectionResponse");
                self.process_paired_key_encryption_frame(v1_frame).await?;
                self.update_state(
                    |e| {
                        e.state = State::SentPairedKeyResult;
                    },
                    false,
                )
                .await;
            }
            State::SentPairedKeyResult => {
                debug!("Processing State::SentPairedKeyResult");
                self.process_paired_key_result(v1_frame).await?;
                self.update_state(
                    |e| {
                        e.state = State::SentIntroduction;
                    },
                    true,
                )
                .await;
            }
            State::SentIntroduction => {
                debug!("Processing State::SentIntroduction");
                self.process_consent(v1_frame).await?;
            }
            State::SendingFiles => {}
            _ => {
                info!(
                    "Unhandled connection state in process_transfer_setup: {:?}",
                    self.state.state
                );
            }
        }

        Ok(())
    }

    async fn process_paired_key_encryption_frame(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_encryption.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        // Ours goes out only now, in reply to the receiver's, because the
        // receiver's is the first sign that its side of the channel is
        // encrypted. A receiver switches to encryption only after sending its
        // connection response, and sending ours the moment that response
        // arrived could beat the switch over BLE: a Pixel then read our first
        // encrypted frame as plaintext, dropped it ("invalid OfflineFrame"),
        // rejected the next one with "Incorrect sequence number" and hung up
        // before the user saw anything. Receivers, ours included, send theirs
        // without waiting for the sender's.
        let paired_encryption = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyEncryption.into()),
                paired_key_encryption: Some(sharing_nearby::PairedKeyEncryptionFrame {
                    secret_id_hash: Some(gen_random(6)),
                    signed_data: Some(gen_random(72)),
                    qr_code_handshake_data: self
                        .qr_key
                        .as_ref()
                        .zip(self.auth_key.as_deref())
                        .map(|(key, auth_key)| crate::qr::handshake_signature(key, auth_key)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.send_encrypted_frame(&paired_encryption).await?;

        let paired_result = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyResult.into()),
                paired_key_result: Some(sharing_nearby::PairedKeyResultFrame {
                    status: Some(paired_key_result_frame::Status::Unable.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&paired_result).await?;

        Ok(())
    }

    async fn process_paired_key_result(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_result.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        let mut file_metadata: Vec<FileMetadata> = vec![];
        let mut text_metadata: Vec<TextMetadata> = vec![];
        let mut transferred_files: HashMap<i64, InternalFileInfo> = HashMap::new();
        let mut total_to_send = 0;
        let mut pending_text: Option<(i64, Vec<u8>)> = None;
        match &self.payload {
            OutboundPayload::Files(files) => {
                for f in files {
                    // A file that can't be sent fails the transfer. Skipping it
                    // used to send the rest and report Finished, so the user
                    // was told everything went when some of it never left.
                    let path = Path::new(f);
                    if !path.is_file() {
                        return Err(anyhow!("can't send {f}: it is not a file"));
                    }

                    let file = File::open(f).map_err(|e| anyhow!("can't open {f}: {e}"))?;
                    let fmetadata = file
                        .metadata()
                        .map_err(|e| anyhow!("can't read the size of {f}: {e}"))?;

                    let ftype = mime_guess::from_path(path)
                        .first_or_octet_stream()
                        .to_string();

                    let meta_type = if ftype.starts_with("image/") {
                        file_metadata::Type::Image
                    } else if ftype.starts_with("video/") {
                        file_metadata::Type::Video
                    } else if ftype.starts_with("audio/") {
                        file_metadata::Type::Audio
                    } else if path.extension().unwrap_or_default() == "apk" {
                        file_metadata::Type::App
                    } else {
                        file_metadata::Type::Unknown
                    };

                    info!("File type to send: {}", ftype);
                    let fname = path
                        .file_name()
                        .ok_or_else(|| anyhow!("Failed to get file_name for {f}"))?;
                    let fmeta = FileMetadata {
                        // The attachment id, distinct from the payload id and
                        // required. Google's receiver swaps an id of 0 for a
                        // random one but files the payload under 0, so it never
                        // pairs the payload with its attachment: Windows Quick
                        // Share saved the file and still said "Can't complete
                        // transfer".
                        id: Some(rand::rng().random::<i64>()),
                        payload_id: Some(rand::rng().random::<i64>()),
                        name: Some(fname.to_string_lossy().into_owned()),
                        size: Some(fmetadata.len() as i64),
                        mime_type: Some(ftype),
                        r#type: Some(meta_type.into()),
                    };
                    transferred_files.insert(
                        fmeta.payload_id(),
                        InternalFileInfo {
                            file_url: path.to_path_buf(),
                            bytes_transferred: 0,
                            total_size: fmeta.size(),
                            file: Some(file),
                            ..Default::default()
                        },
                    );
                    file_metadata.push(fmeta);
                    total_to_send += fmetadata.len();
                }
            }
            OutboundPayload::Text(text) => {
                let payload_id = rand::rng().random::<i64>();
                let bytes = text.clone().into_bytes();
                total_to_send += bytes.len() as u64;

                // Detect URLs so Android opens them in a browser; otherwise
                // send as plain text.
                let trimmed = text.trim();
                let ttype = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    text_metadata::Type::Url
                } else {
                    text_metadata::Type::Text
                };

                text_metadata.push(TextMetadata {
                    text_title: Some(text.chars().take(64).collect::<String>()),
                    r#type: Some(ttype.into()),
                    payload_id: Some(payload_id),
                    size: Some(bytes.len() as i64),
                    id: Some(rand::rng().random::<i64>()),
                });

                pending_text = Some((payload_id, bytes));
            }
        }
        self.state.outbound_text = pending_text;

        self.update_state(
            |e| {
                if let Some(tmd) = e.transfer_metadata.as_mut() {
                    tmd.total_bytes = total_to_send;
                }
                e.transferred_files = transferred_files;
            },
            false,
        )
        .await;

        let introduction = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Introduction.into()),
                introduction: Some(IntroductionFrame {
                    file_metadata,
                    text_metadata,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&introduction).await?;

        Ok(())
    }

    async fn process_consent(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.r#type() != sharing_nearby::v1_frame::FrameType::Response
            || v1_frame.connection_response.is_none()
        {
            return Err(anyhow!("Missing required fields"));
        }

        match v1_frame.connection_response.as_ref().unwrap().status() {
            sharing_nearby::connection_response_frame::Status::Accept => {
                info!("State is now State::SendingFiles");
                self.update_state(
                    |e| {
                        e.state = State::SendingFiles;
                    },
                    true,
                )
                .await;

                // If we're sending a text payload, emit it as a single Bytes
                // payload and finish (there are no files to loop over).
                if let Some((payload_id, bytes)) = self.state.outbound_text.take() {
                    self.send_text_payload(payload_id, bytes).await?;
                    // Finished only once the receiver confirms; see
                    // `peer_disconnected`.
                    self.update_state(
                        |e| {
                            if let Some(tmd) = e.transfer_metadata.as_mut() {
                                tmd.ack_bytes = tmd.total_bytes;
                            }
                        },
                        true,
                    )
                    .await;
                    self.await_peer_hang_up();
                    return Ok(());
                }

                // Kick off a bandwidth upgrade, but do NOT wait for it.
                //
                // Event-driven, not timer-driven. The old design blocked here
                // for a fixed 30s hoping the upgrade would land, then fell back
                // to BLE - but the slow part (Windows associating to the phone's
                // WiFi-Direct group owner) takes a *variable* 5-30s, so a fixed
                // deadline was always wrong: too short and a slow-but-fine join
                // is abandoned, too long and every genuine BLE-only transfer
                // stalls up front. Instead: announce our mediums and ask for a
                // path once, then start streaming file data over BLE right away.
                // When the switch event fires (see the SafeToClose handler) the
                // socket is spliced under the duplex and the very next chunk
                // goes over WiFi - whenever that happens, however long it took.
                // The send-side WiFi upgrade (join the phone's hotspot) rides
                // along with it, on by default - see send_wifi_upgrade_enabled.
                #[cfg(all(feature = "experimental", target_os = "windows"))]
                if self.upgrade_tx.is_some() && send_wifi_upgrade_enabled() {
                    info!("Requesting a bandwidth upgrade; streaming over BLE meanwhile");
                    let _ = self.send_supported_mediums().await;
                    let _ = self.send_upgrade_path_request().await;
                }

                let ids: Vec<i64> = self.state.transferred_files.keys().cloned().collect();
                info!("We are sending: {:?}", ids);
                let mut ids_iter = ids.into_iter();
                // Throughput, reported every 5s. The only way to tell "the
                // switch happened but the data path is slow" from "the link
                // itself is slow" without external tools - the previous crawl
                // over WiFi (ping to the AP was clean) needed exactly this.
                let mut tp_bytes: u64 = 0;
                let mut tp_last = std::time::Instant::now();
                // Loop through all files
                loop {
                    let current = match ids_iter.next() {
                        Some(i) => i,
                        None => {
                            // Sent is not received: Finished waits for the
                            // receiver to confirm; see `peer_disconnected`.
                            info!("All files sent; waiting for the receiver to confirm");
                            // Breaking instead of NotAnError so `handle` keeps
                            // reading until the peer hangs up.
                            self.await_peer_hang_up();
                            break;
                        }
                    };

                    // Ask for a faster medium before committing to a long send.
                    // Only worth it where the transport is slow enough to care -
                    // `chunk_size` was lowered precisely because this one is.
                    // `!self.upgraded` is essential, not just an optimisation.
                    // Once switched, this socket carries encrypted payload; a
                    // negotiation frame injected into that stream is garbage to
                    // the peer, which stops reading - the socket fills, our write
                    // blocks forever, and the transfer dies with a broken pipe
                    // having sent zero bytes. This ran on every file because
                    // chunk_size stays small after the upgrade.
                    // Re-nudge the upgrade per file, opt-in. The initial request
                    // is sent once before the loop; this just keeps prompting a
                    // peer that hasn't offered yet, and only when the send-side
                    // WiFi upgrade is enabled. Stop once an upgrade is running
                    // (`!self.join_started`): the phone has already offered a
                    // path and is hosting it, so another UPGRADE_PATH_REQUEST is
                    // the redundant frame it rejects as "Upgrade in progress".
                    // Never after we've switched (`!self.upgraded`) - a
                    // negotiation frame in the encrypted payload stream wedges the
                    // peer.
                    #[cfg(all(feature = "experimental", target_os = "windows"))]
                    if self.chunk_size < 512 * 1024
                        && !self.upgraded
                        && !self.join_started
                        && send_wifi_upgrade_enabled()
                    {
                        if let Err(e) = self.send_upgrade_path_request().await {
                            warn!("send_upgrade_path_request failed: {e}");
                        }
                    }

                    // Loop until we reached end of file
                    loop {
                        // Answer anything the peer has sent - KeepAlive above
                        // all - before spending another chunk's worth of time
                        // writing. Without this the peer hears nothing for the
                        // whole transfer and closes at its 30s timeout.
                        self.service_pending_frames().await?;

                        // Stop feeding payload the moment the join is ready, and
                        // do not resume until the stream has actually moved.
                        //
                        // The peer promotes its own channel when it receives
                        // CLIENT_INTRODUCTION, not when the old channel closes.
                        // Its log is explicit about both the moment and the
                        // cost: "replaced endpoint's channel from ENCRYPTED_BLE
                        // to ENCRYPTED_WIFI_HOTSPOT" at 13:24:38.394, while this
                        // side went on writing payload over BLE until 13:24:47
                        // because the transport was still draining a 32 KB chunk
                        // queued before any of this began. Nine seconds of
                        // writing to a channel the peer had already replaced,
                        // and the transfer never lined up again - it stopped at
                        // the 98304 bytes BLE had delivered and the peer
                        // cancelled sixty seconds later.
                        //
                        // Holding here empties the transport's backlog instead
                        // of growing it, so the splice happens promptly once the
                        // peer releases the old channel. Waiting is done by
                        // servicing frames, which is also how the event we are
                        // waiting for - the peer's LAST_WRITE - arrives.
                        //
                        // No deadline. The peer sent LAST_WRITE within a second
                        // of the introduction in every capture, and if it never
                        // does, its own transfer timeout ends the session with a
                        // real error rather than this side inventing one.
                        #[cfg(all(feature = "experimental", target_os = "windows"))]
                        while self.join_ready.load(std::sync::atomic::Ordering::Relaxed)
                            && !self.upgraded
                            && self.switch_tx.is_some()
                        {
                            self.await_next_frame().await?;
                        }

                        let tp_elapsed = tp_last.elapsed();
                        if tp_elapsed >= std::time::Duration::from_secs(5) {
                            info!(
                                "Send throughput: {:.0} KB/s",
                                tp_bytes as f64 / 1024.0 / tp_elapsed.as_secs_f64()
                            );
                            tp_bytes = 0;
                            tp_last = std::time::Instant::now();
                        }

                        // Workaround to limit scope of the immutable borrow on self
                        let (curr_state, buffer, bytes_read) = {
                            let curr_state = match self.state.transferred_files.get(&current) {
                                Some(s) => s,
                                None => break,
                            };

                            info!("> Currently sending {:?}", curr_state.file_url);
                            if curr_state.bytes_transferred == curr_state.total_size {
                                debug!("File {current} finished");
                                self.update_state(
                                    |e| {
                                        e.transferred_files.remove(&current);
                                    },
                                    false,
                                )
                                .await;
                                break;
                            }

                            if curr_state.file.is_none() {
                                warn!("File {current} is none");
                                break;
                            }

                            // Read no further than the size the peer was told:
                            // a file that grew since then is sent as it was
                            // announced. One that shrank would otherwise read 0
                            // bytes forever and never reach `total_size`.
                            let remaining = curr_state.total_size - curr_state.bytes_transferred;
                            let want = remaining.min(self.chunk_size as i64) as usize;
                            let mut buffer = vec![0u8; want];
                            let bytes_read = curr_state.file.as_ref().unwrap().read(&mut buffer)?;
                            if bytes_read == 0 {
                                return Err(anyhow!(
                                    "{:?} shrank while sending: {} of {} bytes read",
                                    curr_state.file_url,
                                    curr_state.bytes_transferred,
                                    curr_state.total_size
                                ));
                            }

                            (
                                InternalFileInfo {
                                    file_url: curr_state.file_url.clone(),
                                    bytes_transferred: curr_state.bytes_transferred,
                                    total_size: curr_state.total_size,
                                    file: None,
                                    ..Default::default()
                                },
                                buffer,
                                bytes_read,
                            )
                        };

                        tp_bytes += bytes_read as u64;
                        let sending_buffer = buffer[..bytes_read].to_vec();
                        info!(
                            "> File ready: {bytes_read} bytes && {} && left to send: {} with current offset: {}",
                            sending_buffer.len(),
                            curr_state.total_size - curr_state.bytes_transferred,
							curr_state.bytes_transferred
                        );

                        let payload_header = PayloadHeader {
                            id: Some(current),
                            r#type: Some(payload_header::PayloadType::File.into()),
                            total_size: Some(curr_state.total_size),
                            is_sensitive: Some(false),
                            file_name: curr_state
                                .file_url
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned()),
                            ..Default::default()
                        };

                        let wrapper = location_nearby_connections::OfflineFrame {
							version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
							v1: Some(location_nearby_connections::V1Frame {
								r#type: Some(
									location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
								),
								payload_transfer: Some(PayloadTransferFrame {
									packet_type: Some(PacketType::Data.into()),
									payload_chunk: Some(PayloadChunk {
										offset: Some(curr_state.bytes_transferred),
										flags: Some(0),
										body: Some(buffer[..bytes_read].to_vec()),
									}),
									payload_header: Some(payload_header.clone()),
									..Default::default()
								}),
								..Default::default()
							}),
						};

                        self.encrypt_and_send(&wrapper).await?;
                        // Every chunk updates the progress board; see
                        // channel::PROGRESS for why it isn't broadcast.
                        self.update_state(
                            |e| {
                                if let Some(mu) = e.transferred_files.get_mut(&current) {
                                    mu.bytes_transferred += bytes_read as i64;
                                }

                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                    tmd.ack_bytes += bytes_read as u64;
                                }
                            },
                            false,
                        )
                        .await;
                        self.publish_progress();

                        // If we just sent the last bytes of the file, mark it as finished
                        if curr_state.bytes_transferred + bytes_read as i64 == curr_state.total_size
                        {
                            debug!(
                                "File {current} finished, curr offset: {} over total: {}",
                                curr_state.bytes_transferred + bytes_read as i64,
                                curr_state.total_size
                            );

                            let wrapper = location_nearby_connections::OfflineFrame {
								version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
								v1: Some(location_nearby_connections::V1Frame {
									r#type: Some(
										location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
									),
									payload_transfer: Some(PayloadTransferFrame {
										packet_type: Some(PacketType::Data.into()),
										payload_chunk: Some(PayloadChunk {
											offset: Some(curr_state.total_size),
											flags: Some(1), // lastChunk
											body: Some(vec![]),
										}),
										payload_header: Some(payload_header),
										..Default::default()
									}),
									..Default::default()
								}),
							};

                            self.encrypt_and_send(&wrapper).await?;
                            break;
                        }
                    }
                }
            }
            sharing_nearby::connection_response_frame::Status::Reject
            | sharing_nearby::connection_response_frame::Status::NotEnoughSpace
            | sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType
            | sharing_nearby::connection_response_frame::Status::TimedOut => {
                let status = v1_frame.connection_response.as_ref().unwrap().status();
                warn!("Cannot process: consent denied: {status:?}");
                // A decline is the receiver's answer, not a failure, and the UI
                // says "Transfer rejected" for it. It used to be reported as
                // Disconnected, which shows "Unexpected disconnection". The
                // other statuses are the transfer failing on the receiver's
                // side - no space, a type it can't take, nobody answering.
                let state = if status == sharing_nearby::connection_response_frame::Status::Reject {
                    State::Rejected
                } else {
                    State::Disconnected
                };
                self.update_state(
                    |e| {
                        e.state = state;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
            sharing_nearby::connection_response_frame::Status::Unknown => {
                error!("Unknown consent type: aborting");
                self.update_state(
                    |e| {
                        e.state = State::Disconnected;
                    },
                    true,
                )
                .await;
                self.disconnection().await?;
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
        }

        Ok(())
    }

    async fn send_text_payload(
        &mut self,
        payload_id: i64,
        bytes: Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        let total_size = bytes.len() as i64;
        let payload_header = PayloadHeader {
            id: Some(payload_id),
            r#type: Some(payload_header::PayloadType::Bytes.into()),
            total_size: Some(total_size),
            is_sensitive: Some(false),
            ..Default::default()
        };

        // The text content itself as the payload body.
        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::Data.into()),
                    payload_chunk: Some(PayloadChunk {
                        offset: Some(0),
                        flags: Some(0),
                        body: Some(bytes),
                    }),
                    payload_header: Some(payload_header.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&wrapper).await?;

        // Final empty chunk flagged as the last one.
        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(PayloadTransferFrame {
                    packet_type: Some(PacketType::Data.into()),
                    payload_chunk: Some(PayloadChunk {
                        offset: Some(total_size),
                        flags: Some(1),
                        body: Some(vec![]),
                    }),
                    payload_header: Some(payload_header),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&wrapper).await?;

        Ok(())
    }

    /// Leave the session open for the receiver to close; see
    /// `PEER_HANG_UP_GRACE`.
    fn await_peer_hang_up(&mut self) {
        self.hang_up_deadline = Some(tokio::time::Instant::now() + PEER_HANG_UP_GRACE);
    }

    async fn disconnection(&mut self) -> Result<(), anyhow::Error> {
        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(location_nearby_connections::DisconnectionFrame {
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&frame).await
        } else {
            self.send_frame(frame.encode_to_vec()).await
        }
    }

    async fn finalize_key_exchange(
        &mut self,
        raw_peer_key: GenericPublicKey,
    ) -> Result<(), anyhow::Error> {
        let peer_p256_key = raw_peer_key
            .ec_p256_public_key
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        let mut bytes = vec![0x04];
        bytes.extend_from_slice(&decode_point(&peer_p256_key.x)?);
        bytes.extend_from_slice(&decode_point(&peer_p256_key.y)?);

        let encoded_point = Sec1Point::from_bytes(bytes)?;
        // `from_bytes` only validates the *encoding*; the point may still not lie
        // on the curve, in which case `from_sec1_point` yields none and
        // CtOption::unwrap() would panic on peer-supplied bytes.
        let peer_key: PublicKey = Option::from(PublicKey::from_sec1_point(&encoded_point))
            .ok_or_else(|| anyhow!("Invalid peer public key: point is not on the P-256 curve"))?;
        let priv_key = self.state.private_key.as_ref().unwrap();

        let dhs = diffie_hellman(priv_key.to_nonzero_scalar(), peer_key.as_affine());
        let derived_secret = Sha256::digest(dhs.raw_secret_bytes());

        let mut ukey_info: Vec<u8> = vec![];
        ukey_info.extend_from_slice(self.state.client_init_msg_data.as_ref().unwrap());
        ukey_info.extend_from_slice(self.state.server_init_data.as_ref().unwrap());

        let D2DKeys {
            auth_string,
            client_key,
            client_hmac_key,
            server_key,
            server_hmac_key,
        } = derive_d2d_keys(&derived_secret, &ukey_info)?;
        self.auth_key = Some(auth_string.clone());

        self.update_state(
            |e| {
                e.decrypt_key = Some(server_key);
                e.recv_hmac_key = Some(server_hmac_key);
                e.encrypt_key = Some(client_key);
                e.send_hmac_key = Some(client_hmac_key);
                e.pin_code = Some(to_four_digit_string(&auth_string));
                e.encryption_done = true;

                if let Some(ref mut tm) = e.transfer_metadata {
                    tm.pin_code = Some(to_four_digit_string(&auth_string));
                }
            },
            true,
        )
        .await;

        info!("Pin code: {:?}", self.state.pin_code);

        Ok(())
    }

    async fn send_ukey2_alert(&mut self, atype: AlertType) -> Result<(), anyhow::Error> {
        let alert = Ukey2Alert {
            r#type: Some(atype.into()),
            error_message: None,
        };

        let data = Ukey2Message {
            message_type: Some(atype.into()),
            message_data: Some(alert.encode_to_vec()),
        };

        self.send_frame(data.encode_to_vec()).await
    }

    async fn send_encrypted_frame(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let frame_data = frame.encode_to_vec();
        let body_size = frame_data.len();

        let payload_header = PayloadHeader {
            id: Some(rand::rng().random_range(i64::MIN..i64::MAX)),
            r#type: Some(payload_header::PayloadType::Bytes.into()),
            total_size: Some(body_size as i64),
            is_sensitive: Some(false),
            ..Default::default()
        };

        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(0),
                flags: Some(0),
                body: Some(frame_data),
            }),
            payload_header: Some(payload_header.clone()),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        // Send lastChunk
        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(body_size as i64),
                flags: Some(1), // lastChunk
                body: Some(vec![]),
            }),
            payload_header: Some(payload_header),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        Ok(())
    }

    async fn encrypt_and_send(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
        let plaintext = frame.encode_to_vec();
        // See `unencrypted`: on such a channel the frame goes out as it is.
        if self.unencrypted {
            return self.send_frame(plaintext).await;
        }
        let seq = self.get_server_seq_inc().await;

        // What we actually put on the wire, in order, with its sequence number.
        //
        // The peer discards our first encrypted frame with "Protocol message
        // contained an invalid tag (zero)" and then rejects the next one for an
        // incorrect sequence number, three milliseconds after it flips the
        // channel to encrypted. Both halves of that are statements about bytes
        // and ordering, and until now this side had no record of either - every
        // conclusion about what we sent has been inferred. This makes it
        // observable.
        debug!(
            "encrypt_and_send: seq {seq}, type {:?}, {} B plaintext beginning {:02x?}",
            frame.v1.as_ref().and_then(|v| v.r#type),
            plaintext.len(),
            &plaintext[..plaintext.len().min(16)]
        );

        let d2d_msg = DeviceToDeviceMessage {
            sequence_number: Some(seq),
            message: Some(plaintext),
        };

        let key = self.state.encrypt_key.as_ref().unwrap();
        let msg_data = d2d_msg.encode_to_vec();
        let iv = gen_random(16);

        let encrypted = crate::hdl::aes256_cbc_encrypt(key, &iv, &msg_data)?;

        let hb = HeaderAndBody {
            body: encrypted,
            header: Header {
                encryption_scheme: EncScheme::Aes256Cbc.into(),
                signature_scheme: SigScheme::HmacSha256.into(),
                iv: Some(iv),
                public_metadata: Some(
                    GcmMetadata {
                        r#type: Type::DeviceToDeviceMessage.into(),
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
        };

        let mut hmac = HmacSha256::new_from_slice(self.state.send_hmac_key.as_ref().unwrap())?;
        hmac.update(&hb.encode_to_vec());
        let result = hmac.finalize();

        let smsg = SecureMessage {
            header_and_body: hb.encode_to_vec(),
            signature: result.into_bytes().to_vec(),
        };

        self.send_frame(smsg.encode_to_vec()).await?;

        Ok(())
    }

    /// Start joining the peer's AP *in the background*, so the send loop keeps
    /// streaming over BLE while the (slow, variable) WiFi association runs.
    ///
    /// The task does everything up to and including CLIENT_INTRODUCTION - all
    /// raw socket / WiFi work that needs no encryption state - then hands the
    /// socket to the transport and sets `join_ready`. The main loop watches that
    /// flag and sends LAST_WRITE itself (which needs the encrypted BLE channel),
    /// so the whole thing is event-driven: nothing waits on a fixed timer, and a
    /// slow join simply upgrades later rather than being abandoned.
    ///
    /// This path associates the WiFi adapter to the peer's AP, and records it in
    /// `joined_ap` once it has: cleanup will disconnect it afterwards. The
    /// WIFI_LAN sibling (`spawn_lan_upgrade`) does not.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    fn spawn_join(
        &mut self,
        creds: crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::WifiHotspotCredentials,
    ) {
        if self.join_started {
            return;
        }
        let Some(upgrade_tx) = self.upgrade_tx.clone() else {
            return;
        };
        let (Some(ssid), Some(password)) = (creds.ssid.clone(), creds.password.clone()) else {
            warn!("Bandwidth upgrade: peer offered a hotspot with no credentials");
            return;
        };
        let Ok(port) = u16::try_from(creds.port()) else {
            warn!(
                "Bandwidth upgrade: peer offered a hotspot on port {}",
                creds.port()
            );
            return;
        };
        self.join_started = true;
        let gateway = creds.gateway().to_string();
        let endpoint_id = String::from_utf8_lossy(&self.endpoint_id).to_string();
        let unencrypted = self.upgrade_unencrypted;
        let ready = self.join_ready.clone();
        let joined_ap = self.joined_ap.clone();
        let cancel = self.join_cancel.clone();

        self.join_handle = Some(tokio::spawn(async move {
            info!("Bandwidth upgrade: joining the peer's network {ssid} (background)");
            // Not raced against `cancel`: the join watches it itself and ends
            // promptly, and it must get to record a network it did join, or
            // nothing would ever leave it.
            match crate::hdl::join(&ssid, &password, cancel.clone()).await {
                Ok(joined) => match joined_ap.lock() {
                    Ok(mut slot) => *slot = Some(joined),
                    Err(e) => warn!("Bandwidth upgrade: could not record the join: {e}"),
                },
                Err(e) => {
                    warn!("Bandwidth upgrade: join failed, staying on BLE: {e}");
                    return;
                }
            }

            let run = async {
                // Association is not addressing: wait for a DHCP lease on the
                // peer's subnet before connecting, or the connect has no route
                // and hangs.
                if let Ok(gw) = gateway.parse::<std::net::Ipv4Addr>() {
                    if crate::hdl::wait_for_route(gw).await {
                        info!("Bandwidth upgrade: hold an address on the peer's subnet");
                    } else {
                        warn!("Bandwidth upgrade: no address on the peer's subnet; trying anyway");
                    }
                }

                // One connect. The phone offers its network only once it is
                // listening on it, and this runs once we hold an address there.
                // It used to retry every 500 ms for 20 s, each try capped at
                // 5 s. A failure leaves the transfer on BLE.
                let socket = tokio::net::TcpStream::connect(format!("{gateway}:{port}"))
                    .await
                    .map_err(|e| anyhow!("could not reach {gateway}:{port}: {e}"))?;

                introduce_over_upgraded_socket(socket, endpoint_id, unencrypted, upgrade_tx).await
            };

            tokio::select! {
                r = run => match r {
                    // Signal the main loop to send LAST_WRITE and complete the
                    // switch. Only on success - a failed join leaves us on BLE.
                    Ok(()) => ready.store(true, std::sync::atomic::Ordering::Relaxed),
                    Err(e) => warn!("Bandwidth upgrade: join failed, staying on BLE: {e}"),
                },
                _ = cancel.cancelled() => {
                    info!("Bandwidth upgrade: the transfer ended before the upgrade did");
                }
            }
        }));
    }

    /// Take a WIFI_LAN upgrade the peer offered: connect straight to its LAN
    /// socket and hand the transport the result.
    ///
    /// This is the phone-on-WiFi case. The phone hosts a WiFi-LAN listener at its
    /// own LAN address and sends UPGRADE_PATH_AVAILABLE with that ip:port. We
    /// already reach it over Ethernet, so unlike `spawn_join` there is nothing to
    /// associate - just a second TCP connection over the route we have. Because
    /// no adapter state changes, `joined_ap` stays false and cleanup leaves WiFi
    /// alone.
    ///
    /// Everything after the connect - CLIENT_INTRODUCTION, the ACK, the handoff,
    /// `join_ready`, and the switch the main loop drives off it - is identical to
    /// the hotspot path, because the bandwidth-upgrade protocol is the same
    /// regardless of which medium carries it.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    fn spawn_lan_upgrade(&mut self, ip: std::net::Ipv4Addr, port: u16) {
        if self.join_started {
            return;
        }
        let Some(upgrade_tx) = self.upgrade_tx.clone() else {
            return;
        };
        self.join_started = true;
        let endpoint_id = String::from_utf8_lossy(&self.endpoint_id).to_string();
        let unencrypted = self.upgrade_unencrypted;
        let ready = self.join_ready.clone();
        let cancel = self.join_cancel.clone();

        self.join_handle = Some(tokio::spawn(async move {
            let run = async {
                info!("Bandwidth upgrade: connecting to the peer's LAN socket {ip}:{port} (background)");
                // One connect. The peer logs "Successfully started listening
                // for incoming Wifi Lan connections" before it offers, so the
                // listener is up. It used to retry every 300 ms for 10 s. A
                // connect that fails leaves `join_ready` false and the transfer
                // finishes over BLE, exactly as a failed AP join does.
                let addr = std::net::SocketAddr::from((ip, port));
                let socket = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| anyhow!("could not reach {ip}:{port}: {e}"))?;

                introduce_over_upgraded_socket(socket, endpoint_id, unencrypted, upgrade_tx).await
            };

            tokio::select! {
                r = run => match r {
                    Ok(()) => ready.store(true, std::sync::atomic::Ordering::Relaxed),
                    Err(e) => warn!("Bandwidth upgrade: LAN connect failed, staying on BLE: {e}"),
                },
                _ = cancel.cancelled() => {
                    info!("Bandwidth upgrade: the transfer ended before the upgrade did");
                }
            }
        }));
    }

    /// Stop any in-flight upgrade and leave the peer's AP if we joined it.
    ///
    /// Called when the transfer ends, by every path that ends it; safe to call
    /// more than once. If a small file finished over BLE while we were still
    /// associating, the join stops before connecting - or leaves again if it
    /// had just connected - instead of joining the phone's network after the
    /// transfer is over. Only then is the adapter disconnected, and only from
    /// the network it joined.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    pub async fn cleanup_upgrade(&mut self) {
        self.join_cancel.cancel();
        // Wait rather than abort: the task must have recorded what it joined.
        // It ends promptly once cancelled.
        if let Some(h) = self.join_handle.take() {
            if let Err(e) = h.await {
                warn!("Bandwidth upgrade: the upgrade task failed: {e}");
            }
        }
        let joined = match self.joined_ap.lock() {
            Ok(mut slot) => slot.take(),
            Err(e) => {
                warn!("Bandwidth upgrade: could not read the join: {e}");
                None
            }
        };
        if let Some(joined) = joined {
            crate::hdl::wifi_disconnect(joined).await;
        }
    }

    /// Move the stream onto the upgraded socket.
    ///
    /// Called when the peer answers SAFE_TO_CLOSE_PRIOR_CHANNEL - the point at
    /// which both sides have agreed the old channel is drained.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn switch_to_upgraded_medium(&mut self) -> Result<(), anyhow::Error> {
        // Register for the confirmation *before* asking, so the splice cannot
        // complete in the gap and leave us waiting for a signal already sent.
        let confirmed = self.switched.clone();
        let wait = confirmed.as_ref().map(|n| n.notified());
        tokio::pin!(wait);
        if let Some(w) = wait.as_mut().as_pin_mut() {
            w.enable();
        }

        if let Some(tx) = &self.switch_tx {
            let _ = tx.send(());
        }

        // Wait for the transport to report the socket spliced. No deadline.
        //
        // There was a 10s bound here and it was actively harmful: the transport
        // is not stuck during this wait, it is finishing BLE writes that were
        // already queued - a 32 KB frame at 6 KB/s is about five seconds, so ten
        // is entirely ordinary. When the bound expired we carried on as though
        // upgraded and started writing 512 KB chunks into a bridge still
        // draining over BLE. Measured: we declared the switch at 19:23:42, the
        // transport actually spliced at 19:23:43, and the transfer died of a
        // broken pipe a minute later.
        //
        // Nothing can hang here indefinitely. Every Weave write is bounded, so a
        // dead link ends the pump, which drops the duplex and fails this
        // transfer with a real error rather than a silent corruption.
        //
        // Cancel is the one thing that ends the wait early: the user pressing it
        // during the switch used to be ignored until the splice finished.
        if let Some(mut w) = wait.as_pin_mut() {
            loop {
                tokio::select! {
                    _ = &mut w => break,
                    msg = self.receiver.recv() => match msg {
                        Ok(msg) if self.is_cancel_for_us(&msg) => return self.cancel_by_user().await,
                        _ => {}
                    },
                }
            }
        }

        self.upgraded = true;
        self.unencrypted = self.upgrade_unencrypted;
        // Back to large chunks. 32 KB was chosen so a BLE write couldn't
        // monopolise the loop for 25s; on WiFi that small size caps us at
        // ~40 KB/s, because the per-chunk fixed cost (encrypt, UI update,
        // keepalive service) dominates - roughly one chunk a second regardless
        // of link speed. A 512 KB chunk amortises that overhead ~16x.
        self.chunk_size = 512 * 1024;
        info!(
            "Bandwidth upgrade: switched to the new medium ({}); raising chunk size",
            if self.unencrypted {
                "unencrypted, as the peer offered"
            } else {
                "encrypted"
            }
        );
        Ok(())
    }

    /// Tell the peer which upgrade mediums we support.
    ///
    /// WIFI_HOTSPOT first: over BLE the peer has no network, so the only medium
    /// either side can bring up is an access point. Whether *it* hosts one and
    /// we join, or it takes the one we host, both need this in the negotiated
    /// set - and without an answer here the set collapses to WIFI_LAN, which a
    /// phone with WiFi off cannot host.
    async fn send_supported_mediums(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_retry_frame::Medium;
        use crate::location_nearby_connections::BandwidthUpgradeRetryFrame;

        let mediums = if self.chunk_size < 512 * 1024 {
            vec![Medium::WifiHotspot.into(), Medium::WifiLan.into()]
        } else {
            vec![Medium::WifiLan.into()]
        };

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeRetry.into(),
                ),
                bandwidth_upgrade_retry: Some(BandwidthUpgradeRetryFrame {
                    supported_medium: mediums,
                    is_request: Some(false),
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&frame).await?;
        info!("Bandwidth upgrade: replied with our supported mediums");
        Ok(())
    }

    /// Tell the peer the prior (BLE) channel may be closed. The last thing that
    /// goes over it.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn send_safe_to_close_prior_channel(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType;
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                        .into(),
                ),
                bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::SafeToClosePriorChannel.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&frame).await?;
        info!("Bandwidth upgrade: sent SAFE_TO_CLOSE_PRIOR_CHANNEL");
        Ok(())
    }

    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn send_upgrade_path_request(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType;
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                        .into(),
                ),
                bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::UpgradePathRequest.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.encrypt_and_send(&frame).await?;
        info!("Bandwidth upgrade: asked the peer for an upgrade path");
        Ok(())
    }

    /// Answer a KeepAlive.
    async fn send_keepalive(&mut self) -> Result<(), anyhow::Error> {
        let ack_frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(true) }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&ack_frame).await
        } else {
            self.send_frame(ack_frame.encode_to_vec()).await
        }
    }

    async fn send_frame(&mut self, data: Vec<u8>) -> Result<(), anyhow::Error> {
        let length = data.len();

        // Prepare length prefix in big-endian format
        let length_bytes = [
            (length >> 24) as u8,
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
        ];

        let mut prefixed_length = Vec::with_capacity(length + 4);
        prefixed_length.extend_from_slice(&length_bytes);
        prefixed_length.extend_from_slice(&data);

        // Every frame that leaves this side, encrypted or not, at the point it
        // hits the transport. Pairs with the peer's millisecond timestamps: the
        // question that keeps going unanswered is which of our frames arrives
        // on which side of the peer's switch to an encrypted channel, and that
        // cannot be settled without knowing when each one went out.
        trace!(
            "send_frame: {length} B on the wire beginning {:02x?}",
            &data[..data.len().min(12)]
        );

        self.socket.write_all(&prefixed_length).await?;
        self.socket.flush().await?;

        Ok(())
    }

    async fn get_server_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.server_seq += 1;
            },
            false,
        )
        .await;

        self.state.server_seq
    }

    async fn get_client_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.client_seq += 1;
            },
            false,
        )
        .await;

        self.state.client_seq
    }

    /// This transfer as the front end sees it.
    fn state_message(&self) -> ChannelMessage {
        ChannelMessage {
            id: self.state.id.clone(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(crate::channel::TransferType::Outbound),
            state: Some(self.state.state.clone()),
            meta: self.state.transfer_metadata.clone(),
            ..Default::default()
        }
    }

    fn publish_progress(&self) {
        crate::channel::publish_progress(self.state_message());
    }

    async fn update_state<F>(&mut self, f: F, inform: bool)
    where
        F: FnOnce(&mut InnerState),
    {
        f(&mut self.state);

        if !inform {
            return;
        }

        let _ = self.sender.send(self.state_message());
        if matches!(
            self.state.state,
            State::Finished | State::Cancelled | State::Rejected | State::Disconnected
        ) {
            crate::channel::clear_progress(&self.state.id);
        }
        // Let the receivers run before the next update. This was a 10us sleep,
        // which tokio rounds up to a whole millisecond per update; yielding
        // hands the turn over without a timer.
        tokio::task::yield_now().await;
    }
}

#[cfg(all(test, feature = "experimental", target_os = "windows"))]
mod tests {
    use super::*;
    use crate::utils::DeviceType;

    /// A decline on the receiver is Rejected, which the UI shows as "Transfer
    /// rejected"; the other refusals are failures and stay Disconnected.
    #[tokio::test]
    async fn a_decline_is_rejected_and_other_refusals_are_failures() {
        use crate::sharing_nearby::connection_response_frame::Status;

        for (status, expected) in [
            (Status::Reject, State::Rejected),
            (Status::NotEnoughSpace, State::Disconnected),
            (Status::UnsupportedAttachmentType, State::Disconnected),
            (Status::TimedOut, State::Disconnected),
        ] {
            let (ours, _theirs) = tokio::io::duplex(64 * 1024);
            let mut or = OutboundRequest::new(
                *b"abcd",
                ours,
                "send-1".to_owned(),
                tokio::sync::broadcast::channel(16).0,
                OutboundPayload::Text("hello".to_owned()),
                RemoteDeviceInfo {
                    name: "peer".to_owned(),
                    device_type: DeviceType::Laptop,
                },
            );
            or.state.state = State::SentIntroduction;

            let answer = sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
                connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                    status: Some(status.into()),
                }),
                ..Default::default()
            };
            let ended = or.process_consent(&answer).await;
            assert!(matches!(
                ended
                    .expect_err("a refusal ends the transfer")
                    .downcast_ref(),
                Some(crate::errors::AppError::NotAnError)
            ));
            assert_eq!(or.state.state, expected, "for {status:?}");
        }
    }

    /// Waiting for the peer to release the old channel used to ignore Cancel
    /// until a frame arrived. With no frame coming at all, Cancel has to end
    /// the wait on its own.
    #[tokio::test]
    async fn cancel_ends_the_wait_for_the_medium_switch() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let ui = tokio::sync::broadcast::channel(16).0;
        let mut or = OutboundRequest::new(
            *b"abcd",
            ours,
            "send-1".to_owned(),
            ui.clone(),
            OutboundPayload::Text("hello".to_owned()),
            RemoteDeviceInfo {
                name: "peer".to_owned(),
                device_type: DeviceType::Laptop,
            },
        );

        // Someone else's cancel must not stop this transfer.
        let cancel = |id: &str| ChannelMessage {
            id: id.to_owned(),
            direction: ChannelDirection::FrontToLib,
            action: Some(ChannelAction::CancelTransfer),
            ..Default::default()
        };
        ui.send(cancel("send-2")).unwrap();
        ui.send(cancel("send-1")).unwrap();

        // The timeout only turns a hang into a failure.
        let ended = tokio::time::timeout(std::time::Duration::from_secs(10), or.await_next_frame())
            .await
            .expect("Cancel did not end the wait");
        let err = ended.expect_err("the wait should end the transfer");
        assert!(matches!(
            err.downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        ));
        assert_eq!(or.state.state, State::Cancelled);
        drop(theirs);
    }

    fn sender_with_everything_sent() -> (
        OutboundRequest<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let mut or = OutboundRequest::new(
            *b"abcd",
            ours,
            "send-1".to_owned(),
            tokio::sync::broadcast::channel(16).0,
            OutboundPayload::Text("hello".to_owned()),
            RemoteDeviceInfo {
                name: "peer".to_owned(),
                device_type: DeviceType::Laptop,
            },
        );
        or.state.state = State::SendingFiles;
        or.await_peer_hang_up();
        (or, theirs)
    }

    /// Writing the last byte is not delivery. The receiver closing the
    /// connection afterwards is what confirms it, and only that is Finished.
    #[tokio::test]
    async fn the_receivers_close_after_the_last_byte_is_finished() {
        let (mut or, theirs) = sender_with_everything_sent();
        drop(theirs);

        // The timeout only turns a hang into a failure.
        let ended = tokio::time::timeout(std::time::Duration::from_secs(10), or.handle())
            .await
            .expect("the close was not read");
        assert!(matches!(
            ended
                .expect_err("the close should end the session")
                .downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        ));
        assert_eq!(or.state.state, State::Finished);
    }

    /// A receiver that never confirms did not get the data: a phone that
    /// discarded every frame after an upgrade behaved exactly like this.
    #[tokio::test]
    async fn no_confirmation_after_the_last_byte_is_not_finished() {
        let (mut or, _theirs) = sender_with_everything_sent();
        // Skip the grace period rather than wait it out.
        or.hang_up_deadline = Some(tokio::time::Instant::now());

        let ended = tokio::time::timeout(std::time::Duration::from_secs(10), or.handle())
            .await
            .expect("the grace period did not end the session");
        assert!(ended.is_err());
        assert_eq!(or.state.state, State::Disconnected);
    }

    /// A peer that hangs up in the middle of the handshake ends the transfer
    /// as Disconnected. It used to end it with the state untouched, so the UI
    /// kept showing the last handshake step forever.
    #[tokio::test]
    async fn hang_up_during_the_handshake_reports_disconnected() {
        use tokio::io::AsyncWriteExt;

        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        let ui = tokio::sync::broadcast::channel(16).0;
        let mut or = OutboundRequest::new(
            *b"abcd",
            ours,
            "send-1".to_owned(),
            ui,
            OutboundPayload::Text("hello".to_owned()),
            RemoteDeviceInfo {
                name: "peer".to_owned(),
                device_type: DeviceType::Laptop,
            },
        );
        or.state.state = State::SentConnectionResponse;

        // What a phone sends to hang up: a plaintext DISCONNECTION, length
        // prefixed like every frame.
        let bye = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(location_nearby_connections::DisconnectionFrame::default()),
                ..Default::default()
            }),
        }
        .encode_to_vec();
        theirs
            .write_all(&(bye.len() as u32).to_be_bytes())
            .await
            .unwrap();
        theirs.write_all(&bye).await.unwrap();

        // The timeout only turns a hang into a failure.
        let ended = tokio::time::timeout(std::time::Duration::from_secs(10), or.await_next_frame())
            .await
            .expect("the hang-up was not read");
        let err = ended.expect_err("a hang-up should end the transfer");
        assert!(matches!(
            err.downcast_ref(),
            Some(crate::errors::AppError::NotAnError)
        ));
        assert_eq!(or.state.state, State::Disconnected);
    }
}
