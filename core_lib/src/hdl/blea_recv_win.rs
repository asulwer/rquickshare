//! Windows BLE *receiver* advertiser (issue #425).
//!
//! Advertises a **GATT service** under 0xFEF3 carrying the Nearby Connections
//! fast advertisement as its service data, so a phone doing BLE-only discovery
//! (WiFi off) lists this machine as a target.
//!
//! **Why a GATT service provider and not a plain advertisement publisher.**
//! Every real Quick Share receiver (neighbour phones, Google's own Windows app)
//! advertises `isPrivateGatt=true` in the phone's logcat - a connectable
//! GATT-backed advertisement, not a beacon. We spent a while broadcasting via
//! `BluetoothLEAdvertisementPublisher` (Microsoft: "mainly used to create
//! beacons"); it reached status Started but the phone's *receiver* discovery
//! never surfaced us as a ShareTarget, legacy or extended. `GattServiceProvider`
//! is the connectable/discoverable mechanism the phone actually looks for, and
//! it also gives us the GATT server needed later to serve the device name.

use tokio_util::sync::CancellationToken;
use windows::core::GUID;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicProperties, GattLocalCharacteristic, GattLocalCharacteristicParameters,
    GattReadRequestedEventArgs, GattServiceProvider, GattServiceProviderAdvertisingParameters,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::ble_receiver;

const INNER_NAME: &str = "BleReceiverAdvertiser";

// Copresence service 0000FEF3-0000-1000-8000-00805F9B34FB.
const COPRESENCE_SERVICE_GUID: GUID = GUID::from_u128(0x0000_FEF3_0000_1000_8000_00805F9B34FB);

// Per-slot advertisement characteristic 00000000-0000-3000-8000-000000000000
// (slot 0). A peer that finds our advertisement connects and reads this to get
// the full BleAdvertisement - the `isPrivateGatt` / `rxAdvertisement` flow.
const ADV_SLOT0_CHARACTERISTIC_GUID: GUID =
    GUID::from_u128(0x0000_0000_0000_3000_8000_000000000000);

fn adv_status_name(s: i32) -> &'static str {
    match s {
        0 => "Created",
        1 => "Stopped",
        2 => "Started",
        3 => "Aborted",
        4 => "StartedWithoutAllAdvertisementData",
        _ => "Unknown",
    }
}

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
        // Two forms, two channels:
        //  - fast: compact, for the advertisement packet's service data.
        //  - full: served over GATT when a peer connects. Carries the device
        //    name, which is what the phone needs to build a listable
        //    ShareTarget. Serving the *fast* form here was why the phone read us
        //    successfully and still never listed us.
        let advertisement =
            ble_receiver::build_fast_receiver_advertisement(&self.endpoint_id, self.device_type);
        let full_advertisement = ble_receiver::build_full_receiver_advertisement(
            &self.endpoint_id,
            self.device_type,
            &self.name,
        );

        tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            // Create the GATT service provider for 0xFEF3.
            let create_result =
                GattServiceProvider::CreateAsync(COPRESENCE_SERVICE_GUID)?.get()?;
            let error = create_result.Error()?;
            if error.0 != 0 {
                return Err(anyhow::anyhow!(
                    "GattServiceProvider::CreateAsync failed: BluetoothError {}",
                    error.0
                ));
            }
            let provider = create_result.ServiceProvider()?;

            // Serve the full advertisement from the slot-0 characteristic. The
            // phone connects after seeing our advert and reads this; with no
            // characteristic it finds an empty service and gives up, which is
            // why we were never listed.
            let char_params = GattLocalCharacteristicParameters::new()?;
            char_params.SetCharacteristicProperties(GattCharacteristicProperties::Read)?;
            let char_result = provider
                .Service()?
                .CreateCharacteristicAsync(ADV_SLOT0_CHARACTERISTIC_GUID, &char_params)?
                .get()?;
            let characteristic = char_result.Characteristic()?;

            let adv_for_read = full_advertisement.clone();
            characteristic.ReadRequested(&TypedEventHandler::<
                GattLocalCharacteristic,
                GattReadRequestedEventArgs,
            >::new(move |_sender, args| {
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                // Must take a deferral: fetching the request is async and WinRT
                // will otherwise consider the event handled with no response.
                let deferral = args.GetDeferral()?;
                let request = args.GetRequestAsync()?.get()?;
                let writer = DataWriter::new()?;
                writer.WriteBytes(&adv_for_read)?;
                request.RespondWithValue(&writer.DetachBuffer()?)?;
                deferral.Complete()?;
                info!("*** {INNER_NAME}: served advertisement over GATT read ***");
                Ok(())
            }))?;

            // Connectable: puts the 0xFEF3 service UUID on-air so the phone's
            // receiver scan finds us. A GATT service must be connectable to
            // advertise at all (non-connectable -> status 3 Aborted). Our 26-byte
            // fast advertisement won't also fit the packet (-> status 4,
            // StartedWithoutAllAdvertisementData), so it is NOT delivered inline;
            // the phone fetches the advertisement/metadata over the GATT
            // connection instead (isPrivateGatt / rxAdvertisement). That GATT
            // characteristic is the next piece to build - for now this just gets
            // the UUID discoverable so we can confirm the phone finds us.
            let params = GattServiceProviderAdvertisingParameters::new()?;
            params.SetIsConnectable(true)?;
            params.SetIsDiscoverable(false)?;

            let writer = DataWriter::new()?;
            writer.WriteBytes(&advertisement)?;
            params.SetServiceData(&writer.DetachBuffer()?)?;

            provider.StartAdvertisingWithParameters(&params)?;
            info!(
                "{INNER_NAME}: GATT service advertising started ({} B service data) under 0xFEF3",
                advertisement.len()
            );

            let mut last_status: i32 = -1;
            while !ctk.is_cancelled() {
                if let Ok(status) = provider.AdvertisementStatus() {
                    if status.0 != last_status {
                        last_status = status.0;
                        info!(
                            "{INNER_NAME}: advertisement status = {} ({})",
                            last_status,
                            adv_status_name(last_status)
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }

            info!("{INNER_NAME}: tracker cancelled, stopping advertiser");
            let _ = provider.StopAdvertising();
            Ok(())
        })
        .await??;

        Ok(())
    }
}
