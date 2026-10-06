//! `sctl net tcp`: does a port answer, and how fast (ADR-006).

use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
pub struct TcpOutcome {
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn probe(host: &str, port: u16, timeout: Duration) -> TcpOutcome {
    let start = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => TcpOutcome {
            open: true,
            ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(Err(e)) => TcpOutcome {
            open: false,
            ms: None,
            error: Some(e.to_string()),
        },
        Err(_) => TcpOutcome {
            open: false,
            ms: None,
            error: Some(format!("no answer within {}ms", timeout.as_millis())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_and_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe("127.0.0.1", port, Duration::from_secs(2)).await.open);
        drop(listener);
        assert!(!probe("127.0.0.1", port, Duration::from_secs(2)).await.open);
    }
}
