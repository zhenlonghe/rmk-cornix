# RMK Configuration for Cornix Keyboard

This repository contains an unofficial [RMK](https://rmk.rs/) configuration for
the Cornix keyboard by Jezail Funder. It aims to help users to customize their
own RMK firmware for Cornix, not to replicate the official firmware.

# Features

- It supports all keys and rotary encoders.
- It supports Vial.
- It supports the onboard RGB status indicators for battery and connection
  status.
- Its Vial layout is roughlly compatible with the official firmware, so you can
  load your existing Vial layout (`.vil` file) without much modification.
  Macros, combos, tap dances, key maps for rotary encoders, and some other
  things may lost or be messed up, so you may still need to reconfigure them.

# Notes

- Full RGB lighting effects are not supported. The onboard LEDs are used only as
  short status indicators.
- BLE is configured for balanced daily use: 2M PHY, +8 dBm TX power, and split
  central sleep after 5 minutes of inactivity.
- Status LEDs use an adaptive refresh: they run smooth, gamma-corrected
  breathing with soft fade in/out while active, and drop to a slow idle tick
  once an effect becomes static or turns off, so idle wakeups stay low.
- Status LEDs are power-limited. Connection/profile events show for 3 seconds,
  advertising/disconnected breathing stops after 60 seconds, and low battery
  reminders pulse briefly every 5 minutes.

# Status LEDs

Each half has two independently controlled WS2812 LEDs. Effects are evaluated
in priority order from top to bottom; when a temporary effect expires, the next
active effect is restored automatically.

## Central (left) half

| LED | Priority | Behavior |
| --- | --- | --- |
| Inner (right) | USB charging | Breathes green while charging; shows solid green for 3 seconds after reaching 95%, then turns off |
| Inner (right) | Low battery | Double-pulses red for 5 seconds at 20% or below, repeating about every 5 minutes |
| Inner (right) | Split connection | Breathes blue for up to 60 seconds while disconnected; shows solid blue for 3 seconds after connecting |
| Outer (left) | BLE connection/profile | Shows the active profile color for 3 seconds after connecting or switching profiles |
| Outer (left) | BLE advertising | Breathes the active profile color for up to 60 seconds |
| Outer (left) | Caps Lock | Stays amber while Caps Lock is active |

BLE profile colors are red, green, blue, magenta, and cyan for profiles 1
through 5.

## Peripheral (right) half

| LED | Priority | Behavior |
| --- | --- | --- |
| Inner | USB charging | Breathes green while charging; shows solid green for 3 seconds after reaching 95%, then turns off |
| Inner | Low battery | Double-pulses red for 5 seconds at 20% or below, repeating about every 5 minutes |
| Outer | Split connection | Breathes blue for up to 60 seconds while disconnected; shows solid blue for 3 seconds after connecting |

Both LEDs fade smoothly between states and turn off while the keyboard is
sleeping. Animated effects and fades update every 33 ms; settled solid colors
and off states use a 1-second idle tick. State-change events still trigger a
prompt update. Battery voltage is sampled every 30 seconds where an ADC is
available, and the LED power rail is shut down after both LEDs fade fully to
black.

# Usage

1. Make any changes you want for the firmware.

2. Build the firmware. Execute in the repository root:
   ```sh
   cargo make uf2
   ```
   This will generate two `.uf2` files in the repository root. Make sure you
   have the pinned Rust toolchain and `cargo-make` installed. `cargo build
   --release` only builds the ELF binaries under `target/`.

   Otherwise, fork this repository, go to GitHub Actions tab, tap *Build RMK
   firmware*, and download the artifacts when the build is done.

3. Flash the two `.uf2` files to the left and right halves of the keyboard
   respectively. You may need to delete Bluetooth pairing on your computer first
   and re-pair after flashing.
