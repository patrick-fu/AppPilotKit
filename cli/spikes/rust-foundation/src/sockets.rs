use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum LocalEndpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Debug)]
pub enum SocketError {
    NonLoopback(SocketAddr),
    Cancelled,
    TimedOut,
    Io(io::Error),
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonLoopback(address) => {
                write!(formatter, "TCP endpoint is not loopback: {address}")
            }
            Self::Cancelled => formatter.write_str("local socket operation was cancelled"),
            Self::TimedOut => formatter.write_str("local socket operation timed out"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SocketError {}

pub async fn round_trip_local(
    endpoint: LocalEndpoint,
    request: &[u8],
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<Vec<u8>, SocketError> {
    let mut stream = match endpoint {
        LocalEndpoint::Tcp(address) => {
            if !address.ip().is_loopback() {
                return Err(SocketError::NonLoopback(address));
            }
            LocalStream::Tcp(await_io(TcpStream::connect(address), deadline, &cancellation).await?)
        }
        LocalEndpoint::Unix(path) => {
            LocalStream::Unix(await_io(UnixStream::connect(path), deadline, &cancellation).await?)
        }
    };

    let mut offset = 0;
    while offset < request.len() {
        let written = await_io(stream.write(&request[offset..]), deadline, &cancellation).await?;
        if written == 0 {
            return Err(SocketError::Io(io::Error::from(io::ErrorKind::WriteZero)));
        }
        offset += written;
    }
    await_io(stream.shutdown(), deadline, &cancellation).await?;

    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = await_io(stream.read(&mut buffer), deadline, &cancellation).await?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
    }
    Ok(response)
}

enum LocalStream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl LocalStream {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(bytes).await,
            Self::Unix(stream) => stream.write(bytes).await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.shutdown().await,
            Self::Unix(stream) => stream.shutdown().await,
        }
    }

    async fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(bytes).await,
            Self::Unix(stream) => stream.read(bytes).await,
        }
    }
}

async fn await_io<F, T>(
    operation: F,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<T, SocketError>
where
    F: Future<Output = io::Result<T>>,
{
    tokio::pin!(operation);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(SocketError::Cancelled),
        () = tokio::time::sleep_until(deadline) => Err(SocketError::TimedOut),
        result = &mut operation => result.map_err(SocketError::Io),
    }
}
