use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Receiver;
use tokio::sync::watch;
use tokio::time::{interval_at, Instant};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::utils::{device_name, gen_mdns_endpoint_info, gen_mdns_name, DeviceType};

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
    // Ticks when the user renames the device; the new name is read from
    // `device_name()` rather than carried on the channel.
    device_name_receiver: watch::Receiver<()>,
    // Ticks when the window is (re)opened; we re-announce the unchanged record
    // so a peer that began browsing after startup can still find us.
    reannounce_receiver: watch::Receiver<()>,
    // Kept so the service record can be rebuilt under a new name.
    endpoint_id: [u8; 4],
    service_port: u16,
    device_type: DeviceType,
}

impl MDnsServer {
    pub fn new(
        endpoint_id: [u8; 4],
        service_port: u16,
        ble_receiver: Receiver<()>,
        visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
        visibility_receiver: watch::Receiver<Visibility>,
        device_name_receiver: watch::Receiver<()>,
        reannounce_receiver: watch::Receiver<()>,
    ) -> Result<Self, anyhow::Error> {
        let device_type = DeviceType::Laptop;
        let service_info = Self::build_service(endpoint_id, service_port, device_type.clone())?;

        Ok(Self {
            daemon: ServiceDaemon::new()?,
            service_info,
            ble_receiver,
            visibility_sender,
            visibility_receiver,
            device_name_receiver,
            reannounce_receiver,
            endpoint_id,
            service_port,
            device_type,
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
                _ = self.device_name_receiver.changed() => {
                    self.device_name_receiver.borrow_and_update();

                    // The instance name is derived from the endpoint id, not the
                    // device name, so the fullname is unchanged and there is no
                    // stale record to unregister - upstream documents calling
                    // register() again as the way to re-announce updated info.
                    let rebuilt = Self::build_service(
                        self.endpoint_id,
                        self.service_port,
                        self.device_type.clone(),
                    )?;
                    self.service_info = rebuilt;

                    debug!("{INNER_NAME}: device name changed, re-announcing");
                    if visibility != Visibility::Invisible {
                        self.daemon.register(self.service_info.clone())?;
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
                _ = self.reannounce_receiver.changed() => {
                    self.reannounce_receiver.borrow_and_update();

                    // Nothing to announce while hidden, and re-registering would
                    // undo the unregister that Invisible performed.
                    if visibility == Visibility::Invisible {
                        continue;
                    }

                    // Same re-announce as the ble arm: `register` re-sends the
                    // unsolicited response for the unchanged record, so a peer
                    // that started browsing after our startup announcement now
                    // sees us. Triggered when the window is (re)opened.
                    debug!("{INNER_NAME}: reannounce_receiver: re-announcing");
                    self.daemon.register(self.service_info.clone())?;
                },
                _ = interval.tick() => {
                    // Temporarily has expired: stop advertising and flip back to
                    // Invisible.
                    if visibility == Visibility::Temporarily {
                        let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
                        let _ = receiver.recv();
                        let _ = self.visibility_sender.lock().unwrap().send(Visibility::Invisible);
                        continue;
                    }

                    // Periodic re-announcement while visible. A device left
                    // Visible in the tray otherwise announces only once at
                    // startup, so a peer that starts browsing later never finds
                    // it until something forces a fresh `register`.
                    if visibility == Visibility::Visible {
                        trace!("{INNER_NAME}: periodic re-announce");
                        self.daemon.register(self.service_info.clone())?;
                    }
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
        // The advertised *display* name, which the user can override.
        let display_name = device_name();
        info!("Broadcasting with: {display_name}");
        let endpoint_info = gen_mdns_endpoint_info(device_type as u8, &display_name);

        // The host record stays on the real hostname even after a rename. It is
        // a network name other hosts resolve, not something we display, and
        // pointing it at a user-chosen string invites collisions on a LAN where
        // two machines get the same nickname.
        let hostname = ::hostname::get()?.to_string_lossy().into_owned();

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
