# wayland-mouse

Pointer acceleration, scroll-wheel acceleration, temporary click lock, and per-device profiles for Wayland, configured from a terminal UI.

Wayland has no scroll-wheel speed setting and only a coarse pointer-acceleration curve. `wayland-mouse` is a small daemon that grabs the mouse with evdev and re-emits it through a virtual device, so its curves apply in every application. It is written in Rust, has no GUI dependencies, and is idle when you aren't moving the mouse.

## Features

- **Pointer acceleration.** A logistic gain curve: 1:1 at low speed for precision, higher gain at high speed, with a ceiling. The `mac-like` preset approximates macOS.
- **Wheel acceleration.** Precise at low speed, multiplied when you scroll fast, so long pages take one flick — consistently across app toolkits (see [How it works](#how-it-works)).
- **Profiles and live tuning.** `wayland-mouse tune` creates/updates/deletes profiles, binds them to devices, and plots each curve with a marker that follows the mouse. Edits apply immediately.
- **Temporary click lock.** Hold the left button briefly and release: the click stays held for drag-and-drop, renewed by pointer movement (per device).
- **DPI- and polling-rate-independent.** Curves are derived from kernel event timestamps. Tested at 8000 Hz.
- **Portable.** Works on GNOME, KDE, sway, Hyprland, or any libinput compositor.

## Tuning

`sudo wayland-mouse tune` opens a profile-centric UI. The sidebar always shows your profiles and the devices they are bound to; the header always shows the profile you are editing. Create a profile with `n`, tune it live with the arrow keys, update it with `u`, delete it with `d`, bind it to a device with `a`, and save with `s`. Built-in profiles are read-only; editing one forks a custom copy. Use `Tab` to move between the profile list, device list, and knobs, `1`/`2` to switch the pointer/wheel curve, and `t` to toggle per-device click lock.

<table>
  <tr>
    <td width="50%"><img src="https://raw.githubusercontent.com/monfa-red/wayland-mouse/main/docs/pointer.png" alt="Pointer tab"><br><sub><b>Pointer</b>: gain vs. cursor speed; the dot is your current speed.</sub></td>
    <td width="50%"><img src="https://raw.githubusercontent.com/monfa-red/wayland-mouse/main/docs/wheel.png" alt="Wheel tab"><br><sub><b>Wheel</b>: multiplier vs. scroll speed.</sub></td>
  </tr>
</table>

## Install

Requires a `uinput`-capable kernel (standard on Linux).

```
cargo install wayland-mouse
sudo "$(command -v wayland-mouse)" install
```

The `$(command -v …)` matters: `sudo`'s `secure_path` skips `~/.cargo/bin`, so a bare `sudo wayland-mouse install` runs an older copy on root's `PATH` (or fails with `command not found`) instead of the one you just installed. Re-run both lines to **upgrade** — the installer refuses to reinstall an older binary over a newer one in your cargo bin.

Or download the static binary from the [releases page](https://github.com/monfa-red/wayland-mouse/releases) and run `sudo ./wayland-mouse install`.

`install` copies the binary to `/usr/local/bin`, registers a systemd service, writes a default config to `/etc/wayland-mouse/config.toml`, ensures `uinput` loads at boot, and on GNOME sets the pointer acceleration profile to flat. Then run `sudo wayland-mouse tune`.

## Configuration

The config is TOML at `/etc/wayland-mouse/config.toml`; see the [annotated example](https://github.com/monfa-red/wayland-mouse/blob/main/wayland-mouse.toml.example) for every option. Choose a preset and override individual values:

```toml
preset = "my-trackpad"   # built-in or a custom profile

[presets.my-trackpad]
dpi = 1400
[presets.my-trackpad.pointer]
max_gain = 3.0

[[device]]
match = "Acer BT Keyboard1 Mouse"
preset = "my-trackpad"
[device.click_lock]
enabled = true
```

Validate with `wayland-mouse config --check`; `--print` shows the resolved values. Button remapping was removed: legacy `[[button]]` blocks are ignored (and flagged by `--check`).

## Commands

```
run         run the daemon (default; used by the service)
install     install or reinstall the binary, service, and config
uninstall   remove them and restore desktop settings
status      service state, effective config, and GNOME accel state
tune        live profiles + device bindings
config      --print | --check
```

The `tune` UI is included by default; build with `--no-default-features` for a daemon-only binary.

## Pointer acceleration and the compositor

The compositor's own pointer acceleration must be disabled, or it stacks on top of this one. `install` does this on GNOME and `uninstall` restores it; `wayland-mouse status` reports whether it is still off. On other desktops, set the pointer acceleration profile to flat once (install prints the setting). Wheel and click-lock features need no change and are fully portable.

## How it works

Each mouse is grabbed with evdev and re-emitted through a virtual `uinput` device with the curves applied. The grab is tied to the process, so if the daemon stops, the mouse returns to normal immediately. Pointer motion and buttons are copied through unmodified.

Scroll acceleration reaches every app by working two ways at once. Apps that scroll by the raw wheel delta (Chrome, Firefox, GTK) get a larger delta; apps that scroll a fixed amount per notch and ignore the delta (Flutter, Electron — e.g. the Ubuntu App Center) get extra notches instead. Both cover the same distance, so the feel stays consistent across toolkits. Slow scrolling stays fine-grained, and the crossover between the two is tunable (`min_multiplier`, `steps_above`).

## License

MIT
