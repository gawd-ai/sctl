//! `upgrade.state`: where the device's upgrade stands, pushed to the relay.
//!
//! Offered in `tunnel.register` and sent only when the relay's ack
//! advertises it: once right after the ack (so the relay has the device's
//! state even when nothing is in flight), then on every change, at most one
//! a second. The relay keeps the latest per connection, publishes it on
//! `/api/tunnel/events` and replays it. `docs/upgrade.md` has the fields.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::upgrade::restart::RestartState;
use crate::upgrade::state::UpgradeState;

/// The feature name in `tunnel.register` and `tunnel.register.ack`.
pub const FEATURE: &str = "upgrade.state";
/// The largest message the relay accepts.
pub const MAX_BYTES: usize = crate::upgrade::state::MAX_BYTES;
/// At most one message per this interval on a connection.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// One `upgrade.state` message: the state plus who is sending it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpgradeStateMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The version of the agent sending this: how a collector tells "rolled
    /// back and the old one is talking" from "upgraded and the new one is".
    pub running_version: &'static str,
    pub target: &'static str,
    pub layout: Option<&'static str>,
    #[serde(flatten)]
    pub state: UpgradeState,
    /// The agent's own restart (ADR-014): its record, idle when none was
    /// asked. A collector knows by this key that the agent has the route.
    pub restart: RestartState,
}

/// Whether the relay's `tunnel.register.ack` advertises `upgrade.state`.
pub fn advertised(ack: &Value) -> bool {
    ack["features"]
        .as_array()
        .is_some_and(|features| features.iter().any(|f| f.as_str() == Some(FEATURE)))
}

/// Build the message for `state` and the restart's record.
pub fn build(
    state: UpgradeState,
    restart: RestartState,
    layout: Option<&'static str>,
) -> UpgradeStateMessage {
    UpgradeStateMessage {
        kind: FEATURE,
        running_version: crate::VERSION,
        target: crate::upgrade::TARGET,
        layout,
        state,
        restart,
    }
}

/// The message as text, under the relay's limit when dropping what grows
/// brings it there: the restart's log tail first, then the upgrade's.
/// `None` when it is still over.
fn text_under_the_cap(message: &UpgradeStateMessage) -> Option<String> {
    let mut message = message.clone();
    for trim in 0..3 {
        match trim {
            1 => message.restart.log_tail = None,
            2 => message.state.log_tail = None,
            _ => {}
        }
        let text = serde_json::to_string(&message).ok()?;
        if text.len() <= MAX_BYTES {
            return Some(text);
        }
    }
    None
}

/// Start this connection's forwarder, unless the relay did not advertise
/// the feature. The caller aborts the task when the connection ends.
pub fn spawn_forwarder(
    relay_takes_it: bool,
    rx: watch::Receiver<UpgradeState>,
    restart_rx: watch::Receiver<RestartState>,
    layout: Option<&'static str>,
    tx: mpsc::Sender<WsMessage>,
) -> Option<JoinHandle<()>> {
    if !relay_takes_it {
        return None;
    }
    Some(tokio::spawn(forward(
        rx,
        restart_rx,
        layout,
        tx,
        MIN_INTERVAL,
    )))
}

async fn forward(
    mut rx: watch::Receiver<UpgradeState>,
    mut restart_rx: watch::Receiver<RestartState>,
    layout: Option<&'static str>,
    tx: mpsc::Sender<WsMessage>,
    min_interval: Duration,
) {
    let mut sent_at: Option<Instant> = None;
    loop {
        if let Some(at) = sent_at {
            tokio::time::sleep_until(at + min_interval).await;
        }
        let state = rx.borrow_and_update().clone();
        let restart = restart_rx.borrow_and_update().clone();
        let message = build(state, restart, layout);
        if let Some(text) = text_under_the_cap(&message) {
            if tx.send(WsMessage::Text(text.into())).await.is_err() {
                return;
            }
            debug!(phase = ?message.state.phase, restart = ?message.restart.phase, "upgrade.state: sent");
            sent_at = Some(Instant::now());
        } else {
            warn!("upgrade.state: message over the relay's limit; not sent");
        }
        // The upgrade's state or the restart's record: either change sends.
        tokio::select! {
            changed = rx.changed() => if changed.is_err() { return; },
            changed = restart_rx.changed() => if changed.is_err() { return; },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::state::{Outcome, Phase};
    use serde_json::json;

    #[test]
    fn the_message_carries_the_sender_and_the_state() {
        let state = UpgradeState::staging(Some("r-1".into()), "0.6.7.1", "0.6.8.2");
        let message = build(state, RestartState::idle(), Some("gz-tmp"));
        let json = serde_json::to_value(&message).unwrap();
        // The restart's record rides every message, idle when none was asked.
        assert_eq!(json["restart"], json!({"v": 1, "phase": "idle"}));
        assert_eq!(json["type"], "upgrade.state");
        assert_eq!(json["running_version"], crate::VERSION);
        assert_eq!(json["target"], crate::upgrade::TARGET);
        assert_eq!(json["layout"], "gz-tmp");
        assert_eq!(json["phase"], "staging");
        assert_eq!(json["request_id"], "r-1");
        assert_eq!(json["to_version"], "0.6.8.2");
        assert!(json["outcome"].is_null());
    }

    #[test]
    fn advertised_reads_the_ack() {
        assert!(advertised(
            &json!({"features": ["net.state", "upgrade.state"]})
        ));
        assert!(!advertised(&json!({"features": ["net.state"]})));
        assert!(!advertised(&json!({})));
    }

    async fn next_message(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a message")
            .expect("open");
        match message {
            WsMessage::Text(t) => serde_json::from_str(&t).unwrap(),
            other => panic!("not text: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_forwarder_sends_now_and_on_change() {
        let (state_tx, state_rx) = watch::channel(UpgradeState::idle());
        let (restart_tx, restart_rx) = watch::channel(RestartState::idle());
        let (tx, mut rx) = mpsc::channel(8);
        let task = spawn_forwarder(true, state_rx, restart_rx, None, tx).unwrap();
        let first = next_message(&mut rx).await;
        assert_eq!(first["phase"], "idle");
        state_tx
            .send_replace(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").finished(Outcome::Ok));
        let second = next_message(&mut rx).await;
        assert_eq!(second["phase"], "done");
        assert_eq!(second["outcome"], "ok");
        assert_eq!(second["restart"]["phase"], "idle");
        // A change of the restart's record alone sends one message.
        let me = crate::upgrade::restart::Process {
            instance: "an-instance",
            boot: 1_790_000_000_000,
        };
        restart_tx.send_replace(RestartState::accepted(Some("r-41".into()), &me));
        let third = next_message(&mut rx).await;
        assert_eq!(third["phase"], "done", "the upgrade's state as it was");
        assert_eq!(third["restart"]["phase"], "accepted");
        assert_eq!(third["restart"]["request_id"], "r-41");
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), rx.recv())
                .await
                .is_err(),
            "one message for one change"
        );
        task.abort();
        assert!(spawn_forwarder(
            false,
            state_tx.subscribe(),
            restart_tx.subscribe(),
            None,
            mpsc::channel(1).0
        )
        .is_none());
    }

    #[test]
    fn over_the_cap_the_restarts_log_tail_goes_first() {
        let me = crate::upgrade::restart::Process {
            instance: "an-instance",
            boot: 1_790_000_000_000,
        };
        let mut state =
            UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").finished(Outcome::RolledBack);
        state.log_tail = Some("u".repeat(4500));
        let restart = RestartState::accepted(None, &me)
            .restarting()
            .finished(crate::upgrade::restart::Outcome::NotHealthy, None)
            .with_log_tail(Some("r".repeat(4500)));
        let whole = build(state.clone(), restart.clone(), None);
        assert!(serde_json::to_string(&whole).unwrap().len() > MAX_BYTES);
        let sent: Value = serde_json::from_str(&text_under_the_cap(&whole).unwrap()).unwrap();
        assert!(sent["restart"].get("log_tail").is_none(), "it went first");
        assert_eq!(sent["restart"]["outcome"], "not_healthy");
        assert_eq!(
            sent["log_tail"].as_str().unwrap().len(),
            4500,
            "the upgrade's stays"
        );
        // Still over without it: the upgrade's goes too.
        state.log_tail = Some("u".repeat(MAX_BYTES));
        let sent: Value =
            serde_json::from_str(&text_under_the_cap(&build(state, restart, None)).unwrap())
                .unwrap();
        assert!(sent["log_tail"].is_null());
    }

    #[test]
    fn a_state_with_a_huge_log_tail_is_still_under_the_cap_without_it() {
        let mut state =
            UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").finished(Outcome::RolledBack);
        state.log_tail = Some("x".repeat(crate::upgrade::state::LOG_TAIL_BYTES));
        assert_eq!(state.phase, Phase::Done);
        let text =
            serde_json::to_string(&build(state, RestartState::idle(), Some("usr-bin"))).unwrap();
        assert!(text.len() <= MAX_BYTES);
    }
}
