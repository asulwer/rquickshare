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
//!
//! ## Why this is worth another look (2026-07-17)
//!
//! google/nearby's `bwu_manager.cc` refuses a WIFI_HOTSPOT upgrade outright when
//! the connection is already WiFi-LAN, but deliberately does *not* refuse
//! WIFI_DIRECT unless the client itself is Windows (ours is the phone). So WiFi
//! Direct is the only upgrade a phone will accept from where we are.
//!
//! The previous attempt reported `Started` with no virtual adapter, and we read
//! that as "the adapter can't host an AP". That reading is now doubtful: the
//! Mobile Hotspot path drives the *same radio* into a soft-AP and a phone joins
//! it happily. Two things were missing here rather than in the hardware:
//!
//!   * `IsAutonomousGroupOwnerEnabled` was never set. Without it the publisher
//!     only advertises presence and waits for a peer to negotiate group owner -
//!     no group is created, so no virtual adapter appears, and `Started` is a
//!     truthful report about the *advertisement*, not about an AP.
//!   * `StatusChanged` was never subscribed. Its args carry a `WiFiDirectError`
//!     (Success / RadioNotAvailable / ResourceInUse), which is the only place
//!     Windows explains itself. Polling `Status()` throws that away.

#[cfg(all(target_os = "windows", feature = "experimental"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::net::TcpListener;
    use windows::Devices::WiFiDirect::{
        WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    };
    use windows::Foundation::TypedEventHandler;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    println!("Interfaces BEFORE start (look for a new one after):");
    print_interfaces();

    let publisher = WiFiDirectAdvertisementPublisher::new()?;
    let advertisement = publisher.Advertisement()?;

    // The bit the previous POC missed: without an autonomous group owner there
    // is no P2P group, hence no soft-AP and no virtual adapter.
    advertisement.SetIsAutonomousGroupOwnerEnabled(true)?;
    advertisement.SetListenStateDiscoverability(
        WiFiDirectAdvertisementListenStateDiscoverability::Normal,
    )?;

    let legacy = advertisement.LegacySettings()?;
    legacy.SetIsEnabled(true)?;

    // Status alone can't explain a failure; the error code only arrives here.
    // WiFiDirectError: 0=Success, 1=RadioNotAvailable, 2=ResourceInUse.
    publisher.StatusChanged(&TypedEventHandler::<
        WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    >::new(|_sender, args| {
        if let Some(args) = args.as_ref() {
            let status = args.Status()?.0;
            let error = args.Error()?.0;
            println!(
                "  [event] StatusChanged: status={} ({}), error={} ({})",
                status,
                status_name(status),
                error,
                error_name(error),
            );
        }
        Ok(())
    }))?;

    publisher.Start()?;
    println!("Starting WiFi Direct legacy AP (autonomous group owner)...");

    // Status enum: Created=0, Started=1, Aborted=2, Stopped=3.
    let mut status = -1;
    for _ in 0..40 {
        status = publisher.Status()?.0;
        if status == 1 || status == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    println!("Publisher status = {} ({})", status, status_name(status));
    if status != 1 {
        eprintln!(
            "WiFi Direct AP did not start (status {}). Check the [event] line above \
             for the error code - that's the only place Windows says why.",
            status
        );
        return Ok(());
    }

    let ssid = legacy.Ssid()?;
    let passphrase = legacy.Passphrase()?.Password()?;
    println!("=== WiFi Direct legacy AP reports up ===");
    println!("  SSID:       {}", ssid);
    println!("  Passphrase: {}", passphrase);

    // The real test of "did a group actually get created": a new adapter, most
    // likely on 192.168.137.x (the same ICS subnet the Mobile Hotspot uses).
    // `Started` with no new interface here means the advertisement started but
    // no group exists - exactly the previous failure.
    println!("Interfaces AFTER start:");
    print_interfaces();

    let listener = TcpListener::bind("0.0.0.0:8899")?;
    println!("TCP listening on 0.0.0.0:8899");
    println!(
        "On the phone: join WiFi network '{}' with the passphrase above, then browse \
         to http://<the new IP above>:8899 . Connections appear below.",
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

#[cfg(all(target_os = "windows", feature = "experimental"))]
fn print_interfaces() {
    match get_if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                println!("  {} -> {}", iface.name, iface.ip());
            }
        }
        Err(e) => println!("  (could not enumerate interfaces: {})", e),
    }
}

#[cfg(all(target_os = "windows", feature = "experimental"))]
fn status_name(status: i32) -> &'static str {
    match status {
        0 => "Created",
        1 => "Started",
        2 => "Aborted",
        3 => "Stopped",
        _ => "?",
    }
}

#[cfg(all(target_os = "windows", feature = "experimental"))]
fn error_name(error: i32) -> &'static str {
    match error {
        0 => "Success",
        1 => "RadioNotAvailable",
        2 => "ResourceInUse",
        _ => "?",
    }
}

#[cfg(not(all(target_os = "windows", feature = "experimental")))]
fn main() {
    eprintln!("wifi_direct_poc is Windows-only and requires the `experimental` feature.");
}
