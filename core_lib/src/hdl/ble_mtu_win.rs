//! Read the ATT MTU that a BLE link actually negotiated.
//!
//! The Weave handshake asks us to state the largest packet we can receive, and
//! the peer then sends notifications up to that size. Stating a number the link
//! cannot carry does not fail at our end - our own writes are fragmented for us
//! by the stack - it fails at the *peer's*, when its notification exceeds the
//! MTU:
//!
//! ```text
//! W/NearbyMediums: PhysicalBleSocket ... encountered an error with its internal
//!                  Weave socket.
//! W/NearbyMediums: java.io.IOException: failed with status 133
//!                    at gjon.onNotificationSent(..)
//! E/NearbyConnections: In startServer(), UKEY2 failed with endpoint ...
//! ```
//!
//! Measured: the small Weave control notifications always arrived, and the first
//! large one - the UKEY2 server init - was the one that died. That is the whole
//! shape of the "first connection is flaky" problem, and it varies run to run
//! because the negotiated MTU does.
//!
//! btleplug does not expose the MTU, so this goes to WinRT for it. `MaxPduSize`
//! is the negotiated ATT MTU, valid once the link is up and the exchange has
//! happened - which it has by the time we have discovered services and
//! subscribed.

use windows::Devices::Bluetooth::BluetoothLEDevice;
use windows::Devices::Bluetooth::GenericAttributeProfile::GattSession;

/// The negotiated ATT MTU for the link to `address`, or `None` if it cannot be
/// determined - in which case the caller should keep to a conservative size
/// rather than assume the ceiling.
///
/// `address` is the 48-bit Bluetooth address in the low six bytes, which is how
/// WinRT wants it.
pub async fn negotiated_att_mtu(address: u64) -> Option<u16> {
    tokio::task::spawn_blocking(move || {
        let device = BluetoothLEDevice::FromBluetoothAddressAsync(address)
            .ok()?
            .get()
            .ok()?;
        let id = device.BluetoothDeviceId().ok()?;

        // Not `MaintainConnection`: this is a read of the existing link, not a
        // reason to hold one open. btleplug owns the connection's lifetime and a
        // second owner here would keep the peer connected after a transfer -
        // which is exactly what stops it being discoverable again.
        // Left to drop rather than closed explicitly: `Close` comes from
        // IClosable and needs the trait in scope, and with MaintainConnection
        // false there is nothing held open to release.
        let session = GattSession::FromDeviceIdAsync(&id).ok()?.get().ok()?;
        session.MaxPduSize().ok()
    })
    .await
    .ok()
    .flatten()
}
