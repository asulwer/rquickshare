//! POC for the Windows soft-AP via the Mobile Hotspot / tethering API
//! (`NetworkOperatorTetheringManager`), which is the platform layer for the
//! `WIFI_HOTSPOT` medium (see docs/wifi-direct.md for the shared bandwidth-
//! upgrade machinery).
//!
//! Unlike the WiFi Direct legacy publisher (which didn't activate on the Intel
//! BE200), this uses the same mechanism as Windows Mobile Hotspot — proven to
//! broadcast an AP a phone can join — but lets us set a KNOWN ssid/passphrase
//! to advertise in `WifiHotspotCredentials`.
//!
//! Run on Windows: `cargo run --bin hotspot_poc`

#[cfg(all(target_os = "windows", feature = "experimental"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::net::TcpListener;
    use windows::core::HSTRING;
    use windows::Networking::Connectivity::NetworkInformation;
    use windows::Networking::NetworkOperators::NetworkOperatorTetheringManager;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    const SSID: &str = "rquickshare-xfer";
    const PASSPHRASE: &str = "rquickshare-pass";

    // Tethering shares an existing connection (e.g. ethernet), so we need one.
    let profile = NetworkInformation::GetInternetConnectionProfile()?;
    let manager = NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile)?;

    // Configure a known SSID + passphrase (what we'd hand the phone in
    // WifiHotspotCredentials).
    let config = manager.GetCurrentAccessPointConfiguration()?;
    config.SetSsid(&HSTRING::from(SSID))?;
    config.SetPassphrase(&HSTRING::from(PASSPHRASE))?;
    manager.ConfigureAccessPointAsync(&config)?.get()?;

    // Start the soft-AP.
    let result = manager.StartTetheringAsync()?.get()?;
    println!("StartTethering status = {:?}", result.Status()?);
    println!(
        "Tethering operational state = {:?}",
        manager.TetheringOperationalState()?
    );
    println!("=== Hotspot up ===");
    println!("  SSID:       {}", SSID);
    println!("  Passphrase: {}", PASSPHRASE);

    println!("Local interfaces:");
    match get_if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                println!("  {} -> {}", iface.name, iface.ip());
            }
        }
        Err(e) => println!("  (could not enumerate interfaces: {})", e),
    }

    let listener = TcpListener::bind("0.0.0.0:8899")?;
    println!("TCP listening on 0.0.0.0:8899");
    println!(
        "On the phone: join WiFi '{}' with passphrase '{}', then hit \
         http://<the hotspot gateway IP, usually 192.168.137.1>:8899",
        SSID, PASSPHRASE
    );

    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                println!("*** Connection from {:?} ***", s.peer_addr());
                let mut buf = [0u8; 128];
                match s.read(&mut buf) {
                    Ok(n) => println!("Read {} bytes: {:02x?}", n, &buf[..n]),
                    Err(e) => println!("read error: {}", e),
                }
            }
            Err(e) => println!("accept error: {}", e),
        }
    }

    Ok(())
}

#[cfg(not(all(target_os = "windows", feature = "experimental")))]
fn main() {
    eprintln!("hotspot_poc is Windows-only and requires the `experimental` feature.");
}
