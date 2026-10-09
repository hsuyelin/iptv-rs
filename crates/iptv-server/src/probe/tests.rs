use tokio::net::TcpListener;

use super::*;

async fn answer_once(reply: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 256];
        let _ = stream.read(&mut request).await;
        stream.write_all(reply).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn a_200_answer_is_healthy() {
    let addr = answer_once(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
    probe(addr).await.unwrap();
}

#[tokio::test]
async fn any_other_status_is_unhealthy() {
    let addr = answer_once(b"HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
    let error = probe(addr).await.unwrap_err();
    assert!(
        matches!(&error, ProbeError::Unhealthy { status_line, .. } if status_line.contains("503"))
    );
}

#[tokio::test]
async fn a_closed_port_is_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    assert!(matches!(probe(addr).await, Err(ProbeError::Io { .. })));
}

#[tokio::test(start_paused = true)]
async fn silence_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _held = tokio::spawn(async move {
        let _conn = listener.accept().await;
        std::future::pending::<()>().await;
    });
    assert!(matches!(
        probe(addr).await,
        Err(ProbeError::TimedOut { .. })
    ));
}
