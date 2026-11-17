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
- PC sound in the headset; PC apps hear the headset microphone while they record
- Touch the menu with a finger; resize a monitor with both hands
- A window onto your real keyboard through the passthrough cameras
- Wi-Fi or USB; encrypted, and paired with your PC once
- English and Ukrainian

Requirements: a PC with KDE Plasma 6 or Hyprland on Wayland, Intel or AMD graphics, and the free
Prostir app for the PC from prostir.ishawyha.dev. Prostir is free software (GPL-3.0).

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
- Звук ПК у шоломі; програми на ПК чують мікрофон шолома, поки записують
- Меню, якого можна торкатися пальцем; розмір монітора двома руками
- Вікно на справжню клавіатуру через камери шолома
- Wi-Fi або USB; шифрування й одноразове сполучення з ПК
- Українська й англійська

Вимоги: ПК з KDE Plasma 6 або Hyprland на Wayland, графіка Intel або AMD і безкоштовна програма
Prostir для ПК з prostir.ishawyha.dev. Prostir є вільною програмою (GPL-3.0).

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
| Internet | Local network to the user's own PC (or USB); no internet service |
| Languages | English, Ukrainian |
| Contains ads | No |
| Privacy policy | https://prostir.ishawyha.dev/privacy.html |
| Website | https://prostir.ishawyha.dev |
| Support | https://github.com/OlexiyOdarchuk/prostir/issues |

### Permissions (why the app asks for them)

- **Microphone (RECORD_AUDIO):** the headset microphone becomes a microphone on the
  user's own PC. It records only while a PC app is recording, and the sound goes only to
  that PC.
- **Hand tracking:** pointing, pinching and touching the menu; drawing the hands.
- **Passthrough (no camera images):** the room background and the keyboard window. The
  system composites the cameras; the app never receives images.
- **Network (INTERNET, Wi-Fi state):** finding and talking to the user's PC on the local
  network.

### Data use

- The headset app sends nothing to the developer or any third party. Everything goes
  between the headset and the user's own PC, encrypted.
- Platform features used: entitlement check only.
- The PC app (outside the store) sends one anonymous count a day to GoatCounter, with no
  identifier; see the privacy policy.

### Age rating (IARC) notes

No violence, sexual content, gambling, drugs or user-to-user communication. The app
shows the user's own PC screens; it has no store of its own, no ads and no purchases.
Note it as an app that displays content from the user's own computer.
