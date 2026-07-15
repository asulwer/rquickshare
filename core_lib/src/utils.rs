use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use get_if_addrs::get_if_addrs;
use hkdf::Hkdf;
use num_bigint::{BigUint, ToBigInt};
use p256::elliptic_curve::Generate;
use p256::{PublicKey, SecretKey};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use ts_rs::TS;

use crate::CUSTOM_DOWNLOAD;

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize, TS)]
#[ts(export)]
pub enum DeviceType {
    Unknown = 0,
    Phone = 1,
    Tablet = 2,
    Laptop = 3,
}

impl DeviceType {
    pub fn from_raw_value(value: u8) -> Self {
        match value {
            0 => DeviceType::Unknown,
            1 => DeviceType::Phone,
            2 => DeviceType::Tablet,
            3 => DeviceType::Laptop,
            _ => DeviceType::Unknown,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct RemoteDeviceInfo {
    pub name: String,
    pub device_type: DeviceType,
}

impl RemoteDeviceInfo {
    pub fn serialize(&self) -> Vec<u8> {
        // 1 byte: Version(3 bits)|Visibility(1 bit)|Device Type(3 bits)|Reserved(1 bit)
        let mut endpoint_info: Vec<u8> = vec![((self.device_type.clone() as u8) << 1) & 0b111];

        // 16 bytes: unknown random bytes
        endpoint_info.extend((0..16).map(|_| rand::rng().random_range(0..=255)));

        // Device name in UTF-8 prefixed with 1-byte length
        let mut name_chars = self.name.as_bytes().to_vec();
        if name_chars.len() > 255 {
            name_chars.truncate(255);
        }
        endpoint_info.push(name_chars.len() as u8);
        endpoint_info.extend(name_chars);

        endpoint_info
    }
}

pub fn gen_mdns_name(endpoint_id: [u8; 4]) -> String {
    let mut name_b = Vec::new();

    let pcp: [u8; 1] = [0x23];
    name_b.extend_from_slice(&pcp);

    name_b.extend_from_slice(&endpoint_id);

    let service_id: [u8; 3] = [0xFC, 0x9F, 0x5E];
    name_b.extend_from_slice(&service_id);

    let unknown_bytes: [u8; 2] = [0x00, 0x00];
    name_b.extend_from_slice(&unknown_bytes);

    URL_SAFE_NO_PAD.encode(&name_b)
}

pub fn gen_mdns_endpoint_info(device_type: u8, device_name: &str) -> String {
    let mut record = Vec::new();

    // 1 byte: Version(3 bits)|Visibility(1 bit)|Device Type(3 bits)|Reserved(1 bits)
    // Device types: unknown=0, phone=1, tablet=2, laptop=3
    record.push(device_type << 1);

    let unknown_bytes = rand::rng().random::<[u8; 16]>();
    record.extend_from_slice(&unknown_bytes);

    let device_name = device_name.as_bytes();
    let length = device_name.len() as u8;
    record.push(length);
    record.extend_from_slice(device_name);

    URL_SAFE_NO_PAD.encode(&record)
}

/// TLV record type carrying QR-code data (the advertising token, or the
/// AES-GCM encrypted device name when the peer is hidden).
pub const TLV_TYPE_QR_CODE: u8 = 1;

/// Parsed contents of the mDNS `n` (endpoint info) TXT record.
///
/// Layout:
/// - byte 0: version(3 bits) | visibility(1 bit) | device type(3 bits) | reserved(1 bit)
/// - bytes 1..17: 2-byte salt + 14-byte encrypted metadata key. This identifies
///   the advertiser's account, but is only meaningful to someone holding the
///   matching certificate from Google, so we don't interpret it.
/// - only when *visible*: 1-byte name length, then the UTF-8 device name.
/// - then: optional TLV records (1-byte type, 1-byte length, value).
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointInfoRecord {
    pub device_type: DeviceType,
    /// Set when the advertiser is hidden; such a peer publishes no plaintext
    /// name (it may instead carry an encrypted one in a QR TLV).
    pub hidden: bool,
    /// Plaintext device name. Always `None` when `hidden`.
    pub device_name: Option<String>,
    /// TLV records following the name, as (type, value).
    pub tlvs: Vec<(u8, Vec<u8>)>,
}

impl EndpointInfoRecord {
    /// Value of the first TLV record with `ttype`, if present.
    pub fn tlv(&self, ttype: u8) -> Option<&[u8]> {
        self.tlvs
            .iter()
            .find(|(t, _)| *t == ttype)
            .map(|(_, v)| v.as_slice())
    }
}

/// Parse a base64 (URL-safe, unpadded) endpoint info record.
pub fn parse_endpoint_info(encoded_str: &str) -> Result<EndpointInfoRecord, anyhow::Error> {
    let decoded_bytes = URL_SAFE_NO_PAD.decode(encoded_str)?;
    parse_endpoint_info_bytes(&decoded_bytes)
}

/// Parse a raw (already base64-decoded) endpoint info record.
pub fn parse_endpoint_info_bytes(bytes: &[u8]) -> Result<EndpointInfoRecord, anyhow::Error> {
    // The flag byte plus the 16 identity bytes are always present.
    if bytes.len() < 17 {
        return Err(anyhow!("Endpoint info too short: {} bytes", bytes.len()));
    }

    let flags = bytes[0];
    let hidden = (flags >> 4) & 0x1 == 1;
    let device_type = DeviceType::from_raw_value((flags >> 1) & 0x7);

    let mut i = 17usize;

    // A hidden advertiser omits the plaintext name entirely.
    let device_name = if hidden {
        None
    } else {
        let name_length = *bytes.get(i).ok_or_else(|| anyhow!("Missing name length"))? as usize;
        i += 1;
        let end = i
            .checked_add(name_length)
            .filter(|e| *e <= bytes.len())
            .ok_or_else(|| anyhow!("Invalid name length {name_length}"))?;
        let name = String::from_utf8(bytes[i..end].to_vec())?;
        i = end;
        Some(name)
    };

    // Optional TLV records. A malformed/truncated one only costs us the
    // remainder - keep whatever parsed cleanly rather than failing the peer.
    let mut tlvs = Vec::new();
    while i + 2 <= bytes.len() {
        let ttype = bytes[i];
        let tlen = bytes[i + 1] as usize;
        i += 2;

        let end = match i.checked_add(tlen) {
            Some(e) if e <= bytes.len() => e,
            _ => break,
        };
        tlvs.push((ttype, bytes[i..end].to_vec()));
        i = end;
    }

    Ok(EndpointInfoRecord {
        device_type,
        hidden,
        device_name,
        tlvs,
    })
}


pub async fn stream_read_exact(
    socket: &mut TcpStream,
    buf: &mut [u8],
) -> Result<(), anyhow::Error> {
    match socket.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn gen_ecdsa_keypair() -> (SecretKey, PublicKey) {
    let secret_key = SecretKey::generate();
    let public_key = secret_key.public_key();

    (secret_key, public_key)
}

pub fn encode_point(unsigned: Bytes) -> Result<Vec<u8>, anyhow::Error> {
    let big_int = BigUint::from_bytes_be(&unsigned)
        .to_bigint()
        .ok_or_else(|| anyhow!("Failed to convert to bigint"))?;

    Ok(big_int.to_signed_bytes_be())
}

pub fn hkdf_extract_expand(
    salt: &[u8],
    input: &[u8],
    info: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, anyhow::Error> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), input);
    let mut okm = vec![0u8; output_len];
    hkdf.expand(info, &mut okm)
        .map_err(|e| anyhow!("HKDF expand failed: {}", e))?;
    Ok(okm)
}

pub fn to_four_digit_string(bytes: &Vec<u8>) -> String {
    let k_hash_modulo = 9973;
    let k_hash_base_multiplier = 31;

    let mut hash = 0;
    let mut multiplier = 1;
    for &byte in bytes {
        let byte = byte as i8 as i32;
        hash = (hash + byte * multiplier) % k_hash_modulo;
        multiplier = (multiplier * k_hash_base_multiplier) % k_hash_modulo;
    }

    format!("{:04}", hash.abs())
}

pub fn gen_random(size: usize) -> Vec<u8> {
    let mut data = vec![0; size];
    rand::rng().fill_bytes(&mut data);

    data
}

pub fn get_download_dir() -> PathBuf {
    let cdown = CUSTOM_DOWNLOAD.read();
    match cdown {
        Ok(mg) => {
            if mg.is_some() {
                return mg.as_ref().unwrap().to_path_buf();
            }
        }
        Err(_) => {
            // TODO
        }
    }

    if let Some(user_dirs) = directories::UserDirs::new() {
        if let Some(dd) = user_dirs.download_dir() {
            return dd.to_path_buf();
        }

        return user_dirs.home_dir().to_path_buf();
    }

    Path::new("/").to_path_buf()
}

pub fn is_not_self_ip(ip_address: &IpAddr) -> bool {
    if let Ok(if_addrs) = get_if_addrs() {
        for if_addr in if_addrs {
            if if_addr.ip() == *ip_address {
                return false;
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gen_and_parse_mdns_info() {
        let device_name = "a_device_name";
        let device_type = DeviceType::Laptop;

        let info = gen_mdns_endpoint_info(device_type.clone() as u8, device_name);
        let record = parse_endpoint_info(&info).unwrap();

        assert_eq!(record.device_name.as_deref(), Some(device_name));
        assert_eq!(record.device_type, device_type);
    }

    #[test]
    fn test_parse_visible_endpoint_has_name_and_no_tlvs() {
        let info = gen_mdns_endpoint_info(DeviceType::Laptop as u8, "a_device_name");
        let record = parse_endpoint_info(&info).unwrap();

        assert!(!record.hidden);
        assert_eq!(record.device_name.as_deref(), Some("a_device_name"));
        assert_eq!(record.device_type, DeviceType::Laptop);
        assert!(record.tlvs.is_empty());
    }

    /// Real capture: a hidden Pixel 10 advertising QR data after scanning our
    /// code. 63 bytes = 1 flag + 16 identity + TLV(type 1, len 44).
    #[test]
    fn test_parse_hidden_endpoint_with_qr_tlv() {
        let bytes = hex::decode(
            "3288608fe48bf1d142fee5c7e4225c5974012cc95cc656ef7d2ceef9c9025cf1e4\
             7c1d28ec4e31ad31b4e977413ab8d1720ddff6a36e670fdf2e56f16028fb",
        )
        .unwrap();
        let record = parse_endpoint_info_bytes(&bytes).unwrap();

        // Hidden peers publish no plaintext name...
        assert!(record.hidden);
        assert_eq!(record.device_name, None);
        assert_eq!(record.device_type, DeviceType::Phone);

        // ...but do carry the QR TLV: 12-byte IV + 16-byte ciphertext + 16-byte tag.
        let qr = record.tlv(TLV_TYPE_QR_CODE).expect("QR TLV present");
        assert_eq!(qr.len(), 44);
    }

    /// Real capture: a hidden peer with no QR data (a nearby phone that never
    /// saw our code). Must parse, and must not invent a TLV.
    #[test]
    fn test_parse_hidden_endpoint_without_tlvs() {
        let bytes = hex::decode("328900d62e98303608c6f2654f36e9b2b5").unwrap();
        let record = parse_endpoint_info_bytes(&bytes).unwrap();

        assert!(record.hidden);
        assert_eq!(record.device_name, None);
        assert!(record.tlvs.is_empty());
        assert_eq!(record.tlv(TLV_TYPE_QR_CODE), None);
    }

    #[test]
    fn test_truncated_tlv_is_ignored_not_fatal() {
        // Flag + 16 identity + a TLV claiming 200 bytes but carrying 2.
        let mut bytes = vec![0x32];
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(&[TLV_TYPE_QR_CODE, 200, 0xaa, 0xbb]);

        let record = parse_endpoint_info_bytes(&bytes).unwrap();
        assert!(record.tlvs.is_empty());
    }

    #[test]
    fn test_too_short_endpoint_info_errors() {
        assert!(parse_endpoint_info_bytes(&[0x32, 0x00]).is_err());
    }

    #[test]
    fn test_device_type_from_raw_value() {
        assert_eq!(DeviceType::from_raw_value(0), DeviceType::Unknown);
        assert_eq!(DeviceType::from_raw_value(1), DeviceType::Phone);
        assert_eq!(DeviceType::from_raw_value(2), DeviceType::Tablet);
        assert_eq!(DeviceType::from_raw_value(3), DeviceType::Laptop);

        // The field is 3 bits wide, so a peer can hand us 4..=7. Those must
        // degrade to Unknown rather than panic.
        for v in 4..=7 {
            assert_eq!(DeviceType::from_raw_value(v), DeviceType::Unknown);
        }
    }

    #[test]
    fn test_remote_device_info_serialize_round_trips() {
        let rdi = RemoteDeviceInfo {
            name: "Aaron's PC".to_owned(),
            device_type: DeviceType::Laptop,
        };

        let record = parse_endpoint_info_bytes(&rdi.serialize()).unwrap();
        assert!(!record.hidden);
        assert_eq!(record.device_name.as_deref(), Some("Aaron's PC"));
        assert_eq!(record.device_type, DeviceType::Laptop);
    }

    #[test]
    fn test_serialize_truncates_overlong_name() {
        let rdi = RemoteDeviceInfo {
            name: "a".repeat(300),
            device_type: DeviceType::Phone,
        };

        // The name length is a single byte, so it cannot exceed 255.
        let bytes = rdi.serialize();
        assert_eq!(bytes[17], 255);

        let record = parse_endpoint_info_bytes(&bytes).unwrap();
        assert_eq!(record.device_name.unwrap().len(), 255);
    }

    #[test]
    fn test_gen_mdns_name_layout() {
        let raw = URL_SAFE_NO_PAD
            .decode(gen_mdns_name([0x41, 0x42, 0x43, 0x44]))
            .unwrap();

        assert_eq!(raw.len(), 10);
        assert_eq!(raw[0], 0x23); // PCP
        assert_eq!(&raw[1..5], &[0x41, 0x42, 0x43, 0x44]); // endpoint id
        assert_eq!(&raw[5..8], &[0xFC, 0x9F, 0x5E]); // Quick Share service id
        assert_eq!(&raw[8..], &[0x00, 0x00]);
    }

    #[test]
    fn test_gen_ecdsa_keypair_is_consistent_and_unique() {
        let (secret, public) = gen_ecdsa_keypair();
        assert_eq!(secret.public_key(), public);

        let (other, _) = gen_ecdsa_keypair();
        assert_ne!(secret.to_bytes(), other.to_bytes());
    }

    // `encode_point` follows Java BigInteger semantics: it encodes a *value*,
    // not a fixed-width coordinate.

    #[test]
    fn test_encode_point_prefixes_zero_when_high_bit_set() {
        // 0xff alone would read as -1, so a leading zero keeps it positive.
        assert_eq!(
            encode_point(Bytes::from_static(&[0xff])).unwrap(),
            vec![0x00, 0xff]
        );
    }

    #[test]
    fn test_encode_point_leaves_positive_value_alone() {
        assert_eq!(encode_point(Bytes::from_static(&[0x7f])).unwrap(), vec![0x7f]);
    }

    #[test]
    fn test_encode_point_strips_leading_zeros() {
        assert_eq!(
            encode_point(Bytes::from_static(&[0x00, 0x00, 0x01])).unwrap(),
            vec![0x01]
        );
    }

    #[test]
    fn test_encode_point_zero() {
        assert_eq!(encode_point(Bytes::from_static(&[0x00])).unwrap(), vec![0x00]);
    }

    /// RFC 5869 A.1 (SHA-256, basic case). A real known-answer test: it pins
    /// our HKDF wiring to the RFC rather than to itself, so a bad crate upgrade
    /// fails here instead of on the phone.
    #[test]
    fn test_hkdf_matches_rfc5869_test_case_1() {
        let ikm = hex::decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b").unwrap();
        let salt = hex::decode("000102030405060708090a0b0c").unwrap();
        let info = hex::decode("f0f1f2f3f4f5f6f7f8f9").unwrap();

        let okm = hkdf_extract_expand(&salt, &ikm, &info, 42).unwrap();
        assert_eq!(
            hex::encode(okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865"
        );
    }

    /// RFC 5869 A.3 (SHA-256, empty salt and info).
    #[test]
    fn test_hkdf_matches_rfc5869_test_case_3() {
        let ikm = hex::decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b").unwrap();

        let okm = hkdf_extract_expand(&[], &ikm, &[], 42).unwrap();
        assert_eq!(
            hex::encode(okm),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d\
             9d201395faa4b61a96c8"
        );
    }

    #[test]
    fn test_hkdf_rejects_output_longer_than_255_hashes() {
        // HKDF-SHA256 can emit at most 255 * HashLen bytes.
        assert!(hkdf_extract_expand(b"salt", b"ikm", b"info", 255 * 32).is_ok());
        assert!(hkdf_extract_expand(b"salt", b"ikm", b"info", 255 * 32 + 1).is_err());
    }

    #[test]
    fn test_to_four_digit_string_known_values() {
        assert_eq!(to_four_digit_string(&vec![]), "0000");
        assert_eq!(to_four_digit_string(&vec![0, 0, 0, 0]), "0000");
        assert_eq!(to_four_digit_string(&vec![1]), "0001");

        // 1*1 + 1*31 = 32
        assert_eq!(to_four_digit_string(&vec![1, 1]), "0032");

        // Bytes are read as *signed*: 0xff is -1, and the result is abs()'d.
        assert_eq!(to_four_digit_string(&vec![0xff]), "0001");
    }

    #[test]
    fn test_gen_random_length_and_distinctness() {
        assert_eq!(gen_random(0).len(), 0);
        assert_eq!(gen_random(32).len(), 32);
        assert_ne!(gen_random(32), gen_random(32));
    }

    #[test]
    fn test_is_not_self_ip_for_documentation_address() {
        // TEST-NET-3: reserved for documentation, never on a real interface.
        assert!(is_not_self_ip(&"203.0.113.1".parse().unwrap()));
    }
}
