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

- [x] **WiFi LAN** — mDNS discovery + TCP over a shared network. Both devices
      must be on the same network. **Now dual-stack (IPv4 + IPv6):** discovery
      considers every address a peer advertises, ordered IPv6-first then IPv4
      (matching Quick Share's `address_candidates`), and tries each until one
      connects — previously an IPv6-only peer was ignored outright, and a
      multi-homed peer had no fallback. IPv6 link-local is skipped (connecting
      needs a scope id we don't have), and each probe is capped at 500ms so an
      unreachable candidate can't stall discovery. The listener binds dual-stack
      (`socket2`, `IPV6_V6ONLY` off) with an IPv4-only fallback, and IPv4-mapped
      peer addresses are normalized back to plain IPv4 so ids/logs don't depend
      on how the socket bound.
      **Parked — advertising our own IPv6** (`mdns.rs` stays on `AddrType::V4`
      rather than `AddrType::BOTH`): testing showed the Pixel 10 advertises IPv6
      (global + link-local) but *none of it was reachable* from the PC. So
      advertising ours would likely hand the phone addresses it can't reach
      either, and its fallback behaviour isn't ours to control — risking working
      IPv4 transfers for no measurable gain. Cause unconfirmed; most likely the
      network not routing IPv6 between clients, or the phone's Quick Share
      listener being IPv4-only (its WiFi firmware bug is a less likely third).
      **Retest on the Pixel 10 Pro:** if its advertised IPv6 is reachable from
      the PC, the cause was phone-side and the advertisement can be flipped; if
      it's still unreachable, it's the network and this stays parked for good.
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
- [x] WiFi LAN dual-stack (IPv4 + IPv6) discovery and listener — see above.
- [x] Accept card never appeared: a failing startup call (autostart, which throws
      under `tauri dev`) aborted setup before the `rs2js_channelmessage` listener
      was registered, so incoming transfers never reached the UI. Startup settings
      now load defensively with autostart isolated.
- [x] UI said "Received" for outbound transfers (and rendered a dangling "Saved
      to "): the transfer direction was never mapped into the frontend's
      `DisplayedItem`.

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
