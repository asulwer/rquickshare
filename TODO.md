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
      **Parked for good — advertising our own IPv6** (`mdns.rs` stays on
      `AddrType::V4` rather than `AddrType::BOTH`): the Pixel 10 advertises IPv6
      (global + link-local) but *none of it is reachable* from the PC. So
      advertising ours would likely hand the phone addresses it can't reach
      either, and its fallback behaviour isn't ours to control — risking working
      IPv4 transfers for no measurable gain.
      **Retested on the Pixel 10 Pro / Android 16 (2026-07-16): still
      unreachable**, identical symptom (`[2605:...]:53601 unreachable, trying
      next candidate`). Two different phones on two Android versions failing the
      same way rules out the phone: it's the network not routing IPv6 between
      clients, or Android not accepting inbound IPv6. New hardware will not
      change this — only a different network would, so stop retesting it.
- [ ] **WiFi Direct** — peer-to-peer WiFi link. **This is the only bandwidth
      upgrade an Android phone will accept from a WiFi-LAN connection, so it is
      the priority** (it was previously deprioritized in favour of Hotspot — that
      was backwards; see the Hotspot entry for the proof). Protocol spec in
      `docs/wifi-direct.md`.

      **TESTING GOTCHA — read first, it will waste your afternoon otherwise.**
      A failed WiFi Direct upgrade **wedges the phone's Nearby stack**. After it,
      opening Quick Share on the phone to send drops the phone's WiFi, and it
      keeps doing so on every subsequent attempt — *with no group of ours up and
      the app doing nothing but mDNS + BLE*, so it reads exactly like our
      advertising is at fault. It isn't.
      **Reset: disable the Quick Share extension on the phone, then re-enable it.**
      WiFi then stops dropping. Do this between attempts.
      Mechanism is visible in logcat — the phone cannot tear down its own P2P
      group after our join fails:
      ```
      NearbyMediums: Failed to remove WiFi Direct group — [2]BUSY
      NearbyMediums: Failed to cancel Wifi Direct group: [2]BUSY.
      ```
      `[2]` is `WifiP2pManager.BUSY`. The stale P2P state survives until the
      Nearby stack restarts. This is also why the offer must stay gated behind
      `RQS_TRY_WIFI_DIRECT_UPGRADE` and must not ship on by default: while the
      join is broken, every offer risks wedging the peer's radio.
      (Do not re-diagnose this as "our app drops the phone's WiFi" — that was an
      earlier, separate bug, caused by the hotspot leaking via `mem::forget` and
      surviving the process. That code is deleted and that bug is fixed.)

      **PLATFORM PROVEN 2026-07-17 — the old "BE200 can't do it" note was wrong.**
      The blocker was a single unset property: `IsAutonomousGroupOwnerEnabled`.
      Without it the publisher only advertises *presence* and waits for a peer to
      negotiate group ownership, so no group is created — no AP, no virtual
      adapter. `Start()` then reports `Started` truthfully about the
      *advertisement*, which we misread as the adapter refusing the role.
      With `advertisement.SetIsAutonomousGroupOwnerEnabled(true)` (see
      `core_lib/src/bin/wifi_direct_poc.rs`), verified end to end against a Pixel
      10 Pro:
      - publisher reports `status=Started, error=Success` (the `StatusChanged`
        event carries a `WiFiDirectError` — the only place Windows explains a
        failure; polling `Status()` alone hides it)
      - a real legacy SSID broadcasts (`DIRECT-DLAARONPCUPYQ` / `KYoNRZKV`)
      - the phone associates and gets a DHCP lease (`192.168.137.37`)
      - it reaches a TCP socket on the group owner: `192.168.137.1:8899`
      The GO lands on the **same 192.168.137.1 ICS gateway as the Mobile
      Hotspot**, so `hotspot_gateway_ip()` in `hotspot_win.rs` works unchanged for
      WiFi Direct credentials.
      **Caveats worth keeping:** the adapter appears as a *plain* `Wi-Fi N` entry
      (here `Wi-Fi 5`) with no "Direct" in its name or description — don't hunt
      for it by name. And this PC is on **Ethernet**, leaving the WiFi radio free
      to be an AP; a laptop using WiFi for internet must do both jobs on one radio
      and may not behave the same. Untested.

      Why it's reachable when Hotspot isn't — `bwu_manager.cc` guards them
      asymmetrically:
      ```cpp
      if (channel_manager_->isWifiLanConnected() &&
          ((upgrade_medium == Medium::WIFI_HOTSPOT) ||
           ((upgrade_medium == Medium::WIFI_DIRECT) &&
            (client->GetLocalOsInfo().type() == OsInfo::WINDOWS))))
      ```
      `client` is the *phone's* ClientProxy, so `GetLocalOsInfo()` is ANDROID and
      the WIFI_DIRECT arm is false. WIFI_HOTSPOT has no such escape.

      **Live-negotiation progress 2026-07-16.** The offer now gets *past* the
      guard that killed Hotspot — the phone accepts WIFI_DIRECT as a medium and
      fails on the contents instead. Two bugs found so far, both in what we send:

      1. **NUL-padded passphrase (fixed).** `PasswordCredential.Password()` hands
         back a NUL-padded HSTRING and `to_string()` keeps it, so we advertised
         `password: "PuLLMSIN\0"`. WPA2 requires 8–63 *printable* ASCII, so the
         phone rejected the offer at validation — `UPGRADE_FAILURE` **1 second**
         after the offer, radio never touched, our own `\0` echoed back at us.
         Invisible in the POC, which only ever `println!`'d the value.
         `trim_nuls()` in `wifi_direct_win.rs` fixes it; the password now arrives
         clean (`CYk9QDSW`) and the failure moved to **12 seconds** — the phone
         now genuinely tries to join and times out.
      ## STATE OF PLAY 2026-07-16 (end of session) — READ THIS FIRST

      **The single biggest lesson: every answer came from `adb logcat` on the
      phone. Nothing from the Windows side ever diagnosed anything.** Seven
      theories were reasoned out from our end and all seven were wrong. Start
      with logcat. Procedure and the reset gotcha are documented above.

      ### PROVEN (do not re-litigate)

      1. **`LegacySettings` suppresses the P2P Group Owner bit.** Measured in the
         peer's own discovery, same phone, same code, one property toggled:
         ```
         legacy ON : P2P-DEVICE-FOUND ... name='AARONPC' group_capab=0x88
         legacy OFF: P2P-DEVICE-FOUND ... name='AARONPC' group_capab=0x8b   ← bit0 = GO
         ```
         With legacy on we ran an autonomous GO advertising "I am not a group
         owner", so the phone attempted GO *negotiation* rather than a join, and
         an autonomous GO won't negotiate → sub-second
         `P2P-GROUP-FORMATION-FAILURE`. The BSSID corroborates: legacy off splits
         it from the device address (`72:08:10:a2:6a:b6` vs `p2p_dev_addr
         70:08:10:a2:6a:b7`) — what a real GO looks like on air.
         **Legacy is now OFF by default.** `RQS_WIFI_DIRECT_LEGACY=1` restores it.
      2. **`legacy.Ssid()` / `Passphrase()` still work with legacy disabled.** So
         we get the GO bit *and* the ssid the phone demands. No trade-off.
      3. **The phone discovers us by name.** `P2P-DEVICE-FOUND name='AARONPC'`,
         `config_methods=0x11e8` (PushButton present), `dev_capab=0x25`
         (Invitation supported). Device-name discovery works.
      4. **The GO follows the host's station channel.** Host associated on ch48 →
         analyser measured the group on **ch48**. Host on Ethernet → group picked
         **ch157**. So when a STA link exists we can compute the frequency from
         `log_wlan_interfaces()`, which *can* read the station's channel.
      5. **Frequency is not the blocker.** Sent the correct 5240 with the group
         verified on ch48: unchanged failure. Also dead: the band theory (both
         ends 5GHz) and the channel-concurrency theory.
      6. **The phone's GMS is older than google/nearby `main`.** Current upstream
         *refuses* ssid/password ("SSID/PASSWORD auth type is not supported,
         return") and requires `device_name`. GMS 26.26.34 does the reverse:
         `OfflineFrame BANDWIDTH_UPGRADE_NEGOTIATION(UPGRADE_PATH_AVAILABLE|
         WIFI_DIRECT) missing ssid or not in correct format`. The proto says why:
         device-name on Android is "in the future".
         **So send every field**, as google does
         (`ForBwuWifiDirectPathAvailable(ssid, password, port, freq, ...,
         gateway, device_name, pin)`) — ssid/password for today's phone,
         device_name for tomorrow's. Stripping them produced silence.
      7. `supported_wifi_direct_auth_types=[]` — the phone advertises none, so it
         isn't doing auth-type negotiation at all.
      8. **`assoc key_mgmt 0x0` is a red herring.** It means no association
         happened, not a mismatch. Compare the phone's own working STA link:
         `assoc key_mgmt 0x400 network key_mgmt 0xc000d42` → associated. Group
         security is WPA2-PSK/WPS (analyser), so WPA3 is not involved.

      ### WHERE IT STOPS

      With legacy off, GO bit set, correct ssid/password/frequency/device_name,
      and `Intensive` discoverability: still `UpgradeFailure` at ~17s.
      `ConnectionRequested` has **never** fired. The revealing ordering:
      ```
      16:44:34.303  P2P-GROUP-FORMATION-FAILURE
      16:44:47.355  P2P-GROUP-FORMATION-FAILURE
      16:44:48.813  P2P-DEVICE-FOUND ... group_capab=0x8b   ← finds us only after
      ```
      The phone attempts formation *before* discovery lands, fails, then finds
      us, retries, fails. `Intensive` did not close that gap.

      ### UPDATE 2026-07-19 — early-start done, mechanism now understood

      Group startup was split from the offer (`ensure_wifi_direct_group`, called
      at `WaitingForUserConsent`; idempotent). Group now comes up ~1s before the
      offer instead of at accept. Tested every combination:
      legacy on/off × freq=5240 × early-start × full creds. **All fail identically.**

      logcat pinned the mechanism. The phone does a **FAST P2P client join**
      (`WifiP2pMetrics: startConnectionType:FAST, startGroupRole:CLIENT`,
      `network key_mgmt 0x2` = WPA2-PSK). It **never associates**:
      - `assoc key_mgmt 0x0` every attempt = supplicant selected no BSS.
      - No `Trying to associate` / `Associated with` ever logged.
      - `Nearby: Timed out waiting to connect to DIRECT-xxAARONPCxxxx` ×3, then
        `WifiDirectBandwidthUpgradeMedium failed to connect to the WiFi Direct ssid`.
      - **`ConnectionRequested` has never fired on our WinRT side** — the phone's
        P2P formation frames aren't reaching our GO at all.

      The phone finds us via P2P discovery (`P2P-DEVICE-FOUND name='AARONPC'`) as
      **two entries**: the device addr `70:..:b7` with `group_capab=0x88` (no GO
      bit) and the group BSSID `72:..:b6` with `group_capab=0x8b` (GO bit). But
      discovery ≠ association: it never moves to the operating channel and joins.

      **Leading hypothesis (a real wall, not a bug):** this phone's GMS (26.26.34)
      only does the FAST PSK path, which needs a joinable P2P group whose
      operating passphrase matches what we send. WinRT's autonomous GO exposes
      only the *legacy* passphrase (`LegacySettings.Passphrase`), which is a
      separate credential from the P2P group's, and it hosts joins via the
      device-name/WPS path — the path this phone does **not** yet support ("in
      the future" per the proto). Newer phones use device_name and would work;
      this one can't take what WinRT can give.

      ### RESOLVED 2026-07-19 — Google's own client does NOT upgrade here either

      Ran official **Google Quick Share for Windows**, same Pixel, same network.
      logcat, decisive line:
      ```
      NS_PAYLOAD BandWidthChanged(quality=3, connectionMedium=5,
        localStaFrequency=5240, remoteStaFrequency=5240, instantConnectionResult=2)
      ```
      `connectionMedium=5` = WIFI_LAN, `quality=3` = HIGH. **Zero P2P activity in
      the whole capture** — no p2p-wlan0, no P2P-GROUP, no formation attempt.
      Google connected over WIFI_LAN and stayed there by design.

      **Why: both devices are on the same WiFi (`localStaFrequency ==
      remoteStaFrequency == 5240`).** When the two devices share a fast LAN,
      Nearby does not upgrade to WiFi Direct — WIFI_LAN is already the
      high-bandwidth path. WiFi Direct is for when there is NO shared network.

      **So the entire WiFi Direct effort was tested in the one scenario where it
      is neither needed nor attempted, by us or by Google.** Our transfers were
      already running over WIFI_LAN the whole time. Not a bug in our GO code.

      Consequences:
      1. To actually exercise WiFi Direct (ours or Google's), the phone and PC
         must be on **different** networks: phone on cellular with WiFi off, PC on
         Ethernet, no common LAN. Only then is WIFI_LAN unavailable and WIFI_DIRECT
         attempted. **Never tested — every run had both on homenet-u.**
      2. Mirror Google: do not offer the WiFi Direct upgrade when both devices
         share a fast LAN. Keeping it behind `RQS_TRY_WIFI_DIRECT_UPGRADE` was
         right; a shipping build should gate on "no shared WIFI_LAN", not always.

      The GO-bit / device-name / early-start work stands and is correct for the
      off-LAN case; it just was never the thing blocking on-LAN transfers.

      **CONFIRMED 2026-07-19: with the phone off-WiFi (cellular only), our PC does
      not appear as a Quick Share target at all.** We advertise the receiver only
      over mDNS (WIFI_LAN); the BLE advertiser makes us *visible* but we have no
      BLE/Bluetooth *receiver* + initial connection channel (that's #425, which
      needs a GATT server, not implemented). So off-LAN there is no bootstrap
      channel over which a WiFi Direct upgrade could ever be negotiated.

      ### BOTTOM LINE — WiFi Direct has no reachable+useful scenario today

      - **Both devices share a LAN** (the normal case): Nearby uses WIFI_LAN and
        does not upgrade — proven with Google's own client. WiFi Direct is neither
        needed nor attempted. Transfers already work.
      - **No shared LAN**: WiFi Direct would matter, but the phone can't even
        discover us without a BLE/Bluetooth bootstrap we don't have.

      **The prerequisite for off-LAN transfers (and thus for WiFi Direct to ever
      run) is the #425 BLE receiver + initial Bluetooth channel, NOT more WiFi
      Direct debugging.** The WiFi Direct GO code is correct groundwork; it stays
      behind `RQS_TRY_WIFI_DIRECT_UPGRADE` until #425 provides a path that reaches
      it. Roadmap reordered accordingly: #425 first.

      ### (superseded) earlier "decisive next test" note

      Install **official Google Quick Share for Windows**, send from the same
      Pixel, and watch whether it achieves a WiFi Direct upgrade or also stays on
      WiFi-LAN. This settles whether the goal is even reachable with this phone:
      - Google's app also stays on WiFi-LAN → **true platform wall**, this phone
        + a WinRT GO can't do WiFi Direct, and it is not our bug. Stop here.
      - Google's app upgrades → capture ITS logcat (`P2P-*`, `assoc key_mgmt`) and
        diff against ours; something specific is still wrong on our side.

      ### NEXT LEADS (untried)

      - **Start the group earlier.** We create it at accept time, ~1s before the
        offer. google's `HandleInitializeUpgradedMediumForEndpoint` starts the GO
        and `StartAcceptingConnections` *before* building the frame, and their
        `ListenForService` deliberately waits for the IP. If the group needs to
        be discoverable for several seconds before the peer attempts formation,
        we are structurally too late. Try starting it at
        `WaitingForUserConsent`, or keeping one alive for the session.
      - **`GetConnectionEndpointPairs()` for the real IPs.** We still hardcode the
        ICS gateway. Only reachable once `ConnectionRequested` fires.
      - `os_info` now reports WINDOWS (was hardcoded LINUX). Verified safe:
        `bwu_manager.cc`'s WIFI_DIRECT guard reads `GetLocalOsInfo()` = the
        *phone's own* OS (`client_proxy.h` keeps `local_os_info_` separate from
        `GetRemoteOsInfo`). Did not change the outcome.

      **ANSWERED BY LOGCAT 2026-07-16 — read this before theorising again.**
      Five theories died reasoning from the Windows end (firmware, negotiation
      ordering, "nothing to upgrade to", frequency, band). `adb logcat` on the
      Pixel answered it in one capture. The phone's own account:
      ```
      WifiP2pMetrics: Start connection event, startConnectionType:FAST,
                      startGroupRole:CLIENT, startAttributionTag:nearby_connections
      WifiP2pService: Set P2P operating channel to 0, unsafe channels:
      WifiP2pMetrics: End connection event, endConnectivityLevelFailureCode:GROUP_REMOVED
      NearbyMediums:  MEDIUM_ERROR [NETWORK][WIFI_DIRECT][CONNECT][CONNECT_TO_NETWORK_FAILED][TIMEOUT]
      NearbyMediums:  Failed to remove WiFi Direct group — [2]BUSY
      NearbyConnections: WifiDirectBandwidthUpgradeMedium failed to connect to the
                         WiFi Direct ssid DIRECT-ICAARONPCOO6J for endpoint 7X1u
      ```
      What this settles:
      - **`Set P2P operating channel to 0`** = no channel constraint. The phone
        never uses our `frequency` to pick a channel. **The frequency field and
        the band/channel-concurrency theories are both dead.** `-1` was always
        fine. Don't reopen either.
      - **The phone joins as a P2P client** (`WifiP2pManager`,
        `startGroupRole:CLIENT`, `connectionType:FAST` = join by network name +
        passphrase, no discovery), on the p2p0 interface. **It does not do a
        legacy AP join.**
      - **The POC never tested the real path.** Joining `DIRECT-xxx` by hand from
        the phone's WiFi list is a *legacy* association. That worked, and it is
        why "PLATFORM PROVEN" below is only half true: the AP works; the P2P door
        was never knocked on.
      - `GROUP_REMOVED` after ~4.6s, retried once, same. `[2]BUSY` on cleanup is
        a consequence (no group ever formed), not a cause.

      **Live hypothesis (untested): nothing was answering the P2P door.** We set
      `IsAutonomousGroupOwnerEnabled` + `LegacySettings`, but never created a
      `WiFiDirectConnectionListener` / handled `ConnectionRequested` — the WinRT
      callback through which a Windows GO accepts an incoming *P2P* client. Added
      2026-07-16; resolving the peer via `WiFiDirectDevice::FromIdAsync(id)` is
      what completes the association (there is no explicit accept), and the
      returned device *is* the connection so it must be kept alive. Watch for
      `*** WiFi Direct: ConnectionRequested from ... ***` — if that never appears,
      the phone's association isn't reaching us at all and the problem is below
      WinRT.

      **STATUS: three field-level bugs found and fixed at the
      credential/negotiation layer. All three were real. NONE fixed the join.**
      The failure is a rock-steady **12 seconds** across every one of them, and
      no `phone connected from 192.168.137.x` line has ever appeared. That
      stability is the finding: a phone that rejects an offer at *negotiation*
      fails in ~1s (measured, with the NUL bug below). 12s is a radio-level
      association timeout — the phone accepts the offer, tries to join, and never
      associates. **Stop proposing fixes at the frame layer; nothing we say in
      those bytes will change this.**

      The one unread piece of evidence: `medium_metadata` says the phone is on
      `ap_frequency=5240` (5GHz, committed to its AP) and lists
      `wifi_direct_cli_usable_channels` spanning both bands. The deleted hotspot
      code reported `frequency=2437`, so Windows' soft-AP plausibly lands on
      2.4GHz. A single-radio phone would have to abandon its 5GHz AP to follow us
      to 2.4 — which looks exactly like a 12s timeout, and matches the observed
      "app running + quickshare enabled → phone's wifi drops". `start()` now
      reads the real channel via the Win32 WLAN API and logs every WLAN interface
      (`log_wlan_interfaces`); WinRT exposes no channel. **Awaiting that number.**
      If 2.4GHz: confirm with zero code by moving the phone to the router's
      2.4GHz band and retrying. If 5GHz: the band theory is dead.

      2. **`frequency` absent (fixed; did NOT cure the join).** The two credential
         messages are *not* symmetric, and this is easy to miss:
         ```proto
         message WifiHotspotCredentials { optional int32 frequency = 5 [default = -1]; }
         message WifiDirectCredentials  { optional int32 frequency = 4; }
         ```
         Only the Hotspot one declares a default. Leaving the WiFi Direct field
         unset does **not** mean "unknown" on the wire — proto2 reads it back as
         an implicit **0**, a frequency on no band. google's own
         `WifiDirectCredentials` class (`internal/platform/wifi_credential.h`)
         holds `int frequency_ = -1` and always writes the field, so a real peer
         never sends 0 and Android has no reason to treat it as unset. We now
         send `-1` ("unknown, scan for the SSID") explicitly.
         **Note:** `frequency: -1` was tried against *Hotspot* and changed
         nothing (see that entry) — but that proved only that Hotspot is
         unreachable at the guard, never that the field is inert. Don't let that
         result talk you out of this one.
         **Outcome: sending -1 explicitly changed nothing — still 12s.** Correct
         to send, but not the cure. We now send the *measured* frequency instead.
      3. **Advertised mediums were disjoint from the offer (fixed; did NOT cure
         the join).** `send_supported_mediums` replied `[WIFI_HOTSPOT, WIFI_LAN]`
         — advertising the medium whose code we *deleted*, and omitting the one
         we actually offer — then sent `UPGRADE_PATH_AVAILABLE(WIFI_DIRECT)` for
         something we'd never claimed. The phone announces
         `[WifiLan, WifiDirect, WifiAware, WifiHotspot, BleL2cap, Bluetooth, Ble, Nfc]`.
         Now `[WIFI_DIRECT, WIFI_LAN]`. Still 12s.

      **Diagnostic added:** the phone's `ConnectionRequest.medium_metadata`
      carries `ap_frequency` and `wifi_direct_cli_usable_channels` — the channels
      it can actually use as a WiFi Direct *client*, straight from the phone. We
      had never logged it. If `-1` doesn't fix the join, read that list before
      theorising: every wrong guess so far came from reasoning about our own
      radio instead of asking the peer.

      **Read the log with the file tools, not a shell** — a stale sandbox mount
      served a cached copy and cost a full debugging round on yesterday's run.

      **Next:** the swap (Increment B). `introduce_upgraded_channel()` now reads
      CLIENT_INTRODUCTION and answers CLIENT_INTRODUCTION_ACK (both plaintext,
      4-byte BE length prefix — google reads the introduction *before*
      `ReplaceChannelForEndpoint(..., enable_encryption)`, so nothing on the new
      socket is encrypted until the swap). Still to build:
      LAST_WRITE_TO_PRIOR_CHANNEL → await the phone's LAST_WRITE →
      SAFE_TO_CLOSE_PRIOR_CHANNEL → move `self.socket` onto the new TcpStream.
      **The UKEY2 context and both sequence counters carry straight across**
      (`bwu_manager.cc`: "using the same UKEY2 context for both the previous and
      new EndpointChannels... UKEY2 uses sequence numbers for writes and reads"),
      so keep `InnerState` and swap only the stream. That needs an
      `UpgradableStream` trait — `InboundRequest<S>` is generic and can't be
      handed a concrete `TcpStream` otherwise (tests run on `DuplexStream`).
- [ ] **Hotspot (soft-AP)** — **transport PROVEN, but unreachable by design from
      a WiFi-LAN connection. Code REMOVED 2026-07-17** (`hotspot_win.rs` +
      `offer_hotspot_upgrade`) once the source below settled it — carrying an
      unreachable path behind an `#[allow(dead_code)]` wasn't worth it. It's in
      git history, `wifi_direct_win.rs` inherits its gateway logic verbatim, and
      everything learned is recorded here. Resurrect only if #425/BLE lands and
      makes `isWifiLanConnected()` false. The proto (frame types 8-12,
      `WifiHotspotCredentials`, `BandwidthUpgradeRetryFrame`) is synced to current
      google/nearby and stays.

      **2026-07-16 — SETTLED FROM SOURCE. The earlier "Pixel firmware bug"
      diagnosis was wrong, and so were three later guesses.** `bwu_manager.cc` in
      google/nearby refuses a WIFI_HOTSPOT upgrade whenever the connection is
      already WIFI_LAN — on *both* sides, as the first branch of each function:

      ```cpp
      // ProcessBwuPathAvailableEvent (receiving an offer)
      if (channel_manager_->isWifiLanConnected() &&
          ((upgrade_medium == Medium::WIFI_HOTSPOT) || ...)) {
        LOG(INFO) << "... Don't do the BWU because this will destroy WIFI_LAN "
                     "which will lead BWU fail and other endpoint connection fail";
        RunUpgradeFailedProtocol(client, endpoint_id, upgrade_path_info);
        return;
      }
      // InitiateBwuForEndpoint (sending one) has the mirror check and returns.
      ```

      So Google's own implementation would never send the offer we send, and
      rejects it before reading a single credential. **No value of any field will
      ever work while the connection is WiFi-LAN.** That exactly matches the
      observed behaviour: `UPGRADE_FAILURE` inside one second, credentials echoed
      back untouched, identical with `frequency: -1` and `frequency: 2437`.

      **The transport itself is proven fine** — driven by hand, the phone
      associates with our AP, takes a DHCP lease (192.168.137.34), routes to the
      gateway, passes the firewall and reaches our listener on 8899 (arrived as a
      plain HTTP GET from Chrome). The phone also genuinely *wants* an upgrade for
      big payloads: with a 322 MB file it paused the transfer for 14s at
      `ack_bytes: 0` before giving up and falling back to WiFi-LAN (45 MB doesn't
      pause, so the threshold is between). Fallback is graceful — the 322 MB
      completed. Two earlier "failures" were our own confounds: no firewall rule
      on 8899, and a double-clicked Accept starting a second AP that invalidated
      the credentials we'd just sent.

      **Therefore, two ways forward, neither of which is this code as written:**
      1. **WiFi Direct** (see above) — the *same* guard deliberately does not
         block WIFI_DIRECT for a non-Windows client, so it is reachable from the
         WiFi-LAN connections we already have. This is the near path.
      2. **#425 / BLE** — a connection that didn't start on the LAN makes
         `isWifiLanConnected()` false and unblocks WIFI_HOTSPOT too. The far path.

      Groundwork saved either way: the soft-AP, the offer frame and the proto are
      done and proven. **When resuming:** (1) pick WiFi Direct or BLE per above;
      (2) build the channel-swap (Increment B: CLIENT_INTRODUCTION ack +
      LAST_WRITE/SAFE_TO_CLOSE + move the encrypted stream); (3) fix the hotspot
      lifetime and the accept guard below.

      **Two defects found while testing, both still open:**
      - `offer_hotspot_upgrade` does `std::mem::forget(hotspot)` to keep the AP
        up "for the duration of the transfer". Tethering is *system* state, so
        the AP survives the process: it stays on across app restarts until turned
        off in Settings. Nothing owns its lifetime. Fix: hold the handle in
        `InnerState` and drop it when the transfer ends or the connection dies.
      - `accept_transfer` has no guard against running twice. Two clicks started
        two soft-APs (the second invalidating the first's credentials) and
        collided on port 8899. The UI now debounces this, but the backend should
        not rely on the frontend for correctness.
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

- [x] **#425 — BLE receiver discoverability: WORKING (2026-07-20).** With WiFi
      **off** on the phone, the PC now appears in Quick Share's target list.

      **What it took — the three things that were each individually wrong:**
      1. **Wrong WinRT API.** `BluetoothLEAdvertisementPublisher` makes *beacons*
         (Microsoft's words). Real Quick Share receivers advertise
         `isPrivateGatt=true` - a connectable **GATT service**. Switched to
         `GattServiceProvider`. A beacon is never discovered as a ShareTarget no
         matter how correct its bytes are.
      2. **Must be connectable.** `IsConnectable=false` -> status 3 (Aborted); a
         GATT service that can't be connected to won't advertise at all.
         `IsConnectable=true` -> status 4 (StartedWithoutAllAdvertisementData):
         Windows puts the 0xFEF3 UUID on-air and *drops* our 26-byte service data
         (won't fit 31 bytes). **That's fine** - the UUID alone is enough for the
         phone to find and connect; the payload travels over GATT.
      3. **Serve the FULL advertisement over GATT, not the fast one.** The phone
         connects and reads the slot-0 characteristic
         `00000000-0000-3000-8000-000000000000`. We first served the *fast* form -
         it read fine but carries **no device name**, so the phone could never
         build a listable ShareTarget. Serving the non-fast full advertisement
         (service_id_hash + endpoint_info **with the name** + bt_mac + uwb +
         extra) made the PC appear.

      **The diagnostic that broke it open:** logging on *our* side when the GATT
      read is served (`*** served advertisement over GATT read ***`). Everything
      before that was inference from the phone's logcat, where rotating BLE random
      addresses make it impossible to tell our PC from a neighbour's phone - which
      caused one wrong "it works" call (a discovered `deviceType=1` endpoint was a
      neighbour, not us; see the 0x32 note). **Instrument your own side.**

- [x] **#425 phase 2 — transfer over BLE: WORKING, throughput-limited
      (2026-07-20).** A file sent from the phone with WiFi **off** is received,
      decrypted and written correctly. Discovery -> Weave handshake -> UKEY2 ->
      encrypted stream -> payload all run over the BLE socket, reusing the
      existing `InboundRequest` (which is generic over `AsyncRead + AsyncWrite`,
      so the whole Nearby stack rides a `tokio::io::duplex` unchanged).

      **The four bugs, each of which looked like the previous one:**
      1. **`handle()` services one frame and returns.** `TcpServer` wraps it in a
         loop; the bridge called it once, so after the ConnectionRequest the task
         ended, dropped its end of the duplex, and every later message was
         discarded by a `let _ = send(..)`. No error anywhere. Loop it, and never
         swallow a failed send.
      2. **Weave counter is shared by control *and* data packets, per direction.**
         The phone proves it: ConnectionRequest ctr 0, data 1-4, Error ctr 5. Our
         ConnectionConfirm took ctr 0 and the data sender restarted at 0, so we
         sent ctr 0 twice and got a Weave Error (`0xd2`).
      3. **WinRT dispatches `WriteRequested` on threadpool threads.** BLE delivers
         in order; our handlers do not run in order. A capture showed ctr=2
         (last fragment) reassembled before ctr=1 (first), which emitted a
         tail-only message and flushed ctr=1's head as a bogus "multiplex
         control" frame -> D2D sequence 103 -> 104 -> session dead mid-file.
         **Trust the counter, not arrival order:** park each packet in a slot and
         consume only the expected one.
      4. **`len` vs `buf.len()`.** The data branch moves `buf` into its slot; a
         later block still guarded on the *original* `len` and indexed `buf[0]`,
         panicking on every data packet. A panic in a WinRT callback can't unwind
         across the `extern "system"` boundary, so it aborts - and Windows
         reports that as `STATUS_STACK_BUFFER_OVERRUN`, i.e. "buffer overrun"
         with an empty log. Guard on the live length.

      **Where it stands: correctness is done, throughput is the blocker.**
      Steady **20.5 KB/s** received. The phone pushes **41 KB/s** (its own
      logcat: `Sent FILE data(2376328 bytes) via BLE used 55652 ms ... (41 KB/s)`)
      and reports `SUCCESS, 100%` when the bytes leave Nearby for its socket -
      which is why its progress bar runs ahead of ours and why a truncated file
      still looks "sent". Oversubscribed ~2:1, the phone's stack jams
      (`gatt_act_write() failed op_code=0x52 rt=143`, Android `143 =
      GATT_CONGESTED`, repeating throughout), our indications back up
      (`ind_count=11`), and an indication unconfirmed for 30 s obliges the stack
      to drop the ACL - the phone logs `REMOTE_USER_TERMINATED_CONNECTION`, i.e.
      **the PC killed the link**. Net effect: reliable under ~1.5 MB, fails above
      it. The phone's own `KeepAlive timeout(30000 ms)` line is logged *after*
      the disconnect and is a symptom, not the cause.

      **Not yet established:** whether the 20.5 KB/s ceiling is our receive path
      (~25 ms per 509-byte write, which would make it ours to fix) or the
      connection interval (the central's choice, which WinRT does not expose to
      a GATT server). ~25 ms/packet is suspiciously interval-shaped, but nobody
      has timed the write handler. **Measure before concluding.** Candidate
      fixes, in order: time the handler; drop `Indicate` from the server-tx
      properties so our sends need no confirmation and cannot trigger the ATT
      timeout that terminates the link (note: Notify-alone previously produced an
      empty delivery list, so verify rather than assume); reduce per-write work.
      Surviving slowly is worth more than dying fast - even at 20 KB/s a
      transfer that never gets terminated eventually completes.

      **Also learned:** per-packet logging on the receive path is not free. Three
      `info!` calls inside the WinRT callback, plus a per-chunk `info!` in
      `inbound.rs` and another Debug-formatting the whole `ChannelMessage` in
      `main.rs`, throttled us below the phone's send rate and cost a 1.9 MB photo
      40% of its bytes. All demoted to `trace`. Throughput accounting now lives
      in the pump task, off the callback thread.

      **The phone is asking for the upgrade.** `NearbySharing: timeout when
      waiting for high-quality medium` / `HIGH_QUALITY_MEDIUM_SETUP duration:
      68381, timeout: true` - it spends 68 s waiting for us to offer a faster
      medium and never gets one. This BLE channel is exactly where that
      negotiation belongs, and it makes the WiFi upgrade the other half of #425
      rather than a separate feature.

      **PHASE 2 (transfer over BLE) - socket is OPEN, data is flowing
      (2026-07-20).** With WiFi off the phone now connects and streams the real
      Nearby protocol to us. What it took:

      - Medium is **BLE** (`attempting to connect to endpoint X over mediums
        [BLE]`), not Bluetooth Classic or L2CAP. With WiFi off, availableMediums
        has no WIFI_LAN, so BLE is the initial channel.
      - Socket layer is **uWeave** over two GATT characteristics on our 0xFEF3
        service (`MultiplexBleSocketImpl`):
          client tx (phone -> us, write)  = 00000100-0004-1000-8000-001a11000101
          server tx (us -> phone, notify) = 00000100-0004-1000-8000-001a11000102
        Missing the first gives `missing client tx characteristic ...0101`.
      - **server tx must declare Notify AND Indicate.** With Notify alone
        `NotifyValueAsync` returned an *empty* result list - delivered to nobody -
        even though `SubscribedClients()` said 1. That empty list is the tell.
      - **Weave handshake:** phone writes ConnectionRequest
        `[0x80][ver_min u16][ver_max u16][max_packet u16]` (observed
        `80 00 01 00 01 01 fd` = v1..v1, 509 B). We must reply with
        ConnectionConfirm `[0x81][version u16][packet_size u16]` as a
        notification. Without it: `GATT_SWITCH_TO_DATA_TRANSFERRING_FAILED
        [TIMEOUT]`, then the phone sends control `[0x92]` (command 2 = Error).
      - Do **not** block on `.get()` inside a WinRT event handler - 0x8000000E
        (E_ILLEGAL_METHOD_CALL). A worker thread doesn't work either (IBuffer
        isn't Send). Use an `AsyncOperationCompletedHandler`.

      **Wire format once open** (first byte = Weave header, bit7 clear = data,
      bits 6-4 = packet counter, low nibble = first/last fragment flags):
      ```
      1c | 00 00 00 08 | 01 12 1f 0a 03 fc 9f 5e ...
      28 | fc 9f 5e | 00 00 02 ac | 08 01 12 a7 05 ...   (first fragment)
      34 | 36 ab 31 d3 ...                                (last fragment)
      ```
      Inside: `fc 9f 5e` = service_id_hash, then a **4-byte BE length + protobuf
      OfflineFrame** - the same framing the TCP path already parses. Note
      "Multiplex" in the class name and the packets whose payload starts
      `00 00 00 08` without the hash: there is a multiplex/control layer to work
      out alongside the Nearby frames.

      **MULTIPLEX LAYER DECODED (2026-07-20).** After Weave reassembly there are
      two kinds of message, told apart by the first 3 bytes:

      1. **`fc 9f 5e` = NearbySharing data.** Framing:
         `[service_id_hash(3)][u32 BE length][OfflineFrame protobuf]`
         **Strip the 3-byte hash and the rest is byte-identical to what the TCP
         path already parses** (`[u32 len][frame]`). Observed: a 681-byte frame
         containing "Aaron's Pixel 10 Pro" (ConnectionRequest) and a 136-byte
         frame with `AES_256_CBC-HMAC_SHA256` + 32-byte commitment + 64-byte key
         (UKEY2 ClientInit). So the phone runs the *normal* handshake over BLE.
      2. **`00 00 00` = multiplex control.** `[00 00 00][protobuf]`, no length
         field. e.g. `08 01 12 1f 0a 03 fc9f5e 10 02 1a 16 "<22-char id>"` -
         channel setup naming the service - and `08 02 1a 05 0a 03 fc9f5e`.

      **Bridge plan:** weave-reassembled message -> if it starts `fc 9f 5e`, drop
      those 3 bytes and feed `[u32 len][frame]` into a duplex stream -> hand the
      other half to `InboundRequest` (generic, unchanged). Outbound: take what
      `InboundRequest` writes, prepend `fc 9f 5e`, fragment to (packet_size-1)
      byte Weave data packets with header
      `(counter<<4) | (first<<3) | (last<<2)`, notify on server-tx.
      Note WinRT constraint: the GATT objects aren't `Send`, so the notify side
      must stay on the thread that owns the provider - use channels to reach it.

      **Remaining:** implement that bridge, answer the multiplex control frames,
      then Linux/bluer parity.
      **Correction:** an earlier "discovery worked" reading was a mis-attribution
      - the discovered endpoint (GZON, `deviceType=1` phone, "Christine's Pixel 9
      Pro") was *another phone* in the area advertising as a receiver, NOT our PC
      (`deviceType=3`). That also explains the "0x32" endpoint_info byte that
      matched no version of our code: `(0x32>>1)&7 = 1 = phone`. So our own
      advertisement has **never been confirmed discovered**; do not claim it is.

      State that IS solid:
      - Our advertiser is on-air: WinRT publisher reaches status 2 (Started),
        26-byte fast advertisement under 0xFEF3.
      - `nRF` is useless as a probe here (can't see even the known-good 0xFE2C
        wake beacon). The phone's logcat is the only working probe, and it only
        logs adverts it *successfully* parses.
      - **Google's own Windows Quick Share IS discovered by this phone with WiFi
        off** (deviceType=3 laptop, listed + transferable), so a Windows PC can be
        a BLE receiver. Our bytes/method still differ from Google's somehow.

      **SOLID NEGATIVE + prime suspect nailed (2026-07-20).** Clean test: our
      advertiser Started/on-air 18:02:42-18:05:37; phone scanned FOREGROUND
      low-latency in many bursts 18:03:33-18:04:57 (fully inside); **found nothing
      at all** - no `Found BleAdvertisement`. Windows says Started but isn't
      radiating it discoverably.
      **Why: 2 bytes over the legacy 31-byte limit.** Compare the wake beacon
      (`blea_win` 0xFE2C) that provably radiates (phone reacts to it):
        wake:  UUID(2) + payload(24) = 26  +AD(2) +flags(3) = 31  (fits exactly)
        ours:  UUID(2) + payload(26) = 28  +AD(2) +flags(3) = 33  (overflows by 2)
      Fix options, in order of cheapness:
        1. Suppress the flags AD (WinRT `BluetoothLEAdvertisement.Flags`; a
           non-connectable advert may not need it) -> 30 bytes, fits.
        2. Extended advertising + the *fast* payload (we only ever tried extended
           with the big non-fast advert). Phone scans extended (is-extended-advert
           =true), so this should be catchable.
        3. Trim 2+ bytes from the payload (deviates from Google's exact bytes;
           last resort).
      Google's advert is the same ~26 bytes and IS discovered, so Google must be
      doing (1) or (2) - worth confirming.

      **RESUME HERE (next session):** option 2 is now in `blea_recv_win.rs` -
      `SetUseExtendedAdvertisement(true)` + the fast payload under 0xFEF3. Build,
      run, and capture logcat while the phone scans FOREGROUND (send sheet open,
      WiFi off) for ~30s. Verdict test: grep the logcat for `Found BleAdvertisement`
      and specifically a **`deviceType=3`** discovery = us (a laptop). `deviceType=1`
      is a neighbour's phone - do NOT mistake it for us again (that was the 0x32
      mis-read). If extended still isn't found, the issue is Windows not radiating
      our custom service data discoverably at all; next compare exactly how Google's
      Windows app advertises (legacy+no-flags vs extended). Our advertiser reaching
      status 2 (Started) is necessary but NOT proof it's on-air - the phone finding
      it is the only proof.

      The FAST format itself is almost certainly right (byte-matched to Google's
      captured advertisement); the open question is Windows radiating it
      discoverably. Format in `ble_receiver.rs`:
        Layer 3 fast: `[0x23][endpoint_id(4)][info_len(1)][endpoint_info(17)]`
        Layer 2 fast: `[0x4A][data][device_token(2)]`  in 0xFEF3 service data.
      Later milestones once discovered: identity/name resolution (GATT server,
      `isPrivateGatt`/`rxAdvertisement`), then phase-2 transfer.
      **More valuable than it looked (2026-07-16):** a connection that didn't
      start on the LAN makes google/nearby's `isWifiLanConnected()` false, which
      is what unblocks the WIFI_HOTSPOT upgrade (see the Hotspot entry). It is
      *not* the only route though — WiFi Direct is reachable from a plain
      WiFi-LAN connection today, and needs no new discovery mechanism. Treat BLE
      as the strategic path (it also enables sharing with no shared network at
      all, which is the real feature) and WiFi Direct as the near one.
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
  fixes, send-text. **Caveat: Martichou appears inactive**, so these may have
  nowhere to land — see the mdns-sd note below.
- **mdns-sd: migrated off the fork (2026-07-16).** We were pinned to
  `Martichou/mdns-sd` **branch** `unsolicited`, which is a snapshot of upstream
  **0.10.4 (Feb 2024)**. Now on the published `mdns-sd = "0.20"` from
  `keepsimple1/mdns-sd` — the real crate; Martichou's is a fork *of* it, and
  keepsimple1 is active (0.20.1, Jun 2026).
  - **Why it mattered beyond tidiness:** mDNS parses untrusted multicast input,
    and we were missing two years of parser hardening — name-compression loop
    (#257), `read_u16` length checks (#234), RDATA length checks, and "sanity
    checks in DNS message decoding to prevent unnecessary panics" (#169). A
    branch pin is also a live supply-chain risk: it can move under us or vanish.
  - **It also deleted an ERROR for free.** `Failed to send to 224.0.0.251:5353
    via <192.168.137.1>` fired every transfer once WiFi Direct started bringing
    an interface up and tearing it down under mDNS. The fork error-logs *every*
    failed send; upstream refactored that path away and has exactly **one**
    `error!` in the whole daemon. Not suppressed — gone.
  - **What the fork actually added — and the real PR to send.** Not
    `AddrType`, not unsolicited *sending*: it was **`register_resend()`**, a
    public forced re-announcement, used when BLE sees a phone start discovery
    (Android misses our service if we registered before it started scanning).
    Upstream has the machinery (`Command::RegisterResend`,
    `send_unsolicited_response`) but never exposes it. We now call `register()`
    again instead, which upstream documents as the way to re-announce and which
    sends the unsolicited response immediately. **If that proves slower or
    noisier than a dedicated call, `register_resend` is a genuine upstream gap
    and worth a PR to keepsimple1** — he merges outside contributions readily.
  - API deltas handled: `enable_addr_auto(AddrType::V4)` →
    `enable_addr_auto()` + `set_interfaces(vec![IfKind::IPv4])` (per-service,
    strictly better than the fork's global enum); `ServiceResolved(ServiceInfo)`
    → `ServiceResolved(Box<ResolvedService>)` (auto-derefs, so call sites are
    unchanged); addresses are now `ScopedIp` → `.to_ip_addr()`.
  - **Needs a real send *and* receive test**, not just a compile: this is the
    discovery path that feeds the UI.
- Autostart: build/install the release, then enable Start on boot + Keep running
  on close + Start minimized.
