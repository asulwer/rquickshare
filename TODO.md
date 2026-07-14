# rquickshare fork — TODO / roadmap

Tracks work on this fork: transport mediums, completed features, and parked
items. Check items off (`[x]`) as they land; for parked items, note why.

## Transport mediums

rquickshare currently supports only **WiFi LAN**. Quick Share (Nearby
Connections) negotiates among several mediums and "bandwidth-upgrades" to the
best available one. Goal: support them all.

> **Shared prerequisite:** every non-LAN medium rides on the Nearby Connections
> **bandwidth-upgrade negotiation** (`BandwidthUpgradeNegotiationFrame`), which
> is not implemented here and, per grishka's reverse-engineering, not publicly
> documented — quote: *"It is still not clear how the actual medium switch
> occurs."* Cracking this is the real first milestone; after it, each medium is
> mostly "plug in the platform transport."

- [x] **WiFi LAN** — mDNS discovery + TCP over a shared network. Implemented
      (the existing app). Both devices must be on the same network.
- [ ] **WiFi Direct** — peer-to-peer WiFi link, no shared network needed.
      Windows: WinRT `Windows.Devices.WiFiDirect`. Linux: wpa_supplicant P2P
      (hard). Blocked on the bandwidth-upgrade negotiation. **Next target.**
- [ ] **Hotspot** — one device runs a soft-AP, the other joins. Similar shape
      to WiFi Direct; platform soft-AP APIs vary.
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
      reverse-engineered and unit-tested (see `BLE_RECEIVER_DISCOVERY.md`), but
      needs a BLE **GATT server** (WinRT / bluer) to serve it, plus phase-2
      transfer work. Untestable on current hardware (Pixel 10 doesn't reproduce
      the AirDrop WiFi-drop). Parked with groundwork saved.

## Notes / ideas

- Upstream candidates to PR to Martichou/rquickshare: Windows support, the log
  fixes, send-text.
- Autostart: build/install the release, then enable Start on boot + Keep running
  on close + Start minimized.
