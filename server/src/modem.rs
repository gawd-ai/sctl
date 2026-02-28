//! Native serial AT command interface for Quectel modems.
//!
//! Replaces the shell-based `at_command()` with direct serial I/O via a
//! dedicated `std::thread` that owns the fd. Callers send commands through
//! an `mpsc` channel and get responses via `oneshot` — no mutex, no shell
//! forks, proper termios (raw 115200 8N1, no echo), instant `tcflush`.

use std::os::fd::BorrowedFd;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use nix::fcntl::{self, OFlag};
use nix::sys::stat::Mode;
use nix::sys::termios::{self, SetArg, SpecialCharacterIndices};
use nix::unistd;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Default AT command timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read buffer size (AT responses are small).
const READ_BUF_SIZE: usize = 1024;

struct AtRequest {
    command: String,
    timeout: Duration,
    reply: oneshot::Sender<Result<String, String>>,
}

/// Helper to get a `BorrowedFd` from a `RawFd` for nix termios calls.
///
/// # Safety
/// The caller must ensure `fd` is a valid open file descriptor.
unsafe fn borrow_fd(fd: RawFd) -> BorrowedFd<'static> {
    BorrowedFd::borrow_raw(fd)
}

/// Cloneable handle to a modem serial port.
///
/// Internally holds an `mpsc::Sender` to the I/O thread — cloning just
/// clones the sender. All commands are serialized through the channel.
#[derive(Clone)]
pub struct Modem {
    tx: mpsc::Sender<AtRequest>,
    device: String,
}

impl Modem {
    /// Open a serial device path (e.g. `/dev/ttyUSB2`) and spawn the I/O thread.
    ///
    /// Returns `Err` if the device cannot be opened or termios configuration fails.
    pub fn open(device: &str) -> Result<Self, String> {
        let fd = fcntl::open(
            device,
            OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| format!("open {device}: {e}"))?;

        // Clear O_NONBLOCK now that we have the fd — we want blocking reads
        // with VTIME timeout in the I/O thread.
        let flags =
            fcntl::fcntl(fd, fcntl::FcntlArg::F_GETFL).map_err(|e| format!("F_GETFL: {e}"))?;
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.remove(OFlag::O_NONBLOCK);
        fcntl::fcntl(fd, fcntl::FcntlArg::F_SETFL(oflags)).map_err(|e| format!("F_SETFL: {e}"))?;

        configure_termios(fd)?;

        // Flush any stale data
        // SAFETY: fd is valid — we just opened it
        unsafe {
            termios::tcflush(borrow_fd(fd), termios::FlushArg::TCIOFLUSH)
                .map_err(|e| format!("tcflush: {e}"))?;
        }

        // NOTE: modem_init() (ATE0, echo disable) runs inside the I/O thread,
        // NOT here. It does blocking serial reads that would stall the tokio
        // runtime if the modem is unresponsive (e.g. after a USB reset).

        let (tx, rx) = mpsc::channel::<AtRequest>(32);
        let dev_name = device.to_string();

        std::thread::Builder::new()
            .name(format!("modem-{dev_name}"))
            .spawn(move || modem_thread(fd, rx, &dev_name))
            .map_err(|e| format!("spawn modem thread: {e}"))?;

        info!("Modem {device}: opened (115200 8N1), init on I/O thread");

        Ok(Self {
            tx,
            device: device.to_string(),
        })
    }

    /// Send an AT command with the default timeout (5s).
    pub async fn command(&self, cmd: &str) -> Result<String, String> {
        self.command_with_timeout(cmd, DEFAULT_TIMEOUT).await
    }

    /// Send an AT command with a custom timeout.
    pub async fn command_with_timeout(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = AtRequest {
            command: cmd.to_string(),
            timeout,
            reply: reply_tx,
        };

        self.tx
            .send(req)
            .await
            .map_err(|_| format!("modem {} I/O thread gone", self.device))?;

        reply_rx
            .await
            .map_err(|_| format!("modem {} reply channel dropped", self.device))?
    }

    /// Device path this modem is connected to.
    #[must_use]
    pub fn device(&self) -> &str {
        &self.device
    }
}

/// Configure termios: raw mode, 115200 baud, 8N1, no flow control.
/// VMIN=0, VTIME=1 → reads return after 100ms of silence.
fn configure_termios(fd: RawFd) -> Result<(), String> {
    // SAFETY: fd is valid — caller just opened it
    let borrowed = unsafe { borrow_fd(fd) };

    let mut tio = termios::tcgetattr(borrowed).map_err(|e| format!("tcgetattr: {e}"))?;

    termios::cfmakeraw(&mut tio);

    // 115200 baud
    termios::cfsetispeed(&mut tio, termios::BaudRate::B115200)
        .map_err(|e| format!("cfsetispeed: {e}"))?;
    termios::cfsetospeed(&mut tio, termios::BaudRate::B115200)
        .map_err(|e| format!("cfsetospeed: {e}"))?;

    // 8N1, CLOCAL (ignore modem control), CREAD (enable receiver)
    tio.control_flags |= termios::ControlFlags::CLOCAL | termios::ControlFlags::CREAD;
    tio.control_flags &= !termios::ControlFlags::CRTSCTS; // no hardware flow control

    // VMIN=0, VTIME=1 → read returns after 100ms idle or when data available
    tio.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    tio.control_chars[SpecialCharacterIndices::VTIME as usize] = 1;

    termios::tcsetattr(borrowed, SetArg::TCSANOW, &tio).map_err(|e| format!("tcsetattr: {e}"))?;

    Ok(())
}

/// Initialize modem: abort any partial command, disable echo.
fn modem_init(fd: RawFd) -> Result<(), String> {
    // SAFETY: fd is valid — caller just opened it. BorrowedFd is used only
    // within this function while fd remains open.
    let bfd = unsafe { borrow_fd(fd) };

    // Send bare CR to abort any partial command in the modem's input buffer
    unistd::write(bfd, b"\r").map_err(|e| format!("write CR: {e}"))?;

    // Small delay for modem to process the bare CR
    std::thread::sleep(Duration::from_millis(100));

    // Flush whatever the modem sent back
    termios::tcflush(bfd, termios::FlushArg::TCIOFLUSH)
        .map_err(|e| format!("tcflush after CR: {e}"))?;

    // Disable echo
    unistd::write(bfd, b"ATE0\r").map_err(|e| format!("write ATE0: {e}"))?;

    // Read ATE0 response (with short timeout)
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut response = String::new();
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match unistd::read(fd, &mut buf) {
            Ok(0) | Err(nix::errno::Errno::EAGAIN) => break,
            Ok(n) => {
                response.push_str(&String::from_utf8_lossy(&buf[..n]));
                if response.contains("OK") || response.contains("ERROR") {
                    break;
                }
            }
            Err(e) => return Err(format!("read ATE0 response: {e}")),
        }
    }

    debug!("Modem init ATE0 response: {:?}", response.trim());

    // Final flush — clear any trailing data before first real command
    termios::tcflush(bfd, termios::FlushArg::TCIOFLUSH)
        .map_err(|e| format!("tcflush final: {e}"))?;

    Ok(())
}

/// Blocking I/O thread: receives AT commands from the channel, executes them
/// on the serial fd, and sends back responses.
fn modem_thread(fd: RawFd, mut rx: mpsc::Receiver<AtRequest>, device: &str) {
    // Initialize modem (abort partial command, disable echo) on THIS thread
    // so it never blocks the tokio runtime even if the modem is unresponsive.
    match modem_init(fd) {
        Ok(()) => info!("Modem {device}: initialized (ATE0, echo disabled)"),
        Err(e) => warn!("Modem {device}: init failed ({e}), continuing anyway"),
    }

    while let Some(req) = rx.blocking_recv() {
        let result = execute_at(fd, &req.command, req.timeout);
        match &result {
            Ok(resp) => debug!(
                "Modem {device} AT {}: {:?}",
                req.command,
                if resp.len() > 80 { &resp[..80] } else { resp }
            ),
            Err(e) => warn!("Modem {device} AT {} failed: {e}", req.command),
        }
        let _ = req.reply.send(result);
    }

    // Channel closed — modem is being shut down
    debug!("Modem {device} I/O thread exiting");
    let _ = unistd::close(fd);
}

/// Execute a single AT command: flush → write → read until terminator.
fn execute_at(fd: RawFd, command: &str, timeout: Duration) -> Result<String, String> {
    // SAFETY: fd is valid — owned by the I/O thread for its entire lifetime
    let bfd = unsafe { borrow_fd(fd) };

    // Flush stale data
    termios::tcflush(bfd, termios::FlushArg::TCIOFLUSH).map_err(|e| format!("tcflush: {e}"))?;

    // Write command
    let cmd_bytes = format!("{command}\r");
    unistd::write(bfd, cmd_bytes.as_bytes()).map_err(|e| format!("write: {e}"))?;

    // Read response until OK/ERROR or timeout
    let mut buf = [0u8; READ_BUF_SIZE];
    let mut response = String::with_capacity(256);
    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "timeout after {:.1}s, partial: {}",
                timeout.as_secs_f64(),
                response.trim()
            ));
        }

        match unistd::read(fd, &mut buf) {
            Ok(0) => {
                // VTIME expired with no data
                if response_is_complete(&response) {
                    break;
                }
            }
            Ok(n) => {
                response.push_str(&String::from_utf8_lossy(&buf[..n]));
                if response_is_complete(&response) {
                    break;
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {
                if response_is_complete(&response) {
                    break;
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }

    let cleaned = sanitize_response(&response);
    Ok(strip_echo(&cleaned))
}

/// Check if the AT response contains a final result code.
fn response_is_complete(response: &str) -> bool {
    for line in response.lines() {
        let trimmed = line.trim();
        if trimmed == "OK"
            || trimmed == "ERROR"
            || trimmed.starts_with("+CME ERROR:")
            || trimmed.starts_with("+CMS ERROR:")
        {
            return true;
        }
    }
    false
}

/// Remove NUL bytes and non-printable control characters (except CR/LF) from
/// the modem response. Stale buffer data can contain garbage bytes that break
/// line-based parsing.
fn sanitize_response(response: &str) -> String {
    response
        .chars()
        .filter(|&c| c == '\r' || c == '\n' || !c.is_control())
        .filter(|&c| c != '\u{FFFD}') // replacement character from from_utf8_lossy
        .collect()
}

/// Strip AT echo lines from the response (safety net for echo not fully disabled).
fn strip_echo(response: &str) -> String {
    response
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            // Skip leading non-alpha chars (garbage from stale buffer) before
            // checking for "AT" echo prefix
            let alpha_start = trimmed.find(|c: char| c.is_ascii_alphabetic());
            if let Some(pos) = alpha_start {
                !trimmed[pos..].starts_with("AT")
            } else {
                // No alphabetic chars at all — keep the line
                true
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_is_complete_ok() {
        assert!(response_is_complete("+CSQ: 15,99\r\nOK\r\n"));
    }

    #[test]
    fn test_response_is_complete_error() {
        assert!(response_is_complete("ERROR\r\n"));
    }

    #[test]
    fn test_response_is_complete_cme_error() {
        assert!(response_is_complete("+CME ERROR: 516\r\n"));
    }

    #[test]
    fn test_response_is_complete_partial() {
        assert!(!response_is_complete("+CSQ: 15,99\r\n"));
    }

    #[test]
    fn test_strip_echo() {
        let response = "AT+CSQ\r\n+CSQ: 15,99\r\nOK\r\n";
        let stripped = strip_echo(response);
        assert!(stripped.contains("+CSQ: 15,99"));
        assert!(stripped.contains("OK"));
        assert!(!stripped.contains("AT+CSQ"));
    }

    #[test]
    fn test_strip_echo_no_echo() {
        let response = "+CSQ: 15,99\r\nOK\r\n";
        let stripped = strip_echo(response);
        assert!(stripped.contains("+CSQ: 15,99"));
        assert!(stripped.contains("OK"));
    }

    #[test]
    fn test_strip_echo_with_leading_garbage() {
        // Stale buffer byte before AT echo
        let response = "\x00AT+QGPSLOC=2\r\n+QGPSLOC: 153233.0,45.5,-73.5,1.2,47.0,3,270.5,0.0,0.0,260226,08\r\nOK\r\n";
        let cleaned = sanitize_response(response);
        let stripped = strip_echo(&cleaned);
        assert!(stripped.contains("+QGPSLOC:"));
        assert!(stripped.contains("OK"));
        assert!(!stripped.contains("AT+QGPSLOC"));
    }

    #[test]
    fn test_sanitize_response_removes_nul() {
        let response = "\x00AT+CSQ\r\n+CSQ: 15,99\r\nOK\r\n";
        let cleaned = sanitize_response(response);
        assert!(!cleaned.contains('\x00'));
        assert!(cleaned.contains("+CSQ: 15,99"));
    }

    #[test]
    fn test_sanitize_response_removes_replacement_char() {
        let response = "\u{FFFD}AT+CSQ\r\n+CSQ: 15,99\r\nOK\r\n";
        let cleaned = sanitize_response(response);
        assert!(!cleaned.contains('\u{FFFD}'));
        assert!(cleaned.starts_with("AT"));
    }
}
