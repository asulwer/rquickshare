# WiFi Direct medium + bandwidth upgrade — implementation spec

Goal: let rquickshare transfer over **WiFi Direct** (peer-to-peer, no shared
network), by implementing the Nearby Connections **bandwidth-upgrade** switch.
This same machinery unlocks the other mediums (hotspot, Bluetooth, WiFi Aware,
WebRTC) — only the transport at the end differs.

Feasibility: **tractable.** Unlike #425 (which needed an undocumented GATT
mechanism), the medium switch is fully defined in rquickshare's existing proto
(`offline_wire_formats.proto`) and demonstrated in Google's open-source
`connections/implementation/mediums/wifi_direct_bwu_handler.cc`.

## Bandwidth-upgrade state machine

`BandwidthUpgradeNegotiationFrame.EventType`:

1. `UPGRADE_PATH_AVAILABLE` — the **server** (group owner) offers a new medium +
   its `UpgradePathInfo` credentials (for WiFi Direct: ssid/password/port/
   frequency/gateway).
2. Client joins the new medium and opens a TCP socket to `gateway:port`.
3. `CLIENT_INTRODUCTION` — client sends its `endpoint_id` over the **new**
   channel so the server can match it to the existing session.
4. `CLIENT_INTRODUCTION_ACK` — server acknowledges.
5. `LAST_WRITE_TO_PRIOR_CHANNEL` / `SAFE_TO_CLOSE_PRIOR_CHANNEL` — retire the old
   (WiFi-LAN) channel; all further encrypted frames flow over the new one.
6. `UPGRADE_FAILURE` — abandon the upgrade, keep the current medium.

All of these frames are `OfflineFrame`s of type `BANDWIDTH_UPGRADE_NEGOTIATION`,
already present in the proto.

## WiFi Direct flow (from google/nearby's handler)

Server / group owner (`HandleInitializeUpgradedMediumForEndpoint`):
- `StartWifiDirect()` — become the Group Owner (create the P2P group).
- `GetCredentials()` — ssid, password, port, frequency, gateway.
- Send them in an `UPGRADE_PATH_AVAILABLE` frame (`WifiDirectCredentials`).

Client (`CreateUpgradedEndpointChannel`):
- Read `WifiDirectCredentials` from the frame.
- `ConnectWifiDirect(ssid, password, ...)` — join the group.
- Open a TCP socket to `gateway:port`, send `CLIENT_INTRODUCTION`, then continue.

## Proto (already in rquickshare)

`offline_wire_formats.proto` → `BandwidthUpgradeNegotiationFrame`:
- `UpgradePathInfo.WifiDirectCredentials { ssid, password, port, frequency, gateway }`
- also `WifiHotspotCredentials`, `WifiLanSocket`, `BluetoothCredentials`,
  `WifiAwareCredentials`, `WebRtcCredentials` — the other mediums are pre-wired.

## Windows platform layer (WinRT)

`Windows.Devices.WiFiDirect`:
- **Group Owner / legacy AP:** `WiFiDirectAdvertisementPublisher` with
  `LegacySettings` → a soft-AP with a known SSID + passphrase and DHCP, so a
  phone joins as a normal WiFi client. Extract SSID/passphrase + the local
  gateway IP for the credentials.
- **Client:** `WiFiDirectDevice` / connect to a given SSID + passphrase.

rquickshare is usually the receiver/server, so the **Group Owner** role is the
priority. Linux (wpa_supplicant P2P) is a separate, harder follow-up.

## Work breakdown

1. Implement the bandwidth-upgrade state machine in the offline-frame handler
   (emit/handle the 6 event types; manage old vs new channel).
2. Windows WiFi Direct group-owner via WinRT (create group, get credentials).
3. Re-plumb the encrypted stream onto the new socket after `CLIENT_INTRODUCTION`.
4. Test with the phone on no shared network.

## Open questions

- Which side initiates in practice for phone->PC vs PC->phone, and whether the
  phone will accept rquickshare as the group owner.
- Windows soft-AP DHCP/gateway IP retrieval specifics.
- Encryption-disable flag (`supports_disabling_encryption`) handling.
