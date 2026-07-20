//! Builder for the Quick Share / Nearby Connections BLE *receiver*
//! advertisement (issue #425), so a phone doing BLE-only discovery lists this
//! machine as a target.
//!
//! This builds the **fast** advertisement, which is what Google's Windows Quick
//! Share actually broadcasts - captured from a Pixel's logcat while it
//! discovered Google's app with WiFi off:
//!
//! ```text
//! BleAdvertisement { version=2, socketVersion=2, isFast=true, serviceIdHash=null,
//!   data=[0x23 0x4e 0x58 0x38 0x47 0x11 0x16 ...23 bytes], deviceToken=[0x74 0xeb] }
//!   matched 0000fef3 in fastAdvertisementServiceUuids -> Discovered endpoint NX8G
//! ```
//!
//! The fast form is compact enough to fit a *legacy* 31-byte advertisement under
//! 0xFEF3 - no extended advertising and no GATT round trip to be discovered. It
//! carries no inline device name (too small); the name comes from the account
//! for a self-share, or a later GATT read for an unknown sender (next milestone).
//!
//! Layouts (from google/nearby `ble_advertisement.cc`, fast branch):
//!   Layer 3: [VER/PCP=0x23][ENDPOINT_ID(4)][INFO_SIZE(1)][ENDPOINT_INFO(n)]
//!   Layer 2: [VER/SOCK/FAST=0x4A][DATA(Layer3)][DEVICE_TOKEN(2)]
//! The non-fast form + advertisement header (bloom filter, GATT) lived here too;
//! removed when the fast path replaced it. See git history / the doc if the GATT
//! full-advertisement milestone needs them back.

use sha2::{Digest, Sha256};

fn sha256_prefix(data: &[u8], n: usize) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()[..n].to_vec()
}

/// SHA-256(ascii-decimal of a random u32)[:2].
fn device_token() -> [u8; 2] {
    let n: u32 = rand::random();
    let v = sha256_prefix(n.to_string().as_bytes(), 2);
    [v[0], v[1]]
}

/// Fast endpoint info, byte-matched to Google's Windows laptop advertisement
/// captured from the phone: first byte 0x16 for a Laptop, then 16 bytes of
/// salt + metadata key.
///
/// The 0x16 = `(device_type << 1) | 0x10`. Google's *laptop* advert (deviceType
/// 3, the one this phone lists and transfers to) uses this exact byte, so we do
/// too. (An earlier detour cleared the 0x10 bit based on a mis-read discovery
/// that turned out to be a different phone, deviceType 1, byte 0x32 - not us.)
fn build_endpoint_info_fast(device_type: u8) -> Vec<u8> {
    let mut info = Vec::new();
    info.push((device_type << 1) | 0x10);
    let salt_and_meta: [u8; 16] = rand::random();
    info.extend_from_slice(&salt_and_meta);
    info
}

/// Layer 3 fast: `[version/pcp][endpoint_id(4)][info_len(1)][endpoint_info]`.
/// Omits service_id_hash, bluetooth_mac, uwb and extra_field (all non-fast only),
/// per google's `BleAdvertisement::operator ByteArray` fast branch.
fn build_connections_advertisement_fast(endpoint_id: &[u8; 4], endpoint_info: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x23); // (version 1 << 5) | pcp 3
    out.extend_from_slice(endpoint_id);
    out.push(endpoint_info.len() as u8);
    out.extend_from_slice(endpoint_info);
    out
}

/// Layer 2 fast: `[version/socket/fast byte][data][device_token(2)]`. No
/// service_id_hash and no 4-byte data_size (fast omits both); the reader takes
/// the last 2 bytes as the token and the rest as data.
fn build_medium_advertisement_fast(data: &[u8], device_token: &[u8; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    // version kV2(2) | socket kV2(2) | fast=1 | second_profile=0 = 0x4A
    let version_byte = ((2u8 << 5) & 0xE0) | ((2u8 << 2) & 0x1C) | 0x02;
    out.push(version_byte);
    out.extend_from_slice(data);
    out.extend_from_slice(device_token);
    out
}

/// The complete fast BleAdvertisement to place in the 0xFEF3 service data of a
/// legacy BLE advertisement.
pub fn build_fast_receiver_advertisement(endpoint_id: &[u8; 4], device_type: u8) -> Vec<u8> {
    let endpoint_info = build_endpoint_info_fast(device_type);
    let data = build_connections_advertisement_fast(endpoint_id, &endpoint_info);
    build_medium_advertisement_fast(&data, &device_token())
}

// ---- FULL (non-fast) advertisement, served over GATT -----------------------
//
// A peer that finds us connects and reads the slot-0 characteristic to get this.
// Unlike the fast form it is not size-constrained, so it carries the pieces the
// phone needs to build a listable ShareTarget - notably the **device name**.

/// SHA-256("NearbySharing")[:3] == FC 9F 5E.
fn service_id_hash() -> Vec<u8> {
    sha256_prefix(b"NearbySharing", 3)
}

/// Full endpoint info: bitfield, 16 bytes salt + metadata key, then the
/// length-prefixed UTF-8 device name. Visibility bit clear = visible/Everyone,
/// matching what `gen_mdns_endpoint_info` emits over mDNS.
fn build_endpoint_info_full(device_type: u8, name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.push(device_type << 1);
    let salt_and_meta: [u8; 16] = rand::random();
    info.extend_from_slice(&salt_and_meta);
    let name_bytes = name.as_bytes();
    let len = name_bytes.len().min(255);
    info.push(len as u8);
    info.extend_from_slice(&name_bytes[..len]);
    info
}

/// Layer 3 non-fast: adds service_id_hash, bluetooth_mac, uwb size and the
/// extra field around the endpoint info.
fn build_connections_advertisement_full(endpoint_id: &[u8; 4], endpoint_info: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x23); // (version 1 << 5) | pcp 3
    out.extend_from_slice(&service_id_hash()); // FC 9F 5E
    out.extend_from_slice(endpoint_id);
    out.push(endpoint_info.len() as u8);
    out.extend_from_slice(endpoint_info);
    out.extend_from_slice(&[0u8; 6]); // bluetooth mac (reserved zeros)
    out.push(0x00); // uwb address size = 0
    out.push(0x00); // extra field (webrtc connectable = 0)
    out
}

/// Layer 2 non-fast: `[0x48][service_id_hash(3)][data_size u32 BE][data][token(2)]`.
fn build_medium_advertisement_full(data: &[u8], device_token: &[u8; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    // version kV2(2) | socket kV2(2) | fast=0 | second_profile=0
    out.push(((2u8 << 5) & 0xE0) | ((2u8 << 2) & 0x1C)); // 0x48
    out.extend_from_slice(&service_id_hash());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(device_token);
    out
}

/// The full BleAdvertisement to serve from the slot-0 GATT characteristic.
pub fn build_full_receiver_advertisement(
    endpoint_id: &[u8; 4],
    device_type: u8,
    name: &str,
) -> Vec<u8> {
    let endpoint_info = build_endpoint_info_full(device_type, name);
    let data = build_connections_advertisement_full(endpoint_id, &endpoint_info);
    build_medium_advertisement_full(&data, &device_token())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_advertisement_matches_google_layout() {
        // Deterministic bytes checked against the Pixel capture of Google's app.
        let adv = build_fast_receiver_advertisement(b"NX8G", 3);

        // Layer 2 fast version byte: version 2 | socket 2 | fast.
        assert_eq!(adv[0], 0x4A);
        // Layer 3 begins: version 1 | pcp 3.
        assert_eq!(adv[1], 0x23);
        // endpoint id.
        assert_eq!(&adv[2..6], b"NX8G");
        // endpoint_info length = 17 (1 bitfield + 16 salt/key).
        assert_eq!(adv[6], 17);
        // bitfield for a Laptop - byte-matches Google's captured 0x16.
        assert_eq!(adv[7], 0x16);
        // total: 1 (L2 ver) + 1 (L3 ver) + 4 (id) + 1 (len) + 17 (info) + 2 (token).
        assert_eq!(adv.len(), 26);
    }

    #[test]
    fn device_token_is_two_bytes() {
        assert_eq!(device_token().len(), 2);
    }
}
