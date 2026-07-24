//! AES-256-CBC with PKCS7 padding, hardware-accelerated.
//!
//! Replaces the software `libaes`, which profiling showed spending ~80% of the
//! receive wall-clock time - it was the phone-to-PC throughput ceiling (~7 MB/s
//! with AES pinned at ~800ms of every second). The `aes` crate uses AES-NI on
//! x86 at runtime, which is over an order of magnitude faster, so the ceiling
//! moves to the protobuf decode and the link.
//!
//! One place for the cipher so all four call sites - inbound/outbound encrypt
//! and decrypt - stay identical.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// AES-256-CBC / PKCS7 decrypt. `key` must be at least 32 bytes, `iv` 16.
pub fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, anyhow::Error> {
    Aes256CbcDec::new_from_slices(&key[..32], iv)
        .map_err(|e| anyhow::anyhow!("aes256-cbc init (decrypt): {e}"))?
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|e| anyhow::anyhow!("aes256-cbc decrypt: {e}"))
}

/// AES-256-CBC / PKCS7 encrypt. `key` must be at least 32 bytes, `iv` 16.
pub fn aes256_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, anyhow::Error> {
    Ok(Aes256CbcEnc::new_from_slices(&key[..32], iv)
        .map_err(|e| anyhow::anyhow!("aes256-cbc init (encrypt): {e}"))?
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext))
}
