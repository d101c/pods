# pods — GNOME Shell extension

The interface half of pods: a Quick Settings tile for AirPods, for GNOME Shell 51.
It talks only to the engine (`podsd`) over the session bus, following
`docs/io.github.d101c.Pods1.xml`. Without connected AirPods it shows nothing, and it
never adds an icon to the top bar. Without the engine installed, it says so once, in a
notification, and AirPods Settings offers to install it.

- **Tile**: blue (the system accent) while a noise mode is on, grey in Off. A click
  cycles the modes like a press and hold on the stem (`CycleNoiseMode`).
- **Menu** (the arrow): name and status line ("Not in ears · Paused" while the
  engine holds the media it paused), three battery rings (left, right, case;
  green while charging, coral under 20 %, dotted when unknown), the noise control,
  Conversation Awareness on the models that have it, Spatial Audio (Off, Fixed, and Head
  Tracked on AirPods with motion sensors, `spatial.head`) with a line of help and
  Recenter when the head is tracked (the on-screen display says "Recentered"), and
  AirPods Settings.
- **Case card**: when the case of paired AirPods opens nearby, a card rises where
  the Shell shows its on-screen displays: the open case floating gently, the name,
  the batteries (both earbuds as one when less than 10 points apart), and Connect
  when they are not connected to this computer. It goes 6 s after "Connected", when
  the lid closes, or with its close button; never over a fullscreen window or while
  the screen is shared; not at all with "Show Card When Case Opens" off. With
  animations off it appears at once and the case does not float.
- **On-screen display** when the mode changes from the stem or the tile.
- **Notifications** for the engine's `LowBattery` signal.
- **AirPods Settings** (`prefs.js`), two pages:
  - *General*: press and hold modes, ear detection, name, the one setting of the
    extension itself, "Show Card When Case Opens" (GSettings), and the engine (version,
    Uninstall Engine).
  - *Spatial Audio* (`spatialPage.js`): a live top view of the head and the two virtual
    speakers (`headView.js`, Cairo), the mode, Follow Screen, Recenter, the head profile
    and a test sound (`testSound.js`: a chime left then right, computed, played with
    GStreamer on the default output). The page asks the engine for `HeadPose`
    (`WatchHeadPose(true)`) only while it is on screen and the AirPods have the sensors,
    and gives it up as soon as it is not.
- **Engine install** (`installer.js`): when the engine is missing (its bus name has no
  owner and the bus cannot start it), one notification, once, then a banner on each
  page of AirPods Settings. One click on Install Engine downloads the archive for this
  machine (x86_64 or aarch64) with libsoup, checks its SHA-256 against
  `engine-release.json` (shipped with the extension), unpacks it with libgnome-autoar,
  copies the engine to `~/.local/lib/pods`, the systemd user unit and the D-Bus
  activation file next to it, then asks systemd over D-Bus to reload and start it. No
  administrator password, no program run by the extension, nothing before the click.
  Uninstall Engine stops it and removes those files; `~/.config/pods` stays.

## Files

| File | Role |
|---|---|
| `extension.js` | enable/disable, wiring |
| `engine.js` | D-Bus client of the engine (no Shell import; also used by `prefs.js`) |
| `installer.js`, `engine-release.json` | engine install and uninstall; the release it installs |
| `contract.js` | the D-Bus contract, generated from `docs/` by `tools/update-contract.py` |
| `model.js` | texts and states shown, from a device snapshot (no Shell import) |
| `tile.js`, `rings.js`, `osd.js`, `notify.js` | tile and menu, battery rings, OSD, notifications |
| `caseCard.js` | the card shown when the case opens |
| `prefs.js`, `spatialPage.js`, `headView.js`, `testSound.js` | AirPods Settings |
| `icons/` | symbolic icons, outlined from the mockups by `tools/make-icons.py` |
| `art/` | colour drawings of the case and the earbuds, drawn by hand (no Apple artwork) |
| `schemas/` | the extension's GSettings |
| `stylesheet-dark.css`, `stylesheet-light.css` | the Shell loads the one of the current style |

## Development

```sh
extension/tools/shell-test/run.sh      # headless GNOME Shell + fake engine, all checks, screenshots in out/
gjs -m extension/tools/test-model.js   # texts and states only
extension/scripts/pack.sh              # the zip for extensions.gnome.org, in extension/dist/
```

The headless test runs on private session and system buses with a private home
directory: it never touches the running session, the real Bluetooth or the sound.
`tools/fake-engine.py` serves the contract with made-up AirPods and takes scenarios
on its standard input (`pro1`, `pro2`, `low`, `one-bud`, `charging`, `disconnect`,
`stem`, `paired`, `lid-open`, `lid-close`, `pause`, `spatial`, `pose`, `pose-sim`…; see its
header). It refuses to run on the real session bus. The settings window is driven through
the accessibility bus (`tools/shell-test/a11y.py`, by role and name). The engine install is
tried end to end against a release served by `tools/shell-test/serve.py` on 127.0.0.1 and a
stand-in systemd (`tools/shell-test/fake-systemd.py`), in the test's private home.

`engine-release.json` comes from `engine/scripts/make-release.sh`, which builds the
archive of one architecture and prints its entry (URL, size, SHA-256).

Run `shexli` on the zip before any upload.

Licence: GPL-2.0-or-later (`COPYING`).
