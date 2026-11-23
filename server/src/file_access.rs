//! The one gate before a file's content is read (ADR-011, TRD-9 3.1).
//!
//! `GET /api/files` (and the legacy `tunnel.file.read`, which calls it),
//! `GET /api/files/raw` and gawdxfer's download init call [`Gate::open`]
//! before they open anything. In order:
//!
//! 1. The path as given, split into components with `.` and empty ones
//!    dropped, is matched against the rules. A match is `PROTECTED_PATH`.
//! 2. A handle that opens nothing (`O_PATH | O_CLOEXEC`, links followed): no
//!    FIFO waits and no driver's open runs.
//! 3. From the handle: the real path (`readlink` of `/proc/self/fd/<n>`), the
//!    type and link count (`fstatat` with `AT_EMPTY_PATH`) and the
//!    filesystem (`statfs` of `/proc/self/fd/<n>`). The oldest unit kernel,
//!    Linux 3.3.8, takes an `O_PATH` handle in neither `fstat` nor `fstatfs`.
//! 4. A file with no link left is not found, whatever its real path says.
//! 5. The real path is matched against the rules. On procfs and sysfs only
//!    [`PSEUDO_FILES`] is read, matched on the path as given; anything else
//!    there, and any file on debugfs or tracefs, is `PROTECTED_PATH`.
//! 6. A FIFO, a socket or a device is `NOT_A_REGULAR_FILE` and is never
//!    opened. Where only a regular file moves ([`Purpose::Move`]), so is a
//!    listed pseudo file.
//! 7. The file is opened through `/proc/self/fd/<n>`, read-only, with
//!    `O_NONBLOCK`, `O_NOCTTY` and `O_CLOEXEC`: the file read is the file
//!    judged.
//!
//! The rules: the agent's own directories (taken as configured and as their
//! real paths, resolved again at each read), the names its keys and their
//! copies use, the config file it was started with, a process's secrets and
//! the kernel's streams. Each directory and process rule matches its
//! components anywhere in the path, so the same file reached through
//! `/overlay/upper/...`, `/rom/...` or `/proc/<pid>/root/...` is refused too.
//!
//! The gate keeps routine reads away from secrets and keeps a read from
//! harming the unit. It is not an authorization boundary: exec still reads
//! what it refuses (ADR-011 decision 8).

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{self, AtFlags, OFlag};
use nix::sys::stat::{fstatat, FileStat, Mode};
use nix::sys::statfs::{
    statfs, FsType, DEBUGFS_MAGIC, PROC_SUPER_MAGIC, SYSFS_MAGIC, TRACEFS_MAGIC,
};
use serde_json::json;

use crate::activity::{self, ActivityLog, ActivitySource, ActivityType};
use crate::config::Config;
use crate::error::codes;
use crate::upgrade::install::InstallInfo;

/// The only procfs and sysfs files a read returns (ADR-011 decision 3):
/// what the agent already reads about the system for itself, plus the
/// kernel's neighbour and route tables. Matched on the path as given;
/// `<if>` stands for one interface name. This is the one list: widening it
/// is a change that says why.
pub const PSEUDO_FILES: &[&str] = &[
    "/proc/net/arp",
    "/proc/net/route",
    "/proc/net/dev",
    "/proc/net/tcp",
    "/proc/net/tcp6",
    "/proc/meminfo",
    "/proc/loadavg",
    "/proc/uptime",
    "/proc/cpuinfo",
    "/proc/version",
    "/proc/mounts",
    "/proc/sys/kernel/hostname",
    "/sys/class/net/<if>/address",
    "/sys/class/net/<if>/operstate",
];

/// A process's secrets: `/proc/<pid>/<name>`, also under `task/<tid>`,
/// `<pid>` being a number, `self` or `thread-self`. Refused whatever
/// [`PSEUDO_FILES`] holds.
const PROCESS_SECRETS: &[(&str, &str)] = &[
    ("environ", "a process's environment"),
    ("mem", "a process's memory"),
    ("cwd", "a process's working directory"),
    ("fd", "a process's open files"),
    ("map_files", "a process's mapped files"),
];

/// The kernel's streams: `/proc/<name>`.
const KERNEL_STREAMS: &[(&str, &str)] = &[
    ("kcore", "the kernel's memory"),
    ("kmsg", "the kernel's message stream"),
    ("sysrq-trigger", "the kernel's SysRq trigger"),
];

/// Names the agent's keys and their copies begin with, refused wherever
/// they sit: the rollback copy of the config (always named `sctl.toml`),
/// the install scripts' backups, the Infra credential store and its
/// temporary file, and the Infra config, which holds the SNMP communities a
/// pushed config carries.
const KEY_NAMES: &[&str] = &["sctl.toml", "infra-secrets.json", "infra-monitor.json"];

/// What a read is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// `GET /api/files`: a regular file, or a pseudo file on the list.
    Read,
    /// `GET /api/files/raw` and a transfer: only a regular file on a
    /// filesystem that reports its size.
    Move,
}

/// Where a `PROTECTED_PATH` rule matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The path as given, before any handle was taken.
    Path,
    /// The file the path leads to: its real path or its filesystem.
    File,
}

impl Stage {
    /// The activity detail's form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::File => "file",
        }
    }
}

/// Why the gate gave no file.
#[derive(Debug)]
pub enum Refusal {
    /// `PROTECTED_PATH`: the rule that refused it, and where it matched.
    Protected { rule: String, stage: Stage },
    /// `NOT_A_REGULAR_FILE`: what the file is.
    NotRegular(String),
    /// No such file, or a file with no link left.
    NotFound,
    /// A directory.
    IsDirectory,
    /// The OS refused the handle or the open.
    PermissionDenied,
    /// Any other failure, the gate's own checks included: it fails closed.
    Io(io::Error),
}

impl Refusal {
    /// The catalog code. A directory is the route's to word.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Protected { .. } => codes::PROTECTED_PATH,
            Self::NotRegular(_) => codes::NOT_A_REGULAR_FILE,
            Self::NotFound => codes::FILE_NOT_FOUND,
            Self::IsDirectory => codes::IS_DIRECTORY,
            Self::PermissionDenied => codes::PERMISSION_DENIED,
            Self::Io(_) => codes::IO_ERROR,
        }
    }

    /// The message: the rule or what the file is, never its content.
    pub fn message(&self) -> String {
        match self {
            Self::Protected { rule, .. } => format!("Refused: {rule}"),
            Self::NotRegular(what) => format!("Not a regular file: {what}"),
            Self::NotFound => "File not found".to_string(),
            Self::IsDirectory => "Path is a directory".to_string(),
            Self::PermissionDenied => "Permission denied".to_string(),
            Self::Io(e) => e.to_string(),
        }
    }

    fn protected(rule: impl Into<String>, stage: Stage) -> Self {
        Self::Protected {
            rule: rule.into(),
            stage,
        }
    }

    /// A failed handle or open.
    fn opening(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Io(e),
        }
    }

    /// A failed fact about a handle: never "not found", since the handle is
    /// there; the gate cannot judge the file, so it does not read it.
    fn judging(e: &io::Error) -> Self {
        Self::Io(io::Error::new(
            e.kind(),
            format!("cannot judge the file: {e}"),
        ))
    }
}

/// Record one of the gate's own refusals on the unit: a `FileRead` entry
/// whose summary says the read was refused and whose detail names the rule.
/// A missing file or a directory is not a refusal and leaves no entry.
pub async fn record(
    log: &ActivityLog,
    source: ActivitySource,
    path: &str,
    refusal: &Refusal,
    request_id: Option<String>,
) {
    let detail = match refusal {
        Refusal::Protected { rule, stage } => json!({
            "code": codes::PROTECTED_PATH,
            "rule": rule,
            "matched": stage.as_str(),
        }),
        Refusal::NotRegular(what) => json!({
            "code": codes::NOT_A_REGULAR_FILE,
            "rule": what,
        }),
        _ => return,
    };
    log.log(
        ActivityType::FileRead,
        source,
        format!("read refused: {}", activity::truncate_str(path, 80)),
        Some(detail),
        request_id,
    )
    .await;
}

/// A file's identity: a later read checks it reads the same file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// A file the gate judged and opened.
#[derive(Debug)]
pub struct Opened {
    /// Opened read-only through the handle that was judged.
    pub file: File,
    /// The size its stat gave: on a pseudo file a page or nothing.
    pub size: u64,
    /// Last change, seconds since the epoch (None before 1970).
    pub modified: Option<u64>,
    /// The device and inode judged.
    pub id: FileId,
    /// `Some("procfs")` or `Some("sysfs")` for a listed pseudo file, which
    /// is read to its end, not to its size.
    pub pseudo: Option<&'static str>,
}

/// What a read returned.
#[derive(Debug)]
pub struct ReadOut {
    pub bytes: Vec<u8>,
    /// A regular file's size; on a pseudo file, the bytes to where the
    /// read stopped.
    pub size: u64,
    /// The file goes on past the returned bytes.
    pub truncated: bool,
}

impl Opened {
    /// Read from `offset`, at most `cap` bytes: a regular file up to the
    /// size its stat gave, or until it ends; a pseudo file until `read()`
    /// returns 0, `truncated` when the cap stopped it.
    pub fn read(&mut self, offset: u64, cap: usize) -> io::Result<ReadOut> {
        if offset > 0 {
            self.file.seek(SeekFrom::Start(offset))?;
        }
        if self.pseudo.is_some() {
            // One byte past the cap says whether the cap stopped the read.
            let mut bytes = read_up_to(&mut self.file, cap.saturating_add(1), 64 * 1024)?;
            let truncated = bytes.len() > cap;
            bytes.truncate(cap);
            let size = offset + bytes.len() as u64;
            return Ok(ReadOut {
                bytes,
                size,
                truncated,
            });
        }
        let want = self.size.saturating_sub(offset).min(cap as u64) as usize;
        let bytes = read_up_to(&mut self.file, want, want)?;
        let truncated = offset + (bytes.len() as u64) < self.size;
        Ok(ReadOut {
            bytes,
            size: self.size,
            truncated,
        })
    }
}

/// Read until `limit` bytes or the end, continuing short reads (and
/// reads a signal interrupted). `hint` sizes the first allocation.
pub fn read_up_to(reader: &mut impl Read, limit: usize, hint: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(hint.min(limit));
    reader.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// One of the agent's key directories.
#[derive(Clone, Debug)]
struct KeyDir {
    /// Absolute, as configured.
    path: PathBuf,
    /// What it holds, for the refusal.
    what: &'static str,
}

/// What the gate refuses, taken from the running agent.
#[derive(Clone, Debug)]
pub struct Gate {
    dirs: Vec<KeyDir>,
    /// The config file the agent was started with, absolute.
    config_file: Option<PathBuf>,
    /// File-name prefixes refused wherever they sit.
    names: Vec<OsString>,
}

impl Gate {
    /// The gate for this agent (ADR-011 decision 4): `/etc/sctl`;
    /// `state_dir`; `data_dir` when it is also `state_dir`, else its journal
    /// directory `<data_dir>/sessions`; the rollback directory
    /// `install.json` names; and the config file it was started with
    /// (`--config`, else `./sctl.toml` when that is what it loaded).
    pub fn for_agent(
        config: &Config,
        config_path: Option<&str>,
        install: Option<&InstallInfo>,
    ) -> Self {
        let server = &config.server;
        let state_dir = server.state_dir();
        let mut dirs = vec![
            (PathBuf::from("/etc/sctl"), "the agent's directory"),
            (PathBuf::from(state_dir), "the agent's state directory"),
        ];
        if Path::new(&server.data_dir) != Path::new(state_dir) {
            dirs.push((
                Path::new(&server.data_dir).join("sessions"),
                "the agent's session journals",
            ));
        }
        if let Some(install) = install {
            dirs.push((install.rollback_dir.clone(), "the agent's rollback set"));
        }
        let config_file = match config_path {
            Some(p) => Some(PathBuf::from(p)),
            None => Path::new("sctl.toml")
                .exists()
                .then(|| PathBuf::from("sctl.toml")),
        };
        Self::new(dirs, config_file)
    }

    /// A gate over these directories (each with what it holds) and this
    /// config file. A relative path is taken from the working directory.
    pub fn new(dirs: Vec<(PathBuf, &'static str)>, config_file: Option<PathBuf>) -> Self {
        let dirs = dirs
            .into_iter()
            .map(|(path, what)| KeyDir {
                path: absolute(&path),
                what,
            })
            .collect();
        let config_file = config_file.map(|p| absolute(&p));
        let mut names: Vec<OsString> = KEY_NAMES.iter().map(OsString::from).collect();
        if let Some(name) = config_file.as_deref().and_then(Path::file_name) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_os_string());
            }
        }
        Self {
            dirs,
            config_file,
            names,
        }
    }

    /// Judge `path`, and open it for reading only when every rule lets it
    /// through. Blocking: async callers run it on the blocking pool.
    pub fn open(&self, path: &Path, purpose: Purpose) -> Result<Opened, Refusal> {
        let asked = components(path);
        let dirs = self.dirs_now();
        // 1. The path as given, before anything is taken.
        if let Some(rule) = self.rule_for(&asked, &dirs) {
            return Err(Refusal::protected(rule, Stage::Path));
        }
        // 2. A handle that opens nothing.
        let handle = Handle::take(path).map_err(Refusal::opening)?;
        // 3. Its facts.
        let real = handle.real_path().map_err(|e| Refusal::judging(&e))?;
        let facts = handle.facts().map_err(|e| Refusal::judging(&e))?;
        let fs = handle.filesystem().map_err(|e| Refusal::judging(&e))?;
        // 4. Deleted, or a memory file a process holds.
        if facts.nlink == 0 {
            return Err(Refusal::NotFound);
        }
        // 5. The real path, then the filesystem.
        let rule = self.rule_for(&components(&real), &dirs).or_else(|| {
            self.is_config(&real)
                .then(|| "the agent's config file".to_string())
        });
        if let Some(rule) = rule {
            return Err(Refusal::protected(rule, Stage::File));
        }
        let pseudo = pseudo_filesystem(fs)?;
        if let Some(name) = pseudo {
            if !on_the_list(&asked) {
                return Err(Refusal::protected(
                    format!("a {name} file off the agent's list"),
                    Stage::File,
                ));
            }
        }
        // 6. The type: only a regular file is opened.
        if facts.kind == libc::S_IFDIR {
            return Err(Refusal::IsDirectory);
        }
        if let Some(what) = not_regular(facts.kind) {
            return Err(Refusal::NotRegular(what.to_string()));
        }
        if let (Purpose::Move, Some(name)) = (purpose, pseudo) {
            return Err(Refusal::NotRegular(format!(
                "a {name} file reports no size (GET /api/files reads it)"
            )));
        }
        // 7. The file judged, opened through its handle.
        let file = handle.reopen().map_err(Refusal::opening)?;
        Ok(Opened {
            file,
            size: facts.size,
            modified: facts.modified,
            id: facts.id,
            pseudo,
        })
    }

    /// Every directory as configured, and as its real path when it exists
    /// now: a directory made after start is covered once it exists.
    fn dirs_now(&self) -> Vec<(PathBuf, &KeyDir)> {
        let mut out = Vec::with_capacity(self.dirs.len() * 2);
        for dir in &self.dirs {
            out.push((dir.path.clone(), dir));
            if let Ok(real) = std::fs::canonicalize(&dir.path) {
                if real != dir.path {
                    out.push((real, dir));
                }
            }
        }
        out
    }

    /// The first rule a path's components match: a process's secret or a
    /// kernel stream, a key directory as a run anywhere in the path, or a
    /// key name the last component begins with.
    fn rule_for(&self, comps: &[&OsStr], dirs: &[(PathBuf, &KeyDir)]) -> Option<String> {
        if let Some(rule) = process_rule(comps) {
            return Some(rule.to_string());
        }
        for (path, dir) in dirs {
            if contains_run(comps, &components(path)) {
                return Some(format!("{} {}", dir.what, dir.path.display()));
            }
        }
        let name = comps.last()?.as_bytes();
        self.names
            .iter()
            .find(|n| name.starts_with(n.as_bytes()))
            .map(|n| format!("a name the agent's keys use ({}*)", n.to_string_lossy()))
    }

    /// Whether `real` is the config file the agent was started with.
    fn is_config(&self, real: &Path) -> bool {
        let Some(config) = &self.config_file else {
            return false;
        };
        real == config || std::fs::canonicalize(config).is_ok_and(|c| c == real)
    }
}

/// Open one of the agent's own files for one of its own routes: a
/// playbook, the watchdog's history, the safe-mode flag. These live in the
/// agent's directories, which [`Gate::open`] refuses to a caller's read, so
/// its path rules do not apply; its handle and its type judgment do (ADR-011
/// decisions 2 and 6). A handle that opens nothing is taken (`O_PATH`), the
/// type is read from it, and only a regular file is opened, through that
/// handle and without waiting. A FIFO, a socket or a device placed at the
/// path is `NOT_A_REGULAR_FILE` and is never opened, so it holds no
/// handler. Blocking: async callers run it on the blocking pool.
pub fn open_own(path: &Path) -> Result<Opened, Refusal> {
    let handle = Handle::take(path).map_err(Refusal::opening)?;
    let facts = handle.facts().map_err(|e| Refusal::judging(&e))?;
    if facts.nlink == 0 {
        return Err(Refusal::NotFound);
    }
    if facts.kind == libc::S_IFDIR {
        return Err(Refusal::IsDirectory);
    }
    if let Some(what) = not_regular(facts.kind) {
        return Err(Refusal::NotRegular(what.to_string()));
    }
    let file = handle.reopen().map_err(Refusal::opening)?;
    Ok(Opened {
        file,
        size: facts.size,
        modified: facts.modified,
        id: facts.id,
        pseudo: None,
    })
}

/// Read one of the agent's own files whole, at most `cap` bytes
/// ([`open_own`]). `Ok(None)` when no file is there; a file longer than
/// `cap` is cut there.
pub fn read_own(path: &Path, cap: usize) -> Result<Option<Vec<u8>>, Refusal> {
    match open_own(path) {
        Ok(mut opened) => opened
            .read(0, cap)
            .map(|out| Some(out.bytes))
            .map_err(Refusal::Io),
        Err(Refusal::NotFound) => Ok(None),
        Err(refusal) => Err(refusal),
    }
}

/// Whether one of the agent's own routes may write `path`: nothing is
/// there, or a regular file is. An open for writing waits on a FIFO with no
/// reader and runs a device's open, so anything else is
/// `NOT_A_REGULAR_FILE` before any open (ADR-011 decision 6). The judgment
/// is the read's: a handle that opens nothing, and the type read from it.
pub fn writable_own(path: &Path) -> Result<(), Refusal> {
    let handle = match Handle::take(path) {
        Ok(handle) => handle,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Refusal::opening(e)),
    };
    let facts = handle.facts().map_err(|e| Refusal::judging(&e))?;
    if facts.kind == libc::S_IFDIR {
        return Err(Refusal::IsDirectory);
    }
    match not_regular(facts.kind) {
        Some(what) => Err(Refusal::NotRegular(what.to_string())),
        None => Ok(()),
    }
}

/// Open `path` again for a later read of a file the gate judged, only while
/// it is still that file: the same device and inode, still a regular file.
/// Returns the file and its last change (seconds since the epoch).
pub fn reopen_judged(path: &Path, id: FileId) -> Result<(File, Option<u64>), Reopen> {
    let handle = Handle::take(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Reopen::Changed("the file is gone"),
        _ => Reopen::Io(e),
    })?;
    let facts = handle.facts().map_err(Reopen::Io)?;
    if facts.kind != libc::S_IFREG {
        return Err(Reopen::Changed("it is no longer a regular file"));
    }
    if facts.id != id {
        return Err(Reopen::Changed("another file is at its path"));
    }
    let file = handle.reopen().map_err(Reopen::Io)?;
    Ok((file, facts.modified))
}

/// Why [`reopen_judged`] gave no file.
#[derive(Debug)]
pub enum Reopen {
    /// Not the file judged any more (`FILE_CHANGED`).
    Changed(&'static str),
    /// The handle or the open failed.
    Io(io::Error),
}

/// What the gate reads from a stat.
#[derive(Clone, Copy, Debug)]
struct Facts {
    /// `st_mode & S_IFMT`.
    kind: libc::mode_t,
    nlink: u64,
    size: u64,
    modified: Option<u64>,
    id: FileId,
}

impl Facts {
    fn of(st: &FileStat) -> Self {
        // The widths of these differ by target (a 32-bit time_t on mips).
        #[allow(clippy::useless_conversion)]
        let nlink = u64::from(st.st_nlink);
        #[allow(clippy::useless_conversion)]
        let mtime = i64::from(st.st_mtime);
        #[allow(clippy::useless_conversion)]
        let id = FileId {
            dev: u64::from(st.st_dev),
            ino: u64::from(st.st_ino),
        };
        Self {
            kind: st.st_mode & libc::S_IFMT,
            nlink,
            size: u64::try_from(st.st_size).unwrap_or(0),
            modified: u64::try_from(mtime).ok(),
            id,
        }
    }
}

/// A handle on a path that opens nothing (`O_PATH`): no FIFO waits and no
/// driver's open runs. Links are followed.
struct Handle(File);

impl Handle {
    /// `open(path, O_PATH | O_CLOEXEC)`, called directly. std's
    /// `OpenOptions` keeps only the custom flags outside `O_ACCMODE`, and
    /// musl's `O_ACCMODE` (`0o10000003`) holds `O_PATH`: through std, every
    /// release target took a plain read-only open, which waited on a FIFO
    /// and ran a device's open (0.6.12). With `O_PATH` the kernel ignores
    /// the access mode. A signal during the lookup is retried, as std does.
    fn take(path: &Path) -> io::Result<Self> {
        let fd = loop {
            match fcntl::open(path, OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty()) {
                Err(Errno::EINTR) => {}
                other => break other?,
            }
        };
        // SAFETY: `fd` was just returned by open(2) and is owned by nobody else.
        Ok(Self(File::from(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// `/proc/self/fd/<n>`: names the handle's file for `readlink`,
    /// `statfs` and `open` on every unit kernel.
    fn fd_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    fn real_path(&self) -> io::Result<PathBuf> {
        std::fs::read_link(self.fd_path())
    }

    /// `fstatat(fd, "", AT_EMPTY_PATH)`, which takes an `O_PATH` handle
    /// from Linux 2.6.39 where `fstat` takes it only from 3.6.
    fn facts(&self) -> io::Result<Facts> {
        let st = fstatat(Some(self.0.as_raw_fd()), "", AtFlags::AT_EMPTY_PATH)?;
        Ok(Facts::of(&st))
    }

    /// `statfs` of `/proc/self/fd/<n>`: `fstatfs` takes an `O_PATH` handle
    /// only from Linux 3.12.
    fn filesystem(&self) -> io::Result<FsType> {
        Ok(statfs(&self.fd_path())?.filesystem_type())
    }

    /// The handle's own file, opened for reading. std keeps these flags:
    /// neither lies in `O_ACCMODE` on any target (checked below).
    fn reopen(&self) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(self.fd_path())
    }
}

// std's `custom_flags` keeps only the flags outside `O_ACCMODE`, which on
// musl holds `O_PATH` too. Every flag the agent passes through it (above,
// and in `upgrade::cli_name`'s read of its wrapper) lies outside it on the
// target being built.
const _: () =
    assert!((libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW) & libc::O_ACCMODE == 0);

/// procfs and sysfs files are read only from the list; debugfs and tracefs
/// files never.
fn pseudo_filesystem(fs: FsType) -> Result<Option<&'static str>, Refusal> {
    if fs == PROC_SUPER_MAGIC {
        Ok(Some("procfs"))
    } else if fs == SYSFS_MAGIC {
        Ok(Some("sysfs"))
    } else if fs == DEBUGFS_MAGIC {
        Err(Refusal::protected("a debugfs file", Stage::File))
    } else if fs == TRACEFS_MAGIC {
        Err(Refusal::protected("a tracefs file", Stage::File))
    } else {
        Ok(None)
    }
}

/// What a file that is neither a regular file nor a directory is.
fn not_regular(kind: libc::mode_t) -> Option<&'static str> {
    match kind {
        libc::S_IFREG => None,
        libc::S_IFIFO => Some("a FIFO"),
        libc::S_IFSOCK => Some("a socket"),
        libc::S_IFCHR => Some("a character device"),
        libc::S_IFBLK => Some("a block device"),
        _ => Some("a file of no known type"),
    }
}

/// Whether the path as given is on [`PSEUDO_FILES`].
fn on_the_list(asked: &[&OsStr]) -> bool {
    PSEUDO_FILES.iter().any(|entry| {
        let want: Vec<&str> = entry.split('/').filter(|c| !c.is_empty()).collect();
        want.len() == asked.len()
            && want
                .iter()
                .zip(asked)
                .all(|(w, a)| *w == "<if>" || w.as_bytes() == a.as_bytes())
    })
}

/// The process and kernel rule a path's components match, anywhere in it.
fn process_rule(comps: &[&OsStr]) -> Option<&'static str> {
    for (i, c) in comps.iter().enumerate() {
        if c.as_bytes() != b"proc" {
            continue;
        }
        let rest = &comps[i + 1..];
        if let Some(rule) = rest.first().and_then(|n| lookup(KERNEL_STREAMS, n)) {
            return Some(rule);
        }
        if rest.len() >= 2 && is_pid(rest[0]) {
            if let Some(rule) = lookup(PROCESS_SECRETS, rest[1]) {
                return Some(rule);
            }
            if rest.len() >= 4 && rest[1].as_bytes() == b"task" && is_pid(rest[2]) {
                if let Some(rule) = lookup(PROCESS_SECRETS, rest[3]) {
                    return Some(rule);
                }
            }
        }
    }
    None
}

/// A number, `self` or `thread-self`.
fn is_pid(c: &OsStr) -> bool {
    let b = c.as_bytes();
    b == b"self" || b == b"thread-self" || (!b.is_empty() && b.iter().all(u8::is_ascii_digit))
}

fn lookup(table: &[(&str, &'static str)], name: &OsStr) -> Option<&'static str> {
    table
        .iter()
        .find(|(n, _)| n.as_bytes() == name.as_bytes())
        .map(|(_, rule)| *rule)
}

/// A path's components: the root, `.` and empty ones dropped.
fn components(path: &Path) -> Vec<&OsStr> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            Component::ParentDir => Some(OsStr::new("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// Whether `run` appears in `path` as consecutive components. An empty run
/// (a directory configured as `/`) covers every path.
fn contains_run(path: &[&OsStr], run: &[&OsStr]) -> bool {
    run.is_empty() || path.windows(run.len()).any(|w| w == run)
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    use nix::fcntl::FcntlArg;
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    /// A fresh directory under the system's temp directory.
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sctl-gate-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A gate with only the fixed rules.
    fn plain_gate() -> Gate {
        Gate::new(
            vec![(PathBuf::from("/etc/sctl"), "the agent's directory")],
            None,
        )
    }

    fn mkfifo(path: &Path) {
        nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
    }

    /// Run `f` on its own thread and insist it answers within seconds. An
    /// open that waited on a FIFO would hang the test run, so on a timeout
    /// the FIFO gets a writer (opened without waiting) before the test
    /// fails.
    fn answers_at_once<T: Send + 'static>(
        fifo: Option<&Path>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(v) => v,
            Err(RecvTimeoutError::Disconnected) => panic!("the check failed on its thread"),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(fifo) = fifo {
                    let _ = OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(fifo);
                }
                panic!("the gate waited on a file it must refuse at once");
            }
        }
    }

    fn expect_protected(r: Result<Opened, Refusal>) -> (String, Stage) {
        match r {
            Err(Refusal::Protected { rule, stage }) => (rule, stage),
            other => panic!("expected PROTECTED_PATH, got {other:?}"),
        }
    }

    fn expect_not_regular(r: Result<Opened, Refusal>) -> String {
        match r {
            Err(Refusal::NotRegular(what)) => what,
            other => panic!("expected NOT_A_REGULAR_FILE, got {other:?}"),
        }
    }

    fn read_all(gate: &Gate, path: &Path) -> ReadOut {
        let mut opened = gate.open(path, Purpose::Read).unwrap();
        opened.read(0, 50 * 1024 * 1024).unwrap()
    }

    /// A reader that hands out three bytes per call, with an interrupted
    /// call now and then.
    struct Trickle {
        data: Vec<u8>,
        at: usize,
        calls: usize,
    }

    impl Trickle {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                at: 0,
                calls: 0,
            }
        }
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls.is_multiple_of(5) {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let n = 3.min(buf.len()).min(self.data.len() - self.at);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    #[test]
    fn the_read_loop_continues_short_reads_to_the_end_or_the_limit() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            read_up_to(&mut Trickle::new(&data), 5000, 16).unwrap(),
            data
        );
        assert_eq!(
            read_up_to(&mut Trickle::new(&data), 700, 700).unwrap(),
            data[..700]
        );
        assert!(read_up_to(&mut io::empty(), 10, 10).unwrap().is_empty());
    }

    #[test]
    fn the_read_loop_returns_a_procfs_file_of_several_pages_whole() {
        // Below the gate: smaps is a seq file that answers a page or a
        // record per read(); one read() would cut it.
        let mut f = File::open("/proc/self/smaps").unwrap();
        let bytes = read_up_to(&mut f, 64 * 1024 * 1024, 4096).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.len() > 3 * 4096, "only {} bytes", text.len());
        assert!(text.ends_with('\n'));
        // Every mapping's record is whole: its header line (an address
        // range) and its last line (VmFlags) come in pairs.
        let headers = text
            .lines()
            .filter(|l| {
                l.split_whitespace()
                    .next()
                    .is_some_and(|t| !t.ends_with(':'))
            })
            .count();
        let ends = text.lines().filter(|l| l.starts_with("VmFlags:")).count();
        assert!(ends > 1);
        assert_eq!(headers, ends);
    }

    #[test]
    fn the_handle_facts_match_std() {
        let dir = scratch("facts");
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"twelve bytes").unwrap();
        let fifo = dir.join("pipe");
        mkfifo(&fifo);
        let cases: [(&Path, libc::mode_t); 4] = [
            (&file, libc::S_IFREG),
            (&fifo, libc::S_IFIFO),
            (&dir, libc::S_IFDIR),
            (Path::new("/dev/null"), libc::S_IFCHR),
        ];
        for (path, kind) in cases {
            let owned = path.to_path_buf();
            let (facts, real, fs) = answers_at_once(Some(&fifo), move || {
                let h = Handle::take(&owned).unwrap();
                (
                    h.facts().unwrap(),
                    h.real_path().unwrap(),
                    h.filesystem().unwrap(),
                )
            });
            let meta = std::fs::metadata(path).unwrap();
            assert_eq!(facts.kind, kind, "{}", path.display());
            assert_eq!(facts.kind, meta.mode() & libc::S_IFMT);
            assert_eq!(facts.nlink, meta.nlink());
            assert_eq!(facts.size, meta.size());
            assert_eq!(
                facts.id,
                FileId {
                    dev: meta.dev(),
                    ino: meta.ino()
                }
            );
            assert_eq!(real, std::fs::canonicalize(path).unwrap());
            assert_eq!(fs, statfs(path).unwrap().filesystem_type());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_regular_file_reads_whole_and_by_range() {
        let dir = scratch("regular");
        let file = dir.join("data.bin");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        let gate = plain_gate();

        let out = read_all(&gate, &file);
        assert_eq!(out.bytes, data);
        assert_eq!(out.size, 10_000);
        assert!(!out.truncated);

        let mut opened = gate.open(&file, Purpose::Read).unwrap();
        assert_eq!(opened.pseudo, None);
        let out = opened.read(100, 50).unwrap();
        assert_eq!(out.bytes, data[100..150]);
        assert_eq!(out.size, 10_000);
        assert!(out.truncated);

        let mut opened = gate.open(&file, Purpose::Read).unwrap();
        let out = opened.read(9_990, 4096).unwrap();
        assert_eq!(out.bytes, data[9_990..]);
        assert!(!out.truncated);

        // A move takes a regular file too.
        assert!(gate.open(&file, Purpose::Move).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listed_pseudo_files_read_to_their_end() {
        let gate = plain_gate();
        for path in ["/proc/net/arp", "/proc/meminfo"] {
            let out = read_all(&gate, Path::new(path));
            assert!(!out.bytes.is_empty(), "{path} read empty");
            assert_eq!(out.size, out.bytes.len() as u64, "{path}");
            assert!(!out.truncated);
            assert!(out.bytes.ends_with(b"\n"), "{path}");
        }
        let arp = String::from_utf8(read_all(&gate, Path::new("/proc/net/arp")).bytes).unwrap();
        assert!(arp.starts_with("IP address"), "{arp}");

        // sysfs says 4096 bytes; the file holds 18.
        let mut opened = gate
            .open(Path::new("/sys/class/net/lo/address"), Purpose::Read)
            .unwrap();
        assert_eq!(opened.pseudo, Some("sysfs"));
        assert_eq!(opened.size, 4096);
        let out = opened.read(0, 1024 * 1024).unwrap();
        assert_eq!(out.bytes, b"00:00:00:00:00:00\n");
        assert_eq!(out.size, 18);
        assert!(!out.truncated);

        // The cap stops a pseudo file's read, and says so.
        let mut opened = gate
            .open(Path::new("/proc/meminfo"), Purpose::Read)
            .unwrap();
        let out = opened.read(0, 10).unwrap();
        assert_eq!(out.bytes.len(), 10);
        assert_eq!(out.size, 10);
        assert!(out.truncated);
        let mut opened = gate
            .open(Path::new("/proc/meminfo"), Purpose::Read)
            .unwrap();
        let out = opened.read(4, 6).unwrap();
        assert_eq!(out.size, 10);
        assert!(out.truncated);
    }

    #[test]
    fn process_secrets_and_kernel_streams_are_refused_on_the_path_as_given() {
        let gate = plain_gate();
        let task_environ = format!("/proc/self/task/{}/environ", nix::unistd::gettid());
        for path in [
            "/proc/self/environ",
            "/proc/thread-self/environ",
            task_environ.as_str(),
            "/proc/self/fd/0",
            "/proc/1/root/proc/self/environ",
            "/proc/kmsg",
            "/proc/kcore",
            "/proc/sysrq-trigger",
            "/proc/self/mem",
            "/proc/self/cwd/Cargo.toml",
            "/proc/self/map_files",
            "/proc/./self//environ",
        ] {
            let (rule, stage) = answers_at_once(None, {
                let gate = gate.clone();
                let path = PathBuf::from(path);
                move || expect_protected(gate.open(&path, Purpose::Read))
            });
            assert_eq!(stage, Stage::Path, "{path}: {rule}");
        }
        let (rule, _) = expect_protected(gate.open(Path::new("/proc/self/environ"), Purpose::Read));
        assert_eq!(rule, "a process's environment");
        let (rule, _) = expect_protected(gate.open(Path::new("/proc/kmsg"), Purpose::Move));
        assert_eq!(rule, "the kernel's message stream");
    }

    #[test]
    fn pseudo_files_off_the_list_are_refused_from_the_handles_filesystem() {
        let gate = plain_gate();
        for path in [
            "/proc/self/maps",
            "/proc/self/cmdline",
            "/proc/self/net/arp",
            "/proc/kallsyms",
            "/sys/class/net/lo/mtu",
        ] {
            let (rule, stage) = expect_protected(gate.open(Path::new(path), Purpose::Read));
            assert_eq!(stage, Stage::File, "{path}");
            assert!(rule.contains("off the agent's list"), "{path}: {rule}");
        }
        // A link from elsewhere into procfs is not on the list as given.
        let dir = scratch("proclink");
        let link = dir.join("arp");
        symlink("/proc/net/arp", &link).unwrap();
        let (rule, stage) = expect_protected(gate.open(&link, Purpose::Read));
        assert_eq!(rule, "a procfs file off the agent's list");
        assert_eq!(stage, Stage::File);
        // A link to a process's secret is refused by its real path.
        let secret = dir.join("env");
        symlink("/proc/self/environ", &secret).unwrap();
        let (rule, stage) = expect_protected(gate.open(&secret, Purpose::Read));
        assert_eq!(rule, "a process's environment");
        assert_eq!(stage, Stage::File);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fifos_and_devices_are_refused_at_once() {
        let dir = scratch("fifo");
        let fifo = dir.join("pipe");
        mkfifo(&fifo);
        let gate = plain_gate();
        for _ in 0..40 {
            let what = answers_at_once(Some(&fifo), {
                let (gate, fifo) = (gate.clone(), fifo.clone());
                move || expect_not_regular(gate.open(&fifo, Purpose::Read))
            });
            assert_eq!(what, "a FIFO");
        }
        let what = answers_at_once(Some(&fifo), {
            let (gate, fifo) = (gate.clone(), fifo.clone());
            move || expect_not_regular(gate.open(&fifo, Purpose::Move))
        });
        assert_eq!(what, "a FIFO");
        assert_eq!(
            expect_not_regular(gate.open(Path::new("/dev/null"), Purpose::Read)),
            "a character device"
        );
        assert_eq!(
            expect_not_regular(gate.open(Path::new("/dev/zero"), Purpose::Move)),
            "a character device"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The agent's own routes read the agent's own files through the same
    /// handle and the same type judgment: a regular file is read, and a
    /// FIFO, a socket or a device placed at its path is refused at once and
    /// never opened, for a read and for a write.
    #[test]
    fn the_agents_own_files_are_judged_by_the_same_handle() {
        let dir = scratch("own");
        let file = dir.join("playbook.md");
        std::fs::write(&file, b"twelve bytes").unwrap();
        assert_eq!(read_own(&file, 1024).unwrap().unwrap(), b"twelve bytes");
        assert_eq!(
            read_own(&file, 6).unwrap().unwrap(),
            b"twelve",
            "cut at the cap"
        );
        assert!(read_own(&dir.join("none"), 1024).unwrap().is_none());
        assert!(matches!(open_own(&dir), Err(Refusal::IsDirectory)));
        assert!(writable_own(&file).is_ok());
        assert!(writable_own(&dir.join("none")).is_ok(), "nothing there yet");
        assert!(matches!(writable_own(&dir), Err(Refusal::IsDirectory)));

        let fifo = dir.join("pipe.md");
        mkfifo(&fifo);
        for _ in 0..40 {
            let what = answers_at_once(Some(&fifo), {
                let fifo = fifo.clone();
                move || expect_not_regular(open_own(&fifo))
            });
            assert_eq!(what, "a FIFO");
        }
        let what = answers_at_once(Some(&fifo), {
            let fifo = fifo.clone();
            move || match writable_own(&fifo) {
                Err(Refusal::NotRegular(what)) => what,
                other => panic!("a FIFO was judged writable: {other:?}"),
            }
        });
        assert_eq!(what, "a FIFO");
        assert_eq!(
            expect_not_regular(open_own(Path::new("/dev/null"))),
            "a character device"
        );
        assert!(matches!(
            writable_own(Path::new("/dev/zero")),
            Err(Refusal::NotRegular(_))
        ));
        // A link to a FIFO is the FIFO.
        let link = dir.join("link.md");
        symlink(&fifo, &link).unwrap();
        let what = answers_at_once(Some(&fifo), {
            let link = link.clone();
            move || expect_not_regular(open_own(&link))
        });
        assert_eq!(what, "a FIFO");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The handle is `O_PATH`, so taking it opens nothing. Through std's
    /// `custom_flags` it was only on glibc: musl's `O_ACCMODE` holds
    /// `O_PATH`, std drops the flags in it, and the handle was a plain
    /// read-only open on every release target (0.6.12).
    #[test]
    fn the_handle_carries_o_path_and_only_the_reopen_reads() {
        let dir = scratch("o-path");
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"twelve bytes").unwrap();
        let fifo = dir.join("pipe");
        mkfifo(&fifo);
        let link = dir.join("link");
        symlink(&file, &link).unwrap();
        let paths: [&Path; 5] = [&file, &fifo, &dir, &link, Path::new("/dev/null")];
        for path in paths {
            let owned = path.to_path_buf();
            let (status, fd_flags) = answers_at_once(Some(&fifo), move || {
                let h = Handle::take(&owned).unwrap();
                let fd = h.0.as_raw_fd();
                (
                    fcntl::fcntl(fd, FcntlArg::F_GETFL).unwrap(),
                    fcntl::fcntl(fd, FcntlArg::F_GETFD).unwrap(),
                )
            });
            let shown = path.display();
            assert_ne!(status & libc::O_PATH, 0, "{shown}: flags {status:#o}");
            assert_ne!(fd_flags & libc::FD_CLOEXEC, 0, "{shown}");
        }
        // The file judged is then opened for reading, without waiting.
        let mut reopened = Handle::take(&link).unwrap().reopen().unwrap();
        let status = fcntl::fcntl(reopened.as_raw_fd(), FcntlArg::F_GETFL).unwrap();
        assert_eq!(status & libc::O_PATH, 0, "flags {status:#o}");
        assert_eq!(
            status & libc::O_ACCMODE,
            libc::O_RDONLY,
            "flags {status:#o}"
        );
        assert_ne!(status & libc::O_NONBLOCK, 0, "flags {status:#o}");
        let mut text = String::new();
        reopened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "twelve bytes");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A link outside the key directories to a file in one is refused by
    /// its real path, and the refusal opens nothing: a FIFO there answers at
    /// once, and a key file there sees no open.
    #[test]
    fn a_link_into_a_key_directory_is_refused_without_opening_its_file() {
        let root = scratch("link-in");
        let state_dir = root.join("state");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let gate = Gate::new(
            vec![(state_dir.clone(), "the agent's state directory")],
            None,
        );

        // A FIFO in the key directory: an open would wait for a writer.
        let fifo = state_dir.join("pipe");
        mkfifo(&fifo);
        let link = elsewhere.join("notes");
        symlink(&fifo, &link).unwrap();
        for purpose in [Purpose::Read, Purpose::Move] {
            let (rule, stage) = answers_at_once(Some(&fifo), {
                let (gate, link) = (gate.clone(), link.clone());
                move || expect_protected(gate.open(&link, purpose))
            });
            assert_eq!(stage, Stage::File);
            assert!(rule.starts_with("the agent's state directory"), "{rule}");
        }

        // A key file in it, watched for opens.
        let key = state_dir.join("tls_pins.json");
        std::fs::write(&key, "{}").unwrap();
        let link = elsewhere.join("pins");
        symlink(&key, &link).unwrap();
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC).unwrap();
        inotify.add_watch(&key, AddWatchFlags::IN_OPEN).unwrap();
        let opens = || match inotify.read_events() {
            Ok(events) => events.len(),
            Err(Errno::EAGAIN) => 0,
            Err(e) => panic!("inotify: {e}"),
        };
        // Linux before 5.13 reports an O_PATH handle as an open too; there
        // only the FIFO above shows the refusal opens nothing. The kernel
        // is asked directly, never through the handle under test.
        let fd = fcntl::open(&key, OFlag::O_PATH | OFlag::O_CLOEXEC, Mode::empty()).unwrap();
        nix::unistd::close(fd).unwrap();
        if opens() == 0 {
            let (rule, stage) = expect_protected(gate.open(&link, Purpose::Read));
            assert_eq!(stage, Stage::File);
            assert!(rule.starts_with("the agent's state directory"), "{rule}");
            assert_eq!(opens(), 0, "the refusal opened the key file");
        } else {
            eprintln!("this kernel reports an O_PATH handle as an open: the watch is not checked");
        }
        // The watch sees a plain open.
        drop(File::open(&key).unwrap());
        assert!(opens() > 0, "the watch saw no open");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_move_takes_no_pseudo_file() {
        let gate = plain_gate();
        let what = expect_not_regular(gate.open(Path::new("/proc/net/arp"), Purpose::Move));
        assert!(what.contains("procfs"), "{what}");
        let what =
            expect_not_regular(gate.open(Path::new("/sys/class/net/lo/address"), Purpose::Move));
        assert!(what.contains("sysfs"), "{what}");
        // The rules still come first.
        let (_, stage) = expect_protected(gate.open(Path::new("/proc/self/maps"), Purpose::Move));
        assert_eq!(stage, Stage::File);
    }

    #[test]
    fn a_directory_and_a_missing_file_are_not_refusals() {
        let dir = scratch("dir");
        let gate = plain_gate();
        assert!(matches!(
            gate.open(&dir, Purpose::Read),
            Err(Refusal::IsDirectory)
        ));
        assert!(matches!(
            gate.open(&dir.join("absent"), Purpose::Read),
            Err(Refusal::NotFound)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The gate `main` builds for an agent started with `--config <file>`.
    /// Its data directory is apart from the config's, so only the config's
    /// own rules can refuse the file.
    fn started_with(config_file: &Path, data_dir: &Path) -> Gate {
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = data_dir.display().to_string();
        Gate::for_agent(&config, Some(config_file.to_str().unwrap()), None)
    }

    #[test]
    fn the_agents_config_is_refused_however_it_is_reached() {
        let dir = scratch("config");
        let config = dir.join("agent.toml");
        std::fs::write(&config, "[auth]\napi_key = \"test\"\n").unwrap();
        let data_dir = dir.join("data");
        let gate = started_with(&config, &data_dir);

        // Directly: by its name.
        let (rule, stage) = expect_protected(gate.open(&config, Purpose::Read));
        assert_eq!(rule, "a name the agent's keys use (agent.toml*)");
        assert_eq!(stage, Stage::Path);

        // Through a link with another name: by its real path.
        let link = dir.join("innocent.txt");
        symlink(&config, &link).unwrap();
        let (_, stage) = expect_protected(gate.open(&link, Purpose::Read));
        assert_eq!(stage, Stage::File);

        // Through a process's root.
        let via_root = PathBuf::from(format!("/proc/self/root{}", link.display()));
        let (_, stage) = expect_protected(gate.open(&via_root, Purpose::Read));
        assert_eq!(stage, Stage::File);

        // A copy named after the rollback copy, in another directory.
        let other = scratch("config-copy");
        let copy = other.join("sctl.toml.bak");
        std::fs::copy(&config, &copy).unwrap();
        let (rule, stage) = expect_protected(gate.open(&copy, Purpose::Read));
        assert_eq!(rule, "a name the agent's keys use (sctl.toml*)");
        assert_eq!(stage, Stage::Path);

        // A config the agent was started with through a link: the file it
        // leads to is refused by its real path, whatever its name.
        let target = other.join("settings.conf");
        std::fs::write(&target, "x").unwrap();
        let link = other.join("linked.toml");
        symlink(&target, &link).unwrap();
        let gate = started_with(&link, &data_dir);
        let (rule, stage) = expect_protected(gate.open(&target, Purpose::Read));
        assert_eq!(rule, "the agent's config file");
        assert_eq!(stage, Stage::File);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn the_agents_directories_are_refused_and_its_panic_log_is_not() {
        let root = scratch("dirs");
        let state_dir = root.join("state");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(data_dir.join("sessions")).unwrap();
        std::fs::write(state_dir.join("infra-secrets.json"), "{}").unwrap();
        std::fs::write(state_dir.join("infra-monitor.json"), "{}").unwrap();
        std::fs::write(state_dir.join("tls_pins.json"), "{}").unwrap();
        std::fs::write(data_dir.join("sessions/abc.jsonl"), "{}").unwrap();
        std::fs::write(data_dir.join("last_panic.log"), "panic").unwrap();

        let mut config: Config = toml::from_str("").unwrap();
        config.server.state_dir = state_dir.display().to_string();
        config.server.data_dir = data_dir.display().to_string();
        let gate = Gate::for_agent(&config, None, None);

        for name in ["infra-secrets.json", "infra-monitor.json", "tls_pins.json"] {
            let (rule, stage) = expect_protected(gate.open(&state_dir.join(name), Purpose::Read));
            assert_eq!(stage, Stage::Path, "{name}");
            assert!(rule.starts_with("the agent's state directory"), "{rule}");
        }
        let (rule, _) =
            expect_protected(gate.open(&data_dir.join("sessions/abc.jsonl"), Purpose::Read));
        assert!(rule.starts_with("the agent's session journals"), "{rule}");
        assert_eq!(
            read_all(&gate, &data_dir.join("last_panic.log")).bytes,
            b"panic"
        );

        // Copies of the key files elsewhere are refused by name.
        let elsewhere = scratch("dirs-copy");
        for name in ["infra-secrets.json", "infra-monitor.json.tmp"] {
            let copy = elsewhere.join(name);
            std::fs::write(&copy, "{}").unwrap();
            let (rule, _) = expect_protected(gate.open(&copy, Purpose::Read));
            assert!(rule.starts_with("a name the agent's keys use"), "{rule}");
        }

        // Reached through a link to the state directory: by its real path.
        let link = elsewhere.join("st");
        symlink(&state_dir, &link).unwrap();
        let (_, stage) = expect_protected(gate.open(&link.join("tls_pins.json"), Purpose::Read));
        assert_eq!(stage, Stage::File);

        // Where data_dir is state_dir, all of it is protected.
        config.server.state_dir = String::new();
        let gate = Gate::for_agent(&config, None, None);
        let (rule, _) =
            expect_protected(gate.open(&data_dir.join("last_panic.log"), Purpose::Read));
        assert!(rule.starts_with("the agent's state directory"), "{rule}");

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn a_rollback_directory_made_after_the_gate_is_refused() {
        let root = scratch("rollback");
        // The rollback directory is named through a link that does not
        // exist yet: only resolving it again at the read covers it.
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = root.join("data").display().to_string();
        let mut install = InstallInfo::defaults(crate::upgrade::install::Layout::UsrBin, "x86_64");
        install.rollback_dir = root.join("via").join("rollback");
        let gate = Gate::for_agent(&config, None, Some(&install));

        let real = root.join("real");
        std::fs::create_dir_all(real.join("rollback")).unwrap();
        symlink(&real, root.join("via")).unwrap();
        let payload = real.join("rollback").join("server.bin");
        std::fs::write(&payload, "payload").unwrap();

        let (rule, stage) = expect_protected(gate.open(&payload, Purpose::Read));
        assert_eq!(stage, Stage::Path);
        assert!(rule.starts_with("the agent's rollback set"), "{rule}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_layout_refuses_its_key_directories_on_the_path_as_given() {
        use crate::upgrade::install::Layout;
        // Each install layout with the directories its units' configs set:
        // the BPIs and the relay set neither, the XE300 keeps both on its
        // overlay, and the RUT241 (installed as gz-tmp, later ramboot) and
        // the WE826 keep data in tmpfs and state in /etc/sctl. An empty
        // state_dir is data_dir.
        let units = [
            (Layout::UsrBin, "riscv64", "/var/lib/sctl", "", "sctl.toml"),
            (
                Layout::GzTmp,
                "mips_24kc",
                "/usr/local/lib/sctl/data",
                "",
                "sctl.toml",
            ),
            (
                Layout::GzTmp,
                "mipsel_24kc",
                "/tmp/sctl/data",
                "/etc/sctl/state",
                "sctl.toml",
            ),
            (
                Layout::Ramboot,
                "mips_24kc",
                "/tmp/sctl/data",
                "/etc/sctl/state",
                "sctl.toml",
            ),
            (Layout::Systemd, "x86_64", "/var/lib/sctl", "", "relay.toml"),
        ];
        for (layout, target, data_dir, state_dir, config_name) in units {
            let mut config: Config = toml::from_str("").unwrap();
            config.server.data_dir = data_dir.to_string();
            config.server.state_dir = state_dir.to_string();
            let install = InstallInfo::defaults(layout, target);
            let config_file = format!("/etc/sctl/{config_name}");
            let gate = Gate::for_agent(&config, Some(&config_file), Some(&install));
            let dirs = gate.dirs_now();
            // A match here refuses at the gate's first step, before any
            // handle is taken: no refusal opens the file.
            let rule = |p: &Path| gate.rule_for(&components(p), &dirs);
            let unit = format!("{} with data in {data_dir}", layout.as_str());

            let state = Path::new(config.server.state_dir());
            let rollback = &install.rollback_dir;
            for refused in [
                rollback.join("sctl-server"),
                Path::new("/overlay/upper").join(rollback.strip_prefix("/").unwrap()),
                state.join("infra-secrets.json"),
                state.join("infra-monitor.json"),
                state.join("tls_pins.json"),
                state.join("upgrade/state.json"),
                Path::new(data_dir).join("sessions/journal.jsonl"),
                PathBuf::from(&config_file),
                PathBuf::from("/etc/sctl/install.json"),
                PathBuf::from(format!("/tmp/{config_name}.backup.20261021")),
            ] {
                assert!(
                    rule(&refused).is_some(),
                    "{unit}: {} passed",
                    refused.display()
                );
            }
            // Its panic log is refused only where data_dir is state_dir.
            let panic_log = Path::new(data_dir).join("last_panic.log");
            assert_eq!(
                rule(&panic_log).is_some(),
                state_dir.is_empty(),
                "{unit}: {}",
                panic_log.display()
            );
            // The payloads hold no keys and stay readable.
            for payload in install.files.values() {
                assert_eq!(rule(payload), None, "{unit}: {}", payload.display());
            }
        }
    }

    #[test]
    fn the_matcher_works_on_paths_alone() {
        let gate = Gate::new(
            vec![
                (PathBuf::from("/etc/sctl"), "the agent's directory"),
                (
                    PathBuf::from("/usr/local/lib/sctl/data"),
                    "the agent's state directory",
                ),
            ],
            Some(PathBuf::from("/etc/sctl/relay.toml")),
        );
        let dirs = gate.dirs_now();
        let rule = |p: &str| gate.rule_for(&components(Path::new(p)), &dirs);
        for refused in [
            "/overlay/upper/etc/sctl/sctl.toml",
            "/rom/etc/sctl/install.json",
            "/etc/sctl",
            "/overlay/upper/usr/local/lib/sctl/data/infra-secrets.json",
            "/tmp/sctl.toml.backup.20261021",
            "/tmp/sctl-we826-refresh-rollback/state/infra-secrets.json",
            "/home/dev/relay.toml.old",
            "/proc/1/task/1/environ",
            "/proc/thread-self/fd/3",
            "/proc/12/map_files/400000-401000",
        ] {
            assert!(rule(refused).is_some(), "{refused} passed");
        }
        for allowed in [
            "/home/dev/x",
            "/etc/sctl-old/notes",
            "/usr/local/lib/sctl/sctl-server-mips_24kc.gz",
            "/proc/net/arp",
            "/proc/self/maps",
            "/proc/1/root/etc/hostname",
            "/tmp/my-sctl.toml",
        ] {
            assert_eq!(rule(allowed), None, "{allowed} refused");
        }
    }

    #[test]
    fn no_list_entry_matches_a_process_rule() {
        let samples = [
            "eth0", "lo", "proc", "self", "1", "fd", "environ", "kcore", "task",
        ];
        for entry in PSEUDO_FILES {
            let mut shapes: Vec<String> =
                samples.iter().map(|s| entry.replace("<if>", s)).collect();
            // A file under /proc may lead through /proc/self or a pid: its
            // real path carries one.
            if let Some(rest) = entry.strip_prefix("/proc/") {
                shapes.push(format!("/proc/1/{rest}"));
                shapes.push(format!("/proc/self/{rest}"));
                shapes.push(format!("/proc/1/task/1/{rest}"));
            }
            for shape in shapes {
                assert_eq!(
                    process_rule(&components(Path::new(&shape))),
                    None,
                    "{shape}"
                );
            }
            assert!(on_the_list(&components(Path::new(entry))), "{entry}");
        }
    }

    #[test]
    fn the_list_matches_the_path_as_given_exactly() {
        let on = |p: &str| on_the_list(&components(Path::new(p)));
        assert!(on("/proc/net/arp"));
        assert!(on("/proc//net/./arp"));
        assert!(on("/sys/class/net/wwan0/operstate"));
        assert!(!on("/proc/self/net/arp"));
        assert!(!on("/proc/net/arp/x"));
        assert!(!on("/proc/net"));
        assert!(!on("/sys/class/net/address"));
        assert!(!on("/rom/proc/net/arp"));
    }

    #[test]
    fn a_reopen_reads_only_the_file_judged() {
        let dir = scratch("reopen");
        let file = dir.join("log.txt");
        std::fs::write(&file, b"first").unwrap();
        // Held open, so its inode cannot be reused for the replacement.
        let opened = plain_gate().open(&file, Purpose::Move).unwrap();
        let (mut again, _) = reopen_judged(&file, opened.id).unwrap();
        let mut text = String::new();
        again.read_to_string(&mut text).unwrap();
        assert_eq!(text, "first");

        // Replaced by another inode.
        let next = dir.join("next.txt");
        std::fs::write(&next, b"other").unwrap();
        std::fs::rename(&next, &file).unwrap();
        assert!(matches!(
            reopen_judged(&file, opened.id),
            Err(Reopen::Changed(_))
        ));

        // Replaced by a FIFO: refused without waiting.
        std::fs::remove_file(&file).unwrap();
        mkfifo(&file);
        let (path, id) = (file.clone(), opened.id);
        let r = answers_at_once(Some(&file), move || reopen_judged(&path, id).map(|_| ()));
        assert!(matches!(r, Err(Reopen::Changed(_))));

        // Gone.
        std::fs::remove_file(&file).unwrap();
        assert!(matches!(
            reopen_judged(&file, opened.id),
            Err(Reopen::Changed(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_deleted_file_held_open_is_not_found() {
        let dir = scratch("deleted");
        let file = dir.join("gone.txt");
        std::fs::write(&file, b"held").unwrap();
        let held = File::open(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        // Through a link to the descriptor: the real path says "(deleted)"
        // and no link is left.
        let link = dir.join("held");
        symlink(format!("/proc/self/fd/{}", held.as_raw_fd()), &link).unwrap();
        let gate = Gate::new(vec![], None);
        assert!(matches!(
            gate.open(&link, Purpose::Read),
            Err(Refusal::NotFound)
        ));
        drop(held);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_refusal_is_one_file_read_entry_naming_its_rule() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let log = ActivityLog::new(16, tx);
        let refusal = plain_gate()
            .open(Path::new("/proc/self/environ"), Purpose::Read)
            .unwrap_err();
        record(
            &log,
            ActivitySource::Rest,
            "/proc/self/environ",
            &refusal,
            None,
        )
        .await;
        // Not refusals: no entry.
        record(
            &log,
            ActivitySource::Rest,
            "/nope",
            &Refusal::NotFound,
            None,
        )
        .await;
        record(
            &log,
            ActivitySource::Rest,
            "/tmp",
            &Refusal::IsDirectory,
            None,
        )
        .await;
        let entries = log.read_since(0, 10).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].activity_type, ActivityType::FileRead);
        assert_eq!(entries[0].summary, "read refused: /proc/self/environ");
        let detail = entries[0].detail.as_ref().unwrap();
        assert_eq!(detail["code"], "PROTECTED_PATH");
        assert_eq!(detail["rule"], "a process's environment");
        assert_eq!(detail["matched"], "path");
    }
}
