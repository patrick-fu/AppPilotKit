use apppilotkit_cli_contract::{
    CatalogDispatchPhase, CatalogExchangeError, CatalogExchangeFailure, CatalogRuntime,
    CatalogSelectError, OpenedProtocolSession, SessionSelector,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HANDSHAKE_MAX_BYTES: usize = 64 * 1024;
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(1);
const EXCHANGE_DEADLINE: Duration = Duration::from_secs(2);
const IO_SLICE: Duration = Duration::from_millis(100);

pub(crate) struct FixtureCatalogRuntime {
    socket: PathBuf,
    connection: Mutex<Option<BufReader<UnixStream>>>,
}

impl FixtureCatalogRuntime {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            connection: Mutex::new(None),
        }
    }
}

impl CatalogRuntime for FixtureCatalogRuntime {
    fn select(
        &self,
        selector: SessionSelector<'_>,
    ) -> Result<OpenedProtocolSession, CatalogSelectError> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|_| CatalogSelectError::AuthenticationRequired)?;
        let mut connection = BufReader::new(stream);
        write_json_line(
            connection.get_mut(),
            &serde_json::json!({
                "type": "select",
                "session": selector.session,
                "target": selector.target,
            }),
        )
        .map_err(|_| CatalogSelectError::AuthenticationRequired)?;
        let response = read_json_line(&mut connection, HANDSHAKE_MAX_BYTES, HANDSHAKE_DEADLINE)
            .map_err(|_| CatalogSelectError::AuthenticationRequired)?;
        let session = parse_session(&response)?;
        *self.connection.lock().expect("fixture connection lock") = Some(connection);
        Ok(session)
    }

    fn exchange(
        &self,
        session: &OpenedProtocolSession,
        request: &Value,
    ) -> Result<Vec<u8>, CatalogExchangeError> {
        let mut guard = self.connection.lock().expect("fixture connection lock");
        let outcome = {
            let connection = guard.as_mut().ok_or_else(|| {
                CatalogExchangeError::pre_dispatch(CatalogExchangeFailure::TransportInternal)
            })?;
            write_json_line(
                connection.get_mut(),
                &serde_json::json!({"type": "exchange", "request": request}),
            )
            .map_err(|_| {
                CatalogExchangeError::post_dispatch(CatalogExchangeFailure::TransportInternal)
            })?;
            read_bounded_line(connection, session.max_response_bytes, EXCHANGE_DEADLINE)
        };
        match outcome {
            Ok(BoundedLine::Complete(response)) => Ok(response),
            Ok(BoundedLine::Oversized(response)) => {
                guard.take();
                Ok(response)
            }
            Err(BoundedReadError::Deadline) => {
                guard.take();
                Err(CatalogExchangeError::post_dispatch(
                    CatalogExchangeFailure::Timeout,
                ))
            }
            Err(BoundedReadError::EndOfStream) => {
                guard.take();
                Err(CatalogExchangeError {
                    phase: CatalogDispatchPhase::PostDispatch,
                    failure: CatalogExchangeFailure::EndOfStream,
                })
            }
            Err(BoundedReadError::Io) => {
                guard.take();
                Err(CatalogExchangeError::post_dispatch(
                    CatalogExchangeFailure::TransportInternal,
                ))
            }
        }
    }
}

fn write_json_line(stream: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn read_json_line(
    stream: &mut BufReader<UnixStream>,
    limit: usize,
    timeout: Duration,
) -> Result<Value, ()> {
    match read_bounded_line(stream, limit, timeout).map_err(|_| ())? {
        BoundedLine::Complete(line) => serde_json::from_slice(&line).map_err(|_| ()),
        BoundedLine::Oversized(_) => Err(()),
    }
}

enum BoundedLine {
    Complete(Vec<u8>),
    Oversized(Vec<u8>),
}

enum BoundedReadError {
    Deadline,
    EndOfStream,
    Io,
}

fn read_bounded_line(
    reader: &mut BufReader<UnixStream>,
    payload_limit: usize,
    timeout: Duration,
) -> Result<BoundedLine, BoundedReadError> {
    let deadline = Instant::now() + timeout;
    let mut frame = Vec::with_capacity(payload_limit.saturating_add(1).min(64 * 1024));
    reader
        .get_mut()
        .set_read_timeout(Some(IO_SLICE))
        .map_err(|_| BoundedReadError::Io)?;
    loop {
        if Instant::now() >= deadline {
            return Err(BoundedReadError::Deadline);
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return Err(BoundedReadError::Io),
        };
        if available.is_empty() {
            return Err(BoundedReadError::EndOfStream);
        }
        let remaining_budget = payload_limit.saturating_add(1).saturating_sub(frame.len());
        let inspected = available.len().min(remaining_budget);
        if let Some(newline) = available[..inspected]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return if frame.len() > payload_limit {
                Ok(BoundedLine::Oversized(frame))
            } else {
                Ok(BoundedLine::Complete(frame))
            };
        }
        frame.extend_from_slice(&available[..inspected]);
        reader.consume(inspected);
        if frame.len() > payload_limit {
            return Ok(BoundedLine::Oversized(frame));
        }
    }
}

fn parse_session(value: &Value) -> Result<OpenedProtocolSession, CatalogSelectError> {
    let field = |name: &str| {
        value
            .get(name)
            .ok_or(CatalogSelectError::AuthenticationRequired)
    };
    let usize_field = |name: &str| {
        field(name)?
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(CatalogSelectError::AuthenticationRequired)
    };
    Ok(OpenedProtocolSession {
        session_id: field("session_id")?
            .as_str()
            .ok_or(CatalogSelectError::AuthenticationRequired)?
            .to_owned(),
        generation: field("generation")?
            .as_u64()
            .ok_or(CatalogSelectError::AuthenticationRequired)?,
        target_id: field("target_id")?
            .as_str()
            .ok_or(CatalogSelectError::AuthenticationRequired)?
            .to_owned(),
        protocol_major: field("protocol_major")?
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CatalogSelectError::AuthenticationRequired)?,
        protocol_minor: field("protocol_minor")?
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CatalogSelectError::AuthenticationRequired)?,
        capabilities: field("capabilities")?
            .as_array()
            .ok_or(CatalogSelectError::AuthenticationRequired)?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or(CatalogSelectError::AuthenticationRequired)
            })
            .collect::<Result<Vec<_>, _>>()?,
        max_request_bytes: usize_field("max_request_bytes")?,
        max_response_bytes: usize_field("max_response_bytes")?,
        max_page_items: usize_field("max_page_items")?,
    })
}
