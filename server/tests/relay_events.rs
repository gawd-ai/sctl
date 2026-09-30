//! The relay's `net.state` and `infra.state` handling and its
//! `/api/tunnel/events` stream,
//! against the real router on an ephemeral port: fake devices register over
//! WebSocket the way the tunnel client does, and fake subscribers read the
//! stream the way netage-server does.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use sctl::tunnel::relay::{relay_router, RelayState, MAX_EVENT_SUBSCRIBERS};

const KEY: &str = "tunnel-key-for-tests";
const DEVICE_KEY: &str = "device-api-key";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Relay {
    state: RelayState,
    addr: SocketAddr,
}

async fn relay() -> Relay {
    relay_with(|_| {}).await
}

async fn relay_with(tweak: impl FnOnce(&mut RelayState)) -> Relay {
    let mut state = RelayState::new(KEY.into(), 45, 60, None);
    tweak(&mut state);
    let app = relay_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Relay { state, addr }
}

async fn connect(url: String, bearer: Option<&str>) -> Result<Ws, WsError> {
    let mut request = url.into_client_request().unwrap();
    if let Some(key) = bearer {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {key}").parse().unwrap());
    }
    connect_async(request).await.map(|(ws, _)| ws)
}

fn refused_with(result: Result<Ws, WsError>) -> u16 {
    match result {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("not an HTTP refusal: {other}"),
        Ok(_) => panic!("the upgrade was accepted"),
    }
}

impl Relay {
    async fn subscribe(&self) -> Ws {
        connect(format!("ws://{}/api/tunnel/events", self.addr), Some(KEY))
            .await
            .unwrap()
    }

    /// Subscribe and read through `hello` and the replay; returns the
    /// replayed frames (without `hello` and `replay.done`).
    async fn subscribe_replayed(&self) -> (Ws, Vec<Value>) {
        let mut ws = self.subscribe().await;
        assert_eq!(next(&mut ws).await["type"], "hello");
        let replayed = until(&mut ws, "replay.done").await;
        // The bundle listing opens every replay; the device frames follow.
        assert_eq!(replayed[0]["type"], "artifacts");
        (ws, replayed.into_iter().skip(1).collect())
    }

    /// Register `serial` offering `features`; returns the socket and the ack.
    async fn device(&self, serial: &str, features: Value) -> (Ws, Value) {
        let url = format!("ws://{}/api/tunnel/register?serial={serial}", self.addr);
        let mut ws = connect(url, Some(KEY)).await.unwrap();
        let register =
            json!({"type": "tunnel.register", "api_key": DEVICE_KEY, "features": features});
        send(&mut ws, &register).await;
        let ack = next(&mut ws).await;
        (ws, ack)
    }

    /// Wait until the relay holds a net.state for `serial`'s connection.
    async fn stored(&self, serial: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let held = {
                    let devices = self.state.devices.read().await;
                    let stored = devices[serial].last_net_state.read().await;
                    stored.is_some()
                };
                if held {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the relay stores the net.state");
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split_once("\r\n\r\n").unwrap().1;
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }
}

async fn send(ws: &mut Ws, value: &Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

/// The next JSON frame, skipping control frames and the relay's heartbeat.
async fn next(ws: &mut Ws) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("a frame within 10 s")
            .expect("the socket is open")
            .expect("a readable frame");
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["type"] != "tunnel.ping" {
                return value;
            }
        }
    }
}

/// Frames up to (not including) the first of type `kind`, which is consumed.
async fn until(ws: &mut Ws, kind: &str) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let frame = next(ws).await;
        if frame["type"] == kind {
            return seen;
        }
        seen.push(frame);
    }
}

/// Wait for the relay to close `ws`.
async fn closed(ws: &mut Ws) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await
    .expect("the relay closes the socket");
}

fn net_state(eth1_ip: &str) -> Value {
    json!({
        "type": "net.state",
        "v": 1,
        "boot": 1_790_000_000_000_u64,
        "seq": 1,
        "ts": "2026-09-26T12:00:00Z",
        // Never believed: the frame names the registered serial.
        "serial": "SOMEONE-ELSE",
        "interfaces": [{"name": "eth1", "operstate": "up", "carrier": true, "metric": 10, "ip": eth1_ip}],
        "default_routes": [{"dev": "eth1", "via": "10.42.0.1", "metric": 10}],
        "relay_route": null,
        "tunnel": null,
        "wg_routes": [],
        "truncated": false,
    })
}

#[tokio::test]
async fn registration_with_features_is_acked_with_the_relays() {
    let relay = relay().await;
    let (_device, ack) = relay.device("DEV-1", json!(["net.state"])).await;
    assert_eq!(ack["type"], "tunnel.register.ack");
    assert_eq!(ack["serial"], "DEV-1");
    assert_eq!(
        ack["features"],
        json!(["net.state", "infra.state", "upgrade.state"])
    );

    // An old device offers nothing and is still acked with the relay's.
    let (_old, ack) = relay.device("DEV-OLD", Value::Null).await;
    assert_eq!(
        ack["features"],
        json!(["net.state", "infra.state", "upgrade.state"])
    );

    let (status, list) = relay.get(&format!("/api/tunnel/devices?token={KEY}")).await;
    assert_eq!(status, 200);
    let features: Vec<(&str, &Value)> = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["serial"].as_str().unwrap(), &d["features"]))
        .collect();
    assert!(features.contains(&("DEV-1", &json!(["net.state"]))));
    assert!(features.contains(&("DEV-OLD", &json!([]))));
}

#[tokio::test]
async fn offered_features_are_bounded() {
    let relay = relay().await;
    let mut offered: Vec<Value> = (0..20).map(|n| json!(format!("f{n}"))).collect();
    offered.insert(0, json!("x".repeat(33)));
    offered.insert(1, json!(7));
    offered.insert(2, json!(""));
    let (_device, _) = relay.device("DEV-1", Value::Array(offered)).await;
    let devices = relay.state.devices.read().await;
    let kept = &devices["DEV-1"].features;
    assert_eq!(kept.len(), 16);
    assert_eq!(
        kept[0], "f0",
        "too long, not a string and empty are dropped"
    );
    assert!(kept.iter().all(|f| !f.is_empty() && f.len() <= 32));
}

#[tokio::test]
async fn the_stream_says_hello_replays_then_goes_live() {
    let relay = relay().await;
    let (_b, _) = relay.device("DEV-B", json!(["net.state"])).await;
    let (_a, _) = relay.device("DEV-A", json!([])).await;

    let mut sub = relay.subscribe().await;
    let hello = next(&mut sub).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["relay_version"], sctl::VERSION);
    assert!(hello["relay_epoch"].as_u64().unwrap() > 1_700_000_000_000);
    assert_eq!(
        hello["features"],
        json!(["net.state", "infra.state", "upgrade.state"])
    );

    // The bundle listing opens every replay, before the devices.
    let listing = next(&mut sub).await;
    assert_eq!(listing["type"], "artifacts");
    assert_eq!(listing["replay"], true);
    assert_eq!(listing["versions"], json!([]));

    let a = next(&mut sub).await;
    assert_eq!(a["type"], "device.connected");
    assert_eq!(a["serial"], "DEV-A", "replayed by serial");
    assert_eq!(a["replay"], true);
    assert_eq!(a["features"], json!([]));
    assert!(a["connection_id"].is_u64());
    assert!(a["connected_at"].as_u64().unwrap() > 1_700_000_000_000);
    assert!(a.get("egress_ip").is_some());
    let b = next(&mut sub).await;
    assert_eq!(
        (b["serial"].as_str(), b["replay"].as_bool()),
        (Some("DEV-B"), Some(true))
    );
    assert_eq!(b["features"], json!(["net.state"]));
    assert_eq!(
        next(&mut sub).await,
        json!({"type": "replay.done", "devices": 2})
    );

    let (_c, _) = relay.device("DEV-C", json!(["net.state"])).await;
    let c = next(&mut sub).await;
    assert_eq!(c["type"], "device.connected");
    assert_eq!(c["serial"], "DEV-C");
    assert_eq!(c["replay"], false);
}

#[tokio::test]
async fn net_state_is_kept_published_forwarded_and_reset_on_a_new_connection() {
    let relay = relay().await;
    let (mut sub, replayed) = relay.subscribe_replayed().await;
    assert!(replayed.is_empty());

    let (mut device, _) = relay.device("DEV-1", json!(["net.state"])).await;
    let connected = next(&mut sub).await;
    assert_eq!(connected["type"], "device.connected");
    let first_connection = connected["connection_id"].as_u64().unwrap();

    // A WS client of the device gets the device's own message.
    let client_url = format!("ws://{}/d/DEV-1/api/ws?token={DEVICE_KEY}", relay.addr);
    let mut client = connect(client_url, None).await.unwrap();

    let payload = net_state("10.42.0.7/24");
    send(&mut device, &payload).await;
    let frame = next(&mut sub).await;
    assert_eq!(frame["type"], "net.state");
    assert_eq!(
        frame["serial"], "DEV-1",
        "the registered serial, not the payload's"
    );
    assert_eq!(frame["connection_id"], first_connection);
    assert_eq!(frame["replay"], false);
    assert!(frame["received_at"].as_u64().unwrap() > 1_700_000_000_000);
    assert_eq!(frame["state"], payload);
    assert_eq!(next(&mut client).await, payload);

    // Kept: a new subscriber gets it in the replay, after its device.
    let (_late, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0]["type"], "device.connected");
    assert_eq!(replayed[1]["type"], "net.state");
    assert_eq!(replayed[1]["replay"], true);
    assert_eq!(replayed[1]["state"], payload);

    // The device reconnects: the new connection starts with no net.state,
    // and the replaced one is not announced as a disconnect.
    let (mut again, _) = relay.device("DEV-1", json!(["net.state"])).await;
    let reconnected = next(&mut sub).await;
    assert_eq!(reconnected["type"], "device.connected");
    let second_connection = reconnected["connection_id"].as_u64().unwrap();
    assert_ne!(second_connection, first_connection);
    closed(&mut device).await;
    let (_fresh, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed.len(), 1, "no net.state carried over: {replayed:?}");
    assert_eq!(replayed[0]["connection_id"], second_connection);

    let payload = net_state("10.42.0.8/24");
    send(&mut again, &payload).await;
    let frame = next(&mut sub).await;
    assert_eq!(
        frame["type"], "net.state",
        "no device.disconnected for the replaced connection"
    );
    assert_eq!(frame["connection_id"], second_connection);
    assert_eq!(frame["state"], payload);
}

fn infra_state(status: &str) -> Value {
    json!({
        "type": "infra.state",
        "v": 1,
        "ts": "2026-09-30T12:00:00Z",
        // Never believed: the frame names the registered serial.
        "serial": "SOMEONE-ELSE",
        "config_version": 7,
        "targets": {
            "t1": {
                "status": status, "latency_ms": 3, "since": "2026-09-30T11:58:00Z",
                "consecutive_ok": 1, "consecutive_fail": 0,
                "last_check": "2026-09-30T12:00:00Z", "detail": "PING OK 3ms", "name": "Gateway",
            },
        },
        "truncated": false,
    })
}

#[tokio::test]
async fn infra_state_is_kept_published_forwarded_and_replayed_after_net_state() {
    let relay = relay().await;
    let (mut sub, replayed) = relay.subscribe_replayed().await;
    assert!(replayed.is_empty());

    let (mut device, ack) = relay
        .device(
            "DEV-1",
            json!(["net.state", "infra.state", "upgrade.state"]),
        )
        .await;
    assert_eq!(
        ack["features"],
        json!(["net.state", "infra.state", "upgrade.state"])
    );
    let connected = next(&mut sub).await;
    assert_eq!(connected["type"], "device.connected");
    let first_connection = connected["connection_id"].as_u64().unwrap();

    // A WS client of the device gets the device's own message.
    let client_url = format!("ws://{}/d/DEV-1/api/ws?token={DEVICE_KEY}", relay.addr);
    let mut client = connect(client_url, None).await.unwrap();

    let payload = infra_state("up");
    send(&mut device, &payload).await;
    let frame = next(&mut sub).await;
    assert_eq!(frame["type"], "infra.state");
    assert_eq!(
        frame["serial"], "DEV-1",
        "the registered serial, not the payload's"
    );
    assert_eq!(frame["connection_id"], first_connection);
    assert_eq!(frame["replay"], false);
    assert!(frame["received_at"].as_u64().unwrap() > 1_700_000_000_000);
    assert_eq!(frame["state"], payload);
    assert_eq!(next(&mut client).await, payload);

    // Kept, and replayed after the connection's net.state.
    let network = net_state("10.42.0.7/24");
    send(&mut device, &network).await;
    assert_eq!(next(&mut sub).await["type"], "net.state");
    let (_late, replayed) = relay.subscribe_replayed().await;
    let kinds: Vec<&str> = replayed
        .iter()
        .map(|f| f["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["device.connected", "net.state", "infra.state"]);
    assert_eq!(replayed[1]["state"], network);
    assert_eq!(replayed[2]["replay"], true);
    assert_eq!(replayed[2]["connection_id"], first_connection);
    assert_eq!(replayed[2]["state"], payload);

    // A newer message replaces it; the device reconnects and the new
    // connection starts with none.
    let payload = infra_state("down");
    send(&mut device, &payload).await;
    assert_eq!(next(&mut sub).await["state"], payload);
    let (_late, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed[2]["state"]["targets"]["t1"]["status"], "down");

    let (_again, _) = relay.device("DEV-1", json!(["infra.state"])).await;
    let reconnected = next(&mut sub).await;
    assert_eq!(reconnected["type"], "device.connected");
    assert_ne!(reconnected["connection_id"], first_connection);
    closed(&mut device).await;
    let (_fresh, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed.len(), 1, "nothing carried over: {replayed:?}");
}

#[tokio::test]
async fn an_oversize_or_unparsable_infra_state_is_dropped() {
    let relay = relay().await;
    let (mut device, _) = relay.device("DEV-1", json!(["infra.state"])).await;
    let (mut sub, _) = relay.subscribe_replayed().await;

    let mut oversize = infra_state("up");
    oversize["padding"] = json!("x".repeat(16 * 1024));
    send(&mut device, &oversize).await;
    device
        .send(Message::Text("{\"type\": \"infra.state\", not json".into()))
        .await
        .unwrap();
    let small = infra_state("degraded");
    send(&mut device, &small).await;

    let frame = next(&mut sub).await;
    assert_eq!(frame["type"], "infra.state");
    assert_eq!(
        frame["state"], small,
        "neither the oversize nor the broken one reached the stream"
    );
    let (_late, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[1]["state"], small);
}

#[tokio::test]
async fn an_oversize_net_state_is_dropped() {
    let relay = relay().await;
    let (mut device, _) = relay.device("DEV-1", json!(["net.state"])).await;
    let (mut sub, _) = relay.subscribe_replayed().await;

    let mut oversize = net_state("10.42.0.7/24");
    oversize["padding"] = json!("x".repeat(16 * 1024));
    send(&mut device, &oversize).await;
    let small = net_state("10.42.0.9/24");
    send(&mut device, &small).await;

    let frame = next(&mut sub).await;
    assert_eq!(frame["type"], "net.state");
    assert_eq!(
        frame["state"], small,
        "the oversize one never reached the stream"
    );
    let (_late, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed[1]["state"], small);
}

#[tokio::test]
async fn departures_are_announced_on_close_and_on_the_sweep() {
    let relay = relay().await;
    let (mut sub, _) = relay.subscribe_replayed().await;

    let (mut a, _) = relay.device("DEV-A", json!([])).await;
    let a_connection = next(&mut sub).await["connection_id"].clone();
    a.close(None).await.unwrap();
    assert_eq!(
        next(&mut sub).await,
        json!({
            "type": "device.disconnected",
            "serial": "DEV-A",
            "connection_id": a_connection,
            "reason": "ws_close",
            "replay": false,
        })
    );

    let (mut b, _) = relay.device("DEV-B", json!([])).await;
    let b_connection = next(&mut sub).await["connection_id"].clone();
    // The heartbeat sweep with a zero timeout evicts everything.
    let mut sweeper = relay.state.clone();
    sweeper.heartbeat_timeout_secs = 0;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        sweeper.sweep_dead_devices().await,
        vec!["DEV-B".to_string()]
    );
    let gone = next(&mut sub).await;
    assert_eq!(gone["type"], "device.disconnected");
    assert_eq!(gone["connection_id"], b_connection);
    assert_eq!(gone["reason"], "heartbeat_timeout");

    // B's own handler ending later announces nothing more.
    b.close(None).await.unwrap();
    closed(&mut b).await;
    let (_c, _) = relay.device("DEV-C", json!([])).await;
    let marker = next(&mut sub).await;
    assert_eq!(marker["type"], "device.connected");
    assert_eq!(marker["serial"], "DEV-C");
}

#[tokio::test]
async fn no_net_state_follows_its_connections_departure() {
    let relay = relay().await;
    let (mut device, _) = relay.device("DEV-1", json!(["net.state"])).await;
    let (mut sub, _) = relay.subscribe_replayed().await;

    // Hold the connection's net.state slot: the handler takes the report,
    // finds its connection current, and waits here to store it.
    let slot = relay.state.devices.read().await["DEV-1"]
        .last_net_state
        .clone();
    let held = slot.write().await;
    send(&mut device, &net_state("10.42.0.7/24")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Meanwhile the sweep evicts the connection.
    let mut sweeper = relay.state.clone();
    sweeper.heartbeat_timeout_secs = 0;
    let sweep = tokio::spawn(async move { sweeper.sweep_dead_devices().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    assert_eq!(sweep.await.unwrap(), vec!["DEV-1".to_string()]);
    tokio::time::timeout(Duration::from_secs(5), async {
        while slot.read().await.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the handler stores the report it took");

    relay.state.publish(&json!({"type": "test.marker"}));
    let frames = until(&mut sub, "test.marker").await;
    let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["net.state", "device.disconnected"],
        "a report is never published after its connection's departure: {frames:?}"
    );
}

#[tokio::test]
async fn relay_shutdown_announces_every_departure() {
    let relay = relay().await;
    let (_a, _) = relay.device("DEV-A", json!([])).await;
    let (mut sub, _) = relay.subscribe_replayed().await;
    relay.state.drain_all().await;
    let gone = next(&mut sub).await;
    assert_eq!(gone["type"], "device.disconnected");
    assert_eq!(gone["serial"], "DEV-A");
    assert_eq!(gone["reason"], "relay_shutdown");
}

#[tokio::test]
async fn only_the_tunnel_key_as_a_bearer_opens_the_stream() {
    let relay = relay().await;
    let url = format!("ws://{}/api/tunnel/events", relay.addr);
    assert_eq!(
        refused_with(connect(url.clone(), Some("wrong-key")).await),
        403
    );
    assert_eq!(refused_with(connect(url.clone(), None).await), 403);
    assert_eq!(refused_with(connect(url.clone(), Some("")).await), 403);
    // The key in the query is never read.
    let query = format!("{url}?token={KEY}");
    assert_eq!(refused_with(connect(query, None).await), 403);
    assert!(connect(url, Some(KEY)).await.is_ok());
}

#[tokio::test]
async fn the_ninth_subscriber_is_refused_until_one_leaves() {
    let relay = relay().await;
    let mut subscribers = Vec::new();
    for _ in 0..MAX_EVENT_SUBSCRIBERS {
        let (ws, _) = relay.subscribe_replayed().await;
        subscribers.push(ws);
    }
    let url = format!("ws://{}/api/tunnel/events", relay.addr);
    assert_eq!(refused_with(connect(url.clone(), Some(KEY)).await), 429);

    let mut leaving = subscribers.pop().unwrap();
    leaving.close(None).await.unwrap();
    closed(&mut leaving).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match connect(url.clone(), Some(KEY)).await {
            Ok(_) => break,
            Err(WsError::Http(r))
                if r.status() == 429 && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("the freed slot was not given back: {e}"),
        }
    }
}

#[tokio::test]
async fn a_subscriber_that_falls_behind_is_resynced_with_a_fresh_replay() {
    let relay = relay_with(|state| {
        state.events = tokio::sync::broadcast::channel(2).0;
    })
    .await;
    let (mut device, _) = relay.device("DEV-1", json!(["net.state"])).await;
    let payload = net_state("10.42.0.7/24");
    send(&mut device, &payload).await;
    relay.stored("DEV-1").await;
    let (mut sub, replayed) = relay.subscribe_replayed().await;
    assert_eq!(replayed[1]["state"], payload);

    // Far more than the bus holds, published before the subscriber runs.
    for n in 0..50 {
        relay.state.publish(&json!({"type": "test.flood", "n": n}));
    }
    assert_eq!(next(&mut sub).await, json!({"type": "resync"}));
    let replayed = until(&mut sub, "replay.done").await;
    assert_eq!(replayed.len(), 3);
    assert_eq!(replayed[0]["type"], "artifacts");
    assert_eq!(replayed[1]["type"], "device.connected");
    assert_eq!(replayed[1]["replay"], true);
    assert_eq!(replayed[2]["state"], payload);

    // And it is live again.
    let (_other, _) = relay.device("DEV-2", json!([])).await;
    let live = next(&mut sub).await;
    assert_eq!(
        (live["type"].as_str(), live["serial"].as_str()),
        (Some("device.connected"), Some("DEV-2"))
    );
    assert_eq!(live["replay"], false);
}

#[tokio::test]
async fn the_bus_is_shared_by_every_clone_of_the_state() {
    // main.rs builds the router from one clone and sweeps from another.
    let relay = relay().await;
    let (mut sub, _) = relay.subscribe_replayed().await;
    let other = relay.state.clone();
    other.publish(&json!({"type": "test.marker"}));
    assert_eq!(next(&mut sub).await["type"], "test.marker");
}

// ─── Artifacts: bundles the relay serves to its devices ─────────────────────

const OPERATOR_KEY: &str = "operator-key-for-tests";

/// A raw HTTP/1.1 request with a bearer and a body; returns the status, the
/// headers (lowercased names) and the body bytes.
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    extra: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n");
    if let Some(key) = bearer {
        let _ = write!(request, "Authorization: Bearer {key}\r\n");
    }
    for (name, value) in extra {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    let _ = write!(request, "Content-Length: {}\r\n\r\n", body.len());
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let status: u16 = head[9..12].parse().unwrap();
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect();
    (status, headers, response[split + 4..].to_vec())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

struct Signer(ring::signature::Ed25519KeyPair);

impl Signer {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Self(ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
    }

    fn public_key(&self) -> [u8; 32] {
        use ring::signature::KeyPair;
        let mut key = [0u8; 32];
        key.copy_from_slice(self.0.public_key().as_ref());
        key
    }

    fn sign(&self, bytes: &[u8]) -> String {
        use base64::Engine as _;
        let id = sctl::upgrade::keys::key_id(&self.public_key());
        let sig = base64::engine::general_purpose::STANDARD.encode(self.0.sign(bytes).as_ref());
        format!("ed25519:{id}:{sig}")
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sctl::upgrade::hex_lower(&sha2::Sha256::digest(bytes))
}

#[tokio::test]
async fn bundles_are_verified_at_the_door_served_with_ranges_and_announced() {
    let dir = std::env::temp_dir().join(format!("sctl-relay-artifacts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let signer = Signer::new();
    let mut state = RelayState::new(KEY.into(), 45, 60, Some(dir.to_str().unwrap()));
    state.set_operator(OPERATOR_KEY.into(), vec![signer.public_key()]);
    let app = relay_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let relay = Relay { state, addr };

    let artifact = b"hello, agent".to_vec();
    let manifest = json!({
        "v": 1, "version": "0.6.8.1", "channel": "stable", "min_from_version": "0.6.7.0",
        "published_at": "2026-10-01T00:00:00Z", "notes": "test release",
        "targets": {"x86_64": {"files": [
            {"role": "server", "name": "sctl-x86_64", "size": artifact.len(), "sha256": sha256_hex(&artifact), "gzip": false}
        ]}}
    })
    .to_string();
    let sig = signer.sign(manifest.as_bytes());
    let bundle = "/api/tunnel/artifacts/0.6.8.1";

    // A subscriber sees an empty listing first, then every change.
    let (mut sub, _) = relay.subscribe_replayed().await;

    // Files before the manifest, the tunnel key, and no key are refused.
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/sctl-x86_64"),
        Some(OPERATOR_KEY),
        &[],
        &artifact,
    )
    .await;
    assert_eq!(status, 409);
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/release.json"),
        Some(KEY),
        &[],
        manifest.as_bytes(),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/release.json"),
        None,
        &[],
        manifest.as_bytes(),
    )
    .await;
    assert_eq!(status, 403);

    // The manifest and its signature, in either order, make a verified bundle.
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/release.json"),
        Some(OPERATOR_KEY),
        &[],
        manifest.as_bytes(),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, body) = http(
        addr,
        "PUT",
        &format!("{bundle}/release.json.sig"),
        Some(OPERATOR_KEY),
        &[],
        sig.as_bytes(),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let stored: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(stored["bundle"]["verified"], true);
    assert_eq!(stored["bundle"]["complete"], false);
    assert_eq!(stored["bundle"]["missing"], json!(["sctl-x86_64"]));
    let changed = next(&mut sub).await;
    assert_eq!(changed["type"], "artifacts.changed");

    // A signature by another key is refused and the bundle stays as it was.
    let other = Signer::new().sign(manifest.as_bytes());
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/release.json.sig"),
        Some(OPERATOR_KEY),
        &[],
        other.as_bytes(),
    )
    .await;
    assert_eq!(status, 422);

    // A file that is not the manifest's is refused; the right one lands.
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/sctl-x86_64"),
        Some(OPERATOR_KEY),
        &[],
        b"tampered",
    )
    .await;
    assert_eq!(status, 422);
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/other-file"),
        Some(OPERATOR_KEY),
        &[],
        b"x",
    )
    .await;
    assert_eq!(status, 422);
    let (status, _, body) = http(
        addr,
        "PUT",
        &format!("{bundle}/sctl-x86_64"),
        Some(OPERATOR_KEY),
        &[],
        &artifact,
    )
    .await;
    assert_eq!(status, 200);
    let stored: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(stored["bundle"]["complete"], true);

    // Devices read with the tunnel key, with ranges; nobody reads without a key.
    let (status, headers, body) = http(
        addr,
        "GET",
        &format!("{bundle}/sctl-x86_64"),
        Some(KEY),
        &[],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, artifact);
    assert_eq!(header(&headers, "accept-ranges"), Some("bytes"));
    assert_eq!(
        header(&headers, "etag"),
        Some(format!("\"{}\"", sha256_hex(&artifact)).as_str())
    );
    let (status, headers, body) = http(
        addr,
        "GET",
        &format!("{bundle}/sctl-x86_64"),
        Some(KEY),
        &[("Range", "bytes=7-")],
        b"",
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(body, b"agent");
    assert_eq!(header(&headers, "content-range"), Some("bytes 7-11/12"));
    let (status, _, _) = http(
        addr,
        "GET",
        &format!("{bundle}/sctl-x86_64"),
        Some(KEY),
        &[("Range", "bytes=12-")],
        b"",
    )
    .await;
    assert_eq!(status, 416);

    // A resume names the file it started with; another file is served whole.
    let etag = format!("\"{}\"", sha256_hex(&artifact));
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("{bundle}/sctl-x86_64"),
        Some(KEY),
        &[("Range", "bytes=7-"), ("If-Range", &etag)],
        b"",
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(body, b"agent");
    let (status, _, body) = http(
        addr,
        "GET",
        &format!("{bundle}/sctl-x86_64"),
        Some(KEY),
        &[("Range", "bytes=7-"), ("If-Range", "\"other\"")],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, artifact);
    let (status, _, _) = http(
        addr,
        "GET",
        &format!("{bundle}/sctl-x86_64"),
        None,
        &[],
        b"",
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = http(addr, "GET", &format!("{bundle}/nope"), Some(KEY), &[], b"").await;
    assert_eq!(status, 404);

    // The listing, on the route and at the head of every replay.
    let (status, _, body) = http(addr, "GET", "/api/tunnel/artifacts", Some(KEY), &[], b"").await;
    assert_eq!(status, 200);
    let listing: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listing["versions"][0]["version"], "0.6.8.1");
    assert_eq!(listing["versions"][0]["targets"], json!(["x86_64"]));
    assert_eq!(listing["versions"][0]["complete"], true);
    let mut late = relay.subscribe().await;
    assert_eq!(next(&mut late).await["type"], "hello");
    let head = next(&mut late).await;
    assert_eq!(head["type"], "artifacts");
    assert_eq!(head["replay"], true);
    assert_eq!(head["versions"][0]["version"], "0.6.8.1");

    // A mirror for ramboot devices, then the bundle is removed.
    let (status, _, _) = http(
        addr,
        "PUT",
        &format!("{bundle}/mirror"),
        Some(OPERATOR_KEY),
        &[("Content-Type", "application/json")],
        br#"{"url":"http://mirror/0.6.8.1"}"#,
    )
    .await;
    assert_eq!(status, 200);
    let (_, _, body) = http(addr, "GET", "/api/tunnel/artifacts", Some(KEY), &[], b"").await;
    let listing: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listing["versions"][0]["mirror"], "http://mirror/0.6.8.1");
    let (status, _, _) = http(addr, "DELETE", bundle, Some(OPERATOR_KEY), &[], b"").await;
    assert_eq!(status, 200);
    let (_, _, body) = http(addr, "GET", "/api/tunnel/artifacts", Some(KEY), &[], b"").await;
    let listing: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listing["versions"], json!([]));
    assert!(!dir.join("0.6.8.1").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_device_says_what_it_is_and_its_upgrade_state_is_kept_and_replayed() {
    let relay = relay().await;
    let (mut sub, _) = relay.subscribe_replayed().await;
    let url = format!("ws://{}/api/tunnel/register?serial=DEV-7", relay.addr);
    let mut device = connect(url, Some(KEY)).await.unwrap();
    send(
        &mut device,
        &json!({"type": "tunnel.register", "api_key": DEVICE_KEY, "features": ["upgrade.state"],
                "version": "0.6.7.431", "target": "mips_24kc", "layout": "gz-tmp"}),
    )
    .await;
    let ack = next(&mut device).await;
    assert_eq!(ack["type"], "tunnel.register.ack");
    let connected = next(&mut sub).await;
    assert_eq!(connected["type"], "device.connected");
    assert_eq!(connected["version"], "0.6.7.431");
    assert_eq!(connected["target"], "mips_24kc");
    assert_eq!(connected["layout"], "gz-tmp");
    let (_, list) = relay.get(&format!("/api/tunnel/devices?token={KEY}")).await;
    assert_eq!(list["devices"][0]["version"], "0.6.7.431");
    assert_eq!(list["devices"][0]["layout"], "gz-tmp");

    let state = json!({"type": "upgrade.state", "v": 1, "ts": "2026-10-02T06:12:00Z",
        "running_version": "0.6.7.431", "target": "mips_24kc", "layout": "gz-tmp",
        "request_id": null, "phase": "idle", "outcome": null});
    send(&mut device, &state).await;
    let pushed = next(&mut sub).await;
    assert_eq!(pushed["type"], "upgrade.state");
    assert_eq!(pushed["serial"], "DEV-7");
    assert_eq!(pushed["state"], state);
    let (_, replayed) = relay.subscribe_replayed().await;
    let kinds: Vec<&str> = replayed
        .iter()
        .map(|f| f["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["device.connected", "upgrade.state"]);
    assert_eq!(replayed[1]["state"], state);
}
