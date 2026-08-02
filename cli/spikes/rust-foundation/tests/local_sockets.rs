use apppilotkit_rust_foundation_spike::{LocalEndpoint, SocketError, round_trip_local};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn tcp_and_unix_round_trips_are_local_and_cancellable() {
    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let tcp_address = tcp_listener.local_addr().expect("loopback address");
    let tcp_server = tokio::spawn(async move {
        let (mut stream, _) = tcp_listener.accept().await.expect("TCP accept");
        let mut request = Vec::new();
        stream.read_to_end(&mut request).await.expect("TCP read");
        assert_eq!(request, b"tcp ping");
        stream.write_all(b"tcp pong").await.expect("TCP write");
    });
    let tcp_response = round_trip_local(
        LocalEndpoint::Tcp(tcp_address),
        b"tcp ping",
        tokio::time::Instant::now() + Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await
    .expect("TCP round trip");
    assert_eq!(tcp_response, b"tcp pong");
    tcp_server.await.expect("TCP server task");

    let directory = tempfile::tempdir().expect("Unix socket directory");
    let socket_path = directory.path().join("probe.sock");
    let unix_listener = tokio::net::UnixListener::bind(&socket_path).expect("Unix listener");
    let unix_server = tokio::spawn(async move {
        let (mut stream, _) = unix_listener.accept().await.expect("Unix accept");
        let mut request = Vec::new();
        stream.read_to_end(&mut request).await.expect("Unix read");
        assert_eq!(request, b"unix ping");
        stream.write_all(b"unix pong").await.expect("Unix write");
    });
    let unix_response = round_trip_local(
        LocalEndpoint::Unix(socket_path),
        b"unix ping",
        tokio::time::Instant::now() + Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await
    .expect("Unix round trip");
    assert_eq!(unix_response, b"unix pong");
    unix_server.await.expect("Unix server task");
}

#[tokio::test]
async fn pending_socket_read_obeys_external_cancellation_and_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("loopback address");
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.expect("TCP accept");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let cancelled = round_trip_local(
        LocalEndpoint::Tcp(address),
        b"cancel me",
        tokio::time::Instant::now() + Duration::from_secs(1),
        cancellation,
    )
    .await;
    assert!(matches!(cancelled, Err(SocketError::Cancelled)));
    assert!(started.elapsed() < Duration::from_millis(500));
    server.abort();

    let directory = tempfile::tempdir().expect("Unix socket directory");
    let socket_path = directory.path().join("cancel.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("Unix listener");
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.expect("Unix accept");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let cancelled = round_trip_local(
        LocalEndpoint::Unix(socket_path),
        b"cancel unix",
        tokio::time::Instant::now() + Duration::from_secs(1),
        cancellation,
    )
    .await;
    assert!(matches!(cancelled, Err(SocketError::Cancelled)));
    server.abort();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let address = listener.local_addr().expect("loopback address");
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.expect("TCP accept");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let started = Instant::now();
    let timed_out = round_trip_local(
        LocalEndpoint::Tcp(address),
        b"time out",
        tokio::time::Instant::now() + Duration::from_millis(50),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(timed_out, Err(SocketError::TimedOut)));
    assert!(started.elapsed() < Duration::from_millis(500));
    server.abort();
}

#[tokio::test]
async fn tcp_rejects_non_loopback_addresses_before_connecting() {
    let address = "192.0.2.1:9".parse().expect("documentation address");
    let result = round_trip_local(
        LocalEndpoint::Tcp(address),
        b"must not leave the host",
        tokio::time::Instant::now() + Duration::from_secs(1),
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(SocketError::NonLoopback(actual)) if actual == address));
}
