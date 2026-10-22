//! HTTP route handlers for the infra monitoring API.
//!
//! - `POST /api/infra/config` — push monitoring config, start/restart monitor
//! - `GET  /api/infra/results` — latest monitoring results
//! - `POST /api/infra/check/{target_id}` — on-demand check for one target
//! - `DELETE /api/infra/config` — stop monitoring, remove config
//! - `POST /api/infra/discover` — trigger LAN discovery scan

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use tracing::info;

use super::monitor;
use std::collections::HashMap;

use super::checks::CheckContext;
use super::{CheckSpec, Credential, InfraConfig, InfraResults, Stamp};
use crate::AppState;

/// `POST /api/infra/config` — receive and apply monitoring config.
///
/// Persists config to disk, aborts any running monitor, and spawns a new
/// monitoring loop with the updated config.
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

    let version = config.version;
    let target_count = config.targets.len();

    let mut guard = infra.lock().await;

    // Abort existing monitor if running
    if let Some(handle) = guard.monitor_handle.take() {
        handle.abort();
    }

    // Store and persist config. Results reflect the new target set at once:
    // targets that left are dropped, new ones appear as `unknown`, and the
    // version is the pushed one, so a collector reading results between the
    // push and the first check sees the config it pushed, not the old one.
    // A config equal to the one held is no change: `seq` stays.
    guard.hold_config(config.clone());
    guard.save_config();

    // Spawn new monitor
    let handle = monitor::spawn_monitor(infra.clone(), config);
    guard.monitor_handle = Some(handle);

    info!("Infra config v{version} applied: {target_count} targets");

    Ok(Json(json!({
        "status": "ok",
        "config_version": version,
        "target_count": target_count
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

/// `POST /api/infra/check/{target_id}` — run an immediate on-demand check.
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

    let guard = infra.lock().await;
    let Some(ref config) = guard.config else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "No config loaded", "code": "NOT_FOUND", "message": "No config loaded"}),
            ),
        ));
    };

    let target = config.targets.iter().find(|t| t.id == target_id);
    let Some(target) = target else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": format!("Target {target_id} not found"), "code": "NOT_FOUND", "message": format!("Target {target_id} not found")}),
            ),
        ));
    };

    let check_spec = target.check.clone();
    let ctx = match &check_spec {
        CheckSpec::HttpApi { credential_id, .. } => CheckContext {
            data_dir: state.config.server.state_dir().to_string(),
            credential: credential_id
                .as_ref()
                .and_then(|id| guard.credentials.get(id).cloned()),
            session: guard.sessions.get(&target_id).cloned(),
        },
        _ => CheckContext::default(),
    };
    drop(guard); // release lock during check

    let result = super::checks::run_check_with(&check_spec, &ctx).await;

    // An on-demand check is also how a fresh session is established, so keep
    // what it produced for the monitor's next tick.
    if result.session.is_some() || !result.ok {
        let mut guard = infra.lock().await;
        match &result.session {
            Some(sess) => {
                guard.sessions.insert(target_id.clone(), sess.clone());
            }
            None => {
                guard.sessions.remove(&target_id);
            }
        }
    }

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

    // Abort monitor
    if let Some(handle) = guard.monitor_handle.take() {
        handle.abort();
    }

    // Clear config and results; an empty result set is a change to push,
    // unless the unit held nothing.
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
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        let mut state = AppState::for_tests(config, gate);
        let digest = Arc::new(Digest::new(1_790_000_000_000, false));
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
            monitor::apply_result(&mut guard, &target("t1"), good, 7, Instant::now()).await;
            guard.push_recovery_log(RecoveryLogEntry {
                ts: super::super::now_iso(),
                target_id: "t2".into(),
                command: "true".into(),
                exit_code: 0,
                stdout: String::new(),
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
}
