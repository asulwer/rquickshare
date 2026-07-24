//! Windows soft-AP (Mobile Hotspot / tethering) for the WIFI_HOTSPOT bandwidth-
//! upgrade medium. The Windows tethering platform was proven end-to-end against
//! a Pixel 10 (phone joins the AP and reaches a TCP socket on the gateway).
//!
//! `start` configures a soft-AP with a caller-chosen SSID/passphrase and returns
//! the credentials (including the gateway IP) to advertise in
//! `WifiHotspotCredentials` during a bandwidth upgrade.
//!
//! **Why the manager lives on its own thread.** `NetworkOperatorTetheringManager`
//! is not agile: unlike the WiFi Direct types (for which the `windows` crate
//! emits `Send`/`Sync`), it is apartment-threaded and must only be touched from
//! the thread that created it. Holding one directly in `InboundRequest` made
//! that struct neither `Send` nor `Sync`, which broke every `tokio::spawn` that
//! carries an `InboundRequest` - including the plain TCP path in `manager.rs`.
//! An `unsafe impl Send` would have silenced the compiler while leaving the
//! apartment rule violated. So the manager is created on a dedicated thread,
//! stays there for its whole life, and the only thing that crosses a thread
//! boundary is a stop signal.

use std::net::IpAddr;
use std::sync::mpsc;
use std::sync::Mutex;

use windows::core::HSTRING;
use windows::Networking::Connectivity::NetworkInformation;
use windows::Networking::NetworkOperators::{NetworkOperatorTetheringManager, TetheringWiFiBand};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Credentials for the started hotspot, mapped 1:1 to WifiHotspotCredentials.
#[derive(Debug, Clone)]
pub struct HotspotCredentials {
    pub ssid: String,
    pub passphrase: String,
    /// The AP gateway IP the peer connects to (Windows uses 192.168.137.1).
    pub gateway: String,
}

/// Handle to a running soft-AP. Dropping it stops tethering.
///
/// Holds only a channel - see the module docs for why the WinRT manager itself
/// cannot live here. `Mutex` rather than a bare `Sender` because `InboundRequest`
/// is held across `await` points, so it must be `Sync` as well as `Send`, and
/// `mpsc::Sender` is `Send` but not `Sync`.
pub struct WindowsHotspot {
    stop_tx: Mutex<mpsc::Sender<()>>,
}

impl std::fmt::Debug for WindowsHotspot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsHotspot").finish_non_exhaustive()
    }
}

impl WindowsHotspot {
    /// Start a soft-AP sharing the active internet connection, with the given
    /// SSID/passphrase. Returns the credentials (incl. the gateway IP).
    ///
    /// Blocks until the AP is up or has failed, so the caller can offer the
    /// credentials knowing they're live.
    pub fn start(ssid: &str, passphrase: &str) -> Result<(Self, HotspotCredentials), anyhow::Error> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<HotspotCredentials, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let ssid = ssid.to_string();
        let passphrase = passphrase.to_string();

        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            let started = (|| -> Result<
                (NetworkOperatorTetheringManager, HotspotCredentials),
                anyhow::Error,
            > {
                // Requires an active internet connection profile to share. With
                // no connectivity there is nothing to tether and this fails
                // here rather than at StartTethering.
                let profile = NetworkInformation::GetInternetConnectionProfile()?;
                let manager =
                    NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile)?;

                let config = manager.GetCurrentAccessPointConfiguration()?;
                config.SetSsid(&HSTRING::from(ssid.as_str()))?;
                config.SetPassphrase(&HSTRING::from(passphrase.as_str()))?;

                // Prefer 5 GHz. The default band is 2.4 GHz, which caps a large
                // phone->PC transfer at roughly 4 MB/s - measured on a 667 MB
                // file. 5 GHz is several times faster where the adapter can host
                // it. Guard on IsBandSupported so an adapter that cannot falls
                // back to its default rather than failing to start the AP, and
                // treat any error from the band call as "leave it at default".
                match config.IsBandSupported(TetheringWiFiBand::FiveGigahertz) {
                    Ok(true) => {
                        if let Err(e) = config.SetBand(TetheringWiFiBand::FiveGigahertz) {
                            warn!("hotspot: could not set 5 GHz band ({e}); using default");
                        } else {
                            info!("hotspot: hosting on the 5 GHz band");
                        }
                    }
                    Ok(false) => info!("hotspot: 5 GHz not supported by the adapter; using default"),
                    Err(e) => warn!("hotspot: 5 GHz support query failed ({e}); using default"),
                }

                manager.ConfigureAccessPointAsync(&config)?.get()?;

                manager.StartTetheringAsync()?.get()?;

                let gateway =
                    hotspot_gateway_ip().unwrap_or_else(|| "192.168.137.1".to_string());

                Ok((
                    manager,
                    HotspotCredentials {
                        ssid: ssid.clone(),
                        passphrase: passphrase.clone(),
                        gateway,
                    },
                ))
            })();

            match started {
                Ok((manager, creds)) => {
                    let _ = ready_tx.send(Ok(creds));
                    // Park until the handle is dropped. A recv error means the
                    // sender went away, which is the same signal.
                    let _ = stop_rx.recv();
                    match manager.StopTetheringAsync().and_then(|op| op.get()) {
                        Ok(_) => info!("WindowsHotspot: tethering stopped"),
                        Err(e) => warn!("WindowsHotspot: failed to stop tethering: {e}"),
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                }
            }
        });

        match ready_rx.recv() {
            Ok(Ok(creds)) => Ok((
                Self {
                    stop_tx: Mutex::new(stop_tx),
                },
                creds,
            )),
            Ok(Err(e)) => Err(anyhow::anyhow!("hotspot start failed: {e}")),
            Err(e) => Err(anyhow::anyhow!("hotspot thread ended without reporting: {e}")),
        }
    }
}

/// Tear the soft-AP down with the transfer that started it.
///
/// Without this the hotspot outlives the request and leaves the user's WiFi
/// radio in AP mode indefinitely - the same class of mistake as the WiFi Direct
/// group that "stayed on across restarts".
impl Drop for WindowsHotspot {
    fn drop(&mut self) {
        if let Ok(tx) = self.stop_tx.lock() {
            let _ = tx.send(());
        }
    }
}

/// The Windows tethering adapter lives on the 192.168.137.0/24 subnet; its local
/// IPv4 is the gateway the peer connects to.
fn hotspot_gateway_ip() -> Option<String> {
    for iface in get_if_addrs::get_if_addrs().ok()? {
        if let IpAddr::V4(v4) = iface.ip() {
            let o = v4.octets();
            if o[0] == 192 && o[1] == 168 && o[2] == 137 {
                return Some(v4.to_string());
            }
        }
    }
    None
}
