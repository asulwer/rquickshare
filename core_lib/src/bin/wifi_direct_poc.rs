//! Standalone proof-of-concept for the platform half of WiFi Direct support
//! (see docs/wifi-direct.md).
//!
//! Brings up a Windows WiFi Direct *legacy* soft-AP (group-owner role) and a
//! TCP listener, then prints the SSID / passphrase / local IPs. The goal is to
//! verify, before writing any protocol code, that:
//!   1. Windows can actually start a WiFi Direct legacy AP from a plain exe.
//!   2. The phone can see + join that network.
//!   3. The phone can reach a TCP socket on it.
//!
//! Run on Windows: `cargo run --bin wifi_direct_poc`

#[cfg(all(target_os = "windows", feature = "experimental"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::net::TcpListener;
    use windows::Devices::WiFiDirect::WiFiDirectAdvertisementPublisher;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let publisher = WiFiDirectAdvertisementPublisher::new()?;
    let advertisement = publisher.Advertisement()?;
    let legacy = advertisement.LegacySettings()?;
    legacy.SetIsEnabled(true)?;

    publisher.Start()?;
    println!("Starting WiFi Direct legacy AP...");

    // Status enum: Created=0, Started=1, Aborted=2, Stopped=3.
    let mut status = -1;
    for _ in 0..40 {
        status = publisher.Status()?.0;
        if status == 1 || status == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    println!("Publisher status = {} (1=Started, 2=Aborted)", status);
    if status != 1 {
        eprintln!(
            "WiFi Direct AP did not start (status {}). Windows may require a packaged \
             app / the wiFiControl capability, or the adapter may not support the \
             group-owner/soft-AP role.",
            status
        );
        return Ok(());
    }

    let ssid = legacy.Ssid()?;
    let passphrase = legacy.Passphrase()?.Password()?;
    println!("=== WiFi Direct legacy AP is up ===");
    println!("  SSID:       {}", ssid);
    println!("  Passphrase: {}", passphrase);

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
        "On the phone: join WiFi network '{}' with the passphrase above, then connect \
         to <the AP's IP>:8899 (a browser or a socket tool). Connections appear below.",
        ssid
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
    eprintln!("wifi_direct_poc is Windows-only and requires the `experimental` feature.");
}
