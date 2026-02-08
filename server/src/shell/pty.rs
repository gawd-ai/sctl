//! PTY allocation, shell spawning, and terminal resize.
//!
//! Uses the `nix` crate for POSIX PTY APIs. The PTY master fd is kept alive for
//! the session lifetime so I/O and resize operations can be performed on it.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Stdio;

use nix::pty::{openpty, OpenptyResult, Winsize};
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
