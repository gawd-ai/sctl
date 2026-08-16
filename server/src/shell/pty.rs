//! PTY allocation, shell spawning, and terminal resize.
//!
//! Uses the `nix` crate for POSIX PTY APIs. The PTY master fd is kept alive for
//! the session lifetime so I/O and resize operations can be performed on it.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Stdio;

use nix::pty::{openpty, OpenptyResult, Winsize};
use nix::sys::termios::{self, LocalFlags, SetArg, SpecialCharacterIndices};
use tokio::process::{Child, Command};

/// An allocated PTY pair (master + slave).
pub struct PtyPair {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

/// Allocate a PTY pair with the given terminal size.
pub fn allocate_pty(rows: u16, cols: u16) -> Result<PtyPair, nix::Error> {
    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let OpenptyResult { master, slave } = openpty(&winsize, None)?;

    // Take the slave out of canonical mode. The kernel line discipline
    // assembles canonical input line-by-line and silently discards
    // everything past its per-line limit (MAX_CANON — observed as ~255
    // bytes on the fleet's busybox units), which truncated any long paste
    // or programmatic exec into an interactive session. Full-featured
    // shells re-enter raw mode for their own line editing and never notice;
    // the limit bit only the sessions that stayed canonical.
    //
    // Deliberately NOT cfmakeraw: ISIG stays so ^C still interrupts a
    // runaway foreground command, ECHO stays for programs that rely on
    // kernel echo, and OPOST stays or every output line would staircase.
    let mut t = termios::tcgetattr(&slave)?;
    t.local_flags.remove(LocalFlags::ICANON);
    t.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
    t.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
    termios::tcsetattr(&slave, SetArg::TCSANOW, &t)?;

    Ok(PtyPair { master, slave })
}

/// Spawn a shell on the slave side of the PTY.
///
/// The child becomes a session leader with the PTY slave as its controlling
/// terminal. stdin/stdout/stderr are all connected to the slave fd.
pub fn spawn_shell_pty(
    pty: &PtyPair,
    shell: &str,
    working_dir: &str,
    env: Option<&HashMap<String, String>>,
) -> std::io::Result<Child> {
    let slave_fd = pty.slave.as_raw_fd();
    let mut cmd = Command::new(shell);
    // Start as login shell so rc files (.zshrc, .bashrc, .profile, etc.) are sourced.
    // This matches the behaviour of standard terminal emulators.
    cmd.arg("-l");
    cmd.current_dir(working_dir).kill_on_drop(true);

    // The child's stdio is handled by pre_exec (dup2 to PTY slave), so tell
    // tokio not to set up pipes.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(vars) = env {
        cmd.envs(vars);
    }

    // SAFETY: All syscalls used here are async-signal-safe per POSIX.
    unsafe {
        cmd.pre_exec(move || {
            // Create a new session so the child is the session leader
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Set the PTY slave as the controlling terminal
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Redirect stdin/stdout/stderr to the PTY slave
            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            Ok(())
        });
    }

    cmd.spawn()
}

/// Resize a PTY's terminal window.
pub fn resize_pty(master: &OwnedFd, rows: u16, cols: u16) -> Result<(), nix::Error> {
    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ is a well-defined ioctl that writes a Winsize struct.
    let ret = unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCSWINSZ,
            std::ptr::addr_of!(winsize),
        )
    };
    if ret == -1 {
        Err(nix::Error::last())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::sys::termios::{self, LocalFlags, OutputFlags};

    use super::allocate_pty;

    /// The regression this module exists to prevent: canonical mode held
    /// input in a per-line kernel buffer with a hard cap, so a long paste or
    /// programmatic exec silently lost its tail. With ICANON off, bytes
    /// written to the master must all emerge from the slave — no newline
    /// required, no length cap at the line-discipline level.
    #[test]
    fn long_input_flows_through_without_a_newline() {
        let pty = allocate_pty(24, 80).expect("openpty");
        // Comfortably past the historical MAX_CANON (~255) while staying
        // under the tty input queue size, so the master write cannot block.
        let payload = vec![b'x'; 2000];

        let mut written = 0;
        while written < payload.len() {
            written += nix::unistd::write(&pty.master, &payload[written..]).expect("write master");
        }

        fcntl(pty.slave.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("set nonblock");
        let mut got = 0usize;
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while got < payload.len() && Instant::now() < deadline {
            match nix::unistd::read(pty.slave.as_raw_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    assert!(buf[..n].iter().all(|&b| b == b'x'));
                    got += n;
                }
                Err(nix::errno::Errno::EAGAIN) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("read slave: {e}"),
            }
        }
        assert_eq!(
            got,
            payload.len(),
            "line discipline swallowed {} of {} bytes",
            payload.len() - got,
            payload.len()
        );
    }

    /// What must SURVIVE the ICANON flip: signal generation (^C has to keep
    /// killing runaway foreground commands), echo, and output processing
    /// (OPOST off would staircase every line) — the reasons this fix is not
    /// cfmakeraw.
    #[test]
    fn signals_echo_and_output_processing_survive() {
        let pty = allocate_pty(24, 80).expect("openpty");
        let t = termios::tcgetattr(&pty.slave).expect("tcgetattr");
        assert!(
            !t.local_flags.contains(LocalFlags::ICANON),
            "canonical mode must be off"
        );
        assert!(
            t.local_flags.contains(LocalFlags::ISIG),
            "ISIG must survive"
        );
        assert!(
            t.local_flags.contains(LocalFlags::ECHO),
            "ECHO must survive"
        );
        assert!(
            t.output_flags.contains(OutputFlags::OPOST),
            "OPOST must survive"
        );
    }
}
