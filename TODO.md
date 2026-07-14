# rquickshare fork — TODO / roadmap

Tracks work on this fork: transport mediums, completed features, and parked
items. Check items off (`[x]`) as they land; for parked items, note why.

## Transport mediums

rquickshare currently supports only **WiFi LAN**. Quick Share (Nearby
Connections) negotiates among several mediums and "bandwidth-upgrades" to the
best available one. Goal: support them all.

> **Shared prerequisite:** every non-LAN medium rides on the Nearby Connections
> **bandwidth-upgrade negotiation** (`BandwidthUpgradeNegotiationFrame`), not yet
> implemented here. Good news from the research pass: it's **fully defined** in
> our proto and in google/nearby's open source (see `docs/wifi-direct.md`), so it's
> replicable — despite grishka's note that "it is still not clear how the actual
> medium switch occurs." Implementing it is the first milestone; after it, each
> medium is mostly "plug in the platform transport."

- [x] **WiFi LAN** — mDNS discovery + TCP over a shared network. Implemented
      (the existing app). Both devices must be on the same network.
- [ ] **WiFi Direct** — peer-to-peer WiFi link. Protocol is feasible (see
      `docs/wifi-direct.md`), but the WinRT WiFi Direct *legacy publisher* did
      **not** activate a broadcasting AP on the Intel BE200 (reports Started, no
      virtual adapter appears). **Deprioritized** in favor of Hotspot, which uses
      the same soft-AP and is proven working (below).
- [ ] **Hotspot (soft-AP)** — **platform PROVEN + offer implemented; BLOCKED on a
      Pixel firmware bug.** rquickshare starts a soft-AP with known credentials
      via `NetworkOperatorTetheringManager` (POC: `core_lib/src/bin/hotspot_poc.rs`),
      and now offers a WIFI_HOTSPOT bandwidth upgrade at the correct post-accept
      moment (`accept_transfer`, opt-in via `RQS_TRY_HOTSPOT_UPGRADE`), on a
      blocking thread so it doesn't stall the handshake. Proto synced to current
      google/nearby (frame types 8-12, `BandwidthUpgradeRetryFrame`, etc.).
      **Findings from live testing (Pixel 10, Android 16):** the phone advertises
      it supports WIFI_HOTSPOT and, for a large file (172 MB), *pauses the
      transfer* (ack_bytes stays 0) to attempt the upgrade — so it genuinely
      wants it — but replies `UPGRADE_FAILURE` every time: **it cannot join our
      soft-AP.** This matches the widely-reported Pixel 10 Quick Share WiFi
      firmware bug (network-switch during Quick Share is broken). A failed offer
      also stalls the large transfer (no graceful fallback), so the offer stays
      OFF by default. **PARKED** pending Google's firmware fix or the Pixel 10
      Pro. Groundwork saved. **When resuming:** (1) verify the phone can join;
      (2) build the channel-swap (Increment B: CLIENT_INTRODUCTION ack +
      LAST_WRITE/SAFE_TO_CLOSE + move the encrypted stream); (3) tear down the
      hotspot + listener on transfer end (currently leaks port 8899).
- [ ] **Bluetooth (Classic / RFCOMM)** — low-bandwidth fallback transport.
- [ ] **WiFi Aware (NAN)** — newer; limited/uneven Windows support.
- [ ] **WebRTC** — Quick Share uses it via signaling infrastructure; likely
      needs Google-side signaling. Feasibility unclear.

## Completed (this fork)

- [x] Windows build support (was Linux/macOS only) — issue #295.
- [x] #268 — mDNS discovery busy-spin log flood fix.
- [x] #413 — Windows BLE "wake" advertiser (auto-discovery when sending).
- [x] mDNS/transfer log-noise fixes (daemon shutdown, post-Finished disconnect).
- [x] #67 — send text/URLs to Android (Ctrl+V + clipboard auto-sync).
- [x] #315 — automatic dark theme following the OS.
- [x] Cargo workspace + single-source version; dependency/deprecation cleanup;
      `tauri dev` rebuild-loop fix.

## Parked

- [ ] **#425 — BLE receiver discoverability.** Advertisement format fully
      reverse-engineered and unit-tested (see `docs/ble-receiver-discovery.md`), but
      needs a BLE **GATT server** (WinRT / bluer) to serve it, plus phase-2
      transfer work. Untestable on current hardware (Pixel 10 doesn't reproduce
      the AirDrop WiFi-drop). Parked with groundwork saved.

## Notes / ideas

- Upstream candidates to PR to Martichou/rquickshare: Windows support, the log
  fixes, send-text.
- Autostart: build/install the release, then enable Start on boot + Keep running
  on close + Start minimized.
