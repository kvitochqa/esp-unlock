//! esp-unlock firmware.
//!
//! Runs on a classic ESP32 dev board acting as a physical-presence USB
//! unlock key. See `esp_unlock_protocol` for the wire format; in short:
//!
//!   host -> device: `"CH"` + 32-byte random challenge
//!   device -> host: `"ESPK"` + HMAC-SHA256(secret, challenge), but only
//!                   after a physical button press within 15 seconds;
//!                   otherwise `"ESPN"`.
//!
//! See the top-level README for the full hardware/wiring description and
//! the gotchas this firmware works around (RTC watchdog, explicit UART0
//! pin binding, the `esp_app_desc!()` requirement).
#![no_std]
#![no_main]

use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::main;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::time::{Duration, Instant};
use esp_hal::uart::{Config as UartConfig, Uart};
use esp_unlock_protocol::{FrameReader, RESPONSE_CONFIRMED, RESPONSE_DENIED, compute_tag};

esp_app_desc!();

// Shared secret, baked in at build time from firmware/secret.bin (gitignored,
// provisioned separately -- see README).
static SECRET: &[u8; 32] = include_bytes!("../secret.bin");

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(esp_unlock_protocol::CONFIRM_TIMEOUT_SECS);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // The bootloader leaves the RTC watchdog enabled; our main loop never
    // feeds it, so without disabling it explicitly the chip reboots itself a
    // few seconds into normal operation.
    Rtc::new(peripherals.LPWR).rwdt.disable();

    // UART0's RX/TX pins must be bound explicitly here: esp-hal does not
    // assume the chip's default IO_MUX pin function for them, and without
    // this the device can transmit (e.g. during flashing) but never
    // actually receives anything the host sends.
    let mut uart = Uart::new(peripherals.UART0, UartConfig::default().with_baudrate(115_200))
        .unwrap()
        .with_rx(peripherals.GPIO3)
        .with_tx(peripherals.GPIO1);

    // The display backlight doubles as a "waiting for confirmation" LED.
    let mut backlight = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());

    // GPIO35 is an input-only pin on the classic ESP32 and has no internal
    // pull resistor, so an external pull-up is required on the board for
    // this button. GPIO0 does support an internal pull-up; enable it in case
    // the board doesn't provide an external one for this line.
    let button_a = Input::new(peripherals.GPIO35, InputConfig::default());
    let button_b = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let pressed = || button_a.is_low() || button_b.is_low();

    let mut reader = FrameReader::new();
    let mut byte = [0u8; 1];

    loop {
        if !matches!(uart.read(&mut byte), Ok(1)) {
            continue;
        }
        let Some(challenge) = reader.push(byte[0]) else {
            continue;
        };

        backlight.set_high();
        let confirmed = wait_confirm(&pressed);
        backlight.set_low();

        if confirmed {
            let tag = compute_tag(SECRET, &challenge);
            let _ = uart.write(&RESPONSE_CONFIRMED);
            let _ = uart.write(&tag);
        } else {
            let _ = uart.write(&RESPONSE_DENIED);
        }
        let _ = uart.flush();
    }
}

/// Waits for a physical button press, requiring a release-then-press
/// transition so a button stuck or taped down does not count as a
/// confirmation.
fn wait_confirm(pressed: &impl Fn() -> bool) -> bool {
    let start = Instant::now();

    // If the button happens to already be held down, wait for it to be
    // released first.
    while pressed() {
        if start.elapsed() > CONFIRM_TIMEOUT {
            return false;
        }
    }

    while start.elapsed() < CONFIRM_TIMEOUT {
        if pressed() {
            // Debounce: require the press to still read as pressed after a
            // short settle delay.
            let debounce_start = Instant::now();
            while debounce_start.elapsed() < Duration::from_millis(30) {
                if !pressed() {
                    break;
                }
            }
            if pressed() {
                return true;
            }
        }
    }
    false
}
