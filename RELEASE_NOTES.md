# Release notes

## v0.1.0 -- initial release

First working release of esp-unlock: a USB-attached ESP32 acts as a
physical-presence key that locks/unlocks a GNOME session, confirmed by a
button press on the device.

### Features

- **Firmware** (`firmware/`): runs on a classic ESP32 dev board. Listens on
  UART0 for a `"CH"` + 32-byte challenge, waits up to 15 seconds for a
  physical button press (debounced, requires a release-then-press
  transition so a stuck/taped button can't confirm on its own), and replies
  with `HMAC-SHA256(secret, challenge)` on success or a denial otherwise.
- **Host daemon** (`host/`, binary `esp-unlock-daemon`): watches the
  device's serial port and the GNOME session's lock state via `loginctl`.
  - Unplugging the key while the session is unlocked locks it immediately.
  - While locked and the key is present, it continuously offers challenges
    and unlocks the session on the first valid button-confirmed response.
  - Typing the account password on the lock screen always works and is
    completely unaffected by the daemon.
  - `esp-unlock-daemon test-challenge` runs a single challenge/response
    round for bench testing, without touching the session at all.
- **Shared protocol crate** (`protocol/`): the wire format and HMAC
  challenge/response logic live in one `no_std`-compatible crate used by
  both the firmware and the host, so they can't silently drift apart. Fully
  unit-tested (frame assembly/resync, HMAC verification, a known-answer
  vector).
- Dual-licensed under MIT or Apache-2.0.

### Hardware notes from bring-up

A few ESP32/esp-hal gotchas were hit and worked around during development;
see the README's "Hardware notes" section for details:

- `espflash` refuses to flash an image without an `esp_app_desc!()`-based app
  descriptor by default; this firmware includes one, but older toolchains or
  custom images may need `--ignore-app-descriptor`.
- The RTC watchdog left enabled by the bootloader must be explicitly
  disabled, or the chip reboots itself a few seconds into normal operation.
- UART0's RX/TX pins need to be bound explicitly (`.with_rx()`/`.with_tx()`);
  otherwise the device can transmit but never actually receives anything
  the host sends.
- GPIO35 has no internal pull resistor on the classic ESP32 and needs an
  external pull-up on the board; GPIO0's internal pull-up is enabled in
  firmware as a fallback for the other button.

### Known limitations

- Single shared secret, provisioned as a flat file on both sides -- there is
  no per-device identity, enrollment flow, or revocation.
- The host daemon currently targets a GNOME/logind session on Linux
  specifically (`loginctl show-session`/`lock-session`/`unlock-session`).
- This is a convenience/physical-presence factor, not a replacement for
  full-disk encryption or a hardened second factor against an attacker who
  already has code execution as your user.
