//! The name the agent's tools run by (TRD-9 3.6). At `serve` start the agent
//! makes `/usr/bin/sctl` run its tools, whatever its layout, so a command
//! stored for any unit needs no layout's path. It is a stable name, not a
//! stable install path: `gz-tmp` and `ramboot` units still run the agent
//! from `/tmp`.
//!
//! | Layout | The name |
//! |---|---|
//! | `gz-tmp`, `ramboot` (the agent under `/tmp`) | the guarded wrapper ([`wrapper_text`]): it runs the loader's words, if any, the binary and `"$@"` only while root alone can change those files |
//! | the same, a binary root alone can change, every directory above it included | a symbolic link to the running binary |
//! | `usr-bin`, `systemd` | nothing: the installed binary, in a root-owned system path, is the name (`/usr/local/bin/sctl` on the relay) |
//! | no `install.json` | nothing |
//!
//! Not a link where the agent runs from `/tmp`: while the agent is held
//! down, its files there may be gone, and a link would point root into a
//! directory anyone can write. The wrapper checks, each time it runs, that
//! each file it runs is a regular file owned by root that no one else can
//! write, in a directory owned by root that no one else can write; else it
//! says why on stderr and exits 126, running nothing.
//!
//! The agent writes the name only when it is absent, a link, or its own
//! wrapper (known by the marker) with other content, through a temporary
//! entry renamed over it. It never replaces a regular file that is not its
//! wrapper, and it leaves a name already right alone, so a restart writes
//! nothing to flash. A `ramboot` unit without `helper_prefix` whose
//! `ramboot.conf` runs its agent under a loader (`LOADER` or
//! `MUSL_LIBC_URL`) gets no name, nor does one whose `ramboot.conf` cannot
//! be read: `current_exe()` is then the loader, or may be, not the agent,
//! and the fleet reaches such a unit's tools through the agent path it
//! resolves per unit. `GET /api/health` and `sctl install-info` report the
//! path as `cli`; it is absent when the agent could not make the name, and a
//! log line says why. `SCTL_CLI_PATH` moves the name for tests and benches,
//! as `SCTL_INSTALL_JSON` moves `install.json`.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::install::{InstallInfo, Layout, Running};

/// Where the name is on every unit.
pub const DEFAULT_PATH: &str = "/usr/bin/sctl";
/// What moves the name, for tests and benches.
pub const PATH_ENV: &str = "SCTL_CLI_PATH";
/// The wrapper's second line, by which the agent knows its own wrapper, and
/// `rundev.sh` knows it too. It never changes: an agent knows the wrappers
/// older agents wrote by it.
pub const MARKER: &str = "# sctl: the agent writes this file at start";
/// The wrapper's mode.
const WRAPPER_MODE: u32 = 0o755;
/// Longer than any wrapper: a regular file longer than this is not one,
/// and a wrapper that would be longer is not written.
const MAX_WRAPPER_BYTES: u64 = 4096;
/// The wrapper's guard, after its marker: `root_only <path> <kind>` goes on
/// only when the path is a `directory` or a `regular file`, not a link,
/// owned by root (uid 0) and writable by neither its group nor others, an
/// access list (`+` after the mode) counting as writable; else it says why
/// on stderr and exits 126. POSIX sh as BusyBox runs it: `ls -ldn` and
/// `test`, no `stat`, which a BusyBox may lack. `set -f` keeps `ls`'s
/// fields from being globbed.
pub const GUARD: &str = concat!(
    "# It runs the agent only while root alone can change its files.\n",
    "set -f\n",
    "root_only() {\n",
    "\tif [ \"$2\" = directory ]; then [ -d \"$1\" ]; else [ -f \"$1\" ]; fi &&\n",
    "\t\t[ ! -L \"$1\" ] &&\n",
    "\t\tset -- \"$1\" \"$2\" $(ls -ldn \"$1\" 2>/dev/null) &&\n",
    "\t\t[ \"$5\" = 0 ] &&\n",
    "\t\tcase $3 in ?????-??-?|?????-??-?.) return 0 ;; esac\n",
    "\tprintf 'sctl: not run: %s is not a %s owned by root and writable by no one else\\n' \"$1\" \"$2\" >&2\n",
    "\texit 126\n",
    "}\n",
);
/// What the log says of a regular file the agent never replaces.
const NOT_THE_WRAPPER: &str = "a regular file that is not the agent's wrapper";

/// What the name must be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// The layout's own binary runs the tools (`usr-bin`, `systemd`):
    /// nothing is made.
    Installed(PathBuf),
    /// A symbolic link to this binary, which root alone can change, every
    /// directory above it included ([`root_alone`]).
    Link(PathBuf),
    /// The guarded wrapper's whole text, and the binary it runs.
    Wrapper { text: Vec<u8>, binary: PathBuf },
}

impl Want {
    /// The guarded wrapper that runs `binary` through `loader`.
    pub fn wrapper(loader: &[String], binary: &Path) -> Self {
        Self::Wrapper {
            text: wrapper_text(loader, binary),
            binary: binary.to_path_buf(),
        }
    }

    /// For the log.
    fn describe(&self) -> String {
        match self {
            Self::Installed(path) => format!("the installed binary {}", path.display()),
            Self::Link(target) => format!("a link to {}", target.display()),
            Self::Wrapper { binary, .. } => {
                format!(
                    "the agent's guarded wrapper, which runs {}",
                    binary.display()
                )
            }
        }
    }
}

/// The name, made or found right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    /// What runs the agent's tools: `cli`.
    pub path: PathBuf,
    /// Whether this start wrote it.
    pub written: bool,
}

/// Where the name is: `SCTL_CLI_PATH`, else `/usr/bin/sctl`.
pub fn path() -> PathBuf {
    path_from(std::env::var_os(PATH_ENV))
}

fn path_from(var: Option<OsString>) -> PathBuf {
    var.filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_PATH), PathBuf::from)
}

/// What the name must be on this install, for the agent that runs as
/// `running` ([`InstallInfo::running`]).
pub fn want(install: &InstallInfo, running: &Running) -> Result<Want, String> {
    match install.layout {
        Layout::UsrBin | Layout::Systemd => {
            let server = install
                .files
                .get("server")
                .ok_or_else(|| "install.json names no server file".to_string())?;
            is_file(server, "the installed binary")?;
            Ok(Want::Installed(server.clone()))
        }
        Layout::GzTmp => link_or_wrapper(&running.binary),
        Layout::Ramboot if running.loader.is_empty() => {
            // The running binary's name, unless ramboot.conf has the boot
            // fetcher run the agent under a loader that install.json does
            // not name (a WE826 whose install.json predates 0.6.9, during
            // TRD-8's trial): `current_exe()` is then the loader, and a name
            // for it would run the loader, not the tools. Such a unit gets
            // none, nor one whose ramboot.conf cannot be read (TRD-9 3.6):
            // the fleet reaches its tools through the agent path it resolves
            // per unit.
            let loader = install
                .ramboot_loader()
                .map_err(|e| format!("{e}: whether the agent runs under a loader is not known"))?;
            if let Some(key) = loader {
                return Err(format!(
                    "{} runs the agent under a loader ({key}) that install.json does not name (helper_prefix): the running binary, {}, is then the loader, and a name for it would not run the tools",
                    install.ramboot_conf_path().display(),
                    running.binary.display()
                ));
            }
            link_or_wrapper(&running.binary)
        }
        Layout::Ramboot => {
            is_file(&running.binary, "the binary the loader runs")?;
            guarded(&running.loader, &running.binary)
        }
    }
}

/// The name for a binary the kernel runs itself: a link to it where root
/// alone can change it ([`root_alone`]); anywhere else, the agent under
/// `/tmp` above all, the guarded wrapper.
fn link_or_wrapper(binary: &Path) -> Result<Want, String> {
    is_file(binary, "the running binary")?;
    if root_alone(binary) {
        Ok(Want::Link(binary.to_path_buf()))
    } else {
        guarded(&[], binary)
    }
}

/// The guarded wrapper for `binary` run through `loader`. Each file it runs
/// is checked by an absolute path, and a wrapper too long to be known again
/// at the next start is not written.
fn guarded(loader: &[String], binary: &Path) -> Result<Want, String> {
    for (path, what) in loader
        .first()
        .map(|program| (Path::new(program), "the loader"))
        .into_iter()
        .chain([(binary, "the binary")])
    {
        if !path.is_absolute() {
            return Err(format!(
                "{what} {} is not an absolute path: the wrapper could not check it",
                path.display()
            ));
        }
    }
    let want = Want::wrapper(loader, binary);
    if let Want::Wrapper { text, .. } = &want {
        if text.len() as u64 > MAX_WRAPPER_BYTES {
            return Err(format!(
                "the wrapper for {} would be {} bytes, over {MAX_WRAPPER_BYTES}",
                binary.display(),
                text.len()
            ));
        }
    }
    Ok(want)
}

/// Whether root alone can change what `binary` is: a regular file owned by
/// root that neither its group nor others can write, under directories
/// each owned by root that neither their group nor others can write, from
/// `/` down. A link to such a binary is as safe as the binary. `/tmp`,
/// which anyone can write, is never one of them.
pub fn root_alone(binary: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let roots = |meta: &std::fs::Metadata| meta.uid() == 0 && meta.mode() & 0o022 == 0;
    let Ok(real) = std::fs::canonicalize(binary) else {
        return false;
    };
    std::fs::symlink_metadata(&real).is_ok_and(|meta| meta.is_file() && roots(&meta))
        && real.ancestors().skip(1).all(|dir| {
            std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir() && roots(&meta))
        })
}

fn is_file(path: &Path, what: &str) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(format!("{what} {} is not a file", path.display())),
        Err(e) => Err(format!("{what} {}: {e}", path.display())),
    }
}

/// The guarded wrapper's text: `#!/bin/sh`, [`MARKER`], the guard
/// ([`GUARD`]), then for the loader, if any, and the binary, a check of the
/// directory that holds it and of the file itself, then `exec` of the
/// loader's words, the binary and `"$@"`. Each path and word is in single
/// quotes: the shell expands nothing inside.
pub fn wrapper_text(loader: &[String], binary: &Path) -> Vec<u8> {
    let mut text = Vec::with_capacity(GUARD.len() + 320);
    text.extend_from_slice(b"#!/bin/sh\n");
    text.extend_from_slice(MARKER.as_bytes());
    text.push(b'\n');
    text.extend_from_slice(GUARD.as_bytes());
    let mut checked: Vec<&Path> = Vec::new();
    for program in loader.first().map(Path::new).into_iter().chain([binary]) {
        let dir = program
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("/"));
        for (path, kind) in [(dir, &b" directory\n"[..]), (program, b" 'regular file'\n")] {
            if checked.contains(&path) {
                continue;
            }
            checked.push(path);
            text.extend_from_slice(b"root_only ");
            quote_into(&mut text, path.as_os_str().as_bytes());
            text.extend_from_slice(kind);
        }
    }
    text.extend_from_slice(b"exec");
    for word in loader {
        text.push(b' ');
        quote_into(&mut text, word.as_bytes());
    }
    text.push(b' ');
    quote_into(&mut text, binary.as_os_str().as_bytes());
    text.extend_from_slice(b" \"$@\"\n");
    text
}

/// `word` in single quotes, each quote in it closed, escaped and reopened:
/// the shell expands nothing inside.
fn quote_into(out: &mut Vec<u8>, word: &[u8]) {
    out.push(b'\'');
    for &byte in word {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
}

/// Whether `text` begins as the agent's wrapper does: `#!/bin/sh`, then the
/// marker line.
fn is_wrapper(text: &[u8]) -> bool {
    text.strip_prefix(b"#!/bin/sh\n".as_slice())
        .and_then(|rest| rest.strip_prefix(MARKER.as_bytes()))
        .is_some_and(|rest| rest.starts_with(b"\n"))
}

/// What is at the name now.
enum Found {
    Absent,
    /// A symbolic link, and where it points as written.
    Link(PathBuf),
    /// The agent's own wrapper: its text and mode.
    Wrapper {
        text: Vec<u8>,
        mode: u32,
    },
    /// Anything the agent never replaces, said for the log.
    Other(&'static str),
}

fn inspect(name: &Path) -> Result<Found, String> {
    let meta = match std::fs::symlink_metadata(name) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::Absent),
        Err(e) => return Err(format!("{}: {e}", name.display())),
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        return std::fs::read_link(name)
            .map(Found::Link)
            .map_err(|e| format!("{}: {e}", name.display()));
    }
    if kind.is_dir() {
        return Ok(Found::Other("a directory"));
    }
    if !kind.is_file() {
        return Ok(Found::Other("not a regular file"));
    }
    if meta.len() > MAX_WRAPPER_BYTES {
        return Ok(Found::Other(NOT_THE_WRAPPER));
    }
    // Neither a link nor a FIFO swapped in since the look above is followed
    // or waited on.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(name)
        .map_err(|e| format!("{}: {e}", name.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{}: {e}", name.display()))?;
    if !meta.is_file() {
        return Ok(Found::Other("not a regular file"));
    }
    let mut text = Vec::new();
    file.take(MAX_WRAPPER_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|e| format!("{}: {e}", name.display()))?;
    if is_wrapper(&text) {
        Ok(Found::Wrapper {
            text,
            mode: meta.permissions().mode() & 0o7777,
        })
    } else {
        Ok(Found::Other(NOT_THE_WRAPPER))
    }
}

/// Whether the name already is what `want` makes. `Err` for an entry the
/// agent never replaces.
fn is_right(name: &Path, want: &Want) -> Result<bool, String> {
    Ok(match (inspect(name)?, want) {
        (Found::Other(what), _) => {
            return Err(format!("{} is {what}: left as it is", name.display()))
        }
        (Found::Link(target), Want::Link(wanted)) => target == *wanted,
        (Found::Wrapper { text, mode }, Want::Wrapper { text: wanted, .. }) => {
            text == *wanted && mode == WRAPPER_MODE
        }
        _ => false,
    })
}

/// Make the name (`serve`): write it when it is absent, a link, or the
/// agent's own wrapper, and not already what `want` makes.
pub fn make(name: &Path, want: &Want) -> Result<Made, String> {
    if let Want::Installed(path) = want {
        return Ok(Made {
            path: path.clone(),
            written: false,
        });
    }
    let temp = temp_for(name)?;
    // An entry a start cut short left behind.
    clear(&temp)?;
    if is_right(name, want)? {
        return Ok(Made {
            path: name.to_path_buf(),
            written: false,
        });
    }
    match want {
        Want::Link(target) => std::os::unix::fs::symlink(target, &temp)
            .map_err(|e| format!("{}: {e}", temp.display()))?,
        Want::Wrapper { text, .. } => write_wrapper(&temp, text)?,
        Want::Installed(_) => {}
    }
    std::fs::rename(&temp, name).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("rename {} to {}: {e}", temp.display(), name.display())
    })?;
    Ok(Made {
        path: name.to_path_buf(),
        written: true,
    })
}

/// The name when it already is what `want` makes, read without writing
/// (`sctl install-info`).
pub fn check(name: &Path, want: &Want) -> Result<PathBuf, String> {
    if let Want::Installed(path) = want {
        return Ok(path.clone());
    }
    if is_right(name, want)? {
        Ok(name.to_path_buf())
    } else {
        Err(format!(
            "{} is not {} yet: the agent makes it at start",
            name.display(),
            want.describe()
        ))
    }
}

/// `.<name>.new` beside the name.
fn temp_for(name: &Path) -> Result<PathBuf, String> {
    let file = name
        .file_name()
        .ok_or_else(|| format!("{} names no file", name.display()))?;
    let mut temp = OsString::from(".");
    temp.push(file);
    temp.push(".new");
    Ok(name.with_file_name(temp))
}

fn clear(temp: &Path) -> Result<(), String> {
    match std::fs::remove_file(temp) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", temp.display())),
    }
}

/// The wrapper at `temp`, mode 0755 whatever the umask, and on flash before
/// the rename makes it the name: a start cut short leaves the old name or
/// this one, never a part of it.
fn write_wrapper(temp: &Path, text: &[u8]) -> Result<(), String> {
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(WRAPPER_MODE)
            .open(temp)?;
        file.write_all(text)?;
        file.set_permissions(std::fs::Permissions::from_mode(WRAPPER_MODE))?;
        file.sync_all()
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(temp);
        format!("{}: {e}", temp.display())
    })
}

/// At `serve` start: make the name for this install. The path `cli`
/// reports, or `None`, with a log line that says why. `install_error` is
/// why `install` is `None` when `install.json` exists but cannot be used.
pub fn at_start(install: Option<&InstallInfo>, install_error: Option<&str>) -> Option<PathBuf> {
    let Some(install) = install else {
        if let Some(e) = install_error {
            warn!("No name for the agent's tools: install.json is unusable ({e}); health reports no cli");
        } else {
            info!(
                "No install.json: the agent makes no name for its tools, and health reports no cli"
            );
        }
        return None;
    };
    let name = path();
    let made = install
        .running()
        .and_then(|running| want(install, &running))
        .and_then(|want| make(&name, &want).map(|made| (made, want)));
    match made {
        Ok((made, want)) => {
            if made.written {
                info!(
                    cli = %made.path.display(),
                    "The agent's tools run as {}: {} written",
                    made.path.display(),
                    want.describe()
                );
            } else {
                info!(
                    cli = %made.path.display(),
                    "The agent's tools run as {} ({})",
                    made.path.display(),
                    want.describe()
                );
            }
            Some(made.path)
        }
        Err(e) => {
            warn!("No name for the agent's tools: {e}; health reports no cli");
            None
        }
    }
}

/// What `sctl install-info` reports as `cli`: the name when it already is
/// what the agent makes for the binary that answers. Nothing is written.
pub fn reported(install: &InstallInfo) -> Result<PathBuf, String> {
    let running = install.running()?;
    let want = want(install, &running)?;
    check(&path(), &want)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// A scratch directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sctl-cli-name-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// A stand-in binary, a file of a few bytes.
        fn file(&self, name: &str) -> PathBuf {
            let path = self.path(name);
            std::fs::write(&path, b"\x7fELF stand-in").unwrap();
            path
        }

        /// What is in the directory, sorted.
        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The entry's inode and modification time, the entry itself and not
    /// what a link points to.
    fn stamp(path: &Path) -> (u64, i64, i64) {
        let meta = std::fs::symlink_metadata(path).unwrap();
        (meta.ino(), meta.mtime(), meta.mtime_nsec())
    }

    fn loader_words() -> Vec<String> {
        vec![
            "/tmp/sctl/lib/libc.so".into(),
            "--library-path".into(),
            "/tmp/sctl/lib".into(),
        ]
    }

    #[test]
    fn the_name_is_sctl_cli_path_or_usr_bin_sctl() {
        assert_eq!(path_from(None), PathBuf::from("/usr/bin/sctl"));
        assert_eq!(
            path_from(Some(OsString::new())),
            PathBuf::from("/usr/bin/sctl")
        );
        assert_eq!(
            path_from(Some(OsString::from("/tmp/bench/sctl"))),
            PathBuf::from("/tmp/bench/sctl")
        );
    }

    #[test]
    fn an_absent_name_gets_the_link() {
        let s = Scratch::new("absent");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        let made = make(&name, &Want::Link(binary.clone())).unwrap();
        assert_eq!(
            made,
            Made {
                path: name.clone(),
                written: true
            }
        );
        assert_eq!(std::fs::read_link(&name).unwrap(), binary);
        assert_eq!(
            s.entries(),
            ["sctl", "sctl-server"],
            "no temporary entry left"
        );
    }

    #[test]
    fn a_link_elsewhere_is_replaced() {
        let s = Scratch::new("elsewhere");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        std::os::unix::fs::symlink("/tmp/sctl/old-sctl-server", &name).unwrap();
        assert!(make(&name, &Want::Link(binary.clone())).unwrap().written);
        assert_eq!(std::fs::read_link(&name).unwrap(), binary);
        // A dangling link, and one to the right file by another path, too.
        std::os::unix::fs::symlink(&binary, s.path("alias")).unwrap();
        std::fs::remove_file(&name).unwrap();
        std::os::unix::fs::symlink(s.path("alias"), &name).unwrap();
        assert!(make(&name, &Want::Link(binary.clone())).unwrap().written);
        assert_eq!(std::fs::read_link(&name).unwrap(), binary);
    }

    #[test]
    fn the_right_link_is_left_alone() {
        let s = Scratch::new("right-link");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        std::os::unix::fs::symlink(&binary, &name).unwrap();
        let before = stamp(&name);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let made = make(&name, &Want::Link(binary.clone())).unwrap();
        assert!(!made.written, "a restart writes nothing");
        assert_eq!(stamp(&name), before, "the same inode and mtime");
        assert_eq!(s.entries(), ["sctl", "sctl-server"]);
    }

    #[test]
    fn a_regular_file_that_is_not_the_wrapper_is_left_and_gives_no_cli() {
        let s = Scratch::new("regular");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        // A binary someone copied there, and a script of a person's own.
        for text in [
            &b"\x7fELF a real binary"[..],
            b"#!/bin/sh\nexec /opt/sctl \"$@\"\n",
        ] {
            std::fs::write(&name, text).unwrap();
            let before = stamp(&name);
            for want in [
                Want::Link(binary.clone()),
                Want::wrapper(&loader_words(), &binary),
            ] {
                let err = make(&name, &want).unwrap_err();
                assert!(err.contains(NOT_THE_WRAPPER), "{err}");
                assert!(check(&name, &want).is_err());
            }
            assert_eq!(std::fs::read(&name).unwrap(), text);
            assert_eq!(stamp(&name), before);
        }
        // Nor a directory, nor a FIFO, which is never opened.
        std::fs::remove_file(&name).unwrap();
        std::fs::create_dir(&name).unwrap();
        let err = make(&name, &Want::Link(binary.clone())).unwrap_err();
        assert!(err.contains("a directory"), "{err}");
        std::fs::remove_dir(&name).unwrap();
        let fifo = std::ffi::CString::new(name.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path, read by mkfifo only.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let err = make(&name, &Want::Link(binary)).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
    }

    /// The WE826's guarded wrapper, byte for byte: the loader and the
    /// binary are each checked with the directory that holds it before the
    /// loader runs the binary.
    #[test]
    fn a_loader_prefix_gets_the_wrappers_exact_text_mode_0755() {
        let s = Scratch::new("wrapper");
        let name = s.path("sctl");
        let want = Want::wrapper(&loader_words(), Path::new("/tmp/sctl/sctl-server"));
        assert!(make(&name, &want).unwrap().written);
        let text = std::fs::read_to_string(&name).unwrap();
        assert_eq!(
            text,
            r#"#!/bin/sh
# sctl: the agent writes this file at start
# It runs the agent only while root alone can change its files.
set -f
root_only() {
	if [ "$2" = directory ]; then [ -d "$1" ]; else [ -f "$1" ]; fi &&
		[ ! -L "$1" ] &&
		set -- "$1" "$2" $(ls -ldn "$1" 2>/dev/null) &&
		[ "$5" = 0 ] &&
		case $3 in ?????-??-?|?????-??-?.) return 0 ;; esac
	printf 'sctl: not run: %s is not a %s owned by root and writable by no one else\n' "$1" "$2" >&2
	exit 126
}
root_only '/tmp/sctl/lib' directory
root_only '/tmp/sctl/lib/libc.so' 'regular file'
root_only '/tmp/sctl' directory
root_only '/tmp/sctl/sctl-server' 'regular file'
exec '/tmp/sctl/lib/libc.so' '--library-path' '/tmp/sctl/lib' '/tmp/sctl/sctl-server' "$@"
"#
        );
        assert!(
            text.len() <= 800,
            "under 800 bytes on a WE826's flash: {}",
            text.len()
        );
        let mode = std::fs::metadata(&name).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
        assert_eq!(s.entries(), ["sctl"], "no temporary entry left");
    }

    /// Where the kernel runs the binary itself (an XE300), the wrapper
    /// checks the binary and its directory, then runs it.
    #[test]
    fn a_binary_under_tmp_gets_the_guarded_wrapper() {
        let text = wrapper_text(&[], Path::new("/tmp/sctl/sctl-server"));
        let expected = format!(
            "#!/bin/sh\n{MARKER}\n{GUARD}\
             root_only '/tmp/sctl' directory\n\
             root_only '/tmp/sctl/sctl-server' 'regular file'\n\
             exec '/tmp/sctl/sctl-server' \"$@\"\n"
        );
        assert_eq!(String::from_utf8(text).unwrap(), expected);
        // A binary at the root checks `/` as its directory.
        let at_root = String::from_utf8(wrapper_text(&[], Path::new("/sctl"))).unwrap();
        assert!(
            at_root.contains("root_only '/' directory\nroot_only '/sctl' 'regular file'\n"),
            "{at_root}"
        );
    }

    #[test]
    fn the_right_wrapper_is_not_written_again() {
        let s = Scratch::new("right-wrapper");
        let name = s.path("sctl");
        let want = Want::wrapper(&loader_words(), Path::new("/tmp/sctl/sctl-server"));
        assert!(make(&name, &want).unwrap().written);
        let before = stamp(&name);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!make(&name, &want).unwrap().written);
        assert_eq!(stamp(&name), before, "the same inode and mtime");
        assert_eq!(check(&name, &want).unwrap(), name);
    }

    #[test]
    fn the_agents_own_wrapper_is_rewritten_when_it_differs() {
        let s = Scratch::new("own-wrapper");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        let want = Want::wrapper(&loader_words(), &binary);
        // Another binary, another loader, a mode a person changed.
        let older = wrapper_text(&["/tmp/sctl/lib/ld.so".to_string()], Path::new("/tmp/x"));
        std::fs::write(&name, &older).unwrap();
        std::fs::set_permissions(&name, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(make(&name, &want).unwrap().written);
        let Want::Wrapper { text, .. } = &want else {
            unreachable!()
        };
        assert_eq!(&std::fs::read(&name).unwrap(), text);
        std::fs::set_permissions(&name, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check(&name, &want).is_err());
        assert!(make(&name, &want).unwrap().written);
        let mode = std::fs::metadata(&name).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
        // A unit that runs its binary itself again gets the link in its place.
        assert!(make(&name, &Want::Link(binary.clone())).unwrap().written);
        assert_eq!(std::fs::read_link(&name).unwrap(), binary);
        // And the link gives way to the wrapper.
        assert!(make(&name, &want).unwrap().written);
        assert_eq!(&std::fs::read(&name).unwrap(), text);
    }

    #[test]
    fn a_temporary_entry_a_cut_start_left_is_removed() {
        let s = Scratch::new("stale");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        std::fs::write(s.path(".sctl.new"), b"half a wrap").unwrap();
        assert!(make(&name, &Want::Link(binary.clone())).unwrap().written);
        std::os::unix::fs::symlink("/nowhere", s.path(".sctl.new")).unwrap();
        assert!(!make(&name, &Want::Link(binary)).unwrap().written);
        assert_eq!(s.entries(), ["sctl", "sctl-server"]);
    }

    #[test]
    fn check_reads_without_writing() {
        let s = Scratch::new("check");
        let binary = s.file("sctl-server");
        let name = s.path("sctl");
        let want = Want::Link(binary.clone());
        let err = check(&name, &want).unwrap_err();
        assert!(err.contains("a link to"), "{err}");
        assert_eq!(s.entries(), ["sctl-server"], "nothing made");
        std::os::unix::fs::symlink("/tmp/sctl/other", &name).unwrap();
        assert!(check(&name, &want).is_err());
        assert_eq!(
            std::fs::read_link(&name).unwrap(),
            Path::new("/tmp/sctl/other")
        );
        std::fs::remove_file(&name).unwrap();
        std::os::unix::fs::symlink(&binary, &name).unwrap();
        assert_eq!(check(&name, &want).unwrap(), name);
        let installed = Want::Installed(binary.clone());
        assert_eq!(check(&s.path("absent"), &installed).unwrap(), binary);
        assert_eq!(make(&s.path("absent"), &installed).unwrap().path, binary);
        assert!(!s.path("absent").exists(), "nothing made for usr-bin");
    }

    /// Each layout's name, the running binary and the boot fetcher's config
    /// stood in by files in a scratch directory.
    #[test]
    fn each_layout_wants_its_name() {
        let s = Scratch::new("layouts");
        let server = s.file("sctl-server");
        let loader = s.file("libc.so");
        let conf = s.path("ramboot.conf");
        std::fs::write(
            &conf,
            format!("RUN_DIR='{}'\nBIN='{}'\n", s.0.display(), server.display()),
        )
        .unwrap();
        let plain = Running {
            binary: server.clone(),
            loader: Vec::new(),
        };

        for layout in [Layout::UsrBin, Layout::Systemd] {
            let mut install = InstallInfo::defaults(layout, "x86_64");
            install.files.insert("server".into(), server.clone());
            assert_eq!(
                want(&install, &plain).unwrap(),
                Want::Installed(server.clone())
            );
            install.files.insert("server".into(), s.path("missing"));
            assert!(
                want(&install, &plain).is_err(),
                "{layout:?}: no binary, no cli"
            );
        }
        assert_eq!(
            InstallInfo::defaults(Layout::Systemd, "x86_64").files["server"],
            PathBuf::from("/usr/local/bin/sctl"),
            "the relay's cli"
        );

        // A binary in a directory anyone may come to own, as /tmp/sctl:
        // the guarded wrapper.
        let gz_tmp = InstallInfo::defaults(Layout::GzTmp, "mips_24kc");
        assert_eq!(want(&gz_tmp, &plain).unwrap(), Want::wrapper(&[], &server));
        let gone = Running {
            binary: s.path("deleted"),
            loader: Vec::new(),
        };
        assert!(
            want(&gz_tmp, &gone).is_err(),
            "no name for a binary that is not there"
        );
        // A binary root alone can change, every directory above it
        // included: the link.
        match ["/usr/bin/env", "/bin/sh"]
            .map(PathBuf::from)
            .into_iter()
            .find(|path| root_alone(path))
        {
            Some(system) => {
                let running = Running {
                    binary: system.clone(),
                    loader: Vec::new(),
                };
                assert_eq!(want(&gz_tmp, &running).unwrap(), Want::Link(system));
            }
            None => eprintln!("no binary here that root alone can change: no link to show"),
        }

        let mut ramboot = InstallInfo::defaults(Layout::Ramboot, "mipsel_24kc");
        ramboot.ramboot_conf = Some(conf.clone());
        assert_eq!(want(&ramboot, &plain).unwrap(), Want::wrapper(&[], &server));

        ramboot.helper_prefix = vec![
            loader.display().to_string(),
            "--library-path".into(),
            s.0.display().to_string(),
        ];
        let running = ramboot.running().unwrap();
        assert_eq!(running.binary, server, "ramboot.conf's BIN");
        assert_eq!(
            want(&ramboot, &running).unwrap(),
            Want::wrapper(&ramboot.helper_prefix, &server)
        );
    }

    /// A WE826 whose install.json does not name its loader, while its
    /// ramboot.conf runs the agent under one, runs the agent as the loader:
    /// no name is made for the loader, and nothing is written. Nor for a
    /// unit whose ramboot.conf cannot be read: nothing then says whether a
    /// loader runs. The fleet reaches such a unit's tools through the agent
    /// path it resolves per unit.
    #[test]
    fn ramboot_under_a_loader_install_json_does_not_name_gets_no_name() {
        let s = Scratch::new("unnamed-loader");
        let server = s.file("sctl-server");
        let loader = s.file("libc.so");
        let conf = s.path("ramboot.conf");
        let mut ramboot = InstallInfo::defaults(Layout::Ramboot, "mips_24kc");
        ramboot.ramboot_conf = Some(conf.clone());
        let under_loader = Running {
            binary: loader,
            loader: Vec::new(),
        };
        // A WE826's ramboot.conf names the musl libc the fetcher runs as the
        // loader; one may name the loader itself.
        for (key, line) in [
            (
                "MUSL_LIBC_URL",
                "MUSL_LIBC_URL='http://192.0.2.10/libc.so.gz'",
            ),
            ("LOADER", "LOADER='/tmp/sctl/lib/libc.so'"),
        ] {
            std::fs::write(&conf, format!("BIN='{}'\n{line}\n", server.display())).unwrap();
            let err = want(&ramboot, &under_loader).unwrap_err();
            assert!(err.contains("helper_prefix"), "{err}");
            assert!(err.contains(key), "{err}");
        }
        ramboot.ramboot_conf = Some(s.path("absent.conf"));
        let err = want(&ramboot, &under_loader).unwrap_err();
        assert!(err.contains("not known"), "{err}");
    }

    /// A `ramboot` unit whose ramboot.conf has the kernel run the binary
    /// itself gets the guarded wrapper for the running binary, whatever
    /// ramboot.conf's `BIN` line says: none (the fetcher's default), one
    /// written with the fetcher's variables, or another binary (an agent
    /// started by hand).
    #[test]
    fn ramboot_without_a_loader_names_the_running_binary_whatever_bin_says() {
        let s = Scratch::new("no-loader");
        let server = s.file("sctl-server");
        let conf = s.path("ramboot.conf");
        let mut ramboot = InstallInfo::defaults(Layout::Ramboot, "mipsel_24kc");
        ramboot.ramboot_conf = Some(conf.clone());
        let plain = Running {
            binary: server.clone(),
            loader: Vec::new(),
        };
        for text in [
            "RUN_DIR='/tmp/sctl'\nSERVER_URL='http://192.0.2.10/s'\n",
            "RUN_DIR='/tmp/sctl'\nBIN=$RUN_DIR/sctl-server\n",
            "BIN='/tmp/sctl/another-sctl-server'\nMUSL_LIBC_URL=''\n",
        ] {
            std::fs::write(&conf, text).unwrap();
            assert_eq!(
                want(&ramboot, &plain).unwrap(),
                Want::wrapper(&[], &server),
                "{text}"
            );
        }
    }

    /// A link is as safe as what it names only where root alone can change
    /// the binary and every directory above it: never under /tmp, which
    /// anyone can write, nor in a directory another account owns.
    #[test]
    fn root_alone_holds_only_for_a_binary_and_a_path_that_are_root_s() {
        let s = Scratch::new("root-alone");
        let binary = s.file("sctl-server");
        if std::fs::metadata(&binary).unwrap().uid() != 0 {
            assert!(!root_alone(&binary), "this account's own file");
        }
        assert!(!root_alone(&s.path("missing")));
        assert!(!root_alone(Path::new("/tmp")), "a directory, not a binary");
        if let Some(system) = ["/usr/bin/env", "/bin/sh"]
            .map(PathBuf::from)
            .into_iter()
            .find(|path| root_alone(path))
        {
            // Through a link of this account's own: it names the binary,
            // and the binary is what is judged.
            std::os::unix::fs::symlink(&system, s.path("alias")).unwrap();
            assert!(root_alone(&s.path("alias")));
        }
    }

    /// The wrapper checks each file it runs by an absolute path, and is
    /// never longer than the agent knows its own wrapper to be.
    #[test]
    fn a_wrapper_with_a_relative_path_or_too_long_is_not_wanted() {
        let err = guarded(&["ld.so".into()], Path::new("/tmp/sctl/sctl-server")).unwrap_err();
        assert!(
            err.contains("the loader ld.so is not an absolute path"),
            "{err}"
        );
        let err = guarded(&[], Path::new("sctl-server")).unwrap_err();
        assert!(
            err.contains("the binary sctl-server is not an absolute path"),
            "{err}"
        );
        let deep = format!("/tmp/{}/sctl-server", "d".repeat(4000));
        let err = guarded(&[], Path::new(&deep)).unwrap_err();
        assert!(err.contains("over 4096"), "{err}");
        assert!(guarded(&loader_words(), Path::new("/tmp/sctl/sctl-server")).is_ok());
    }

    #[test]
    fn the_wrapper_quotes_every_word() {
        let text = wrapper_text(
            &[
                "/opt/it's here/ld.so".into(),
                "--x=$(printf EXPANDED)".into(),
            ],
            Path::new("/opt/a b/`sctl`"),
        );
        assert_eq!(
            String::from_utf8(text).unwrap(),
            format!(
                "#!/bin/sh\n{MARKER}\n{GUARD}\
                 root_only '/opt/it'\\''s here' directory\n\
                 root_only '/opt/it'\\''s here/ld.so' 'regular file'\n\
                 root_only '/opt/a b' directory\n\
                 root_only '/opt/a b/`sctl`' 'regular file'\n\
                 exec '/opt/it'\\''s here/ld.so' '--x=$(printf EXPANDED)' '/opt/a b/`sctl`' \"$@\"\n"
            )
        );
        assert!(is_wrapper(&wrapper_text(&[], Path::new("/x"))));
        assert!(!is_wrapper(
            b"#!/bin/sh\n# sctl: the agent writes this file at start, or not\n"
        ));
        assert!(!is_wrapper(MARKER.as_bytes()));
    }
}
