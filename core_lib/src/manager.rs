use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::channel::{ChannelDirection, ChannelMessage};
use crate::errors::AppError;
use crate::hdl::{InboundRequest, OutboundPayload, OutboundRequest, State};
use crate::utils::RemoteDeviceInfo;

/// A dual-stack listener reports IPv4 peers as IPv4-mapped IPv6 addresses
/// (`::ffff:a.b.c.d`). Convert those back to plain IPv4 so connection ids and
/// logs stay identical regardless of how the listener happened to bind. Real
/// IPv6 peers pass through untouched.
fn normalize_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

const INNER_NAME: &str = "TcpServer";

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct SendInfo {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub ob: OutboundPayload,
}

pub struct TcpServer {
    endpoint_id: [u8; 4],
    tcp_listener: TcpListener,
    sender: Sender<ChannelMessage>,
    connect_receiver: Receiver<SendInfo>,
}

impl TcpServer {
    pub fn new(
        endpoint_id: [u8; 4],
        tcp_listener: TcpListener,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            endpoint_id,
            tcp_listener,
            sender,
            connect_receiver,
        })
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        loop {
            let cctk = ctk.clone();

            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                Some(i) = self.connect_receiver.recv() => {
                    info!("{INNER_NAME}: connect_receiver: got {:?}", i);
                    if let Err(e) = self.connect(cctk, i).await {
                        error!("{INNER_NAME}: error sending: {e}");
                    }
                }
                r = self.tcp_listener.accept() => {
                    match r {
                        Ok((socket, remote_addr)) => {
                            let remote_addr = normalize_addr(remote_addr);
                            trace!("{INNER_NAME}: new client: {remote_addr}");
                            let esender = self.sender.clone();
                            let csender = self.sender.clone();

                            tokio::spawn(async move {
                                let mut ir = InboundRequest::new(socket, remote_addr.to_string(), csender);

                                loop {
                                    match ir.handle().await {
                                        Ok(_) => {},
                                        Err(e) => match e.downcast_ref() {
                                            Some(AppError::NotAnError) => break,
                                            None => {
                                                if ir.state.state == State::Initial {
                                                    // A peer that opens a connection and leaves
                                                    // without speaking is a port scanner or a
                                                    // stray connect, not a failed transfer. One
                                                    // that sent something we couldn't handle is a
                                                    // real protocol failure and must not be silent.
                                                    let quiet = e
                                                        .downcast_ref::<std::io::Error>()
                                                        .is_some_and(|io| {
                                                            matches!(
                                                                io.kind(),
                                                                std::io::ErrorKind::UnexpectedEof
                                                                    | std::io::ErrorKind::ConnectionReset
                                                                    | std::io::ErrorKind::ConnectionAborted
                                                                    | std::io::ErrorKind::BrokenPipe
                                                            )
                                                        });

                                                    if quiet {
                                                        trace!("{INNER_NAME}: client left without speaking: {e}");
                                                    } else {
                                                        warn!("{INNER_NAME}: handshake failed on the first frame: {e}");
                                                    }
                                                    break;
                                                }

                                                if ir.state.state != State::Finished {
                                                    let _ = esender.send(ChannelMessage {
                                                        id: remote_addr.to_string(),
                                                        direction: ChannelDirection::LibToFront,
                                                        state: Some(State::Disconnected),
                                                        ..Default::default()
                                                    });
                                                }
                                                if ir.state.state == State::Finished {
                                                    debug!("{INNER_NAME}: client disconnected after transfer: {e}");
                                                } else {
                                                    error!("{INNER_NAME}: error while handling client: {e} ({:?})", ir.state.state);
                                                }
                                                break;
                                            }
                                        },
                                    }
                                }
                            });
                        },
                        Err(err) => {
                            error!("{INNER_NAME}: error accepting: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// To be called inside a separate task if we want to handle concurrency
    ///
    /// `si.addr` is normally `ip:port` from mDNS. A peer found over BLE has no
    /// IP, so `BleDiscovery` reports it as `ble:<bluetooth address>` and it is
    /// routed onto the Weave socket instead - which is the only way to reach a
    /// phone whose WiFi is off.
    pub async fn connect(&self, ctk: CancellationToken, si: SendInfo) -> Result<(), anyhow::Error> {
        debug!("{INNER_NAME}: Connecting to: {}", si.addr);

        #[cfg(feature = "experimental")]
        if let Some(address) = si.addr.strip_prefix("ble:") {
            // Stop advertising as a receiver for the duration, too. The phone
            // sees our advertisement, tries to fetch the full version over GATT,
            // and that collides with the Weave connection it is already serving
            // us on - its GATT server then fails to notify (status 133) and
            // tears the socket down mid-UKEY2. Same RAII reasoning as above.
            let _adv = crate::hdl::AdvertisePause::new();

            let address = address.to_string();
            let channel = crate::hdl::open_ble_by_address(&address).await?;

            // Stop BLE scanning now that we are connected - not before. A scan
            // and a connection share the radio, and scanning through a transfer
            // cost ~5x throughput, so the pause is worth holding for the
            // transfer. But acquiring it earlier deadlocked the connect itself:
            // if the stored address had gone stale (they rotate), the refresh
            // needs to scan to find the phone's current one, and a pause held
            // from the top blocked that scan - so every retry failed with
            // "could not find ... out of range" while discovery sat paused by
            // the very send that needed it. RAII, so every exit releases it.
            #[cfg(feature = "experimental")]
            let _pause = crate::hdl::DiscoveryPause::new();

            let (stream, upgrade_tx, switch_tx, switched) = (
                channel.stream,
                channel.upgrade_tx,
                channel.switch_tx,
                channel.switched,
            );
            // 8 KB, not the default 512 KB. Nothing else happens while a chunk
            // is being written, so the chunk size is also the granularity at
            // which this transport can react to anything.
            //
            // 512 KB was ~25s at BLE speeds: keepalives went unanswered until
            // the peer closed at 30s, and cancel was inert for the same 25s.
            // 32 KB fixed that but was still coarse - measured at the real
            // ~7 KB/s it is ~4.5s, and the bandwidth upgrade paid for it
            // directly: the peer released the old channel and the stream could
            // not move for a further ten seconds because a chunk was already in
            // flight. Nothing was waiting on the peer; it was purely this.
            //
            // 8 KB is ~1.1s at the same rate. Throughput is unaffected because
            // BLE is radio-bound, not per-chunk-overhead-bound - and this size
            // only applies until the upgrade lands, after which chunks go back
            // to 512 KB for the fast medium.
            //
            // NB: the first BLE outbound after launch tends to die in the
            // handshake and a manual re-send works. An automatic retry was
            // tried and reverted - it reconnected without disconnecting first,
            // so each retry hit an already-open Weave connection, timed out over
            // 10s, and rescanned for a phone that (being connected to us) was no
            // longer advertising, cascading into a shutdown hang. If retried
            // again it must disconnect the peripheral between attempts and not
            // block shutdown.
            return self
                .drive_outbound(
                    ctk,
                    si,
                    stream,
                    Some(8 * 1024),
                    Some((upgrade_tx, switch_tx, switched)),
                )
                .await;
        }

        let socket = TcpStream::connect(si.addr.clone()).await?;
        self.drive_outbound(ctk, si, socket, None, None).await
    }

    /// The transport-independent half of `connect`: everything after a stream
    /// exists. Generic so the same code drives TCP and the BLE Weave socket.
    async fn drive_outbound<S>(
        &self,
        ctk: CancellationToken,
        si: SendInfo,
        stream: S,
        chunk_size: Option<usize>,
        #[allow(unused_variables)] upgrade_sinks: Option<(
            tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
            tokio::sync::mpsc::UnboundedSender<()>,
            std::sync::Arc<tokio::sync::Notify>,
        )>,
    ) -> Result<(), anyhow::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // The key the front end tracks this transfer by, kept before `si` is
        // taken apart so the failure path can report under the same one.
        let transfer_id = si.id.clone();

        let mut or = OutboundRequest::new(
            self.endpoint_id,
            stream,
            si.id,
            self.sender.clone(),
            si.ob,
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: si.name,
            },
        );

        if let Some(n) = chunk_size {
            or.set_chunk_size(n);
        }
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        if let Some((upgrade_tx, switch_tx, switched)) = upgrade_sinks {
            or.set_upgrade_sinks(upgrade_tx, switch_tx, switched);
        }

        // Send connection request
        or.send_connection_request().await?;
        // Send UKEY init
        or.send_ukey2_client_init().await?;

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                },
                r = or.handle() => {
                    if let Err(e) = r {
                        match e.downcast_ref() {
                            Some(AppError::NotAnError) => break,
                            None => {
                                // Report a failure in State::Initial too.
                                //
                                // Staying quiet is right for the *inbound*
                                // server, where a peer that connects and says
                                // nothing is a port scanner. Here the user
                                // pressed send, so silence is the worst
                                // outcome: a peer that accepts the connection
                                // and then closes - a phone advertising Nearby
                                // but not on the receiving screen - failed with
                                // nothing shown at all.
                                if or.state.state != State::Finished && or.state.state != State::Cancelled {
                                    // `si.id`, not `si.addr`. Every other message
                                    // about this transfer - all the progress
                                    // updates from OutboundRequest - is keyed on
                                    // si.id, and over BLE the two differ: the id
                                    // is "16:57:DC:A0:5E:40" while the address is
                                    // "ble:16:57:DC:A0:5E:40". Reporting the end
                                    // under the address meant the front end never
                                    // saw this transfer finish, left it showing as
                                    // sending, and sent the user's cancel presses
                                    // to a session that had already gone - seven
                                    // of them, all inert.
                                    let _ = self.sender.clone().send(ChannelMessage {
                                        id: transfer_id.clone(),
                                        direction: ChannelDirection::LibToFront,
                                        state: Some(State::Disconnected),
                                        ..Default::default()
                                    });
                                }
                                if or.state.state == State::Finished || or.state.state == State::Cancelled {
                                    debug!("{INNER_NAME}: client disconnected after {:?}: {e}", or.state.state);
                                } else {
                                    error!("{INNER_NAME}: error while handling client: {e} ({:?})", or.state.state);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
