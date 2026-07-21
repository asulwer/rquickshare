//! Join a peer's access point, for the WIFI_HOTSPOT bandwidth upgrade when we
//! are the *sender*.
//!
//! The mirror of `hotspot_win.rs`. Receiving, we host an AP and the phone joins
//! us. Sending, the phone hosts - it brings one up even with WiFi off, which is
//! the normal Quick Share flow - and hands us credentials in
//! UPGRADE_PATH_AVAILABLE:
//!
//! ```text
//! medium: WifiHotspot, ssid: "DIRECT-C8-23666B", password: "27637516",
//! gateway: "192.168.49.1", port: 42321
//! ```
//!
//! `192.168.49.1` is the standard Android P2P/hotspot gateway. So this side of
//! the upgrade is a WiFi *client* problem, not a tethering one.
//!
//! Like the tethering manager, `WiFiAdapter` is apartment-threaded, so it stays
//! on a thread of its own and only the result crosses.

use std::sync::mpsc;

use windows::core::HSTRING;
use windows::Devices::WiFi::{WiFiAdapter, WiFiConnectionStatus, WiFiReconnectionKind};
use windows::Security::Credentials::PasswordCredential;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Connect to `ssid` with `password`, blocking until it succeeds or fails.
///
/// Rejoining the previous network afterwards is left to Windows: the connection
/// is made with `WiFiReconnectionKind::Manual`, so it is not persisted and the
/// adapter returns to its usual network when this one goes away.
pub async fn join(ssid: &str, password: &str) -> Result<(), anyhow::Error> {
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let (ssid, password) = (ssid.to_string(), password.to_string());

    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let result = (|| -> Result<(), anyhow::Error> {
            let adapters = WiFiAdapter::FindAllAdaptersAsync()?.get()?;
            if adapters.Size()? == 0 {
                return Err(anyhow::anyhow!("no WiFi adapter"));
            }
            let adapter = adapters.GetAt(0)?;

            // Scan first: a freshly created AP will not be in a stale report.
            adapter.ScanAsync()?.get()?;

            let networks = adapter.NetworkReport()?.AvailableNetworks()?;
            let wanted = HSTRING::from(ssid.as_str());
            let mut found = None;
            for i in 0..networks.Size()? {
                let n = networks.GetAt(i)?;
                if n.Ssid()? == wanted {
                    found = Some(n);
                    break;
                }
            }
            let network = found.ok_or_else(|| {
                anyhow::anyhow!("peer's network {ssid} was not found in the scan")
            })?;

            let credential = PasswordCredential::new()?;
            credential.SetPassword(&HSTRING::from(password.as_str()))?;

            // Manual: do not persist it or auto-rejoin later. This is a
            // transfer-scoped network, not one the user chose.
            let outcome = adapter
                .ConnectWithPasswordCredentialAsync(
                    &network,
                    WiFiReconnectionKind::Manual,
                    &credential,
                )?
                .get()?;

            match outcome.ConnectionStatus()? {
                WiFiConnectionStatus::Success => Ok(()),
                other => Err(anyhow::anyhow!("could not join {ssid}: status {other:?}")),
            }
        })();

        let _ = tx.send(result.map_err(|e| e.to_string()));
    });

    // Blocking recv on a blocking pool thread, so the executor keeps running -
    // stalling it here would break the in-flight frames on the old channel.
    let outcome = tokio::task::spawn_blocking(move || rx.recv()).await??;
    outcome.map_err(|e| anyhow::anyhow!(e))
}

/// Wait until an interface holds an address on the same /24 as `gateway`.
///
/// Association and addressing are separate: `ConnectAsync` returns Success as
/// soon as the link is up, but DHCP from the peer's AP takes a further moment.
/// Connecting before then has no route to the gateway and simply hangs -
/// measured as "timed out connecting to 192.168.49.1" two seconds after a
/// successful join.
pub async fn wait_for_route(gateway: std::net::Ipv4Addr, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let want = gateway.octets();

    loop {
        let on_subnet = get_if_addrs::get_if_addrs()
            .ok()
            .into_iter()
            .flatten()
            .any(|i| match i.ip() {
                std::net::IpAddr::V4(v4) => {
                    let o = v4.octets();
                    o[0] == want[0] && o[1] == want[1] && o[2] == want[2] && !v4.is_loopback()
                }
                _ => false,
            });
        if on_subnet {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}
