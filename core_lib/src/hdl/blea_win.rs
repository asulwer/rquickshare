//! Windows implementation of the BLE advertiser.
//!
//! Mirrors the Linux (`bluer`) advertiser in `blea.rs`: it broadcasts the
//! Quick Share / Nearby Share BLE service data so a nearby Android device makes
//! its mDNS service available, letting this machine discover the phone when
//! sending (removing the "put the phone in receive mode first" workaround).
//!
//! Uses the WinRT `BluetoothLEAdvertisementPublisher` API via the `windows`
//! crate. All WinRT calls run on a dedicated blocking thread with an
//! initialized (MTA) COM apartment, since WinRT activation requires an
//! initialized apartment and tokio worker threads are not guaranteed to be.

use tokio_util::sync::CancellationToken;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementDataSection, BluetoothLEAdvertisementPublisher,
};
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

// Same 24-byte service data payload broadcast by the Linux advertiser.
const SERVICE_DATA: [u8; 24] = [
    252, 18, 142, 1, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 191, 45, 91, 160, 225, 216, 117, 36, 202, 0,
];

// GAP AD type 0x16 = "Service Data - 16-bit UUID".
const AD_TYPE_SERVICE_DATA_16BIT: u8 = 0x16;
// Quick Share / Nearby Share 16-bit service UUID (0xFE2C).
const SERVICE_UUID_16: u16 = 0xFE2C;

const INNER_NAME: &str = "BleAdvertiser";

// BluetoothLEAdvertisementPublisherStatus enum values.
fn status_name(s: i32) -> &'static str {
    match s {
        0 => "Created",
        1 => "Waiting",
        2 => "Started",
        3 => "Stopping",
        4 => "Stopped",
        5 => "Aborted",
        _ => "Unknown",
    }
}

pub struct BleAdvertiser;

impl BleAdvertiser {
    pub async fn new() -> Result<Self, anyhow::Error> {
        // The WinRT objects must be created on the COM-initialized advertising
        // thread, so all construction is deferred to `run`.
        Ok(Self)
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
            // WinRT activation requires an initialized COM apartment. Join the
            // MTA; if this thread is already initialized it is a harmless no-op.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            let publisher = BluetoothLEAdvertisementPublisher::new()?;
            let advertisement = publisher.Advertisement()?;

            // Build the "Service Data - 16-bit UUID" AD structure: the 16-bit
            // UUID in little-endian order followed by the service data payload.
            // This is byte-for-byte equivalent to what the bluer advertiser
            // emits on Linux.
            let writer = DataWriter::new()?;
            writer.WriteByte((SERVICE_UUID_16 & 0xFF) as u8)?;
            writer.WriteByte((SERVICE_UUID_16 >> 8) as u8)?;
            writer.WriteBytes(&SERVICE_DATA)?;
            let buffer = writer.DetachBuffer()?;

            let section = BluetoothLEAdvertisementDataSection::new()?;
            section.SetDataType(AD_TYPE_SERVICE_DATA_16BIT)?;
            section.SetData(&buffer)?;
            advertisement.DataSections()?.Append(&section)?;

            publisher.Start()?;
            info!("{INNER_NAME}: advertising via WinRT BluetoothLEAdvertisementPublisher");

            // Keep this thread (and the publisher) alive until cancellation.
            // Dropping the publisher stops the advertisement. Log status
            // transitions so we can tell whether the advertisement is actually
            // on-air (Started) or was rejected by the adapter (Aborted).
            let mut last_status: i32 = -1;
            while !ctk.is_cancelled() {
                if let Ok(status) = publisher.Status() {
                    if status.0 != last_status {
                        last_status = status.0;
                        info!(
                            "{INNER_NAME}: publisher status = {} ({})",
                            last_status,
                            status_name(last_status)
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }

            info!("{INNER_NAME}: tracker cancelled, stopping advertiser");
            let _ = publisher.Stop();

            Ok(())
        })
        .await??;

        Ok(())
    }
}
