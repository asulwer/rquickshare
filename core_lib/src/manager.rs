use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use p256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::channel::{ChannelDirection, ChannelMessage};
use crate::errors::AppError;
use crate::hdl::{InboundRequest, OutboundPayload, OutboundRequest, State};
use crate::qr::QrPeers;
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
    /// Peers that scanned our QR code, whose accept prompt a send can skip.
    qr_peers: QrPeers,
    /// Peers with an outbound attempt already running. A send is one entire
    /// transfer long, so without this a second request for the same peer would
    /// open a second connection to a device that is already receiving.
    outbound_inflight: Arc<Mutex<HashSet<String>>>,
    /// Signalled whenever one of our connections ends and gives its socket
    /// back. Out of handles, accept() waits on this rather than spinning or
    /// sleeping: a connection closing is what frees one.
    connection_closed: Arc<tokio::sync::Notify>,
}

/// Takes a send's id out of `outbound_inflight` when dropped.
struct InflightGuard {
    set: Arc<Mutex<HashSet<String>>>,
    id: String,
    closed: Arc<tokio::sync::Notify>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        self.closed.notify_one();
    }
}

/// Signals `TcpServer::connection_closed` when an inbound connection's task
/// ends, however it ends.
struct ClosedSignal(Arc<tokio::sync::Notify>);

impl Drop for ClosedSignal {
    fn drop(&mut self) {
        // notify_one keeps a permit when nobody is waiting yet, so a
        // connection that closes just before accept() starts waiting still
        // wakes it.
        self.0.notify_one();
    }
}

/// What an accept() error means for the server.
#[derive(Debug, PartialEq)]
enum AcceptError {
    /// About one incoming connection (reset or aborted before we took it, a
    /// network blip). The listener is fine.
    Connection,
    /// Out of sockets or file handles. Retrying at once would fail again.
    OutOfHandles,
    /// The listener itself is unusable.
    Fatal,
}

fn classify_accept_error(e: &std::io::Error) -> AcceptError {
    // EMFILE / ENFILE / ENOBUFS, and WSAEMFILE / WSAENOBUFS on Windows.
    #[cfg(target_os = "linux")]
    const OUT_OF_HANDLES: &[i32] = &[24, 23, 105];
    #[cfg(target_os = "macos")]
    const OUT_OF_HANDLES: &[i32] = &[24, 23, 55];
    #[cfg(target_os = "windows")]
    const OUT_OF_HANDLES: &[i32] = &[10024, 10055];
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    const OUT_OF_HANDLES: &[i32] = &[];

    // EBADF / ENOTSOCK, and WSAENOTSOCK / WSAEINVAL on Windows.
    #[cfg(target_os = "linux")]
    const BROKEN_LISTENER: &[i32] = &[9, 88];
    #[cfg(target_os = "macos")]
    const BROKEN_LISTENER: &[i32] = &[9, 38];
    #[cfg(target_os = "windows")]
    const BROKEN_LISTENER: &[i32] = &[10038, 10022];
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    const BROKEN_LISTENER: &[i32] = &[];

    match e.raw_os_error() {
        Some(code) if OUT_OF_HANDLES.contains(&code) => AcceptError::OutOfHandles,
        Some(code) if BROKEN_LISTENER.contains(&code) => AcceptError::Fatal,
        _ if e.kind() == std::io::ErrorKind::OutOfMemory => AcceptError::OutOfHandles,
        _ if e.kind() == std::io::ErrorKind::InvalidInput => AcceptError::Fatal,
        _ => AcceptError::Connection,
    }
}

/// What the BLE send path hands `drive_outbound` so a transfer can move onto
/// WiFi: where the upgraded socket arrives, the release signal, and the splice
/// notification. Only Windows can join the phone's hotspot, so elsewhere the
/// type has no values and a send always stays on the transport it started on.
#[cfg(all(feature = "experimental", target_os = "windows"))]
type UpgradeSinks = (
    tokio::sync::mpsc::UnboundedSender<tokio::net::TcpStream>,
    tokio::sync::mpsc::UnboundedSender<()>,
    std::sync::Arc<tokio::sync::Notify>,
);
#[cfg(not(all(feature = "experimental", target_os = "windows")))]
type UpgradeSinks = std::convert::Infallible;

/// How one send differs from the defaults: what its transport needs, and
/// whether the peer scanned our QR code.
#[derive(Default)]
struct OutboundOptions {
    chunk_size: Option<usize>,
    upgrade_sinks: Option<UpgradeSinks>,
    /// Signs the handshake so the peer that scanned our QR code accepts
    /// without asking. See `qr::handshake_signature`.
    qr_key: Option<SigningKey>,
}

impl TcpServer {
    pub fn new(
        endpoint_id: [u8; 4],
        tcp_listener: TcpListener,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
        qr_peers: QrPeers,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            endpoint_id,
            tcp_listener,
            sender,
            connect_receiver,
            qr_peers,
            outbound_inflight: Arc::new(Mutex::new(HashSet::new())),
            connection_closed: Arc::new(tokio::sync::Notify::new()),
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

                    // Spawned, like the inbound branch below, because a send is
                    // an entire transfer long. Awaited here it held the whole
                    // select loop: queued sends ran strictly one after another
                    // and `accept()` was starved for the duration. A user who
                    // clicked an unreachable BLE peer a few times (each attempt
                    // failing only on a 5-40s timeout) then waited minutes for
                    // the queue to reach the LAN entry that actually worked.
                    let id = i.id.clone();
                    if !self.outbound_inflight.lock().unwrap().insert(id.clone()) {
                        info!("{INNER_NAME}: already sending to {id}, ignoring duplicate request");
                        continue;
                    }

                    let endpoint_id = self.endpoint_id;
                    let sender = self.sender.clone();
                    let inflight = self.outbound_inflight.clone();
                    let closed = self.connection_closed.clone();
                    let qr_key = self.qr_peers.lock().ok().and_then(|peers| peers.get(&id).cloned());

                    tokio::spawn(async move {
                        // Released however the task ends, a panic included;
                        // otherwise every later send to this device would be
                        // ignored as a duplicate until restart.
                        let _inflight = InflightGuard { set: inflight, id: id.clone(), closed };

                        if let Err(e) = Self::connect(endpoint_id, sender.clone(), cctk, i, qr_key).await {
                            error!("{INNER_NAME}: error sending: {e}");

                            // Failing to even get a stream (no route, BLE peer
                            // out of range, handshake refused) used to be logged
                            // and nothing else, so the front end kept showing the
                            // device as if the click had never happened. Report it
                            // under the same id every other message for this
                            // transfer uses.
                            let _ = sender.send(ChannelMessage {
                                id: id.clone(),
                                direction: ChannelDirection::LibToFront,
                                state: Some(State::Disconnected),
                                ..Default::default()
                            });
                        }
                    });
                }
                r = self.tcp_listener.accept() => {
                    match r {
                        Ok((socket, remote_addr)) => {
                            let remote_addr = normalize_addr(remote_addr);
                            trace!("{INNER_NAME}: new client: {remote_addr}");
                            let esender = self.sender.clone();
                            let csender = self.sender.clone();
                            let closed = ClosedSignal(self.connection_closed.clone());

                            tokio::spawn(async move {
                                let _closed = closed;
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
                        // One bad accept used to stop the server, so nothing more
                        // was received until a restart.
                        Err(err) => match classify_accept_error(&err) {
                            AcceptError::Connection => {
                                debug!("{INNER_NAME}: dropped an incoming connection: {err}");
                            }
                            AcceptError::OutOfHandles => {
                                warn!(
                                    "{INNER_NAME}: out of sockets accepting ({err}); waiting for a connection to close"
                                );
                                tokio::select! {
                                    _ = ctk.cancelled() => break,
                                    _ = self.connection_closed.notified() => {}
                                }
                            }
                            AcceptError::Fatal => {
                                error!("{INNER_NAME}: the listener failed, no longer receiving: {err}");
                                break;
                            }
                        },
                    }
                }
            }
        }

        Ok(())
    }

    /// Runs one outbound attempt to completion; always spawned, never awaited
    /// on the accept loop.
    ///
    /// `si.addr` is normally `ip:port` from mDNS. A peer found over BLE has no
    /// IP, so `BleDiscovery` reports it as `ble:<bluetooth address>` and it is
    /// routed onto the Weave socket instead - which is the only way to reach a
    /// phone whose WiFi is off.
    ///
    /// Takes the pieces of the server it needs by value rather than `&self`
    /// so the whole attempt can be driven from a spawned task. `qr_key` is set
    /// when the peer scanned our QR code (see `qr::QrPeers`).
    async fn connect(
        endpoint_id: [u8; 4],
        sender: Sender<ChannelMessage>,
        ctk: CancellationToken,
        si: SendInfo,
        qr_key: Option<SigningKey>,
    ) -> Result<(), anyhow::Error> {
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
            let channel = crate::hdl::open_ble_by_address(&address, &ctk).await?;

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

            #[cfg(target_os = "windows")]
            let (stream, upgrade_sinks) = (
                channel.stream,
                Some((channel.upgrade_tx, channel.switch_tx, channel.switched)),
            );
            #[cfg(not(target_os = "windows"))]
            let (stream, upgrade_sinks) = (channel.stream, None);
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
            return Self::drive_outbound(
                endpoint_id,
                sender,
                ctk,
                si,
                stream,
                OutboundOptions {
                    chunk_size: Some(8 * 1024),
                    upgrade_sinks,
                    qr_key,
                },
            )
            .await;
        }

        let socket = TcpStream::connect(si.addr.clone()).await?;
        let options = OutboundOptions {
            qr_key,
            ..Default::default()
        };
        Self::drive_outbound(endpoint_id, sender, ctk, si, socket, options).await
    }

    /// The transport-independent half of `connect`: everything after a stream
    /// exists. Generic so the same code drives TCP and the BLE Weave socket.
    async fn drive_outbound<S>(
        endpoint_id: [u8; 4],
        sender: Sender<ChannelMessage>,
        ctk: CancellationToken,
        si: SendInfo,
        stream: S,
        options: OutboundOptions,
    ) -> Result<(), anyhow::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // The key the front end tracks this transfer by, kept before `si` is
        // taken apart so the failure path can report under the same one.
        let transfer_id = si.id.clone();

        let mut or = OutboundRequest::new(
            endpoint_id,
            stream,
            si.id,
            sender.clone(),
            si.ob,
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: si.name,
            },
        );

        if let Some(n) = options.chunk_size {
            or.set_chunk_size(n);
        }
        if let Some(key) = options.qr_key {
            or.set_qr_key(key);
        }
        if let Some(sinks) = options.upgrade_sinks {
            #[cfg(all(feature = "experimental", target_os = "windows"))]
            {
                let (upgrade_tx, switch_tx, switched) = sinks;
                or.set_upgrade_sinks(upgrade_tx, switch_tx, switched);
            }
            #[cfg(not(all(feature = "experimental", target_os = "windows")))]
            match sinks {}
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
                                    let _ = sender.send(ChannelMessage {
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

        // Every way out of the loop above ends the transfer, shutdown through
        // `ctk` included, so this is where a WiFi upgrade is undone for all of
        // them. A second call after `handle` already did it is harmless.
        #[cfg(all(feature = "experimental", target_os = "windows"))]
        or.cleanup_upgrade().await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_connection_does_not_stop_the_server() {
        let e = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert_eq!(classify_accept_error(&e), AcceptError::Connection);
        let e = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        assert_eq!(classify_accept_error(&e), AcceptError::Connection);
    }

    #[test]
    fn running_out_of_handles_waits_instead_of_spinning() {
        #[cfg(target_os = "windows")]
        let code = 10024;
        #[cfg(not(target_os = "windows"))]
        let code = 24;
        let e = std::io::Error::from_raw_os_error(code);
        assert_eq!(classify_accept_error(&e), AcceptError::OutOfHandles);
    }

    #[test]
    fn a_broken_listener_is_fatal() {
        #[cfg(target_os = "windows")]
        let code = 10038;
        #[cfg(not(target_os = "windows"))]
        let code = 9;
        let e = std::io::Error::from_raw_os_error(code);
        assert_eq!(classify_accept_error(&e), AcceptError::Fatal);
    }
}
