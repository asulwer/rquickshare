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
//!
//! The scan also lets the phone skip its accept prompt: when we send to the
//! peer that scanned, our PairedKeyEncryption frame carries a signature made
//! with the QR's private key (see [`handshake_signature`]). How that works was
//! worked out by NearDrop (github.com/grishka/NearDrop, PROTOCOL.md).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::anyhow;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::elliptic_curve::Generate;
use p256::SecretKey;
use sha2::Sha256;

use crate::utils::{EndpointInfoRecord, TLV_TYPE_QR_CODE};

const QR_URL_PREFIX: &str = "https://quickshare.google/qrcode#key=";

/// A hidden peer's name is AES-GCM: 12-byte IV || ciphertext || 16-byte tag.
const GCM_IV_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

/// The QR signing key of each peer that scanned one of our codes, by the
/// endpoint id discovery reported it under. Discovery fills it in; a send to
/// one of these ids signs its handshake with the key.
pub type QrPeers = Arc<Mutex<HashMap<String, SigningKey>>>;

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
    /// The private half of the key in the QR, for `qr_code_handshake_data`.
    signing_key: SigningKey,
    peers: QrPeers,
}

impl QrSession {
    pub fn new(peers: QrPeers) -> Result<Self, anyhow::Error> {
        // Only the public half goes in the QR. The private half stays here to
        // sign the handshake with the peer that scans it.
        let secret = SecretKey::generate();
        let point = secret.public_key().to_sec1_point(true);

        // key param = 2-byte version (0) + SEC1 compressed point (0x02|0x03 || X)
        let mut key_param = Vec::with_capacity(35);
        key_param.extend_from_slice(&[0u8, 0u8]);
        key_param.extend_from_slice(point.as_bytes());

        let (advertising_token, name_key) = derive_tokens(&key_param)?;

        Ok(Self {
            url: format!("{QR_URL_PREFIX}{}", URL_SAFE_NO_PAD.encode(&key_param)),
            advertising_token,
            name_key,
            signing_key: SigningKey::from(&secret),
            peers,
        })
    }

    /// Records that the peer discovery reported as `endpoint_id` scanned this
    /// session's code, so a send to it can skip the peer's accept prompt.
    pub fn remember_peer(&self, endpoint_id: &str) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.insert(endpoint_id.to_owned(), self.signing_key.clone());
        }
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

/// The `qr_code_handshake_data` that tells the peer which scanned our code to
/// accept without asking: an ECDSA P-256 / SHA-256 signature of the UKEY2 auth
/// key (the one the PIN comes from) made with the QR's private key, as the
/// 64 bytes of R and S (IEEE P1363). Signing this session's auth key means it
/// can't be replayed into another connection.
pub fn handshake_signature(key: &SigningKey, auth_key: &[u8]) -> Vec<u8> {
    let signature: Signature = key.sign(auth_key);
    signature.to_bytes().to_vec()
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
        let session = QrSession::new(QrPeers::default()).unwrap();
        let encoded = session.url.strip_prefix(QR_URL_PREFIX).expect("url prefix");
        let key_param = URL_SAFE_NO_PAD.decode(encoded).unwrap();

        // 2-byte version + 33-byte SEC1 compressed point.
        assert_eq!(key_param.len(), 35);
        assert_eq!(&key_param[..2], &[0, 0]);
        assert!(key_param[2] == 2 || key_param[2] == 3);
    }

    #[test]
    fn test_matches_visible_peer_echoing_token() {
        let session = QrSession::new(QrPeers::default()).unwrap();
        let rec = record(
            vec![(TLV_TYPE_QR_CODE, session.advertising_token.to_vec())],
            false,
            Some("Pixel"),
        );

        assert_eq!(session.match_endpoint(&rec).as_deref(), Some("Pixel"));
    }

    #[test]
    fn test_matches_hidden_peer_by_decrypting_name() {
        let session = QrSession::new(QrPeers::default()).unwrap();

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
        let ours = QrSession::new(QrPeers::default()).unwrap();
        let theirs = QrSession::new(QrPeers::default()).unwrap();

        let rec = record(
            vec![(TLV_TYPE_QR_CODE, theirs.advertising_token.to_vec())],
            false,
            Some("Pixel"),
        );
        assert_eq!(ours.match_endpoint(&rec), None);
    }

    #[test]
    fn test_peer_without_qr_tlv_is_not_a_match() {
        let session = QrSession::new(QrPeers::default()).unwrap();
        assert_eq!(session.match_endpoint(&record(vec![], true, None)), None);
    }

    #[test]
    fn test_garbage_qr_tlv_is_not_a_match() {
        let session = QrSession::new(QrPeers::default()).unwrap();
        let rec = record(vec![(TLV_TYPE_QR_CODE, vec![0xaa; 44])], true, None);
        assert_eq!(session.match_endpoint(&rec), None);
    }

    #[test]
    fn test_handshake_signature_verifies_with_the_key_in_the_url() {
        use p256::ecdsa::signature::Verifier;
        use p256::ecdsa::VerifyingKey;

        let peers = QrPeers::default();
        let session = QrSession::new(peers.clone()).unwrap();
        session.remember_peer("abcd");
        let key = peers
            .lock()
            .unwrap()
            .get("abcd")
            .cloned()
            .expect("remembered");

        let auth_key = [0x5au8; 32];
        let sig = handshake_signature(&key, &auth_key);
        // R || S, as the peer expects: no DER wrapping.
        assert_eq!(sig.len(), 64);

        // The phone only has the public key from the QR it scanned.
        let encoded = session.url.strip_prefix(QR_URL_PREFIX).unwrap();
        let key_param = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        let public = VerifyingKey::from_sec1_bytes(&key_param[2..]).unwrap();
        let sig = Signature::from_slice(&sig).unwrap();
        assert!(public.verify(&auth_key, &sig).is_ok());
        assert!(public.verify(&[0u8; 32], &sig).is_err());
    }

    #[test]
    fn test_only_remembered_peers_get_a_key() {
        let peers = QrPeers::default();
        let session = QrSession::new(peers.clone()).unwrap();
        session.remember_peer("abcd");
        assert!(peers.lock().unwrap().contains_key("abcd"));
        assert!(!peers.lock().unwrap().contains_key("efgh"));
    }

    #[test]
    fn test_sessions_are_unique() {
        let a = QrSession::new(QrPeers::default()).unwrap();
        let b = QrSession::new(QrPeers::default()).unwrap();

        assert_ne!(a.url, b.url);
        assert_ne!(a.advertising_token, b.advertising_token);
        assert_ne!(a.name_key, b.name_key);
    }
}
