//! Windows BLE *receiver* advertiser (issue #425, Route A: extended advertising).
//!
//! Broadcasts the Nearby Connections discoverable-endpoint advertisement so a
//! phone doing BLE-only discovery can list this machine as a target:
//!   - service data under 0xFEF3 = the advertisement header
//!   - service data under the slot-0 advertisement UUID = the full advertisement
//! Both are placed in a BLE 5 *extended* advertisement (the payload exceeds the
//! 31-byte legacy limit).
//!
//! This is a first attempt at the extended-advertising assembly (see
//! docs/ble-receiver-discovery.md open items) and will be refined against the phone.
#![allow(dead_code)]

use tokio_util::sync::CancellationToken;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementDataSection, BluetoothLEAdvertisementPublisher,
};
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::ble_receiver;

const INNER_NAME: &str = "BleReceiverAdvertiser";
const COPRESENCE_UUID16: u16 = 0xFEF3;

// Slot-0 advertisement UUID 00000000-0000-3000-8000-000000000000, in the
// little-endian byte order BLE service-data sections use.
const ADV_SLOT0_UUID_LE: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const AD_TYPE_SERVICE_DATA_16BIT: u8 = 0x16;
const AD_TYPE_SERVICE_DATA_128BIT: u8 = 0x21;

pub struct BleReceiverAdvertiser {
    endpoint_id: [u8; 4],
    device_type: u8,
    name: String,
}

impl BleReceiverAdvertiser {
    pub fn new(endpoint_id: [u8; 4], device_type: u8, name: String) -> Self {
        Self {
            endpoint_id,
            device_type,
            name,
        }
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        let (header, _advertisement) = ble_receiver::build_receiver_advertisement(
            &self.endpoint_id,
            self.device_type,
            &self.name,
        );

        tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            let publisher = BluetoothLEAdvertisementPublisher::new()?;

            let advert = publisher.Advertisement()?;
            let sections = advert.DataSections()?;
            // Legacy advertisement carrying just the 17-byte header under 0xFEF3
            // (fits in 31 bytes and is visible to legacy scanners, unlike an
            // extended advertisement). The full advertisement is served over a
            // GATT characteristic (Route B) — still to be implemented.
            sections.Append(&service_data_16(COPRESENCE_UUID16, &header)?)?;

            publisher.Start()?;
            info!(
                "{INNER_NAME}: legacy advertising started (header {} B) under 0xFEF3",
                header.len()
            );

            let mut last_status: i32 = -1;
            while !ctk.is_cancelled() {
                if let Ok(status) = publisher.Status() {
                    if status.0 != last_status {
                        last_status = status.0;
                        info!("{INNER_NAME}: publisher status = {}", last_status);
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

fn service_data_16(
    uuid16: u16,
    payload: &[u8],
) -> Result<BluetoothLEAdvertisementDataSection, anyhow::Error> {
    let writer = DataWriter::new()?;
    writer.WriteByte((uuid16 & 0xFF) as u8)?;
    writer.WriteByte((uuid16 >> 8) as u8)?;
    writer.WriteBytes(payload)?;
    let buffer = writer.DetachBuffer()?;

    let section = BluetoothLEAdvertisementDataSection::new()?;
    section.SetDataType(AD_TYPE_SERVICE_DATA_16BIT)?;
    section.SetData(&buffer)?;
    Ok(section)
}

fn service_data_128(
    uuid_le: &[u8; 16],
    payload: &[u8],
) -> Result<BluetoothLEAdvertisementDataSection, anyhow::Error> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(uuid_le)?;
    writer.WriteBytes(payload)?;
    let buffer = writer.DetachBuffer()?;

    let section = BluetoothLEAdvertisementDataSection::new()?;
    section.SetDataType(AD_TYPE_SERVICE_DATA_128BIT)?;
    section.SetData(&buffer)?;
    Ok(section)
}
