//! The running agent's half of an upgrade: refuse early, fetch and verify
//! the manifest, fetch and verify the artifacts, prove the binary runs,
//! write the rollback set, write the helper's plan, hand off to the
//! detached helper. Nothing here changes an installed file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::fetch::{Fetcher, Target};
use super::install::{InstallInfo, Layout};
use super::manifest::{ArtifactFile, Manifest, MAX_MANIFEST_BYTES};
use super::state::{Handle, Phase, UpgradeState};
use super::version::Version;
use super::{Reason, Refusal, TARGET};

/// A clock before this is "not set" (a box before NTP reads 1970 or 2000).
const CLOCK_SANITY_UNIX: u64 = 1_704_067_200; // 2024-01-01T00:00:00Z

/// `POST /api/upgrade`'s body.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Request {
    pub version: String,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub manifest_url: Option<String>,
    #[serde(default)]
    pub not_before: Option<String>,
    #[serde(default)]
    pub not_after: Option<String>,
    #[serde(default)]
    pub allow_downgrade: bool,
    /// Where a ramboot device's boot fetcher can get the same files.
    #[serde(default)]
    pub mirror_base: Option<String>,
}

/// What the staging needs from the agent.
#[derive(Clone)]
pub struct Context {
    pub state_dir: String,
    pub listen: String,
    pub install: Option<InstallInfo>,
    pub tunnel: Option<crate::config::TunnelConfig>,
    pub trust_keys: Vec<[u8; 32]>,
    pub config_path: Option<String>,
    pub hold: bool,
    pub hold_file: PathBuf,
    pub running_version: String,
}

/// One file the helper moves into place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    pub staged: PathBuf,
    pub target: PathBuf,
    pub sha256: String,
    pub size: u64,
    /// Octal mode, e.g. `0o755`.
    pub mode: u32,
}

/// One file the helper restores on rollback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restore {
    pub backup: PathBuf,
    pub target: PathBuf,
    pub mode: u32,
}

/// The helper's instructions, written to `<stage>/plan.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub v: u32,
    pub request_id: Option<String>,
    pub from_version: String,
    pub to_version: String,
    pub layout: Layout,
    pub swaps: Vec<Swap>,
    pub restores: Vec<Restore>,
    pub restart: String,
    pub health_url: String,
    /// Whether health must show the tunnel connected.
    pub need_tunnel: bool,
    pub min_free_kb: u64,
    pub state_path: PathBuf,
    pub log_path: PathBuf,
}

/// Parse an RFC 3339 instant (`2026-10-02T06:00:00Z`, fractional seconds
/// and `+hh:mm` offsets tolerated) to unix seconds.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let split = rest.find(['Z', 'z', '+', '-'])?;
    let (time, offset) = (&rest[..split], &rest[split..]);
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next().unwrap_or("0").parse().ok()?;
    if t.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset_secs: i64 = match offset {
        "Z" | "z" => 0,
        o => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let (oh, om) = o[1..].split_once(':')?;
            let oh: i64 = oh.parse().ok()?;
            let om: i64 = om.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The checks that refuse a request before anything is fetched. `Ok` is the
/// target's parsed version.
pub fn refuse_early(ctx: &Context, req: &Request) -> Result<Version, Refusal> {
    if ctx.hold {
        return Err(Refusal::new(Reason::Held, "[upgrade] hold = true"));
    }
    if let Some(reason) = super::install::hold_file_reason(&ctx.hold_file) {
        return Err(Refusal::new(
            Reason::Held,
            format!("{}: {reason}", ctx.hold_file.display()),
        ));
    }
    let Some(install) = &ctx.install else {
        return Err(Refusal::new(
            Reason::NoInstallJson,
            "no /etc/sctl/install.json: this device was installed before 0.6.7",
        ));
    };
    let target = Version::parse(&req.version).ok_or_else(|| {
        Refusal::new(
            Reason::ManifestInvalid,
            format!("'{}' is not a version", req.version),
        )
    })?;
    let running = Version::parse(&ctx.running_version);
    if let Some(running) = &running {
        if target < *running && !req.allow_downgrade {
            return Err(Refusal::new(
                Reason::Downgrade,
                format!("{target} is below the running {running}"),
            ));
        }
    }
    if req.not_before.is_some() || req.not_after.is_some() {
        let now = now_unix();
        if now < CLOCK_SANITY_UNIX {
            return Err(Refusal::new(Reason::NoClock, "the clock is not set"));
        }
        let now = i64::try_from(now).unwrap_or(i64::MAX);
        if let Some(nb) = &req.not_before {
            let at = parse_rfc3339(nb).ok_or_else(|| {
                Refusal::new(
                    Reason::ManifestInvalid,
                    format!("not_before '{nb}' is not a time"),
                )
            })?;
            if now < at {
                return Err(Refusal::new(
                    Reason::WindowPassed,
                    format!("the window opens at {nb}"),
                ));
            }
        }
        if let Some(na) = &req.not_after {
            let at = parse_rfc3339(na).ok_or_else(|| {
                Refusal::new(
                    Reason::ManifestInvalid,
                    format!("not_after '{na}' is not a time"),
                )
            })?;
            if now > at {
                return Err(Refusal::new(
                    Reason::WindowPassed,
                    format!("the window closed at {na}"),
                ));
            }
        }
    }
    if install.layout == Layout::Ramboot && req.mirror_base.is_none() {
        return Err(Refusal::new(
            Reason::NoMirror,
            "a ramboot device needs mirror_base: where its boot fetcher can get the files",
        ));
    }
    Ok(target)
}

/// The manifest URL: the request's, or the relay's artifacts route.
pub fn manifest_url(ctx: &Context, req: &Request) -> Result<String, Refusal> {
    if let Some(url) = &req.manifest_url {
        return Ok(url.clone());
    }
    let tunnel_url = ctx
        .tunnel
        .as_ref()
        .and_then(|tc| tc.url.clone())
        .ok_or_else(|| {
            Refusal::new(
                Reason::Internal,
                "no manifest_url and no [tunnel] url to derive it from",
            )
        })?;
    let (tls, rest) = if let Some(r) = tunnel_url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = tunnel_url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(Refusal::new(Reason::Internal, "tunnel url is not ws(s)://"));
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let scheme = if tls { "https" } else { "http" };
    Ok(format!(
        "{scheme}://{authority}/api/tunnel/artifacts/{}/release.json",
        req.version
    ))
}

/// The stage directory for `version`: `<state_dir>/upgrade/stage/<version>/`,
/// or the ramboot cache. One directory per version, so a file left by an
/// earlier release under the same name is never mistaken for a partial
/// download of this one.
pub fn stage_dir(ctx: &Context, version: &str) -> PathBuf {
    match ctx.install.as_ref() {
        Some(i) if i.layout == Layout::Ramboot => i
            .cache_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/sctl/cache")),
        _ => super::state::upgrade_dir(&ctx.state_dir)
            .join("stage")
            .join(version),
    }
}

/// Remove every other version's stage directory (their downloads, helper
/// copies and logs), keeping the space for this one.
fn sweep_other_stages(ctx: &Context, keep: &Path) {
    let root = super::state::upgrade_dir(&ctx.state_dir).join("stage");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != keep && path.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warn!(
                    "upgrade: could not remove old stage {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// Free space on the filesystem holding `path`, in bytes.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut probe = path;
    loop {
        if let Ok(st) = nix::sys::statvfs::statvfs(probe) {
            // `c_ulong` is u32 on the 32-bit targets, u64 here.
            fn widen(v: impl Into<u64>) -> u64 {
                v.into()
            }
            let bsize = widen(st.fragment_size());
            let avail = widen(st.blocks_available());
            return Some(bsize.saturating_mul(avail));
        }
        probe = probe.parent()?;
    }
}

fn need_space(path: &Path, bytes: u64, min_free_kb: u64) -> Result<(), Refusal> {
    let free = free_bytes(path).unwrap_or(u64::MAX);
    let need = bytes.saturating_add(min_free_kb.saturating_mul(1024));
    if free < need {
        return Err(Refusal::new(
            Reason::NoSpace,
            format!(
                "{} has {} KB free, needs {} KB",
                path.display(),
                free / 1024,
                need / 1024
            ),
        ));
    }
    Ok(())
}

/// Run `--version` on a staged server artifact (expanded first when it is
/// gzipped) and compare with the manifest's version.
async fn prove_binary(staged: &Path, gzip: bool, expected: &Version) -> Result<(), Refusal> {
    let probe = std::env::temp_dir().join(format!("sctl-upgrade-probe-{}", std::process::id()));
    let run: PathBuf = if gzip {
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "gzip -dc '{}' > '{}' && chmod 0755 '{}'",
                staged.display(),
                probe.display(),
                probe.display()
            ))
            .status()
            .await
            .map_err(|e| Refusal::new(Reason::BinaryDoesNotRun, format!("gzip: {e}")))?;
        if !status.success() {
            let _ = std::fs::remove_file(&probe);
            return Err(Refusal::new(
                Reason::BinaryDoesNotRun,
                "the payload does not gunzip",
            ));
        }
        probe.clone()
    } else {
        set_mode(staged, 0o755).map_err(|e| Refusal::new(Reason::Internal, e))?;
        staged.to_path_buf()
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(&run).arg("--version").output(),
    )
    .await;
    if gzip {
        let _ = std::fs::remove_file(&probe);
    }
    let output = match output {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(Refusal::new(
                Reason::BinaryDoesNotRun,
                format!("{}: {e}", run.display()),
            ))
        }
        Err(_) => {
            return Err(Refusal::new(
                Reason::BinaryDoesNotRun,
                "--version took over 20s",
            ))
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    match Version::parse(text.trim()) {
        Some(v) if v == *expected => Ok(()),
        Some(v) => Err(Refusal::new(
            Reason::BinaryDoesNotRun,
            format!("the new binary prints {v}, the manifest says {expected}"),
        )),
        None => Err(Refusal::new(
            Reason::BinaryDoesNotRun,
            format!(
                "the new binary did not print a version (exit {:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        )),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

fn mode_for(role: &str) -> u32 {
    match role {
        "server" | "libc" => 0o755,
        _ => 0o644,
    }
}

/// Copy `from` to `to` (create parents), best effort on modes.
fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))
}

/// The new `ramboot.conf`: the current one with every URL and SHA-256 the
/// manifest covers replaced.
pub fn ramboot_conf(current: &str, files: &[ArtifactFile], mirror_base: &str) -> String {
    let mirror_base = mirror_base.trim_end_matches('/');
    let key_of = |role: &str| match role {
        "server" => Some("SERVER"),
        "plugin" => Some("PLUGIN"),
        "libc" => Some("MUSL_LIBC"),
        "libgcc" => Some("LIBGCC"),
        _ => None,
    };
    let mut out = String::new();
    for line in current.lines() {
        let mut replaced = false;
        for f in files {
            let Some(key) = key_of(&f.role) else { continue };
            if line.starts_with(&format!("{key}_URL=")) {
                let _ = writeln!(out, "{key}_URL='{mirror_base}/{}'", f.name);
                replaced = true;
            } else if line.starts_with(&format!("{key}_SHA256=")) {
                let _ = writeln!(out, "{key}_SHA256='{}'", f.sha256);
                replaced = true;
            } else if line.starts_with(&format!("{key}_GZIP=")) {
                let _ = writeln!(out, "{key}_GZIP='{}'", u8::from(f.gzip));
                replaced = true;
            }
            if replaced {
                break;
            }
        }
        if !replaced {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Everything after the early refusal: the outcome is written to the
/// handle either way. Called from the route's spawned task and from
/// `sctl upgrade`.
pub async fn run(ctx: Context, req: Request, handle: Arc<Handle>) {
    let state = handle.current();
    match stage(&ctx, &req, &handle).await {
        Ok(()) => {}
        Err(refusal) => {
            warn!("upgrade: not applied: {refusal}");
            handle.set(state.not_applied(refusal.reason, refusal.detail));
        }
    }
}

async fn stage(ctx: &Context, req: &Request, handle: &Handle) -> Result<(), Refusal> {
    let install = ctx
        .install
        .as_ref()
        .ok_or_else(|| Refusal::new(Reason::NoInstallJson, "no install.json"))?;
    let target_version = Version::parse(&req.version)
        .ok_or_else(|| Refusal::new(Reason::ManifestInvalid, "not a version"))?;
    let url = manifest_url(ctx, req)?;
    let manifest_target =
        Target::parse(&url).map_err(|e| Refusal::new(Reason::ManifestInvalid, e))?;
    let bearer = ctx
        .tunnel
        .as_ref()
        .map(|tc| tc.tunnel_key.clone())
        .filter(|k| !k.is_empty());
    let fetcher = Fetcher::new(ctx.tunnel.clone(), bearer);

    // The manifest and its signature.
    let manifest_bytes = fetcher
        .get_small(&url, MAX_MANIFEST_BYTES)
        .await
        .map_err(|e| Refusal::new(Reason::DownloadFailed, e))?;
    if manifest_bytes.status != 200 {
        return Err(Refusal::new(
            Reason::DownloadFailed,
            format!("{url}: HTTP {}", manifest_bytes.status),
        ));
    }
    let sig_url = format!("{url}.sig");
    let sig = fetcher
        .get_small(&sig_url, 1024)
        .await
        .map_err(|e| Refusal::new(Reason::DownloadFailed, e))?;
    if sig.status != 200 {
        return Err(Refusal::new(
            Reason::ManifestUnsigned,
            format!("{sig_url}: HTTP {}", sig.status),
        ));
    }
    let sig_line = String::from_utf8_lossy(&sig.body).into_owned();
    let manifest = Manifest::parse_signed(&manifest_bytes.body, &sig_line, &ctx.trust_keys)
        .map_err(|e| e.refusal())?;
    if manifest.parsed_version() != target_version {
        return Err(Refusal::new(
            Reason::ManifestInvalid,
            format!(
                "the manifest is for {}, not {}",
                manifest.version, req.version
            ),
        ));
    }
    if let (Some(min), Some(running)) = (
        manifest
            .min_from_version
            .as_deref()
            .and_then(Version::parse),
        Version::parse(&ctx.running_version),
    ) {
        if running < min {
            return Err(Refusal::new(
                Reason::BelowMinFromVersion,
                format!(
                    "{} is below the release's min_from_version {min}",
                    ctx.running_version
                ),
            ));
        }
    }
    let files = manifest.files_for(TARGET).ok_or_else(|| {
        Refusal::new(
            Reason::NoArtifactForTarget,
            format!("release {} has no artifact for {TARGET}", manifest.version),
        )
    })?;
    info!(
        version = %manifest.version,
        files = files.len(),
        "upgrade: manifest verified"
    );

    // The artifacts, into this version's stage.
    let stage = stage_dir(ctx, &manifest.version);
    sweep_other_stages(ctx, &stage);
    std::fs::create_dir_all(&stage)
        .map_err(|e| Refusal::new(Reason::Internal, format!("mkdir {}: {e}", stage.display())))?;
    let dir_url = manifest_target.with_path(&manifest_target.dir());
    let mut staged: Vec<(ArtifactFile, PathBuf)> = Vec::new();
    for file in files {
        let dest = if install.layout == Layout::Ramboot {
            install
                .files
                .get(&file.role)
                .cloned()
                .unwrap_or_else(|| stage.join(&file.name))
        } else {
            stage.join(&file.name)
        };
        let have = std::fs::metadata(&dest).map_or(0, |m| m.len());
        need_space(&stage, file.size.saturating_sub(have), install.min_free_kb)?;
        fetcher
            .get_to_file(
                &format!("{dir_url}{}", file.name),
                &dest,
                file.size,
                Some(&file.sha256),
            )
            .await
            .map_err(|e| Refusal::new(Reason::DownloadFailed, e))?;
        let actual = super::sha256_file(&dest)
            .map_err(|e| Refusal::new(Reason::Internal, format!("{}: {e}", dest.display())))?;
        if actual != file.sha256 {
            // A stale partial from an older attempt: drop it and say so; the
            // next request downloads it afresh.
            let _ = std::fs::remove_file(&dest);
            return Err(Refusal::new(
                Reason::Sha256Mismatch,
                format!(
                    "{}: got {actual}, the manifest says {}",
                    file.name, file.sha256
                ),
            ));
        }
        staged.push((file.clone(), dest));
    }

    // The binary must run here before anything moves.
    if install.layout.can_prove_binary() {
        if let Some((file, path)) = staged.iter().find(|(f, _)| f.role == "server") {
            prove_binary(path, file.gzip, &target_version).await?;
        }
    }

    // The plan: swaps, restores, restart, health.
    let mut swaps = Vec::new();
    let mut restores = Vec::new();
    let rollback = &install.rollback_dir;
    std::fs::create_dir_all(rollback).map_err(|e| {
        Refusal::new(
            Reason::Internal,
            format!("mkdir {}: {e}", rollback.display()),
        )
    })?;
    if install.layout == Layout::Ramboot {
        let conf_path = install
            .ramboot_conf
            .clone()
            .unwrap_or_else(|| PathBuf::from("/etc/sctl/ramboot.conf"));
        let current = std::fs::read_to_string(&conf_path)
            .map_err(|e| Refusal::new(Reason::Internal, format!("{}: {e}", conf_path.display())))?;
        let mirror = req.mirror_base.as_deref().unwrap_or_default();
        let new_conf = ramboot_conf(&current, files, mirror);
        let staged_conf = stage.join("ramboot.conf.new");
        std::fs::write(&staged_conf, new_conf.as_bytes()).map_err(|e| {
            Refusal::new(Reason::Internal, format!("{}: {e}", staged_conf.display()))
        })?;
        let backup = rollback.join("ramboot.conf");
        copy_file(&conf_path, &backup).map_err(|e| Refusal::new(Reason::Internal, e))?;
        let sha = super::sha256_file(&staged_conf)
            .map_err(|e| Refusal::new(Reason::Internal, e.to_string()))?;
        swaps.push(Swap {
            staged: staged_conf,
            target: conf_path.clone(),
            sha256: sha,
            size: new_conf.len() as u64,
            mode: 0o600,
        });
        restores.push(Restore {
            backup,
            target: conf_path,
            mode: 0o600,
        });
    } else {
        for (file, path) in &staged {
            let Some(target) = install.files.get(&file.role) else {
                info!(role = %file.role, "upgrade: no installed file for this role; skipped");
                continue;
            };
            let mode = mode_for(&file.role);
            if target.exists() {
                let backup = rollback.join(&file.role);
                copy_file(target, &backup).map_err(|e| Refusal::new(Reason::Internal, e))?;
                restores.push(Restore {
                    backup,
                    target: target.clone(),
                    mode,
                });
            }
            swaps.push(Swap {
                staged: path.clone(),
                target: target.clone(),
                sha256: file.sha256.clone(),
                size: file.size,
                mode,
            });
        }
    }
    if let Some(config) = ctx.config_path.as_deref() {
        let backup = rollback.join("sctl.toml");
        if copy_file(Path::new(config), &backup).is_ok() {
            restores.push(Restore {
                backup,
                target: PathBuf::from(config),
                mode: 0o600,
            });
        }
    }
    let plan = Plan {
        v: 1,
        request_id: req.request_id.clone(),
        from_version: ctx.running_version.clone(),
        to_version: manifest.version.clone(),
        layout: install.layout,
        swaps,
        restores,
        restart: install.restart.clone(),
        health_url: install.health_url(&ctx.listen),
        need_tunnel: ctx
            .tunnel
            .as_ref()
            .is_some_and(|tc| tc.url.is_some() && !tc.relay),
        min_free_kb: install.min_free_kb,
        state_path: handle.path().to_path_buf(),
        log_path: stage.join("log"),
    };
    let plan_path = stage.join("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).unwrap_or_default(),
    )
    .map_err(|e| Refusal::new(Reason::Internal, format!("{}: {e}", plan_path.display())))?;

    // The helper: a copy of this binary, detached.
    let helper = stage.join("helper");
    let me = std::env::current_exe()
        .map_err(|e| Refusal::new(Reason::Internal, format!("current_exe: {e}")))?;
    copy_file(&me, &helper).map_err(|e| Refusal::new(Reason::Internal, e))?;
    set_mode(&helper, 0o755).map_err(|e| Refusal::new(Reason::Internal, e))?;
    // The old helper's log starts afresh for this attempt.
    let _ = std::fs::remove_file(&plan.log_path);
    handle.set(
        UpgradeState::staging(
            req.request_id.clone(),
            &ctx.running_version,
            &manifest.version,
        )
        .with_phase(Phase::Applying),
    );
    spawn_helper(
        &helper,
        &stage,
        install.layout,
        &manifest.version,
        &plan.log_path,
    )
    .map_err(|e| Refusal::new(Reason::Internal, e))?;
    info!(version = %manifest.version, "upgrade: handed off to the helper");
    Ok(())
}

/// Start `helper upgrade-apply --stage <stage>` in its own session with
/// its output in `log`; on the systemd layout as a transient unit so it
/// lives outside the hardened service's cgroup.
fn spawn_helper(
    helper: &Path,
    stage: &Path,
    layout: Layout,
    version: &str,
    log: &Path,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let err_file = log_file
        .try_clone()
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let mut command = if layout == Layout::Systemd {
        let unit = format!(
            "sctl-upgrade-{}",
            version
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
        );
        let mut c = std::process::Command::new("systemd-run");
        c.arg("--collect")
            .arg(format!("--unit={unit}"))
            .arg("--quiet")
            .arg(format!(
                "--property=StandardOutput=append:{}",
                log.display()
            ))
            .arg(format!("--property=StandardError=append:{}", log.display()))
            .arg(helper)
            .arg("upgrade-apply")
            .arg("--stage")
            .arg(stage);
        c
    } else {
        let mut c = std::process::Command::new(helper);
        c.arg("upgrade-apply").arg("--stage").arg(stage);
        c
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(err_file);
    // SAFETY: setsid only changes the child's session; nothing here
    // allocates or takes a lock between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| format!("spawn helper: {e}"))?;
    if layout == Layout::Systemd {
        // systemd-run returns once the unit is started; its exit says
        // whether that worked.
        let status = child.wait().map_err(|e| format!("systemd-run: {e}"))?;
        if !status.success() {
            return Err(format!("systemd-run exited {:?}", status.code()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses_utc_offsets_and_fractions() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-02T06:00:00Z"), Some(1_790_920_800));
        assert_eq!(
            parse_rfc3339("2026-10-02T06:00:00.123Z"),
            Some(1_790_920_800)
        );
        assert_eq!(
            parse_rfc3339("2026-10-02T02:00:00-04:00"),
            Some(1_790_920_800)
        );
        assert_eq!(
            parse_rfc3339("2026-10-02T08:00:00+02:00"),
            Some(1_790_920_800)
        );
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2026-13-02T06:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-10-02T06:00"), None);
    }

    fn ctx(install: Option<InstallInfo>) -> Context {
        Context {
            state_dir: "/tmp/sctl-test-state".into(),
            listen: "127.0.0.1:1337".into(),
            install,
            tunnel: None,
            trust_keys: Vec::new(),
            config_path: None,
            hold: false,
            hold_file: PathBuf::from("/nonexistent/upgrade-hold"),
            running_version: "0.6.7.100".into(),
        }
    }

    fn req(version: &str) -> Request {
        Request {
            version: version.into(),
            ..Request::default()
        }
    }

    #[test]
    fn early_refusals_in_order() {
        let install = InstallInfo::defaults(Layout::UsrBin, "x86_64");
        let mut c = ctx(None);
        assert_eq!(
            refuse_early(&c, &req("0.6.8.1")).unwrap_err().reason,
            Reason::NoInstallJson
        );
        c.install = Some(install.clone());
        assert!(refuse_early(&c, &req("0.6.8.1")).is_ok());
        assert_eq!(
            refuse_early(&c, &req("0.6.6.1")).unwrap_err().reason,
            Reason::Downgrade
        );
        let mut down = req("0.6.6.1");
        down.allow_downgrade = true;
        assert!(refuse_early(&c, &down).is_ok());
        assert_eq!(
            refuse_early(&c, &req("nope")).unwrap_err().reason,
            Reason::ManifestInvalid
        );
        c.hold = true;
        assert_eq!(
            refuse_early(&c, &req("0.6.8.1")).unwrap_err().reason,
            Reason::Held
        );
        c.hold = false;
        c.install = Some(InstallInfo::defaults(Layout::Ramboot, "mips_24kc"));
        assert_eq!(
            refuse_early(&c, &req("0.6.8.1")).unwrap_err().reason,
            Reason::NoMirror
        );
    }

    #[test]
    fn a_window_is_judged_by_the_clock() {
        let c = ctx(Some(InstallInfo::defaults(Layout::UsrBin, "x86_64")));
        let mut r = req("0.6.8.1");
        r.not_after = Some("2020-01-01T00:00:00Z".into());
        assert_eq!(
            refuse_early(&c, &r).unwrap_err().reason,
            Reason::WindowPassed
        );
        let mut r = req("0.6.8.1");
        r.not_before = Some("2099-01-01T00:00:00Z".into());
        assert_eq!(
            refuse_early(&c, &r).unwrap_err().reason,
            Reason::WindowPassed
        );
        let mut r = req("0.6.8.1");
        r.not_before = Some("2020-01-01T00:00:00Z".into());
        r.not_after = Some("2099-01-01T00:00:00Z".into());
        assert!(refuse_early(&c, &r).is_ok());
        let mut r = req("0.6.8.1");
        r.not_after = Some("soon".into());
        assert_eq!(
            refuse_early(&c, &r).unwrap_err().reason,
            Reason::ManifestInvalid
        );
    }

    #[test]
    fn the_hold_file_refuses_with_its_reason() {
        let dir = std::env::temp_dir().join(format!("sctl-stage-hold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hold = dir.join("upgrade-hold");
        std::fs::write(&hold, "bench in use\n").unwrap();
        let mut c = ctx(Some(InstallInfo::defaults(Layout::UsrBin, "x86_64")));
        c.hold_file = hold;
        let r = refuse_early(&c, &req("0.6.8.1")).unwrap_err();
        assert_eq!(r.reason, Reason::Held);
        assert!(r.detail.contains("bench in use"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_manifest_url_derives_from_the_tunnel() {
        let mut c = ctx(Some(InstallInfo::defaults(Layout::UsrBin, "x86_64")));
        assert_eq!(
            manifest_url(&c, &req("0.6.8.1")).unwrap_err().reason,
            Reason::Internal
        );
        let mut tc = crate::config::TunnelConfig::plain();
        tc.url = Some("wss://relay.example:8443/api/tunnel/register".into());
        c.tunnel = Some(tc);
        assert_eq!(
            manifest_url(&c, &req("0.6.8.1")).unwrap(),
            "https://relay.example:8443/api/tunnel/artifacts/0.6.8.1/release.json"
        );
        let mut r = req("0.6.8.1");
        r.manifest_url = Some("http://mirror/r.json".into());
        assert_eq!(manifest_url(&c, &r).unwrap(), "http://mirror/r.json");
    }

    #[test]
    fn the_ramboot_conf_is_rewritten_per_role() {
        let current = "RUN_DIR='/tmp/sctl'\nSERVER_URL='http://old/s.gz'\nSERVER_SHA256='aa'\nSERVER_GZIP='1'\nPLUGIN_URL='http://old/p.gz'\nPLUGIN_SHA256='bb'\nMIN_TMP_KB='24576'\n";
        let files = vec![
            ArtifactFile {
                role: "server".into(),
                name: "sctl-server-mips_24kc.gz".into(),
                size: 1,
                sha256: "11".into(),
                gzip: true,
            },
            ArtifactFile {
                role: "plugin".into(),
                name: "sctl-comms-quectel-mips_24kc.so.gz".into(),
                size: 1,
                sha256: "22".into(),
                gzip: true,
            },
        ];
        let out = ramboot_conf(current, &files, "http://mirror/0.6.8.1/");
        assert_eq!(
            out,
            "RUN_DIR='/tmp/sctl'\nSERVER_URL='http://mirror/0.6.8.1/sctl-server-mips_24kc.gz'\nSERVER_SHA256='11'\nSERVER_GZIP='1'\nPLUGIN_URL='http://mirror/0.6.8.1/sctl-comms-quectel-mips_24kc.so.gz'\nPLUGIN_SHA256='22'\nMIN_TMP_KB='24576'\n"
        );
    }

    #[test]
    fn free_space_is_read_for_a_real_path() {
        assert!(free_bytes(Path::new("/tmp")).unwrap() > 0);
        assert!(need_space(Path::new("/tmp"), 1, 0).is_ok());
        assert_eq!(
            need_space(Path::new("/tmp"), u64::MAX / 2, 0)
                .unwrap_err()
                .reason,
            Reason::NoSpace
        );
    }
}
