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

#[cfg(test)]
mod tests {
    use std::{
        mem,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        time::SystemTime,
    };

    use super::*;

    fn deadline_after(millis: u64) -> AbsoluteDeadline {
        let now = u64::try_from(
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("timestamp");
        AbsoluteDeadline::new(now + millis).unwrap_or_else(|_| panic!("deadline"))
    }

    #[test]
    fn unavailable_loopback_endpoint_stops_at_deadline() {
        let (_reservation, port) = reserve_unlistening_tcp_port();
        let error = connect(port, &Cancellation::new(), deadline_after(30))
            .err()
            .expect("connection deadline");
        assert_eq!(error.kind(), PlatformFailureKind::TimedOut);
    }

    fn reserve_unlistening_tcp_port() -> (OwnedFd, u16) {
        // SAFETY: the returned descriptor is immediately wrapped in OwnedFd;
        // all sockaddr pointers reference initialized local storage for the call.
        unsafe {
            let descriptor = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert!(descriptor >= 0, "socket reservation");
            let descriptor = OwnedFd::from_raw_fd(descriptor);
            let mut address: libc::sockaddr_in = mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            assert_eq!(
                libc::bind(
                    descriptor.as_raw_fd(),
                    (&raw const address).cast(),
                    libc::socklen_t::try_from(mem::size_of_val(&address)).expect("sockaddr length"),
                ),
                0,
                "bind reservation"
            );
            let mut length =
                libc::socklen_t::try_from(mem::size_of_val(&address)).expect("sockaddr length");
            assert_eq!(
                libc::getsockname(
                    descriptor.as_raw_fd(),
                    (&raw mut address).cast(),
                    &mut length,
                ),
                0,
                "reserved address"
            );
            let port = u16::from_be(address.sin_port);
            assert_ne!(port, 0);
            (descriptor, port)
        }
    }
}
