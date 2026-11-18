# Meta Horizon Store listing

Text for the Developer Dashboard. Art: `python3 packaging/store/make_art.py CAPTURE OUT`.

## English

**Title:** Prostir

**Short description**

Extra monitors for a Linux PC, around you in the headset. Real desktop outputs:
windows, panels and the mouse move across them. Needs the free Prostir app on a PC
with KDE Plasma 6 or Hyprland.

**Long description**

Prostir puts extra monitors for your Linux laptop or desktop (KDE Plasma or Hyprland)
around you in Quest. They are real outputs of the PC: drag windows onto them, maximize
them, keep a panel or workspaces there. The mouse moves from one monitor to the next where they meet in front of
you, sideways and up or down.

Add, remove and arrange monitors from the headset: 1080p, 1440p, 4K, ultrawide or
portrait, up to six. Line them up on an arc, in a grid, in a column or above your head
for lying down, flat or curved. The laptop's own screen can hang under the main monitor.

Text stays sharp: the video goes straight into the headset's compositor layers instead
of a flat window, and the PC encodes it on the GPU.

- Profiles for work, cinema and lying down, switched in one press
- Focus mode dims every monitor except the one with the mouse
- PC sound in the headset; PC apps hear the headset microphone while they record, and a
  switch in the headset menu turns the microphone off
- Touch the menu with a finger; resize a monitor with both hands
- A window onto your real keyboard through the passthrough cameras
- Over your local Wi-Fi network; encrypted, and paired with your PC once
- English and Ukrainian

Requirements: a PC with KDE Plasma 6 or Hyprland on Wayland on the same network, and the
free Prostir app for the PC from prostir.ishawyha.dev. Intel or AMD graphics encode in
hardware; with other graphics the PC encodes on the CPU. Prostir is free software
(GPL-3.0-or-later).

## Українською

**Назва:** Prostir

**Короткий опис**

Додаткові монітори для ПК з Linux навколо вас у шоломі. Справжні виходи робочого
столу: вікна, панелі й миша ходять по них. Потрібна безкоштовна програма Prostir на ПК
з KDE Plasma 6 або Hyprland.

**Повний опис**

Prostir ставить навколо вас у Quest додаткові монітори для ноутбука чи ПК з Linux (KDE
Plasma або Hyprland). Це справжні виходи ПК: переносьте на них вікна, розгортайте,
тримайте там панель чи робочі столи. Миша переходить з одного монітора на інший там, де вони стикаються перед вами,
вбік і вгору чи вниз.

Додавайте, прибирайте й розставляйте монітори прямо з шолома: 1080p, 1440p, 4K, широкі
чи портретні, до шести. Дугою, сіткою, стовпчиком або над головою, щоб лежати; пласкі чи
вигнуті. Екран самого ноутбука може висіти під головним монітором.

Текст чіткий: відео йде просто в шари компоновника шолома, а не в пласке вікно, а ПК
кодує його відеокартою.

- Профілі для роботи, кіно й лежачи, одним натиском
- Режим фокусу притемнює всі монітори, крім того, де миша
- Звук ПК у шоломі; програми на ПК чують мікрофон шолома, поки записують, а перемикач
  у меню шолома вимикає мікрофон
- Меню, якого можна торкатися пальцем; розмір монітора двома руками
- Вікно на справжню клавіатуру через камери шолома
- Через вашу локальну мережу Wi-Fi; шифрування й одноразове сполучення з ПК
- Українська й англійська

Вимоги: ПК з KDE Plasma 6 або Hyprland на Wayland у тій самій мережі й безкоштовна
програма Prostir для ПК з prostir.ishawyha.dev. Графіка Intel чи AMD кодує апаратно; з
іншою графікою ПК кодує процесором. Prostir є вільною програмою (GPL-3.0-or-later).

## Trailer

`prostir-trailer.mp4`: the demo cut with music. The music needs this credit wherever the
trailer is published (store page, posts):

Music: "Digital Lemonade" by Kevin MacLeod (incompetech.com), licensed under Creative
Commons: By Attribution 4.0, https://creativecommons.org/licenses/by/4.0/ (ISRC
USUAN1700010)

## Details (draft answers)

| Field | Answer |
|---|---|
| Category | Productivity (genre: Utilities) |
| Price | Free |
| Supported devices | Quest 3, Quest 3S (Quest 2 and Pro untested, leave off for now) |
| Input | Touch controllers and hand tracking |
| Play mode | Seated, stationary |
| Comfort | Comfortable (no artificial locomotion) |
| Players | Single user |
| Internet | Local network to the user's own PC; no internet service |
| Languages | English, Ukrainian |
| Contains ads | No |
| Privacy policy | https://prostir.ishawyha.dev/privacy.html |
| Website | https://prostir.ishawyha.dev |
| Support | https://github.com/OlexiyOdarchuk/prostir/issues |
| Private contact (security) | https://github.com/OlexiyOdarchuk/prostir/security/advisories/new |
| Third-party licenses | In the APK under assets/licenses/, and https://github.com/OlexiyOdarchuk/prostir/releases/latest/download/THIRD-PARTY-LICENSES.txt |

### Permissions (why the app asks for them)

- **Microphone (RECORD_AUDIO):** the headset microphone becomes a microphone on the
  user's own PC. It records only while a PC app is recording and the microphone switch
  in the headset menu (Sound and language) is on, and the sound goes only to that PC,
  encrypted. The PC can ask for the microphone but cannot turn the switch on.
- **Hand tracking:** pointing, pinching and touching the menu; drawing the hands.
- **Passthrough (no camera images):** the room background and the keyboard window. The
  system composites the cameras; the app never receives images.
- **Network (INTERNET, Wi-Fi state):** finding and talking to the user's PC on the local
  network.

### Data use

- The headset app sends nothing to the developer or any third party. Everything goes
  between the headset and the user's own PC, encrypted. It collects no account data,
  no analytics and no crash reports.
- Platform features used: entitlement check only.
- The PC app is not part of the store listing. Its official builds count users: once a
  day, when a headset connects, the PC sends GoatCounter one request with only the date
  in it (and `/install` the first time). No identifier, host name or settings. A
  notification on the first start explains it and how to turn it off (tray item "Count
  this PC among users", or `PROSTIR_NO_COUNT=1`); the first count is sent no earlier
  than the next day (UTC). See the privacy policy.
- Answer the Data Use form accordingly: the headset app collects and shares no user
  data. Microphone audio is processed only to send it to the user's own PC.

### Age rating (IARC) notes

No violence, sexual content, gambling, drugs or user-to-user communication. The app
shows the user's own PC screens; it has no store of its own, no ads and no purchases.
Note it as an app that displays content from the user's own computer.

## Notes for the reviewer

Text for the testing-notes field of the submission.

Prostir is a companion app: it shows extra monitors of the user's own Linux PC. To test
everything you need:

- a PC running Linux with KDE Plasma 6 (Wayland session) or Hyprland, on the same Wi-Fi
  network as the headset, with no client isolation between devices;
- the free Prostir app for the PC from https://prostir.ishawyha.dev (packages for Arch,
  Debian/Ubuntu and Fedora, or a tarball with `install.sh`).

Steps:

1. On the PC, install and start Prostir. Its icon appears in the system tray. If a
   firewall runs on the PC, allow TCP 47830 and UDP 47831.
2. On the headset, open Prostir. Without a PC it shows the start page with these
   requirements and "Looking for PCs on the network".
3. The PC appears in the list. Select it and confirm "Pair with <PC name>?".
4. The headset shows a 6-digit code. Type it into the dialog that opens on the PC.
5. Three monitors appear around you. Open the menu (☰ or X on the left controller) and
   try the tabs: Monitors (add, remove, layout, curve), Profiles (Work, Cinema, Lying
   down), Look (backgrounds, hands, focus mode), Sound and language (microphone switch,
   language).
6. Put a window on a virtual monitor from the PC; take the headset off and on again
   (streaming pauses and resumes).

If you cannot set up such a PC, a video of the whole flow (pairing, monitors, menu,
hands, microphone) is at https://prostir.ishawyha.dev/demo.mp4.

TODO before submitting: record and upload that video; the link is a placeholder.

## End-user license (to do)

Without a custom EULA, the store applies Meta's default end-user license, which grants
personal, non-commercial use and forbids modification unless the developer allows it.
Prostir is GPL-3.0-or-later, and once it includes code from other contributors the
store version must not add such restrictions (GPLv3 section 10).

TODO for the developer: in the Developer Dashboard, set a custom EULA along these lines:

> Prostir is free software, licensed under the GNU General Public License, version 3 or
> (at your option) any later version, with an additional permission for linking the
> Meta Platform SDK. You may use, study, share and modify it under those terms. The
> source code and the license texts are at https://github.com/OlexiyOdarchuk/prostir.
> Third-party components keep their own licenses, listed in THIRD-PARTY-LICENSES.txt.

Alternatively, extend LICENSE-EXCEPTION.md so that it explicitly permits distribution
through the Meta Horizon Store under the store's terms. This is a legal choice for the
developer to make.
