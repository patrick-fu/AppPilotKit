use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, PlatformFailure, PlatformFailureKind, RawConnector, RawDuplex,
};

use crate::process::{ensure_active, failure, remaining};

const CONNECT_RETRY: Duration = Duration::from_millis(5);

pub(crate) struct LoopbackConnector {
    port: u16,
}

impl LoopbackConnector {
    pub(crate) const fn new(port: u16) -> Self {
        Self { port }
    }
}

impl RawConnector for LoopbackConnector {
    fn connect(
        &self,
        cancellation: Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
        connect(self.port, &cancellation, deadline)
            .map(|stream| Arc::new(stream) as Arc<dyn RawDuplex>)
    }
}

pub(crate) fn connect(
    port: u16,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<LoopbackRaw, PlatformFailure> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    loop {
        ensure_active(cancellation, deadline)?;
        let timeout = remaining(deadline)?.min(Duration::from_millis(50));
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => return LoopbackRaw::new(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) =>
            {
                thread::sleep(CONNECT_RETRY.min(remaining(deadline)?));
            }
            Err(_) => return Err(failure(PlatformFailureKind::Unavailable)),
        }
    }
}

pub(crate) struct LoopbackRaw {
    read: Mutex<TcpStream>,
    write: Mutex<TcpStream>,
    control: TcpStream,
    cancelled: AtomicBool,
}

impl LoopbackRaw {
    fn new(stream: TcpStream) -> Result<Self, PlatformFailure> {
        stream
            .set_nodelay(true)
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        let read = stream
            .try_clone()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        let write = stream
            .try_clone()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        Ok(Self {
            read: Mutex::new(read),
            write: Mutex::new(write),
            control: stream,
            cancelled: AtomicBool::new(false),
        })
    }

    fn check(&self, deadline: AbsoluteDeadline) -> Result<Duration, PlatformFailure> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(failure(PlatformFailureKind::Cancelled));
        }
        remaining(deadline)
    }
}

impl RawDuplex for LoopbackRaw {
    fn read(
        &self,
        output: &mut [u8],
        deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure> {
        let timeout = self.check(deadline)?;
        let mut stream = self
            .read
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream.read(output).map_err(map_io)
    }

    fn write(&self, input: &[u8], deadline: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
        let timeout = self.check(deadline)?;
        let mut stream = self
            .write
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream.write(input).map_err(map_io)
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.control.shutdown(Shutdown::Both);
        }
    }
}

fn map_io(error: io::Error) -> PlatformFailure {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            failure(PlatformFailureKind::TimedOut)
        }
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::NotConnected => failure(PlatformFailureKind::Eof),
        _ => failure(PlatformFailureKind::Unavailable),
    }
}
