# Architecture

Pods is two programs: an engine that talks to the AirPods, and a GNOME Shell extension
that shows them. They only meet on the session bus, through the contract in
[io.github.d101c.Pods1.xml](io.github.d101c.Pods1.xml).

```
AirPods ──Bluetooth── BlueZ ──────────────┐              PipeWire
   │                     AdvertisementMonitor1           ▲
   │ L2CAP PSM 0x1001 (AAP)                │             │ spatial sink
   └──────────────────────────────► podsd (Rust, systemd user service)
                                        │
                          session bus: io.github.d101c.Pods
                                        │
                     GNOME Shell extension (GJS) + AirPods Settings (libadwaita)
```

## Rules

1. **Only the engine talks to the AirPods.** The AirPods send their notifications to
   the last host that opened the channel, so two clients steal them from each other.
   The engine reports another client it sees (`Conflict`).
2. **The extension only talks to the engine.** It never touches BlueZ or the protocol.
3. **No administrator rights.** Everything runs in the user's session. Nothing changes
   `/etc/bluetooth/main.conf`.
4. **Nothing runs at rest.** There is no active Bluetooth scan, and the motion sensors
   only stream while sound plays to the AirPods and spatial audio is head tracked.

## Engine (`engine/`)

| Crate | Role |
|---|---|
| `pods-proto` | AAP frames, the proximity advertisement, AES and address resolution. Pure, no I/O. |
| `pods-bt` | Raw L2CAP socket, BlueZ over zbus (device watch, alias, BatteryProvider1, AdvertisementMonitor1). |
| `pods-motion` | Head tracking: starting the sensors over AAP, yaw, Recenter, Follow Screen. No I/O. |
| `pods-spatial` | Binaural rendering in a PipeWire filter, and taking and giving back the default output. |
| `podsd` | The daemon: supervision, state, D-Bus API, MPRIS, case card, keyring. Also `podsctl`. |

**AAP link.** The engine uses a raw `SOCK_SEQPACKET` L2CAP socket on PSM 0x1001 with
`BT_SECURITY_MEDIUM` and a 4096-byte MTU. It finds the AirPods through BlueZ signals
and the AAP service UUID. When opening, it sends the handshake, the feature flags and
the notification request, then reads the battery, the ear state, the settings and the
model information. Capabilities come from a table per model.

**Case card.** A passive BlueZ `AdvertisementMonitor1` matches Apple proximity
advertisements (`4C 00 07 19`), with the filtering offloaded to the controller when it
supports it. The engine resolves the private address with the IRK, so other people's
AirPods never trigger anything. It decrypts the exact battery levels with the
encryption key. Both keys are requested once over AAP and kept in the Secret Service.

**Head tracking.** Models with an H1 chip (AirPods Pro 1st generation, AirPods 3)
expose their sensors as RemoteHID devices. H2 models use SensorDataWX. The sensors are
only announced while sound plays to the AirPods. Yaw is the twist about the reference
gravity of `q0⁻¹·q`; Euler angles are wrong here because the earbud sits tilted in
the ear. On real AirPods Pro (1st generation), a quarter turn reads ±85°, the return to
the centre is within 2°, and the drift at rest is about 2.6°/min. Follow Screen absorbs
that drift.

**Spatial audio.** Two virtual speakers at ±30°, rendered with the MIT KEMAR HRTF,
with these corrections:
- The HRIR set is diffuse-field equalised: the raw set carries the measuring
  loudspeaker's response, about −17 dB at 63 Hz and +13 dB at 2 kHz.
- Frequencies below 150 Hz pass dry.

When the head moves, two convolvers that share the same input history crossfade over
one block. Changing the angle of a stock convolver clicks; this does not. Fixed is the
same renderer at a constant angle. Results on the bench (`engine/crates/pods-spatial/BENCH.md`):
- response within ±2 dB of spatial Off from 63 Hz to 8 kHz, averaged over directions;
- no transient above −60 dB;
- about 1 % of one core;
- no added latency.

Spatial audio pauses while another app shares the audio (Listen Together).

## Extension (`extension/`)

A `QuickMenuToggle` (`tile.js`) carries the battery rings, the noise control, spatial
audio and AirPods Settings. `caseCard.js` shows the card where the Shell shows its
on-screen displays. `osd.js` and `notify.js` cover mode changes and low battery.
`prefs.js` and `spatialPage.js` are AirPods Settings, with a live top view of the head.
`installer.js` downloads and installs the engine when the user asks for it.

## Tests

- **Engine**: unit and integration tests on private D-Bus buses with a fake BlueZ, a
  fake link and recorded real frames. No test opens a real Bluetooth socket.
- **Spatial rendering**: measured on a private PipeWire instance (`bench/`).
- **Extension**: checked in a headless GNOME Shell against a fake engine
  (`extension/tools/shell-test`).
- **Real AirPods**: the `real-world test` issues.
