use std::collections::HashMap;

use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use self::info::{InternalFileInfo, TransferMetadata};
use crate::securegcm::ukey2_client_init::CipherCommitment;
use crate::utils::RemoteDeviceInfo;

#[cfg(feature = "experimental")]
mod ble;
#[cfg(feature = "experimental")]
pub use ble::*;
#[cfg(all(feature = "experimental", target_os = "linux"))]
mod blea;
#[cfg(all(feature = "experimental", target_os = "linux"))]
pub use blea::*;
#[cfg(all(feature = "experimental", target_os = "windows"))]
mod blea_win;
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub use blea_win::*;
// BLE receiver advertiser (issue #425). Broadcasts the 0xFEF3 discoverable
// header so a phone doing BLE-only discovery can list us. Serving the *full*
// advertisement (GATT / extended adv) is the next milestone.
#[cfg(all(feature = "experimental", target_os = "windows"))]
mod blea_recv_win;
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub use blea_recv_win::*;
// Pure byte-format builders for the BLE receiver advertisement, unit-tested.
// Compiled only where their sole consumer (blea_recv_win) is, so nothing is
// dead code on other targets - no blanket `allow(dead_code)` needed.
// Not Windows-only any more: the send path parses a *peer's* advertisement with
// the same code that builds ours, and that path runs on Linux too.
#[cfg(feature = "experimental")]
mod ble_receiver;
#[cfg(feature = "experimental")]
pub use ble_receiver::{parse_full_advertisement, parse_peer_advertisement};
// Reads the negotiated ATT MTU, which btleplug does not expose. Windows-only
// because it goes to WinRT for it; the send path falls back to a conservative
// packet size everywhere else.
#[cfg(all(feature = "experimental", target_os = "windows"))]
mod ble_mtu_win;
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub use ble_mtu_win::*;
// BLE *client* half of the Weave socket, for sending to a phone with WiFi off.
// btleplug rather than WinRT, so this one works on Linux too.
#[cfg(feature = "experimental")]
mod blea_send;
#[cfg(feature = "experimental")]
pub use blea_send::*;
// Finds phones advertising as receivers over BLE, so there is something to send
// to when the peer has no IP.
#[cfg(feature = "experimental")]
mod blea_discovery;
#[cfg(feature = "experimental")]
pub use blea_discovery::*;
// Windows soft-AP for the WIFI_HOTSPOT bandwidth-upgrade medium. Removed in
// 908ab5b because a phone can never accept that upgrade from a WiFi-LAN
// connection - with the note "it's in git history if BLE ever makes it
// reachable". BLE did: over the BLE socket the phone's ConnectionRequest omits
// WIFI_LAN entirely (its WiFi is off) and advertises WIFI_HOTSPOT, so the
// objection that removed this no longer applies. Restored verbatim.
// Joining a peer's AP, for when the *phone* hosts the upgrade medium.
#[cfg(all(feature = "experimental", target_os = "windows"))]
mod wifi_join_win;
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub use wifi_join_win::*;
#[cfg(all(feature = "experimental", target_os = "windows"))]
mod hotspot_win;
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub use hotspot_win::*;
// Windows WiFi Direct group owner for the WIFI_DIRECT bandwidth-upgrade medium.
#[cfg(all(feature = "experimental", target_os = "windows"))]
mod wifi_direct_win;
#[cfg(all(feature = "experimental", target_os = "windows"))]
pub use wifi_direct_win::*;
mod frame_reader;
pub use frame_reader::*;
mod inbound;
pub use inbound::*;
pub(crate) mod info;
mod mdns_discovery;
pub use mdns_discovery::*;
mod mdns;
pub use mdns::*;
mod outbound;
pub use outbound::*;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[ts(export)]
pub enum State {
    #[default]
    Initial,
    ReceivedConnectionRequest,
    SentUkeyServerInit,
    SentUkeyClientInit,
    SentUkeyClientFinish,
    SentPairedKeyEncryption,
    ReceivedUkeyClientFinish,
    SentConnectionResponse,
    SentPairedKeyResult,
    SentIntroduction,
    ReceivedPairedKeyResult,
    WaitingForUserConsent,
    ReceivingFiles,
    SendingFiles,
    Disconnected,
    Rejected,
    Cancelled,
    Finished,
}

#[derive(Debug, Default)]
pub struct InnerState {
    pub id: String,
    pub server_seq: i32,
    pub client_seq: i32,
    pub encryption_done: bool,

    // Subject to be used-facing for progress, ...
    pub state: State,
    pub remote_device_info: Option<RemoteDeviceInfo>,
    pub pin_code: Option<String>,
    pub transfer_metadata: Option<TransferMetadata>,
    pub transferred_files: HashMap<i64, InternalFileInfo>,

    // Everything needed for encryption/decryption/verif
    pub cipher_commitment: Option<CipherCommitment>,
    pub private_key: Option<SecretKey>,
    pub public_key: Option<PublicKey>,
    pub server_init_data: Option<Vec<u8>>,
    pub client_init_msg_data: Option<Vec<u8>>,
    pub ukey_client_finish_msg_data: Option<Vec<u8>>,
    pub decrypt_key: Option<Vec<u8>>,
    pub recv_hmac_key: Option<Vec<u8>>,
    pub encrypt_key: Option<Vec<u8>>,
    pub send_hmac_key: Option<Vec<u8>>,

    // Used to handle/track ingress transfer
    pub text_payload: Option<TextPayloadInfo>,
    // Used to handle egress text transfer: (payload_id, text bytes)
    pub outbound_text: Option<(i64, Vec<u8>)>,
    // pub text_payload_id: i64,
    // pub text_is_url: bool,
    // pub wifi_ssid: Option<String>,
    pub payload_buffers: HashMap<i64, Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum TextPayloadInfo {
    Url(i64),
    Text(i64),
    Wifi((i64, String)),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub enum TextPayloadType {
    Url,
    Text,
    Wifi,
}

impl TextPayloadInfo {
    fn get_i64_value(&self) -> i64 {
        match self {
            TextPayloadInfo::Url(value)
            | TextPayloadInfo::Text(value)
            | TextPayloadInfo::Wifi((value, _)) => value.to_owned(),
        }
    }
}
