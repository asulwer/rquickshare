use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Receiver;
use tokio::sync::watch;
use tokio::time::{interval_at, Instant};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::utils::{gen_mdns_endpoint_info, gen_mdns_name, DeviceType};

const INNER_NAME: &str = "MDnsServer";
const TICK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub enum Visibility {
    Visible = 0,
    Invisible = 1,
    Temporarily = 2,
}

impl Visibility {
    pub fn from_raw_value(value: u64) -> Self {
        match value {
            0 => Visibility::Visible,
            1 => Visibility::Invisible,
            2 => Visibility::Temporarily,
            _ => unreachable!(),
        }
    }
}

pub struct MDnsServer {
    daemon: ServiceDaemon,
    service_info: ServiceInfo,
    ble_receiver: Receiver<()>,
    visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
    visibility_receiver: watch::Receiver<Visibility>,
}

impl MDnsServer {
    pub fn new(
        endpoint_id: [u8; 4],
        service_port: u16,
        ble_receiver: Receiver<()>,
        visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
        visibility_receiver: watch::Receiver<Visibility>,
    ) -> Result<Self, anyhow::Error> {
        let service_info = Self::build_service(endpoint_id, service_port, DeviceType::Laptop)?;

        Ok(Self {
            daemon: ServiceDaemon::new()?,
            service_info,
            ble_receiver,
            visibility_sender,
            visibility_receiver,
        })
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");
        let monitor = self.daemon.monitor()?;
        let ble_receiver = &mut self.ble_receiver;
        let mut visibility = *self.visibility_receiver.borrow();
        let mut interval = interval_at(Instant::now() + TICK_INTERVAL, TICK_INTERVAL);

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                r = monitor.recv_async() => {
                    match r {
                        Ok(_) => continue,
                        Err(err) => return Err(err.into()),
                    }
                },
                _ = self.visibility_receiver.changed() => {
                    visibility = *self.visibility_receiver.borrow_and_update();

                    debug!("{INNER_NAME}: visibility changed: {visibility:?}");
                    if visibility == Visibility::Visible {
                        self.daemon.register(self.service_info.clone())?;
                    } else if visibility == Visibility::Invisible {
                        let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
                        let _ = receiver.recv();
                    } else if visibility == Visibility::Temporarily {
                        self.daemon.register(self.service_info.clone())?;
                        interval.reset();
                    }
                }
                _ = ble_receiver.recv() => {
                    if visibility == Visibility::Invisible {
                        continue;
                    }

                    debug!("{INNER_NAME}: ble_receiver: got event");
                    // Android can sometime not see the mDNS service if the service
                    // was running BEFORE Android started the Discovery phase for QuickShare.
                    // So resend a broadcast if there's a android device sending.
                    //
                    // This was `register_resend()`, which exists only on the fork and
                    // is the sole reason we were pinned to it. Upstream documents the
                    // replacement on `register`: "To re-announce a service with an
                    // updated service_info, just call this register function again. No
                    // need to call unregister first." `register_service` sends the
                    // unsolicited response immediately, so this re-announces whether or
                    // not we are already registered - collapsing both arms into one.
                    self.daemon.register(self.service_info.clone())?;
                },
                _ = interval.tick() => {
                    if visibility != Visibility::Temporarily {
                        continue;
                    }

                    let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
                    let _ = receiver.recv();
                    let _ = self.visibility_sender.lock().unwrap().send(Visibility::Invisible);
                }
            }
        }

        // Unregister the mDNS service - we're shutting down
        let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
        if let Ok(event) = receiver.recv() {
            info!("MDnsServer: service unregistered: {event:?}");
        }

        // Shut the daemon down so its background thread stops cleanly instead
        // of being orphaned, which otherwise floods the log with
        // "sending on a closed channel" errors. Await the shutdown response so
        // the daemon doesn't log a "failed to send response of shutdown" error.
        if let Ok(receiver) = self.daemon.shutdown() {
            let _ = receiver.recv_async().await;
        }

        Ok(())
    }

    fn build_service(
        endpoint_id: [u8; 4],
        service_port: u16,
        device_type: DeviceType,
    ) -> Result<ServiceInfo, anyhow::Error> {
        let name = gen_mdns_name(endpoint_id);
        let hostname = ::hostname::get()?.to_string_lossy().into_owned();
        info!("Broadcasting with: {hostname}");
        let endpoint_info = gen_mdns_endpoint_info(device_type as u8, &hostname);

        // The mDNS host name must be fully qualified; the *display* name must not
        // be. These were the same string until mdns-sd 0.11.0 made `register()`
        // reject a hostname that doesn't end in ".local." - and since `register`
        // is called with `?`, a bare "AaronPC" doesn't just fail to announce, it
        // kills the whole MDnsServer task on the first visibility change. The
        // 0.10.4 fork we came from predates that check, which is why this only
        // broke on the upgrade.
        let mdns_hostname = if hostname.ends_with(".local.") {
            hostname.clone()
        } else {
            format!("{hostname}.local.")
        };

        let properties = [("n", endpoint_info)];
        let mut si = ServiceInfo::new(
            "_FC9F5ED42C8A._tcp.local.",
            &name,
            &mdns_hostname,
            "",
            service_port,
            &properties[..],
        )?
        .enable_addr_auto();

        // Was `enable_addr_auto(AddrType::V4)` on the fork. Upstream splits the
        // two concerns: auto-fill addresses, and restrict which interfaces are
        // considered. Per upstream's docs on `set_interfaces`: "When ips are
        // auto-detected (via 'enable_addr_auto') only addresses on these
        // interfaces will be considered." This is per-service, where the fork's
        // enum was a global on the address type - so it's a strict improvement.
        // IPv4-only is deliberate: see the parked IPv6 entry in TODO.md.
        si.set_interfaces(vec![IfKind::IPv4]);

        Ok(si)
    }
}
