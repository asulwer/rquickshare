#![allow(dead_code)]
//! Builders for the Quick Share / Nearby Connections BLE *receiver*
//! advertisement (issue #425), so a phone doing BLE-only discovery can list
//! this machine as a target.
//!
//! The advertisement is nested across three layers, all transcribed from
//! Google's open-source implementation (github.com/google/nearby,
//! connections/implementation/...). See BLE_RECEIVER_DISCOVERY.md for the full
//! spec. Every value below is covered by unit tests against known references.
//!
//! Layer 3 (connections advertisement, the "data"):
//!   [VER/PCP=0x23][SERVICE_ID_HASH(3)][ENDPOINT_ID(4)][INFO_SIZE(1)]
//!   [ENDPOINT_INFO(n)][BT_MAC(6)][UWB_SIZE(1)=0][EXTRA(1)=0]
//! Layer 2 (mediums BleAdvertisement, wraps Layer 3 as its data):
//!   [VER/SOCK=0x48][SERVICE_ID_HASH(3)][DATA_SIZE(u32 BE)][DATA][DEVICE_TOKEN(2)]
//! Layer 1 (advertisement header, goes in the 0xFEF3 service data):
//!   [VER/EXT/NUM_SLOTS(1)][BLOOM(10)][ADV_HASH(4)][PSM(2 BE)]

use sha2::{Digest, Sha256};

/// Nearby Share service id. SHA-256 of this yields the FC9F5E… service hash.
pub const SERVICE_ID: &str = "NearbySharing";

/// Copresence service UUID the header is advertised under (16-bit 0xFEF3).
pub const COPRESENCE_SERVICE_UUID_16: u16 = 0xFEF3;

// ---- hashing helpers -------------------------------------------------------

fn sha256_prefix(data: &[u8], n: usize) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()[..n].to_vec()
}

/// SHA-256("NearbySharing")[:3] == FC 9F 5E.
pub fn service_id_hash() -> Vec<u8> {
    sha256_prefix(SERVICE_ID.as_bytes(), 3)
}

/// SHA-256(advertisement)[:4], used in the advertisement header.
pub fn advertisement_hash(advertisement: &[u8]) -> [u8; 4] {
    let v = sha256_prefix(advertisement, 4);
    [v[0], v[1], v[2], v[3]]
}

/// SHA-256(ascii-decimal of a random u32)[:2].
pub fn device_token() -> [u8; 2] {
    let n: u32 = rand::random();
    let v = sha256_prefix(n.to_string().as_bytes(), 2);
    [v[0], v[1]]
}

// ---- MurmurHash3 x64 128 (hand-rolled; verified against the canonical impl) -

fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51afd7ed558ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ceb9fe1a85ec53);
    k ^= k >> 33;
    k
}

/// MurmurHash3_x64_128. Returns the two 64-bit halves (h1, h2).
fn murmur3_x64_128(data: &[u8], seed: u32) -> (u64, u64) {
    const C1: u64 = 0x87c37b91114253d5;
    const C2: u64 = 0x4cf5ad432745937f;

    let mut h1 = seed as u64;
    let mut h2 = seed as u64;

    let nblocks = data.len() / 16;
    for i in 0..nblocks {
        let mut k1 = u64::from_le_bytes(data[i * 16..i * 16 + 8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(data[i * 16 + 8..i * 16 + 16].try_into().unwrap());

        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dce729);

        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x38495ab5);
    }

    let tail = &data[nblocks * 16..];
    let l = tail.len();
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    if l > 8 {
        for i in 8..l {
            k2 ^= (tail[i] as u64) << (8 * (i - 8));
        }
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }
    if l > 0 {
        for i in 0..l.min(8) {
            k1 ^= (tail[i] as u64) << (8 * i);
        }
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }

    h1 ^= data.len() as u64;
    h2 ^= data.len() as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    (h1, h2)
}

/// The 10-byte (80-bit) service-id bloom filter used in the header.
pub fn bloom_filter_10(service_id: &str) -> [u8; 10] {
    let (h1, _h2) = murmur3_x64_128(service_id.as_bytes(), 0);
    let hash1 = (h1 & 0xFFFF_FFFF) as u32 as i32;
    let hash2 = ((h1 >> 32) & 0xFFFF_FFFF) as u32 as i32;

    let mut bits = [false; 80];
    for i in 1..=5i32 {
        let mut combined = hash1.wrapping_add(i.wrapping_mul(hash2));
        if combined < 0 {
            combined = !combined;
        }
        let pos = (combined as u32 as usize) % 80;
        bits[pos] = true;
    }

    let mut out = [0u8; 10];
    for (p, set) in bits.iter().enumerate() {
        if *set {
            out[p / 8] |= 1 << (p % 8);
        }
    }
    out
}

// ---- advertisement builders ------------------------------------------------

/// Raw (non-base64) endpoint info: the same structure `gen_mdns_endpoint_info`
/// emits over mDNS. `device_type`: 0=unknown, 1=phone, 2=tablet, 3=laptop.
pub fn build_endpoint_info(device_type: u8, name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    // bitfield: version(3)=0 | visibility(1)=0 visible | device_type(3) | reserved(1)=0
    info.push(device_type << 1);
    let salt_and_meta: [u8; 16] = rand::random();
    info.extend_from_slice(&salt_and_meta);
    let name_bytes = name.as_bytes();
    let len = name_bytes.len().min(255);
    info.push(len as u8);
    info.extend_from_slice(&name_bytes[..len]);
    info
}

/// Layer 3: the Connections BLE advertisement (becomes Layer 2's DATA).
pub fn build_connections_advertisement(endpoint_id: &[u8; 4], endpoint_info: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x23); // (version 1 << 5) | pcp 3 (point-to-point)
    out.extend_from_slice(&service_id_hash()); // FC 9F 5E
    out.extend_from_slice(endpoint_id); // 4 bytes
    out.push(endpoint_info.len() as u8);
    out.extend_from_slice(endpoint_info);
    out.extend_from_slice(&[0u8; 6]); // bluetooth mac (reserved zeros)
    out.push(0x00); // uwb address size = 0
    out.push(0x00); // extra field (webrtc connectable = 0)
    out
}

/// Layer 2: the mediums BleAdvertisement wrapping `data`.
pub fn build_medium_advertisement(data: &[u8], device_token: &[u8; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    // version kV2(2) | socket kV2(2) | fast=0 | second_profile=0
    let version_byte = ((2u8 << 5) & 0xE0) | ((2u8 << 2) & 0x1C);
    out.push(version_byte); // 0x48
    out.extend_from_slice(&service_id_hash()); // 3 bytes
    out.extend_from_slice(&(data.len() as u32).to_be_bytes()); // data size, u32 BE
    out.extend_from_slice(data);
    out.extend_from_slice(device_token); // 2 bytes
    out
}

/// Layer 1: the advertisement header advertised in the 0xFEF3 service data.
pub fn build_advertisement_header(
    num_slots: u8,
    extended_advertisement: bool,
    bloom: &[u8; 10],
    adv_hash: &[u8; 4],
    psm: u16,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut b = (2u8 << 5) & 0xE0; // version kV2
    if extended_advertisement {
        b |= (1 << 4) & 0x10;
    }
    b |= num_slots & 0x1F;
    out.push(b);
    out.extend_from_slice(bloom); // 10 bytes
    out.extend_from_slice(adv_hash); // 4 bytes
    out.extend_from_slice(&psm.to_be_bytes()); // 2 bytes BE
    out
}

/// Convenience: build the (header, full advertisement) pair to broadcast.
/// `header` goes in the 0xFEF3 service data; `advertisement` is served via GATT
/// or (route A) put on-air via extended advertising.
pub fn build_receiver_advertisement(
    endpoint_id: &[u8; 4],
    device_type: u8,
    name: &str,
) -> (Vec<u8>, Vec<u8>) {
    let endpoint_info = build_endpoint_info(device_type, name);
    let data = build_connections_advertisement(endpoint_id, &endpoint_info);
    let advertisement = build_medium_advertisement(&data, &device_token());
    let bloom = bloom_filter_10(SERVICE_ID);
    let adv_hash = advertisement_hash(&advertisement);
    let header = build_advertisement_header(1, true, &bloom, &adv_hash, 0);
    (header, advertisement)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur_matches_reference() {
        // Verified against mmh3 / the canonical MurmurHash3_x64_128.
        assert_eq!(
            murmur3_x64_128(b"NearbySharing", 0),
            (0x798579c252d5fe64, 0x94d2c58bcc87064a)
        );
    }

    #[test]
    fn service_id_hash_is_fc9f5e() {
        assert_eq!(service_id_hash(), vec![0xfc, 0x9f, 0x5e]);
    }

    #[test]
    fn bloom_filter_reference() {
        // Reference computed from google/nearby's algorithm.
        assert_eq!(
            bloom_filter_10("NearbySharing"),
            [0x20, 0x00, 0x00, 0x10, 0x02, 0x00, 0x00, 0x03, 0x00, 0x00]
        );
    }

    #[test]
    fn connections_advertisement_layout() {
        let info = build_endpoint_info(3, "Test");
        let data = build_connections_advertisement(b"ABCD", &info);
        assert_eq!(data[0], 0x23); // version 1 | pcp 3 (matches grishka's mDNS name byte)
        assert_eq!(&data[1..4], &[0xfc, 0x9f, 0x5e]); // service id hash
        assert_eq!(&data[4..8], b"ABCD"); // endpoint id
        assert_eq!(data[8] as usize, info.len()); // endpoint info size
    }

    #[test]
    fn medium_advertisement_version_byte() {
        let advert = build_medium_advertisement(b"xyz", &[0xaa, 0xbb]);
        assert_eq!(advert[0], 0x48); // version kV2 | socket kV2
        assert_eq!(&advert[1..4], &[0xfc, 0x9f, 0x5e]);
        assert_eq!(&advert[4..8], &(3u32).to_be_bytes()); // data size = 3
    }

    #[test]
    fn header_has_extended_flag() {
        let bloom = bloom_filter_10(SERVICE_ID);
        let header = build_advertisement_header(1, true, &bloom, &[1, 2, 3, 4], 0);
        assert_eq!(header.len(), 1 + 10 + 4 + 2);
        assert_eq!(header[0] & 0x10, 0x10); // extended advertisement flag set
        assert_eq!(header[0] >> 5, 2); // version kV2
    }
}
