# Changelog

## 0.1.2 (2026-11-19)

A fix for the headset app. The PC part is the same as in 0.1.1.

### Monitors and windows

- Tapping a monitor on the map no longer closes the app when monitors overlap or
  stand far apart.

## 0.1.1 (2026-11-19)

Fixes for things that came up in the first days of use. Update both the PC part and
the headset app.

### Monitors and windows

- Take the headset off and put it on again, or let it reconnect after a short drop,
  and the monitors show their picture straight away. On KDE the picture used to stay
  frozen, or the monitors did not come back until the headset app was restarted.
- Windows that cross from one monitor to the next continue where they do in the
  headset. The desktop is now arranged from where the monitors' edges meet in space,
  so it no longer depends on where your head was when the arrangement was centred.
- Line up (the A button) stands every monitor upright again and joins the laptop
  screen after the monitors, so a monitor never ends up hanging off the laptop screen
  instead of beside the main one.
- Moving all monitors together keeps the arrangement in one piece while it moves.
- The map in the Monitors tab shows the monitors as the desktop has them, each at its
  own size.
- Cinema mode no longer loses your arrangement: opening the Meta menu, resizing with
  both hands, follow mode or a profile switch while in cinema used to save the cinema
  screen over it. Started in cinema mode, the app shows the cinema screen.
- After the PC part restarts, the desktop is arranged the way the monitors stood in
  the headset straight away, above and below included.
- A monitor that comes back with another resolution (a profile, a rotated monitor)
  keeps the same size of text as its neighbours, instead of the width the old
  resolution had. After a restart, "left", "middle" and "right" in the menu match
  where the monitors stand.

### In the headset

- Monitors stay visible under the Meta system menu instead of going dark.
- The app no longer closes when you come back to the headset, and it connects again
  after Horizon OS restarts it.
- With hands: a quick pinch on a menu button presses it when you let go; pinch and
  hold takes the menu, on a button too. Something taken within reach follows your
  hand, so you can pull it closer.

### PC

- A codec or encoder switch that fails leaves every monitor working with the previous
  settings, instead of a broken set that could later crash the PC part.
- Reconnecting starts each monitor's picture over once, not twice.
- Monitors given with `-m` no longer reset the sound and microphone choices, and
  quality changes in the tray are kept when no headset is connected.
- A hung `kscreen-doctor`, `qdbus`, `pactl` or screenshot tool can no longer hang
  Prostir.
- The PC part is split into a shared core and a Linux layer, the groundwork for
  GNOME and other systems. Nothing changes in use.
- The AUR package is published automatically, and release builds retry downloading
  the OpenXR loader.

## 0.1.0 (2026-11-18)

First public version.

### Monitors

- Virtual monitors that are real outputs of the desktop: KWin virtual outputs on KDE
  Plasma 6, headless outputs captured with wlr-screencopy straight into GPU buffers on
  Hyprland. Windows, panels and the mouse treat them like plugged-in screens.
- Up to six, at 1920×1080, 2560×1440, 3840×2160, 3440×1440 or 1440×2560, added,
  removed and made primary from the headset, on a map of the monitors as seen from
  where you sit.
- Shown as OpenXR compositor layers, flat or curved, so text stays sharp.
- Layouts: Around me, Wall and Lying down. Move, turn, bring nearer and resize any
  monitor with a controller or your hands, one at a time or all together.
- Line up (the A button, the Monitors tab or the palm menu) closes the seams in your
  own arrangement: neighbours join edge to edge, curving around you at the sides and
  tilting towards you above or below. Monitors put clearly apart stay.
- Stick together: a monitor let go almost against another joins it edge to edge; the
  edge lights up while you drag.
- The desktop is arranged the way you see it, so the mouse crosses where the monitors
  meet in the headset, sideways and up or down.
- The laptop screen in the headset, as part of the arrangement; it hides when the lid
  is closed.
- Profiles Work, Cinema and Lying down, each of which can be overwritten with your own
  setup. Monitors of the same size stay when you switch, so their windows do not move.
- Cinema mode (one big screen, and you pick which), focus mode (other monitors dim) and
  follow my head.
- Detach up to three PC windows into floating panels and attach them back; move the
  active window to the monitor in front of you; save a screenshot of that monitor to a
  Prostir folder in your Pictures folder (on KDE taken from that monitor's own stream).
- Monitor brightness for all monitors at once (100, 80, 60 or 40 %).

### In the headset

- A menu with Quick, Monitors and Settings tabs, which you can touch with a finger when
  your hands are in view.
- A round palm menu: hold an open palm to your face for In front, Focus, Cinema,
  Mouse?, Screenshot, Line up and Menu, with the time, the date and the headset's
  battery in the middle. Left or right hand; 24- or 12-hour clock, the date in
  Ukrainian, English or numbers. The round menu button by the monitor can be hidden.
- Hands shown, and if you like used for pointing: pinch to click, pinch and hold to
  drag.
- Backgrounds: black, your room through the passthrough cameras (plain or dimmed),
  four built-in panoramas or a picture of your own. A keyboard window shows the real
  desk.
- The "Mouse?" button shows where the pointer is, with rings; on Hyprland the mouse can
  follow your gaze.
- Optional PC notifications in the headset and rest reminders; a 72 Hz display rate to
  save battery.
- First-run tips, and a start screen that explains where to get the PC part.
- Ukrainian and English, in the headset and on the PC.

### Sound

- PC sound plays in the headset, and the headset microphone becomes the PC's
  microphone. The headset records only while a PC app listens and the microphone switch
  in the headset is on.
- Optional sound from the screen: the sound comes from the side of the monitor whose
  window is playing it.

### PC

- Encoding on the GPU through VA-API (tested on Intel; AMD should work but is
  untested) or NVENC on NVIDIA (experimental and untested), and on the CPU with x264
  everywhere else. HEVC or H.264, video quality and sharper text, set from the headset
  or the tray.
- Tray icon: connection status, headset battery with a warning at 15 %, monitors in
  front of you, video quality, autostart (KDE), the usage counter, forgetting paired headsets.
- Taking the headset off pauses streaming; after 10 s the virtual monitors go away and
  windows, the panel, sound and microphone return to the laptop until you put it on
  again.
- When the headset and the PC run versions that cannot talk, both say which one to
  update.
- Packages: `prostir-bin` on the AUR, a `.deb` and an `.rpm` (Fedora), a tarball with
  `install.sh` (with `--autostart` and `--uninstall --purge`), and a Nix flake
  (`nix run github:OlexiyOdarchuk/prostir`, NixOS and `nix develop`). Every release
  ships `THIRD-PARTY-LICENSES.txt`.
- Official builds count how many PCs use Prostir: one anonymous request a day with only
  the date in it, announced on the first start; off in the tray or with
  `PROSTIR_NO_COUNT=1`. Builds from source never count.

### Connection and security

- The headset finds the PC over Wi-Fi by itself, or you pick it from a list or type its
  IP address. In developer mode a USB cable works too (`adb reverse`).
- Encrypted connections (Noise XX) and one-time pairing with a 6-digit code (SPAKE2),
  confirmed in the headset first and typed on either side. The PC slows down repeated
  failed attempts.
- The headset lists the paired PCs, and you can forget any of them.
- The protocol between the headset and the PC is documented in `PROTOCOL.md`.
- Website with downloads and the privacy policy: prostir.ishawyha.dev.
