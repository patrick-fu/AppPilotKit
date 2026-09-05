//! Private D2 bridge between the publish-disabled platform seam and C1.
//!
//! This module owns only byte transport and bootstrap resource transfer.  It
//! deliberately has no broker IPC, JSON, or platform launch policy.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use apppilotkit_transport_crypto_core::{
    BootstrapBinding, BrokerBootstrap, BrokerLeaseConnection, BrokerSession,
    BrokerStaticPrivateKey, CloseReason, HandoffState, OuterFrameDecoder, ProcessBootstrapSecret,
    SessionBinding, TargetBootstrapAck,
};

use crate::{
    Platform,
    adapter::{
        AbsoluteDeadline, Cancellation, CleanupReceipt, LaunchEndpoint, PlatformFailure,
        PlatformFailureKind, RawConnector, RawDuplex,
    },
};

const SESSION_TOTAL_MS: u64 = 4_000;
const SESSION_CONNECT_MS: u64 = 1_000;
const SESSION_HANDSHAKE_MS: u64 = 1_000;
const SESSION_OPEN_RESPONSE_MS: u64 = 2_000;
const INCOMPLETE_OUTER_MS: u64 = 2_000;
const CLEANUP_MS: u64 = 2_000;

fn now_unix_ms() -> Result<u64, TransportFailure> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TransportFailure::new(CloseReason::InternalError))?
            .as_millis(),
    )
    .map_err(|_| TransportFailure::new(CloseReason::InternalError))
}

const fn deadline_expired_at(now: u64, deadline: u64) -> bool {
    now >= deadline
}

const fn phase_deadline_value(command: u64, started: u64, budget_ms: u64) -> u64 {
    let budget_deadline = started.saturating_add(budget_ms);
    if command < budget_deadline {
        command
    } else {
        budget_deadline
    }
}

fn phase_deadline(
    command: AbsoluteDeadline,
    started: u64,
    budget_ms: u64,
) -> Result<AbsoluteDeadline, TransportFailure> {
    AbsoluteDeadline::new(phase_deadline_value(command.value(), started, budget_ms))
        .map_err(platform_failure)
}

fn ensure_deadline(deadline: AbsoluteDeadline) -> Result<(), TransportFailure> {
    ensure_deadline_at(now_unix_ms()?, deadline)
}

fn ensure_deadline_at(now: u64, deadline: AbsoluteDeadline) -> Result<(), TransportFailure> {
    if deadline_expired_at(now, deadline.value()) {
        return Err(TransportFailure::new(CloseReason::Timeout));
    }
    Ok(())
}

fn cleanup_deadline() -> Result<AbsoluteDeadline, TransportFailure> {
    AbsoluteDeadline::new(now_unix_ms()?.saturating_add(CLEANUP_MS)).map_err(platform_failure)
}

fn platform_failure(failure: PlatformFailure) -> TransportFailure {
    let kind = failure.kind();
    let reason = match kind {
        PlatformFailureKind::TimedOut | PlatformFailureKind::Cancelled => CloseReason::Timeout,
        PlatformFailureKind::Eof => CloseReason::PeerClosed,
        PlatformFailureKind::Rejected => CloseReason::BindingMismatch,
        PlatformFailureKind::CleanupFailed => CloseReason::CleanupFailed,
        PlatformFailureKind::Unavailable | PlatformFailureKind::Internal => {
            CloseReason::InternalError
        }
    };
    let transport = if kind == PlatformFailureKind::CleanupFailed {
        TransportFailure::cleanup_failed(reason)
    } else {
        TransportFailure::new(reason)
    };
    #[cfg(feature = "internal-diagnostics")]
    {
        if kind == PlatformFailureKind::Rejected {
            return transport.with_bootstrap_origin(BootstrapFailureOrigin::AdapterRejected);
        }
    }
    transport
}

#[cfg(feature = "internal-diagnostics")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootstrapFailureOrigin {
    AdapterRejected,
    AckBindingMismatch,
}

#[cfg(feature = "internal-diagnostics")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionFailureOrigin {
    TargetNoSessionFrames,
    LeaseTerminalBeforeSessionCommit,
}

/// All terminal information this layer is allowed to pass to its caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransportFailure {
    pub(crate) close_reason: CloseReason,
    pub(crate) handoff: HandoffState,
    pub(crate) cleanup_failed: bool,
    #[cfg(feature = "internal-diagnostics")]
    pub(crate) bootstrap_origin: Option<BootstrapFailureOrigin>,
    #[cfg(feature = "internal-diagnostics")]
    pub(crate) session_origin: Option<SessionFailureOrigin>,
}

impl TransportFailure {
    const fn new(close_reason: CloseReason) -> Self {
        Self {
            close_reason,
            handoff: HandoffState::NotHandedOff,
            cleanup_failed: false,
            #[cfg(feature = "internal-diagnostics")]
            bootstrap_origin: None,
            #[cfg(feature = "internal-diagnostics")]
            session_origin: None,
        }
    }

    const fn cleanup_failed(close_reason: CloseReason) -> Self {
        Self {
            close_reason,
            handoff: HandoffState::NotHandedOff,
            cleanup_failed: true,
            #[cfg(feature = "internal-diagnostics")]
            bootstrap_origin: None,
            #[cfg(feature = "internal-diagnostics")]
            session_origin: None,
        }
    }

    const fn with_handoff(mut self, handoff: HandoffState) -> Self {
        self.handoff = match (self.handoff, handoff) {
            (HandoffState::HandoffPossibleOrConfirmed, _)
            | (_, HandoffState::HandoffPossibleOrConfirmed) => {
                HandoffState::HandoffPossibleOrConfirmed
            }
            _ => HandoffState::NotHandedOff,
        };
        self
    }

    const fn with_cleanup_failed(mut self) -> Self {
        self.cleanup_failed = true;
        self
    }

    #[cfg(feature = "internal-diagnostics")]
    const fn with_bootstrap_origin(mut self, origin: BootstrapFailureOrigin) -> Self {
        self.bootstrap_origin = Some(origin);
        self
    }

    #[cfg(feature = "internal-diagnostics")]
    const fn with_session_origin(mut self, origin: SessionFailureOrigin) -> Self {
        self.session_origin = Some(origin);
        self
    }
}

/// The resources which become Broker-owned only after successful bootstrap.
pub(crate) struct BootstrapSuccess {
    pub(crate) ack: TargetBootstrapAck,
    pub(crate) lease: BrokerLeaseConnection,
    pub(crate) bootstrap: Arc<dyn RawDuplex>,
    pub(crate) connector: Arc<dyn RawConnector>,
    pub(crate) cleanup: Box<dyn CleanupReceipt>,
    pub(crate) reader: RawFrameReader,
}

/// A fully authenticated, Target-bound session. It deliberately does not cross
/// the adapter seam: C1 state and handoff semantics remain D2-private.
pub(crate) struct OpenedSession {
    pub(crate) raw: Arc<dyn RawDuplex>,
    pub(crate) session: BrokerSession,
    pub(crate) response: Vec<u8>,
    pub(crate) handoff: HandoffState,
    pub(crate) reader: RawFrameReader,
}

/// Incremental raw reader which preserves frames coalesced in a single read.
pub(crate) struct RawFrameReader {
    decoder: OuterFrameDecoder,
    ready: VecDeque<Vec<u8>>,
    outer_progress: OuterProgress,
    incomplete_deadline: Option<AbsoluteDeadline>,
    terminal: Option<TransportFailure>,
}

#[derive(Default)]
struct OuterProgress {
    header: Vec<u8>,
    expected: Option<usize>,
    payload_len: usize,
}

impl OuterProgress {
    fn is_incomplete(&self) -> bool {
        !self.header.is_empty() || self.expected.is_some()
    }

    fn push(&mut self, input: &[u8]) {
        for &byte in input {
            match self.expected {
                None => {
                    self.header.push(byte);
                    if self.header.len() == 2 {
                        self.expected =
                            Some(u16::from_be_bytes([self.header[0], self.header[1]]) as usize);
                        self.header.clear();
                        self.payload_len = 0;
                    }
                }
                Some(expected) => {
                    self.payload_len += 1;
                    if self.payload_len == expected {
                        self.expected = None;
                        self.payload_len = 0;
                    }
                }
            }
        }
    }
}

impl RawFrameReader {
    pub(crate) fn new() -> Self {
        Self {
            decoder: OuterFrameDecoder::new(),
            ready: VecDeque::new(),
            outer_progress: OuterProgress::default(),
            incomplete_deadline: None,
            terminal: None,
        }
    }

    pub(crate) fn read_outer(
        &mut self,
        raw: &dyn RawDuplex,
        deadline: AbsoluteDeadline,
    ) -> Result<Vec<u8>, TransportFailure> {
        self.read_outer_with_now(raw, deadline, &mut now_unix_ms)
    }

    fn read_outer_with_now<F>(
        &mut self,
        raw: &dyn RawDuplex,
        deadline: AbsoluteDeadline,
        now: &mut F,
    ) -> Result<Vec<u8>, TransportFailure>
    where
        F: FnMut() -> Result<u64, TransportFailure>,
    {
        self.ensure_not_terminal()?;
        let current = now()?;
        self.expire_incomplete_at(current)?;
        ensure_deadline_at(current, deadline)?;
        if let Some(frame) = self.ready.pop_front() {
            return Ok(frame);
        }
        let mut chunk = [0_u8; 4096];
        loop {
            let current = now()?;
            self.expire_incomplete_at(current)?;
            ensure_deadline_at(current, deadline)?;
            let read_deadline = self.read_deadline(deadline)?;
            let read = match raw.read(&mut chunk, read_deadline) {
                Ok(read) => read,
                Err(failure) => {
                    self.expire_incomplete_at(now()?)?;
                    if failure.kind() == PlatformFailureKind::Eof {
                        return Err(self.eof_from_outer());
                    }
                    return Err(platform_failure(failure));
                }
            };
            if read > chunk.len() {
                return Err(self.terminate(TransportFailure::new(CloseReason::InternalError)));
            }
            if read == 0 {
                return Err(self.eof_from_outer());
            }
            let frames = match self.decoder.push(&chunk[..read]).map_err(core_failure) {
                Ok(frames) => frames,
                Err(failure) => return Err(self.terminate(failure)),
            };
            self.track_outer_progress(&chunk[..read], deadline, now()?)?;
            self.ready.extend(frames);
            if let Some(frame) = self.ready.pop_front() {
                return Ok(frame);
            }
        }
    }

    fn read_outer_for_session(
        &mut self,
        raw: &dyn RawDuplex,
        session: &mut BrokerSession,
        deadline: AbsoluteDeadline,
    ) -> Result<Vec<u8>, TransportFailure> {
        self.ensure_not_terminal()?;
        let current = now_unix_ms()?;
        self.expire_incomplete_at(current)?;
        ensure_deadline_at(current, deadline)?;
        if let Some(frame) = self.ready.pop_front() {
            return Ok(frame);
        }
        let mut chunk = [0_u8; 4096];
        loop {
            let current = now_unix_ms()?;
            self.expire_incomplete_at(current)?;
            ensure_deadline_at(current, deadline)?;
            let read_deadline = self.read_deadline(deadline)?;
            let read = match raw.read(&mut chunk, read_deadline) {
                Ok(read) => read,
                Err(failure) => {
                    self.expire_incomplete_at(now_unix_ms()?)?;
                    if failure.kind() == PlatformFailureKind::Eof {
                        return Err(self.eof_from_session(raw, session, deadline));
                    }
                    return Err(platform_failure(failure));
                }
            };
            if read > chunk.len() {
                return Err(self.terminate(TransportFailure::new(CloseReason::InternalError)));
            }
            if read == 0 {
                return Err(self.eof_from_session(raw, session, deadline));
            }
            let frames = match self.decoder.push(&chunk[..read]).map_err(core_failure) {
                Ok(frames) => frames,
                Err(failure) => return Err(self.terminate(failure)),
            };
            self.track_outer_progress(&chunk[..read], deadline, now_unix_ms()?)?;
            self.ready.extend(frames);
            if let Some(frame) = self.ready.pop_front() {
                return Ok(frame);
            }
        }
    }

    fn read_deadline(
        &self,
        command_deadline: AbsoluteDeadline,
    ) -> Result<AbsoluteDeadline, TransportFailure> {
        match self.incomplete_deadline {
            Some(incomplete) => {
                AbsoluteDeadline::new(command_deadline.value().min(incomplete.value()))
                    .map_err(platform_failure)
            }
            None => Ok(command_deadline),
        }
    }

    fn track_outer_progress(
        &mut self,
        input: &[u8],
        command_deadline: AbsoluteDeadline,
        now: u64,
    ) -> Result<(), TransportFailure> {
        self.outer_progress.push(input);
        if self.outer_progress.is_incomplete() {
            if self.incomplete_deadline.is_none() {
                self.incomplete_deadline =
                    Some(phase_deadline(command_deadline, now, INCOMPLETE_OUTER_MS)?);
            }
        } else {
            self.incomplete_deadline = None;
        }
        Ok(())
    }

    fn ensure_not_terminal(&self) -> Result<(), TransportFailure> {
        self.terminal.map_or(Ok(()), Err)
    }

    fn terminate(&mut self, failure: TransportFailure) -> TransportFailure {
        self.terminal = Some(failure);
        failure
    }

    fn eof_from_outer(&mut self) -> TransportFailure {
        let failure = self
            .decoder
            .eof()
            .map_err(core_failure)
            .err()
            .unwrap_or_else(|| TransportFailure::new(CloseReason::PeerClosed));
        self.terminate(failure)
    }

    fn eof_from_session(
        &mut self,
        raw: &dyn RawDuplex,
        session: &mut BrokerSession,
        deadline: AbsoluteDeadline,
    ) -> TransportFailure {
        let outer_failure = self.decoder.eof().map_err(core_failure).err();
        let session_failure = session
            .eof()
            .map_err(|error| c1_failure_after_close(raw, error, deadline))
            .err();
        self.terminate(
            outer_failure
                .or(session_failure)
                .unwrap_or_else(|| TransportFailure::new(CloseReason::PeerClosed)),
        )
    }

    fn expire_incomplete_at(&mut self, now: u64) -> Result<(), TransportFailure> {
        self.ensure_not_terminal()?;
        if self
            .incomplete_deadline
            .is_some_and(|deadline| deadline_expired_at(now, deadline.value()))
        {
            let failure = self
                .decoder
                .timeout()
                .map_err(core_failure)
                .err()
                .unwrap_or_else(|| TransportFailure::new(CloseReason::Timeout));
            self.terminal = Some(failure);
            return Err(failure);
        }
        Ok(())
    }
}

pub(crate) fn write_all(
    raw: &dyn RawDuplex,
    mut bytes: &[u8],
    deadline: AbsoluteDeadline,
) -> Result<(), TransportFailure> {
    while !bytes.is_empty() {
        ensure_deadline(deadline)?;
        let written = raw.write(bytes, deadline).map_err(platform_failure)?;
        if written == 0 || written > bytes.len() {
            return Err(TransportFailure::new(CloseReason::PeerClosed));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

/// Writes C1's one-shot authenticated Close if it exists, but always retains
/// the C1 failure that caused the terminal transition.
fn return_after_reserved_close(
    raw: &dyn RawDuplex,
    failure: TransportFailure,
    close_frame: Option<Vec<u8>>,
    deadline: AbsoluteDeadline,
) -> TransportFailure {
    if let Some(frame) = close_frame {
        let _ = write_all(raw, &frame, deadline);
    }
    failure
}

fn c1_failure_after_close(
    raw: &dyn RawDuplex,
    mut error: apppilotkit_transport_crypto_core::Error,
    deadline: AbsoluteDeadline,
) -> TransportFailure {
    let failure = core_failure_details(&error);
    return_after_reserved_close(raw, failure, error.take_close_frame(), deadline)
}

/// Executes one authenticated lease heartbeat round trip.
pub(crate) fn heartbeat(
    raw: &dyn RawDuplex,
    reader: &mut RawFrameReader,
    crypto: &mut BrokerLeaseConnection,
    counter: u64,
    deadline: AbsoluteDeadline,
) -> Result<(), TransportFailure> {
    let request = crypto
        .write_heartbeat_request(counter)
        .map_err(|error| c1_failure_after_close(raw, error, deadline))?;
    write_all(raw, &request, deadline)?;
    let received = heartbeat_reply(raw, reader, crypto, deadline)?;
    if received != counter {
        return Err(TransportFailure::new(CloseReason::SequenceViolation));
    }
    Ok(())
}

/// Receives and authenticates a pending lease heartbeat reply without emitting
/// a new request. The Broker uses this after a timed-out heartbeat write/read
/// round so it never duplicates the counter on the same control connection.
pub(crate) fn heartbeat_reply(
    raw: &dyn RawDuplex,
    reader: &mut RawFrameReader,
    crypto: &mut BrokerLeaseConnection,
    deadline: AbsoluteDeadline,
) -> Result<u64, TransportFailure> {
    let reply = reader.read_outer(raw, deadline)?;
    crypto
        .read_heartbeat_reply(&reply)
        .map_err(|error| c1_failure_after_close(raw, error, deadline))
}

/// Opens one fresh authenticated session and receives its first response.
pub(crate) fn open_session(
    connector: &dyn RawConnector,
    cancellation: Cancellation,
    binding: SessionBinding,
    pbs: &ProcessBootstrapSecret,
    request: &[u8],
    deadline: AbsoluteDeadline,
) -> Result<OpenedSession, TransportFailure> {
    let started = now_unix_ms()?;
    let overall_deadline = phase_deadline(deadline, started, SESSION_TOTAL_MS)?;
    let connect_deadline = phase_deadline(overall_deadline, started, SESSION_CONNECT_MS)?;
    ensure_deadline(connect_deadline)?;
    let raw = connector
        .connect(cancellation, connect_deadline)
        .map_err(platform_failure)?;
    let result = (|| {
        let mut reader = RawFrameReader::new();
        let handshake_deadline =
            phase_deadline(overall_deadline, now_unix_ms()?, SESSION_HANDSHAKE_MS)?;
        let m1 = reader
            .read_outer(raw.as_ref(), handshake_deadline)
            .map_err(|failure| {
                #[cfg(feature = "internal-diagnostics")]
                {
                    failure.with_session_origin(SessionFailureOrigin::TargetNoSessionFrames)
                }
                #[cfg(not(feature = "internal-diagnostics"))]
                failure
            })?;
        let mut session = BrokerSession::new(binding, pbs)
            .map_err(|error| c1_failure_after_close(raw.as_ref(), error, handshake_deadline))?;
        let m2 = session
            .read_m1_write_m2(&m1)
            .map_err(|error| c1_failure_after_close(raw.as_ref(), error, handshake_deadline))?;
        write_all(raw.as_ref(), &m2, handshake_deadline)?;

        let target_finished =
            reader.read_outer_for_session(raw.as_ref(), &mut session, handshake_deadline)?;
        session
            .read_finished(&target_finished)
            .map_err(|error| c1_failure_after_close(raw.as_ref(), error, handshake_deadline))?;
        let broker_finished = session
            .write_finished()
            .map_err(|error| c1_failure_after_close(raw.as_ref(), error, handshake_deadline))?;
        write_all(raw.as_ref(), &broker_finished, handshake_deadline)?;

        let open_deadline =
            phase_deadline(overall_deadline, now_unix_ms()?, SESSION_OPEN_RESPONSE_MS)?;
        let open_frames = session
            .write_session_open(request)
            .map_err(|error| c1_failure_after_close(raw.as_ref(), error, open_deadline))?;
        write_frames(raw.as_ref(), &open_frames, open_deadline)?;
        let response = read_response(raw.as_ref(), &mut reader, &mut session, open_deadline)
            .map_err(|failure| failure.with_handoff(HandoffState::HandoffPossibleOrConfirmed))?;
        Ok(OpenedSession {
            raw: Arc::clone(&raw),
            session,
            response,
            handoff: HandoffState::HandoffPossibleOrConfirmed,
            reader,
        })
    })();
    if result.is_err() {
        raw.cancel();
    }
    result
}

/// Sends one opaque request and waits until C1 has reassembled one response.
pub(crate) fn exchange(
    raw: &dyn RawDuplex,
    reader: &mut RawFrameReader,
    session: &mut BrokerSession,
    request: &[u8],
    deadline: AbsoluteDeadline,
) -> Result<Vec<u8>, TransportFailure> {
    let request_frames = session
        .write_application_request(request)
        .map_err(|error| c1_failure_after_close(raw, error, deadline))?;
    write_frames(raw, &request_frames, deadline)?;
    read_response(raw, reader, session, deadline)
        .map_err(|failure| failure.with_handoff(HandoffState::HandoffPossibleOrConfirmed))
}

/// Best-effort terminal session close. Cancellation is unconditional once the
/// close attempt has started, so no caller can reuse a half-closed raw stream.
pub(crate) fn close_session(
    raw: &dyn RawDuplex,
    session: &mut BrokerSession,
    reason: CloseReason,
    handoff: HandoffState,
    deadline: AbsoluteDeadline,
) -> Result<(), TransportFailure> {
    let result = session
        .write_close(reason, handoff)
        .map_err(|error| c1_failure_after_close(raw, error, deadline))
        .and_then(|frame| write_all(raw, &frame, deadline))
        .map_err(|failure| failure.with_handoff(handoff));
    raw.cancel();
    result
}

/// Best-effort terminal lease close.
pub(crate) fn close_lease(
    raw: &dyn RawDuplex,
    lease: &mut BrokerLeaseConnection,
    reason: CloseReason,
    handoff: HandoffState,
    deadline: AbsoluteDeadline,
) -> Result<(), TransportFailure> {
    let result = lease
        .write_close(reason, handoff)
        .map_err(|error| c1_failure_after_close(raw, error, deadline))
        .and_then(|frame| write_all(raw, &frame, deadline))
        .map_err(|failure| failure.with_handoff(handoff));
    raw.cancel();
    result
}

fn write_frames(
    raw: &dyn RawDuplex,
    frames: &[Vec<u8>],
    deadline: AbsoluteDeadline,
) -> Result<(), TransportFailure> {
    for frame in frames {
        write_all(raw, frame, deadline)?;
    }
    Ok(())
}

fn read_response(
    raw: &dyn RawDuplex,
    reader: &mut RawFrameReader,
    session: &mut BrokerSession,
    deadline: AbsoluteDeadline,
) -> Result<Vec<u8>, TransportFailure> {
    loop {
        let outer = reader.read_outer_for_session(raw, session, deadline)?;
        if let Some(response) = session
            .read_application_response(&outer)
            .map_err(|error| c1_failure_after_close(raw, error, deadline))?
        {
            return Ok(response);
        }
    }
}

/// Performs exactly M1 -> M2 -> ACK and transfers raw ownership on success.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bootstrap(
    bootstrap: Arc<dyn RawDuplex>,
    connector: Arc<dyn RawConnector>,
    cleanup: Box<dyn CleanupReceipt>,
    broker_static_private: BrokerStaticPrivateKey,
    pbs: &ProcessBootstrapSecret,
    binding: BootstrapBinding,
    deadline: AbsoluteDeadline,
) -> Result<BootstrapSuccess, TransportFailure> {
    let result: Result<_, TransportFailure> = (|| {
        let mut reader = RawFrameReader::new();
        let m1 = reader.read_outer(bootstrap.as_ref(), deadline)?;
        let responder = BrokerBootstrap::new(binding, broker_static_private, pbs)
            .map_err(|error| c1_failure_after_close(bootstrap.as_ref(), error, deadline))?;
        let (m2, ack_receiver) = responder
            .read_m1_write_m2(&m1)
            .map_err(|error| c1_failure_after_close(bootstrap.as_ref(), error, deadline))?;
        write_all(bootstrap.as_ref(), &m2, deadline)?;
        let ack_outer = reader.read_outer(bootstrap.as_ref(), deadline)?;
        let (ack, lease) = ack_receiver
            .read_ack(&ack_outer)
            .map_err(|error| bootstrap_ack_failure(bootstrap.as_ref(), error, deadline))?;
        Ok((ack, lease, reader))
    })();

    match result {
        Ok((ack, lease, reader)) => Ok(BootstrapSuccess {
            ack,
            lease,
            bootstrap,
            connector,
            cleanup,
            reader,
        }),
        Err(failure) => {
            bootstrap.cancel();
            let cancellation = Cancellation::new();
            let cleanup_deadline = cleanup_deadline();
            match cleanup_deadline.and_then(|deadline| {
                ensure_deadline(deadline)?;
                cleanup
                    .cleanup(cancellation, deadline)
                    .map_err(platform_failure)
            }) {
                Ok(()) => Err(failure),
                Err(_) => Err(failure.with_cleanup_failed()),
            }
        }
    }
}

fn bootstrap_ack_failure(
    raw: &dyn RawDuplex,
    error: apppilotkit_transport_crypto_core::Error,
    deadline: AbsoluteDeadline,
) -> TransportFailure {
    #[cfg(feature = "internal-diagnostics")]
    let close_reason = error.close_reason();
    let failure = c1_failure_after_close(raw, error, deadline);
    #[cfg(feature = "internal-diagnostics")]
    if let Some(origin) = bootstrap_ack_failure_origin(close_reason) {
        return failure.with_bootstrap_origin(origin);
    }
    failure
}

#[cfg(feature = "internal-diagnostics")]
fn bootstrap_ack_failure_origin(close_reason: CloseReason) -> Option<BootstrapFailureOrigin> {
    if close_reason == CloseReason::BindingMismatch {
        Some(BootstrapFailureOrigin::AckBindingMismatch)
    } else {
        None
    }
}

fn core_failure(error: apppilotkit_transport_crypto_core::Error) -> TransportFailure {
    core_failure_details(&error)
}

fn core_failure_details(error: &apppilotkit_transport_crypto_core::Error) -> TransportFailure {
    let handoff = error
        .peer_close_details()
        .map_or(HandoffState::NotHandedOff, |(_, handoff)| handoff);
    TransportFailure::new(error.close_reason()).with_handoff(handoff)
}

fn endpoint_matches_platform(platform: Platform, endpoint: &LaunchEndpoint) -> bool {
    match platform {
        Platform::IosSimulator => endpoint.ios_port().is_some(),
        Platform::AndroidEmulator => endpoint.android_name().is_some(),
    }
}

/// Encodes CDDL `launch-descriptor` as RFC 8949 deterministic CBOR.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_launch_descriptor(
    platform: Platform,
    endpoint: &LaunchEndpoint,
    lease_id: [u8; 16],
    target_nonce: [u8; 32],
    app_artifact_digest: [u8; 32],
    broker_static_public: [u8; 32],
    expiry_ms: u64,
    target_reference_digest: [u8; 32],
) -> Result<Vec<u8>, TransportFailure> {
    if expiry_ms == 0 || !endpoint_matches_platform(platform, endpoint) {
        return Err(TransportFailure::new(CloseReason::BindingMismatch));
    }
    let mut out = Vec::with_capacity(180);
    out.push(0xa9); // map(9), keys are written in ascending numeric order.
    cbor_uint(&mut out, 0);
    cbor_uint(&mut out, 1);
    cbor_uint(&mut out, 1);
    cbor_uint(
        &mut out,
        match platform {
            Platform::IosSimulator => 0,
            Platform::AndroidEmulator => 1,
        },
    );
    cbor_uint(&mut out, 2);
    cbor_bytes(&mut out, &lease_id);
    cbor_uint(&mut out, 3);
    cbor_bytes(&mut out, &target_nonce);
    cbor_uint(&mut out, 4);
    cbor_bytes(&mut out, &app_artifact_digest);
    cbor_uint(&mut out, 5);
    cbor_bytes(&mut out, &broker_static_public);
    cbor_uint(&mut out, 6);
    match platform {
        Platform::IosSimulator => {
            out.push(0xa2);
            cbor_uint(&mut out, 0);
            cbor_text(&mut out, "127.0.0.1");
            cbor_uint(&mut out, 1);
            cbor_uint(
                &mut out,
                endpoint.ios_port().expect("validated platform") as u64,
            );
        }
        Platform::AndroidEmulator => {
            out.push(0xa1);
            cbor_uint(&mut out, 0);
            cbor_text(
                &mut out,
                endpoint.android_name().expect("validated platform"),
            );
        }
    }
    cbor_uint(&mut out, 7);
    cbor_uint(&mut out, expiry_ms);
    cbor_uint(&mut out, 8);
    cbor_bytes(&mut out, &target_reference_digest);
    Ok(out)
}

fn cbor_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    cbor_major_len(out, 2, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn cbor_text(out: &mut Vec<u8>, value: &str) {
    cbor_major_len(out, 3, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn cbor_uint(out: &mut Vec<u8>, value: u64) {
    cbor_major_len(out, 0, value);
}

fn cbor_major_len(out: &mut Vec<u8>, major: u8, value: u64) {
    match value {
        0..=23 => out.push((major << 5) | value as u8),
        24..=0xff => out.extend_from_slice(&[(major << 5) | 24, value as u8]),
        0x100..=0xffff => {
            out.push((major << 5) | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push((major << 5) | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push((major << 5) | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };

    use apppilotkit_transport_crypto_core::{
        BrokerStaticKeypair, ProcessBootstrapSecret, TargetBootstrap, TargetSession,
    };

    use super::*;
    use crate::adapter::{CleanupReceipt, PlatformFailure, PlatformFailureKind};

    struct MemoryRaw {
        incoming: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
        outgoing: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
        max_write: usize,
        cancelled: Mutex<bool>,
    }

    impl RawDuplex for MemoryRaw {
        fn read(&self, output: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            let (lock, wake) = &*self.incoming;
            let mut bytes = lock.lock().expect("memory raw input");
            while bytes.is_empty() && !*self.cancelled.lock().expect("memory raw cancellation") {
                bytes = wake.wait(bytes).expect("memory raw wait");
            }
            if bytes.is_empty() {
                return Ok(0);
            }
            let count = output.len().min(bytes.len()).min(7);
            for slot in &mut output[..count] {
                *slot = bytes.pop_front().expect("count is bounded by input");
            }
            Ok(count)
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            let count = input.len().min(self.max_write);
            let (lock, wake) = &*self.outgoing;
            lock.lock()
                .expect("memory raw output")
                .extend(&input[..count]);
            wake.notify_all();
            Ok(count)
        }

        fn cancel(&self) {
            *self.cancelled.lock().expect("memory raw cancellation") = true;
            self.incoming.1.notify_all();
        }
    }

    fn pair() -> (Arc<dyn RawDuplex>, Arc<dyn RawDuplex>) {
        let a_to_b = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let b_to_a = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let a = MemoryRaw {
            incoming: Arc::clone(&b_to_a),
            outgoing: Arc::clone(&a_to_b),
            max_write: 5,
            cancelled: Mutex::new(false),
        };
        let b = MemoryRaw {
            incoming: a_to_b,
            outgoing: b_to_a,
            max_write: 3,
            cancelled: Mutex::new(false),
        };
        (Arc::new(a), Arc::new(b))
    }

    struct NoopConnector;
    impl RawConnector for NoopConnector {
        fn connect(
            &self,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
            Err(PlatformFailure::new(PlatformFailureKind::Unavailable))
        }
    }

    struct QueueConnector {
        raws: Mutex<VecDeque<Arc<dyn RawDuplex>>>,
        connections: AtomicUsize,
    }

    struct CountingConnector {
        connections: AtomicUsize,
    }

    impl RawConnector for CountingConnector {
        fn connect(
            &self,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
            self.connections.fetch_add(1, Ordering::Relaxed);
            Err(PlatformFailure::new(PlatformFailureKind::Unavailable))
        }
    }

    struct PartialTimeoutRaw {
        reads: AtomicUsize,
        writes: AtomicUsize,
    }

    impl RawDuplex for PartialTimeoutRaw {
        fn read(&self, output: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            if self.reads.fetch_add(1, Ordering::Relaxed) == 0 {
                output[0] = 0;
                Ok(1)
            } else {
                Err(PlatformFailure::new(PlatformFailureKind::TimedOut))
            }
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            if self.writes.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(input.len().min(1))
            } else {
                Err(PlatformFailure::new(PlatformFailureKind::TimedOut))
            }
        }

        fn cancel(&self) {}
    }

    struct CloseRecordingRaw {
        written: Mutex<Vec<u8>>,
        max_write: usize,
        fail_write: bool,
    }

    impl RawDuplex for CloseRecordingRaw {
        fn read(&self, _: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            Err(PlatformFailure::new(PlatformFailureKind::Eof))
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            if self.fail_write {
                return Err(PlatformFailure::new(PlatformFailureKind::TimedOut));
            }
            let written = input.len().min(self.max_write);
            self.written
                .lock()
                .expect("recorded close writes")
                .extend_from_slice(&input[..written]);
            Ok(written)
        }

        fn cancel(&self) {}
    }

    struct ScriptedRaw {
        reads: Mutex<VecDeque<Vec<u8>>>,
        read_count: AtomicUsize,
        read_deadlines: Mutex<Vec<u64>>,
    }

    impl ScriptedRaw {
        fn new(reads: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                reads: Mutex::new(reads.into_iter().collect()),
                read_count: AtomicUsize::new(0),
                read_deadlines: Mutex::new(Vec::new()),
            }
        }
    }

    impl RawDuplex for ScriptedRaw {
        fn read(
            &self,
            output: &mut [u8],
            deadline: AbsoluteDeadline,
        ) -> Result<usize, PlatformFailure> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            self.read_deadlines
                .lock()
                .expect("scripted raw deadlines")
                .push(deadline.value());
            let bytes = self
                .reads
                .lock()
                .expect("scripted raw reads")
                .pop_front()
                .unwrap_or_default();
            output[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            Ok(input.len())
        }

        fn cancel(&self) {}
    }

    struct EofRaw {
        read_count: AtomicUsize,
    }

    impl RawDuplex for EofRaw {
        fn read(&self, _: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            Err(PlatformFailure::new(PlatformFailureKind::Eof))
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            Ok(input.len())
        }

        fn cancel(&self) {}
    }

    struct OversizedReadRaw {
        read_count: AtomicUsize,
    }

    impl RawDuplex for OversizedReadRaw {
        fn read(&self, output: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            Ok(output.len() + 1)
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            Ok(input.len())
        }

        fn cancel(&self) {}
    }

    #[derive(Clone, Copy)]
    enum TerminalRead {
        Zero,
        PlatformEof,
    }

    struct FragmentThenTerminalRaw {
        bytes: Mutex<VecDeque<u8>>,
        terminal: TerminalRead,
        read_count: AtomicUsize,
    }

    impl FragmentThenTerminalRaw {
        fn new(bytes: Vec<u8>, terminal: TerminalRead) -> Self {
            Self {
                bytes: Mutex::new(bytes.into_iter().collect()),
                terminal,
                read_count: AtomicUsize::new(0),
            }
        }
    }

    impl RawDuplex for FragmentThenTerminalRaw {
        fn read(&self, output: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            let mut bytes = self.bytes.lock().expect("fragmented raw bytes");
            if bytes.is_empty() {
                return match self.terminal {
                    TerminalRead::Zero => Ok(0),
                    TerminalRead::PlatformEof => {
                        Err(PlatformFailure::new(PlatformFailureKind::Eof))
                    }
                };
            }
            let count = output.len().min(bytes.len());
            for slot in &mut output[..count] {
                *slot = bytes.pop_front().expect("count is bounded by input");
            }
            Ok(count)
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            Ok(input.len())
        }

        fn cancel(&self) {}
    }

    struct CancelledRaw {
        cancelled: AtomicBool,
        read_count: AtomicUsize,
    }

    impl RawDuplex for CancelledRaw {
        fn read(&self, _: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            self.read_count.fetch_add(1, Ordering::Relaxed);
            assert!(self.cancelled.load(Ordering::Relaxed));
            Ok(0)
        }

        fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
            Ok(input.len())
        }

        fn cancel(&self) {
            self.cancelled.store(true, Ordering::Relaxed);
        }
    }

    impl QueueConnector {
        fn new(raws: impl IntoIterator<Item = Arc<dyn RawDuplex>>) -> Self {
            Self {
                raws: Mutex::new(raws.into_iter().collect()),
                connections: AtomicUsize::new(0),
            }
        }
    }

    impl RawConnector for QueueConnector {
        fn connect(
            &self,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
            let raw = self
                .raws
                .lock()
                .expect("connector queue")
                .pop_front()
                .ok_or_else(|| PlatformFailure::new(PlatformFailureKind::Unavailable))?;
            self.connections.fetch_add(1, Ordering::Relaxed);
            Ok(raw)
        }
    }

    struct NoopCleanup;
    impl CleanupReceipt for NoopCleanup {
        fn cleanup(
            self: Box<Self>,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<(), PlatformFailure> {
            Ok(())
        }
    }

    struct FailingCleanup;

    impl CleanupReceipt for FailingCleanup {
        fn cleanup(
            self: Box<Self>,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<(), PlatformFailure> {
            Err(PlatformFailure::new(PlatformFailureKind::CleanupFailed))
        }
    }

    fn binding() -> BootstrapBinding {
        BootstrapBinding {
            target_reference_digest: [0x79; 32],
            lease_id: [0x51; 16],
            target_nonce: [0x71; 32],
            app_artifact_digest: [0x81; 32],
            expiry_ms: 1_893_456_000_000,
        }
    }

    fn deadline() -> AbsoluteDeadline {
        platform_ok(AbsoluteDeadline::new(9_000_000_000_000_000))
    }

    #[test]
    fn platform_failure_mapping_is_closed_and_non_sensitive() {
        let cases = [
            (PlatformFailureKind::TimedOut, CloseReason::Timeout),
            (PlatformFailureKind::Cancelled, CloseReason::Timeout),
            (PlatformFailureKind::Eof, CloseReason::PeerClosed),
            (PlatformFailureKind::Unavailable, CloseReason::InternalError),
            (PlatformFailureKind::Internal, CloseReason::InternalError),
            (PlatformFailureKind::Rejected, CloseReason::BindingMismatch),
            (
                PlatformFailureKind::CleanupFailed,
                CloseReason::CleanupFailed,
            ),
        ];

        for (kind, reason) in cases {
            let failure = platform_failure(PlatformFailure::new(kind));
            assert_eq!(failure.close_reason, reason);
            assert_eq!(
                failure.cleanup_failed,
                kind == PlatformFailureKind::CleanupFailed
            );
        }
    }

    #[cfg(feature = "internal-diagnostics")]
    #[test]
    fn bootstrap_failure_origins_are_closed_to_the_two_probe_sources() {
        assert_eq!(
            platform_failure(PlatformFailure::new(PlatformFailureKind::Rejected)).bootstrap_origin,
            Some(BootstrapFailureOrigin::AdapterRejected)
        );
        assert_eq!(
            bootstrap_ack_failure_origin(CloseReason::BindingMismatch),
            Some(BootstrapFailureOrigin::AckBindingMismatch)
        );
        for close_reason in [CloseReason::AuthenticationFailed, CloseReason::Malformed] {
            assert_eq!(bootstrap_ack_failure_origin(close_reason), None);
        }
    }

    #[test]
    fn phase_deadlines_share_the_earliest_absolute_command_budget() {
        assert_eq!(phase_deadline_value(20_000, 10_000, 4_000), 14_000);
        assert_eq!(phase_deadline_value(12_000, 10_000, 4_000), 12_000);
        assert_eq!(phase_deadline_value(20_000, 10_000, 0), 10_000);
    }

    #[test]
    fn deadline_boundary_is_expired_at_the_exact_millisecond() {
        assert!(!deadline_expired_at(9_999, 10_000));
        assert!(deadline_expired_at(10_000, 10_000));
        assert!(deadline_expired_at(10_001, 10_000));
    }

    #[test]
    fn zero_first_read_is_peer_closed_and_poisoned_reader_stops_io() {
        let raw = ScriptedRaw::new([Vec::new(), vec![0, 3, b'o', b'k', b'!']]);
        let mut reader = RawFrameReader::new();
        let deadline = platform_ok(AbsoluteDeadline::new(10_000));
        let mut moments = VecDeque::from([0_u64, 100, 9_999, 9_999]);
        let failure = reader
            .read_outer_with_now(&raw, deadline, &mut || {
                Ok(moments.pop_front().expect("scripted time"))
            })
            .expect_err("a zero-byte read is EOF, not an idle retry");

        assert_eq!(failure.close_reason, CloseReason::PeerClosed);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            reader
                .read_outer_with_now(&raw, deadline, &mut || -> Result<u64, TransportFailure> {
                    panic!("terminal reader must not consult the clock")
                })
                .expect_err("EOF terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn partial_outer_then_zero_read_is_malformed_and_poisoned_reader_stops_io() {
        let raw = ScriptedRaw::new([vec![0], Vec::new(), vec![0, 3, b'o', b'k', b'!']]);
        let mut reader = RawFrameReader::new();
        let deadline = platform_ok(AbsoluteDeadline::new(10_000));
        let mut moments = VecDeque::from([0_u64, 100, 100, 100, 100]);
        let failure = reader
            .read_outer_with_now(&raw, deadline, &mut || {
                Ok(moments.pop_front().expect("scripted time"))
            })
            .expect_err("truncated outer header followed by EOF is malformed");

        assert_eq!(failure.close_reason, CloseReason::Malformed);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            reader
                .read_outer_with_now(&raw, deadline, &mut || -> Result<u64, TransportFailure> {
                    panic!("terminal reader must not consult the clock")
                })
                .expect_err("malformed EOF terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn platform_eof_is_peer_closed_and_poisoned_reader_stops_io() {
        let raw = EofRaw {
            read_count: AtomicUsize::new(0),
        };
        let mut reader = RawFrameReader::new();
        let failure = reader
            .read_outer(&raw, deadline())
            .expect_err("platform EOF is terminal");

        assert_eq!(failure.close_reason, CloseReason::PeerClosed);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            reader
                .read_outer(&raw, deadline())
                .expect_err("platform EOF terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancelled_raw_zero_read_is_peer_closed_and_poisoned_reader_stops_io() {
        let raw = CancelledRaw {
            cancelled: AtomicBool::new(false),
            read_count: AtomicUsize::new(0),
        };
        raw.cancel();
        let mut reader = RawFrameReader::new();
        let failure = reader
            .read_outer(&raw, deadline())
            .expect_err("cancelled raw EOF is terminal");

        assert_eq!(failure.close_reason, CloseReason::PeerClosed);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            reader
                .read_outer(&raw, deadline())
                .expect_err("cancelled raw EOF terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn session_reader_zero_read_calls_c1_eof_and_poisoned_reader_stops_io() {
        let raw = ScriptedRaw::new([Vec::new(), vec![0, 3, b'o', b'k', b'!']]);
        let pbs = ProcessBootstrapSecret::new([0x51; 32]);
        let mut session = BrokerSession::new(session_binding(), &pbs).expect("broker session");
        let mut reader = RawFrameReader::new();
        let failure = reader
            .read_outer_for_session(&raw, &mut session, deadline())
            .expect_err("session reader maps a zero-byte read through C1 EOF");

        assert_eq!(failure.close_reason, CloseReason::PeerClosed);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            reader
                .read_outer_for_session(&raw, &mut session, deadline())
                .expect_err("session reader EOF terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn oversized_read_count_is_internal_error_and_poisoned_outer_reader_stops_io() {
        let raw = OversizedReadRaw {
            read_count: AtomicUsize::new(0),
        };
        let mut reader = RawFrameReader::new();
        let failure = reader
            .read_outer(&raw, deadline())
            .expect_err("adapter count beyond the supplied buffer must not panic");

        assert_eq!(failure.close_reason, CloseReason::InternalError);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            reader
                .read_outer(&raw, deadline())
                .expect_err("oversized read terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn oversized_read_count_is_internal_error_and_poisoned_session_reader_stops_io() {
        let raw = OversizedReadRaw {
            read_count: AtomicUsize::new(0),
        };
        let pbs = ProcessBootstrapSecret::new([0x51; 32]);
        let mut session = BrokerSession::new(session_binding(), &pbs).expect("broker session");
        let mut reader = RawFrameReader::new();
        let failure = reader
            .read_outer_for_session(&raw, &mut session, deadline())
            .expect_err("adapter count beyond the supplied buffer must not panic");

        assert_eq!(failure.close_reason, CloseReason::InternalError);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            reader
                .read_outer_for_session(&raw, &mut session, deadline())
                .expect_err("oversized session read terminal state is sticky"),
            failure
        );
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn partial_c1_response_then_zero_read_is_malformed_and_poisoned_session_reader_stops_io() {
        assert_partial_c1_response_eof(TerminalRead::Zero);
    }

    #[test]
    fn partial_c1_response_then_platform_eof_is_malformed_and_poisoned_session_reader_stops_io() {
        assert_partial_c1_response_eof(TerminalRead::PlatformEof);
    }

    #[test]
    fn incomplete_outer_frame_times_out_at_two_seconds_and_poisoned_reader_stops_io() {
        let raw = ScriptedRaw::new([vec![0], vec![0, 3, b'o', b'k', b'!']]);
        let mut reader = RawFrameReader::new();
        let deadline = platform_ok(AbsoluteDeadline::new(10_000));
        let mut moments = VecDeque::from([100_u64, 100, 100, 2_100]);
        let failure = reader
            .read_outer_with_now(&raw, deadline, &mut || {
                Ok(moments.pop_front().expect("scripted time"))
            })
            .expect_err("partial outer frame expires exactly two seconds later");

        assert_eq!(failure.close_reason, CloseReason::Timeout);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
        let repeated = reader
            .read_outer_with_now(&raw, deadline, &mut || -> Result<u64, TransportFailure> {
                panic!("terminal reader must not consult the clock")
            })
            .expect_err("terminal failure is sticky");
        assert_eq!(repeated, failure);
        assert_eq!(raw.read_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn partial_read_and_write_loops_preserve_platform_timeout() {
        let raw = PartialTimeoutRaw {
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        };
        let mut reader = RawFrameReader::new();
        let read_failure = reader
            .read_outer(&raw, deadline())
            .expect_err("partial outer frame must not reset its deadline");
        assert_eq!(read_failure.close_reason, CloseReason::Timeout);
        assert_eq!(raw.reads.load(Ordering::Relaxed), 2);

        let write_failure = write_all(&raw, b"ab", deadline())
            .expect_err("partial write must surface the following timeout");
        assert_eq!(write_failure.close_reason, CloseReason::Timeout);
        assert_eq!(raw.writes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reserved_close_write_is_complete_and_never_replaces_the_c1_failure() {
        let raw = CloseRecordingRaw {
            written: Mutex::new(Vec::new()),
            max_write: 2,
            fail_write: false,
        };
        let original = TransportFailure::new(CloseReason::RecordLimit)
            .with_handoff(HandoffState::HandoffPossibleOrConfirmed);

        let returned = return_after_reserved_close(
            &raw,
            original,
            Some(b"authenticated-close".to_vec()),
            deadline(),
        );

        assert_eq!(returned, original);
        assert_eq!(
            *raw.written.lock().expect("recorded close writes"),
            b"authenticated-close"
        );

        let failed_raw = CloseRecordingRaw {
            written: Mutex::new(Vec::new()),
            max_write: 1,
            fail_write: true,
        };
        assert_eq!(
            return_after_reserved_close(
                &failed_raw,
                original,
                Some(b"authenticated-close".to_vec()),
                deadline(),
            ),
            original
        );
        assert!(
            failed_raw
                .written
                .lock()
                .expect("recorded close writes")
                .is_empty()
        );
    }

    #[test]
    fn expired_session_deadline_blocks_connect_before_platform_io() {
        let connector = CountingConnector {
            connections: AtomicUsize::new(0),
        };
        let pbs = ProcessBootstrapSecret::new([0x51; 32]);
        let result = open_session(
            &connector,
            Cancellation::new(),
            session_binding(),
            &pbs,
            b"open",
            platform_ok(AbsoluteDeadline::new(1)),
        );
        let failure = match result {
            Ok(_) => panic!("expired command deadline must fail before connect"),
            Err(failure) => failure,
        };
        assert_eq!(failure.close_reason, CloseReason::Timeout);
        assert_eq!(connector.connections.load(Ordering::Relaxed), 0);
    }

    #[cfg(feature = "internal-diagnostics")]
    #[test]
    fn first_read_outer_without_complete_m1_marks_only_target_no_session_frames() {
        let pbs = ProcessBootstrapSecret::new([0x51; 32]);
        let missing = match open_session(
            &QueueConnector::new([Arc::new(ScriptedRaw::new([vec![0]])) as Arc<dyn RawDuplex>]),
            Cancellation::new(),
            session_binding(),
            &pbs,
            b"open",
            deadline(),
        ) {
            Ok(_) => panic!("an incomplete target M1 must fail"),
            Err(failure) => failure,
        };
        assert_eq!(missing.close_reason, CloseReason::Malformed);
        assert_eq!(
            missing.session_origin,
            Some(SessionFailureOrigin::TargetNoSessionFrames)
        );
    }

    fn platform_ok<T>(result: Result<T, PlatformFailure>) -> T {
        match result {
            Ok(value) => value,
            Err(_) => panic!("valid test platform value"),
        }
    }

    #[test]
    fn canonical_ios_descriptor_matches_checked_in_d0_vector() {
        let encoded = encode_launch_descriptor(
            Platform::IosSimulator,
            &platform_ok(LaunchEndpoint::ios_loopback(55_001)),
            [0x51; 16],
            [0x71; 32],
            [0x81; 32],
            hex32("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13"),
            1_893_456_000_000,
            hex32("791b63ed11406e77475fafbf092c8dc786d728ed0773d7662373741dea079404"),
        )
        .expect("canonical descriptor");
        let vector =
            include_str!("../../../../transport/contracts/v1/vectors/bootstrap-nk-success.json");
        let expected = json_string(vector, "launch_descriptor_cbor_hex");
        assert_eq!(hex(&encoded), expected);
    }

    #[test]
    fn canonical_android_descriptor_matches_checked_in_d0_vector() {
        let encoded = encode_launch_descriptor(
            Platform::AndroidEmulator,
            &platform_ok(LaunchEndpoint::android_local_abstract(
                "apppilotkit-android-bootstrap-0123456789abcdef".to_owned(),
            )),
            [0x51; 16],
            [0x71; 32],
            [0x81; 32],
            hex32("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13"),
            1_893_456_000_000,
            hex32("791b63ed11406e77475fafbf092c8dc786d728ed0773d7662373741dea079404"),
        )
        .expect("canonical descriptor");
        let vector = include_str!(
            "../../../../transport/contracts/v1/vectors/bootstrap-android-descriptor.json"
        );
        let expected = json_string(vector, "launch_descriptor_cbor_hex");
        assert_eq!(hex(&encoded), expected);
    }

    #[test]
    fn bootstrap_completes_real_m1_m2_ack_over_partial_memory_raw() {
        let (broker_raw, target_raw) = pair();
        let keypair = BrokerStaticKeypair::generate().expect("broker keypair");
        let public = keypair.public_key();
        let target_binding = binding();
        let target = thread::spawn(move || {
            let mut initiator =
                TargetBootstrap::new(target_binding, public).expect("target bootstrap");
            write_all(
                target_raw.as_ref(),
                &initiator.write_m1().expect("M1"),
                deadline(),
            )
            .expect("write M1");
            let mut reader = RawFrameReader::new();
            let m2 = reader
                .read_outer(target_raw.as_ref(), deadline())
                .expect("read M2");
            let (sender, _) = initiator.read_m2(&m2, 7, 1).expect("read M2");
            let (ack, _) = sender.write_ack().expect("write ACK");
            write_all(target_raw.as_ref(), &ack, deadline()).expect("write ACK");
        });
        let pbs = ProcessBootstrapSecret::generate().expect("PBS");
        let success = bootstrap(
            broker_raw,
            Arc::new(NoopConnector),
            Box::new(NoopCleanup),
            keypair.into_private_key(),
            &pbs,
            binding(),
            deadline(),
        )
        .expect("bootstrap success");
        assert_eq!(success.ack.process_generation, 7);
        assert_eq!(success.ack.listener_epoch, 1);
        let _ = (
            &success.lease,
            &success.bootstrap,
            &success.connector,
            &success.cleanup,
        );
        target.join().expect("target completes");
    }

    #[test]
    fn wrong_binding_is_rejected_before_m2() {
        let (broker_raw, target_raw) = pair();
        let keypair = BrokerStaticKeypair::generate().expect("broker keypair");
        let public = keypair.public_key();
        let mut wrong = binding();
        wrong.target_nonce[0] ^= 1;
        let target = thread::spawn(move || {
            let mut initiator = TargetBootstrap::new(wrong, public).expect("target bootstrap");
            write_all(
                target_raw.as_ref(),
                &initiator.write_m1().expect("M1"),
                deadline(),
            )
            .expect("write M1");
        });
        let pbs = ProcessBootstrapSecret::generate().expect("PBS");
        let result = bootstrap(
            broker_raw,
            Arc::new(NoopConnector),
            Box::new(NoopCleanup),
            keypair.into_private_key(),
            &pbs,
            binding(),
            deadline(),
        );
        let failure = match result {
            Ok(_) => panic!("wrong binding must fail"),
            Err(failure) => failure,
        };
        assert_eq!(failure.close_reason, CloseReason::AuthenticationFailed);
        assert!(!failure.cleanup_failed);
        target.join().expect("target completes");
    }

    #[test]
    fn wrong_binding_with_failed_cleanup_preserves_authentication_failure() {
        let (broker_raw, target_raw) = pair();
        let keypair = BrokerStaticKeypair::generate().expect("broker keypair");
        let public = keypair.public_key();
        let mut wrong = binding();
        wrong.target_nonce[0] ^= 1;
        let target = thread::spawn(move || {
            let mut initiator = TargetBootstrap::new(wrong, public).expect("target bootstrap");
            write_all(
                target_raw.as_ref(),
                &initiator.write_m1().expect("M1"),
                deadline(),
            )
            .expect("write M1");
        });
        let pbs = ProcessBootstrapSecret::generate().expect("PBS");
        let result = bootstrap(
            broker_raw,
            Arc::new(NoopConnector),
            Box::new(FailingCleanup),
            keypair.into_private_key(),
            &pbs,
            binding(),
            deadline(),
        );
        let failure = match result {
            Ok(_) => panic!("wrong binding must fail even when cleanup fails"),
            Err(failure) => failure,
        };

        assert_eq!(failure.close_reason, CloseReason::AuthenticationFailed);
        assert_eq!(failure.handoff, HandoffState::NotHandedOff);
        assert!(failure.cleanup_failed);
        target.join().expect("target completes");
    }

    fn session_binding() -> SessionBinding {
        SessionBinding {
            lease_id: [0x31; 16],
            process_generation: 7,
            listener_epoch: 1,
            nk_handshake_hash: [0x61; 32],
        }
    }

    fn assert_partial_c1_response_eof(terminal: TerminalRead) {
        let pbs = ProcessBootstrapSecret::new([0x51; 32]);
        let binding = session_binding();
        let mut target = TargetSession::new(binding.clone(), &pbs).expect("target session");
        let mut session = BrokerSession::new(binding, &pbs).expect("broker session");
        let m1 = target.write_m1().expect("target M1");
        let m2 = session.read_m1_write_m2(&m1).expect("broker M2");
        target.read_m2(&m2).expect("target reads M2");
        let target_finished = target.write_finished().expect("target finished");
        session
            .read_finished(&target_finished)
            .expect("broker reads target finished");
        let broker_finished = session.write_finished().expect("broker finished");
        target
            .read_finished(&broker_finished)
            .expect("target reads broker finished");

        let open_frames = session.write_session_open(b"open").expect("open frames");
        for frame in &open_frames {
            let _ = target
                .read_application(frame)
                .expect("target accepts open request");
        }
        let response_frames = target
            .write_application_response(&vec![0x44; 65_508])
            .expect("fragmented open response");
        assert!(
            response_frames.len() > 1,
            "response must have a START without END"
        );

        let raw = FragmentThenTerminalRaw::new(response_frames[0].clone(), terminal);
        let mut reader = RawFrameReader::new();
        let first_outer = reader
            .read_outer_for_session(&raw, &mut session, deadline())
            .expect("first encrypted response fragment");
        assert!(
            session
                .read_application_response(&first_outer)
                .expect("C1 accepts response START")
                .is_none()
        );
        let reads_after_start = raw.read_count.load(Ordering::Relaxed);
        assert!(
            reads_after_start <= 17,
            "first outer frame is bounded by 17 reads"
        );

        let failure = reader
            .read_outer_for_session(&raw, &mut session, deadline())
            .expect_err("C1 reassembly EOF must be malformed");
        assert_eq!(failure.close_reason, CloseReason::Malformed);
        assert_eq!(
            raw.read_count.load(Ordering::Relaxed),
            reads_after_start + 1,
            "exactly one terminal raw read follows the response START"
        );
        assert_eq!(
            reader
                .read_outer_for_session(&raw, &mut session, deadline())
                .expect_err("C1 malformed EOF terminal state is sticky"),
            failure
        );
        assert_eq!(
            raw.read_count.load(Ordering::Relaxed),
            reads_after_start + 1,
            "terminal reader must not issue another read"
        );
    }

    fn spawn_session_peer(raw: Arc<dyn RawDuplex>, label: &'static [u8]) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let peer_pbs = ProcessBootstrapSecret::new([0x51; 32]);
            let mut target =
                TargetSession::new(session_binding(), &peer_pbs).expect("target session");
            let mut reader = RawFrameReader::new();
            write_all(
                raw.as_ref(),
                &target.write_m1().expect("target M1"),
                deadline(),
            )
            .expect("write target M1");
            let m2 = reader
                .read_outer(raw.as_ref(), deadline())
                .expect("broker M2");
            target.read_m2(&m2).expect("read broker M2");
            write_all(
                raw.as_ref(),
                &target.write_finished().expect("target finished"),
                deadline(),
            )
            .expect("write target finished");
            let broker_finished = reader
                .read_outer(raw.as_ref(), deadline())
                .expect("broker finished");
            target
                .read_finished(&broker_finished)
                .expect("read broker finished");

            let opened = read_target_application(raw.as_ref(), &mut reader, &mut target);
            assert_eq!(opened, b"open");
            write_frames(
                raw.as_ref(),
                &target
                    .write_application_response(label)
                    .expect("open response"),
                deadline(),
            )
            .expect("write open response");

            let exchanged = read_target_application(raw.as_ref(), &mut reader, &mut target);
            assert_eq!(exchanged, b"request");
            write_frames(
                raw.as_ref(),
                &target
                    .write_application_response(b"reply")
                    .expect("exchange response"),
                deadline(),
            )
            .expect("write exchange response");

            let close = reader
                .read_outer(raw.as_ref(), deadline())
                .expect("broker close");
            let (reason, handoff) = target.read_close(&close).expect("read close");
            assert_eq!(reason, CloseReason::PeerClosed);
            assert_eq!(handoff, HandoffState::HandoffPossibleOrConfirmed);
        })
    }

    fn read_target_application(
        raw: &dyn RawDuplex,
        reader: &mut RawFrameReader,
        target: &mut TargetSession,
    ) -> Vec<u8> {
        loop {
            let outer = reader
                .read_outer(raw, deadline())
                .expect("broker application frame");
            if let Some(application) = target.read_application(&outer).expect("target application")
            {
                return application;
            }
        }
    }

    #[test]
    fn fresh_raw_session_links_open_exchange_and_close_over_partial_io() {
        let (broker_a, target_a) = pair();
        let (broker_b, target_b) = pair();
        let peer_a = spawn_session_peer(target_a, b"opened-a");
        let peer_b = spawn_session_peer(target_b, b"opened-b");
        let connector = QueueConnector::new([broker_a, broker_b]);
        let pbs = ProcessBootstrapSecret::new([0x51; 32]);

        let mut first = open_session(
            &connector,
            Cancellation::new(),
            session_binding(),
            &pbs,
            b"open",
            deadline(),
        )
        .expect("open first session");
        assert_eq!(first.response, b"opened-a");
        assert_eq!(
            exchange(
                first.raw.as_ref(),
                &mut first.reader,
                &mut first.session,
                b"request",
                deadline()
            )
            .expect("exchange"),
            b"reply"
        );
        close_session(
            first.raw.as_ref(),
            &mut first.session,
            CloseReason::PeerClosed,
            first.handoff,
            deadline(),
        )
        .expect("close first session");

        let mut second = open_session(
            &connector,
            Cancellation::new(),
            session_binding(),
            &pbs,
            b"open",
            deadline(),
        )
        .expect("open second session");
        assert_eq!(second.response, b"opened-b");
        assert_eq!(
            exchange(
                second.raw.as_ref(),
                &mut second.reader,
                &mut second.session,
                b"request",
                deadline()
            )
            .expect("exchange"),
            b"reply"
        );
        close_session(
            second.raw.as_ref(),
            &mut second.session,
            CloseReason::PeerClosed,
            second.handoff,
            deadline(),
        )
        .expect("close second session");

        assert_eq!(connector.connections.load(Ordering::Relaxed), 2);
        peer_a.join().expect("first peer completes");
        peer_b.join().expect("second peer completes");
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn hex32(value: &str) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).expect("hex byte");
        }
        bytes
    }

    fn json_string<'a>(input: &'a str, key: &str) -> &'a str {
        let marker = format!("\"{key}\": \"");
        let start = input.find(&marker).expect("vector key") + marker.len();
        let tail = &input[start..];
        &tail[..tail.find('"').expect("vector string end")]
    }
}
