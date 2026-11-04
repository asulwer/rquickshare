# Known issues (BLE / bandwidth-upgrade path)

Two rare, phone-side, retry-recoverable failures observed during PC↔Pixel
testing. Neither is reproducible on demand, and neither appeared in the last
retained session log (07-24, 18:32–19:32Z: 7 PC→phone sends, 6 successes, the
one non-success being the WIFI_LAN upgrade stall since fixed). Both are deferred
until they resurface *with logcat already capturing* — they can't be root-caused
from our side's log alone.

## A. Outbound first-connection handshake flake

The first PC→phone BLE send after launching the app occasionally fails during
UKEY2, then succeeds on retry.

- Our side: `Handling State::SentUkeyClientInit frame` followed by
  `peer closed the Weave connection` / an EOF reading the UKEY2 server init.
- Phone side (when captured): the Nearby Weave socket errors during UKEY2,
  BLE GATT `status 133`, `onDisconnected`.
- Recovery: the next attempt connects and transfers normally.

To root-cause, capture phone logcat *before* launching and send PC→phone
immediately, then line up the `NearbyConnections` UKEY2 lines
(`startClient`, Weave socket error, `status 133`, `EOFException`,
`onDisconnected`) against our `SentUkeyClientInit` timestamp.

## B. Role confusion after a transfer

The phone occasionally holds a stale *receiver* role, so a subsequent
connection comes in as the wrong role.

- Our side: a Response frame arrives where an introduction is expected; we
  detect it (`peer connected as a receiver …`), disconnect, and fail cleanly so
  the transfer can be retried.
- Suspected trigger: the Quick Share extension making the phone drop and
  reconnect WiFi mid-session, leaving its medium-selection / role state stale.

To root-cause, capture phone logcat spanning the end of one transfer and the
start of the next, and check the `NearbySharing` role / advertising state
between them.

## C. The phone drops WiFi while its Quick Share send sheet is open

This is why the BLE receiver exists; see
[ble-receiver-discovery.md](docs/ble-receiver-discovery.md). On Pixels with
AirDrop compatibility, opening the send sheet starts
`com.google.android.mosey`, which runs its own 5 GHz link. If the phone's WiFi
is also on 5 GHz, Mosey disconnects it until the sheet closes:

```
Mosey.A: Disconnect Wi-Fi because STA and mosey are running on the same band
```

It is a band check, not a channel check, so a phone on a 2.4 GHz or 6 GHz
network keeps its WiFi and finds us over mDNS as usual.

## D. Linux: phones ask to pair and can't find us over Bluetooth

With `Experimental = true` in `/etc/bluetooth/main.conf` (or bluetoothd started
with `-E`), BlueZ loads its LE Audio media control client, `mcp`. When a phone
connects to read our receiver advertisement, `mcp` reads the phone's Media
Player Name, gets Insufficient Authentication, and BlueZ asks the phone to
pair. The phone declines and drops the link before reading the advertisement,
then retries while the send sheet is open. With its WiFi dropped (C), it never
lists us. The side menu shows a note when experimental mode is on.

[ble-nearby-alert.md](docs/ble-nearby-alert.md#ways-to-stop-the-pairing-request)
has the details. Fixes, in order:

- Start bluetoothd with `-P mcp`, with a systemd drop-in. You lose media
  control of the phone over LE Audio.
- Or pair the phone with this computer once, from the phone's Bluetooth
  settings with the send sheet closed. That only fixes that phone.
- Or set `Client = false` under `[GATT]`. That stops BlueZ reading the
  services of any device that connects to it, which breaks BLE mice and
  keyboards.

## Capturing phone logcat

```powershell
$adb = "C:\Program Files (x86)\Android\android-sdk\platform-tools\adb.exe"
& $adb logcat -c
& $adb logcat -v time > "$env:LOCALAPPDATA\dev.mandre.rquickshare\logs\logcat.txt"
```

This drops `logcat.txt` next to `rquickshare.log`, so both sides of a failure
line up by timestamp.
