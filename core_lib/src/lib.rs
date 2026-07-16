#[macro_use]
extern crate log;

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::anyhow;
use channel::ChannelMessage;
#[cfg(all(feature = "experimental", any(target_os = "linux", target_os = "windows")))]
use hdl::BleAdvertiser;
use hdl::MDnsDiscovery;
use once_cell::sync::Lazy;
use rand::distr::Alphanumeric;
use rand::Rng;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[cfg(feature = "experimental")]
use crate::hdl::BleListener;
use crate::hdl::MDnsServer;
use crate::manager::TcpServer;

/// Bind a TCP listener that accepts both IPv4 and IPv6 peers.
///
/// Windows (and some BSDs) default `IPV6_V6ONLY` to on, which would make an
/// `[::]` socket refuse IPv4 connections, so clear it explicitly to get a
/// single dual-stack socket.
fn bind_dual_stack(port: u32) -> Result<TcpListener, anyhow::Error> {
    let port: u16 = port
        .try_into()
        .map_err(|_| anyhow!("invalid port number: {port}"))?;
    let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port);
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;

    Ok(TcpListener::from_std(socket.into())?)
}

pub mod channel;
mod errors;
mod hdl;
mod manager;
mod qr;
mod utils;

pub use hdl::{EndpointInfo, OutboundPayload, State, Visibility};
pub use manager::SendInfo;
pub use utils::DeviceType;

pub mod sharing_nearby {
    include!(concat!(env!("OUT_DIR"), "/sharing.nearby.rs"));
}

pub mod securemessage {
    include!(concat!(env!("OUT_DIR"), "/securemessage.rs"));
}

pub mod securegcm {
    include!(concat!(env!("OUT_DIR"), "/securegcm.rs"));
}

pub mod location_nearby_connections {
    include!(concat!(env!("OUT_DIR"), "/location.nearby.connections.rs"));
}

static CUSTOM_DOWNLOAD: Lazy<RwLock<Option<PathBuf>>> = Lazy::new(|| RwLock::new(None));

#[derive(Debug)]
pub struct RQS {
    tracker: Option<TaskTracker>,
    ctoken: Option<CancellationToken>,
    // Discovery token is different than ctoken because he is on his own
    // - can be cancelled while the ctoken is still active
    discovery_ctk: Option<CancellationToken>,

    // Used to trigger a change in the mDNS visibility (and later on, BLE)
    pub visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
    visibility_receiver: watch::Receiver<Visibility>,

    // Only used to send the info "a nearby device is sharing"
    ble_sender: broadcast::Sender<()>,

    port_number: Option<u32>,

    pub message_sender: broadcast::Sender<ChannelMessage>,
}

impl Default for RQS {
    fn default() -> Self {
        Self::new(Visibility::Visible, None, None)
    }
}

impl RQS {
    pub fn new(
        visibility: Visibility,
        port_number: Option<u32>,
        download_path: Option<PathBuf>,
    ) -> Self {
        let mut guard = CUSTOM_DOWNLOAD.write().unwrap();
        *guard = download_path;

        let (message_sender, _) = broadcast::channel(50);
        let (ble_sender, _) = broadcast::channel(5);

        // Define default visibility as per the args inside the new()
        let (visibility_sender, visibility_receiver) = watch::channel(Visibility::Invisible);
        let _ = visibility_sender.send(visibility);

        Self {
            tracker: None,
            ctoken: None,
            discovery_ctk: None,
            visibility_sender: Arc::new(Mutex::new(visibility_sender)),
            visibility_receiver,
            ble_sender,
            port_number,
            message_sender,
        }
    }

    pub async fn run(
        &mut self,
    ) -> Result<(mpsc::Sender<SendInfo>, broadcast::Receiver<()>), anyhow::Error> {
        let tracker = TaskTracker::new();
        let ctoken = CancellationToken::new();
        self.tracker = Some(tracker.clone());
        self.ctoken = Some(ctoken.clone());

        let endpoint_id: Vec<u8> = rand::rng().sample_iter(Alphanumeric).take(4).collect();
        // Bind dual-stack so peers can reach us over IPv4 or IPv6. If the
        // platform won't hand us a dual-stack socket, fall back to IPv4-only
        // rather than failing to start.
        let port = self.port_number.unwrap_or(0);
        let tcp_listener = match bind_dual_stack(port) {
            Ok(listener) => listener,
            Err(e) => {
                warn!("Dual-stack bind failed ({e}), falling back to IPv4-only");
                TcpListener::bind(format!("0.0.0.0:{port}")).await?
            }
        };
        let binded_addr = tcp_listener.local_addr()?;
        info!("TcpListener on: {}", binded_addr);

        // MPSC for the TcpServer
        let send_channel = mpsc::channel(10);
        // Start TcpServer in own "task"
        let mut server = TcpServer::new(
            endpoint_id[..4].try_into()?,
            tcp_listener,
            self.message_sender.clone(),
            send_channel.1,
        )?;
        let ctk = ctoken.clone();
        tracker.spawn(async move {
            if let Err(e) = server.run(ctk).await {
                error!("TcpServer: stopped with an error: {e}");
            }
        });

        #[cfg(feature = "experimental")]
        {
            // Don't threat BleListener error as fatal, it's a nice to have.
            if let Ok(ble) = BleListener::new(self.ble_sender.clone()).await {
                let ctk = ctoken.clone();
                tracker.spawn(async move {
                    if let Err(e) = ble.run(ctk).await {
                        error!("BleListener: stopped with an error: {e}");
                    }
                });
            }
        }

        // Start MDnsServer in own "task"
        let mut mdns = MDnsServer::new(
            endpoint_id[..4].try_into()?,
            binded_addr.port(),
            self.ble_sender.subscribe(),
            self.visibility_sender.clone(),
            self.visibility_receiver.clone(),
        )?;
        let ctk = ctoken.clone();
        // Log the outcome rather than dropping it. These `run` methods return
        // Result and use `?` internally (e.g. `daemon.register(..)?`), so a
        // single failure ends the task - and with the Result discarded, the
        // service just silently stops existing. That is exactly how a bare
        // hostname failing mdns-sd's `.local.` check looked: "service starting",
        // "visibility changed: Visible", then nothing, forever, with no error.
        tracker.spawn(async move {
            if let Err(e) = mdns.run(ctk).await {
                error!("MDnsServer: stopped with an error: {e}");
            }
        });

        // NOTE (issue #425): a BLE "receiver" advertiser is implemented in
        // hdl::BleReceiverAdvertiser (with the ble_receiver builders, see
        // docs/ble-receiver-discovery.md) but is intentionally NOT started here.
        // Making a phone list rquickshare over BLE additionally requires a BLE
        // GATT server to serve the full advertisement, which is not implemented.
        tracker.close();

        Ok((send_channel.0, self.ble_sender.subscribe()))
    }

    pub fn discovery(
        &mut self,
        sender: broadcast::Sender<EndpointInfo>,
    ) -> Result<(), anyhow::Error> {
        self.start_discovery(sender, None)
    }

    /// Start discovery and return a QR URL to display to the peer.
    ///
    /// A phone that opens this URL starts advertising **even while hidden**, so
    /// this is how we reach a device that isn't set to "Everyone" visibility -
    /// the scan itself is the authorization. The returned peer arrives on
    /// `sender` like any other, with the name recovered from its QR data.
    pub fn discovery_with_qr(
        &mut self,
        sender: broadcast::Sender<EndpointInfo>,
    ) -> Result<String, anyhow::Error> {
        let session = qr::QrSession::new()?;
        let url = session.url.clone();
        self.start_discovery(sender, Some(session))?;

        Ok(url)
    }

    fn start_discovery(
        &mut self,
        sender: broadcast::Sender<EndpointInfo>,
        qr_session: Option<qr::QrSession>,
    ) -> Result<(), anyhow::Error> {
        let tracker = self
            .tracker
            .as_ref()
            .ok_or_else(|| anyhow!("The service wasn't first started"))?;

        let ctk = CancellationToken::new();
        self.discovery_ctk = Some(ctk.clone());

        #[cfg(all(feature = "experimental", any(target_os = "linux", target_os = "windows")))]
        {
            let ctk_blea = ctk.clone();
            tracker.spawn(async move {
                let blea = match BleAdvertiser::new().await {
                    Ok(b) => b,
                    Err(e) => {
                        error!("Couldn't init BleAdvertiser: {}", e);
                        return;
                    }
                };

                if let Err(e) = blea.run(ctk_blea).await {
                    error!("Couldn't start BleAdvertiser: {}", e);
                }
            });
        }

        let mut discovery = MDnsDiscovery::new(sender)?;
        if let Some(session) = qr_session {
            discovery = discovery.with_qr_session(session);
        }
        tracker.spawn(async move {
            if let Err(e) = discovery.run(ctk.clone()).await {
                error!("MDnsDiscovery: stopped with an error: {e}");
            }
        });

        Ok(())
    }

    pub fn stop_discovery(&mut self) {
        if let Some(discovert_ctk) = &self.discovery_ctk {
            discovert_ctk.cancel();
            self.discovery_ctk = None;
        }
    }

    pub fn change_visibility(&mut self, nv: Visibility) {
        self.visibility_sender
            .lock()
            .unwrap()
            .send_modify(|state| *state = nv);
    }

    pub async fn stop(&mut self) {
        self.stop_discovery();

        if let Some(ctoken) = &self.ctoken {
            ctoken.cancel();
        }

        if let Some(tracker) = &self.tracker {
            tracker.wait().await;
        }

        self.ctoken = None;
        self.tracker = None;
    }

    // Setting None here will resume the default settings
    pub fn set_download_path(&self, p: Option<PathBuf>) {
        debug!("Setting the download path to {:?}", p);
        let mut guard = CUSTOM_DOWNLOAD.write().unwrap();
        *guard = p;
    }
}
