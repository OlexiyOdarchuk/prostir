# Prostir

**Extra KDE Plasma monitors floating around you in a Meta Quest 3.**

[Українська версія](README.uk.md)

Prostir (Ukrainian *простір*, "space") turns a Quest headset into a multi-monitor
setup for a KDE Plasma laptop or desktop. Every monitor you see in the headset is a
real output in KWin: it shows up in System Settings → Display, windows can be
moved and maximized onto it, the Plasma panel can live there, and the mouse crosses
from one screen to the next exactly where the screens meet in front of you.

<!-- Screenshot: put a headset capture in docs/ and link it here. -->

## Features

- **Real virtual monitors** created by KWin — any number up to six, any size
  (1080p, 1440p, 4K, ultrawide, portrait), added and removed from the headset.
- **Sharp text**: frames are decoded straight into the Quest compositor's own layers
  (OpenXR `XR_KHR_android_surface_swapchain`) with compositor supersampling — no
  intermediate 2D window.
- **Low latency**: hardware HEVC encoding on the PC (VA-API, Intel/AMD), hardware
  decoding on the Quest. ~14 ms from encoded frame to shown on Wi-Fi in testing.
- **Layouts** that match KDE: arc, grid, column, overhead (for lying down), monitors
  meeting edge to edge like one big screen; drag a monitor to another place in the
  row and KDE's arrangement follows.
- **Curved monitors** (gentle or strong), with seams that still line up.
- **Laptop screen mirror** under the main monitor, tilted towards you — so you never
  need to take the headset off. Disappears when the lid closes.
- **Backgrounds**: black, your room (passthrough), four built-in panoramas or your
  own picture.
- **Hands**: see your tracked hands (e.g. over the keyboard), optionally point,
  pinch to click and pinch-and-hold to drag.
- **Tray icon** on the PC: connection status, video quality, start with the session.
- Works over **Wi-Fi** (automatic discovery) or **USB**.

## Status

Version 0.1 — early, but used daily by its author. Tested with KDE Plasma 6.8
(Wayland) on Intel Arc graphics and a Meta Quest 3.

## Requirements

**PC**

- KDE Plasma 6 on Wayland (KWin provides the virtual outputs).
- PipeWire and GStreamer ≥ 1.22 with the `va` plugin (gst-plugins-bad) and the
  PipeWire GStreamer plugin.
- A GPU with a VA-API HEVC or H.264 encoder (Intel, AMD). Without one, run with
  `--software` (x264 on the CPU).
- `kscreen-doctor` (part of Plasma). Optional: `qdbus6` for matching wallpapers,
  `adb` for USB.

| Distribution | Packages |
|---|---|
| Arch | `gstreamer gst-plugins-bad gst-plugin-pipewire` + `intel-media-driver` (Intel) or `libva-mesa-driver` (AMD) |
| Fedora | `gstreamer1 gstreamer1-plugins-bad-free pipewire-gstreamer` + VA driver |
| Debian/Ubuntu | `gstreamer1.0-plugins-bad gstreamer1.0-pipewire` + VA driver |

**Headset**

- Meta Quest 3 or 3S (Quest 2 and Pro should work, untested).
- Developer mode enabled (in the Meta Horizon phone app: Devices → Headset settings →
  Developer mode), to install the app with `adb`.

## Install

### From a release

1. Download `prostir-<version>-x86_64-linux.tar.gz` and `prostir-<version>.apk` from
   the [releases](../../releases).
2. PC:
   ```sh
   tar xf prostir-*-x86_64-linux.tar.gz && cd prostir-*/
   ./install.sh             # into ~/.local, no root; add --autostart to start with Plasma
   ```
3. Headset (USB cable, developer mode on):
   ```sh
   adb install prostir-*.apk
   ```

### From source

```sh
# PC part
cargo build --release -p prostir-host
./scripts/install.sh

# Quest part: Android SDK with NDK and build-tools, plus the Rust target
rustup target add aarch64-linux-android
./scripts/build-apk.sh --install   # builds build/prostir.apk and installs it via adb
```

## Use

1. Start **Prostir** on the PC (application menu, or `prostir`). An icon appears in
   the system tray.
2. On the headset open **Prostir** (Library → Unknown sources).
3. The headset finds the PC by itself: over USB if the cable is plugged in,
   otherwise over Wi-Fi. With several PCs around, pick one in the menu — or type
   the IP on the keypad.
4. The monitors appear in front of you. Press **A** to line them up.

The monitors stay in KDE for 30 s after the headset disconnects (`--linger`), then
windows move back to the laptop screen.

### Controls

| | Controller | Hands (menu → Hands → "and control") |
|---|---|---|
| Open the menu | ☰ or **X** on the left controller, or the round ☰ button under the main monitor | pinch the ☰ button |
| Click | trigger | pinch |
| Move a monitor or the menu | hold **grip** and point | pinch and hold for ~0.3 s |
| Nearer / farther | thumbstick up/down while holding | reach out / pull your hand in |
| Bigger / smaller | thumbstick left/right while holding | — |
| Line monitors up | **A** | menu → Layout |
| Recenter | hold the Meta button (system) | system gesture |

### Menu

- **Monitors**: add (1920×1080, 2560×1440, 3840×2160, 3440×1440, 1440×2560), remove,
  make primary (gets the Plasma panel), move left/right in the row, show or hide the
  laptop screen.
- **Layout**: arc, grid, column, overhead. **Curve**: flat, gentle, strong.
- **Background**: black, room (passthrough), night, sunset, space, aurora, or your own
  photo — copy an equirectangular `background.jpg`/`.png` to
  `/sdcard/Android/data/dev.prostir.client/files/`.
- **Hands**: hidden, shown, or shown and used for pointing.

### Tray (PC)

Connection status, number of monitors, **video quality** (15/30/50 Mbit/s per monitor,
applied immediately), **start with the session**, quit.

### Command line

```
prostir [--monitor WxH[@scale]]... [--codec h265|h264] [--bitrate MBPS] [--software]
        [--keep-primary] [--linger SECONDS] [--port PORT] [--no-discovery] [--no-adb]
```

Without `--monitor` the set from the last session is used. Settings live in
`~/.config/prostir/monitors.txt`.

## Performance and battery

Measured with three 2560×1440 monitors and the laptop mirror:

- **PC**: about 2 % of one CPU core and 250 MB of RAM. Encoding runs on the GPU's
  video engine, and only when a screen changes.
- **Network**: up to the chosen bitrate per monitor while it changes, almost nothing
  when it is still.
- **Headset**: each monitor costs one hardware decoder (idle when the screen is still)
  and one compositor layer drawn every frame. Fewer or smaller monitors, the black
  background and hidden hands save the most battery. The room background keeps the
  passthrough cameras running.

## Security

The connection is **neither authenticated nor encrypted**: anyone on the same network
can connect and see the virtual monitors. Use Prostir on a trusted network (or USB).
Ports: TCP 47830 (stream), UDP 47831 (discovery broadcast).

## Troubleshooting

- **Headset log**: `adb logcat -s prostir`
- **PC log**: run `prostir` in a terminal; `RUST_LOG=debug prostir` for details,
  including the GStreamer pipeline and encoder timings.
- `prostir-probe [HOST:PORT] [SECONDS] [DIR]` connects like the headset and dumps
  the streams — handy to check the PC side without a headset.
- **Not found over Wi-Fi**: PC and headset must be on the same network without client
  isolation (guest networks often isolate); try the keypad with the PC's IP, or USB.
- **Screenshots in 16:9 are black**: the Quest's landscape capture leaves out the
  compositor layers Prostir uses; switch captures to square in the Quest camera
  settings.

## How it works

```
KWin: zkde_screencast stream_virtual_output  →  PipeWire  →  VA-API HEVC encoder
                                                                    │ TCP (Wi-Fi / USB)
Quest: MediaCodec (hardware) → Android Surface owned by the compositor
       → OpenXR quad/cylinder layer (+ menu, cursor, hands and panorama layers)
```

The PC part (`host/`) also lays the outputs out in KDE with `kscreen-doctor`, keeps the
wallpapers in sync through Plasma's scripting API and mirrors the physical display with
`stream_output`. The headset part (`client/`) is a native OpenXR app written in Rust;
the only thing it renders itself is the hands overlay. See `proto/` for the wire format.

## Similar projects

- [qwayland](https://github.com/jboero/qwayland) — the same idea with 2D Horizon OS
  panels and NVIDIA encoding.
- [WiVRn](https://github.com/WiVRn/WiVRn) + [wlx-overlay-s](https://github.com/galister/wlx-overlay-s)
  — general OpenXR streaming with desktop overlays.
- [Breezy Desktop](https://github.com/wheaney/breezy-desktop) — virtual displays for
  AR glasses on KDE and GNOME.
- Immersed — proprietary, cross-platform.

## Development

```
proto/    wire protocol (shared)
host/     PC: virtual outputs, encoders, tray, discovery
client/   Quest: OpenXR app (app.rs main loop, ui.rs menu, layout.rs geometry, …)
scripts/  build-apk.sh, install.sh, package-release.sh
```

`cargo test` runs the protocol, layout and menu tests on the PC.
`cargo run -p prostir-client --example panel_preview -- menu.png` renders the headset
menu to a PNG; `--example sky_preview -- DIR` renders the panoramas.

## License

[GPL-3.0-or-later](LICENSE). The bundled Noto Sans font is under the
[SIL Open Font License 1.1](client/assets/OFL-NotoSans.txt).

Meta and Meta Quest are trademarks of Meta Platforms, Inc. KDE and Plasma are
trademarks of KDE e.V. Prostir is an independent project, not affiliated with or
endorsed by either.
