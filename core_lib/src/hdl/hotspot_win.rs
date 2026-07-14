#![allow(dead_code)]
//! Windows soft-AP (Mobile Hotspot / tethering) for the WIFI_HOTSPOT bandwidth-
//! upgrade medium. The Windows tethering platform was proven end-to-end against
//! a Pixel 10 (phone joins the AP and reaches a TCP socket on the gateway).
//!
//! `start` configures a soft-AP with a caller-chosen SSID/passphrase and returns
//! the credentials (including the gateway IP) to advertise in
//! `WifiHotspotCredentials` during a bandwidth upgrade.

use std::net::IpAddr;

use windows::core::HSTRING;
use windows::Networking::Connectivity::NetworkInformation;
use windows::Networking::NetworkOperators::NetworkOperatorTetheringManager;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Credentials for the started hotspot, mapped 1:1 to WifiHotspotCredentials.
#[derive(Debug, Clone)]
pub struct HotspotCredentials {
    pub ssid: String,
    pub passphrase: String,
    /// The AP gateway IP the peer connects to (Windows uses 192.168.137.1).
    pub gateway: String,
}

pub struct WindowsHotspot {
    manager: NetworkOperatorTetheringManager,
}

impl WindowsHotspot {
    /// Start a soft-AP sharing the active internet connection, with the given
    /// SSID/passphrase. Returns the credentials (incl. the gateway IP).
    pub fn start(
        ssid: &str,
        passphrase: &str,
    ) -> Result<(Self, HotspotCredentials), anyhow::Error> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let profile = NetworkInformation::GetInternetConnectionProfile()?;
        let manager = NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile)?;

        let config = manager.GetCurrentAccessPointConfiguration()?;
        config.SetSsid(&HSTRING::from(ssid))?;
        config.SetPassphrase(&HSTRING::from(passphrase))?;
        manager.ConfigureAccessPointAsync(&config)?.get()?;

        manager.StartTetheringAsync()?.get()?;

        let gateway = hotspot_gateway_ip().unwrap_or_else(|| "192.168.137.1".to_string());

        Ok((
            Self { manager },
            HotspotCredentials {
                ssid: ssid.to_string(),
                passphrase: passphrase.to_string(),
                gateway,
            },
        ))
    }

    pub fn stop(&self) -> Result<(), anyhow::Error> {
        self.manager.StopTetheringAsync()?.get()?;
        Ok(())
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
