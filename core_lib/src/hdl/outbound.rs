use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use libaes::{Cipher, AES_256_KEY_LEN};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{PublicKey, Sec1Point};
use prost::Message;
use rand::Rng;
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
    derive_d2d_keys, encode_point, gen_ecdsa_keypair, gen_random,
    to_four_digit_string, D2DKeys, DeviceType, RemoteDeviceInfo,
};
use crate::{location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
const SANITY_DURATION: Duration = Duration::from_micros(10);

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
    /// Where the upgrade listener delivers the socket the peer connected on,
    /// and the signal that the old channel has been released. Set by the BLE
    /// send path, which is the only transport that can adopt an upgraded socket
    /// and the only one slow enough to need to.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgrade_tx: Option<tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>>,
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    switch_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// Soft-AP hosted for the upgrade, held so it is torn down with the transfer.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    hotspot: Option<crate::hdl::WindowsHotspot>,
    /// Set once the stream has moved onto an upgraded (WiFi) socket, so the
    /// send loop stops waiting for one and proceeds.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgraded: bool,
    /// How much file data goes into one payload chunk.
    ///
    /// This sets how often the send loop comes up for air, because nothing else
    /// happens while a chunk is being written. At 512 KB over BLE that is ~25s
    /// at 20 KB/s - so keepalives went unanswered until the peer closed at its
    /// 30s timeout, and the cancel button did nothing for the same 25s. Over TCP
    /// a chunk is milliseconds and none of it matters.
    chunk_size: usize,
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
                encryption_done: true,
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
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgrade_tx: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            switch_tx: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            hotspot: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgraded: false,
            chunk_size: 512 * 1024,
        }
    }

    /// Let this request upgrade off a slow transport. Only the BLE send path
    /// can adopt an upgraded socket.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    pub fn set_upgrade_sinks(
        &mut self,
        upgrade_tx: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
        switch_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) {
        self.upgrade_tx = Some(upgrade_tx);
        self.switch_tx = Some(switch_tx);
    }

    /// Use smaller payload chunks, for a transport where writing one takes long
    /// enough to matter. See `chunk_size`.
    pub fn set_chunk_size(&mut self, chunk_size: usize) {
        info!("Using {chunk_size} B payload chunks");
        self.chunk_size = chunk_size;
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        tokio::select! {
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
                    Some(frame_data) => self._handle(frame_data).await?,
                    None => return Err(anyhow!(std::io::Error::from(
                        std::io::ErrorKind::UnexpectedEof
                    ))),
                }
            }
        }

        Ok(())
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
            if channel_msg.direction == ChannelDirection::LibToFront
                || channel_msg.id != self.state.id
            {
                continue;
            }
            if let Some(ChannelAction::CancelTransfer) = channel_msg.action {
                info!("Cancelling the transfer at the user's request");
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
        }

        loop {
            match self.frames.try_recv() {
                // Boxed to break a type-level cycle: this calls `_handle`,
                // which can reach `process_consent`, which contains the send
                // loop that calls back here. At runtime the frames arriving
                // mid-send are keepalives and acks rather than consent frames,
                // so it does not actually re-enter - but the compiler cannot
                // know that and needs the indirection to size the future.
                Ok(frame_data) => Box::pin(self._handle(frame_data)).await?,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(anyhow!(std::io::Error::from(
                        std::io::ErrorKind::UnexpectedEof
                    )))
                }
            }
        }
    }

    pub async fn _handle(&mut self, frame_data: Vec<u8>) -> Result<(), anyhow::Error> {
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

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::SentPairedKeyEncryption;
                        e.server_init_data = Some(frame_data);
                        e.encryption_done = true;
                    },
                    false,
                )
                .await;
            }
            _ => {
                debug!("Handling SecureMessage frame");
                let smsg = SecureMessage::decode(&*frame_data)?;
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
                    endpoint_name: Some(hostname::get()?.to_string_lossy().into_owned().into_bytes()),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: hostname::get()?.to_string_lossy().into_owned(),
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
                    mediums: if self.chunk_size < 512 * 1024 {
                        vec![Medium::WifiHotspot.into(), Medium::WifiLan.into()]
                    } else {
                        vec![Medium::WifiLan.into()]
                    },
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
                e.public_key = Some(public_key);
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
            return Err(anyhow!(format!("Unexpected None connection_response",)));
        }

        if v1_frame.connection_response.as_ref().unwrap().response() != ResponseStatus::Accept {
            return Err(anyhow!(format!("Connection rejected by third party",)));
        }

        let paired_encryption = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::PairedKeyEncryption.into()),
                paired_key_encryption: Some(sharing_nearby::PairedKeyEncryptionFrame {
                    secret_id_hash: Some(gen_random(6)),
                    signed_data: Some(gen_random(72)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&paired_encryption).await?;

        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let mut hmac = HmacSha256::new_from_slice(self.state.recv_hmac_key.as_ref().unwrap())?;
        hmac.update(&smsg.header_and_body);
        let computed = hmac.finalize().into_bytes();
        if computed[..] != smsg.signature[..] {
            return Err(anyhow!("hmac!=signature"));
        }

        let header_and_body = HeaderAndBody::decode(&*smsg.header_and_body)?;

        let msg_data = header_and_body.body;
        let key = self.state.decrypt_key.as_ref().unwrap();

        let mut cipher = Cipher::new_256(key[..AES_256_KEY_LEN].try_into()?);
        cipher.set_auto_padding(true);
        let decrypted = cipher.cbc_decrypt(header_and_body.header.iv(), &msg_data);

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

                        if header.total_size() > SANE_FRAME_LENGTH.into() {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!(
                                "Payload too large: {} bytes",
                                header.total_size()
                            ));
                        }

                        self.state
                            .payload_buffers
                            .entry(payload_id)
                            .or_insert_with(|| Vec::with_capacity(header.total_size() as usize));

                        // Get the current length of the buffer, if it exists, without holding a mutable borrow.
                        let buffer_len = self.state.payload_buffers.get(&payload_id).unwrap().len();
                        if chunk.offset() != buffer_len as i64 {
                            self.state.payload_buffers.remove(&payload_id);
                            return Err(anyhow!(
                                "Unexpected chunk offset: {}, expected: {}",
                                chunk.offset(),
                                buffer_len
                            ));
                        }

                        let buffer = self.state.payload_buffers.get_mut(&payload_id).unwrap();
                        if let Some(body) = &chunk.body {
                            buffer.extend(body);
                        }

                        if (chunk.flags() & 1) == 1 {
                            debug!("Chunk flags & 1 == 1 ?? End of data ??");

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
                self.send_keepalive(true).await?;
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

                // The peer is hosting a medium and has given us credentials.
                //
                // This is the direction that actually works: with WiFi off the
                // phone still brings up its own AP (192.168.49.1, the standard
                // Android P2P gateway) and offers it - it just needed to see
                // WIFI_HOTSPOT in the negotiated set, which it did not until we
                // started answering BandwidthUpgradeRetry. So we join rather
                // than host.
                #[cfg(all(feature = "experimental", target_os = "windows"))]
                if let Some(creds) = v1_frame
                    .bandwidth_upgrade_negotiation
                    .as_ref()
                    .filter(|bun| {
                        bun.event_type()
                            == location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::UpgradePathAvailable
                    })
                    .and_then(|bun| bun.upgrade_path_info.as_ref())
                    .and_then(|info| info.wifi_hotspot_credentials.clone())
                {
                    if let Err(e) = self.join_peer_hotspot(creds).await {
                        warn!("Bandwidth upgrade: could not take the peer's medium: {e}");
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
                    if is_last_write {
                        // Only reachable if we host, which as the sender we no
                        // longer do. Kept because the frame is legal and
                        // answering it costs nothing.
                        info!("Bandwidth upgrade: LAST_WRITE_TO_PRIOR_CHANNEL; releasing the old channel");
                        if let Err(e) = self.send_safe_to_close_prior_channel().await {
                            warn!("send_safe_to_close_prior_channel failed: {e}");
                        }
                        if let Some(tx) = &self.switch_tx {
                            let _ = tx.send(());
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
                    if is_safe_to_close {
                        info!("Bandwidth upgrade: peer released the prior channel; switching");
                        if let Some(tx) = &self.switch_tx {
                            let _ = tx.send(());
                        }
                        // The send loop's head-start window can now proceed, over
                        // the new medium.
                        self.upgraded = true;
                        // Back to large chunks. 32 KB was chosen so a BLE write
                        // couldn't monopolise the loop for 25s; on WiFi that
                        // small size caps us at ~40 KB/s, because the per-chunk
                        // fixed cost (encrypt, UI update, keepalive service)
                        // dominates - roughly one chunk a second regardless of
                        // link speed. A 512 KB chunk amortises that overhead
                        // ~16x.
                        self.chunk_size = 512 * 1024;
                        info!("Bandwidth upgrade: raised chunk size to 512 KB for the fast medium");
                    }
                }
            }
            location_nearby_connections::v1_frame::FrameType::Disconnection => {
                // The remote device closed the session (normal after a transfer
                // completes). End cleanly rather than logging it as an error.
                info!("Received disconnect frame, ending session");
                return Err(anyhow!(crate::errors::AppError::NotAnError));
            }
            _ => {
                error!("Unhandled offline frame encrypted: {:?}", offline);
            }
        }

        Ok(())
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
            State::SentPairedKeyEncryption => {
                debug!("Processing State::SentPairedKeyEncryption");
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
                    let path = Path::new(f);
                    if !path.is_file() {
                        warn!("Path is not a file: {}", f);
                        continue;
                    }

                    let file = match File::open(f) {
                        Ok(_f) => _f,
                        Err(e) => {
                            error!("Failed to open file: {f}: {:?}", e);
                            continue;
                        }
                    };
                    let fmetadata = match file.metadata() {
                        Ok(_fm) => _fm,
                        Err(e) => {
                            error!("Failed to get metadata for: {f}: {:?}", e);
                            continue;
                        }
                    };

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
                        payload_id: Some(rand::rng().random::<i64>()),
                        name: Some(fname.to_os_string().into_string().unwrap()),
                        size: Some(fmetadata.len() as i64),
                        mime_type: Some(ftype),
                        r#type: Some(meta_type.into()),
                        ..Default::default()
                    };
                    transferred_files.insert(
                        fmeta.payload_id(),
                        InternalFileInfo {
                            payload_id: fmeta.payload_id(),
                            file_url: path.to_path_buf(),
                            bytes_transferred: 0,
                            total_size: fmeta.size(),
                            file: Some(file),
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
                let ttype = if trimmed.starts_with("http://") || trimmed.starts_with("https://")
                {
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
                    self.update_state(
                        |e| {
                            if let Some(tmd) = e.transfer_metadata.as_mut() {
                                tmd.ack_bytes = tmd.total_bytes;
                            }
                            e.state = State::Finished;
                        },
                        true,
                    )
                    .await;
                    self.disconnection().await?;
                    return Ok(());
                }

                // Give a bandwidth upgrade a head start before flooding the
                // link with file data.
                //
                // Over BLE the upgrade is several round trips - retry, our
                // supported-mediums reply, the peer's offer, join, introduce -
                // and every one competes with our data chunks for a ~20 KB/s
                // link. Blasting a 158 MB file from offset 0 starved the
                // negotiation so it never happened, and the whole transfer
                // crawled over BLE. Wait a bounded window here, servicing
                // inbound frames (which is what drives the upgrade and honours a
                // cancel), and start sending only once the medium has switched
                // or the window elapses. Transparent either way: the switch
                // swaps the socket under the same duplex, so the file loop below
                // runs over WiFi without knowing.
                #[cfg(all(feature = "experimental", target_os = "windows"))]
                if self.upgrade_tx.is_some() {
                    info!("Giving a bandwidth upgrade a head start before sending file data");
                    // Drive it, don't just wait for it.
                    //
                    // The phone offers a path only after we answer its
                    // BandwidthUpgradeRetry, and that exchange does not always
                    // finish on its own within the window - some runs upgraded in
                    // seconds, others never offered and fell to BLE at 3 KB/s.
                    // Prompt it: announce our mediums and ask for a path up front,
                    // then re-ask periodically until it offers or the window ends.
                    let _ = self.send_supported_mediums().await;
                    let _ = self.send_upgrade_path_request().await;

                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(30);
                    let mut last_nudge = std::time::Instant::now();
                    while std::time::Instant::now() < deadline && !self.upgraded {
                        self.service_pending_frames().await?;
                        if last_nudge.elapsed() >= std::time::Duration::from_secs(6) {
                            let _ = self.send_upgrade_path_request().await;
                            last_nudge = std::time::Instant::now();
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    if self.upgraded {
                        info!("Bandwidth upgrade completed; sending file data over the new medium");
                    } else {
                        info!("No upgrade within the window; sending file data over BLE");
                    }
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
                            info!("All files have been transferred");
                            self.update_state(
                                |e| {
                                    e.state = State::Finished;
                                },
                                true,
                            )
                            .await;
                            self.disconnection().await?;
                            // Breaking instead of NotAnError to allow peacefull termination
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
                    #[cfg(all(feature = "experimental", target_os = "windows"))]
                    let want_upgrade = self.chunk_size < 512 * 1024 && !self.upgraded;
                    #[cfg(not(all(feature = "experimental", target_os = "windows")))]
                    let want_upgrade = self.chunk_size < 512 * 1024;
                    if want_upgrade {
                        // Ask, then offer. The request is free and the peer has
                        // never answered one, but if it ever does we would
                        // rather take its medium than host our own.
                        if let Err(e) = self.send_upgrade_path_request().await {
                            warn!("send_upgrade_path_request failed: {e}");
                        }
                        // Deliberately no hotspot of our own here.
                        //
                        // The *receiver* hosts the upgrade medium and the sender
                        // joins it - that is Nearby's model, the receiver being
                        // the connection's server. Confirmed both ways: when the
                        // PC receives it hosts at 192.168.137.1 and the phone
                        // joins; when the phone receives it hosts at
                        // 192.168.49.1 and offers us the credentials. Hosting as
                        // the sender was simply the wrong role, which is why the
                        // peer never took it - it had already decided it was
                        // hosting. See `join_peer_hotspot`, which takes its
                        // offer instead. RQS_TRY_SEND_UPGRADE=1 restores the old
                        // behaviour for experiments.
                        if std::env::var("RQS_TRY_SEND_UPGRADE").is_ok() {
                            if let Err(e) = self.offer_hotspot_upgrade().await {
                                warn!("offer_hotspot_upgrade failed: {e}");
                            }
                        }
                    }

                    // Loop until we reached end of file
                    loop {
                        // Answer anything the peer has sent - KeepAlive above
                        // all - before spending another chunk's worth of time
                        // writing. Without this the peer hears nothing for the
                        // whole transfer and closes at its 30s timeout.
                        self.service_pending_frames().await?;

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

                            let mut buffer = vec![0u8; self.chunk_size];
                            let bytes_read = curr_state.file.as_ref().unwrap().read(&mut buffer)?;

                            (
                                InternalFileInfo {
                                    payload_id: curr_state.payload_id,
                                    file_url: curr_state.file_url.clone(),
                                    bytes_transferred: curr_state.bytes_transferred,
                                    total_size: curr_state.total_size,
                                    file: None,
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
                        self.update_state(
                            |e| {
                                if let Some(mu) = e.transferred_files.get_mut(&current) {
                                    mu.bytes_transferred += bytes_read as i64;
                                }

                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                    tmd.ack_bytes += bytes_read as u64;
                                }
                            },
                            true,
                        )
                        .await;

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
                warn!(
                    "Cannot process: consent denied: {:?}",
                    v1_frame.connection_response.as_ref().unwrap().status()
                );
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
        // Ensure no more than 32 bytes for the keys
        if peer_p256_key.x.len() > 32 {
            bytes.extend_from_slice(&peer_p256_key.x[peer_p256_key.x.len() - 32..]);
        } else {
            bytes.extend_from_slice(&peer_p256_key.x);
        }
        if peer_p256_key.y.len() > 32 {
            bytes.extend_from_slice(&peer_p256_key.y[peer_p256_key.y.len() - 32..]);
        } else {
            bytes.extend_from_slice(&peer_p256_key.y);
        }

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
        let d2d_msg = DeviceToDeviceMessage {
            sequence_number: Some(self.get_server_seq_inc().await),
            message: Some(frame.encode_to_vec()),
        };

        let key = self.state.encrypt_key.as_ref().unwrap();
        let msg_data = d2d_msg.encode_to_vec();
        let iv = gen_random(16);

        let mut cipher = Cipher::new_256(&key[..AES_256_KEY_LEN].try_into().unwrap());
        cipher.set_auto_padding(true);
        let encrypted = cipher.cbc_encrypt(&iv, &msg_data);

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

    /// Ask the peer to offer us a faster medium.
    ///
    /// Sending is stuck at BLE speed because the upgrade is the server's to
    /// initiate, and when we send, the *phone* is the server - it offers
    /// nothing, and a full WiFi-off transfer contains no UPGRADE_PATH_AVAILABLE
    /// at all. UPGRADE_PATH_REQUEST is the frame that asks it to.
    ///
    /// Whatever comes back is logged by the BandwidthUpgradeNegotiation arm.
    /// Acting on it is the next piece of work and a different shape from the
    /// receive path: the peer would host the medium and we would have to *join*
    /// it, which on Windows means WlanConnect rather than tethering.
    /// Host a soft-AP and offer it, so a send can leave BLE.
    ///
    /// We host rather than join because the peer will not offer anything: asked
    /// directly with UPGRADE_PATH_REQUEST while receiving, it answers nothing,
    /// and it never offers unprompted. The upgrade "server" is whoever hosts the
    /// medium, not whoever sends the file, so hosting puts us in the same role
    /// the receive path already handles - the peer connects, introduces itself,
    /// sends LAST_WRITE, and we answer SAFE_TO_CLOSE.
    ///
    /// A hotspot rather than our LAN address: reaching this peer over BLE at all
    /// means it had no network to be found on.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn offer_hotspot_upgrade(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
            upgrade_path_info::{Medium, WifiHotspotCredentials},
            EventType, UpgradePathInfo,
        };
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        let Some(upgrade_tx) = self.upgrade_tx.clone() else {
            return Ok(());
        };

        let port: u16 = 8899;
        let suffix: String = rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(4)
            .map(char::from)
            .collect();
        let ssid = format!("DIRECT-rq{suffix}");
        let passphrase: String = rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(12)
            .map(char::from)
            .collect();

        // On a blocking thread: the WinRT tethering calls take seconds and
        // stalling the executor breaks in-flight frames.
        let (ssid_c, pass_c) = (ssid.clone(), passphrase.clone());
        let (handle, creds) = match tokio::task::spawn_blocking(move || {
            crate::hdl::WindowsHotspot::start(&ssid_c, &pass_c)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                warn!("Bandwidth upgrade: hotspot start failed: {e}");
                return Ok(());
            }
            Err(e) => {
                warn!("Bandwidth upgrade: hotspot task join failed: {e}");
                return Ok(());
            }
        };
        info!(
            "Bandwidth upgrade: hotspot up (ssid={}, gateway={}, port={})",
            creds.ssid, creds.gateway, port
        );
        self.hotspot = Some(handle);

        // Accept before offering: the peer acts on the frame immediately.
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await {
                Ok(l) => {
                    info!("Bandwidth upgrade: listening on 0.0.0.0:{port}");
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(45),
                        l.accept(),
                    )
                    .await
                    {
                        Ok(Ok((s, addr))) => {
                            info!("*** Bandwidth upgrade: peer connected from {addr} ***");
                            match crate::hdl::introduce_upgraded_channel(s).await {
                                Ok(sock) => {
                                    if upgrade_tx.send(sock).is_err() {
                                        warn!("Bandwidth upgrade: nobody left to adopt the socket");
                                    }
                                }
                                Err(e) => warn!("Bandwidth upgrade: introduction failed: {e}"),
                            }
                        }
                        Ok(Err(e)) => warn!("Bandwidth upgrade: accept failed: {e}"),
                        Err(_) => warn!(
                            "Bandwidth upgrade: peer never joined the hotspot; staying on BLE"
                        ),
                    }
                }
                Err(e) => warn!("Bandwidth upgrade: bind failed: {e}"),
            }
        });

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                        .into(),
                ),
                bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::UpgradePathAvailable.into()),
                    upgrade_path_info: Some(UpgradePathInfo {
                        medium: Some(Medium::WifiHotspot.into()),
                        wifi_hotspot_credentials: Some(WifiHotspotCredentials {
                            ssid: Some(creds.ssid.clone()),
                            password: Some(creds.passphrase.clone()),
                            port: Some(port as i32),
                            gateway: Some(creds.gateway.clone()),
                            frequency: Some(-1),
                        }),
                        supports_client_introduction_ack: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.encrypt_and_send(&frame).await?;
        info!(
            "Bandwidth upgrade: offered WIFI_HOTSPOT (ssid={}, gateway={}, port={})",
            creds.ssid, creds.gateway, port
        );
        Ok(())
    }

    #[cfg(not(all(feature = "experimental", target_os = "windows")))]
    async fn offer_hotspot_upgrade(&mut self) -> Result<(), anyhow::Error> {
        Ok(())
    }

    /// Join the AP the peer is hosting, introduce ourselves on it, and hand the
    /// socket to the transport.
    ///
    /// We are the *client* of the new medium here, the reverse of the receive
    /// path: we send CLIENT_INTRODUCTION and wait for the ACK, rather than
    /// answering one. After that we tell the peer we are done with the old
    /// channel and wait for it to release it.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn join_peer_hotspot(
        &mut self,
        creds: crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::WifiHotspotCredentials,
    ) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
            ClientIntroduction, EventType,
        };
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let Some(upgrade_tx) = self.upgrade_tx.clone() else {
            return Ok(());
        };
        let (Some(ssid), Some(password)) = (creds.ssid.clone(), creds.password.clone()) else {
            return Err(anyhow!("peer offered a hotspot with no credentials"));
        };
        let gateway = creds.gateway().to_string();
        let port = creds.port() as u16;

        info!("Bandwidth upgrade: joining the peer's network {ssid}");
        crate::hdl::join(&ssid, &password).await?;

        // Association is not addressing. ConnectAsync returns as soon as the
        // link is up, but DHCP from the peer's AP takes a further moment, and
        // connecting before we hold an address on its subnet has no route to
        // the gateway - it just hangs. Measured: "timed out connecting to
        // 192.168.49.1" two seconds after a successful join.
        if let Ok(gw) = gateway.parse::<std::net::Ipv4Addr>() {
            if crate::hdl::wait_for_route(gw, std::time::Duration::from_secs(20)).await {
                info!("Bandwidth upgrade: hold an address on the peer's subnet");
            } else {
                warn!(
                    "Bandwidth upgrade: no address on the peer's subnet after 20s; trying anyway"
                );
            }
        }
        info!("Bandwidth upgrade: connecting to {gateway}:{port}");

        // Retry: the AP can accept the association a beat before its listener is
        // ready, so a single refused connect is not conclusive.
        let mut socket = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let why = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::net::TcpStream::connect(format!("{gateway}:{port}")),
                )
                .await
                {
                    Ok(Ok(s)) => break s,
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "connect timed out".to_string(),
                };
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "could not reach {gateway}:{port} after joining: {why}"
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        };

        // Introduce ourselves. The peer expects this as the first frame on the
        // upgraded channel and answers with CLIENT_INTRODUCTION_ACK.
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
                        endpoint_id: Some(
                            String::from_utf8_lossy(&self.endpoint_id).to_string(),
                        ),
                        supports_disabling_encryption: Some(false),
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

        // Wait for the ACK before abandoning the old channel.
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            socket.read_exact(&mut len_buf),
        )
        .await
        .map_err(|_| anyhow!("timed out waiting for CLIENT_INTRODUCTION_ACK"))??;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > SANE_FRAME_LENGTH as usize {
            return Err(anyhow!("insane ack frame length {len}"));
        }
        let mut frame = vec![0u8; len];
        socket.read_exact(&mut frame).await?;
        info!("Bandwidth upgrade: got CLIENT_INTRODUCTION_ACK");

        // Hand the socket over; the transport holds it until the old channel is
        // released.
        if upgrade_tx.send(socket).is_err() {
            return Err(anyhow!("transport is gone, cannot adopt the upgraded socket"));
        }

        // Tell the peer we are finished with the old channel. It answers
        // SAFE_TO_CLOSE_PRIOR_CHANNEL, which is what fires the switch.
        let last_write = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                        .into(),
                ),
                bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::LastWriteToPriorChannel.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&last_write).await?;
        info!("Bandwidth upgrade: sent LAST_WRITE_TO_PRIOR_CHANNEL");
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

    async fn send_keepalive(&mut self, ack: bool) -> Result<(), anyhow::Error> {
        let ack_frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(ack) }),
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

    async fn update_state<F>(&mut self, f: F, inform: bool)
    where
        F: FnOnce(&mut InnerState),
    {
        f(&mut self.state);

        if !inform {
            return;
        }

        let _ = self.sender.send(ChannelMessage {
            id: self.state.id.clone(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(crate::channel::TransferType::Outbound),
            state: Some(self.state.state.clone()),
            meta: self.state.transfer_metadata.clone(),
            ..Default::default()
        });
        // Add a small sleep timer to allow the Tokio runtime to have
        // some spare time to process channel's message. Otherwise it
        // get spammed by new requests. Currently set to 10 micro secs.
        tokio::time::sleep(SANITY_DURATION).await;
    }
}
