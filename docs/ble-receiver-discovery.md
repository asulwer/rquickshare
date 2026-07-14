# BLE receiver discoverability (issue #425) — implementation spec

Goal: make rquickshare discoverable as a Quick Share **receiver over BLE**, so a
phone that is doing BLE-only discovery (e.g. Pixel 10 Pro with WiFi dropped
during AirDrop-compatible discovery) lists rquickshare as a target. After the
phone selects it, the phone reconnects to WiFi and the transfer runs over the
existing Wi-Fi LAN path.

This is a two-phase flow:

- **Phase 1 — discovery over BLE** (this document). Broadcast a Nearby
  Connections *discoverable-endpoint* advertisement so the phone can list us.
- **Phase 2 — transfer** (not covered here). After selection the phone likely
  opens the initial connection over the BLE "medium" and then bandwidth-upgrades
  to Wi-Fi LAN. Still to be reverse-engineered.

Reproduction / test loop (no AirDrop bug needed): turn **WiFi off + Bluetooth
on** on the phone, open Quick Share to send. Use **nRF Connect** on the phone to
confirm our advertisement bytes are on-air and well-formed.

All formats below are transcribed from Google's own open-source implementation:
`github.com/google/nearby`, `connections/implementation/mediums/ble/`.

---

## Service UUID

`kCopresenceServiceUuid` = `0000FEF3-0000-1000-8000-00805F9B34FB` (16-bit
**0xFEF3**). Note: this is NOT the `0xFE2C` used by our existing "wake" beacon —
the receiver advertisement lives under **0xFEF3**.

(There is also `kDctServiceUuid` = `0xFC73`, used for a different DCT path;
ignore for now.)

## Delivery: GATT vs extended advertising

A legacy BLE advertisement is 31 bytes; the full endpoint advertisement is
bigger, so Google uses one of two mechanisms:

- **Route B — GATT.** Advertise a small `BleAdvertisementHeader` under 0xFEF3;
  run a GATT server whose characteristic returns the full `BleAdvertisement`.
  (`BleAdvertisementHeader` header comment: "V2 puts the data inside a GATT
  characteristic".) Big new subsystem on both platforms.
- **Route A — extended advertising (what we're building first).** If BLE 5
  extended advertising is available, put the header + full advertisement on-air
  directly, no GATT. WinRT supports `UseExtendedAdvertisement`. Risk: the phone
  must *scan* extended advertisements for this to be seen — unverified.

---

## Layer 1: BleAdvertisementHeader (goes in the 0xFEF3 service data)

Byte layout: `[VERSION/EXT/NUM_SLOTS (1)][SERVICE_ID_BLOOM_FILTER (10)][ADVERTISEMENT_HASH (4)][PSM (2)]`

First byte:
- bits 7..5: version (3 bits) = `kV2` = 2  → `(2 << 5) & 0xE0`
- bit 4: extended-advertisement flag → `(ext << 4) & 0x10`
- bits 3..0 (mask 0x1F): num_slots → `num_slots & 0x1F` (use 1)

Then:
- `service_id_bloom_filter`: 10 bytes (see bloom filter algorithm below)
- `advertisement_hash`: 4 bytes = `SHA256(full BleAdvertisement bytes)[:4]`
- `psm`: 2 bytes big-endian (`0x0000` when no L2CAP)

## Layer 2: BleAdvertisement (the full advertisement; served via GATT or extended adv)

Regular (non-fast) layout:
`[VERSION/SOCKET/FAST/RSVD (1)][SERVICE_ID_HASH (3)][DATA_SIZE (uint32 BE)][DATA (n)][DEVICE_TOKEN (2)]`

First byte:
- bits 7..5: version = `kV2` = 2 → `(2 << 5) & 0xE0`
- bits 4..2: socket_version = `kV2` = 2 → `(2 << 2) & 0x1C`
- bit 1: fast-advertisement flag → `(fast << 1) & 0x02` (0 here; fast caps at 27 bytes and doesn't fit)
- bit 0: second-profile flag → 0

Then:
- `service_id_hash`: 3 bytes = `SHA256("NearbySharing")[:3]` = `FC 9F 5E`
  (omitted entirely in fast mode)
- `data_size`: uint32 big-endian = length of DATA
- `DATA`: the "connections advertisement" — **see Layer 3 (still to confirm)**
- `device_token`: 2 bytes = `SHA256(str(random_u32))[:2]`

Extended advertising adds an extra-fields byte + optional L2CAP PSM /
instant-connection data via `ByteArrayWithExtraField()`; can be empty for us.

## Layer 3: DATA — the Connections BLE advertisement (CONFIRMED)

Source: `connections/implementation/ble_advertisement.{h,cc}`. This is the
`advertisement_bytes` the Connections layer hands to `Ble::StartAdvertising`,
which the mediums layer wraps as Layer 2's DATA.

Non-fast layout:
`[VERSION/PCP (1)][SERVICE_ID_HASH (3)][ENDPOINT_ID (4)][ENDPOINT_INFO_SIZE (1)][ENDPOINT_INFO (n)][BLUETOOTH_MAC (6)][UWB_ADDRESS_SIZE (1)][UWB_ADDRESS (0)][EXTRA_FIELD (1)]`

- byte 0 = `(version << 5) & 0xE0 | (pcp & 0x1F)` = `(1 << 5) | 3` = **`0x23`**
  - version = `kV1` = 1; pcp = 3 (point-to-point).
  - **Cross-check:** grishka documents the mDNS *name* starting with `0x23` and
    calls it "PCP" — same byte, confirming version=1 / pcp=3 and the whole format.
- `service_id_hash` = `SHA256("NearbySharing")[:3]` = `FC 9F 5E` (3 bytes)
- `endpoint_id` = the 4 alphanumeric bytes (same as `lib.rs` `endpoint_id` /
  mDNS name), so a later WiFi hop can correlate.
- `endpoint_info_size` = 1 byte length of endpoint_info
- `endpoint_info` = the **base64-decoded** form of `gen_mdns_endpoint_info`:
  - 1 byte bitfield: `version(3) | visibility(1) | device_type(3) | reserved(1)` (`device_type << 1`)
  - 16 bytes: 2-byte salt + 14-byte encrypted-metadata key (random is fine)
  - 1 byte name length + UTF-8 device name
- `bluetooth_mac_address` = 6 bytes; if unknown, **6 zero bytes** (reserved)
- `uwb_address_size` = 1 byte = `0x00` (no UWB; still written in non-fast)
- `extra_field` = 1 byte = webrtc-connectable flag = `0x00`

We can reuse the existing endpoint-info builder — just skip the base64 step so we
emit raw bytes here (mDNS emits the base64 string of the same bytes).

---

## Algorithms

### SHA-256 hashes (crate: `sha2`, already a dependency)
- `service_id_hash` = `SHA256("NearbySharing")[:3]`
- `advertisement_hash` = `SHA256(<Layer-2 bytes>)[:4]`
- `device_token` = `SHA256(<ascii-decimal of a random u32>)[:2]`

### Service-ID bloom filter (10 bytes / 80 bits)
Guava/Murmur-based. For service id `s` = `"NearbySharing"`:
1. `h128 = MurmurHash3_x64_128(s, seed=0)` (crate: `murmur3`)
2. `hash64 = low 64 bits of h128`
3. `hash1 = hash64 & 0xFFFFFFFF` (as i32); `hash2 = (hash64 >> 32) & 0xFFFFFFFF` (as i32)
4. For `i` in 1..=5: `combined = (i32)(hash1 + i*hash2)`; if `combined < 0` then `combined = !combined`; `pos = (usize)combined % 80`; set bit `pos`.
5. Serialize the 80-bit set to 10 bytes. **Bit ordering is subtle**: Google's
   `operator ByteArray` walks the bitset's string form from the high end down in
   groups of 8 (rightmost char = bit position 0). Replicate exactly and unit-test
   against a known vector.

---

## WinRT extended advertising (Route A wiring)

Reuse the `blea_win.rs` publisher pattern but:
- set `BluetoothLEAdvertisementPublisher.UseExtendedAdvertisement = true`
- add two service-data sections (or as many as the phone needs):
  - UUID `0xFEF3` → `BleAdvertisementHeader` bytes
  - per-slot advertisement UUID (`0x00000000-0000-3000-8000-000000000000 | slot`) → full `BleAdvertisement` bytes
- run this advertiser while the receiver is **visible** (alongside `MDnsServer`,
  wired in `lib.rs::run`), using the same `endpoint_id`, device type = Laptop,
  and hostname as the mDNS service.

Linux equivalent (later): bluer advertising with the same service data.

---

## Open items before this can work

1. **Bloom-filter byte ordering** — replicate Google's bit reversal exactly and
   unit-test against a known value.
2. **Extended-adv assembly** — which service-data UUID carries the header vs the
   full advertisement, and whether the phone actually scans extended
   advertisements during Quick Share discovery (nRF Connect + WiFi-off test).
3. **Phase 2** — the post-selection connection (BLE socket → WiFi upgrade).

All three nested advertisement layers are now fully reverse-engineered and
cross-validated (Layer 3 byte 0 = `0x23` matches grishka's mDNS name).

## Verification milestones
- M1: nRF Connect shows our 0xFEF3 header + advertisement bytes on-air.
- M2: phone (WiFi off) lists rquickshare as a target.
- M3: selecting it leads to a WiFi transfer (phase 2).
