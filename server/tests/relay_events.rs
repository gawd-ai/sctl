//! The relay's `net.state` handling and its `/api/tunnel/events` stream,
//! against the real router on an ephemeral port: fake devices register over
//! WebSocket the way the tunnel client does, and fake subscribers read the
//! stream the way netage-server does.

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
        (ws, replayed)
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
    assert_eq!(ack["features"], json!(["net.state"]));

    // An old device offers nothing and is still acked with the relay's.
    let (_old, ack) = relay.device("DEV-OLD", Value::Null).await;
    assert_eq!(ack["features"], json!(["net.state"]));

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
    assert_eq!(hello["features"], json!(["net.state"]));

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
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0]["type"], "device.connected");
    assert_eq!(replayed[0]["replay"], true);
    assert_eq!(replayed[1]["state"], payload);

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
