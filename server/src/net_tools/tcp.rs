//! `sctl net tcp`: does a port answer, and how fast (ADR-006), from a socket
//! bound where the guard approved (ADR-012).

use std::io;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::guard::{Approved, Connect};

#[derive(Debug, Serialize, PartialEq)]
pub struct TcpOutcome {
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Connect to `port` on the approved address, bound to its interface and
/// source address. `Err` when the socket could not be bound: nothing was
/// sent.
pub async fn probe(approved: &Approved, port: u16, timeout: Duration) -> io::Result<TcpOutcome> {
    let start = Instant::now();
    match tokio::time::timeout(timeout, approved.connect_tcp(port)).await {
        Ok(Ok(_)) => Ok(TcpOutcome {
            open: true,
            ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        }),
        Ok(Err(Connect::Bind(e))) => Err(e),
        Ok(Err(Connect::Connect(e))) => Ok(TcpOutcome {
            open: false,
            ms: None,
            error: Some(e.to_string()),
        }),
        Err(_) => Ok(TcpOutcome {
            open: false,
            ms: None,
            error: Some(format!("no answer within {}ms", timeout.as_millis())),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::net_tools::guard::Guard;

    #[tokio::test]
    async fn open_and_closed() {
        let approved = Guard::default().approve(Ipv4Addr::LOCALHOST).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let open = probe(&approved, port, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(open.open, "{open:?}");
        drop(listener);
        let closed = probe(&approved, port, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!closed.open);
        assert!(closed.error.is_some());
    }

    #[tokio::test]
    async fn a_socket_that_cannot_be_bound_sends_nothing() {
        let approved = Guard::default().approve(Ipv4Addr::LOCALHOST).await.unwrap();
        let gone = Approved {
            dev: "sctl-none0".into(),
            ..approved
        };
        assert!(probe(&gone, 9, Duration::from_secs(2)).await.is_err());
    }
}
