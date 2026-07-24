use std::fs::File;
use std::time::Duration;

/// Write the entire buffer to `file` at the given byte `offset`, independent
/// of the file cursor. Cross-platform replacement for the Unix-only
/// `std::os::unix::fs::FileExt::write_all_at`.
#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    // `seek_write` is not guaranteed to write the whole buffer in one call,
    // so loop until everything is written (mirrors `write_all_at` semantics).
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

use anyhow::anyhow;
use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{PublicKey, Sec1Point};
use prost::Message;
use rand::Rng;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast::{Receiver, Sender};

use super::{InnerState, State};
use crate::channel::{ChannelAction, ChannelDirection, ChannelMessage};
use crate::hdl::info::{InternalFileInfo, TransferMetadata};
use crate::hdl::{TextPayloadInfo, TextPayloadType};
use crate::location_nearby_connections::payload_transfer_frame::{
    payload_header, PacketType, PayloadChunk, PayloadHeader,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::securegcm::ukey2_alert::AlertType;
use crate::securegcm::{
    ukey2_message, DeviceToDeviceMessage, GcmMetadata, Type, Ukey2Alert, Ukey2ClientFinished,
    Ukey2ClientInit, Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit,
};
use crate::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType,
    SecureMessage, SigScheme,
};
use crate::sharing_nearby::{paired_key_result_frame, text_metadata};
use crate::utils::{
    derive_d2d_keys, encode_point, gen_ecdsa_keypair, gen_random, get_download_dir,
    stream_read_exact, to_four_digit_string, D2DKeys, DeviceType, RemoteDeviceInfo,
};
use crate::{location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;
const SANITY_DURATION: Duration = Duration::from_micros(10);

/// Generic over the transport so the handshake can be driven by a test over an
/// in-memory duplex pair, not just a real socket. In production `S` is always
/// `tokio::net::TcpStream`.
#[derive(Debug)]
pub struct InboundRequest<S> {
    /// Write half only. Reading happens in a separate task - see `frames` and
    /// `frame_reader` for why.
    socket: tokio::io::WriteHalf<S>,
    /// Frames as they came off the wire, still encrypted. Fed by a reader task
    /// so no read is ever cancelled by `select!`.
    frames: tokio::sync::mpsc::Receiver<crate::hdl::RawFrame>,
    pub state: InnerState,
    sender: Sender<ChannelMessage>,
    receiver: Receiver<ChannelMessage>,
    /// Held so the WiFi Direct group lives exactly as long as this transfer and
    /// is torn down with it. The hotspot path instead did `mem::forget`, and
    /// because tethering is *system* state the AP then survived the process and
    /// stayed on across restarts - don't repeat that.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    wifi_direct: Option<crate::hdl::WindowsWifiDirect>,
    /// Credentials read off the group when it started, so the offer frame can be
    /// built later. Set together with `wifi_direct`.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    wifi_direct_creds: Option<crate::hdl::WifiDirectCreds>,
    /// Where to hand the upgraded socket once the phone has introduced itself.
    /// The BLE bridge sets this and then splices the socket into the stream in
    /// place of the Weave transport; unset elsewhere, where there is nothing to
    /// upgrade from.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgrade_tx: Option<tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>>,
    /// Signals the bridge that SAFE_TO_CLOSE_PRIOR_CHANNEL has gone out and the
    /// stream may now move onto the upgraded socket.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    switch_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// When we last answered a KeepAlive, so redundant responses can be skipped.
    /// Not transport-specific, but it matters on BLE: see the KeepAlive arm.
    last_keepalive_response: Option<std::time::Instant>,
    /// When we last broadcast a receive-progress update. A per-chunk broadcast
    /// floods the shared ChannelMessage bus - thousands of messages during a
    /// fast transfer - overflowing its 50-slot buffer so this handler's own
    /// subscription (watching for a UI cancel) lags and logs "channel lagged",
    /// and risks dropping the cancel. Byte counts still advance every chunk; the
    /// broadcast is throttled to this.
    last_progress_broadcast: Option<std::time::Instant>,
    /// Receive-throughput profiling, reset every report. Splits wall-clock time
    /// into decrypt (HMAC + AES + protobuf decode) and disk write, so a slow
    /// transfer shows where the time actually goes rather than us guessing.
    prof_decrypt: std::time::Duration,
    prof_aes: std::time::Duration,
    prof_write: std::time::Duration,
    prof_bytes: u64,
    prof_since: Option<std::time::Instant>,
    /// Soft-AP for the WIFI_HOTSPOT upgrade, held for the life of the transfer
    /// so tethering is torn down with the request rather than left running.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    hotspot: Option<crate::hdl::WindowsHotspot>,
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    hotspot_creds: Option<crate::hdl::HotspotCredentials>,
    /// Whether the shared upgrade listener on 0.0.0.0:8899 is already up. Both
    /// the WIFI_LAN and WIFI_HOTSPOT offers can be sent for one transfer, and a
    /// single listener accepts whichever interface the phone reaches us on - so
    /// the second offer must not try to bind the port again.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    upgrade_listener_started: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> InboundRequest<S> {
    pub fn new(socket: S, id: String, sender: Sender<ChannelMessage>) -> Self {
        let receiver = sender.subscribe();
        // Split so reads happen in their own task. See `frame_reader` - a read
        // raced against the channel in `select!` loses bytes when cancelled.
        let (reader, writer) = tokio::io::split(socket);
        let frames = crate::hdl::spawn_frame_reader(reader, SANE_FRAME_LENGTH as usize);

        Self {
            socket: writer,
            frames,
            state: InnerState {
                id,
                server_seq: 0,
                client_seq: 0,
                state: State::Initial,
                encryption_done: true,
                ..Default::default()
            },
            sender,
            receiver,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            wifi_direct: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            wifi_direct_creds: None,
            last_keepalive_response: None,
            last_progress_broadcast: None,
            prof_decrypt: std::time::Duration::ZERO,
            prof_aes: std::time::Duration::ZERO,
            prof_write: std::time::Duration::ZERO,
            prof_bytes: 0,
            prof_since: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgrade_tx: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            switch_tx: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            hotspot: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            hotspot_creds: None,
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            upgrade_listener_started: false,
        }
    }

    /// Where to deliver the upgraded socket once the peer has introduced itself
    /// on it. Only the BLE bridge can adopt one today.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    pub fn set_upgrade_sink(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
        switch_tx: tokio::sync::mpsc::UnboundedSender<()>,
    ) {
        self.upgrade_tx = Some(tx);
        self.switch_tx = Some(switch_tx);
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

                        debug!("inbound: got: {:?}", channel_msg);
                        match channel_msg.action {
                            Some(ChannelAction::AcceptTransfer) => {
                                self.accept_transfer().await?;
                            },
                            Some(ChannelAction::RejectTransfer) => {
                                self.update_state(
                                    |e| {
                                        e.state = State::Rejected;
                                    },
                                    true,
                                ).await;

                                self.reject_transfer(Some(
                                    sharing_nearby::connection_response_frame::Status::Reject
                                )).await?;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            },
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
                        }
                    }
                    Err(e) => {
                        error!("inbound: channel error: {}", e);
                    }
                }
            },
            // Both branches are channel receives, and both are cancel-safe.
            // Whichever loses simply hasn't consumed its message yet.
            frame = self.frames.recv() => {
                match frame {
                    Some(frame_data) => self._handle(frame_data).await?,
                    // The reader task ended: the transport is finished.
                    None => return Err(anyhow!(std::io::Error::from(
                        std::io::ErrorKind::UnexpectedEof
                    ))),
                }
            }
        }

        Ok(())
    }

    pub async fn _handle(&mut self, frame_data: Vec<u8>) -> Result<(), anyhow::Error> {
        let current_state = &self.state;
        // Now determine what will be the request type based on current state
        match current_state.state {
            State::Initial => {
                debug!("Handling State::Initial frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                let rdi = self.process_connection_request(&frame)?;
                info!("RemoteDeviceInfo: {rdi:?}");

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::ReceivedConnectionRequest;
                        e.remote_device_info = Some(rdi);
                    },
                    false,
                )
                .await;
            }
            State::ReceivedConnectionRequest => {
                debug!("Handling State::ReceivedConnectionRequest frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.process_ukey2_client_init(&msg).await?;

                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::SentUkeyServerInit;
                        e.client_init_msg_data = Some(frame_data);
                    },
                    false,
                )
                .await;
            }
            State::SentUkeyServerInit => {
                debug!("Handling State::SentUkeyServerInit frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.process_ukey2_client_finish(&msg, &frame_data).await?;

                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::ReceivedUkeyClientFinish;
                    },
                    false,
                )
                .await;
            }
            State::ReceivedUkeyClientFinish => {
                debug!("Handling State::ReceivedUkeyClientFinish frame");
                let frame = location_nearby_connections::OfflineFrame::decode(&*frame_data)?;
                self.process_connection_response(&frame).await?;

                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::SentConnectionResponse;
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

    fn process_connection_request(
        &mut self,
        frame: &location_nearby_connections::OfflineFrame,
    ) -> Result<RemoteDeviceInfo, anyhow::Error> {
        let v1_frame = frame
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        if v1_frame.r#type() != location_nearby_connections::v1_frame::FrameType::ConnectionRequest
        {
            return Err(anyhow!(format!(
                "Unexpected frame type: {:?}",
                v1_frame.r#type()
            )));
        }

        let connection_request = v1_frame
            .connection_request
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        // Per grishka's PROTOCOL.md the client states the mediums it will accept
        // here, and the server is meant to intersect them with its own. If this
        // list is just [WIFI_LAN] then a WIFI_HOTSPOT upgrade was never in the
        // agreed set, and every offer we've made was outside the negotiation.
        // Medium: 1=MDNS 2=BLUETOOTH 3=WIFI_HOTSPOT 4=BLE 5=WIFI_LAN
        //         6=WIFI_AWARE 7=NFC 8=WIFI_DIRECT 9=WEB_RTC 10=BLE_L2CAP 11=USB
        info!(
            "ConnectionRequest: mediums (raw) = {:?}",
            connection_request.mediums
        );

        // The phone states, unprompted, which channels it can actually use as a
        // WiFi Direct *client* - and the frequency of the AP it's sitting on.
        // That's ground truth for whether our group owner is even reachable:
        // a single-radio phone can only follow us to a channel it lists here,
        // and `ap_frequency` tells us the channel it's already committed to.
        // Every previous guess about the radio came from our side of the link;
        // this comes from the phone's.
        if let Some(meta) = &connection_request.medium_metadata {
            info!(
                "ConnectionRequest: medium_metadata: ap_frequency={:?} supports_5ghz={:?} \
                 supports_6ghz={:?} wifi_direct_cli_usable_channels={:?} available_channels={:?}",
                meta.ap_frequency,
                meta.supports_5_ghz,
                meta.supports_6_ghz,
                meta.wifi_direct_cli_usable_channels
                    .as_ref()
                    .map(|c| &c.channels),
                meta.available_channels.as_ref().map(|c| &c.channels),
            );
            // The decisive field, and we had it all along without reading it.
            // 1 = WIFI_DIRECT_WITH_PASSWORD (ssid/password - "Android supports
            // this type, but Windows does not"), 3 = WIFI_DIRECT_WITH_DEVICE_NAME.
            // The phone rejected our device_name-only offer with "missing ssid or
            // not in correct format", so either it doesn't list 3 here - in which
            // case this peer cannot do the only type Windows can host - or it
            // does, and we are failing to signal that we're using it.
            info!(
                "ConnectionRequest: supported_wifi_direct_auth_types={:?} medium_role={:?}",
                meta.supported_wifi_direct_auth_types, meta.medium_role,
            );
        } else {
            info!("ConnectionRequest: no medium_metadata sent");
        }

        let endpoint_info = connection_request
            .endpoint_info
            .as_ref()
            .ok_or_else(|| anyhow!("Missing endpoint info"))?;

        // Check if endpoint info length is greater than 17
        if endpoint_info.len() <= 17 {
            return Err(anyhow!("Endpoint info too short"));
        }

        let device_name_length = endpoint_info[17] as usize;
        // Validate length including device name
        if endpoint_info.len() < device_name_length + 18 {
            return Err(anyhow!(
                "Endpoint info too short to contain the device name"
            ));
        }

        // Extract and validate device name based on length
        let device_name = std::str::from_utf8(&endpoint_info[18..(18 + device_name_length)])
            .map_err(|_| anyhow!("Device name is not valid UTF-8"))?;

        // Parsing the device type
        let raw_device_type = (endpoint_info[0] & 7) >> 1_usize;

        Ok(RemoteDeviceInfo {
            name: device_name.to_string(),
            device_type: DeviceType::from_raw_value(raw_device_type),
        })
    }

    async fn process_ukey2_client_init(&mut self, msg: &Ukey2Message) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ClientInit {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ClientInit",
                msg.message_type
            ));
        }

        let client_init = match Ukey2ClientInit::decode(msg.message_data()) {
            Ok(uk2ci) => uk2ci,
            Err(e) => {
                self.send_ukey2_alert(AlertType::BadMessageData).await?;
                return Err(anyhow!("UKey2: Ukey2ClientInit::decode: {}", e));
            }
        };

        if client_init.version() != 1 {
            self.send_ukey2_alert(AlertType::BadVersion).await?;
            return Err(anyhow!("UKey2: client_init.version != 1"));
        }

        if client_init.random().len() != 32 {
            self.send_ukey2_alert(AlertType::BadRandom).await?;
            return Err(anyhow!("UKey2: client_init.random.len != 32"));
        }

        // Searching for preferred cipher commitment
        let mut found = false;
        for commitment in &client_init.cipher_commitments {
            trace!("CipherCommitment: {:?}", commitment.handshake_cipher());
            if Ukey2HandshakeCipher::P256Sha512 == commitment.handshake_cipher() {
                found = true;
                self.update_state(
                    |e| {
                        e.cipher_commitment = Some(commitment.clone());
                    },
                    false,
                )
                .await;
                break;
            }
        }

        if !found {
            self.send_ukey2_alert(AlertType::BadHandshakeCipher).await?;
            return Err(anyhow!("UKey2: badHandshakeCipher"));
        }

        if client_init.next_protocol() != "AES_256_CBC-HMAC_SHA256" {
            self.send_ukey2_alert(AlertType::BadNextProtocol).await?;
            return Err(anyhow!(
                "UKey2: badNextProtocol: {}",
                client_init.next_protocol()
            ));
        }

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

        let server_init = Ukey2ServerInit {
            version: Some(1),
            random: Some(rand::rng().random::<[u8; 32]>().to_vec()),
            handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512.into()),
            public_key: Some(pkey.encode_to_vec()),
        };

        let server_init_msg = Ukey2Message {
            message_type: Some(ukey2_message::Type::ServerInit.into()),
            message_data: Some(server_init.encode_to_vec()),
        };

        let server_init_data = server_init_msg.encode_to_vec();
        self.update_state(
            |e| {
                e.private_key = Some(secret_key);
                e.public_key = Some(public_key);
                e.server_init_data = Some(server_init_data.clone());
            },
            false,
        )
        .await;

        self.send_frame(server_init_data).await?;

        Ok(())
    }

    async fn process_ukey2_client_finish(
        &mut self,
        msg: &Ukey2Message,
        frame_data: &Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ClientFinish {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ClientFinish",
                msg.message_type
            ));
        }

        let sha512 = Sha512::digest(frame_data);
        if self.state.cipher_commitment.as_ref().unwrap().commitment() != &sha512[..] {
            error!("cipher_commitment isn't equals to sha512(frame_data)");
            return Err(anyhow!("UKey2: cipher_commitment != sha512"));
        }

        let client_finish = match Ukey2ClientFinished::decode(msg.message_data()) {
            Ok(uk2cf) => uk2cf,
            Err(e) => {
                return Err(anyhow!("UKey2: Ukey2ClientFinished::decode: {}", e));
            }
        };

        if client_finish.public_key.is_none() {
            return Err(anyhow!("UKey2: client_finish.public_key None"));
        }

        let client_public_key = match GenericPublicKey::decode(client_finish.public_key()) {
            Ok(cpk) => cpk,
            Err(e) => {
                return Err(anyhow!("UKey2: GenericPublicKey::decode: {}", e));
            }
        };

        self.finalize_key_exchange(client_public_key).await?;

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

        // Announce the WiFi Direct device name up front, so the peer knows what
        // to look for if it later asks for a WIFI_DIRECT upgrade. That flow is
        // device-name discovery (the only type Windows can host), and this field
        // is how the group owner declares its name - google's Windows client
        // fills it in here, in the connection response, long before any upgrade
        // is negotiated. Must match what `WindowsWifiDirect::start` advertises:
        // the uppercased computer name.
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        let wifi_direct_device_name = ::hostname::get()
            .ok()
            .map(|h| h.to_string_lossy().to_uppercase());
        #[cfg(not(all(feature = "experimental", target_os = "windows")))]
        let wifi_direct_device_name: Option<String> = None;

        // Report the OS we actually are.
        //
        // This said LINUX unconditionally - inherited from upstream, which is a
        // Linux project. That is not cosmetic: the proto ties WiFi Direct auth
        // type to OS ("Android supports this type, but Windows does not"), so
        // claiming Linux while offering a `device_name` credential is an
        // incoherent pair - a Linux peer would be expected to offer
        // ssid/password. On 2026-07-16 the phone responded to exactly that
        // combination with *silence*: it took the offer and never attempted a
        // join, where every ssid/password attempt at least tried and failed
        // loudly.
        //
        // Safe against `bwu_manager.cc`'s guard, which refuses WIFI_DIRECT when
        // `client->GetLocalOsInfo().type() == WINDOWS`: `GetLocalOsInfo` is the
        // *phone's own* OS (`client_proxy.h` keeps `local_os_info_` separate
        // from `GetRemoteOsInfo`), so it reads ANDROID there regardless of what
        // we claim.
        #[cfg(target_os = "windows")]
        let os_type = location_nearby_connections::os_info::OsType::Windows;
        #[cfg(target_os = "macos")]
        let os_type = location_nearby_connections::os_info::OsType::Apple;
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let os_type = location_nearby_connections::os_info::OsType::Linux;

        let response = location_nearby_connections::OfflineFrame {
			version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
			v1: Some(location_nearby_connections::V1Frame {
				r#type: Some(location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into()),
				connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					os_info: Some(location_nearby_connections::OsInfo {
						r#type: Some(os_type.into())
					}),
					wifi_direct_device_name,
					..Default::default()
				}),
				..Default::default()
			})
		};

        self.send_frame(response.encode_to_vec()).await?;

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

    /// Offer a WIFI_DIRECT bandwidth upgrade: start a group and hand the peer
    /// its credentials.
    ///
    /// This is the medium to use from a WiFi-LAN connection. google/nearby's
    /// `bwu_manager.cc` rejects WIFI_HOTSPOT outright while WiFi-LAN is up ("this
    /// will destroy WIFI_LAN"), but only rejects WIFI_DIRECT when the *client* is
    /// Windows - and ours is the phone. See TODO.md.
    /// Bring up the WiFi Direct group owner and start listening, *without*
    /// sending the offer. Idempotent - a second call is a no-op.
    ///
    /// Split out from the offer so the group can be started early, at
    /// `WaitingForUserConsent`, seconds before the phone is asked to join.
    /// logcat showed the phone attempting P2P group formation *before* its
    /// `P2P-DEVICE-FOUND` for us landed - i.e. we were structurally too late,
    /// the group not yet discoverable when the peer first tried. google's
    /// handler likewise starts the GO and begins accepting connections before it
    /// builds the frame.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn ensure_wifi_direct_group(&mut self) -> Result<(), anyhow::Error> {
        if self.wifi_direct.is_some() {
            debug!("Bandwidth upgrade: WiFi Direct group already up");
            return Ok(());
        }

        let port: u16 = 8899;

        // On a blocking thread: the WinRT calls take seconds, and stalling the
        // async executor breaks the in-flight handshake frames - that's what
        // caused the "Missing required fields (ReceivedPairedKeyResult)" failures
        // when the hotspot was first wired in.
        let (handle, creds) = match tokio::task::spawn_blocking(
            crate::hdl::WindowsWifiDirect::start,
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                warn!("Bandwidth upgrade: WiFi Direct start failed: {e}");
                return Ok(());
            }
            Err(e) => {
                warn!("Bandwidth upgrade: WiFi Direct task join failed: {e}");
                return Ok(());
            }
        };

        info!(
            "Bandwidth upgrade: WiFi Direct group up (device_name={}, ssid={}, gateway={}, port={})",
            creds.device_name, creds.ssid, creds.gateway, port
        );

        // Keep the group and its credentials alive for the transfer.
        self.wifi_direct = Some(handle);
        self.wifi_direct_creds = Some(creds);

        // Wait for the phone to join the group and introduce itself. A join
        // arrives from the P2P subnet - anything from the LAN subnet (192.168.1.x)
        // is the phone still talking over WiFi-LAN and is *not* evidence the
        // upgrade worked.
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await {
                Ok(l) => {
                    info!("Bandwidth upgrade: listening on 0.0.0.0:{port}");
                    match l.accept().await {
                        Ok((s, addr)) => {
                            info!("*** Bandwidth upgrade: phone connected from {addr} ***");
                            if let Err(e) = introduce_upgraded_channel(s).await {
                                warn!("Bandwidth upgrade: introduction failed: {e}");
                            }
                        }
                        Err(e) => warn!("Bandwidth upgrade: accept failed: {e}"),
                    }
                }
                Err(e) => warn!("Bandwidth upgrade: bind failed: {e}"),
            }
        });

        Ok(())
    }

    /// Our IPv4 on the local network, for a WIFI_LAN upgrade offer.
    ///
    /// Skips loopback, link-local, and the 192.168.137.0/24 tethering subnet -
    /// that one is our own soft-AP, which a peer already on the LAN cannot reach
    /// and must not be told to use.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    fn lan_ipv4() -> Option<std::net::Ipv4Addr> {
        get_if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| {
            match i.ip() {
                std::net::IpAddr::V4(v4)
                    if !v4.is_loopback()
                        && !v4.is_link_local()
                        && !(v4.octets()[0] == 192
                            && v4.octets()[1] == 168
                            && v4.octets()[2] == 137) =>
                {
                    Some(v4)
                }
                _ => None,
            }
        })
    }

    /// Offer WIFI_LAN: the peer is already on our network, so just tell it where
    /// to connect.
    ///
    /// This is the right upgrade for a phone that reached us over BLE while
    /// having WiFi on - which happens, because we advertise as a receiver over
    /// both mDNS and BLE and the phone picks. A hotspot is wrong there (it would
    /// have to leave its network, which google/nearby refuses), but leaving it
    /// on BLE means 20 KB/s and the ~1 MB indication-timeout wall. Nothing has
    /// to be brought up: the network already exists.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn offer_wifi_lan_upgrade(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
            upgrade_path_info::{Medium, WifiLanSocket},
            EventType, UpgradePathInfo,
        };
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        let Some(ip) = Self::lan_ipv4() else {
            warn!("Bandwidth upgrade: no LAN address to offer; staying on the prior channel");
            return Ok(());
        };

        let port: u16 = 8899;
        self.ensure_upgrade_listener(port).await;

        let frame = OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                        .into(),
                ),
                bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::UpgradePathAvailable.into()),
                    upgrade_path_info: Some(UpgradePathInfo {
                        medium: Some(Medium::WifiLan.into()),
                        wifi_lan_socket: Some(WifiLanSocket {
                            // Network byte order, per the proto comment.
                            ip_address: Some(ip.octets().to_vec()),
                            wifi_port: Some(port as i32),
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
        info!("Bandwidth upgrade: offered WIFI_LAN ({ip}:{port})");
        Ok(())
    }

    /// Offer the phone every upgrade path we can serve, and let it pick.
    ///
    /// The phone's request carries no medium (the proto has the field only on
    /// the host's offer), and its WiFi state is not knowable at this point, so
    /// rather than guess we present both: WIFI_LAN when we have a LAN address,
    /// and WIFI_HOTSPOT. A single listener on 0.0.0.0:8899 accepts the phone on
    /// whichever interface it reaches us - the LAN one if it is on our network,
    /// the AP one if it is not. Whichever it connects on wins; the other offer
    /// is simply never taken up.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn offer_upgrade_paths(&mut self) {
        let have_lan = Self::lan_ipv4().is_some();

        // WIFI_LAN first: it needs no soft-AP, does not disturb the phone's own
        // network, and is the faster path when the phone is on our LAN.
        if have_lan {
            if let Err(e) = self.offer_wifi_lan_upgrade().await {
                warn!("offer_wifi_lan_upgrade failed: {e}");
            }
        }

        // WIFI_HOTSPOT as well, for a phone that genuinely has no network. When
        // we already have a LAN path this is the fallback the phone falls to
        // only if it cannot reach us on the LAN.
        if let Err(e) = self.offer_wifi_hotspot_upgrade().await {
            warn!("offer_wifi_hotspot_upgrade failed: {e}");
        }
    }

    #[cfg(not(all(feature = "experimental", target_os = "windows")))]
    async fn offer_upgrade_paths(&mut self) {}

    /// Accept the upgraded channel on `port`, introduce it, and hand it to the
    /// bridge. Shared by the hotspot and WIFI_LAN offers - only the medium and
    /// credentials differ, never what happens once the peer connects.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn ensure_upgrade_listener(&mut self, port: u16) {
        // Idempotent: a transfer may offer both WIFI_LAN and WIFI_HOTSPOT, and
        // one listener on 0.0.0.0 accepts the phone on whichever interface it
        // reaches us - a second bind would only fail AddrInUse.
        if self.upgrade_listener_started {
            return;
        }
        self.upgrade_listener_started = true;

        let upgrade_tx = self.upgrade_tx.clone();
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await {
                Ok(l) => {
                    info!("Bandwidth upgrade: listening on 0.0.0.0:{port}");
                    let accepted = match tokio::time::timeout(
                        std::time::Duration::from_secs(45),
                        l.accept(),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => {
                            warn!(
                                "Bandwidth upgrade: no peer connected within 45s; staying on the \
                                 prior channel"
                            );
                            return;
                        }
                    };
                    match accepted {
                        Ok((s, addr)) => {
                            info!("*** Bandwidth upgrade: phone connected from {addr} ***");
                            match introduce_upgraded_channel(s).await {
                                Ok(sock) => match &upgrade_tx {
                                    Some(tx) => {
                                        if tx.send(sock).is_err() {
                                            warn!(
                                                "Bandwidth upgrade: nothing left to hand the \
                                                 upgraded socket to"
                                            );
                                        }
                                    }
                                    None => warn!(
                                        "Bandwidth upgrade: introduced, but this transport cannot \
                                         adopt the upgraded socket"
                                    ),
                                },
                                Err(e) => warn!("Bandwidth upgrade: introduction failed: {e}"),
                            }
                        }
                        Err(e) => warn!("Bandwidth upgrade: accept failed: {e}"),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => warn!(
                    "Bandwidth upgrade: port {port} still held by a previous transfer; staying on \
                     the prior channel"
                ),
                Err(e) => warn!("Bandwidth upgrade: bind failed: {e}"),
            }
        });
    }

    /// Bring up the Windows soft-AP and start accepting the upgraded channel,
    /// without sending the offer. Idempotent.
    ///
    /// Same shape as `ensure_wifi_direct_group`, and for the same reason: start
    /// the medium and begin accepting *before* the offer goes out, because the
    /// phone acts on the frame immediately.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn ensure_hotspot(&mut self) -> Result<(), anyhow::Error> {
        if self.hotspot.is_some() {
            debug!("Bandwidth upgrade: hotspot already up");
            return Ok(());
        }

        let port: u16 = 8899;

        // SSID/passphrase are per-session. The phone only ever learns them from
        // the credentials frame, so they don't need to be memorable - but they
        // do need to be WPA2-legal: 8..=63 characters.
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

        // On a blocking thread: the WinRT tethering calls take seconds, and
        // stalling the executor breaks in-flight handshake frames - that is
        // exactly what produced "Missing required fields
        // (ReceivedPairedKeyResult)" when the hotspot was first wired in.
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
        self.hotspot_creds = Some(creds);

        // One 0.0.0.0 listener accepts the phone on whichever interface it
        // reaches us, LAN or the AP - so when both mediums are offered and the
        // WIFI_LAN offer already bound it, don't bind a second and trip
        // AddrInUse (which logged a misleading "held by a previous transfer" for
        // the *same* transfer). The AP itself is still up; only the redundant
        // listener is skipped. Same guard as ensure_upgrade_listener.
        if self.upgrade_listener_started {
            return Ok(());
        }
        self.upgrade_listener_started = true;

        let upgrade_tx = self.upgrade_tx.clone();
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await {
                Ok(l) => {
                    info!("Bandwidth upgrade: listening on 0.0.0.0:{port}");
                    // Bounded. The phone joined in ~5s in every successful run,
                    // and it abandons the upgrade after ~68s anyway. An
                    // unbounded accept leaks this task for the life of the
                    // process and holds port 8899, so one phone that never
                    // joined would break every subsequent transfer's upgrade.
                    let accepted =
                        match tokio::time::timeout(std::time::Duration::from_secs(45), l.accept())
                            .await
                        {
                            Ok(r) => r,
                            Err(_) => {
                                warn!(
                                    "Bandwidth upgrade: no peer joined the hotspot within 45s; \
                                     staying on the prior channel"
                                );
                                return;
                            }
                        };
                    match accepted {
                        Ok((s, addr)) => {
                            info!("*** Bandwidth upgrade: phone connected from {addr} ***");
                            match introduce_upgraded_channel(s).await {
                                Ok(sock) => match &upgrade_tx {
                                    Some(tx) => {
                                        if tx.send(sock).is_err() {
                                            warn!(
                                                "Bandwidth upgrade: nothing left to hand the \
                                                 upgraded socket to"
                                            );
                                        }
                                    }
                                    // The upgrade completed but no transport can
                                    // adopt it, so the payload would arrive on a
                                    // socket nobody reads. Say so rather than
                                    // dropping it silently.
                                    None => warn!(
                                        "Bandwidth upgrade: introduced, but this transport \
                                         cannot adopt the upgraded socket"
                                    ),
                                },
                                Err(e) => warn!("Bandwidth upgrade: introduction failed: {e}"),
                            }
                        }
                        Err(e) => warn!("Bandwidth upgrade: accept failed: {e}"),
                    }
                }
                // AddrInUse here means a previous transfer's listener is still
                // holding the port. The transfer still works - it just stays on
                // the prior channel - so say which it is rather than failing
                // opaquely.
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => warn!(
                    "Bandwidth upgrade: port {port} still held by a previous transfer; \
                     staying on the prior channel"
                ),
                Err(e) => warn!("Bandwidth upgrade: bind failed: {e}"),
            }
        });

        Ok(())
    }

    /// Offer WIFI_HOTSPOT. Unlike the WiFi Direct frame this one is
    /// unambiguous - ssid/password/gateway/port is a plain soft-AP the phone
    /// joins as an ordinary client, with no P2P device discovery and no
    /// auth-type incompatibility between GMS versions.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn offer_wifi_hotspot_upgrade(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
            upgrade_path_info::{Medium, WifiHotspotCredentials},
            EventType, UpgradePathInfo,
        };
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        self.ensure_hotspot().await?;

        let port: u16 = 8899;
        let creds = match &self.hotspot_creds {
            Some(c) => c.clone(),
            None => {
                warn!("Bandwidth upgrade: no hotspot credentials; soft-AP failed to start");
                return Ok(());
            }
        };

        let frame = OfflineFrame {
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
                            // -1 = unspecified. Windows picks the band; naming a
                            // frequency we don't control is how the WiFi Direct
                            // attempt misled the peer.
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
    async fn offer_wifi_hotspot_upgrade(&mut self) -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn offer_wifi_direct_upgrade(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
            upgrade_path_info::{Medium, WifiDirectCredentials},
            EventType, UpgradePathInfo,
        };
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        // Start the group if it wasn't started early at WaitingForUserConsent.
        self.ensure_wifi_direct_group().await?;

        let port: u16 = 8899;
        let creds = match &self.wifi_direct_creds {
            Some(c) => c.clone(),
            None => {
                warn!("Bandwidth upgrade: no WiFi Direct credentials; group failed to start");
                return Ok(());
            }
        };

        let frame = OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation
                        .into(),
                ),
                bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                    event_type: Some(EventType::UpgradePathAvailable.into()),
                    upgrade_path_info: Some(UpgradePathInfo {
                        medium: Some(Medium::WifiDirect.into()),
                        // Send *every* field, exactly as google does:
                        //   ForBwuWifiDirectPathAvailable(ssid, password, port,
                        //       freq, disabling_encryption, gateway, device_name, pin)
                        //
                        // Two incompatible peers exist and this frame has to
                        // satisfy both:
                        //   - Current google/nearby *refuses* ssid/password
                        //     ("SSID/PASSWORD auth type is not supported") and
                        //     requires device_name.
                        //   - The Pixel's shipped GMS 26.26.34 does the reverse:
                        //     a device-name-only frame is dropped with "missing
                        //     ssid or not in correct format". The proto says why
                        //     - device-name on Android is "in the future".
                        // google populates all of them (ssid/password simply come
                        // back empty on Windows), so nobody has to choose. We
                        // stripped them and got silence; don't strip them again.
                        wifi_direct_credentials: Some(WifiDirectCredentials {
                            ssid: Some(creds.ssid),
                            password: Some(creds.passphrase),
                            gateway: Some(creds.gateway),
                            device_name: Some(creds.device_name),
                            // ConfirmOnly pairing: the proto notes pin "is not
                            // used for WIFI_DIRECT_WITH_DEVICE_NAME, reserve for
                            // future expansion". google sets it to "".
                            pin: Some(String::new()),
                            port: Some(port as i32),
                            // The frequency the peer should look for our group on.
                            //
                            // Do NOT dismiss this as ignored: logcat's "Set P2P
                            // operating channel to 0" is the phone configuring
                            // *its own* GO channel, not where it hunts for ours.
                            // On a fast-connect join Android passes this down to
                            // the supplicant, and with -1 it has nothing to go on
                            // - so it falls back to scanning the social channels
                            // (1/6/11, 2.4GHz) while our group sits on ch157 /
                            // 5785MHz (measured with a WiFi analyser on the
                            // phone; Windows exposes no channel through WinRT or
                            // the WLAN API). It never finds us, which is the 12-14s
                            // timeout and GROUP_REMOVED with no association.
                            //
                            // Override with RQS_WIFI_DIRECT_FREQ to probe values
                            // without a rebuild. -1 means "unknown" and must be
                            // sent explicitly: this message declares no proto2
                            // default, so an absent field reads back as 0.
                            frequency: Some(
                                std::env::var("RQS_WIFI_DIRECT_FREQ")
                                    .ok()
                                    .and_then(|v| v.parse::<i32>().ok())
                                    .unwrap_or(-1),
                            ),
                            ..Default::default()
                        }),
                        // We do answer the introduction (see
                        // `introduce_upgraded_channel`), so say so. google's GO
                        // writes the ACK unconditionally, but the *client* only
                        // waits for one when this is set - leaving it unset and
                        // then sending an ACK anyway would put a frame on the
                        // channel the phone isn't expecting to read.
                        supports_client_introduction_ack: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&frame).await?;
        info!("Bandwidth upgrade: sent UPGRADE_PATH_AVAILABLE (WIFI_DIRECT)");
        Ok(())
    }

    #[cfg(not(all(feature = "experimental", target_os = "windows")))]
    async fn offer_wifi_direct_upgrade(&mut self) -> Result<(), anyhow::Error> {
        Ok(())
    }

    // Respond to the phone's BANDWIDTH_UPGRADE_RETRY by telling it which mediums
    // we support, so it can drive the upgrade toward WIFI_DIRECT.
    //
    // This claimed [WIFI_HOTSPOT, WIFI_LAN] - left over from the hotspot era, and
    // wrong in both directions now: it advertises a medium whose code we deleted,
    // and omits the one we actually offer. The phone announces
    // [WifiLan, WifiDirect, WifiAware, WifiHotspot, BleL2cap, Bluetooth, Ble, Nfc],
    // so WIFI_DIRECT is in its set; we were answering with a disjoint claim and
    // then sending UPGRADE_PATH_AVAILABLE(WIFI_DIRECT) for something we'd never
    // said we could do.
    #[cfg(all(feature = "experimental", target_os = "windows"))]
    async fn send_supported_mediums(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_retry_frame::Medium;
        use crate::location_nearby_connections::BandwidthUpgradeRetryFrame;

        let frame = OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeRetry.into(),
                ),
                bandwidth_upgrade_retry: Some(BandwidthUpgradeRetryFrame {
                    // Claim exactly what we will offer, and offer both when we
                    // can.
                    //
                    // WIFI_HOTSPOT is the soft-AP a phone with no network joins.
                    // WIFI_LAN is added whenever we have a LAN address, because
                    // the phone's WiFi state is not knowable at negotiation time
                    // - the Quick Share extension drops WiFi to send and then
                    // reconnects, so a phone that reported no LAN in its
                    // ConnectionRequest is frequently back on the LAN moments
                    // later, unable to join our AP (one radio, already
                    // associated). Advertising both lets us offer both paths and
                    // the phone connect on whichever it can actually reach.
                    //
                    // WIFI_DIRECT stays out: its offer is gated off (a failed
                    // join wedges the phone's P2P state at [2]BUSY), so claiming
                    // it would invite a medium we never provide.
                    supported_medium: if Self::lan_ipv4().is_some() {
                        vec![Medium::WifiLan.into(), Medium::WifiHotspot.into()]
                    } else {
                        vec![Medium::WifiHotspot.into()]
                    },
                    is_request: Some(false),
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&frame).await?;
        if Self::lan_ipv4().is_some() {
            info!("Bandwidth upgrade: replied with supported mediums [WIFI_LAN, WIFI_HOTSPOT]");
        } else {
            info!("Bandwidth upgrade: replied with supported mediums [WIFI_HOTSPOT]");
        }
        Ok(())
    }

    #[cfg(not(all(feature = "experimental", target_os = "windows")))]
    async fn send_supported_mediums(&mut self) -> Result<(), anyhow::Error> {
        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let _prof_t = std::time::Instant::now();
        let mut hmac = HmacSha256::new_from_slice(self.state.recv_hmac_key.as_ref().unwrap())?;
        hmac.update(&smsg.header_and_body);
        let computed = hmac.finalize().into_bytes();
        if computed[..] != smsg.signature[..] {
            return Err(anyhow!("hmac!=signature"));
        }

        let header_and_body = HeaderAndBody::decode(&*smsg.header_and_body)?;

        let msg_data = header_and_body.body;
        let key = self.state.decrypt_key.as_ref().unwrap();

        let _prof_a = std::time::Instant::now();
        let decrypted =
            crate::hdl::aes256_cbc_decrypt(key, header_and_body.header.iv(), &msg_data)?;
        self.prof_aes += _prof_a.elapsed();

        let d2d_msg = DeviceToDeviceMessage::decode(&*decrypted)?;
        // Everything from _prof_t is HMAC + AES + protobuf decode - the CPU cost
        // of a frame; prof_aes isolates just the AES within it. Accumulate for
        // the throughput report at the file write.
        self.prof_decrypt += _prof_t.elapsed();

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
            .ok_or_else(|| anyhow!("Missing required fields: decrypted OfflineFrame has no v1"))?;
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
                        info!("Processing PayloadType::Bytes");
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

                            if self.state.text_payload.is_some()
                                && self.state.text_payload.as_ref().unwrap().get_i64_value()
                                    == payload_id
                            {
                                info!("Transfer finished");
                                let end_index =
                                    buffer.iter().position(|&b| b == 16).unwrap_or(buffer.len());
                                let payload = std::str::from_utf8(&buffer[..end_index])?.to_owned();

                                match self.state.text_payload.clone().unwrap() {
                                    TextPayloadInfo::Url(_) => {
                                        self.update_state(
                                            |e| {
                                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                                    tmd.text_payload = Some(payload);
                                                    tmd.text_type = Some(TextPayloadType::Url);
                                                }
                                            },
                                            false,
                                        )
                                        .await;
                                    }
                                    TextPayloadInfo::Text(_) => {
                                        self.update_state(
                                            |e| {
                                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                                    tmd.text_payload = Some(payload);
                                                    tmd.text_type = Some(TextPayloadType::Text);
                                                }
                                            },
                                            false,
                                        )
                                        .await;
                                    }
                                    TextPayloadInfo::Wifi((_, ssid)) => {
                                        self.update_state(
                                            |e| {
                                                if let Some(tmd) = e.transfer_metadata.as_mut() {
                                                    tmd.text_payload =
                                                        Some(format!("{ssid}: {}", payload.trim()));
                                                    tmd.text_type = Some(TextPayloadType::Wifi);
                                                }
                                            },
                                            false,
                                        )
                                        .await;
                                    }
                                }

                                self.update_state(
                                    |e| {
                                        e.state = State::Finished;
                                    },
                                    true,
                                )
                                .await;
                                self.disconnection().await?;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            } else {
                                let innner_frame =
                                    sharing_nearby::Frame::decode(buffer.as_slice())?;
                                self.process_transfer_setup(&innner_frame).await?;
                            }
                        }
                    }
                    payload_header::PayloadType::File => {
                        // Once per chunk (~35/s over BLE), so `trace`.
                        trace!("Processing PayloadType::File");
                        let payload_id = header.id();

                        let file_internal = self
                            .state
                            .transferred_files
                            .get_mut(&payload_id)
                            .ok_or_else(|| {
                                anyhow!("File payload ID ({}) is not known", payload_id)
                            })?;

                        let current_offset = file_internal.bytes_transferred;
                        if chunk.offset() != current_offset {
                            return Err(anyhow!(
                                "Invalid offset into file {}, expected {}",
                                chunk.offset(),
                                current_offset
                            ));
                        }

                        let chunk_size = chunk.body().len();
                        if current_offset + chunk_size as i64 > file_internal.total_size {
                            return Err(anyhow!(
                                "Transferred file size exceeds previously specified value: {} vs {}", current_offset + chunk_size as i64, file_internal.total_size
                            ));
                        }

                        if !chunk.body().is_empty() {
                            let _prof_w = std::time::Instant::now();
                            write_all_at(
                                file_internal.file.as_ref().unwrap(),
                                chunk.body(),
                                current_offset as u64,
                            )?;
                            file_internal.bytes_transferred += chunk_size as i64;

                            // Throughput profiling. Split wall-clock into
                            // decrypt, write, and the remainder (socket read +
                            // duplex + protobuf dispatch + waiting on the link),
                            // reported every 5s. Tells us whether the ~4 MB/s
                            // ceiling is our CPU (decrypt), our disk (write), or
                            // neither - i.e. the network/link is simply not
                            // delivering faster.
                            self.prof_write += _prof_w.elapsed();
                            self.prof_bytes += chunk_size as u64;
                            let now = std::time::Instant::now();
                            let since = *self.prof_since.get_or_insert(now);
                            let elapsed = now.duration_since(since);
                            if elapsed >= Duration::from_secs(5) {
                                let secs = elapsed.as_secs_f64();
                                debug!(
                                    "recv profile: {:.1} MB/s over {secs:.0}s | decrypt {:.0}ms/s \
                                     (of which AES {:.0}ms/s), write {:.0}ms/s, rest {:.0}ms/s",
                                    self.prof_bytes as f64 / 1_048_576.0 / secs,
                                    self.prof_decrypt.as_millis() as f64 / secs,
                                    self.prof_aes.as_millis() as f64 / secs,
                                    self.prof_write.as_millis() as f64 / secs,
                                    (elapsed.saturating_sub(self.prof_decrypt).saturating_sub(
                                        self.prof_write
                                    ))
                                    .as_millis() as f64
                                        / secs,
                                );
                                self.prof_decrypt = Duration::ZERO;
                                self.prof_aes = Duration::ZERO;
                                self.prof_write = Duration::ZERO;
                                self.prof_bytes = 0;
                                self.prof_since = Some(now);
                            }

                            // Advance the byte count every chunk, but broadcast
                            // it at most every 100ms - see last_progress_broadcast.
                            // 100ms is smooth for a progress bar and keeps the
                            // shared channel from overflowing. The final state is
                            // always broadcast on completion below, so the bar
                            // still lands on 100%.
                            let now = std::time::Instant::now();
                            let due = self
                                .last_progress_broadcast
                                .map(|t| now.duration_since(t) >= Duration::from_millis(100))
                                .unwrap_or(true);
                            if due {
                                self.last_progress_broadcast = Some(now);
                            }
                            self.update_state(
                                |e| {
                                    if let Some(tmd) = e.transfer_metadata.as_mut() {
                                        tmd.ack_bytes += chunk_size as u64;
                                    }
                                },
                                due,
                            )
                            .await;
                        } else if (chunk.flags() & 1) == 1 {
                            self.state.transferred_files.remove(&payload_id);
                            if self.state.transferred_files.is_empty() {
                                info!("Transfer finished");
                                self.update_state(
                                    |e| {
                                        e.state = State::Finished;
                                    },
                                    true,
                                )
                                .await;
                                self.disconnection().await?;
                                return Err(anyhow!(crate::errors::AppError::NotAnError));
                            }
                        }
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
                // Answer every one. Rate-limiting these to 12s was tried on the
                // theory that fewer outbound indications would clear the BLE
                // link's confirmation backlog; it measurably made things *worse*
                // (failure moved from ~1.3 MB to ~777 KB), because there was no
                // backlog to clear - the indications were confirming one at a
                // time with `0 still queued`. Don't retry this.
                trace!("Sending keepalive");
                self.last_keepalive_response = Some(std::time::Instant::now());
                self.send_keepalive(true).await?;
            }
            location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation => {
                info!(
                    "Received BandwidthUpgradeNegotiation: {:?}",
                    v1_frame.bandwidth_upgrade_negotiation
                );
                let is_path_request = v1_frame
                    .bandwidth_upgrade_negotiation
                    .as_ref()
                    .map(|bun| {
                        bun.event_type()
                            == location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::UpgradePathRequest
                    })
                    .unwrap_or(false);
                // The client announces it has finished writing to the old
                // channel and will not proceed until we say the old one may be
                // closed. Switching at CLIENT_INTRODUCTION_ACK instead left this
                // frame arriving at a bridge that had already torn down, and the
                // phone cancelled the transfer.
                let is_last_write = v1_frame
                    .bandwidth_upgrade_negotiation
                    .as_ref()
                    .map(|bun| {
                        bun.event_type()
                            == location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType::LastWriteToPriorChannel
                    })
                    .unwrap_or(false);
                if is_last_write {
                    info!("Bandwidth upgrade: LAST_WRITE_TO_PRIOR_CHANNEL; releasing the old channel");
                    if let Err(e) = self.send_safe_to_close_prior_channel().await {
                        warn!("send_safe_to_close_prior_channel failed: {e}");
                    }
                    // Only now is it safe to move the stream across.
                    #[cfg(all(feature = "experimental", target_os = "windows"))]
                    if let Some(tx) = &self.switch_tx {
                        let _ = tx.send(());
                    }
                }

                if is_path_request {
                    // WIFI_HOTSPOT, not WIFI_DIRECT. Windows can only host
                    // WIFI_DIRECT_WITH_DEVICE_NAME, which needs Wi-Fi P2P device
                    // discovery and never completed against this phone; a
                    // soft-AP is a medium Windows genuinely serves. Set
                    // RQS_TRY_WIFI_DIRECT_UPGRADE=1 to use the old path.
                    if std::env::var("RQS_TRY_WIFI_DIRECT_UPGRADE").is_ok() {
                        info!("Phone requested an upgrade path; offering WIFI_DIRECT");
                        if let Err(e) = self.offer_wifi_direct_upgrade().await {
                            warn!("offer_wifi_direct_upgrade (on request) failed: {}", e);
                        }
                    } else {
                        info!("Phone requested an upgrade path; offering WIFI_LAN and WIFI_HOTSPOT");
                        self.offer_upgrade_paths().await;
                    }
                }
            }
            location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeRetry => {
                info!(
                    "Received BandwidthUpgradeRetry: {:?}",
                    v1_frame.bandwidth_upgrade_retry
                );
                // The phone announces its mediums with `is_request` unset and
                // then waits for ours before asking for an upgrade path. Gating
                // the reply on `is_request` meant we never answered, so it never
                // asked - and the unsolicited UPGRADE_PATH_AVAILABLE we pushed
                // at accept time was answering a question nobody had put.
                //
                // Verified reachable: the phone joins our AP, takes a DHCP lease
                // and reaches 8899 when driven by hand. Only the negotiation is
                // missing.
                if let Err(e) = self.send_supported_mediums().await {
                    warn!("send_supported_mediums failed: {}", e);
                }
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
            .ok_or_else(|| anyhow!("Missing required fields: transfer-setup frame has no v1"))?;

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
                        e.state = State::ReceivedPairedKeyResult;
                    },
                    false,
                )
                .await;
            }
            State::ReceivedPairedKeyResult => {
                debug!("Processing State::ReceivedPairedKeyResult");
                self.process_introduction(v1_frame).await?;
            }
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
            return Err(anyhow!(
                "Missing required fields: expected PairedKeyEncryption, got {:?}",
                v1_frame.r#type()
            ));
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
        &self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_result.is_none() {
            return Err(anyhow!(
                "Missing required fields: expected PairedKeyResult, got {:?}",
                v1_frame.r#type()
            ));
        }

        Ok(())
    }

    async fn process_introduction(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        // A Response here means the peer thinks *it* is receiving.
        //
        // The sender drives PairedKeyEncryption -> PairedKeyResult ->
        // Introduction; the receiver answers with Response. So a Response
        // arriving where an Introduction belongs is a role mismatch, not a
        // corrupt frame - the peer connected believing we were sending to it.
        //
        // Seen after a completed PC -> phone transfer, when the phone then
        // connects over BLE still holding the earlier session's role. This
        // handler can only receive, so there is nothing to salvage; say so
        // plainly instead of failing with a confusing "missing field" and
        // leaving the card hanging.
        if v1_frame.r#type() == sharing_nearby::v1_frame::FrameType::Response {
            warn!(
                "Peer sent Response where an Introduction belongs: it believes it is the \
                 receiver. Dropping the session so it can start a fresh one."
            );
            // Tell it to let go, so the stale role dies with this connection.
            // Without this the peer keeps the role and every retry fails the
            // same way until the *app* is restarted, which is what we saw.
            if let Err(e) = self.disconnection().await {
                debug!("Could not send disconnection to the confused peer: {e}");
            }
            self.update_state(
                |e| {
                    e.state = State::Disconnected;
                },
                true,
            )
            .await;
            return Err(anyhow!(
                "peer connected as a receiver, expecting us to send - it is still holding the \
                 role from an earlier transfer. Nothing to receive here."
            ));
        }

        let introduction = v1_frame.introduction.as_ref().ok_or_else(|| {
            anyhow!(
                "Missing required fields: expected Introduction, got {:?}",
                v1_frame.r#type()
            )
        })?;

        // No need to inform the channel here, we'll do it anyway with files info
        self.update_state(
            |e| {
                e.state = State::WaitingForUserConsent;
            },
            false,
        )
        .await;

        // Start the WiFi Direct group *now*, while the user is deciding, rather
        // than at accept time. Bringing up the group owner takes a couple of
        // seconds of WinRT work; starting it here gives it a head start so it's
        // discoverable before the phone's first P2P formation attempt (which
        // logcat showed racing ahead of discovery). The offer frame is still
        // sent at accept time; ensure_wifi_direct_group() is idempotent.
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        if std::env::var("RQS_TRY_WIFI_DIRECT_UPGRADE").is_ok() {
            if let Err(e) = self.ensure_wifi_direct_group().await {
                warn!("ensure_wifi_direct_group (early) failed: {e}");
            }
        }

        if !introduction.file_metadata.is_empty() && introduction.text_metadata.is_empty() {
            trace!("process_introduction: handling file_metadata");
            let mut files_name = Vec::with_capacity(introduction.file_metadata.len());
            let mut total_bytes: u64 = 0;

            for file in &introduction.file_metadata {
                info!("File name: {}", file.name());

                let mut dest = get_download_dir();
                dest.push(file.name());

                info!("Destination: {:?}", dest);
                if dest.exists() {
                    let mut counter = 1;
                    dest.pop();

                    loop {
                        dest.push(format!("{}_{}", counter, file.name()));
                        if !dest.exists() {
                            break;
                        }
                        dest.pop();
                        counter += 1;
                    }

                    info!("New destination: {:?}", dest);
                }

                let info = InternalFileInfo {
                    payload_id: file.payload_id(),
                    file_url: dest,
                    bytes_transferred: 0,
                    total_size: file.size(),
                    file: None,
                };
                total_bytes += info.total_size as u64;
                self.state.transferred_files.insert(file.payload_id(), info);
                files_name.push(file.name().to_owned());
            }

            let metadata = TransferMetadata {
                id: self.state.id.clone(),
                destination: Some(
                    get_download_dir()
                        .into_os_string()
                        .into_string()
                        .map_err(|_| anyhow!("failed to convert PathBuf to String"))?,
                ),
                source: self.state.remote_device_info.clone(),
                files: Some(files_name),
                pin_code: self.state.pin_code.clone(),
                text_description: None,
                total_bytes,
                ..Default::default()
            };

            info!("Asking for user consent: {:?}", metadata);
            self.update_state(
                |e| {
                    e.transfer_metadata = Some(metadata);
                },
                true,
            )
            .await;
        } else if introduction.text_metadata.len() == 1 {
            trace!("process_introduction: handling text_metadata");
            let meta = introduction.text_metadata.first().unwrap();

            match meta.r#type() {
                text_metadata::Type::Url => {
                    let metadata = TransferMetadata {
                        id: self.state.id.clone(),
                        destination: None,
                        source: self.state.remote_device_info.clone(),
                        files: None,
                        pin_code: self.state.pin_code.clone(),
                        text_description: meta.text_title.clone(),
                        ..Default::default()
                    };

                    info!("Asking for user consent: {:?}", metadata);
                    self.update_state(
                        |e| {
                            e.text_payload = Some(TextPayloadInfo::Url(meta.payload_id()));
                            e.transfer_metadata = Some(metadata);
                        },
                        true,
                    )
                    .await;
                }
                text_metadata::Type::PhoneNumber
                | text_metadata::Type::Address
                | text_metadata::Type::Text => {
                    let metadata = TransferMetadata {
                        id: self.state.id.clone(),
                        destination: None,
                        source: self.state.remote_device_info.clone(),
                        files: None,
                        pin_code: self.state.pin_code.clone(),
                        text_description: meta.text_title.clone(),
                        ..Default::default()
                    };

                    info!("Asking for user consent: {:?}", metadata);
                    self.update_state(
                        |e| {
                            e.text_payload = Some(TextPayloadInfo::Text(meta.payload_id()));
                            e.transfer_metadata = Some(metadata);
                        },
                        true,
                    )
                    .await;
                }
                text_metadata::Type::Unknown => {
                    // Reject transfer
                    self.reject_transfer(Some(
						sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType,
					))
					.await?;
                }
            }
        } else if introduction.wifi_credentials_metadata.len() == 1 {
            trace!("process_introduction: handling wifi_credentials_metadata");
            let meta = introduction.wifi_credentials_metadata.first().unwrap();

            let metadata = TransferMetadata {
                id: self.state.id.clone(),
                destination: None,
                source: self.state.remote_device_info.clone(),
                files: None,
                pin_code: self.state.pin_code.clone(),
                text_description: meta.ssid.clone(),
                ..Default::default()
            };

            self.update_state(
                |e| {
                    e.text_payload = Some(TextPayloadInfo::Wifi((
                        meta.payload_id(),
                        meta.ssid().to_owned(),
                    )));
                    e.transfer_metadata = Some(metadata);
                },
                true,
            )
            .await;
        } else {
            // Reject transfer
            self.reject_transfer(Some(
                sharing_nearby::connection_response_frame::Status::UnsupportedAttachmentType,
            ))
            .await?;
        }

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

    async fn accept_transfer(&mut self) -> Result<(), anyhow::Error> {
        let ids: Vec<i64> = self.state.transferred_files.keys().cloned().collect();

        for id in ids {
            let mfi = self.state.transferred_files.get_mut(&id).unwrap();

            let file = File::create(&mfi.file_url)?;
            info!("Created file: {file:?}");
            mfi.file = Some(file);
        }

        let frame = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
                connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                    status: Some(sharing_nearby::connection_response_frame::Status::Accept.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&frame).await?;

        // Offer the bandwidth upgrade now that the user has accepted and file
        // bytes are about to flow — grishka's PROTOCOL.md says this push is the
        // design ("after the transfer is accepted, the server may ask the client
        // for a bandwidth upgrade"). WIFI_DIRECT, not WIFI_HOTSPOT: the phone
        // refuses the latter outright while it's on WiFi-LAN.
        // Opt-in while this is being brought up.
        // Push it unsolicited. Over BLE the phone never sends
        // BandwidthUpgradeRetry *or* UPGRADE_PATH_REQUEST - measured: not one
        // of either in a full WiFi-off transfer - while its own log says
        // "timeout when waiting for high-quality medium" for 68s. Both sides
        // were waiting for the other. Our handlers stay in place for the peers
        // that do ask; this covers the ones that don't.
        //
        // The WIFI_HOTSPOT-is-refused-on-WiFi-LAN objection doesn't apply here:
        // with the phone's WiFi off its ConnectionRequest omits WIFI_LAN, so
        // there is no WiFi-LAN connection for a hotspot to destroy.
        // Only upgrade a transport that can actually be upgraded.
        //
        // `upgrade_tx` is set by the BLE bridge alone - it is the only transport
        // that can swap the stream onto a new socket, and the only one slow
        // enough to want to. Offering on a connection that is already TCP is
        // both pointless and destructive: the peer took the offer, connected to
        // our upgrade port, introduced itself, sent LAST_WRITE and moved to the
        // new socket, while this request carried on reading the old one - the
        // stream desynced and the next frame died with "SecureMessage.
        // header_and_body: invalid wire type".
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        let can_upgrade = self.upgrade_tx.is_some();
        #[cfg(not(all(feature = "experimental", target_os = "windows")))]
        let can_upgrade = false;

        if !can_upgrade {
            debug!("Bandwidth upgrade: transport cannot adopt an upgraded socket, not offering");
        } else if std::env::var("RQS_TRY_WIFI_DIRECT_UPGRADE").is_ok() {
            if let Err(e) = self.offer_wifi_direct_upgrade().await {
                warn!("offer_wifi_direct_upgrade failed: {}", e);
            }
        } else {
            // Offer both paths and let the phone connect on whichever it can
            // reach. Deciding here from the ConnectionRequest was unreliable:
            // it reflects the phone's WiFi state at one instant, and the Quick
            // Share extension drops WiFi to send then reconnects, so a phone
            // that reported no LAN is often back on it moments later, unable to
            // join our AP. Offering only the AP stranded exactly those
            // transfers (measured: the phone sat on its home network, could not
            // associate to our soft-AP, and the BLE link died on the 30s
            // indication timeout). See offer_upgrade_paths.
            self.offer_upgrade_paths().await;
        }

        self.update_state(
            |e| {
                e.state = State::ReceivingFiles;
            },
            true,
        )
        .await;

        Ok(())
    }

    async fn reject_transfer(
        &mut self,
        reason: Option<sharing_nearby::connection_response_frame::Status>,
    ) -> Result<(), anyhow::Error> {
        let sreason = if let Some(r) = reason {
            r
        } else {
            sharing_nearby::connection_response_frame::Status::Reject
        };

        let frame = sharing_nearby::Frame {
            version: Some(sharing_nearby::frame::Version::V1.into()),
            v1: Some(sharing_nearby::V1Frame {
                r#type: Some(sharing_nearby::v1_frame::FrameType::Response.into()),
                connection_response: Some(sharing_nearby::ConnectionResponseFrame {
                    status: Some(sreason.into()),
                }),
                ..Default::default()
            }),
        };

        self.send_encrypted_frame(&frame).await?;

        Ok(())
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
                e.decrypt_key = Some(client_key);
                e.recv_hmac_key = Some(client_hmac_key);
                e.encrypt_key = Some(server_key);
                e.send_hmac_key = Some(server_hmac_key);
                e.pin_code = Some(to_four_digit_string(&auth_string));
                e.encryption_done = true;
            },
            false,
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

    /// Tell the client the prior (BLE) channel may be closed. Sent on the *old*
    /// channel, which is the last thing that goes over it.
    async fn send_safe_to_close_prior_channel(&mut self) -> Result<(), anyhow::Error> {
        use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::EventType;
        use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

        let frame = OfflineFrame {
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

        trace!("Sending msg into the channel");
        let _ = self.sender.send(ChannelMessage {
            id: self.state.id.clone(),
            direction: ChannelDirection::LibToFront,
            rtype: Some(crate::channel::TransferType::Inbound),
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

/// Read the phone's CLIENT_INTRODUCTION off the freshly-joined WiFi Direct
/// socket and answer it with CLIENT_INTRODUCTION_ACK.
///
/// These two frames are **plaintext** `OfflineFrame`s with the usual 4-byte
/// big-endian length prefix. google/nearby reads the introduction before
/// `ReplaceChannelForEndpoint(..., enable_encryption)` hands the UKEY2 context
/// to the new channel, so nothing on this socket is encrypted until the swap -
/// which is why this runs on its own without touching the handshake keys or the
/// sequence counters.
///
/// Free-standing rather than a method on `InboundRequest<S>`: an async fn in
/// that impl produces a future parameterised by `S` even when it never touches
/// it, so `tokio::spawn` would demand `S: Send + 'static` from an impl that
/// doesn't promise it.
///
/// Stops at the ACK for now. The swap itself (LAST_WRITE_TO_PRIOR_CHANNEL ->
/// SAFE_TO_CLOSE_PRIOR_CHANNEL, then moving the encrypted stream across) is
/// still to build, so the phone will introduce itself, get its ACK, and then
/// wait for a move that never comes. The transfer continues on WiFi-LAN.
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub(crate) async fn introduce_upgraded_channel(
    mut socket: tokio::net::TcpStream,
) -> Result<tokio::net::TcpStream, anyhow::Error> {
    use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::{
        ClientIntroductionAck, EventType,
    };
    use crate::location_nearby_connections::BandwidthUpgradeNegotiationFrame;

    let mut length_buf = [0u8; 4];
    stream_read_exact(&mut socket, &mut length_buf).await?;
    let length = i32::from_be_bytes(length_buf);
    if length <= 0 || length > SANE_FRAME_LENGTH {
        return Err(anyhow!("insane introduction frame length: {length}"));
    }

    let mut frame_buf = vec![0u8; length as usize];
    stream_read_exact(&mut socket, &mut frame_buf).await?;
    let frame = OfflineFrame::decode(frame_buf.as_slice())?;
    info!("Bandwidth upgrade: introduction frame: {:?}", frame);

    let endpoint_id = frame
        .v1
        .as_ref()
        .and_then(|v1| v1.bandwidth_upgrade_negotiation.as_ref())
        .filter(|bun| bun.event_type() == EventType::ClientIntroduction)
        .and_then(|bun| bun.client_introduction.as_ref())
        .map(|intro| intro.endpoint_id.clone())
        .ok_or_else(|| anyhow!("first frame on the upgraded socket was not CLIENT_INTRODUCTION"))?;

    info!("*** Bandwidth upgrade: CLIENT_INTRODUCTION from endpoint_id={endpoint_id:?} ***");

    let ack = OfflineFrame {
        version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
        v1: Some(location_nearby_connections::V1Frame {
            r#type: Some(
                location_nearby_connections::v1_frame::FrameType::BandwidthUpgradeNegotiation.into(),
            ),
            bandwidth_upgrade_negotiation: Some(BandwidthUpgradeNegotiationFrame {
                event_type: Some(EventType::ClientIntroductionAck.into()),
                client_introduction_ack: Some(ClientIntroductionAck {}),
                ..Default::default()
            }),
            ..Default::default()
        }),
    };
    let data = ack.encode_to_vec();
    socket.write_all(&(data.len() as u32).to_be_bytes()).await?;
    socket.write_all(&data).await?;
    socket.flush().await?;
    info!("Bandwidth upgrade: sent CLIENT_INTRODUCTION_ACK");

    // Hand the socket back to the caller, which splices it into the existing
    // encrypted stream. It was previously drained and logged here, which is how
    // we learned the phone starts sending payload bytes the moment it is
    // acknowledged - it does not wait for LAST_WRITE_TO_PRIOR_CHANNEL on the old
    // channel. So there is nothing to do between the ACK and the swap, and
    // anything read here would be payload we then had to replay.
    Ok(socket)
}
