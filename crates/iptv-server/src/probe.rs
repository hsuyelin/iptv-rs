//! Self-check used by Docker health checks: `iptv-rs --healthcheck`.
//!
//! The runtime image holds no shell or HTTP client, so the binary asks its own `/health`
//! route and reports through the exit code.

use std::{net::SocketAddr, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

/// Longest wait for the connection plus the answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Why the relay did not answer as healthy.
#[derive(Debug, Error)]
pub enum ProbeError {
    /// Connecting or exchanging data failed.
    #[error("cannot reach the relay at {addr}: {source}")]
    Io {
        /// Address that was probed.
        addr: SocketAddr,
        /// Underlying failure.
        source: std::io::Error,
    },
    /// No answer arrived within [`PROBE_TIMEOUT`].
    #[error("the relay at {addr} did not answer in time")]
    TimedOut {
        /// Address that was probed.
        addr: SocketAddr,
    },
    /// The relay answered with something other than `200`.
    #[error("the relay at {addr} answered `{status_line}`")]
    Unhealthy {
        /// Address that was probed.
        addr: SocketAddr,
        /// First line of the answer.
        status_line: String,
    },
}

/// Requests `/health` from `addr` and succeeds only on a `200` answer.
///
/// # Errors
/// Returns [`ProbeError`] when the relay is unreachable, silent or unhealthy.
pub async fn probe(addr: SocketAddr) -> Result<(), ProbeError> {
    let exchange = async {
        let mut stream = TcpStream::connect(addr).await?;
        stream
            .write_all(
                b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await?;
        let mut head = Vec::new();
        // Only the status line matters; the answer is closed by the server.
        stream.take(512).read_to_end(&mut head).await?;
        Ok::<_, std::io::Error>(head)
    };
    let bytes = timeout(PROBE_TIMEOUT, exchange)
        .await
        .map_err(|_| ProbeError::TimedOut { addr })?
        .map_err(|source| ProbeError::Io { addr, source })?;
    let text = String::from_utf8_lossy(&bytes);
    let status_line = text.lines().next().unwrap_or_default().trim().to_owned();
    if status_line.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(ProbeError::Unhealthy { addr, status_line })
    }
}

#[cfg(test)]
mod tests;
