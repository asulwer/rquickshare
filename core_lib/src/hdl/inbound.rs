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
use libaes::{Cipher, AES_256_KEY_LEN};
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
    socket: S,
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
}

impl<S: AsyncRead + AsyncWrite + Unpin> InboundRequest<S> {
    pub fn new(socket: S, id: String, sender: Sender<ChannelMessage>) -> Self {
        let receiver = sender.subscribe();

        Self {
            socket,
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
        }
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        // Buffer for the 4-byte length
        let mut length_buf = [0u8; 4];

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
            h = stream_read_exact(&mut self.socket, &mut length_buf) => {
                h?;

                self._handle(length_buf).await?
            }
        }

        Ok(())
    }

    pub async fn _handle(&mut self, length_buf: [u8; 4]) -> Result<(), anyhow::Error> {
        let msg_length = u32::from_be_bytes(length_buf) as usize;
        // Ensure the message length is not unreasonably big to avoid allocation attacks
        if msg_length > SANE_FRAME_LENGTH as usize {
            error!("Message length too big");
            return Err(anyhow!("value"));
        }

        // Allocate buffer for the actual message and read it
        let mut frame_data = vec![0u8; msg_length];
        stream_read_exact(&mut self.socket, &mut frame_data).await?;

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
        &self,
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
                    supported_medium: vec![Medium::WifiDirect.into(), Medium::WifiLan.into()],
                    is_request: Some(false),
                }),
                ..Default::default()
            }),
        };
        self.encrypt_and_send(&frame).await?;
        info!("Bandwidth upgrade: replied with supported mediums [WIFI_DIRECT, WIFI_LAN]");
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
                            write_all_at(
                                file_internal.file.as_ref().unwrap(),
                                chunk.body(),
                                current_offset as u64,
                            )?;
                            file_internal.bytes_transferred += chunk_size as i64;

                            self.update_state(
                                |e| {
                                    if let Some(tmd) = e.transfer_metadata.as_mut() {
                                        tmd.ack_bytes += chunk_size as u64;
                                    }
                                },
                                true,
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
                trace!("Sending keepalive");
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
                if is_path_request {
                    info!("Phone requested an upgrade path; offering WIFI_DIRECT");
                    if let Err(e) = self.offer_wifi_direct_upgrade().await {
                        warn!("offer_wifi_direct_upgrade (on request) failed: {}", e);
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
        &self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        if v1_frame.paired_key_result.is_none() {
            return Err(anyhow!("Missing required fields"));
        }

        Ok(())
    }

    async fn process_introduction(
        &mut self,
        v1_frame: &sharing_nearby::V1Frame,
    ) -> Result<(), anyhow::Error> {
        let introduction = v1_frame
            .introduction
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

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
        if std::env::var("RQS_TRY_WIFI_DIRECT_UPGRADE").is_ok() {
            if let Err(e) = self.offer_wifi_direct_upgrade().await {
                warn!("offer_wifi_direct_upgrade failed: {}", e);
            }
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
async fn introduce_upgraded_channel(mut socket: tokio::net::TcpStream) -> Result<(), anyhow::Error> {
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

    // Hold the socket open and report whatever the phone does next. Dropping it
    // here would close the group's only connection and the phone would call the
    // upgrade failed before we ever got the chance to move onto it.
    //
    // Read rather than `mem::forget` to keep it alive: forgetting leaks the fd
    // for the life of the process, which is the mistake the hotspot code made.
    // This ends on EOF - when the phone gives up - or when the transfer finishes
    // and the group is torn down.
    //
    // What arrives here is the next question worth answering. The phone should
    // now be waiting for LAST_WRITE_TO_PRIOR_CHANNEL on the *old* channel and
    // send nothing at all on this one; bytes turning up instead would mean it
    // expects us further along the protocol than we are.
    let mut buf = [0u8; 1024];
    loop {
        match tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await {
            Ok(0) => {
                info!("Bandwidth upgrade: phone closed the upgraded socket");
                return Ok(());
            }
            Ok(n) => info!(
                "Bandwidth upgrade: {n} bytes on the upgraded socket: {:02x?}",
                &buf[..n.min(64)]
            ),
            Err(e) => {
                info!("Bandwidth upgrade: upgraded socket ended: {e}");
                return Ok(());
            }
        }
    }
}
