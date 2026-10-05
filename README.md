# esp-unlock

A USB-attached ESP32 as a physical-presence key for your Linux desktop:
plug it in and press the button to unlock your GNOME session; unplug it
and the session locks. Your account password keeps working exactly as
before and is never affected by any of this.

```
  unplug the key  ──────────────────────────────▶  session locks
  plug it back in + press the button on it  ────▶  session unlocks
  type your password, any time, key or no key ───▶  session unlocks
```

## How it works

Two programs talk to each other over a USB-serial link, using a tiny
challenge/response protocol defined once in the [`protocol`](protocol)
crate and shared by both sides:

```
   host (esp-unlock-daemon)                    device (ESP32 firmware)
   ────────────────────────                    ───────────────────────
   session locked & key present
          │
          ├── "CH" + 32 random bytes ─────────▶
          │                                     waits up to 15s for a
          │                                     physical button press
          │                                     (release-then-press;
          │                                     a held-down button
          │                                     does not count)
          │
          │          button pressed in time?
          │        ┌───────────────┴───────────────┐
          │       yes                               no
          │        │                                 │
          │   "ESPK" + HMAC-SHA256(secret, challenge) │
          │   ◀─────────────────────────────────    "ESPN"
          │        │                                 │
          │   verify tag over the challenge          loop: send a
          │   it just sent (prevents replay)         new challenge
          │        │
          │   loginctl unlock-session
          ▼
     session unlocked
```

The host daemon separately watches:

- **the serial device's presence** -- unplugging it while the session is
  unlocked triggers `loginctl lock-session` right away;
- **the session's `LockedHint`** (via `loginctl show-session`) -- as soon as
  it's locked and the key is present, the daemon starts offering challenges;
  as soon as it reads back "unlocked" (whether that happened through the key
  or through someone just typing their password on the lock screen) it goes
  back to idling.

The host never tells GNOME/logind *how* the session got unlocked, and it
never disables or wraps the normal password prompt -- it only calls the same
`loginctl lock-session`/`unlock-session` commands a user or another tool
could call by hand. This is a convenience/physical-presence factor layered
on top of your existing login, not a replacement for it; see
[Security notes](#security-notes) below.

### Wire protocol, byte for byte

| Direction       | Bytes                                                  |
|------------------|---------------------------------------------------------|
| host -> device   | `"CH"` + 32-byte random challenge (34 bytes total)      |
| device -> host   | `"ESPK"` + 32-byte `HMAC-SHA256(secret, challenge)`, **or** `"ESPN"` |

- The challenge is fresh random bytes on every round, so a captured
  response can't be replayed for a later unlock.
- The device only computes and sends the tag *after* a button press is
  confirmed; the host only unlocks after verifying that tag against the
  challenge it just sent.
- Full byte-level details and the frame-resync state machine live in
  [`protocol/src/lib.rs`](protocol/src/lib.rs), with unit tests covering
  both the framing and the HMAC logic.

## Hardware

Any classic ESP32 dev board works; this was built and tested against:

| Component        | What was used                                              |
|-------------------|-------------------------------------------------------------|
| MCU               | ESP32 (original, dual-core Xtensa LX6, silicon rev v1.1), 240 MHz, 16 MB flash |
| USB-serial bridge | QinHeng CH9102/CH340-family (USB VID:PID `1a86:55d4`), shows up as `/dev/ttyACM0` on Linux |
| Confirmation      | Two push buttons wired to GPIO35 and GPIO0 (either confirms) |
| Status indicator  | An LED/backlight on GPIO4, lit while waiting for a button press |

Pin wiring used by [`firmware/src/main.rs`](firmware/src/main.rs):

| Signal              | GPIO   | Notes                                                              |
|---------------------|--------|----------------------------------------------------------------------|
| UART0 TX            | GPIO1  | Bound explicitly via `.with_tx()`; also the chip's flashing/console pin |
| UART0 RX            | GPIO3  | Bound explicitly via `.with_rx()` -- see [Hardware notes](#hardware-notes-from-bring-up) |
| Button A            | GPIO35 | Input-only pin on the classic ESP32: **needs an external pull-up on the board**, the chip has none for this pin |
| Button B            | GPIO0  | Also the boot-mode strapping pin used by the auto-reset/flashing circuit; firmware enables the internal pull-up as a fallback |
| Status LED/backlight| GPIO4  | Driven high while the device is waiting for a confirmation          |

Any other classic ESP32 board can be used by adjusting these pin numbers in
`firmware/src/main.rs` to match your wiring.

## Repository layout

```
protocol/   Shared, no_std-compatible wire protocol + HMAC helpers (tested)
firmware/   ESP32 firmware (esp-hal, no_std, targets xtensa-esp32-none-elf)
host/       Host daemon (esp-unlock-daemon) + its test-challenge diagnostic
contrib/    Example systemd user service unit
```

## Building and flashing the firmware

Prerequisites: the `esp` Rust toolchain (via [espup](https://github.com/esp-rs/espup))
and [`espflash`](https://github.com/esp-rs/espflash).

```sh
# One-time: provision the shared secret (32 random bytes), kept out of git.
head -c 32 /dev/urandom > firmware/secret.bin

cd firmware
# The xtensa toolchain's own linker/gcc need to be on PATH; espup prints the
# exact path for your install, typically something like:
export PATH="$HOME/.rustup/toolchains/esp/xtensa-esp-elf/esp-*/xtensa-esp-elf/bin:$PATH"

cargo build --release
espflash flash target/xtensa-esp32-none-elf/release/esp-unlock
```

The same `firmware/secret.bin` must be readable by the host daemon (see
`ESP_UNLOCK_SECRET_PATH` below) -- it's the shared HMAC key for both sides.

## Running the host daemon

```sh
cd host
cargo build --release

# One-off bench test: sends one challenge and waits for a button press,
# without touching the session lock state at all.
ESP_UNLOCK_SECRET_PATH=../firmware/secret.bin ./target/release/esp-unlock-daemon test-challenge
```

Environment variables:

| Variable                  | Default                        | Purpose                                   |
|----------------------------|---------------------------------|---------------------------------------------|
| `ESP_UNLOCK_SECRET_PATH`   | `/etc/esp-unlock/secret.bin`    | Path to the shared secret file              |
| `ESP_UNLOCK_DEVICE_PATH`   | `/dev/ttyACM0`                  | Serial device path for the key              |

### Installing as a systemd user service

Copy [`contrib/systemd/esp-unlock.service`](contrib/systemd/esp-unlock.service)
to `~/.config/systemd/user/esp-unlock.service`, edit the two paths in it for
your setup, then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now esp-unlock.service
```

It restarts automatically on crash and starts on every graphical login.

## Testing

```sh
cd protocol && cargo test   # frame assembly/resync, HMAC verify, known-answer vector
cd host      && cargo test  # loginctl output parsing (session id, LockedHint)
```

The firmware crate (`firmware/`) isn't unit-tested directly: it's `no_std`,
targets `xtensa-esp32-none-elf`, and its logic is almost entirely either
thin hardware I/O or the shared protocol code above, which *is* tested on
the host target. The hardware-dependent parts (GPIO debounce timing, UART
reset behavior, `loginctl` integration) were validated by hand against real
hardware during development; see [`RELEASE_NOTES.md`](RELEASE_NOTES.md) for
what that bring-up turned up.

## Hardware notes from bring-up

A few non-obvious issues came up getting this running on real hardware,
left here in case they save someone else the debugging session:

- **`espflash` refuses to flash without an app descriptor.** esp-hal
  applications need `esp_bootloader_esp_idf::esp_app_desc!()` (already
  present in `firmware/src/main.rs`); if you still hit this, `espflash flash
  --ignore-app-descriptor` skips the check.
- **The chip reboots itself after a few seconds of normal operation.** The
  bootloader leaves the RTC watchdog running, and a bare `no_std` main loop
  never feeds it. Fix: `Rtc::new(peripherals.LPWR).rwdt.disable()` early in
  `main()`.
- **The device can transmit but never receives anything.** `Uart::new(...)`
  alone does not bind UART0 to its pins on this esp-hal version; without
  `.with_rx(peripherals.GPIO3).with_tx(peripherals.GPIO1)`, outgoing bytes
  work fine (useful for debugging -- you'll see output) but incoming bytes
  are silently dropped, and `uart.read()` blocks forever waiting for a byte
  that will never arrive.
- **GPIO35 has no internal pull resistor.** It's an input-only pin on the
  classic ESP32 and the SoC simply doesn't wire one up for it; it needs an
  external pull-up on the board, or it floats and reads as pressed at
  random. GPIO0 does support an internal pull-up (`Pull::Up`), used here as
  a fallback for the other button -- but note GPIO0 is also the boot-mode
  strapping pin driven by most boards' auto-reset/flashing circuit.
- **Opening the serial port resets the device.** Most ESP32 boards wire
  DTR/RTS to EN/GPIO0 for auto-reset during flashing; the host daemon keeps
  the port open across challenge rounds rather than reopening it every time,
  and waits briefly after opening for the boot log to finish before sending
  anything (see `DEVICE_BOOT_SETTLE` in `host/src/main.rs`).

## Security notes

- This is a **physical-presence convenience factor**, not a hardened
  authentication system. The shared secret is a flat 32-byte file on both
  sides; anyone who can read it (e.g. root, or anyone with filesystem access
  as the account that ran the daemon) can forge responses.
- The random challenge prevents a captured response from being replayed for
  a later unlock, but does not protect against someone physically pressing
  the button on your key while they have access to your unlocked computer.
- The host daemon never disables, wraps, or intercepts the normal password
  prompt -- password login keeps working identically whether or not this
  daemon is running, so there is no lockout risk from this project.
- There is no per-device identity, multi-key enrollment, or revocation: it's
  one shared secret for one key and one machine.

## License

Dual-licensed under either of:

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
