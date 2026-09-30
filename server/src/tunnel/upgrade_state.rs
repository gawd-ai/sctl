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
}

/// Whether the relay's `tunnel.register.ack` advertises `upgrade.state`.
pub fn advertised(ack: &Value) -> bool {
    ack["features"]
        .as_array()
        .is_some_and(|features| features.iter().any(|f| f.as_str() == Some(FEATURE)))
}

/// Build the message for `state`.
pub fn build(state: UpgradeState, layout: Option<&'static str>) -> UpgradeStateMessage {
    UpgradeStateMessage {
        kind: FEATURE,
        running_version: crate::VERSION,
        target: crate::upgrade::TARGET,
        layout,
        state,
    }
}

/// Start this connection's forwarder, unless the relay did not advertise
/// the feature. The caller aborts the task when the connection ends.
pub fn spawn_forwarder(
    relay_takes_it: bool,
    rx: watch::Receiver<UpgradeState>,
    layout: Option<&'static str>,
    tx: mpsc::Sender<WsMessage>,
) -> Option<JoinHandle<()>> {
    if !relay_takes_it {
        return None;
    }
    Some(tokio::spawn(forward(rx, layout, tx, MIN_INTERVAL)))
}

async fn forward(
    mut rx: watch::Receiver<UpgradeState>,
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
        let message = build(state, layout);
        match serde_json::to_string(&message) {
            Ok(mut text) => {
                if text.len() > MAX_BYTES {
                    // The log tail is the only part that grows: drop it.
                    let trimmed = build(
                        UpgradeState {
                            log_tail: None,
                            ..message.state.clone()
                        },
                        layout,
                    );
                    text = serde_json::to_string(&trimmed).unwrap_or_default();
                }
                if text.len() > MAX_BYTES {
                    warn!(
                        bytes = text.len(),
                        "upgrade.state: message over the relay's limit; not sent"
                    );
                } else if tx.send(WsMessage::Text(text.into())).await.is_err() {
                    return;
                } else {
                    debug!(phase = ?message.state.phase, "upgrade.state: sent");
                    sent_at = Some(Instant::now());
                }
            }
            Err(e) => warn!("upgrade.state: serialize failed: {e}"),
        }
        if rx.changed().await.is_err() {
            return;
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
        let message = build(state, Some("gz-tmp"));
        let json = serde_json::to_value(&message).unwrap();
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
        let (tx, mut rx) = mpsc::channel(8);
        let task = spawn_forwarder(true, state_rx, None, tx).unwrap();
        let first = next_message(&mut rx).await;
        assert_eq!(first["phase"], "idle");
        state_tx
            .send_replace(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").finished(Outcome::Ok));
        let second = next_message(&mut rx).await;
        assert_eq!(second["phase"], "done");
        assert_eq!(second["outcome"], "ok");
        task.abort();
        assert!(spawn_forwarder(false, state_tx.subscribe(), None, mpsc::channel(1).0).is_none());
    }

    #[test]
    fn a_state_with_a_huge_log_tail_is_still_under_the_cap_without_it() {
        let mut state =
            UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").finished(Outcome::RolledBack);
        state.log_tail = Some("x".repeat(crate::upgrade::state::LOG_TAIL_BYTES));
        assert_eq!(state.phase, Phase::Done);
        let text = serde_json::to_string(&build(state, Some("usr-bin"))).unwrap();
        assert!(text.len() <= MAX_BYTES);
    }
}
