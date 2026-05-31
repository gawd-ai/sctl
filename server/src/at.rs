//! Generic serial AT command transport owned by `sctl`.
//!
//! Comms plugins know vendor command semantics, but `sctl` owns the serial file
//! descriptor, termios setup, command serialization, and timeout handling.

use std::os::fd::BorrowedFd;
use std::os::unix::io::RawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::fcntl::{self, OFlag};
use nix::sys::stat::Mode;
use nix::sys::termios::{self, SetArg, SpecialCharacterIndices};
use nix::unistd;
use tracing::{debug, info, warn};

const READ_BUF_SIZE: usize = 1024;

struct AtRequest {
    command: String,
    timeout: Duration,
    reply: mpsc::Sender<Result<String, String>>,
}

/// Cloneable handle to one AT serial port.
#[derive(Clone)]
pub struct AtPort {
    tx: mpsc::Sender<AtRequest>,
    device: String,
}

impl AtPort {
    /// Open a serial AT device path and spawn the blocking I/O owner thread.
    pub fn open(device: &str) -> Result<Self, String> {
        let fd = fcntl::open(
            device,
            OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| format!("open {device}: {e}"))?;

        let flags =
            fcntl::fcntl(fd, fcntl::FcntlArg::F_GETFL).map_err(|e| format!("F_GETFL: {e}"))?;
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.remove(OFlag::O_NONBLOCK);
        fcntl::fcntl(fd, fcntl::FcntlArg::F_SETFL(oflags)).map_err(|e| format!("F_SETFL: {e}"))?;

        configure_termios(fd)?;
        // SAFETY: fd is valid because it was just opened.
        unsafe {
            termios::tcflush(borrow_fd(fd), termios::FlushArg::TCIOFLUSH)
                .map_err(|e| format!("tcflush: {e}"))?;
        }

        let (tx, rx) = mpsc::channel::<AtRequest>();
        let dev_name = device.to_string();
        std::thread::Builder::new()
            .name(format!("sctl-at-{dev_name}"))
            .spawn(move || at_thread(fd, rx, &dev_name))
            .map_err(|e| format!("spawn AT thread: {e}"))?;

        info!("AT port {device}: opened (115200 8N1)");
        Ok(Self {
            tx,
            device: device.to_string(),
        })
    }

    pub fn command_blocking(&self, cmd: &str, timeout: Duration) -> Result<String, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(AtRequest {
                command: cmd.to_string(),
                timeout,
                reply: reply_tx,
            })
            .map_err(|_| format!("AT port {} I/O thread gone", self.device))?;
        reply_rx
            .recv_timeout(timeout + Duration::from_secs(1))
            .map_err(|_| format!("AT port {} reply timeout", self.device))?
    }

    #[must_use]
    pub fn device(&self) -> &str {
        &self.device
    }
}

unsafe fn borrow_fd(fd: RawFd) -> BorrowedFd<'static> {
    BorrowedFd::borrow_raw(fd)
}

fn configure_termios(fd: RawFd) -> Result<(), String> {
    // SAFETY: fd is valid while the caller owns the open descriptor.
    let borrowed = unsafe { borrow_fd(fd) };
    let mut tio = termios::tcgetattr(borrowed).map_err(|e| format!("tcgetattr: {e}"))?;
    termios::cfmakeraw(&mut tio);
    termios::cfsetispeed(&mut tio, termios::BaudRate::B115200)
        .map_err(|e| format!("cfsetispeed: {e}"))?;
    termios::cfsetospeed(&mut tio, termios::BaudRate::B115200)
        .map_err(|e| format!("cfsetospeed: {e}"))?;
    tio.control_flags |= termios::ControlFlags::CLOCAL | termios::ControlFlags::CREAD;
    tio.control_flags &= !termios::ControlFlags::CRTSCTS;
    tio.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    tio.control_chars[SpecialCharacterIndices::VTIME as usize] = 1;
    termios::tcsetattr(borrowed, SetArg::TCSANOW, &tio).map_err(|e| format!("tcsetattr: {e}"))?;
    Ok(())
}

// Takes the receiver by value on purpose: this thread is its final owner, so
// the channel closes when the thread exits.
#[allow(clippy::needless_pass_by_value)]
fn at_thread(fd: RawFd, rx: mpsc::Receiver<AtRequest>, device: &str) {
    match at_init(fd) {
        Ok(()) => info!("AT port {device}: initialized (ATE0)"),
        Err(e) => warn!("AT port {device}: init failed ({e}), continuing"),
    }

    while let Ok(req) = rx.recv() {
        let result = execute_at(fd, &req.command, req.timeout);
        match &result {
            Ok(resp) => debug!(
                "AT {device} {}: {:?}",
                req.command,
                if resp.len() > 80 { &resp[..80] } else { resp }
            ),
            Err(e) => warn!("AT {device} {} failed: {e}", req.command),
        }
        let _ = req.reply.send(result);
    }

    debug!("AT port {device}: I/O thread exiting");
    let _ = unistd::close(fd);
}

fn at_init(fd: RawFd) -> Result<(), String> {
    // SAFETY: fd is owned by the AT thread.
    let bfd = unsafe { borrow_fd(fd) };
    unistd::write(bfd, b"\r").map_err(|e| format!("write CR: {e}"))?;
    std::thread::sleep(Duration::from_millis(100));
    termios::tcflush(bfd, termios::FlushArg::TCIOFLUSH)
        .map_err(|e| format!("tcflush after CR: {e}"))?;
    unistd::write(bfd, b"ATE0\r").map_err(|e| format!("write ATE0: {e}"))?;

    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut response = String::new();
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match read_fd(fd, &mut buf) {
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
    debug!("AT init response: {:?}", response.trim());
    termios::tcflush(bfd, termios::FlushArg::TCIOFLUSH)
        .map_err(|e| format!("tcflush final: {e}"))?;
    Ok(())
}

fn execute_at(fd: RawFd, command: &str, timeout: Duration) -> Result<String, String> {
    // SAFETY: fd is owned by the AT thread.
    let bfd = unsafe { borrow_fd(fd) };
    termios::tcflush(bfd, termios::FlushArg::TCIOFLUSH).map_err(|e| format!("tcflush: {e}"))?;
    let cmd_bytes = format!("{command}\r");
    unistd::write(bfd, cmd_bytes.as_bytes()).map_err(|e| format!("write: {e}"))?;

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

        match read_fd(fd, &mut buf) {
            Ok(0) | Err(nix::errno::Errno::EAGAIN) => {
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
            Err(e) => return Err(format!("read: {e}")),
        }
    }

    let cleaned = sanitize_response(&response);
    Ok(strip_echo(&cleaned))
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize, nix::errno::Errno> {
    unistd::read(fd, buf)
}

fn response_is_complete(response: &str) -> bool {
    response.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "OK"
            || trimmed == "ERROR"
            || trimmed.starts_with("+CME ERROR:")
            || trimmed.starts_with("+CMS ERROR:")
    })
}

fn sanitize_response(response: &str) -> String {
    response
        .chars()
        .filter(|&c| c == '\r' || c == '\n' || !c.is_control())
        .filter(|&c| c != '\u{FFFD}')
        .collect()
}

fn strip_echo(response: &str) -> String {
    response
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            let alpha_start = trimmed.find(|c: char| c.is_ascii_alphabetic());
            alpha_start.is_none_or(|pos| !trimmed[pos..].starts_with("AT"))
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_response_completion_detects_final_codes() {
        assert!(response_is_complete("+CSQ: 15,99\r\nOK\r\n"));
        assert!(response_is_complete("+CME ERROR: 516\r\n"));
        assert!(!response_is_complete("+CSQ: 15,99\r\n"));
    }

    #[test]
    fn sanitizer_strips_echo_and_nul() {
        let cleaned = sanitize_response("\0AT+CSQ\r\n+CSQ: 15,99\r\nOK\r\n");
        let stripped = strip_echo(&cleaned);
        assert!(!stripped.contains('\0'));
        assert!(!stripped.contains("AT+CSQ"));
        assert!(stripped.contains("+CSQ: 15,99"));
    }
}
