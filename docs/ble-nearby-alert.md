# The "nearby device is sharing" alert and the Bluetooth radio

When a phone nearby opens its Quick Share sheet, rquickshare can notify you, so
you can switch from Hidden to visible. `BleListener` (`core_lib/src/hdl/ble.rs`)
does the listening. This page records what that listening costs on Linux, how it
works now, and what we tried. The findings come from `btmon` captures on a
laptop with an Intel combo Wi-Fi/Bluetooth chip, BlueZ 5.85 and a Pixel phone.

## The problem

A Bluetooth mouse sometimes dropped, and sometimes couldn't reconnect, while
rquickshare was running. The listener used to keep a BlueZ discovery scan
running for the life of the app. BlueZ only offers active scanning through
discovery, and on a chip that shares one radio between Wi-Fi and Bluetooth, a
scan that never stops leaves too little airtime for other devices. While the
scan runs, the controller can't take a reconnection from a mouse either.

## What the phone sends

A sharing phone puts service data under UUID 0xFE2C, which is the Fast Pair
UUID, and starts that data with Quick Share's model ID, 0xFC128E:

```
1b 16 2c fe fc 12 8e 01 42 00 00 ...
```

The phone uses a new random address every few seconds while the sheet is open.

0xFE2C alone is not enough. Find My Device beacons and similar devices send it
all the time, for example:

```
11 16 2c fe 10 40 ...   (followed by an Eddystone 0xFEAA frame)
```

Matching on 0xFE2C alone raised the alert for one of these beacons, which was
in range for every test, including runs where no phone was sharing. The
listener now requires `fc 12 8e` after the UUID. The phone carries 0xFE2C only as service data, not in its service UUID
list, so BlueZ's UUID scan filter never matches it. The scan runs unfiltered and
the check happens in our code.

## How it listens now

1. **The user can turn it off.** Settings has "Notify me when a nearby device is
   sharing". When it's off, nothing listens. The change applies right away
   (`RQS::set_nearby_alerts`), and the app reads the stored value before it
   starts the core, so a user who turned it off never gets a scan at startup.
2. **On Linux, a passive advertisement monitor if BlueZ has one.** The listener
   registers a BlueZ `AdvertisementMonitor` that matches AD type 0x16 starting
   with `2c fe fc 12 8e`. The kernel serves it with a passive scan, and
   controllers with Microsoft's vendor extension (MSFT) do the matching in
   firmware. That passive scan is the same one the kernel uses to wait for a
   paired mouse, so mice and keyboards reconnect normally.
3. **On Windows, a passive advertisement watcher.** A
   `BluetoothLEAdvertisementWatcher` in passive mode, with a byte pattern on
   the same AD type 0x16 data. Its signal strength filter reports a phone gone
   with one last advertisement at -127 dBm.
4. **Otherwise, a continuous scan.** This is the case on Linux when BlueZ has
   no monitor, and on macOS. It's an active scan, so while it runs a mouse
   that drops may not reconnect. Turning the setting off is the way out. On
   Linux the scan is LE only, without BlueZ's default classic inquiry, and
   without duplicate reports, so BlueZ only signals a device when its data
   changes. Each device's service data is read once when the device is first
   heard and then followed through those change signals, which carry the new
   data. BlueZ also lists the devices it already knows when a scan starts,
   with the data it last heard from them, so a device without a signal
   strength waits for its next advert.

The phone changes address every few seconds, so the monitor and the watcher
report a stream of short-lived devices for one share. The alert goes out when
the first one is found, and not again until all of them are lost, which
happens 10 s after the phone's last advertisement. After bluetoothd restarts,
BlueZ can remove a device before reporting it lost and then drops the report,
so the monitor also counts an address as lost when BlueZ removes the device.
The Linux scan counts an
address as lost when its service data changes to something else, or when
BlueZ removes the device, 30 s after its last advertisement. The macOS scan has no lost event, so it treats the share as
over after 10 s with no sharing advertisement.

If Bluetooth is turned off, the listener stops and starts again once
Bluetooth is back on: the adapter is powered on or reappears after bluetoothd
restarts on Linux, the radio turns on on Windows, or the adapter reports it's
on on macOS. Turning the setting off and on also starts it again. If BlueZ
drops the advertisement monitor, the listener registers it again once
bluetoothd is back, rather than falling back to the scan. There's no timer: if
listening fails with Bluetooth on, it waits for Bluetooth to go off and on
again, or for the setting to change. The receiver advertisement works the same
way: if resuming it fails, it tries again once Bluetooth turns on.

The listener does a second job: while you're visible, each alert makes the
mDNS service re-announce at once, so the sharing phone lists this device right
away. With the setting off, the phone has to wait for the periodic re-announce,
which runs every minute.

## Why the monitor isn't on by default

BlueZ only offers `AdvertisementMonitorManager1` with `Experimental = true` in
`/etc/bluetooth/main.conf`. `RegisterMonitor` is an experimental D-Bus method
(`src/adv_monitor.c:1412` in BlueZ 5.85). In 5.85 `Experimental` is a single
true/false switch, so you can't turn on the monitor alone.

The same switch turns on the LE Audio profiles, including the media control
client `mcp` (`profiles/audio/mcp.c:396`, `.experimental = true`). That has a
side effect when a phone connects to rquickshare's Bluetooth receiver
advertisement (see [ble-receiver-discovery.md](ble-receiver-discovery.md)):

1. The phone connects, with the laptop as the peripheral.
2. bluetoothd reads the phone's GATT services. `mcp` attaches to the phone's
   Generic Media Control service and reads it.
3. The phone answers Insufficient Authentication.
4. BlueZ raises the link security and sends an SMP Security Request (bonding,
   MITM, SC).
5. The phone shows a pairing request.

`mcp` doesn't check who started the connection or whether the device is bonded,
and BlueZ's automatic security elevation has no off switch in 5.85. Commits to
`mcp.c` after 5.85 don't change this.

So we don't ask anyone to turn experimental mode on. Systems that already run
it, for LE Audio for example, get the passive monitor automatically and already
see the pairing request when a phone connects to the receiver advertisement.
Declining the request is harmless.

## Ways to stop the pairing request

All of these need root and a bluetoothd restart. None of them is something
rquickshare can do over D-Bus.

| Option | Verified | What you lose |
| --- | --- | --- |
| Start bluetoothd with `-P mcp` (systemd drop-in) | Tested, the request stopped | Showing or controlling the phone's media over LE Audio |
| `ReverseServiceDiscovery = false` in `[General]` | Source only (`src/device.c:6149`) | bluetoothd stops reading the services of any device that connects to the laptop on its own. Connections the laptop starts are unaffected. |
| `Client = false` in `[GATT]` | Source only (`src/device.c:6154`) | Same as `ReverseServiceDiscovery = false` for LE |
| `[GATT] Security` | Only on BlueZ master, needs `Testing = true` | Turns off automatic security on every link. Not worth it. |

Things that don't work:

- Blocking the phone (`Device1.Blocked`) also stops it connecting to the
  receiver advertisement.
- The admin policy allowlist (`AdminPolicySet1.SetServiceAllowList`) is
  callable without root, but BlueZ applies it only after service discovery
  finishes, so a phone's first connection still prompts. Phones change address
  often, so most connections count as first ones. It also applies to every
  device on the machine.
- `Pairable = false` on the adapter. The kernel still sends the Security
  Request.
- Advertisement properties and the GATT application we register have no effect
  on whether bluetoothd reads the phone's services.

The real fix belongs in BlueZ: `mcp` could skip devices that the laptop didn't
connect to itself.

## Checking it yourself

Run `sudo btmon -T > /tmp/btmon.txt` and start rquickshare with
`RUST_LOG=trace`.

- The log says which mode is active: `listening with a passive advertisement
  monitor` or `listening with a continuous scan`. With the alert on, each
  address found or lost shows up as `monitor found <address>` or `monitor lost
  <address>`.
- `grep -E "Type: (Active|Passive)" /tmp/btmon.txt | sort | uniq -c` shows the
  scan types. With the monitor there should only be passive ones.
- `grep "4d 53 46 54 02" /tmp/btmon.txt` lists the controller's MSFT found and
  lost events. The last byte is 01 for found and 00 for lost.
- `grep -E "SMP: Security Request|Insufficient Authentication" /tmp/btmon.txt`
  shows pairing attempts.
