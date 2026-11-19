# pods engine

`podsd` owns the link to the AirPods (Apple Accessory Protocol over L2CAP
PSM 0x1001) and serves `io.github.d101c.Pods1` on the session bus. The GNOME
Shell extension only talks to that API. Licence: GPL-3.0-or-later
(`COPYING`); origins of the code in `NOTICE`.

| Crate | Role |
|---|---|
| `pods-proto` | AAP frames, BLE proximity advertisement, AES-128, address resolution: pure, no I/O |
| `pods-bt` | raw L2CAP socket, BlueZ over zbus (device watch, alias, BatteryProvider1, AdvertisementMonitor1) |
| `pods-motion` | head tracking: sensor start/stop over AAP, yaw by twist about gravity, recentring, Follow Screen; no I/O |
| `pods-spatial` | spatial output on PipeWire: one binaural renderer for both modes (diffuse-field equalised KEMAR, dry bass, click-free head tracking), the AirPods' node state (`SinkWatch`); see `crates/pods-spatial/BENCH.md` |
| `podsd` | the daemon (supervision, state, D-Bus API, MPRIS, case card, keyring, spatial audio and head tracking) and `podsctl` |

No `bluer`, no libdbus, no `bluetoothctl`/`pactl`/`wpctl`/`pw-dump`
subprocesses, no root.

## Build and test

```sh
cargo build --release
cargo test        # needs dbus-daemon; never opens a Bluetooth socket
cargo clippy --all-targets -- -D warnings
```

The tests run the engine on a private `dbus-daemon` with a fake BlueZ (with
the advertisement monitor manager), a fake Secret Service and fake AirPods
that replay the frames captured from a real AirPods Pro
(`crates/pods-proto/tests/fixtures/`). BLE advertisements are podctl's
captures of an AirPods Pro 2, plus AirPods Pro 1 advertisements built and encrypted
with made-up keys. `pods_bt::forbid_real_bluetooth()` makes any real socket
attempt fail in the test process; nothing registers with the real BlueZ or
touches the user's keyring. Spatial audio runs against a fake renderer (the
`Renderer` trait) and a fake `bluez_output` node (the `AudioMonitor` trait):
the tests never connect to PipeWire. The head-tracking tests replay the
sensor frames of a real AirPods Pro (1st generation)
(`crates/pods-motion/tests/fixtures/`, M0 quater).

## Install (current user)

```sh
scripts/install-local.sh              # build, install, reload systemd
scripts/install-local.sh --uninstall
```

This copies `podsd` and `podsctl` to `~/.local/lib/pods`, the unit
`pods-engine.service` (Type=dbus, hardened) to `~/.config/systemd/user` and
the D-Bus activation file to `~/.local/share/dbus-1/services`. The engine
starts on the first call to `io.github.d101c.Pods`. Logs:
`journalctl --user -u pods-engine`.

Settings: `~/.config/pods/config.toml` (written by the engine).

```toml
low_battery_alerts = true
watchdog_secs = 60

[devices."AA:BB:CC:DD:EE:FF"]
press_hold_modes = ["transparency", "anc"]          # the user's choice
press_hold_cycle = ["off", "anc", "transparency"]   # read from the buds
color = "white"
ear_detection = true
pause_on_remove = true
resume_on_return = true
spatial_mode = "head"                               # "off", "fixed", "head"
follow_screen = true                                # on unless set
```

## API

The contract is `../docs/io.github.d101c.Pods1.xml`; a test checks that the
served objects match it member for member.

- `/io/github/d101c/Pods`, `io.github.d101c.Pods1.Manager`: `Devices`,
  `ActiveDevice`, `EngineVersion`, `Conflict` (another AAP client running:
  podctl, EarPort, LibrePods…); signals `CaseOpened(device, snapshot)` and
  `CaseClosed(device)` (the case card, below).
- `/io/github/d101c/Pods/devices/AA_BB_CC_DD_EE_FF`,
  `io.github.d101c.Pods1.Device`: identity, `Color`, `Connected`, `Battery`,
  `Wearing`, `NoiseMode(s)`, `PressHoldModes` (in the order the buds walk
  them), `EarDetection`, `PauseOnRemove`, `ResumeOnReturn`, `MediaPaused`,
  `ConversationAwareness`; `SetNoiseMode`, `CycleNoiseMode`,
  `SetPressHoldModes`, `Rename`, `SetEarDetection`, `SetPauseOnRemove`,
  `SetResumeOnReturn`, `SetConversationAwareness`, `Connect`, `Disconnect`;
  signals `NoiseModeChanged(mode, source)` (`tile` when the order came
  through the API, `stem` otherwise) and `LowBattery(part, percent)` (each
  bud, once at 10 % and once at 5 %, re-armed by charging).
  Spatial audio and head tracking: `SpatialMode`, `SpatialStatus`,
  `FollowScreen`, `HeadTracking`; `SetSpatialMode`, `SetFollowScreen`,
  `Recenter`, `WatchHeadPose`; signal `HeadPose(yaw, pitch)` (below).

Errors: `io.github.d101c.Pods1.Error.NotConnected`, `.NotSupported`,
`.InvalidArgument`, `.Failed`.

`podsctl` is a small client of that API for debugging:

```sh
podsctl status
podsctl set-noise transparency
podsctl cycle
podsctl press-hold off anc
podsctl rename "My AirPods"
podsctl spatial head        # off | fixed | head
podsctl recenter
podsctl follow-screen off
```

## Behaviour

- The link opens while BlueZ reports the AirPods connected (signals on
  `Device1`, AAP UUID `74ec2172-0bad-4d01-8f77-997b2be0722a`). Opening
  sequence: handshake, feature flags `0x4D D7`, notifications `0x0F FFFFFFFF`,
  then the press-and-hold cycle if the user chose one.
- Failed attempts wait 2, 4, 8, 16 then 32 s. With `watchdog_secs` set, no
  frame for that long while BlueZ says connected reopens the link without
  `Connected` going false (off by default: a Pro 1 stays silent for minutes).
- The first time a pair connects, the engine asks for its proximity keys
  (AAP `0x30`, mask `05`); the reply (`0x31`, IRK and ENC) goes to the
  user's keyring through the Secret Service (`org.freedesktop.secrets`,
  item schema `io.github.d101c.Pods.ProximityKeys`). The keys are never
  written to a file or to the journal (a test captures every log line).
- The press-and-hold cycle: the Pro 1 never reports it (control `0x1A`) on
  connect and AAP has no read request, so the engine follows the stem: a
  press that lands on a mode outside the known cycle, or out of its order,
  corrects `PressHoldModes` and is kept in `press_hold_cycle`. A `0x1A`
  report, when a model sends one, wins. `CycleNoiseMode` walks the same
  cycle as the stem.
- Capabilities come from a table per product id (the Pro 1 sends no `0x02`
  reply): AirPods Pro (1st generation) offer off, transparency and ANC.
- When a bud comes out while audio goes to the AirPods (an active BlueZ
  media transport), the MPRIS players that were playing are paused; the
  same ones (by unique bus name) resume if the bud is back within a minute.
  `MediaPaused` is true while the engine holds such players, until they
  resume, the link drops or the minute is over.
- GNOME's Power panel gets the lowest bud through `org.bluez.BatteryProvider1`.
- Conversation awareness (control `0x28`) for models with the
  `conversation` capability; `SetConversationAwareness` fails with
  `NotSupported` on the others (the Pro 1).

## The case card

- A passive `org.bluez.AdvertisementMonitor1` (`or_patterns`, AD `0xFF` at
  position 0, content `4C 00 07 19`) is registered with every adapter at
  start, under `/io/github/d101c/Pods/monitor` on the system bus, and stays
  registered, connected or not. BlueZ hands the pattern to the controller
  when it supports `controller-patterns`. **No `StartDiscovery`.** RSSI: in
  range at -80 dBm for 1 s, lost after 8 s without a packet at -90 dBm.
- An advertisement counts only when its private address resolves with the
  IRK of a pair paired here (`ah`, AES-128). The last 16 bytes decrypt with
  the ENC key into exact levels, used when they agree with the clear
  levels (tens of percent); otherwise the clear ones.
- The lid: data byte 8 (bit 3 closed, bits 0-2 opening counter), read only
  while both buds are in the case. Closed → open when the counter moved, or
  "open" read twice within 300 ms, or held for 300 ms. Open → closed at the
  first "closed", when a bud leaves the case, when BlueZ reports the
  advertiser lost (8 s of silence), or after 60 s without any change.
- `CaseOpened(device, snapshot)` once per opening, then at most every 2 s
  while the snapshot changes (`connected` included: the Connect button of
  the card goes through BlueZ `Device1.Connect`). `Color` comes from the
  same advertisement and is kept in the settings.

## Spatial audio and head tracking

One task per pair of AirPods owns the renderer (`pods-spatial`) and the head
sensors (`pods-motion`).

- `SetSpatialMode(off|fixed|head)` fails with `InvalidArgument` for another
  word, `NotSupported` for `head` on a model without the `spatial.head`
  capability, `NotConnected` while the AirPods are not connected. It returns
  at once: the renderer starts on a blocking thread (about 0.7 s) and
  `SpatialStatus` tells how it goes. Another mode is a stop, then a start.
  The mode is kept per device in `config.toml` and applied again at every
  connection.
- `spatial.head` is announced for the models with motion sensors: AirPods
  Pro (all), AirPods 3, AirPods 4 (both), AirPods Max (all); not for
  AirPods 1 and 2. The fixed mode works on every model.
- `SpatialStatus`: `off` (mode off, not connected, or the user picked
  another output: it stays off until the mode is set again or the next
  connection), `starting` (renderer starting, or waiting for the AirPods'
  node `bluez_output.<address>.1`), `active`, `paused` (Listen Together
  shares the sound), `failed` (the renderer could not start or its backend
  died: tried again when the mode is set again or the node appears anew).
  When the node goes away under the renderer (`TargetGone`, e.g. a profile
  switch) everything stops and starts again once the node is back.
- Every AAP frame of the link goes through `HeadTracking::handle_frame`,
  whatever the mode (the Pro 1 announces its sensors once, right after the
  link opens); the frames it answers go back on the link. The sensors are
  asked for only in `head` mode, while the renderer is active (not paused),
  a bud is worn (or wearing is unknown) and sound flows to the AirPods: the
  engine follows the state of `bluez_output.<address>.1` through the
  `pipewire` crate (node `info` events, `pods_spatial::SinkWatch`), and
  `running` means sound. They stop 10 s after the sound stops (pods-motion's
  grace period) and get an interval of 0 when the mode changes.
- `HeadTracking`: `unavailable` (no sensors on this model, or not
  connected), `idle` (sensors stopped), `starting` (asked, no sample yet),
  `active` (samples flow).
- Render poses (≤ 50 Hz) go to the renderer's yaw; `HeadPose(yaw, pitch)`
  (≤ 30 Hz) is sent only while at least one client watches. Yaw: degrees,
  positive to the left. **Pitch is the tilt of the head** (`tilt_deg` of
  pods-motion): how far it leans from its upright reference, unsigned, nod
  and roll alike, since nothing yet tells the bud's forward axis.
- `WatchHeadPose(b)` counts clients by unique bus name (twice the same
  client is one watcher); a client that leaves the bus stops watching.
- `Recenter` (connected, `spatial.head`) makes the current direction
  straight ahead. `SetFollowScreen(b)` (`spatial.head`) is kept per device
  in `config.toml`, on by default.
- Everything stops cleanly when the AirPods disconnect (renderer stopped,
  sensors forgotten: nothing can be sent), when the daemon stops (SIGTERM:
  interval 0 written to the buds, default output given back, at most 8 s)
  and when the renderer reports `TargetGone` or `BackendFailed`.
