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
- [x] **QR-code sharing — send to a phone that isn't set to "Everyone".**
      Displaying a QR (our ECDSA public key inside a
      `https://quickshare.google/qrcode#key=` URL) makes a phone that opens it
      start advertising **even while hidden**: the scan *is* the authorization,
      so no Google account and no certificates are involved. Both sides derive an
      advertising token and a name-encryption key from the QR key material via
      HKDF-SHA256; we recognise the scanner either by matching the token (visible
      peer) or by AES-GCM-decrypting the name it advertised (hidden peer), then
      auto-send to it. This required teaching the endpoint-info parser about the
      visibility bit and TLV records, which it previously ignored entirely (it
      stopped at the device name and errored on hidden peers). Verified end to
      end against a hidden Pixel 10.

## Parked

- [ ] **#425 — BLE receiver discoverability.** Advertisement format fully
      reverse-engineered and unit-tested (see `BLE_RECEIVER_DISCOVERY.md`), but
      needs a BLE **GATT server** (WinRT / bluer) to serve it, plus phase-2
      transfer work. Untestable on current hardware (Pixel 10 doesn't reproduce
      the AirDrop WiFi-drop). Parked with groundwork saved.
- [ ] **Skip the receiver's accept prompt on a QR send.** The mechanism is
      documented: put an ECDSA signature of the UKEY2 auth key (IEEE P1363
      format, R||S, 64 bytes) into `qr_code_handshake_data` inside the
      `PairedKeyEncryptionFrame`, signed with the QR's private key. Parked on
      purpose — the cost/benefit is poor:
      - The payoff is **one tap**, and it's arguably a tap worth keeping: it's
        the receiver confirming what lands on their phone. The annoyance that
        actually motivated this work (the "Everyone" toggle, which reverts every
        10 minutes and broke outright on current Pixel firmware) is already
        solved by QR sharing above.
      - grishka's own note on it: *"TODO: figure out why this sometimes fails and
        the prompt still appears"* — so the realistic outcome is "skips the tap
        sometimes".
      - The blast radius is disproportionate. It needs `qr_code_handshake_data`
        added to `PairedKeyEncryptionFrame` (our `wire_format.proto` lacks it —
        take the field number from Google's source rather than guessing: a wrong
        number would silently corrupt a frame that works today), p256's `ecdsa`
        feature, `QrSession` retaining its secret key, and — the real cost — QR
        state threaded from *discovery* into the *outbound* path. `TcpServer` is
        spawned in `run()` before any QR session exists, so this means a shared
        `Arc<RwLock<Option<QrSession>>>` reaching code that carries **every**
        transfer, to benefit only QR sends, and only sometimes.
      Revisit only if the accept prompt turns out to be a real irritation in
      daily use.

## Notes / ideas

- **Contacts / "Your devices" visibility is not achievable for a third party** —
  don't re-chase it. The phone identifies a contact by decrypting the 16 identity
  bytes in our mDNS advertisement (2-byte salt + 14-byte encrypted metadata key,
  which encodes an *account identifier*) against certificates it downloaded from
  Google for that account. We advertise random bytes there — as NearDrop does —
  which is precisely why only "Everyone" works, and why the paired-key frame we
  send is random too. Obtaining real credentials means authenticating as the user
  and registering a certificate with Google's **private** Nearby Sharing backend;
  Chromium does exactly this, but with privileged OAuth scopes reserved for
  Google's own clients. It's an access-control boundary by design (identity
  verification is the whole point of contacts mode), not a hard problem to grind
  through. QR sharing solves the real underlying need instead.
- Upstream candidates to PR to Martichou/rquickshare: Windows support, the log
  fixes, send-text.
- Autostart: build/install the release, then enable Start on boot + Keep running
  on close + Start minimized.
