# Changelog

## 0.1.0 (unreleased)

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
- Tray icon on the PC: status, video quality, autostart, forget paired headsets, quit.
- Encrypted connections (Noise XX) and one-time pairing with a 6-digit code (SPAKE2),
  shown on either side.
- Profiles: Work, Cinema and Lying down, each with its own monitors, laptop screen,
  layout, background and mode; any of them can be overwritten with the current setup.
- Cinema mode: only the primary monitor, big and further away.
- Focus mode: monitors without the mouse get darker.
- PC sound in the headset and the headset microphone on the PC (the headset records
  only while a PC app listens).
- Taking the headset off pauses streaming; after 10 s the virtual monitors go away
  until it is worn again. A headset that vanishes is noticed within about 10 s.
- Ukrainian and English, in the headset and on the PC.
- The headset menu is split into tabs: Monitors, Profiles, Look, Sound and language.
