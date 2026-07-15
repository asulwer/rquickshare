//! QR-code sharing.
//!
//! A sender shows a QR code carrying an ECDSA public key. When a phone opens
//! that URL, it starts advertising its mDNS service **even while hidden**, and
//! includes a QR TLV in its endpoint info. Both sides independently derive the
//! same two tokens from the QR's key material, which lets us recognise the
//! phone that scanned our code — with no Google account and no certificates.
//!
//! That's the point of this module: it's the one path to sending to a phone
//! that isn't in "Everyone" mode, since the scan itself is the authorization.
//!
//! Verified against a real Pixel 10: it advertised while hidden and we
//! decrypted its name out of the QR TLV.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::anyhow;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use p256::elliptic_curve::rand_core::OsRng;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::SecretKey;
use sha2::Sha256;

use crate::utils::{EndpointInfoRecord, TLV_TYPE_QR_CODE};

const QR_URL_PREFIX: &str = "https://quickshare.google/qrcode#key=";

/// A hidden peer's name is AES-GCM: 12-byte IV || ciphertext || 16-byte tag.
const GCM_IV_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

/// An active QR sharing session.
///
/// Each session owns a fresh keypair, so its tokens are unique: a peer that
/// scanned someone else's QR will never match ours.
pub struct QrSession {
    /// URL to render as a QR code for the peer to scan.
    pub url: String,
    /// A *visible* peer echoes this back verbatim in its QR TLV.
    pub advertising_token: [u8; 16],
    /// AES-128-GCM key decrypting a *hidden* peer's name from its QR TLV.
    pub name_key: [u8; 16],
}

impl QrSession {
    pub fn new() -> Result<Self, anyhow::Error> {
        // Only the public half goes in the QR. The private half is needed only
        // to skip the peer's accept prompt (`qr_code_handshake_data`), which we
        // don't do, so we don't retain it.
        let secret = SecretKey::random(&mut OsRng);
        let point = secret.public_key().to_encoded_point(true);

        // key param = 2-byte version (0) + SEC1 compressed point (0x02|0x03 || X)
        let mut key_param = Vec::with_capacity(35);
        key_param.extend_from_slice(&[0u8, 0u8]);
        key_param.extend_from_slice(point.as_bytes());

        let (advertising_token, name_key) = derive_tokens(&key_param)?;

        Ok(Self {
            url: format!("{QR_URL_PREFIX}{}", URL_SAFE_NO_PAD.encode(&key_param)),
            advertising_token,
            name_key,
        })
    }

    /// If `record` is the peer that scanned our QR, return its device name.
    ///
    /// A visible peer echoes the advertising token as-is; a hidden one sends
    /// its name encrypted under `name_key`, authenticated with the token.
    pub fn match_endpoint(&self, record: &EndpointInfoRecord) -> Option<String> {
        let qr = record.tlv(TLV_TYPE_QR_CODE)?;

        if qr == self.advertising_token {
            return Some(
                record
                    .device_name
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_owned()),
            );
        }

        self.decrypt_name(qr)
    }

    fn decrypt_name(&self, qr: &[u8]) -> Option<String> {
        if qr.len() <= GCM_IV_LEN + GCM_TAG_LEN {
            return None;
        }

        let cipher = Aes128Gcm::new_from_slice(&self.name_key).ok()?;
        let iv: [u8; GCM_IV_LEN] = qr[..GCM_IV_LEN].try_into().ok()?;
        let plain = cipher
            .decrypt(
                &Nonce::from(iv),
                Payload {
                    msg: &qr[GCM_IV_LEN..],
                    aad: &self.advertising_token,
                },
            )
            .ok()?;

        String::from_utf8(plain).ok()
    }
}

/// Derive both tokens from the decoded key param (version bytes included),
/// with an empty salt, exactly as the peer does.
fn derive_tokens(key_param: &[u8]) -> Result<([u8; 16], [u8; 16]), anyhow::Error> {
    let hk = Hkdf::<Sha256>::new(None, key_param);

    let mut advertising_token = [0u8; 16];
    hk.expand(b"advertisingContext", &mut advertising_token)
        .map_err(|e| anyhow!("HKDF expand (advertisingContext) failed: {e}"))?;

    let mut name_key = [0u8; 16];
    hk.expand(b"encryptionKey", &mut name_key)
        .map_err(|e| anyhow!("HKDF expand (encryptionKey) failed: {e}"))?;

    Ok((advertising_token, name_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::DeviceType;

    fn record(tlvs: Vec<(u8, Vec<u8>)>, hidden: bool, name: Option<&str>) -> EndpointInfoRecord {
        EndpointInfoRecord {
            device_type: DeviceType::Phone,
            hidden,
            device_name: name.map(|s| s.to_owned()),
            tlvs,
        }
    }

    #[test]
    fn test_url_carries_versioned_compressed_key() {
        let session = QrSession::new().unwrap();
        let encoded = session.url.strip_prefix(QR_URL_PREFIX).expect("url prefix");
        let key_param = URL_SAFE_NO_PAD.decode(encoded).unwrap();

        // 2-byte version + 33-byte SEC1 compressed point.
        assert_eq!(key_param.len(), 35);
        assert_eq!(&key_param[..2], &[0, 0]);
        assert!(key_param[2] == 2 || key_param[2] == 3);
    }

    #[test]
    fn test_matches_visible_peer_echoing_token() {
        let session = QrSession::new().unwrap();
        let rec = record(
            vec![(TLV_TYPE_QR_CODE, session.advertising_token.to_vec())],
            false,
            Some("Pixel"),
        );

        assert_eq!(session.match_endpoint(&rec).as_deref(), Some("Pixel"));
    }

    #[test]
    fn test_matches_hidden_peer_by_decrypting_name() {
        let session = QrSession::new().unwrap();

        // Encrypt a name the way a hidden phone does.
        let cipher = Aes128Gcm::new_from_slice(&session.name_key).unwrap();
        let iv = [7u8; GCM_IV_LEN];
        let sealed = cipher
            .encrypt(
                &Nonce::from(iv),
                Payload {
                    msg: b"Aaron's Pixel 10",
                    aad: &session.advertising_token,
                },
            )
            .unwrap();

        let mut tlv = iv.to_vec();
        tlv.extend_from_slice(&sealed);

        // 12 IV + 16 name + 16 tag - the same 44 bytes the Pixel really sent.
        assert_eq!(tlv.len(), 44);

        let rec = record(vec![(TLV_TYPE_QR_CODE, tlv)], true, None);
        assert_eq!(
            session.match_endpoint(&rec).as_deref(),
            Some("Aaron's Pixel 10")
        );
    }

    #[test]
    fn test_does_not_match_another_senders_qr() {
        let ours = QrSession::new().unwrap();
        let theirs = QrSession::new().unwrap();

        let rec = record(
            vec![(TLV_TYPE_QR_CODE, theirs.advertising_token.to_vec())],
            false,
            Some("Pixel"),
        );
        assert_eq!(ours.match_endpoint(&rec), None);
    }

    #[test]
    fn test_peer_without_qr_tlv_is_not_a_match() {
        let session = QrSession::new().unwrap();
        assert_eq!(session.match_endpoint(&record(vec![], true, None)), None);
    }

    #[test]
    fn test_garbage_qr_tlv_is_not_a_match() {
        let session = QrSession::new().unwrap();
        let rec = record(vec![(TLV_TYPE_QR_CODE, vec![0xaa; 44])], true, None);
        assert_eq!(session.match_endpoint(&rec), None);
    }

    #[test]
    fn test_sessions_are_unique() {
        let a = QrSession::new().unwrap();
        let b = QrSession::new().unwrap();

        assert_ne!(a.url, b.url);
        assert_ne!(a.advertising_token, b.advertising_token);
        assert_ne!(a.name_key, b.name_key);
    }
}
