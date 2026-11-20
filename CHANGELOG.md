# Changelog

## 0.2.0 (not released yet)

### Sound, menu and video while the picture changes a lot

- Sound, the microphone, focus and the menu's answers no longer wait while the PC sends
  a big frame. Frames go out in pieces of 16 KiB, and whatever is more urgent goes
  between them; the monitors take turns, so one monitor's keyframe no longer holds up
  the others. The PC also spreads what it sends over time, a little under what the
  network carries, instead of pushing a whole keyframe into the Wi-Fi router's queue at
  once: it watches the round trip, which grows as that queue fills, and never sends
  slower than the monitors need. A fast link is not slowed down. On a 100 Mbit/s link with
  keyframes on three monitors together, sound now arrives within about 10 ms (95 % of
  it) and at most about 20 ms late (before: 26 to 32 ms, and up to 90 ms), without the clicks
  of pauses over 30 ms (13 to 23 in 15 s before).
- A frame that waits too long is still dropped, and a new keyframe now also drops the
  older frames of its monitor still waiting, so a slow moment costs less.

### Monitors and the desktop

- Monitors in the headset no longer overlap, however many there are. When a row would
  reach behind your back (past 210 degrees), the circle grows a little; past 1.5 m the
  monitors make a second row above the main one, then a third. Three 2560×1440
  monitors stay as they were; six big ones (three ultrawide) make two rows at about
  1.35 m.
- Your arrangement stays yours. A layout is applied only when you pick it (and on the
  very first start). A new monitor takes a free place at the end of a row, beside the
  others: of the longest row with room, at the end nearer to where you look; over or
  under the rows only when no row has room, and the next one then joins it there.
  Nothing else moves; a removed one leaves the others where they are. Two rows stay two rows after a restart
  of the PC or of the headset app, a reconnect, taking the headset off, Line up and a
  profile switch.
- Line up (the A button) takes the crookedness out of each row and nothing more: a
  monitor turned or hanging crooked, gaps and small steps between neighbours. Every row
  stays where it was and keeps how it stands: tilted back for a reclined pose,
  overhead for lying down (Line up now works there too), or turned a little as a
  whole. Two monitors placed as mirror images stay mirror images. Rows never merge, and
  a gap you left between two rows stays.
- A monitor stays in its row while you move it about. A portrait monitor between two
  others no longer jumps into a row of its own when one of them is nudged; a monitor
  leaves its row only when you clearly move it out.
- A profile keeps where its monitors stood, so switching to it and back brings your
  rows back. Profiles saved before keep working and are laid out by their layout.
- The PC desktop matches the headset in every direction: the mouse, and a window you
  drag across, go from one monitor to the next where the two meet in the headset,
  sideways and up or down. A monitor you hung higher on purpose is just as much higher
  on the desktop; a step the hand leaves between neighbours in a row, up to a tenth of
  the monitor's height (7 cm on a 1440-pixel one), is lined up. Neighbours always
  share enough of an edge for the mouse to cross, and the desktop no longer spreads out
  into a staircase. The map in the menu shows exactly the desktop the PC makes.
- A monitor whose picture cannot start on the PC keeps its place and is tried again at
  the next connection; the headset says why. Before, it was dropped from the saved set.
- A display larger than the headset can show (a 5120-pixel-wide monitor of the PC
  where the headset decodes H.264 up to 4096) is shown as failed, saying why, instead
  of ending the connection again and again; the others stay. Switching to a codec in
  which a monitor would be too large is refused, and nothing changes.
- Each display of the PC itself (the laptop's panel, a monitor on a cable) is now one
  of its own: it is recognised by its make and model, keeps its own place in the
  headset, and comes back there after the lid was closed or the cable pulled out.
  Another monitor plugged into the same connector gets a place of its own instead of
  taking the old one's. The eight seen last are remembered while not connected; older
  ones are forgotten, so a laptop that met many projectors does not gather them. A profile can show or hide each of them, and make one of them
  the primary display.
- A display keeps its number for good: a removed monitor's number is never given to
  another one, so after a restart or a change of monitors nothing lands in another
  monitor's place.
- The sizes to add a monitor come from the PC, with 5120×1440 when the headset decodes
  HEVC.
- "No limits" in the PC tray allows up to ten monitors and four of the PC's own
  displays, for when the headset copes with more than the safe six.
- Switching to a profile puts its monitors in their places as soon as the PC has made
  them, instead of watching for up to 8 s.
- A monitor you add shows at once where it will appear, as a frame saying it is being
  added, and its picture takes that place; a monitor you remove leaves the view at
  once, and comes back if the PC cannot remove it.
- When something the headset asked for does not happen, the menu says why next to the
  control that asked, in the headset's language (for example "The limit is reached" or
  "No active window on the PC"), with the PC's own detail under it. A request the PC
  does not answer within 30 s says so instead of waiting for ever.

### Settings

- The PC keeps its settings in `~/.config/prostir/settings.txt` and the monitors with
  their rows in `screens.txt`, plain text. The first start moves them over from
  `monitors.txt`, which stays for an older version to read.
- Options on the command line (`--monitor`, `--bitrate`, `--codec`, `--software`) hold
  for that run only and no longer replace what was saved. What you change while it
  runs is saved, also in the tray while no headset is connected.
- Monitors keep their numbers between runs, so after a restart each one comes back
  where you put it in the headset.
- The codec you choose in the headset stays chosen when the graphics card has to use
  another one.

### Control from the PC

- `prostir ctl` asks the running Prostir from a terminal, a script or a hot key:
  `prostir ctl recenter`, `prostir ctl profile Work`, `prostir ctl monitor add
  2560x1440@125%`, `prostir ctl status`, `prostir ctl watch` (the headset connecting,
  taken off, put on, leaving), each with `--json` and exit codes for scripts.
- The same on D-Bus for other programs and hot keys (`io.github.OlexiyOdarchuk.Prostir`
  on the session bus, see `LOCAL-API.md`).
- The menu entry runs `prostir open`: it starts Prostir if it is not running and opens
  the settings window; the tray has "Settings…" too. `prostir` alone works as before.

### Sound

- The PC tray says when the headset's microphone has no permission to record, so a
  silent microphone is no longer a mystery.
- The PC no longer keeps a microphone setting of its own: its microphone follows the
  switch in the headset.

### Connection

- A dropped connection, a PC gone to sleep or a headset switched off is noticed within
  5 s on both sides (it took about 10 s).
- The headset says why a session ended: "The PC app was closed", "The PC ended the
  connection", or "No connection to the PC" when it fell silent. It looks for the PC
  again as before.
- "Forget paired headsets" in the PC tray ends the session of the forgotten headset at
  once; the headset then offers to pair again.
- Leaving a PC in the headset (another PC, forgetting it, closing the app) tells the
  PC why, and the PC writes it in its log. Both sides log the round trip of the
  connection every 10 s.
- What the tray asks of the headset (bring the monitors in front, No limits) waits for
  the headset's answer; a headset that does not answer within 30 s is written in the
  log instead of being waited for.
- A second headset can be given fewer rights with a line in `settings.txt`, for
  example only moving the mouse; the main headset can do everything, as before. A
  mistyped line gives that headset no rights rather than all of them.

## 0.1.3 (2026-11-20)

Fixes for the PC part. The headset app is the same as in 0.1.2.

### Monitors and windows

- On KDE, sharing one of Prostir's monitors in another app (Discord, OBS) no longer
  leaves it shrunk: Prostir puts its size back within a second.

### Sound

- The headset microphone could record only silence on the PC: the sound system kept
  a mute or a near-zero volume on Prostir's internal sound device from an earlier
  run. It now always stays at full volume.
- A recording no longer starts with old sound or loses its first moments, and sound
  held up by a Wi-Fi hiccup is dropped instead of coming out late: what you say comes
  out about 20 ms later.

### Packages

- The AUR has a source package, `prostir`, next to the prebuilt `prostir-bin`; either
  one replaces the other.

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
