# Daliscope

A native macOS / Linux DALI commissioning tool and bus monitor for the
**Lunatone DALI-2 USB** interface. It does the core of what DALI Cockpit does,
without Windows.

- **Plug-and-play.** Plug the dongle in and it connects. No port picking and no
  driver. Unplug it and the app waits; replug it and the monitor carries on.
- **Bus monitor.** Live, decoded IEC 62386 traffic: 102 control gear, DT6
  (LED), DT8 (colour), 103 control devices and events, 301 push buttons, 303
  occupancy. Includes DTR tracking, send-twice detection, query/answer
  pairing, bus-clock timing (Δ ms), frames/s and bus load, filters, search,
  pause, markers, a frame inspector with a bit view, CSV/JSONL export and a raw
  frame sender with live decode.
- **Commissioning.** Scan the bus, run addressing (new installation or
  system extension, random-address search for gear *and* control devices),
  change or delete short addresses, and identify devices.
- **Parameter editing.** Gear: levels, fade, groups, scenes, DT6 dimming
  curve, live level control. Devices: application controller, groups,
  instances (event scheme, priority, groups, filter) and 303 deadtime, hold
  and report timers. Every write is read back and verified. Memory bank viewer.
- **Simulator.** A virtual DALI-2 USB with drivers and occupancy sensors that
  speaks the real protocol, so you can try everything without hardware.

The interface protocol (LUBA) is implemented from Lunatone's public
[LUBAP V1.10 specification](https://www.lunatone.com/wp-content/uploads/2021/04/LUBA_Protocol_EN.pdf).
Daliscope uses no Lunatone software or code.

## Install

**Linux (.deb):** `sudo apt install ./Daliscope_0.1.0_amd64.deb`. The package
installs a udev rule that gives you access to the dongle and stops
ModemManager from probing it. Unplug and replug the dongle once after
installing. Without the package, copy `packaging/99-daliscope.rules` to
`/etc/udev/rules.d/` and run `sudo udevadm control --reload`, or add yourself
to the `dialout` group.

**macOS:** build it yourself (see below) or take the `.dmg` from CI. The app
is unsigned, so the first time you run it, right-click the app and choose
Open (or run `xattr -dr com.apple.quarantine /Applications/Daliscope.app`).
The dongle shows up as `/dev/cu.usbmodem…` and needs no driver.

## Build

Requirements: Rust (stable) and Node (only for the Tauri CLI).

```sh
# Linux build deps (Debian/Ubuntu)
sudo apt install libwebkit2gtk-4.1-dev libudev-dev librsvg2-dev libayatana-appindicator3-dev

cargo test -p dali-core               # protocol, decoder, addressing tests
cargo run -p daliscope                # run the app (debug)
cd src-tauri && npx @tauri-apps/cli@2 build   # .deb / .AppImage / .app / .dmg
```

On macOS: `npx @tauri-apps/cli@2 build --target universal-apple-darwin`.
`.github/workflows/build.yml` builds the macOS universal `.dmg` and the Linux
bundles on GitHub Actions when you push a `v*` tag.

## Tools without the GUI

```sh
cargo run -p dali-core --example watch            # auto-detect dongle, print decoded traffic
cargo run -p dali-core --example watch sim        # same, against the simulator
cargo run -p dali-core --bin dali-sim             # simulator on a pseudo-terminal; prints its path
```

`dali-sim` lets you point the app (Settings → Preferred port) or any other
LUBA client at a fake interface over a real serial path.

## Layout

```
crates/dali-core/      protocol + logic (no GUI)
  luba.rs              LUBA framing, commands, descriptor/info/settings parsing
  event.rs             EVENT MESSAGE parsing
  dali.rs              IEC 62386 decoder + stateful annotator (DTRs, DT context, answers)
  monitor.rs           events -> timestamped monitor records (tick unwrapping)
  dongle.rs            plug-and-play manager, request/response + frame-id matching
  ops.rs               scan, addressing, gear/device read/write, memory banks
  sim.rs               simulated interface + virtual DALI bus
src-tauri/             desktop shell (Tauri 2)
ui/                    front end (plain HTML/CSS/JS, no build step)
```

## Notes and limits

- Addressing, scans and parameter access use plain DALI frames, so every
  step is visible in the monitor. The interface's built-in macros aren't
  needed.
- Bus power: the toggle appears only if the interface reports an integrated
  supply. Keep the total supply on the line at or under 250 mA.
- Not yet implemented: firmware updates of the interface, DT8 colour
  editing beyond reading Tc, emergency (DT1) testing, project files, and
  multi-line interfaces (line 0 only).
- Developed against the spec and a byte-level simulator. Please report any
  differences you see on real hardware. Event payload byte order is detected
  automatically because the spec doesn't state it.

Not affiliated with Lunatone. DALI and DALI-2 are trademarks of the DALI Alliance.
