//! `infra.state`: the device's Infra results, pushed to the relay when they
//! change.
//!
//! The device offers the feature in `tunnel.register` and sends the message
//! only when the relay's `tunnel.register.ack` advertises it too: once right
//! after the ack, then after every change the infra module signals (a
//! target's status, a config applied or removed; see
//! [`InfraState::changed`]) whose content differs from the last message
//! sent on this connection, at most one a second. A latency reading or a
//! counter alone is not a change. It carries what `GET /api/infra/results`
//! carries per target, minus `data` and the recovery log, so a collector
//! that reads the stream can stop polling that route.
//!
//! ```json
//! {"type": "infra.state", "v": 1, "ts": "2026-09-30T12:00:00Z",
//!  "config_version": 7,
//!  "targets": {
//!    "6f0b2c4e-9d3a-4c1f-8e2b-1a2b3c4d5e6f": {
//!      "status": "up", "latency_ms": 40, "since": "2026-09-30T11:58:00Z",
//!      "consecutive_ok": 3, "consecutive_fail": 0,
//!      "last_check": "2026-09-30T12:00:00Z", "detail": "API OK",
//!      "name": "Peplink MAX BR1", "http_status": 200}},
//!  "truncated": false}
//! ```
//!
//! `targets` is sorted by id. `detail` is the first 160 characters.
//! `http_status` is present only when the check speaks HTTP. A message must
//! fit the 16 KiB the relay accepts: over it, every `detail` goes first,
//! then every `name`, then the tail of the target list, and `truncated`
//! says so.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::infra::{InfraResults, InfraState, TargetStatus};

/// The feature name in `tunnel.register` and `tunnel.register.ack`.
pub const FEATURE: &str = "infra.state";
/// The message version, `v`.
pub const VERSION: u32 = 1;
/// The largest message the relay accepts.
pub const MAX_BYTES: usize = 16 * 1024;
/// `detail` is cut to this many characters.
pub const MAX_DETAIL_CHARS: usize = 160;
/// At most one message per this interval on a connection.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// One `infra.state` message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InfraStateMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub v: u32,
    /// The device's clock when the message was built.
    pub ts: String,
    #[serde(flatten)]
    pub body: Body,
}

/// What a message says about the targets: the part compared to decide
/// whether a new message is worth sending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Body {
    /// The config the results reflect; 0 when the device holds none.
    pub config_version: u32,
    /// Per-target state, by target id.
    pub targets: BTreeMap<String, TargetEntry>,
    /// Whether the cap dropped a field or a target.
    pub truncated: bool,
}

/// One target, as `GET /api/infra/results` reports it minus `data`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TargetEntry {
    pub status: TargetStatus,
    pub latency_ms: Option<u64>,
    /// When the current status began.
    pub since: String,
    pub consecutive_ok: u32,
    pub consecutive_fail: u32,
    /// When the last check ran; empty before the first one.
    pub last_check: String,
    /// The last check's detail, cut to [`MAX_DETAIL_CHARS`]; dropped first
    /// when the message is over the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The target's name from the config; dropped second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Present when the check speaks HTTP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

/// Whether the relay's `tunnel.register.ack` advertises `infra.state`.
pub fn advertised(ack: &Value) -> bool {
    ack["features"]
        .as_array()
        .is_some_and(|features| features.iter().any(|f| f.as_str() == Some(FEATURE)))
}

fn fits(message: &InfraStateMessage) -> bool {
    serde_json::to_vec(message).is_ok_and(|bytes| bytes.len() <= MAX_BYTES)
}

/// Build the message for `results`, applying the cap.
pub fn build(results: &InfraResults, ts: String) -> InfraStateMessage {
    let targets: BTreeMap<String, TargetEntry> = results
        .targets
        .iter()
        .map(|(id, target)| {
            let entry = TargetEntry {
                status: target.status,
                latency_ms: target.latency_ms,
                since: target.since.clone(),
                consecutive_ok: target.consecutive_ok,
                consecutive_fail: target.consecutive_fail,
                last_check: target.last_check.clone(),
                detail: Some(target.detail.chars().take(MAX_DETAIL_CHARS).collect()),
                name: Some(target.name.clone()),
                http_status: target.http_status,
            };
            (id.clone(), entry)
        })
        .collect();
    let mut message = InfraStateMessage {
        kind: FEATURE,
        v: VERSION,
        ts,
        body: Body {
            config_version: results.config_version,
            targets,
            truncated: false,
        },
    };
    if fits(&message) {
        return message;
    }
    message.body.truncated = true;
    for entry in message.body.targets.values_mut() {
        entry.detail = None;
    }
    if fits(&message) {
        return message;
    }
    for entry in message.body.targets.values_mut() {
        entry.name = None;
    }
    if fits(&message) {
        return message;
    }
    // The first N by id, for the largest N that fits: the size grows with
    // N, so a binary search finds it. `fitting` fits (no targets always
    // does), `over` does not.
    let all: Vec<(String, TargetEntry)> = std::mem::take(&mut message.body.targets)
        .into_iter()
        .collect();
    let (mut fitting, mut over) = (0, all.len());
    while over - fitting > 1 {
        let mid = fitting + (over - fitting) / 2;
        message.body.targets = all[..mid].iter().cloned().collect();
        if fits(&message) {
            fitting = mid;
        } else {
            over = mid;
        }
    }
    message.body.targets = all[..fitting].iter().cloned().collect();
    message
}

/// Start this connection's `infra.state` forwarder, unless the relay did
/// not advertise the feature or the device has no infra state. The caller
/// aborts the task when the connection ends.
pub fn spawn_forwarder(
    relay_takes_it: bool,
    infra: Option<Arc<Mutex<InfraState>>>,
    tx: mpsc::Sender<WsMessage>,
) -> Option<JoinHandle<()>> {
    if !relay_takes_it {
        return None;
    }
    Some(tokio::spawn(forward(infra?, tx, MIN_INTERVAL)))
}

/// Send the results now, then after every signalled change whose message
/// body differs from the last one sent, at most one per `min_interval`. A
/// send waits for room in the writer's queue: a change is never dropped,
/// only folded into the next message. Returns when the writer is gone.
async fn forward(
    infra: Arc<Mutex<InfraState>>,
    tx: mpsc::Sender<WsMessage>,
    min_interval: Duration,
) {
    let mut changes = infra.lock().await.watch_changes();
    let mut sent: Option<Body> = None;
    let mut sent_at: Option<Instant> = None;
    loop {
        if let Some(at) = sent_at {
            tokio::time::sleep_until(at + min_interval).await;
        }
        // Changes signalled during the wait are in the results read now.
        changes.borrow_and_update();
        let message = {
            let infra = infra.lock().await;
            build(&infra.results, crate::infra::now_iso())
        };
        if sent.as_ref() != Some(&message.body) {
            match serde_json::to_string(&message) {
                Ok(text) if text.len() > MAX_BYTES => warn!(
                    bytes = text.len(),
                    "infra.state: message over the relay's limit despite the cap; not sent"
                ),
                Ok(text) => {
                    if tx.send(WsMessage::Text(text.into())).await.is_err() {
                        return;
                    }
                    debug!(
                        config_version = message.body.config_version,
                        targets = message.body.targets.len(),
                        "infra.state: sent"
                    );
                    sent = Some(message.body);
                    sent_at = Some(Instant::now());
                }
                Err(e) => warn!("infra.state: serialize failed: {e}"),
            }
        }
        // The infra state is gone with its sender: nothing more to push.
        if changes.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::infra::checks::CheckResult;
    use crate::infra::monitor::apply_result;
    use crate::infra::{CheckSpec, InfraConfig, InfraTarget, TargetState};

    fn state(id: &str, status: TargetStatus, detail: &str, name: &str) -> TargetState {
        TargetState {
            status,
            latency_ms: Some(40),
            since: "2026-09-30T11:58:00Z".into(),
            consecutive_ok: 3,
            consecutive_fail: 0,
            last_check: "2026-09-30T12:00:00Z".into(),
            detail: detail.into(),
            name: name.into(),
            http_status: id.starts_with("api").then_some(200),
            data: id.starts_with("api").then(|| json!({"ap_wan_up": 1})),
        }
    }

    fn results(targets: Vec<(String, TargetState)>) -> InfraResults {
        InfraResults {
            ts: "2026-09-30T12:00:00Z".into(),
            config_version: 7,
            targets: targets.into_iter().collect::<HashMap<_, _>>(),
            recovery_log: Vec::new(),
        }
    }

    #[test]
    fn the_message_has_the_wire_shape() {
        let results = results(vec![
            (
                "ping-1".into(),
                state("ping-1", TargetStatus::Down, "PING TIMEOUT", "Switch"),
            ),
            (
                "api-1".into(),
                state("api-1", TargetStatus::Up, "API OK", "Peplink MAX BR1"),
            ),
        ]);
        let message = build(&results, "2026-09-30T12:00:00Z".into());
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "type": "infra.state",
                "v": 1,
                "ts": "2026-09-30T12:00:00Z",
                "config_version": 7,
                "targets": {
                    "api-1": {
                        "status": "up", "latency_ms": 40, "since": "2026-09-30T11:58:00Z",
                        "consecutive_ok": 3, "consecutive_fail": 0,
                        "last_check": "2026-09-30T12:00:00Z", "detail": "API OK",
                        "name": "Peplink MAX BR1", "http_status": 200,
                    },
                    "ping-1": {
                        "status": "down", "latency_ms": 40, "since": "2026-09-30T11:58:00Z",
                        "consecutive_ok": 3, "consecutive_fail": 0,
                        "last_check": "2026-09-30T12:00:00Z", "detail": "PING TIMEOUT",
                        "name": "Switch",
                    },
                },
                "truncated": false,
            }),
            "sorted by id, no data, no recovery log, http_status only when present"
        );
    }

    #[test]
    fn a_device_that_holds_nothing_says_version_zero_and_no_targets() {
        let mut empty = results(Vec::new());
        empty.config_version = 0;
        let value = serde_json::to_value(build(&empty, String::new())).unwrap();
        assert_eq!(value["config_version"], 0);
        assert_eq!(value["targets"], json!({}));
        assert_eq!(value["truncated"], false);
    }

    #[test]
    fn a_long_detail_is_cut_to_its_width_without_calling_the_message_truncated() {
        let detail = "é".repeat(MAX_DETAIL_CHARS + 40);
        let results = results(vec![(
            "t".into(),
            state("t", TargetStatus::Up, &detail, "gw"),
        )]);
        let message = build(&results, String::new());
        let cut = message.body.targets["t"].detail.as_deref().unwrap();
        assert_eq!(cut.chars().count(), MAX_DETAIL_CHARS);
        assert!(cut.chars().all(|c| c == 'é'), "cut on a character boundary");
        assert!(!message.body.truncated);
    }

    /// `n` targets with a detail of `detail_chars` characters each.
    fn crowded(n: usize, detail_chars: usize) -> InfraResults {
        let detail = "x".repeat(detail_chars);
        results(
            (0..n)
                .map(|i| {
                    let id = format!("target-{i:04}");
                    let t = state(&id, TargetStatus::Up, &detail, &format!("Target {i}"));
                    (id, t)
                })
                .collect(),
        )
    }

    #[test]
    fn over_the_cap_the_details_go_first() {
        let results = crowded(60, MAX_DETAIL_CHARS);
        let message = build(&results, String::new());
        assert!(message.body.truncated);
        assert_eq!(message.body.targets.len(), 60, "every target kept");
        assert!(message.body.targets.values().all(|t| t.detail.is_none()));
        assert!(message.body.targets.values().all(|t| t.name.is_some()));
        assert!(serde_json::to_string(&message).unwrap().len() <= MAX_BYTES);
    }

    #[test]
    fn then_the_names_then_the_tail_of_the_list_sorted_by_id() {
        let results = crowded(300, MAX_DETAIL_CHARS);
        let message = build(&results, String::new());
        assert!(message.body.truncated);
        let bytes = serde_json::to_string(&message).unwrap().len();
        assert!(bytes <= MAX_BYTES, "{bytes} bytes");
        let kept = message.body.targets.len();
        assert!(kept > 0 && kept < 300, "{kept} targets kept");
        assert!(message.body.targets.values().all(|t| t.detail.is_none()));
        assert!(message.body.targets.values().all(|t| t.name.is_none()));
        let ids: Vec<&String> = message.body.targets.keys().collect();
        let expected: Vec<String> = (0..kept).map(|i| format!("target-{i:04}")).collect();
        assert_eq!(
            ids,
            expected.iter().collect::<Vec<_>>(),
            "the first N by id"
        );
        assert_eq!(message.body.config_version, 7);

        // The largest N that fits: one more target is over the cap.
        let mut one_more = message.clone();
        one_more.body.targets.insert(
            format!("target-{kept:04}"),
            one_more.body.targets.values().next().unwrap().clone(),
        );
        assert!(serde_json::to_string(&one_more).unwrap().len() > MAX_BYTES);
    }

    #[test]
    fn only_an_ack_that_names_the_feature_advertises_it() {
        assert!(advertised(
            &json!({"type": "tunnel.register.ack", "features": ["net.state", "infra.state"]})
        ));
        assert!(!advertised(
            &json!({"type": "tunnel.register.ack", "features": ["net.state"]})
        ));
        assert!(!advertised(
            &json!({"type": "tunnel.register.ack", "serial": "D1"})
        ));
        assert!(!advertised(&json!({"features": "infra.state"})));
        assert!(!advertised(&json!({"features": ["infra.states"]})));
    }

    /// The forwarder's interval in these tests, and how long one waits to
    /// call a forwarder silent.
    const INTERVAL: Duration = Duration::from_millis(100);
    const QUIET: Duration = Duration::from_millis(400);

    fn target(id: &str) -> InfraTarget {
        InfraTarget {
            id: id.into(),
            name: format!("Target {id}"),
            check: CheckSpec::Ping {
                host: "10.0.0.1".into(),
                timeout_ms: None,
            },
            degraded_threshold_ms: 200,
            down_after_consecutive: 3,
            up_after_consecutive: 2,
            interval_secs: None,
            recovery: None,
        }
    }

    /// An infra state holding config v7 with `targets`, seeded and not yet
    /// checked, as the device after a push.
    fn infra(targets: &[&str]) -> Arc<Mutex<InfraState>> {
        let mut state = InfraState::new("/tmp/sctl-infra-state-test");
        let config = InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets: targets.iter().map(|id| target(id)).collect(),
        };
        state.seed_results(&config);
        state.config = Some(config);
        Arc::new(Mutex::new(state))
    }

    fn ok(latency_ms: u64) -> CheckResult {
        CheckResult {
            ok: true,
            latency_ms: Some(latency_ms),
            detail: format!("PING OK {latency_ms}ms"),
            ..CheckResult::default()
        }
    }

    async fn check(infra: &Arc<Mutex<InfraState>>, id: &str, result: CheckResult) {
        let mut guard = infra.lock().await;
        apply_result(&mut guard, &target(id), result, 7, 1_000).await;
    }

    fn start(infra: Arc<Mutex<InfraState>>, tx: mpsc::Sender<WsMessage>) -> JoinHandle<()> {
        tokio::spawn(forward(infra, tx, INTERVAL))
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

    async fn silent(rx: &mut mpsc::Receiver<WsMessage>) -> bool {
        tokio::time::timeout(QUIET, rx.recv()).await.is_err()
    }

    #[tokio::test]
    async fn the_forwarder_sends_at_start_and_on_a_status_change_only() {
        let infra = infra(&["t1"]);
        let (tx, mut rx) = mpsc::channel(8);
        let task = start(infra.clone(), tx);

        let first = next_message(&mut rx).await;
        assert_eq!(first["type"], "infra.state");
        assert_eq!(first["v"], 1);
        assert_eq!(first["config_version"], 7);
        assert_eq!(first["targets"]["t1"]["status"], "unknown");
        assert_eq!(first["targets"]["t1"]["name"], "Target t1");

        // The first check: unknown becomes up.
        check(&infra, "t1", ok(3)).await;
        let up = next_message(&mut rx).await;
        assert_eq!(up["targets"]["t1"]["status"], "up");
        assert_eq!(up["targets"]["t1"]["latency_ms"], 3);
        assert_eq!(up["targets"]["t1"]["detail"], "PING OK 3ms");

        // Another good check with another latency changes nothing worth sending.
        check(&infra, "t1", ok(9)).await;
        assert!(silent(&mut rx).await);

        check(&infra, "t1", CheckResult::failed("PING TIMEOUT")).await;
        let degraded = next_message(&mut rx).await;
        assert_eq!(degraded["targets"]["t1"]["status"], "degraded");
        assert_eq!(degraded["targets"]["t1"]["consecutive_fail"], 1);
        assert_eq!(degraded["targets"]["t1"]["detail"], "PING TIMEOUT");
        task.abort();
    }

    #[tokio::test]
    async fn a_config_applied_or_removed_is_a_change() {
        let infra = infra(&["t1"]);
        let (tx, mut rx) = mpsc::channel(8);
        let task = start(infra.clone(), tx);
        next_message(&mut rx).await;

        {
            let mut guard = infra.lock().await;
            let config = InfraConfig {
                version: 8,
                check_interval_secs: 60,
                targets: vec![target("t1"), target("t2")],
            };
            guard.seed_results(&config);
            guard.config = Some(config);
        }
        let pushed = next_message(&mut rx).await;
        assert_eq!(pushed["config_version"], 8);
        assert_eq!(pushed["targets"]["t2"]["status"], "unknown");

        // The same config again says nothing new.
        {
            let mut guard = infra.lock().await;
            let config = guard.config.clone().unwrap();
            guard.seed_results(&config);
        }
        assert!(silent(&mut rx).await);

        {
            let mut guard = infra.lock().await;
            guard.config = None;
            guard.results.targets.clear();
            guard.results.config_version = 0;
            guard.changed();
        }
        let removed = next_message(&mut rx).await;
        assert_eq!(removed["config_version"], 0);
        assert_eq!(removed["targets"], json!({}));
        task.abort();
    }

    #[tokio::test]
    async fn changes_inside_the_interval_send_only_the_latest_after_it() {
        let infra = infra(&["t1", "t2"]);
        let (tx, mut rx) = mpsc::channel(8);
        let task = start(infra.clone(), tx);
        next_message(&mut rx).await;
        let first_at = Instant::now();

        check(&infra, "t1", ok(3)).await;
        tokio::task::yield_now().await;
        check(&infra, "t2", ok(4)).await;
        let latest = next_message(&mut rx).await;
        assert_eq!(latest["targets"]["t1"]["status"], "up");
        assert_eq!(
            latest["targets"]["t2"]["status"], "up",
            "the second change rode the same message"
        );
        assert!(first_at.elapsed() >= INTERVAL);
        assert!(silent(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn without_the_ack_feature_or_an_infra_state_nothing_is_sent() {
        let (tx, mut rx) = mpsc::channel(8);
        let old_relay = json!({"type": "tunnel.register.ack", "features": ["net.state"]});
        assert!(
            spawn_forwarder(advertised(&old_relay), Some(infra(&["t1"])), tx.clone()).is_none()
        );
        assert!(spawn_forwarder(true, None, tx.clone()).is_none());
        // `tx` stays open, so silence is a wait that times out.
        assert!(silent(&mut rx).await);
        drop(tx);
    }

    #[tokio::test]
    async fn with_the_ack_feature_the_forwarder_starts() {
        let (tx, mut rx) = mpsc::channel(8);
        let relay =
            json!({"type": "tunnel.register.ack", "features": ["net.state", "infra.state"]});
        let task = spawn_forwarder(advertised(&relay), Some(infra(&["t1"])), tx).unwrap();
        assert_eq!(next_message(&mut rx).await["type"], "infra.state");
        task.abort();
    }

    #[tokio::test]
    async fn the_forwarder_ends_with_the_writer() {
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let task = spawn_forwarder(true, Some(infra(&["t1"])), tx).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("ends")
            .unwrap();
    }
}
