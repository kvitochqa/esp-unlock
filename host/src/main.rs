//! esp-unlock-daemon -- watches a physical key (ESP32 over UART) and syncs
//! it with the GNOME session lock state through logind:
//!
//!   - key unplugged while the session is unlocked -> `loginctl lock-session`
//!     immediately.
//!   - session locked while the key is present -> repeatedly send a
//!     challenge and wait for a button press on the device; on a valid
//!     HMAC response -> `loginctl unlock-session`.
//!   - typing the account password on the lock screen works exactly as
//!     normal and does not depend on this daemon at all: we only read
//!     `LockedHint` and react once it flips back to "no".
//!
//! See `esp_unlock_protocol` for the wire format shared with the firmware.
//!
//! The serial port is kept open across challenge rounds: opening a CH340
//! port toggles DTR/RTS and resets the ESP32, so reopening it on every
//! single challenge would mean the device never gets past the bootloader's
//! boot log before the next write.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use esp_unlock_protocol::{CHALLENGE_LEN, REQUEST_PREFIX, RESPONSE_CONFIRMED, RESPONSE_DENIED, TAG_LEN, verify_tag};
use rand::RngCore;
use serialport::SerialPort;

const DEFAULT_DEVICE_PATH: &str = "/dev/ttyACM0";
const BAUD_RATE: u32 = 115_200;
// A little longer than the firmware's own 15s confirmation timeout, so we
// have time to read a late "ESPN" instead of timing out ourselves first.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(16);
// Time to let the ESP32 finish rebooting (CH340 auto-reset on port open) and
// print its bootloader boot log before we send it a challenge.
const DEVICE_BOOT_SETTLE: Duration = Duration::from_millis(1500);
const POLL_IDLE: Duration = Duration::from_millis(1000);
const FAIL_BACKOFF: Duration = Duration::from_millis(300);

// Default location for the shared secret when `ESP_UNLOCK_SECRET_PATH` isn't
// set. During development it's usually more convenient to point the env var
// at `firmware/secret.bin` in the repo instead.
const DEFAULT_SECRET_PATH: &str = "/etc/esp-unlock/secret.bin";

fn secret_path() -> String {
    std::env::var("ESP_UNLOCK_SECRET_PATH").unwrap_or_else(|_| DEFAULT_SECRET_PATH.to_string())
}

/// Serial device path. Override with `ESP_UNLOCK_DEVICE_PATH` if the key
/// doesn't show up as `/dev/ttyACM0` (e.g. a different USB-serial chip, or
/// multiple serial devices plugged in -- see the README for setting up a
/// stable udev symlink).
fn device_path() -> String {
    std::env::var("ESP_UNLOCK_DEVICE_PATH").unwrap_or_else(|_| DEFAULT_DEVICE_PATH.to_string())
}

fn load_secret() -> Vec<u8> {
    let path = secret_path();
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read secret {path}: {e}"))
}

/// Picks the logind session id for `user` out of `loginctl list-sessions
/// --no-legend` output, preferring one marked "active".
fn parse_session_id(list_sessions_output: &str, user: &str) -> Option<String> {
    let mut fallback = None;
    for line in list_sessions_output.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 || cols[2] != user {
            continue;
        }
        if cols[5] == "active" {
            return Some(cols[0].to_string());
        }
        fallback.get_or_insert_with(|| cols[0].to_string());
    }
    fallback
}

/// Interprets `loginctl show-session <id> -p LockedHint --value` output.
fn parse_locked_hint(show_session_output: &str) -> bool {
    show_session_output.trim() == "yes"
}

/// Finds the logind session id for the current user (preferring an active
/// one), retrying with `--no-legend` output parsed by [`parse_session_id`].
fn find_session_id() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_session_id(&text, &user)
}

fn is_locked(session: &str) -> bool {
    Command::new("loginctl")
        .args(["show-session", session, "-p", "LockedHint", "--value"])
        .output()
        .map(|o| parse_locked_hint(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(false)
}

fn lock_session(session: &str) {
    let _ = Command::new("loginctl")
        .args(["lock-session", session])
        .status();
}

fn unlock_session(session: &str) {
    let _ = Command::new("loginctl")
        .args(["unlock-session", session])
        .status();
}

fn device_present(device_path: &str) -> bool {
    Path::new(device_path).exists()
}

/// Opens the port and waits for the ESP32 to finish rebooting and printing
/// its bootloader boot log, then discards everything that accumulated in
/// the buffer during that window.
fn open_device(device_path: &str) -> Option<Box<dyn SerialPort>> {
    let port = match serialport::new(device_path, BAUD_RATE)
        .timeout(DEVICE_TIMEOUT)
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("esp-unlock: failed to open {device_path}: {e}");
            return None;
        }
    };
    std::thread::sleep(DEVICE_BOOT_SETTLE);
    let _ = port.clear(serialport::ClearBuffer::All);
    Some(port)
}

enum Outcome {
    /// The device confirmed a button press and sent back a valid HMAC tag.
    Confirmed,
    /// The device replied with a denial (ESPN), or a valid ESPK header with
    /// a tag that doesn't check out.
    Denied,
    /// A port- or protocol-level problem; the connection should be reopened.
    Error,
}

/// Runs one challenge/response round over an already-open port. Blocks for
/// up to `~DEVICE_TIMEOUT` waiting for a button press on the device.
fn try_unlock(secret: &[u8], port: &mut dyn SerialPort) -> Outcome {
    let mut challenge = [0u8; CHALLENGE_LEN];
    rand::thread_rng().fill_bytes(&mut challenge);

    let mut frame = Vec::with_capacity(REQUEST_PREFIX.len() + CHALLENGE_LEN);
    frame.extend_from_slice(&REQUEST_PREFIX);
    frame.extend_from_slice(&challenge);
    if let Err(e) = port.write_all(&frame) {
        eprintln!("esp-unlock: write error: {e}");
        return Outcome::Error;
    }
    let _ = port.flush();

    let mut header = [0u8; 4];
    if let Err(e) = port.read_exact(&mut header) {
        // Timeout, or the device dropped out while we waited for a press.
        eprintln!("esp-unlock: timed out waiting for a response header: {e}");
        return Outcome::Error;
    }
    if header == RESPONSE_DENIED {
        return Outcome::Denied;
    }
    if header != RESPONSE_CONFIRMED {
        eprintln!("esp-unlock: unexpected response header: {header:?}");
        return Outcome::Error;
    }

    let mut tag = [0u8; TAG_LEN];
    if port.read_exact(&mut tag).is_err() {
        return Outcome::Error;
    }

    if verify_tag(secret, &challenge, &tag) {
        Outcome::Confirmed
    } else {
        eprintln!("esp-unlock: got ESPK with an invalid HMAC tag");
        Outcome::Denied
    }
}

fn main() {
    let secret = load_secret();
    let device_path = device_path();

    if std::env::args().any(|a| a == "test-challenge") {
        // Diagnostic: a single challenge/response round, without touching
        // the session lock state at all.
        let Some(mut port) = open_device(&device_path) else {
            println!("FAIL: could not open {device_path}");
            return;
        };
        eprintln!(
            "esp-unlock: sending a challenge, press the button on the device (within {DEVICE_TIMEOUT:?})..."
        );
        match try_unlock(&secret, port.as_mut()) {
            Outcome::Confirmed => println!("OK: device confirmed the button press, HMAC is valid"),
            Outcome::Denied => println!("FAIL: denied (ESPN) or invalid HMAC tag"),
            Outcome::Error => println!("FAIL: port/protocol error, see the log above"),
        }
        return;
    }

    let session = loop {
        match find_session_id() {
            Some(s) => break s,
            None => {
                eprintln!("esp-unlock: no logind session found yet, retrying in {POLL_IDLE:?}");
                std::thread::sleep(POLL_IDLE);
            }
        }
    };
    eprintln!("esp-unlock: watching session {session}, device {device_path}");

    let mut device_was_present = device_present(&device_path);
    let mut port: Option<Box<dyn SerialPort>> = None;

    loop {
        let present = device_present(&device_path);
        // Lock only on the unplug event itself, not on every tick while the
        // key is absent -- otherwise unlocking with the password while the
        // key is unplugged would immediately get locked right back.
        let just_unplugged = device_was_present && !present;
        if present != device_was_present {
            eprintln!(
                "esp-unlock: device {}",
                if present { "connected" } else { "disconnected" }
            );
            device_was_present = present;
        }

        if !present {
            port = None;
            if just_unplugged && !is_locked(&session) {
                eprintln!("esp-unlock: key disconnected -- locking the session");
                lock_session(&session);
            }
            std::thread::sleep(POLL_IDLE);
            continue;
        }

        if !is_locked(&session) {
            // Already unlocked (e.g. via password) -- just wait. Keep the
            // port open so we don't needlessly reset the device.
            std::thread::sleep(POLL_IDLE);
            continue;
        }

        if port.is_none() {
            port = open_device(&device_path);
            if port.is_none() {
                std::thread::sleep(FAIL_BACKOFF);
                continue;
            }
        }

        // Locked, with the key present: try a challenge/response round. This
        // blocks the thread while the device waits for a button press
        // (up to ~16s).
        match try_unlock(&secret, port.as_mut().unwrap().as_mut()) {
            Outcome::Confirmed => {
                eprintln!("esp-unlock: confirmed by button press -- unlocking the session");
                unlock_session(&session);
                std::thread::sleep(POLL_IDLE);
            }
            Outcome::Denied => {
                // ESPN / wrong tag: the firmware already waited out its own
                // ~15s timeout, so just try again right away.
            }
            Outcome::Error => {
                port = None;
                std::thread::sleep(FAIL_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_id_prefers_active_session() {
        let output = "\
7      1000 alice    seat0 tty2  closing  yes  -
32     1000 alice    seat0 tty2  active   no   -
";
        assert_eq!(parse_session_id(output, "alice"), Some("32".to_string()));
    }

    #[test]
    fn parse_session_id_falls_back_to_first_match_if_none_active() {
        let output = "\
5      1000 alice    seat0 tty1  closing  yes  -
9      1000 alice    -     -     online   no   -
";
        assert_eq!(parse_session_id(output, "alice"), Some("5".to_string()));
    }

    #[test]
    fn parse_session_id_ignores_other_users() {
        let output = "\
1      0    root     -     -     active   no   -
32     1000 alice    seat0 tty2  active   no   -
";
        assert_eq!(parse_session_id(output, "alice"), Some("32".to_string()));
    }

    #[test]
    fn parse_session_id_returns_none_when_no_match() {
        let output = "1      0    root     -     -     active   no   -\n";
        assert_eq!(parse_session_id(output, "alice"), None);
    }

    #[test]
    fn parse_session_id_handles_empty_output() {
        assert_eq!(parse_session_id("", "alice"), None);
    }

    #[test]
    fn parse_locked_hint_recognizes_yes() {
        assert!(parse_locked_hint("yes\n"));
    }

    #[test]
    fn parse_locked_hint_recognizes_no() {
        assert!(!parse_locked_hint("no\n"));
    }

    #[test]
    fn parse_locked_hint_defaults_to_unlocked_on_garbage() {
        assert!(!parse_locked_hint(""));
        assert!(!parse_locked_hint("unexpected output"));
    }
}
