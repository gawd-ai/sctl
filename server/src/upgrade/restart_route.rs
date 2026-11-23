//! `POST /api/restart` (ADR-014): the agent restarts itself as the managed
//! upgrade does, less everything an upgrade moves. A caller sends no shell
//! text and needs no knowledge of the layout.
//!
//! The route judges, in a fixed order, everything that would strand the
//! agent or cut something that must not be cut, and refuses before
//! anything stops. A refusal that costs nothing comes before one that
//! reads files, and the gate on recovery runs is closed last, so a refusal
//! leaves nothing to undo. Then it writes the record, answers, and hands
//! off in a task: the sessions are ended, the helper is started, and the
//! helper owns the restart from there.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::apply::HEALTH_WINDOW;
use super::install::{self, InstallInfo, Layout};
use super::restart::{self, Ends, Outcome, Process, RestartState};
use super::restart_apply::{self, Plan};
use super::restart_checks::{self, Reason, Site};
use crate::error::ApiError;
use crate::AppState;

/// The longest `request_id` the route takes, in bytes.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// The request's body. Both keys are optional, and an empty body is a
/// request with neither.
#[derive(Debug, Default, Deserialize)]
struct Request {
    /// The caller's name for this restart, echoed in the record.
    #[serde(default)]
    request_id: Option<String>,
    /// The kinds of work that refuse the restart while they run:
    /// `sessions`, `jobs`, `transfers`.
    #[serde(default)]
    unless: Vec<String>,
}

/// How the helper is started: the directory its plan is in, the install,
/// and when the request was accepted, in unix seconds.
pub type StartHelper = dyn Fn(&Path, &InstallInfo, u64) -> Result<(), String> + Send + Sync;

/// Free space on the filesystem that holds a path, in KB.
pub type FreeKb = dyn Fn(&Path) -> Option<u64> + Send + Sync;

/// What the route reaches outside the app's state. Given, so a test stages
/// a unit and stands in for the helper.
#[derive(Clone)]
pub struct Deps {
    /// The kill switch.
    pub disabled: PathBuf,
    /// The ramboot init's wrapper.
    pub wrapper: PathBuf,
    /// Free space on the filesystem that holds a path, in KB.
    pub free_kb: Arc<FreeKb>,
    /// Start the detached helper.
    pub start_helper: Arc<StartHelper>,
}

impl Deps {
    /// The unit as it is: its own paths, and the helper started as the
    /// upgrade's is.
    pub fn production() -> Self {
        Self {
            disabled: install::disabled_path(),
            wrapper: PathBuf::from(install::RAMBOOT_WRAPPER_PATH),
            free_kb: Arc::new(|path| super::stage::free_bytes(path).map(|bytes| bytes / 1024)),
            start_helper: Arc::new(start_helper),
        }
    }
}

/// Start `restart-apply --dir <dir>` from the running binary, detached:
/// its own session, its output in the directory's log, the unit's loader
/// words, a transient unit on systemd (`stage::spawn_helper`). Not a copy
/// of the binary: nothing is replaced, so nothing it runs from is.
fn start_helper(dir: &Path, install: &InstallInfo, accepted_unix_secs: u64) -> Result<(), String> {
    let running = install.running()?;
    let log = dir.join(restart_apply::LOG_FILE);
    // The last request's log starts afresh.
    let _ = std::fs::remove_file(&log);
    super::stage::spawn_helper(
        &running.binary,
        &restart_apply::helper_args(dir),
        &restart_apply::helper_unit(accepted_unix_secs),
        install.layout,
        &log,
        &running.loader,
    )
}

type Refused = (StatusCode, Json<ApiError>);

/// An accepted request: the answer, and the task that hands off.
pub struct Accepted {
    pub status: StatusCode,
    pub body: Value,
    pub hand_off: tokio::task::JoinHandle<()>,
}

fn bad_request(why: impl Into<String>) -> Refused {
    ApiError::new("RESTART_BAD_REQUEST", why).into_response_with(StatusCode::BAD_REQUEST)
}

fn refused(reason: Reason, detail: &str) -> Refused {
    refused_with(reason, detail, json!({}))
}

fn refused_with(reason: Reason, detail: &str, mut extra: Value) -> Refused {
    extra["reason"] = json!(reason);
    extra["detail"] = json!(detail);
    ApiError::new("RESTART_REFUSED", detail)
        .with_detail(extra)
        .into_response_with(StatusCode::UNPROCESSABLE_ENTITY)
}

/// What is in flight, as a 409: a restart when its record says so, else
/// the upgrade.
pub fn in_flight(state: &AppState) -> Refused {
    let record = state.restart.current();
    if record.in_flight() {
        ApiError::new("RESTART_IN_FLIGHT", "a restart is in progress")
            .with_detail(json!({"restart": record}))
            .into_response_with(StatusCode::CONFLICT)
    } else {
        ApiError::new("UPGRADE_IN_FLIGHT", "an upgrade is in progress")
            .with_detail(json!({"state": state.upgrade.current()}))
            .into_response_with(StatusCode::CONFLICT)
    }
}

/// Whether a restart is in flight, once a helper that ended without the
/// agent seeing it is settled. `POST /api/upgrade` asks too: an upgrade
/// and a restart exclude each other.
pub fn restart_in_flight(state: &AppState) -> bool {
    restart::settle_lost_helper(
        &state.restart,
        &Process {
            instance: &state.instance,
            boot: state.boot_unix_ms,
        },
    );
    state.restart.current().in_flight()
}

/// `POST /api/restart`.
pub async fn post_restart(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), Refused> {
    let accepted = accept(&state, &body, Deps::production()).await?;
    Ok((accepted.status, Json(accepted.body)))
}

/// Judge a request and, when nothing refuses it, write the record, close
/// the gate on recovery runs and start the hand-off.
pub async fn accept(state: &AppState, body: &[u8], deps: Deps) -> Result<Accepted, Refused> {
    // 1. The body.
    let req: Request = if body.iter().all(u8::is_ascii_whitespace) {
        Request::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| bad_request(format!("the body is not a restart request: {e}")))?
    };
    if req
        .request_id
        .as_ref()
        .is_some_and(|id| id.len() > MAX_REQUEST_ID_BYTES)
    {
        return Err(bad_request(format!(
            "request_id is over {MAX_REQUEST_ID_BYTES} bytes"
        )));
    }
    if let Some(word) = req
        .unless
        .iter()
        .find(|w| !matches!(w.as_str(), "sessions" | "jobs" | "transfers"))
    {
        return Err(bad_request(format!(
            "unless: '{word}' is not one of sessions, jobs, transfers"
        )));
    }

    // 2. One in flight at a time, across upgrades and restarts.
    let Ok(guard) = state.upgrade.in_flight.try_lock() else {
        return Err(in_flight(state));
    };
    if restart_in_flight(state) || state.upgrade.current().in_flight() {
        return Err(in_flight(state));
    }

    // 3. A relay's restart drops every unit's tunnel at once.
    if state.config.tunnel.as_ref().is_some_and(|tc| tc.relay) {
        return Err(refused(
            Reason::Relay,
            "this agent is a relay: it is restarted at its host, not by this route",
        ));
    }

    // 4. Whether the init would start the agent again.
    let install = state.install.clone();
    let running = install
        .as_deref()
        .map_or_else(|| Err("no install.json".to_string()), InstallInfo::running);
    let free_kb = deps.free_kb.clone();
    restart_checks::judge(
        install.as_deref(),
        &Site {
            config: state.config_path.as_deref().map(Path::new),
            disabled: &deps.disabled,
            wrapper: &deps.wrapper,
            running,
            free_kb: &move |path| free_kb(path),
        },
    )
    .map_err(|refusal| refused(refusal.reason, &refusal.detail))?;
    let Some(install) = install else {
        // `judge` refuses a unit with no install record.
        return Err(refused(Reason::NoInstallJson, "no install.json"));
    };

    // 5. What the restart ends, and what the caller asked it not to end.
    let ends = Ends::read(&state.session_manager, &state.transfer_manager).await;
    for word in &req.unless {
        let busy = match word.as_str() {
            "sessions" if ends.attached > 0 => {
                Some((Reason::SessionsOpen, "a terminal has a client attached"))
            }
            "jobs" if !ends.jobs.is_empty() => Some((Reason::JobsRunning, "a job is running")),
            "transfers" if ends.transfers > 0 => {
                Some((Reason::TransfersRunning, "a file transfer is in flight"))
            }
            _ => None,
        };
        if let Some((reason, detail)) = busy {
            return Err(refused_with(reason, detail, json!({"ends": ends})));
        }
    }

    // 6. The gate: closed in the step that finds no recovery run, so none
    // starts between this answer and the stop.
    let runs = state.infra_digest.runs();
    if !runs.close_if_idle() {
        return Err(refused(
            Reason::RecoveryRunning,
            "an Infra recovery run is in progress: a restart would cut it",
        ));
    }

    // 7. The record, then the answer.
    let me = Process {
        instance: &state.instance,
        boot: state.boot_unix_ms,
    };
    let accepted = RestartState::accepted(req.request_id, &me);
    state.restart.set(accepted.clone());
    let accepted_unix_secs = crate::infra::now_unix_ms() / 1000;
    let second_run = install.layout == Layout::GzTmp;
    info!(
        request_id = accepted.request_id.as_deref().unwrap_or(""),
        layout = install.layout.as_str(),
        "restart: accepted"
    );
    let body = json!({
        "accepted": true,
        "restart": accepted,
        "window_secs": HEALTH_WINDOW.as_secs(),
        // On gz-tmp the helper may run the command a second time and wait
        // a second window (ADR-014 decision 5).
        "second_run": second_run,
        "safe_mode": Path::new(&state.config.server.data_dir).join("safe_mode.flag").exists(),
        "ends": ends,
    });
    drop(guard);

    // 8. The hand-off, after the answer.
    let hand_off = tokio::spawn(hand_off(state.clone(), deps, install, accepted_unix_secs));
    Ok(Accepted {
        status: StatusCode::ACCEPTED,
        body,
        hand_off,
    })
}

/// After the answer: end every session and job, write the helper's plan,
/// move the record to `restarting` and start the helper. Then watch the
/// record: a process that is still here when the helper ends it was not
/// restarted, and opens the gate on recovery runs again.
async fn hand_off(state: AppState, deps: Deps, install: Arc<InstallInfo>, accepted_unix_secs: u64) {
    // Held for the hand-off and the helper's run: the route's guard dropped
    // when it answered, and the record already refuses the next request.
    let _guard = state.upgrade.in_flight.lock().await;
    let runs = state.infra_digest.runs();
    // The agent ends them itself, so none is left to a supervisor's kill
    // timeout. The answer said so.
    state.session_manager.kill_all().await;

    let dir = state.restart.helper_dir().to_path_buf();
    let plan = Plan {
        v: 1,
        restart: install.restart.clone(),
        health_url: install.health_url(&state.config.server.listen),
        layout: install.layout,
        record_path: state.restart.path().to_path_buf(),
        instance_before: state.instance.to_string(),
    };
    let started = restart_apply::save_plan(&dir, &plan).and_then(|()| {
        state.restart.set(state.restart.current().restarting());
        (deps.start_helper)(&dir, &install, accepted_unix_secs)
    });
    if let Err(e) = started {
        warn!("restart: the helper could not be started: {e}");
        state.restart.set(state.restart.current().finished(
            Outcome::NotStarted,
            Some(format!("the helper could not be started: {e}")),
        ));
        runs.reopen();
        return;
    }
    info!("restart: handed off to the helper");

    let ended = restart::watch_until_done(
        state.restart.clone(),
        state.instance.to_string(),
        state.boot_unix_ms,
    )
    .await;
    if ended.is_none() {
        // The record could not be watched. A helper that ends it while this
        // process still runs is read once both of its windows have passed.
        tokio::time::sleep(2 * HEALTH_WINDOW + Duration::from_mins(1)).await;
        if !restart_in_flight(&state) {
            restart::note_outcome(&state.restart.current());
        }
    }
    // Still here: this process was not restarted. Recoveries may run again.
    runs.reopen();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::file_access::Gate;
    use crate::upgrade::restart::Phase;
    use crate::upgrade::state::UpgradeState;
    use std::sync::Mutex;

    /// A unit staged for the route: its state, what the route is handed,
    /// and what the stand-in helper recorded.
    struct Bench {
        dir: PathBuf,
        state: AppState,
        deps: Deps,
        /// The record's phase each time the helper was started.
        started: Arc<Mutex<Vec<Phase>>>,
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn bench(tag: &str) -> Bench {
        let dir =
            std::env::temp_dir().join(format!("sctl-restart-route-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        let mut state = AppState::for_tests(config, gate);
        state.install = Some(Arc::new(InstallInfo::defaults(
            Layout::UsrBin,
            crate::upgrade::TARGET,
        )));
        let started = Arc::new(Mutex::new(Vec::new()));
        let (recorded, record) = (started.clone(), state.restart.clone());
        let deps = Deps {
            disabled: dir.join("disabled"),
            wrapper: dir.join("ramboot.sh"),
            free_kb: Arc::new(|_| Some(1_000_000)),
            start_helper: Arc::new(move |_, _, _| {
                recorded.lock().unwrap().push(record.current().phase);
                Ok(())
            }),
        };
        Bench {
            dir,
            state,
            deps,
            started,
        }
    }

    async fn ask(bench: &Bench, body: &str) -> Result<Accepted, (StatusCode, Value)> {
        accept(&bench.state, body.as_bytes(), bench.deps.clone())
            .await
            .map_err(|(status, Json(error))| (status, serde_json::to_value(error).unwrap()))
    }

    /// A refusal: its status, its code and its reason.
    async fn refusal(bench: &Bench, body: &str) -> (u16, String, Value) {
        let Err((status, error)) = ask(bench, body).await else {
            panic!("a refusal");
        };
        (
            status.as_u16(),
            error["code"].as_str().unwrap().to_string(),
            error["detail"]["reason"].clone(),
        )
    }

    /// Nothing was stopped or left behind by a refusal.
    fn untouched(bench: &Bench) {
        assert!(
            !bench.state.infra_digest.runs().is_closed(),
            "the gate on recovery runs is open"
        );
        assert!(bench.started.lock().unwrap().is_empty(), "no helper");
    }

    /// End the record as the helper would, and let the hand-off finish.
    async fn helper_ends(bench: &Bench, accepted: Accepted, outcome: Outcome) {
        let record = &bench.state.restart;
        tokio::time::timeout(Duration::from_secs(10), async {
            while record.current().phase != Phase::Restarting {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the hand-off reaches the helper");
        restart::save(
            record.path(),
            &record
                .current()
                .finished(outcome, Some("as the helper wrote it".into())),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), accepted.hand_off)
            .await
            .expect("the hand-off ends when the helper ends the record")
            .unwrap();
    }

    #[tokio::test]
    async fn a_request_is_accepted_recorded_and_handed_off_after_the_answer() {
        let bench = bench("accept");
        let accepted = ask(&bench, r#"{"request_id": "r-41"}"#).await.unwrap();
        assert_eq!(accepted.status, StatusCode::ACCEPTED);
        let body = &accepted.body;
        assert_eq!(body["accepted"], true);
        assert_eq!(body["window_secs"], 180);
        assert_eq!(body["second_run"], false, "usr-bin: one run");
        assert_eq!(body["safe_mode"], false);
        assert_eq!(
            body["ends"],
            json!({"sessions": 0, "attached": 0, "jobs": [], "transfers": 0})
        );
        assert_eq!(body["restart"]["phase"], "accepted");
        assert_eq!(body["restart"]["request_id"], "r-41");
        assert_eq!(body["restart"]["instance_before"], &*bench.state.instance);
        assert_eq!(body["restart"]["boot_before"], bench.state.boot_unix_ms);
        // The gate closed with the accept: no recovery run starts from here.
        let runs = bench.state.infra_digest.runs();
        assert!(runs.is_closed());
        assert!(runs.start("pdu").is_none());
        // The record is on disk before the hand-off moves it on.
        assert!(restart::load(bench.state.restart.path()).is_some());

        helper_ends(&bench, accepted, Outcome::NotRestarted).await;
        // The helper was started once, after the record said so.
        assert_eq!(*bench.started.lock().unwrap(), [Phase::Restarting]);
        // Its plan is what the install says, and names this process.
        let plan = restart_apply::load_plan(bench.state.restart.helper_dir()).unwrap();
        assert_eq!(plan.restart, "/etc/init.d/sctl restart");
        assert_eq!(plan.layout, Layout::UsrBin);
        assert_eq!(plan.instance_before, &*bench.state.instance);
        assert_eq!(plan.record_path, bench.state.restart.path());
        assert!(plan.health_url.starts_with("http://127.0.0.1:"));
        // This process is still here: the record says so, and recoveries
        // may run again.
        let record = bench.state.restart.current();
        assert_eq!(record.outcome, Some(Outcome::NotRestarted));
        assert_eq!(record.boot_after, None);
        assert!(!runs.is_closed());
        // And the next request is taken.
        let again = ask(&bench, "").await.unwrap();
        assert_eq!(again.body["restart"].get("request_id"), None);
        helper_ends(&bench, again, Outcome::NotHealthy).await;
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_request_is_refused_as_such() {
        let bench = bench("body");
        let long = format!(r#"{{"request_id": "{}"}}"#, "x".repeat(129));
        for body in [
            "not json",
            r#"{"request_id": 7}"#,
            long.as_str(),
            r#"{"unless": ["sessions", "everything"]}"#,
        ] {
            let (status, code, _) = refusal(&bench, body).await;
            assert_eq!(
                (status, code.as_str()),
                (400, "RESTART_BAD_REQUEST"),
                "{body}"
            );
        }
        untouched(&bench);
        assert_eq!(bench.state.restart.current(), RestartState::idle());
        // 128 bytes is taken, and an unknown key is ignored.
        let id = "x".repeat(128);
        let accepted = ask(&bench, &format!(r#"{{"request_id": "{id}", "later": 1}}"#))
            .await
            .unwrap();
        helper_ends(&bench, accepted, Outcome::NotRestarted).await;
    }

    #[tokio::test]
    async fn each_refusal_names_its_reason_and_stops_nothing() {
        // No install record.
        let mut b = bench("none");
        b.state.install = None;
        assert_eq!(
            refusal(&b, "").await,
            (422, "RESTART_REFUSED".into(), json!("no_install_json"))
        );
        untouched(&b);

        // The kill switch.
        let b = bench("disabled");
        std::fs::write(&b.deps.disabled, "").unwrap();
        assert_eq!(
            refusal(&b, "").await,
            (422, "RESTART_REFUSED".into(), json!("disabled"))
        );
        untouched(&b);

        // A relay, before anything on the unit is read.
        let mut b = bench("relay");
        let mut config: Config =
            toml::from_str("[tunnel]\nrelay = true\ntunnel_key = \"k\"\n").unwrap();
        config.server.data_dir = b.dir.join("data").display().to_string();
        b.state.config = Arc::new(config);
        std::fs::write(&b.deps.disabled, "").unwrap();
        assert_eq!(
            refusal(&b, "").await,
            (422, "RESTART_REFUSED".into(), json!("relay"))
        );
        untouched(&b);

        // A gz-tmp unit with too little room.
        let mut b = bench("space");
        let mut install = InstallInfo::defaults(Layout::GzTmp, crate::upgrade::TARGET);
        for role in ["server", "plugin"] {
            let path = b.dir.join(role);
            std::fs::write(&path, b"payload").unwrap();
            install.files.insert(role.to_string(), path);
        }
        b.state.install = Some(Arc::new(install));
        b.deps.free_kb = Arc::new(|_| Some(16383));
        assert_eq!(
            refusal(&b, "").await,
            (422, "RESTART_REFUSED".into(), json!("no_space"))
        );
        untouched(&b);
        // With the room its init asks for, it is accepted, and the answer
        // says a second run is possible there.
        b.deps.free_kb = Arc::new(|_| Some(1_000_000));
        let accepted = ask(&b, "").await.unwrap();
        assert_eq!(accepted.body["second_run"], true);
        helper_ends(&b, accepted, Outcome::NotRestarted).await;

        // A recovery run in progress: refused, the run not cut, the gate open.
        let b = bench("recovery");
        let runs = b.state.infra_digest.runs();
        let run = runs.start("pdu").expect("a run");
        assert_eq!(
            refusal(&b, "").await,
            (422, "RESTART_REFUSED".into(), json!("recovery_running"))
        );
        untouched(&b);
        assert!(runs.contains("pdu"));
        drop(run);
        assert_eq!(b.state.restart.current(), RestartState::idle());
    }

    #[tokio::test]
    async fn an_upgrade_and_a_restart_exclude_each_other() {
        // An upgrade staging refuses a restart.
        let b = bench("upgrading");
        b.state.upgrade.set(UpgradeState::staging(
            Some("u-1".into()),
            "0.6.13.309",
            "0.6.14.1",
        ));
        let (status, error) = ask(&b, "").await.err().unwrap();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "UPGRADE_IN_FLIGHT");
        assert_eq!(error["detail"]["state"]["request_id"], "u-1");
        untouched(&b);

        // A restart accepted refuses a second restart, with the first's
        // record, and an upgrade.
        let b = bench("restarting");
        let first = ask(&b, r#"{"request_id": "r-1"}"#).await.unwrap();
        let (status, error) = ask(&b, r#"{"request_id": "r-2"}"#).await.err().unwrap();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "RESTART_IN_FLIGHT");
        assert_eq!(error["detail"]["restart"]["request_id"], "r-1");
        let upgrade = super::super::routes::post_upgrade(
            State(b.state.clone()),
            Json(crate::upgrade::stage::Request {
                version: "9.9.9.1".into(),
                ..Default::default()
            }),
        )
        .await;
        let (status, Json(error)) = upgrade.expect_err("refused");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            serde_json::to_value(error).unwrap()["code"],
            "RESTART_IN_FLIGHT"
        );
        helper_ends(&b, first, Outcome::NotRestarted).await;
        assert_eq!(b.state.restart.current().request_id.as_deref(), Some("r-1"));
    }

    /// A helper that is gone without an outcome does not hold the unit: the
    /// next request settles its record and is judged.
    #[tokio::test]
    async fn a_lost_helper_is_settled_before_the_next_request_is_judged() {
        let b = bench("lost");
        let me = Process {
            instance: &b.state.instance,
            boot: b.state.boot_unix_ms,
        };
        b.state
            .restart
            .set(RestartState::accepted(Some("old".into()), &me).restarting());
        let accepted = ask(&b, r#"{"request_id": "new"}"#).await.unwrap();
        assert_eq!(accepted.body["restart"]["request_id"], "new");
        helper_ends(&b, accepted, Outcome::NotRestarted).await;
    }

    #[tokio::test]
    async fn a_helper_that_cannot_be_started_ends_as_not_started_with_the_gate_open() {
        let mut b = bench("nohelper");
        b.deps.start_helper = Arc::new(|_, _, _| Err("spawn helper: no such file".into()));
        let accepted = ask(&b, r#"{"request_id": "r-9"}"#).await.unwrap();
        assert!(b.state.infra_digest.runs().is_closed());
        accepted.hand_off.await.unwrap();
        let record = b.state.restart.current();
        assert_eq!(record.outcome, Some(Outcome::NotStarted));
        assert!(record.detail.unwrap().contains("no such file"));
        assert!(
            !b.state.infra_digest.runs().is_closed(),
            "recoveries may run"
        );
        assert_eq!(
            restart::load(b.state.restart.path()).unwrap().outcome,
            Some(Outcome::NotStarted)
        );
    }

    /// The agent ends every session and job itself, before the helper is
    /// started; `unless` lets a caller ask it to refuse instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sessions_are_listed_refused_on_request_and_ended_before_the_helper() {
        let mut b = bench("sessions");
        let sessions = b.state.session_manager.clone();
        let (events, _) = tokio::sync::broadcast::channel(8);
        // The test's own children: a terminal with its client attached, and
        // a job.
        sessions
            .create_session("/bin/sh", "/", None, true)
            .await
            .unwrap();
        sessions
            .create_job(
                "/bin/sh",
                "/",
                "sleep 30",
                None,
                Some("pb:outlet-cycle"),
                60,
                events,
            )
            .await
            .unwrap();

        for (unless, reason) in [("sessions", "sessions_open"), ("jobs", "jobs_running")] {
            let (status, error) = ask(&b, &format!(r#"{{"unless": ["transfers", "{unless}"]}}"#))
                .await
                .err()
                .unwrap();
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(error["code"], "RESTART_REFUSED");
            assert_eq!(error["detail"]["reason"], reason);
            assert_eq!(error["detail"]["ends"]["attached"], 1);
            assert_eq!(error["detail"]["ends"]["jobs"], json!(["pb:outlet-cycle"]));
        }
        untouched(&b);
        assert_eq!(sessions.session_count().await, 2, "both alive");

        // `transfers` alone refuses nothing here: none is in flight.
        // The helper's stand-in records how many sessions were left.
        let left = Arc::new(Mutex::new(None));
        let (seen, manager) = (left.clone(), sessions.clone());
        let handle = tokio::runtime::Handle::current();
        b.deps.start_helper = Arc::new(move |_, _, _| {
            let manager = manager.clone();
            let running = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        handle.block_on(async {
                            let list = manager.list_sessions().await;
                            list.iter().filter(|s| s.status == "running").count()
                        })
                    })
                    .join()
                    .unwrap()
            });
            *seen.lock().unwrap() = Some(running);
            Ok(())
        });
        let accepted = ask(&b, r#"{"unless": ["transfers"]}"#).await.unwrap();
        assert_eq!(
            accepted.body["ends"],
            json!({"sessions": 1, "attached": 1, "jobs": ["pb:outlet-cycle"], "transfers": 0})
        );
        helper_ends(&b, accepted, Outcome::NotRestarted).await;
        assert_eq!(
            *left.lock().unwrap(),
            Some(0),
            "no session or job was running when the helper was started"
        );
    }
}
