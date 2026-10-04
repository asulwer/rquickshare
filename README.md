<div align="center">
  <h1>rquickshare</h1>

  <p>
    <strong>Google Quick Share (formerly Nearby Share) for Windows 11, Linux & macOS</strong>
  </p>
  <p>

[![Check](https://github.com/asulwer/rquickshare/actions/workflows/check.yml/badge.svg)](https://github.com/asulwer/rquickshare/actions/workflows/check.yml)
[![Build](https://github.com/asulwer/rquickshare/actions/workflows/build.yml/badge.svg)](https://github.com/asulwer/rquickshare/actions/workflows/build.yml)

  </p>
</div>

![demo image](.github/demo.png)

Send and receive files, text and links between an Android phone and your computer, using the same Quick Share your phone already has.

This is a fork of [Martichou/rquickshare](https://github.com/Martichou/rquickshare) that adds Windows support and transfers that work without a shared Wi-Fi network (see [How devices connect](#how-devices-connect)).

Installation
--------------------------

There are no published releases of this fork yet, so for now you build it from source - see [BUILD.md](BUILD.md). The packages below are produced by that build.

**Important notes:**
- The minimum GLIBC version supported is included in the Linux package name.
  - You can check yours with `ldd --version`.

#### Windows 11

Run the installer (`.msi` or `-setup.exe`), or run `rquickshare.exe` directly.

- Bluetooth is needed for transfers with a phone that isn't on your Wi-Fi network.
- Windows Firewall will ask whether to allow rquickshare the first time it runs. Allow it on **Private** networks, otherwise your phone can't reach it over Wi-Fi.

#### macOS

Simply install the .dmg.

Note that you may have to first allow the app to install under `Settings > Privacy & Security > Security` (you should see a dialog asking for permission).

#### Linux

##### Install dependencies

RQuickShare requires one of the following libraries to be installed:

- `libayatana-appindicator`
- `libappindicator3`

The packages should install those dependencies by themselves, but if this is not the case you may have to install them manually.

##### Debian / Ubuntu
```bash
sudo dpkg -i r-quick-share_${VERSION}.deb
```

##### DNF (preferred over RPM)
```bash
sudo dnf install r-quick-share-${VERSION}.rpm
```

##### RPM
```bash
sudo rpm -i r-quick-share-${VERSION}.rpm
```

##### AppImage (no root required)

There's no installation needed, you simply have to make it executable and run it:

```bash
chmod +x r-quick-share_${VERSION}.AppImage
./r-quick-share_${VERSION}.AppImage
```

---

<details>
<summary>Packages of the original project</summary>

These install [Martichou/rquickshare](https://github.com/Martichou/rquickshare), not this fork, so they don't include the Windows and Bluetooth work.

#### AUR (Arch)

```bash
yay -S r-quick-share
```

#### Nix

Available here: [NixOS](https://search.nixos.org/packages?show=rquickshare&query=rquickshare)

```bash
nix-shell -p rquickshare
```
</details>

---

How devices connect
--------------------------

- **Same Wi-Fi network:** devices find each other over mDNS and transfer directly over the network. This is the fastest path and works on every platform.
- **No shared Wi-Fi (Windows):** the transfer starts over Bluetooth LE, then moves to Wi-Fi - your local network if the phone can reach it, otherwise a hotspot rquickshare brings up on your PC (Windows Mobile Hotspot). Your PC needs Bluetooth, and a Wi-Fi adapter for the hotspot.
- **Bluetooth on Linux** is newer: receiving from a phone over Bluetooth works, and the transfer moves to Wi-Fi when both devices are on the same network.

Bluetooth support is still being refined; see [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for the problems we know about.

FAQ
--------------------------

### My Android device doesn't see my computer

- Make sure rquickshare's visibility (top of the side menu) isn't **Hidden from everyone**.
- On the same Wi-Fi network, mDNS has to be allowed on that network; public networks (coffee shops, airports, etc.) often block it. On Windows, also check the firewall note above.
- Without a shared network, your computer needs Bluetooth turned on.

### My computer doesn't see my Android device

Android doesn't advertise itself all the time, even in "Everyone" mode. A few ways around that:

- **Scan the QR code.** When you pick something to send, rquickshare shows a QR code. Scanning it with your phone makes the phone show itself to rquickshare, even when it isn't set to "Everyone".
- **Bluetooth.** On Windows and Linux, rquickshare broadcasts a Bluetooth advertisement that makes nearby Android devices reveal themselves, if your computer has Bluetooth.
- **Open the receive screen on the phone.** In the "[Files](https://play.google.com/store/apps/details?id=com.google.android.apps.nbu.files)" app, open the "Nearby Share" tab. If that isn't there, use a shortcut maker (see [here](https://xdaforums.com/t/how-to-manually-create-a-homescreen-shortcut-to-a-known-unique-android-activity.4336833)) to create a shortcut to either:
  - Activity: `com.google.android.gms.nearby.sharing.ReceiveSurfaceActivity`
  - Action: `com.google.android.gms.RECEIVE_NEARBY` with Mime type `*/*`

_Note: Samsung changed Quick Share on its devices, so the shortcut workaround may not work there._

### When sharing a file, my phone appears and disappears "randomly"

This is normal when your computer is relying on Bluetooth to find the phone: Android periodically withdraws its advertisement and only reappears when it sees the Bluetooth message again.

### How do I send text or a link?

Copy the text (or URL) to your clipboard, then press `Ctrl+V` (or `Cmd+V`) inside the rquickshare window and pick the target device. URLs are sent so the receiving Android device can open them in a browser. You can also enable **Auto-stage clipboard text** in Settings to have newly copied text staged for sending automatically.

Text, links and Wi-Fi networks sent *to* your computer show up as a notification, with a **Copy** button (and **Open**, for links).

### Can I send a folder?

No - Quick Share only sends individual files. Dropping a folder on the window shows a message asking you to send the files inside it instead.

### How do I change the name other devices see?

Open **Settings** and set **Device name**. Leave it empty to use your computer's name.

### Once I close the window, the app is still running

That's by default: closing the window keeps rquickshare running in the system tray so it can still receive. To quit when the window closes, turn off **Keep running on close** in Settings.

On Linux, if your desktop has no system tray, you can check whether it's still running with:

```bash
ps aux | grep r-quick-share
```

### My firewall is blocking the connection

You can give rquickshare a fixed port to allow in your firewall. Close the app, then add a `"port"` entry to its settings file:

- **Windows:** `%APPDATA%\dev.mandre.rquickshare\.settings.json`
- **Linux:** `~/.local/share/dev.mandre.rquickshare/.settings.json`
- **macOS:** `~/Library/Application Support/dev.mandre.rquickshare/.settings.json`

> [!WARNING]
>
> The json must stay valid after your modification; for example, if "port" is the last item of the JSON it must not have a comma after it, otherwise the config will be reset.

```json
{
	...existing_config...,
	"port": 12345
}
```

By default the port is random (the OS will decide).

### Where are the logs?

- **Windows:** `%LOCALAPPDATA%\dev.mandre.rquickshare\logs`
- **Linux:** `~/.local/share/dev.mandre.rquickshare/logs`
- **macOS:** `~/Library/Logs/dev.mandre.rquickshare`

To capture a problem, set **Logging level** to `trace` in Settings, reproduce it, and attach the log to an issue.

### The app opens but I just get a blank window or cannot run it (Linux)

This happens for some users running Linux + NVIDIA cards. Start rquickshare with:

```bash
env WEBKIT_DISABLE_COMPOSITING_MODE=1 rquickshare
```

Status
--------------------------

`rquickshare` is still in development. Windows 11 and Linux are the main targets; macOS should work but is less tested. Expect the design to change between versions.

Got feedback or suggestions? Feel free to open an issue.

Credits
--------------------------

This project wouldn't exist without these amazing open-source projects:

- https://github.com/Martichou/rquickshare (the original project this fork builds on)
- https://github.com/grishka/NearDrop
- https://github.com/vicr123/QNearbyShare

Contributing
--------------------------

Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change. See [BUILD.md](BUILD.md) to get a development build running; pull requests must pass the checks in `.github/workflows/check.yml` (rustfmt, cargo test, lint, type-check).
