//! A test SNMP agent (TRD-9 3.4) for the net tools' rehearsals.
//!
//! It answers each GET, GETNEXT or SET with the request itself turned into
//! a GetResponse, after a delay the test sets, and keeps every request it
//! got with its community. TRD-6's `echo_agent` is a private helper bound
//! to 127.0.0.1 that stops after 500 ms idle; this one binds the address
//! the test gives, on a thread the test prepares first (entering a network
//! namespace), and runs until it is dropped.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// One request the agent got.
#[derive(Clone, Debug)]
pub struct Request {
    pub from: SocketAddr,
    /// The PDU's tag: 0xA0 GET, 0xA1 GETNEXT, 0xA3 SET.
    pub kind: u8,
    pub community: Vec<u8>,
}

/// A running agent; dropping it stops it.
pub struct Agent {
    requests: Arc<Mutex<Vec<Request>>>,
    delay_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Agent {
    /// Bind `addr` on a thread that first runs `prepare`, and answer every
    /// request `delay` after it arrives.
    pub fn start(
        addr: SocketAddr,
        delay: Duration,
        prepare: impl FnOnce() + Send + 'static,
    ) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let delay_ms = Arc::new(AtomicU64::new(delay.as_millis() as u64));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = std::sync::mpsc::channel();
        let handle = {
            let (requests, delay_ms, stop) = (requests.clone(), delay_ms.clone(), stop.clone());
            std::thread::spawn(move || {
                prepare();
                let socket = UdpSocket::bind(addr).expect("the SNMP agent binds");
                socket
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                ready.send(()).unwrap();
                let mut buf = [0u8; 4096];
                while !stop.load(Ordering::SeqCst) {
                    let Ok((n, from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let bytes = buf[..n].to_vec();
                    let Some((kind, community)) = request_of(&bytes) else {
                        continue;
                    };
                    requests.lock().unwrap().push(Request {
                        from,
                        kind,
                        community,
                    });
                    std::thread::sleep(Duration::from_millis(delay_ms.load(Ordering::SeqCst)));
                    if let Some(reply) = response_to(&bytes) {
                        let _ = socket.send_to(&reply, from);
                    }
                }
            })
        };
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("the SNMP agent started");
        Self {
            requests,
            delay_ms,
            stop,
            handle: Some(handle),
        }
    }

    pub fn set_delay(&self, delay: Duration) {
        self.delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// Every request so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// (offset of the value, length of the value) of the TLV at `at`.
fn tlv(b: &[u8], at: usize) -> Option<(usize, usize)> {
    let first = usize::from(*b.get(at + 1)?);
    if first < 0x80 {
        return Some((at + 2, first));
    }
    let n = first & 0x7f;
    let len = b
        .get(at + 2..at + 2 + n)?
        .iter()
        .fold(0usize, |acc, x| (acc << 8) | usize::from(*x));
    Some((at + 2 + n, len))
}

/// The offsets of the community's value and of the PDU's tag.
fn layout(b: &[u8]) -> Option<(usize, usize, usize)> {
    let (message, _) = tlv(b, 0)?;
    let (version, version_len) = tlv(b, message)?;
    let community_at = version + version_len;
    let (community, community_len) = tlv(b, community_at)?;
    Some((community, community_len, community + community_len))
}

/// The PDU tag and the community of a request.
fn request_of(b: &[u8]) -> Option<(u8, Vec<u8>)> {
    let (community, community_len, pdu) = layout(b)?;
    let kind = *b.get(pdu)?;
    matches!(kind, 0xA0 | 0xA1 | 0xA3)
        .then(|| {
            (
                kind,
                b.get(community..community + community_len)
                    .map(<[u8]>::to_vec),
            )
        })
        .and_then(|(kind, c)| c.map(|c| (kind, c)))
}

/// The request turned into a GetResponse with error-status 0.
fn response_to(b: &[u8]) -> Option<Vec<u8>> {
    let mut reply = b.to_vec();
    let (_, _, pdu) = layout(b)?;
    reply[pdu] = 0xA2;
    let (inner, _) = tlv(&reply, pdu)?;
    let (id, id_len) = tlv(&reply, inner)?;
    let (status, status_len) = tlv(&reply, id + id_len)?;
    if status_len != 1 {
        return None;
    }
    reply[status] = 0;
    Some(reply)
}
