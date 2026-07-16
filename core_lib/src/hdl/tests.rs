//! Loopback handshake tests: a real `OutboundRequest` driven against a real
//! `InboundRequest` over an in-memory duplex pair, with no socket and no phone.
//!
//! What these prove: the two sides agree with *each other*, so a regression in
//! the UKEY2 exchange, the ECDH, or the key derivation fails here instead of on
//! the phone.
//!
//! What these cannot prove: that we agree with Google. Both sides run our code,
//! so a shared misunderstanding of the protocol passes happily. Only the
//! known-answer tests built from real captures can catch that.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use libaes::{Cipher, AES_256_KEY_LEN};
use prost::Message;
use sha2::Sha256;
use tokio::io::{duplex, DuplexStream};
use tokio::sync::broadcast;

use super::{InboundRequest, OutboundPayload, OutboundRequest, State};
use crate::securegcm::DeviceToDeviceMessage;
use crate::securemessage::{HeaderAndBody, SecureMessage};
use crate::utils::{derive_d2d_keys, to_four_digit_string, DeviceType, RemoteDeviceInfo};

/// Run the UKEY2 handshake to completion and hand back both sides for
/// inspection.
async fn run_handshake() -> (InboundRequest<DuplexStream>, OutboundRequest<DuplexStream>) {
    let (client, server) = duplex(64 * 1024);

    // Separate channels: in production these are separate processes, and a
    // shared one would let each side observe the other's UI traffic.
    let (in_tx, _in_rx) = broadcast::channel(64);
    let (out_tx, _out_rx) = broadcast::channel(64);

    let mut ir = InboundRequest::new(server, "inbound".to_owned(), in_tx);
    let mut or = OutboundRequest::new(
        *b"abcd",
        client,
        "outbound".to_owned(),
        out_tx,
        OutboundPayload::Text("hello".to_owned()),
        RemoteDeviceInfo {
            name: "peer".to_owned(),
            device_type: DeviceType::Laptop,
        },
    );

    // Same prelude `TcpServer::connect` performs before its handle loop.
    or.send_connection_request()
        .await
        .expect("send_connection_request");
    or.send_ukey2_client_init()
        .await
        .expect("send_ukey2_client_init");

    // The duplex buffers, so the sides need not run in lockstep: each drives
    // itself until its own half of the handshake is done. Inbound finishes one
    // frame earlier than outbound, which then reads the already-buffered
    // connection response.
    let inbound = async move {
        while ir.state.state != State::SentConnectionResponse {
            ir.handle().await.expect("inbound handle");
        }
        ir
    };
    let outbound = async move {
        while or.state.state != State::SentPairedKeyEncryption {
            or.handle().await.expect("outbound handle");
        }
        or
    };

    // A deadlock here should fail the test, not hang the suite.
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(inbound, outbound)
    })
    .await
    .expect("handshake timed out")
}

/// The point of the whole exchange: each side's send key is the other's
/// receive key. This is what a bad crypto-crate upgrade breaks silently.
#[tokio::test]
async fn loopback_handshake_derives_matching_keys() {
    let (ir, or) = run_handshake().await;

    assert!(ir.state.decrypt_key.is_some(), "inbound derived no keys");
    assert!(or.state.decrypt_key.is_some(), "outbound derived no keys");

    assert_eq!(ir.state.decrypt_key, or.state.encrypt_key);
    assert_eq!(ir.state.encrypt_key, or.state.decrypt_key);
    assert_eq!(ir.state.recv_hmac_key, or.state.send_hmac_key);
    assert_eq!(ir.state.send_hmac_key, or.state.recv_hmac_key);
}

/// Both sides must show the user the same 4-digit code, or the human
/// verification step is meaningless.
#[tokio::test]
async fn loopback_handshake_agrees_on_pin_code() {
    let (ir, or) = run_handshake().await;

    assert!(ir.state.pin_code.is_some(), "inbound has no pin code");
    assert_eq!(ir.state.pin_code, or.state.pin_code);
}

/// The connection request must survive the round trip: the receiver names the
/// sender from the endpoint info it parsed off the wire.
#[tokio::test]
async fn loopback_handshake_carries_device_info() {
    let (ir, _or) = run_handshake().await;

    let rdi = ir
        .state
        .remote_device_info
        .expect("inbound learned no remote device info");
    assert_eq!(rdi.device_type, DeviceType::Laptop);
    assert!(!rdi.name.is_empty());
}

/// Two handshakes must not derive the same keys: the keypairs are ephemeral.
#[tokio::test]
async fn loopback_handshake_keys_are_session_unique() {
    let (first, _) = run_handshake().await;
    let (second, _) = run_handshake().await;

    assert_ne!(first.state.decrypt_key, second.state.decrypt_key);
}

// ---------------------------------------------------------------------------
// Known-answer tests from a real Pixel session (2026-07-15).
//
// Unlike the loopback tests above, these contain bytes *the phone produced*.
// They are the only tests here that can catch us and Google disagreeing: a
// shared misunderstanding of the protocol passes every loopback test and fails
// these.
//
// Captured with the `capture!` macro in inbound.rs. The keys are from a
// long-dead ephemeral session and protect nothing.
// ---------------------------------------------------------------------------

/// SHA-256 of the ECDH shared secret for that session.
const KAT_DERIVED_SECRET: &str =
    "f641f2dec33098baa9f2c9be5ea4af173e42023c07e44e129fa9eb8cf884446c";

/// The client init frame followed by the server init frame, as they went over
/// the wire.
const KAT_UKEY_INFO: &str = "080212830108011220f3c5ff8d000084653bcd891e8ed4c1f6a3a3f71bca1755b4\
6ff9fe41a245d6ed1a44086412403a02631fa9aa3a9e64b06c01d2d568cfff95a8f6fb5bb8067c614399c21877bc24c8\
d5db4a68f2c3b7e3ddb9a48bbdfb6664224e1847e5abc7120885595dbefa22174145535f3235365f4342432d484d4143\
5f5348413235360803127008011220c94e6f6bcf03b8ac92c5131a41b3af2efc9657d5e1f178db416d9f415d6e79e418\
642248080112440a201bb43809243dba4b29c2ad8db88ddc2d5a31440759627bc49305041e26ced6451220313ce5334a\
c3a7bf3a3f7396040132953bd0dac76afbb4fe8dd09b8da197557b";

const KAT_AUTH_STRING: &str = "babba2b81de228efa8c34f543727ade07fa9cb6c5d200c5434a281f519a6e026";
const KAT_DECRYPT_KEY: &str = "3e7277e5978988543db4fe98b0a04758a0860af992464ad02c8dab6084c80e28";
const KAT_RECV_HMAC_KEY: &str = "7718dabc0286a6bded78319cd0829d34179d0fd70d24fefe35e4ed254c6268a0";
const KAT_ENCRYPT_KEY: &str = "6d0a653efac690dcee0b6f32214ae463883b3a866e907e0d4b10d68b2bfe4387";
const KAT_SEND_HMAC_KEY: &str = "bcca2d7c6ddfa1f4f7b466d9332f2d15815abd3cdf9d9f5e5d3051e6db3e4fbb";

/// The first encrypted SecureMessage the Pixel sent after the handshake.
const KAT_SECURE_MESSAGE: &str = "0ab1010a1c080110022a101aa0730b9d41a68db34ae1c60c203b3b3204080d\
100112900160593ec3ef51d63a907aee81569c8bc46413800ee0ed273f2ee68a84a8e718efe73889c24a0f71d3d1bb7d\
e5746e65906b112037938cac3b243d22ec7cccace4f3c0b398f5c1e6062b6273356a0f2a613f6bceecf5f59c81f2369b\
1c4d7773c2ad70f578b0174cf3a37d2cf5b7b6feca85cf79a7c43558cde23f5a85541ad70a80dac9935f9caab51f2fc8\
2bd316bce91220c71a53b4ce48a7885d7ef50bf340cd325dcbdcbc0b64557e2553af9e9e1b51de";

/// Pins every label and salt in the HKDF ladder to a session the phone
/// accepted. Changing `ENC:2`, `SIG:1`, `client`/`server`, either salt, or
/// either UKEY2 label breaks this - where the loopback tests would not notice,
/// because both sides would drift together.
#[test]
fn kat_derives_the_keys_a_real_pixel_agreed_with() {
    let keys = derive_d2d_keys(
        &hex::decode(KAT_DERIVED_SECRET).unwrap(),
        &hex::decode(KAT_UKEY_INFO).unwrap(),
    )
    .unwrap();

    assert_eq!(hex::encode(&keys.auth_string), KAT_AUTH_STRING);
    assert_eq!(hex::encode(&keys.client_key), KAT_DECRYPT_KEY);
    assert_eq!(hex::encode(&keys.client_hmac_key), KAT_RECV_HMAC_KEY);
    assert_eq!(hex::encode(&keys.server_key), KAT_ENCRYPT_KEY);
    assert_eq!(hex::encode(&keys.server_hmac_key), KAT_SEND_HMAC_KEY);
}

/// The phone displayed 7191 for this session; so must we, or the human
/// verification step is theatre.
#[test]
fn kat_pin_code_matches_the_real_session() {
    let keys = derive_d2d_keys(
        &hex::decode(KAT_DERIVED_SECRET).unwrap(),
        &hex::decode(KAT_UKEY_INFO).unwrap(),
    )
    .unwrap();

    assert_eq!(to_four_digit_string(&keys.auth_string), "7191");
}

/// The proof that our key derivation matches Google's: verify an HMAC the
/// *phone* computed, using a key *we* derived. Nothing but agreement on the
/// full ladder makes this pass.
#[test]
fn kat_verifies_an_hmac_the_pixel_computed() {
    let keys = derive_d2d_keys(
        &hex::decode(KAT_DERIVED_SECRET).unwrap(),
        &hex::decode(KAT_UKEY_INFO).unwrap(),
    )
    .unwrap();

    let smsg = SecureMessage::decode(&*hex::decode(KAT_SECURE_MESSAGE).unwrap()).unwrap();

    let mut hmac = Hmac::<Sha256>::new_from_slice(&keys.client_hmac_key).unwrap();
    hmac.update(&smsg.header_and_body);

    assert_eq!(hmac.finalize().into_bytes()[..], smsg.signature[..]);
}

/// And that we can actually read what it sent: AES-256-CBC decrypt a frame the
/// phone encrypted, and get a well-formed D2D message out.
#[test]
fn kat_decrypts_a_frame_the_pixel_encrypted() {
    let keys = derive_d2d_keys(
        &hex::decode(KAT_DERIVED_SECRET).unwrap(),
        &hex::decode(KAT_UKEY_INFO).unwrap(),
    )
    .unwrap();

    let smsg = SecureMessage::decode(&*hex::decode(KAT_SECURE_MESSAGE).unwrap()).unwrap();
    let hab = HeaderAndBody::decode(&*smsg.header_and_body).unwrap();

    let mut cipher = Cipher::new_256(keys.client_key[..AES_256_KEY_LEN].try_into().unwrap());
    cipher.set_auto_padding(true);
    let decrypted = cipher.cbc_decrypt(hab.header.iv(), &hab.body);

    let d2d = DeviceToDeviceMessage::decode(&*decrypted).unwrap();
    assert_eq!(d2d.sequence_number(), 1);

    // The body must be a decodable offline frame, not merely non-garbage.
    let offline =
        crate::location_nearby_connections::OfflineFrame::decode(d2d.message()).unwrap();
    assert_eq!(
        offline.v1.unwrap().r#type(),
        crate::location_nearby_connections::v1_frame::FrameType::PayloadTransfer
    );
}
