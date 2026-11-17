# Changelog

## 0.1.0 — unreleased

First public version.

- Virtual KDE Plasma monitors (KWin `stream_virtual_output`) streamed to a Meta Quest 3,
  hardware HEVC/H.264 via VA-API on the PC, MediaCodec on the headset.
- Monitors shown as OpenXR compositor layers (quad or cylinder) with supersampling.
- Add, remove, reorder and pick the primary monitor from the headset; KDE's layout
  follows the order in the headset.
- Layouts: arc, grid, column, overhead; flat, gently or strongly curved monitors.
- Mirror of the laptop screen under the main monitor, following the lid.
- Backgrounds: black, room (passthrough), night, sunset, space, aurora, own picture.
- Tracked hands shown, optional hand pointing (pinch, pinch-and-hold to drag).
- Discovery over Wi-Fi, USB via `adb reverse`, PC list and IP keypad in the headset.
- Tray icon on the PC: status, video quality, autostart, quit.
