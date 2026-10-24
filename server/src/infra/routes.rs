//! HTTP route handlers for the infra monitoring API.
//!
//! - `POST /api/infra/config`: push monitoring config; an identical push changes nothing
//! - `GET  /api/infra/results`: latest monitoring results
//! - `POST /api/infra/check/{target_id}`: Check now, a reading of one target, recorded
//! - `DELETE /api/infra/config`: stop monitoring, remove config
//! - `POST /api/infra/discover`: trigger LAN discovery scan

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use tracing::info;

use super::monitor::{self, CheckNowError};
use std::collections::HashMap;

use super::{CheckSpec, Credential, InfraConfig, InfraResults, Stamp};
use crate::AppState;

/// `POST /api/infra/config`: receive and apply a monitoring config.
///
/// A config equal to the one held, every field and the version included,
/// changes nothing: the monitor keeps running, nothing is seeded or
/// signalled, and the config is written again only if its last save failed
/// (`unchanged: true`). Another config is held and saved, and its monitor
/// replaces the running one; a recovery run in progress goes on. Outside
/// safe mode a push starts a monitor when none runs for the config held
/// (`started`). Version 0 means the unit holds no config, so it is refused.
pub async fn push_config(
    State(state): State<AppState>,
    Json(config): Json<InfraConfig>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": "Infra monitoring not available", "code": "INFRA_UNAVAILABLE", "message": "Infra monitoring not available"}),
            ),
        ));
    };
    if config.version == 0 {
        let message =
            "version 0 means the unit holds no config: name a config by a version above 0";
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": message, "code": "INVALID_REQUEST", "message": message})),
        ));
    }

    let version = config.version;
    let target_count = config.targets.len();

    let mut guard = infra.lock().await;
    let unchanged = guard.config.as_ref() == Some(&config);
    if unchanged {
        if !guard.config_saved {
            guard.save_config();
        }
    } else {
        // The running monitor ends with its checks; a recovery run in
        // progress is not one of them.
        if let Some(handle) = guard.monitor_handle.take() {
            handle.abort();
        }
        // Results reflect the new target set at once: targets that left are
        // dropped, new ones appear as `unknown`, and the version is the
        // pushed one, so a collector reading results between the push and
        // the first check sees the config it pushed, not the old one.
        guard.hold_config(config);
        guard.save_config();
    }
    let started = monitor::start(infra, &mut guard);

    if unchanged {
        info!("Infra config v{version} pushed again, unchanged (monitor started: {started})");
    } else {
        info!(
            "Infra config v{version} applied: {target_count} targets (monitor started: {started})"
        );
    }

    Ok(Json(json!({
        "status": "ok",
        "config_version": version,
        "target_count": target_count,
        "unchanged": unchanged,
        "started": started
    })))
}

/// What `GET /api/infra/results` answers: the results, named as a report
/// names them (`boot`, `seq`, `hash`, `recoveries`), so a pull settles a
/// comparison with the `infra` group of `GET /api/info` (ADR-010).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResultsReport {
    #[serde(flatten)]
    pub stamp: Stamp,
    #[serde(flatten)]
    pub results: InfraResults,
}

/// `GET /api/infra/results` — return latest monitoring results.
pub async fn get_results(
    State(state): State<AppState>,
) -> Result<Json<ResultsReport>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Ok(Json(ResultsReport {
            stamp: state.infra_digest.stamp(),
            results: InfraResults {
                ts: super::now_iso(),
                config_version: 0,
                targets: HashMap::default(),
                recovery_log: Vec::new(),
            },
        }));
    };

    // The stamp is read under the lock every writer of the results holds,
    // so it names exactly these results.
    let guard = infra.lock().await;
    Ok(Json(ResultsReport {
        stamp: guard.stamp(),
        results: guard.results.clone(),
    }))
}

/// `POST /api/infra/check/{target_id}`: Check now, a reading of one target
/// (ADR-010 decision 10). It runs under the monitor's 90 s deadline and its
/// result is applied as a monitor check's is, a status change pushed; it
/// counts no round and never starts a recovery. While a check of the target
/// is in flight it waits for that check and answers its result.
pub async fn check_target(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": "Infra monitoring not available", "code": "INFRA_UNAVAILABLE", "message": "Infra monitoring not available"}),
            ),
        ));
    };

    let result = match monitor::check_now(infra, &target_id).await {
        Ok(result) => result,
        Err(CheckNowError::NoConfig) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "No config loaded", "code": "NOT_FOUND", "message": "No config loaded"}),
                ),
            ));
        }
        Err(CheckNowError::UnknownTarget) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(
                    json!({"error": format!("Target {target_id} not found"), "code": "NOT_FOUND", "message": format!("Target {target_id} not found")}),
                ),
            ));
        }
    };

    Ok(Json(json!({
        "target_id": target_id,
        "ok": result.ok,
        "latency_ms": result.latency_ms,
        "detail": result.detail,
        "http_status": result.http_status,
        "data": result.data,
        // Present when a TLS target's certificate was not the pinned one (or
        // none was pinned): the fingerprint an operator may now choose to pin.
        "presented_sha256": result.presented_sha256,
    })))
}

/// `GET /api/infra/history/{target_id}` — recent structured readings of one
/// target (newest last), so a collector can backfill a window it missed.
pub async fn target_history(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Ok(Json(json!({"target_id": target_id, "samples": []})));
    };
    let guard = infra.lock().await;
    let samples: Vec<Value> = guard
        .data_history
        .get(&target_id)
        .map(|ring| {
            ring.iter()
                .filter_map(|s| serde_json::to_value(s).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(json!({"target_id": target_id, "samples": samples})))
}

/// Body of `POST /api/infra/credentials`.
#[derive(Debug, serde::Deserialize)]
pub struct CredentialUpsert {
    pub id: String,
    pub username: String,
    pub password: String,
}

/// `POST /api/infra/credentials` — store or replace one credential.
///
/// Credentials live in their own owner-only file, never in the monitoring
/// config, and no route ever returns a password.
pub async fn upsert_credential(
    State(state): State<AppState>,
    Json(body): Json<CredentialUpsert>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err(unavailable());
    };
    if body.id.trim().is_empty() || body.username.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"code": "INVALID_REQUEST", "message": "id and username are required"})),
        ));
    }
    let mut guard = infra.lock().await;
    guard.credentials.insert(
        body.id.clone(),
        Credential {
            username: body.username,
            password: body.password,
        },
    );
    // Any cached session was opened with the old credential.
    let stale_targets: Vec<String> = guard
        .config
        .as_ref()
        .map(|c| {
            c.targets
                .iter()
                .filter(|t| matches!(&t.check, CheckSpec::HttpApi { credential_id: Some(id), .. } if *id == body.id))
                .map(|t| t.id.clone())
                .collect()
        })
        .unwrap_or_default();
    for id in &stale_targets {
        guard.sessions.remove(id);
    }
    let saved = guard.save_credentials();
    info!(
        "Infra credential {} stored ({} targets use it)",
        body.id,
        stale_targets.len()
    );
    Ok(Json(
        json!({"status": "ok", "id": body.id, "persisted": saved}),
    ))
}

/// `DELETE /api/infra/credentials/{id}` — forget one credential.
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err(unavailable());
    };
    let mut guard = infra.lock().await;
    let existed = guard.credentials.remove(&id).is_some();
    let saved = guard.save_credentials();
    Ok(Json(
        json!({"status": "ok", "id": id, "existed": existed, "persisted": saved}),
    ))
}

/// `GET /api/infra/credentials` — ids and usernames only, for reconciliation.
pub async fn list_credentials(State(state): State<AppState>) -> Json<Value> {
    let Some(ref infra) = state.infra_state else {
        return Json(json!({"credentials": []}));
    };
    let guard = infra.lock().await;
    let mut list: Vec<Value> = guard
        .credentials
        .iter()
        .map(|(id, c)| json!({"id": id, "username": c.username}))
        .collect();
    list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(json!({"credentials": list}))
}

fn unavailable() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(
            json!({"error": "Infra monitoring not available", "code": "INFRA_UNAVAILABLE", "message": "Infra monitoring not available"}),
        ),
    )
}

/// `GET /api/infra/discover/progress` — return current discovery scan progress.
pub async fn discover_progress(State(state): State<AppState>) -> Json<Value> {
    let Some(ref infra) = state.infra_state else {
        return Json(json!({"active": false, "phase": "idle"}));
    };
    let guard = infra.lock().await;
    Json(
        serde_json::to_value(&guard.discovery_progress)
            .unwrap_or(json!({"active": false, "phase": "idle"})),
    )
}

/// `GET /api/infra/discover/subnets` — return auto-detected LAN subnets.
pub async fn discover_subnets() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match super::discovery::auto_detect_subnets().await {
        Ok(subnets) => Ok(Json(json!({ "subnets": subnets }))),
        Err(e) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": e, "reason": "ip_command_failed", "code": "EXEC_FAILED", "message": e }),
            ),
        )),
    }
}

/// `DELETE /api/infra/config` — stop monitoring and remove config.
pub async fn delete_config(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": "Infra monitoring not available", "code": "INFRA_UNAVAILABLE", "message": "Infra monitoring not available"}),
            ),
        ));
    };

    let mut guard = infra.lock().await;

    // The monitor ends with its checks; a recovery run in progress goes on
    // and is logged when it ends.
    if let Some(handle) = guard.monitor_handle.take() {
        handle.abort();
    }

    // Clear config, results and every recovery count; an empty result set
    // is a change to push, unless the unit held nothing.
    guard.drop_config();

    // Remove persisted config
    let _ = std::fs::remove_file(&guard.config_path);

    info!("Infra monitoring stopped and config removed");

    Ok(Json(
        json!({"status": "ok", "message": "Monitoring stopped"}),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;
    use tokio::time::Instant;

    use super::*;
    use crate::config::Config;
    use crate::file_access::Gate;
    use crate::infra::checks::CheckResult;
    use crate::infra::{
        picture_hash, picture_text, Digest, InfraState, InfraTarget, RecoveryLogEntry, TargetStatus,
    };

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sctl-infra-routes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A state with an Infra state in `dir`, both sharing one digest.
    fn state(dir: &std::path::Path) -> (AppState, Arc<Mutex<InfraState>>) {
        state_with(dir, Digest::new(1_790_000_000_000, false))
    }

    /// [`state`] with `digest`: an agent that started in safe mode, say.
    fn state_with(dir: &std::path::Path, digest: Digest) -> (AppState, Arc<Mutex<InfraState>>) {
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        let mut state = AppState::for_tests(config, gate);
        let digest = Arc::new(digest);
        let infra = Arc::new(Mutex::new(InfraState::with_digest(
            &dir.join("state").display().to_string(),
            digest.clone(),
        )));
        state.infra_state = Some(infra.clone());
        state.infra_digest = digest;
        (state, infra)
    }

    fn target(id: &str) -> InfraTarget {
        InfraTarget {
            id: id.into(),
            name: format!("Target {id}"),
            check: CheckSpec::Ping {
                host: "192.0.2.1".into(),
                timeout_ms: None,
            },
            degraded_threshold_ms: 200,
            down_after_consecutive: 3,
            up_after_consecutive: 2,
            interval_secs: None,
            recovery: None,
        }
    }

    #[tokio::test]
    async fn the_results_name_the_picture_they_carry() {
        let dir = scratch("results");
        let (state, infra) = state(&dir);
        {
            let mut guard = infra.lock().await;
            guard.hold_config(InfraConfig {
                version: 7,
                check_interval_secs: 60,
                targets: vec![target("t1"), target("t2")],
            });
            let good = CheckResult {
                ok: true,
                latency_ms: Some(3),
                detail: "PING OK 3ms".into(),
                ..CheckResult::default()
            };
            let run = monitor::apply_result(
                &mut guard,
                &target("t1"),
                good,
                7,
                Instant::now(),
                monitor::Reading::Monitor,
            );
            assert!(run.is_none());
            guard.push_recovery_log(RecoveryLogEntry {
                ts: super::super::now_iso(),
                target_id: "t2".into(),
                command: "true".into(),
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let Json(report) = get_results(State(state.clone())).await.unwrap();
        let body = serde_json::to_value(&report).unwrap();
        let stamp = state.infra_digest.stamp();
        assert_eq!(body["boot"], 1_790_000_000_000_u64);
        assert_eq!(body["seq"], stamp.seq);
        assert_eq!(body["hash"], stamp.hash.as_str());
        assert_eq!(body["recoveries"], 1);
        assert_eq!(body["config_version"], 7);
        assert_eq!(body["recovery_log"].as_array().unwrap().len(), 1);

        // A pull settles a comparison: the results hash to the hash beside them.
        let statuses: Vec<(String, TargetStatus)> = body["targets"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, t)| {
                let status = serde_json::from_value(t["status"].clone()).unwrap();
                (id.clone(), status)
            })
            .collect();
        let text = picture_text(
            1_790_000_000_000,
            7,
            statuses.iter().map(|(id, status)| (id.as_str(), *status)),
        );
        assert_eq!(text, "1790000000000\n7\nt1 up\nt2 unknown\n");
        assert_eq!(body["hash"], picture_hash(&text).as_str());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_push_of_the_config_held_moves_no_seq() {
        let dir = scratch("push");
        let (state, infra) = state(&dir);
        // No target: the monitor this starts checks nothing.
        let config = InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets: Vec::new(),
        };
        let push = |config: InfraConfig| {
            let state = state.clone();
            async move { push_config(State(state), Json(config)).await.unwrap().0 }
        };
        let remove = || {
            let state = state.clone();
            async move { delete_config(State(state)).await.unwrap().0 }
        };

        assert_eq!(push(config.clone()).await["config_version"], 7);
        let first = state.infra_digest.stamp();
        assert_eq!(first.seq, 2, "a config applied is a change");
        assert_eq!(push(config.clone()).await["status"], "ok");
        assert_eq!(state.infra_digest.stamp(), first, "nothing changed");
        assert!(state.infra_digest.group().off.is_none());

        let moved = InfraConfig {
            version: 8,
            ..config
        };
        assert_eq!(push(moved).await["config_version"], 8);
        assert_eq!(state.infra_digest.stamp().seq, 3);

        assert_eq!(remove().await["status"], "ok");
        assert_eq!(state.infra_digest.stamp().seq, 4, "a removal is a change");
        assert_eq!(remove().await["status"], "ok");
        assert_eq!(
            state.infra_digest.stamp().seq,
            4,
            "removing nothing is not one"
        );
        if let Some(handle) = infra.lock().await.monitor_handle.take() {
            handle.abort();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── The monitor (ADR-010 decisions 6 to 10) ─────────────────────

    use std::collections::HashMap as Map;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use futures_util::FutureExt as _;
    use tokio::sync::{mpsc, Semaphore};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    use crate::infra::checks::CheckContext;
    use crate::infra::monitor::{Checker, Kit, Timing};
    use crate::infra::{Off, RecoveryConfig};

    async fn push(state: &AppState, config: InfraConfig) -> Result<Value, (StatusCode, Value)> {
        push_config(State(state.clone()), Json(config))
            .await
            .map(|Json(body)| body)
            .map_err(|(status, Json(body))| (status, body))
    }

    async fn check_now(state: &AppState, id: &str) -> Value {
        check_target(State(state.clone()), Path(id.to_string()))
            .await
            .unwrap()
            .0
    }

    /// The forwarder of a relay that takes `infra.state`.
    fn forwarder(
        infra: &Arc<Mutex<InfraState>>,
    ) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<WsMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let task =
            crate::tunnel::infra_state::spawn_forwarder(true, Some(infra.clone()), tx).unwrap();
        (task, rx)
    }

    async fn next_message(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a message")
            .expect("the forwarder is running");
        match message {
            WsMessage::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("not text: {other:?}"),
        }
    }

    /// The next message in which `id` reads `status`.
    async fn message_with(rx: &mut mpsc::Receiver<WsMessage>, id: &str, status: &str) -> Value {
        loop {
            let message = next_message(rx).await;
            if message["targets"][id]["status"] == status {
                return message;
            }
        }
    }

    /// Nothing sent for longer than the forwarder's one second.
    async fn silent(rx: &mut mpsc::Receiver<WsMessage>) -> bool {
        tokio::time::timeout(Duration::from_millis(1500), rx.recv())
            .await
            .is_err()
    }

    async fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + limit;
        while !done() {
            assert!(std::time::Instant::now() < deadline, "not within {limit:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn answer(ok: bool, latency_ms: u64) -> CheckResult {
        if ok {
            CheckResult {
                ok: true,
                latency_ms: Some(latency_ms),
                detail: format!("OK {latency_ms}ms"),
                ..CheckResult::default()
            }
        } else {
            CheckResult::failed("NO ANSWER")
        }
    }

    /// Checks counted by target id, each answering as `answers` says for
    /// its target. Nothing is sent.
    fn counting(
        answers: impl Fn(&str) -> CheckResult + Send + Sync + 'static,
    ) -> (Checker, Arc<StdMutex<Map<String, u32>>>) {
        let calls = Arc::new(StdMutex::new(Map::new()));
        let checker: Checker = {
            let calls = calls.clone();
            Arc::new(move |target: &InfraTarget, _: CheckContext| {
                *calls.lock().unwrap().entry(target.id.clone()).or_insert(0) += 1;
                let result = answers(&target.id);
                async move { result }.boxed()
            })
        };
        (checker, calls)
    }

    /// Checks that answer only as the test opens `gate`, one per permit,
    /// each with `latency_ms` 40 plus its call number.
    fn gated(gate: Arc<Semaphore>) -> (Checker, Arc<StdMutex<u32>>) {
        let calls = Arc::new(StdMutex::new(0));
        let checker: Checker = {
            let calls = calls.clone();
            Arc::new(move |_: &InfraTarget, _: CheckContext| {
                let call = {
                    let mut calls = calls.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    answer(true, 40 + u64::from(call))
                }
                .boxed()
            })
        };
        (checker, calls)
    }

    async fn use_checks(infra: &Arc<Mutex<InfraState>>, timing: Timing, checker: Checker) {
        infra.lock().await.monitor_kit = Kit::Given { timing, checker };
    }

    async fn monitor_task(infra: &Arc<Mutex<InfraState>>) -> tokio::task::Id {
        infra.lock().await.monitor_handle.as_ref().unwrap().id()
    }

    async fn stop_monitor(infra: &Arc<Mutex<InfraState>>) {
        if let Some(handle) = infra.lock().await.monitor_handle.take() {
            handle.abort();
        }
    }

    fn switch(community: &str) -> InfraConfig {
        InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets: vec![InfraTarget {
                check: CheckSpec::Snmp {
                    host: "192.0.2.10".into(),
                    community: Some(community.into()),
                    timeout_ms: None,
                },
                ..target("switch")
            }],
        }
    }

    /// The fleet pushes its config again whenever it is unsure; a push
    /// that changes nothing must cost the unit nothing.
    #[tokio::test]
    async fn an_identical_push_changes_nothing_and_a_changed_one_is_applied() {
        let dir = scratch("identical");
        let (state, infra) = state(&dir);
        // Checks that never answer: no status moves while pushes are read.
        let never: Checker =
            Arc::new(|_: &InfraTarget, _: CheckContext| futures_util::future::pending().boxed());
        use_checks(&infra, Timing::default(), never).await;
        let (task, mut rx) = forwarder(&infra);
        next_message(&mut rx).await;

        let first = push(&state, switch("site-a")).await.unwrap();
        assert_eq!(
            (&first["unchanged"], &first["started"]),
            (&json!(false), &json!(true))
        );
        assert_eq!(next_message(&mut rx).await["config_version"], 7);
        let monitor = monitor_task(&infra).await;
        let stamp = state.infra_digest.stamp();

        let again = push(&state, switch("site-a")).await.unwrap();
        assert_eq!(
            (&again["unchanged"], &again["started"]),
            (&json!(true), &json!(false))
        );
        assert_eq!(monitor_task(&infra).await, monitor, "the same monitor task");
        assert_eq!(state.infra_digest.stamp(), stamp, "nothing signalled");
        assert!(silent(&mut rx).await, "no infra.state");

        // One community changed under the same version: applied.
        let changed = push(&state, switch("site-b")).await.unwrap();
        assert_eq!(
            (&changed["unchanged"], &changed["started"]),
            (&json!(false), &json!(true))
        );
        assert_ne!(monitor_task(&infra).await, monitor, "a new monitor");
        assert_eq!(infra.lock().await.config, Some(switch("site-b")));
        assert_eq!(state.infra_digest.stamp().seq, stamp.seq + 1);
        let saved = std::fs::read_to_string(dir.join("state/infra-monitor.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<InfraConfig>(&saved).unwrap(),
            switch("site-b")
        );
        next_message(&mut rx).await;

        // The monitor ended: the same config starts one, and pushes nothing.
        let ended = infra
            .lock()
            .await
            .monitor_handle
            .as_ref()
            .unwrap()
            .abort_handle();
        ended.abort();
        wait_until(Duration::from_secs(5), || ended.is_finished()).await;
        let restarted = push(&state, switch("site-b")).await.unwrap();
        assert_eq!(
            (&restarted["unchanged"], &restarted["started"]),
            (&json!(true), &json!(true))
        );
        assert!(!infra
            .lock()
            .await
            .monitor_handle
            .as_ref()
            .unwrap()
            .is_finished());
        assert!(silent(&mut rx).await);

        // Version 0 means the unit holds no config: refused, nothing changed.
        let (status, body) = push(
            &state,
            InfraConfig {
                version: 0,
                ..switch("site-c")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            (status, &body["code"]),
            (StatusCode::BAD_REQUEST, &json!("INVALID_REQUEST"))
        );
        assert_eq!(infra.lock().await.config, Some(switch("site-b")));

        stop_monitor(&infra).await;
        task.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_identical_push_writes_the_config_again_only_after_a_failed_save() {
        let dir = scratch("resave");
        let (state, infra) = state(&dir);
        use_checks(&infra, Timing::default(), counting(|_| answer(true, 3)).0).await;
        // A file where the config's directory should be: every save fails.
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let path = blocked.join("infra-monitor.json");
        infra.lock().await.config_path.clone_from(&path);

        push(&state, switch("site-a")).await.unwrap();
        assert!(!infra.lock().await.config_saved);
        assert!(!path.exists());

        // The disk takes it now: the same config is written again.
        std::fs::remove_file(&blocked).unwrap();
        let again = push(&state, switch("site-a")).await.unwrap();
        assert_eq!(again["unchanged"], true);
        assert!(infra.lock().await.config_saved);
        assert!(path.exists());

        // Saved already: an identical push writes nothing.
        std::fs::remove_file(&path).unwrap();
        push(&state, switch("site-a")).await.unwrap();
        assert!(!path.exists(), "not written again");

        stop_monitor(&infra).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn in_safe_mode_a_push_saves_the_config_and_starts_nothing() {
        let dir = scratch("safe-mode");
        let (state, infra) = state_with(&dir, Digest::new(1_790_000_000_000, true));
        let config = switch("site-a");

        let pushed = push(&state, config.clone()).await.unwrap();
        assert_eq!(
            (&pushed["unchanged"], &pushed["started"]),
            (&json!(false), &json!(false))
        );
        assert_eq!(pushed["status"], "ok");
        assert!(infra.lock().await.monitor_handle.is_none(), "no monitor");
        let saved = std::fs::read_to_string(dir.join("state/infra-monitor.json")).unwrap();
        assert_eq!(serde_json::from_str::<InfraConfig>(&saved).unwrap(), config);
        assert_eq!(state.infra_digest.group().off, Some(Off::SafeMode));

        let again = push(&state, config).await.unwrap();
        assert_eq!(
            (&again["unchanged"], &again["started"]),
            (&json!(true), &json!(false))
        );
        assert!(infra.lock().await.monitor_handle.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A monitor a push started runs under the supervisor: a panic in its
    /// loop is rebuilt after 5 s, from the config the unit holds then.
    #[tokio::test(start_paused = true)]
    async fn a_monitor_a_push_started_is_rebuilt_from_the_config_held_then() {
        let dir = scratch("rebuilt");
        let (state, infra) = state(&dir);
        let calls: Arc<StdMutex<Vec<(String, Instant)>>> = Arc::default();
        let checker: Checker = {
            let calls = calls.clone();
            Arc::new(move |target: &InfraTarget, _: CheckContext| {
                let first = {
                    let mut calls = calls.lock().unwrap();
                    calls.push((target.id.clone(), Instant::now()));
                    calls.len() == 1
                };
                // The tick starts a check by calling this: a panic here is
                // a panic in the monitor's own loop.
                assert!(!first, "a defect in the monitor's loop");
                async { answer(true, 3) }.boxed()
            })
        };
        use_checks(&infra, Timing::default(), checker).await;
        let t0 = Instant::now();
        let config = |version: u32, id: &str| InfraConfig {
            version,
            check_interval_secs: 3600,
            targets: vec![target(id)],
        };
        assert_eq!(
            push(&state, config(7, "t1")).await.unwrap()["started"],
            true
        );
        let supervisor = monitor_task(&infra).await;

        // While the supervisor waits, the unit comes to hold another config.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(infra.lock().await.hold_config(config(8, "t2")));
        tokio::time::sleep(Duration::from_secs(5)).await;

        let calls = calls.lock().unwrap().clone();
        let seen: Vec<(&str, Duration)> = calls
            .iter()
            .map(|(id, at)| (id.as_str(), at.duration_since(t0)))
            .collect();
        assert_eq!(
            seen,
            [("t1", Duration::ZERO), ("t2", Duration::from_secs(5))],
            "rebuilt after 5 s, watching the config held then"
        );
        assert_eq!(
            monitor_task(&infra).await,
            supervisor,
            "the same supervisor"
        );
        assert_eq!(
            infra.lock().await.results.targets["t2"].status,
            TargetStatus::Up,
            "its results are applied"
        );
        stop_monitor(&infra).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The monitor's clock in the recovery tests: a tick of 100 ms and a
    /// deadline of 500 ms, so a running check's permit held through a run
    /// would stop the rounds 0.6 s after it was taken.
    fn fast() -> Timing {
        Timing {
            tick: Duration::from_millis(100),
            deadline: Duration::from_millis(500),
            ..Timing::default()
        }
    }

    /// A unit's power outlet that answers no check, Down at its second
    /// failure, whose recovery runs `command` with `credential_ids`, and a
    /// camera that answers: both due at every tick.
    fn outlet_and_camera(command: &str, credential_ids: Vec<String>) -> InfraConfig {
        let mut pdu = target("pdu");
        pdu.interval_secs = Some(0);
        pdu.down_after_consecutive = 1;
        pdu.recovery = Some(RecoveryConfig {
            enabled: true,
            command: command.to_string(),
            cooldown_secs: 3600,
            max_retries: 5,
            credential_ids,
        });
        let mut cam = target("cam");
        cam.interval_secs = Some(0);
        InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets: vec![pdu, cam],
        }
    }

    /// A unit's power outlet that answers no check, with a recovery that
    /// prints its saved record as it starts and then takes 4 s.
    #[tokio::test]
    async fn a_recovery_runs_after_the_down_outside_the_lock_and_a_push_does_not_cut_it() {
        let dir = scratch("recovery");
        let (state, infra) = state(&dir);
        let record = dir.join("state/infra-recovery.json");
        // The record's path reaches the command as a credential's username,
        // in its environment, so the shell never parses the path.
        infra.lock().await.credentials.insert(
            "record".into(),
            Credential {
                username: record.display().to_string(),
                password: "unused".into(),
            },
        );
        let command = "cat \"$SCTL_CRED_USER_1\"; sleep 4".to_string();
        let (checker, calls) = counting(|id| answer(id != "pdu", 3));
        let calls_of = |id: &str| calls.lock().unwrap().get(id).copied().unwrap_or(0);
        use_checks(&infra, fast(), checker).await;
        let (task, mut rx) = forwarder(&infra);
        next_message(&mut rx).await;
        let config = outlet_and_camera(&command, vec!["record".into()]);
        push(&state, config.clone()).await.unwrap();

        // The Down leaves while the run is in progress, never after it.
        let down = message_with(&mut rx, "pdu", "down").await;
        assert_eq!(down["recoveries"], 0, "not logged yet");
        let runs = state.infra_digest.runs();
        assert!(runs.contains("pdu"), "the run is in progress");

        // The run holds the target's place, not the permit of a running
        // check: the rounds go on past the deadline plus a tick.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let rounds = state.infra_digest.group().rounds;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let later = state.infra_digest.group().rounds;
        assert!(later >= rounds + 3, "rounds {rounds}, then {later}");
        assert!(runs.contains("pdu"), "both read during the run");

        // During the run: no check of pdu starts, cam's go on, and Check
        // now of pdu answers the Down check's result at once.
        let (pdu_checks, cam_checks) = (calls_of("pdu"), calls_of("cam"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls_of("pdu"), pdu_checks, "its place is the run's");
        assert!(
            calls_of("cam") >= cam_checks + 3,
            "another target's checks go on"
        );
        let asked = std::time::Instant::now();
        let now = check_now(&state, "pdu").await;
        assert!(asked.elapsed() < Duration::from_millis(500));
        assert_eq!(
            (&now["ok"], &now["detail"]),
            (&json!(false), &json!("NO ANSWER"))
        );
        assert_eq!(calls_of("pdu"), pdu_checks, "no second check of pdu");
        assert_eq!(runs.in_progress(), 1, "no second run");

        // A push during the run lets it finish.
        let mut renamed = config.clone();
        renamed.targets[1].name = "Camera".into();
        assert_eq!(push(&state, renamed).await.unwrap()["started"], true);
        assert!(runs.contains("pdu"));

        // It ends by itself, logged once, and the logged run is pushed.
        wait_until(Duration::from_secs(10), || !runs.contains("pdu")).await;
        while next_message(&mut rx).await["recoveries"] != 1 {}
        let log = infra.lock().await.results.recovery_log.clone();
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].target_id.as_str(), log[0].exit_code), ("pdu", 0));
        assert_eq!(state.infra_digest.stamp().recoveries, 1);
        // Its record was on disk, not ended, before the command started:
        // the command printed it.
        let at_start: Value = serde_json::from_str(&log[0].stdout).unwrap();
        assert_eq!(at_start["running"]["pdu"]["command"], command.as_str());
        assert_eq!(at_start["counts"]["pdu"], 1);
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
        assert_eq!(now, json!({"counts": {"pdu": 1}, "running": {}}), "ended");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(infra.lock().await.results.recovery_log.len(), 1, "once");

        stop_monitor(&infra).await;
        task.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A removed config does not cut a recovery run: the run ends by
    /// itself, logged once with the command's own exit code, and saved as
    /// ended.
    #[tokio::test]
    async fn a_removed_config_does_not_cut_a_recovery_run() {
        let dir = scratch("removed");
        let (state, infra) = state(&dir);
        let (checker, _) = counting(|id| answer(id != "pdu", 3));
        use_checks(&infra, fast(), checker).await;
        push(&state, outlet_and_camera("sleep 2; exit 7", Vec::new()))
            .await
            .unwrap();
        let runs = state.infra_digest.runs();
        wait_until(Duration::from_secs(5), || runs.contains("pdu")).await;

        let removed = delete_config(State(state.clone())).await.unwrap().0;
        assert_eq!(removed["status"], "ok");
        assert!(infra.lock().await.config.is_none());
        assert!(runs.contains("pdu"), "the run goes on");

        wait_until(Duration::from_secs(10), || !runs.contains("pdu")).await;
        let log = infra.lock().await.results.recovery_log.clone();
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].target_id.as_str(), log[0].exit_code), ("pdu", 7));
        assert!(
            !dir.join("state/infra-recovery.json").exists(),
            "saved as ended, and the removal cleared the count"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(infra.lock().await.results.recovery_log.len(), 1, "once");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A monitor that panics while a recovery run is in progress is rebuilt
    /// after 5 s by the same supervisor, and the run, outside its task set,
    /// goes on: it ends by itself, logged once with the command's own exit
    /// code.
    #[tokio::test]
    async fn a_monitor_rebuilt_after_a_panic_does_not_cut_a_recovery_run() {
        let dir = scratch("rebuilt-run");
        let (state, infra) = state(&dir);
        let runs = state.infra_digest.runs();
        let panicked = Arc::new(AtomicBool::new(false));
        let cam_checks: Arc<StdMutex<Vec<Instant>>> = Arc::default();
        let checker: Checker = {
            let (runs, panicked, cam_checks) = (runs.clone(), panicked.clone(), cam_checks.clone());
            Arc::new(move |target: &InfraTarget, _: CheckContext| {
                // The tick calls this as it starts a check: the first start
                // of cam once pdu's run is in progress is a defect in the
                // monitor's own loop.
                let defect =
                    target.id == "cam" && runs.contains("pdu") && !panicked.swap(true, SeqCst);
                assert!(!defect, "a defect in the monitor's loop");
                if target.id == "cam" {
                    cam_checks.lock().unwrap().push(Instant::now());
                }
                let result = answer(target.id != "pdu", 3);
                async move { result }.boxed()
            })
        };
        use_checks(&infra, fast(), checker).await;
        push(&state, outlet_and_camera("sleep 7; exit 7", Vec::new()))
            .await
            .unwrap();
        let supervisor = monitor_task(&infra).await;
        wait_until(Duration::from_secs(5), || panicked.load(SeqCst)).await;
        let panicked_at = Instant::now();
        assert!(runs.contains("pdu"), "the run goes on");

        // Rebuilt after 5 s, while the run goes on: cam is checked again,
        // pdu, whose place the run holds, is not.
        wait_until(Duration::from_secs(10), || {
            cam_checks
                .lock()
                .unwrap()
                .last()
                .is_some_and(|at| *at > panicked_at)
        })
        .await;
        assert!(panicked_at.elapsed() >= Duration::from_secs(4));
        assert!(runs.contains("pdu"), "the run is still in progress");
        assert_eq!(
            monitor_task(&infra).await,
            supervisor,
            "the same supervisor"
        );

        wait_until(Duration::from_secs(10), || !runs.contains("pdu")).await;
        let log = infra.lock().await.results.recovery_log.clone();
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].target_id.as_str(), log[0].exit_code), ("pdu", 7));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(infra.lock().await.results.recovery_log.len(), 1, "once");
        stop_monitor(&infra).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Check now is a reading: it moves the record, and a few quick clicks
    /// on a failing target bring the Down sooner. It never starts a
    /// recovery.
    #[tokio::test]
    async fn check_now_records_its_reading_and_starts_no_recovery() {
        let dir = scratch("check-now");
        let (state, infra) = state(&dir);
        let script = Arc::new(StdMutex::new(vec![
            answer(true, 7),
            answer(false, 0),
            answer(false, 0),
            answer(false, 0),
        ]));
        let checker: Checker = {
            let script = script.clone();
            Arc::new(move |_: &InfraTarget, _: CheckContext| {
                let result = script.lock().unwrap().remove(0);
                async move { result }.boxed()
            })
        };
        use_checks(&infra, Timing::default(), checker).await;
        let mut t1 = target("t1");
        t1.recovery = Some(RecoveryConfig {
            enabled: true,
            command: "true".into(),
            cooldown_secs: 0,
            max_retries: 5,
            credential_ids: Vec::new(),
        });
        // Held with no monitor: only Check now reads the target.
        infra.lock().await.hold_config(InfraConfig {
            version: 7,
            check_interval_secs: 3600,
            targets: vec![t1],
        });
        let (task, mut rx) = forwarder(&infra);
        next_message(&mut rx).await;
        let rounds = state.infra_digest.group().rounds;

        let first = check_now(&state, "t1").await;
        assert_eq!(
            (&first["ok"], &first["latency_ms"]),
            (&json!(true), &json!(7))
        );
        {
            let guard = infra.lock().await;
            let t1 = &guard.results.targets["t1"];
            assert!(!t1.last_check.is_empty(), "last_check moved");
            assert_eq!((t1.status, t1.latency_ms), (TargetStatus::Up, Some(7)));
        }
        message_with(&mut rx, "t1", "up").await;

        // Up, then failures: Degraded, and Down on the one that crosses
        // down_after (3).
        for expected in ["degraded", "degraded", "down"] {
            assert_eq!(check_now(&state, "t1").await["ok"], false);
            let status = infra.lock().await.results.targets["t1"].status;
            assert_eq!(status.to_string(), expected);
        }
        let down = message_with(&mut rx, "t1", "down").await;
        assert_eq!(down["targets"]["t1"]["consecutive_fail"], 3);
        let guard = infra.lock().await;
        assert!(guard.results.recovery_log.is_empty(), "no recovery");
        assert!(guard.recovery_tracker.is_empty());
        assert_eq!(state.infra_digest.runs().in_progress(), 0);
        drop(guard);
        assert_eq!(
            state.infra_digest.group().rounds,
            rounds,
            "no round counted"
        );
        task.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A target has at most one check in flight: Check now on a target the
    /// monitor is checking waits for that check and answers its result.
    #[tokio::test]
    async fn check_now_waits_for_the_monitors_check_in_flight() {
        let dir = scratch("in-flight");
        let (state, infra) = state(&dir);
        let gate = Arc::new(Semaphore::new(0));
        let (checker, calls) = gated(gate.clone());
        use_checks(&infra, Timing::default(), checker).await;
        let config = InfraConfig {
            version: 7,
            check_interval_secs: 3600,
            targets: vec![target("t1")],
        };
        push(&state, config).await.unwrap();
        wait_until(Duration::from_secs(5), || *calls.lock().unwrap() == 1).await;

        let asking = tokio::spawn({
            let state = state.clone();
            async move { check_now(&state, "t1").await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!asking.is_finished(), "it waits for the check in flight");
        gate.add_permits(1);
        let answered = tokio::time::timeout(Duration::from_secs(5), asking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answered["latency_ms"], 41, "that check's result");
        assert_eq!(*calls.lock().unwrap(), 1, "no second check ran");
        assert_eq!(
            infra.lock().await.results.targets["t1"].latency_ms,
            Some(41)
        );
        stop_monitor(&infra).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A result is applied only while the unit still holds the config the
    /// check ran under.
    #[tokio::test]
    async fn a_check_now_whose_config_changed_during_the_check_is_answered_not_applied() {
        let dir = scratch("changed");
        let (state, infra) = state(&dir);
        let gate = Arc::new(Semaphore::new(0));
        let (checker, calls) = gated(gate.clone());
        use_checks(&infra, Timing::default(), checker).await;
        let config = |version: u32| InfraConfig {
            version,
            check_interval_secs: 3600,
            targets: vec![target("t1")],
        };
        infra.lock().await.hold_config(config(7));

        let asking = tokio::spawn({
            let state = state.clone();
            async move { check_now(&state, "t1").await }
        });
        wait_until(Duration::from_secs(5), || *calls.lock().unwrap() == 1).await;
        assert!(infra.lock().await.hold_config(config(8)));
        let seq = state.infra_digest.stamp().seq;
        gate.add_permits(1);
        let answered = tokio::time::timeout(Duration::from_secs(5), asking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (&answered["ok"], &answered["latency_ms"]),
            (&json!(true), &json!(41))
        );
        let guard = infra.lock().await;
        let t1 = &guard.results.targets["t1"];
        assert_eq!(t1.status, TargetStatus::Unknown, "not applied");
        assert!(t1.last_check.is_empty());
        assert_eq!(state.infra_digest.stamp().seq, seq);
        drop(guard);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Check now runs under the monitor's 90 s deadline, and waits for a
    /// check in flight at most that long. Each call is bounded a second
    /// past it: on the paused clock with no other timer, a Check now that
    /// lost its deadline would wait for good, and the run would hang
    /// instead of failing.
    #[tokio::test(start_paused = true)]
    async fn check_now_answers_within_the_deadline() {
        let dir = scratch("deadline");
        let (state, infra) = state(&dir);
        let never: Checker =
            Arc::new(|_: &InfraTarget, _: CheckContext| futures_util::future::pending().boxed());
        use_checks(&infra, Timing::default(), never).await;
        infra.lock().await.hold_config(InfraConfig {
            version: 7,
            check_interval_secs: 3600,
            targets: vec![target("t1")],
        });
        let bounded = |state: AppState| async move {
            tokio::time::timeout(Duration::from_secs(91), check_now(&state, "t1"))
                .await
                .expect("bounded by the deadline")
        };

        let asked = Instant::now();
        let answered = bounded(state.clone()).await;
        assert_eq!(asked.elapsed(), Duration::from_secs(90));
        assert_eq!(answered["detail"], "CHECK DEADLINE: no answer within 90s");
        let t1 = infra.lock().await.results.targets["t1"].clone();
        assert_eq!(
            (t1.status, t1.consecutive_fail),
            (TargetStatus::Degraded, 1),
            "a failed reading, applied"
        );

        // A check in flight that never answers: Check now waits 90 s, and
        // applies nothing of its own.
        let held = infra.lock().await.in_flight.start("t1").unwrap();
        let asked = Instant::now();
        let answered = bounded(state.clone()).await;
        assert_eq!(asked.elapsed(), Duration::from_secs(90));
        assert_eq!(
            answered["detail"],
            "CHECK DEADLINE: the check in flight gave no answer within 90s"
        );
        assert_eq!(infra.lock().await.results.targets["t1"].consecutive_fail, 1);
        drop(held);
        std::fs::remove_dir_all(&dir).ok();
    }
}
