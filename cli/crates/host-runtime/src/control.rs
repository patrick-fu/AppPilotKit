pub use apppilotkit_transport_crypto_core::{CloseReason, HandoffState};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use minicbor::{Decoder, Encoder};
use sha2::Digest as _;

pub const GLOBAL_CBOR_CAP: usize = 67_109_120;
pub const GLOBAL_PACKET_CAP: usize = 67_109_124;
pub const OPEN_SESSION_CBOR_CAP: usize = 73_728;
pub const EXCHANGE_REQUEST_CBOR_CAP: usize = 16_777_480;
pub const NON_EXCHANGE_CBOR_CAP: usize = 8_192;
pub const READY_REFERENCE_TTL_MS: u64 = 30_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

// These feature-gated strings are the complete private diagnostic vocabulary
// which may cross the frozen failure-message field. They are normalized before
// any public CLI rendering.
#[cfg(feature = "internal-diagnostics")]
pub(crate) const INTERNAL_BOOTSTRAP_ADAPTER_REJECTED: &str = "bootstrap_adapter_rejected";
#[cfg(feature = "internal-diagnostics")]
pub(crate) const INTERNAL_BOOTSTRAP_ACK_BINDING_MISMATCH: &str = "bootstrap_ack_binding_mismatch";
#[cfg(feature = "internal-diagnostics")]
pub(crate) const INTERNAL_TARGET_NO_SESSION_FRAMES: &str = "target_no_session_frames";
#[cfg(feature = "internal-diagnostics")]
pub(crate) const INTERNAL_LEASE_TERMINAL_BEFORE_SESSION_COMMIT: &str =
    "lease_terminal_before_session_commit";

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ReadyReference([u8; 32]);
impl ReadyReference {
    pub const fn from_token(token: [u8; 32]) -> Self {
        Self(token)
    }
    pub const fn token(self) -> [u8; 32] {
        self.0
    }
    pub fn parse(value: &str) -> Result<Self, ControlFailure> {
        let encoded = value
            .strip_prefix("target_")
            .ok_or_else(selection_failure)?;
        if encoded.len() != 43
            || encoded.contains('=')
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(selection_failure());
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| selection_failure())?;
        let token: [u8; 32] = decoded.try_into().map_err(|_| selection_failure())?;
        if URL_SAFE_NO_PAD.encode(token) != encoded {
            return Err(selection_failure());
        }
        Ok(Self(token))
    }
}

fn selection_failure() -> ControlFailure {
    ControlFailure {
        kind: ErrorKind::TargetSelectionRequired,
        message: "Target selection is invalid",
        retryable: false,
        stage: ErrorStage::Ipc,
        handoff: HandoffState::NotHandedOff,
        close_reason: CloseReason::BindingMismatch,
    }
}
impl core::fmt::Display for ReadyReference {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "target_{}", URL_SAFE_NO_PAD.encode(self.0))
    }
}
impl core::fmt::Debug for ReadyReference {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_tuple("ReadyReference")
            .field(&self.to_string())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum Platform {
    IosSimulator = 0,
    AndroidEmulator = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SideEffect {
    ReadOnly = 0,
    LocalWrite = 1,
    AppMutation = 2,
    DeviceMutation = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ErrorKind {
    TargetSelectionRequired = 0,
    SessionExpired = 1,
    TransportAuthenticationRequired = 2,
    Timeout = 3,
    InternalError = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ErrorStage {
    Ipc = 0,
    Prepare = 1,
    Bootstrap = 2,
    SessionHandshake = 3,
    SessionOpen = 4,
    Exchange = 5,
    Close = 6,
    Cleanup = 7,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareBody {
    pub platform: Platform,
    pub device_selector: String,
    pub app_id: String,
    pub app_artifact: String,
    pub app_artifact_sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenSessionBody {
    pub target_token: [u8; 32],
    pub session_id: Option<String>,
    pub required_capabilities: Vec<String>,
    pub session_open_request: Option<Vec<u8>>,
    pub session_open_request_sha256: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExchangeBody {
    pub target_token: [u8; 32],
    pub session_id: String,
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub message: Vec<u8>,
    pub message_sha256: [u8; 32],
    pub side_effect: SideEffect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloseSessionBody {
    pub target_token: [u8; 32],
    pub session_id: String,
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub reason: CloseReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloseLeaseBody {
    pub target_token: [u8; 32],
    pub reason: CloseReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request<T> {
    pub request_id: [u8; 16],
    pub deadline_unix_ms: u64,
    pub body: T,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlRequest {
    Prepare(Request<PrepareBody>),
    OpenSession(Request<OpenSessionBody>),
    Exchange(Request<ExchangeBody>),
    CloseSession(Request<CloseSessionBody>),
    CloseLease(Request<CloseLeaseBody>),
}

impl ControlRequest {
    pub const fn request_id(&self) -> [u8; 16] {
        match self {
            Self::Prepare(request) => request.request_id,
            Self::OpenSession(request) => request.request_id,
            Self::Exchange(request) => request.request_id,
            Self::CloseSession(request) => request.request_id,
            Self::CloseLease(request) => request.request_id,
        }
    }

    pub const fn deadline_unix_ms(&self) -> u64 {
        match self {
            Self::Prepare(request) => request.deadline_unix_ms,
            Self::OpenSession(request) => request.deadline_unix_ms,
            Self::Exchange(request) => request.deadline_unix_ms,
            Self::CloseSession(request) => request.deadline_unix_ms,
            Self::CloseLease(request) => request.deadline_unix_ms,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyTarget {
    pub target_token: [u8; 32],
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOpened {
    pub target_token: [u8; 32],
    pub response: Vec<u8>,
    pub response_sha256: [u8; 32],
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub handoff: HandoffState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExchangeComplete {
    pub target_token: [u8; 32],
    pub session_id: String,
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub message: Vec<u8>,
    pub message_sha256: [u8; 32],
    pub handoff: HandoffState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Closed {
    pub target_token: [u8; 32],
    pub session_id: Option<String>,
    pub handoff: HandoffState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlSuccess {
    TargetReady(ReadyTarget),
    SessionOpened(SessionOpened),
    ExchangeComplete(ExchangeComplete),
    Closed(Closed),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlResult {
    Success {
        request_id: [u8; 16],
        result: ControlSuccess,
    },
    Failure {
        request_id: [u8; 16],
        error: ControlFailure,
    },
}

/// Incremental one-request decoder for one Unix-domain control connection.
pub struct ControlPacketDecoder {
    pending: Vec<u8>,
    expected_total: Option<usize>,
    finished: bool,
}

impl Default for ControlPacketDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlPacketDecoder {
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            expected_total: None,
            finished: false,
        }
    }

    pub fn push(&mut self, input: &[u8]) -> Result<Option<ControlRequest>, ControlFailure> {
        if self.finished {
            return Err(ControlFailure::ipc(CloseReason::SequenceViolation));
        }
        let prospective = self.pending.len().saturating_add(input.len());
        let mut prefix = Vec::with_capacity(64);
        prefix.extend_from_slice(&self.pending[..self.pending.len().min(64)]);
        if prefix.len() < 64 {
            prefix.extend_from_slice(&input[..input.len().min(64 - prefix.len())]);
        }
        if prefix.len() >= 4 {
            let declared = u32::from_be_bytes(prefix[..4].try_into().expect("prefix")) as usize;
            if declared == 0 {
                return self.reject(CloseReason::Malformed);
            }
            if declared > EXCHANGE_REQUEST_CBOR_CAP {
                return self.reject(CloseReason::Oversize);
            }
            self.expected_total = Some(declared + 4);
            if prospective > declared + 4 {
                return self.reject(CloseReason::Malformed);
            }
            match request_operation_prefix(&prefix[4..]) {
                Some(operation) => {
                    let cap = match operation {
                        1 => OPEN_SESSION_CBOR_CAP,
                        2 => EXCHANGE_REQUEST_CBOR_CAP,
                        0 | 3 | 4 => NON_EXCHANGE_CBOR_CAP,
                        _ => return self.reject(CloseReason::Malformed),
                    };
                    if declared > cap {
                        return self.reject(CloseReason::Oversize);
                    }
                }
                None if prefix.len() == 64 => return self.reject(CloseReason::Malformed),
                None => {}
            }
        } else if prospective > 64 {
            return self.reject(CloseReason::Malformed);
        }
        self.pending.extend_from_slice(input);
        let Some(expected) = self.expected_total else {
            return Ok(None);
        };
        if self.pending.len() < expected {
            return Ok(None);
        }
        if self.pending.len() != expected {
            self.finished = true;
            self.pending.clear();
            return Err(ControlFailure::ipc(CloseReason::Malformed));
        }
        self.finished = true;
        decode_request_packet(&self.pending).map(Some)
    }

    fn reject<T>(&mut self, reason: CloseReason) -> Result<T, ControlFailure> {
        self.finished = true;
        self.pending.clear();
        Err(ControlFailure::ipc(reason))
    }

    pub fn timeout(&mut self) -> Result<(), ControlFailure> {
        if self.finished {
            return Err(ControlFailure::ipc(CloseReason::SequenceViolation));
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        self.finished = true;
        self.pending.clear();
        Err(ControlFailure::ipc(CloseReason::Timeout))
    }

    pub fn eof(&mut self) -> Result<(), ControlFailure> {
        if self.finished {
            return Err(ControlFailure::ipc(CloseReason::SequenceViolation));
        }
        self.finished = true;
        if self.pending.is_empty() {
            Ok(())
        } else {
            self.pending.clear();
            Err(ControlFailure::ipc(CloseReason::Malformed))
        }
    }
}

fn request_operation_prefix(cbor: &[u8]) -> Option<u8> {
    let mut decoder = Decoder::new(cbor);
    if decoder.map().ok()? != Some(5)
        || decoder.u8().ok()? != 0
        || decoder.u8().ok()? != 1
        || decoder.u8().ok()? != 1
        || decoder.bytes().ok()?.len() != 16
        || decoder.u8().ok()? != 2
        || decoder.u64().ok().is_none()
        || decoder.u8().ok()? != 3
    {
        return None;
    }
    decoder.u8().ok()
}

#[derive(Clone, Eq, PartialEq)]
pub struct ControlFailure {
    pub kind: ErrorKind,
    pub message: &'static str,
    pub retryable: bool,
    pub stage: ErrorStage,
    pub handoff: HandoffState,
    pub close_reason: CloseReason,
}

impl core::fmt::Debug for ControlFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ControlFailure")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("retryable", &self.retryable)
            .field("stage", &self.stage)
            .field("handoff", &self.handoff)
            .field("close_reason", &self.close_reason)
            .finish()
    }
}

impl core::fmt::Display for ControlFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ControlFailure {}

impl ControlFailure {
    pub const fn ipc(close_reason: CloseReason) -> Self {
        let (kind, message) = match close_reason {
            CloseReason::Timeout => (ErrorKind::Timeout, "Broker control deadline expired"),
            CloseReason::AuthenticationFailed => (
                ErrorKind::TransportAuthenticationRequired,
                "Broker control authentication failed",
            ),
            CloseReason::InternalError | CloseReason::CleanupFailed => {
                (ErrorKind::InternalError, "Broker control failed")
            }
            _ => (ErrorKind::SessionExpired, "Broker control packet rejected"),
        };
        Self {
            kind,
            message,
            retryable: false,
            stage: ErrorStage::Ipc,
            handoff: HandoffState::NotHandedOff,
            close_reason,
        }
    }
}

pub fn decode_request_packet(packet: &[u8]) -> Result<ControlRequest, ControlFailure> {
    let cbor = unframe(packet)?;
    validate_deterministic_cbor(cbor)?;
    let operation = request_operation(cbor)?;
    let cap = match operation {
        1 => OPEN_SESSION_CBOR_CAP,
        2 => EXCHANGE_REQUEST_CBOR_CAP,
        _ => NON_EXCHANGE_CBOR_CAP,
    };
    if cbor.len() > cap {
        return Err(ControlFailure::ipc(CloseReason::Oversize));
    }
    decode_request(cbor, operation)
}

/// Encodes the client half of the frozen Broker control contract.
pub fn encode_request_packet(request: &ControlRequest) -> Result<Vec<u8>, ControlFailure> {
    let (request_id, deadline, operation) = match request {
        ControlRequest::Prepare(value) => (value.request_id, value.deadline_unix_ms, 0),
        ControlRequest::OpenSession(value) => (value.request_id, value.deadline_unix_ms, 1),
        ControlRequest::Exchange(value) => (value.request_id, value.deadline_unix_ms, 2),
        ControlRequest::CloseSession(value) => (value.request_id, value.deadline_unix_ms, 3),
        ControlRequest::CloseLease(value) => (value.request_id, value.deadline_unix_ms, 4),
    };
    let mut cbor = Vec::new();
    {
        let mut encoder = Encoder::new(&mut cbor);
        encoder
            .map(5)
            .map_err(encode_error)?
            .u8(0)
            .map_err(encode_error)?
            .u8(1)
            .map_err(encode_error)?
            .u8(1)
            .map_err(encode_error)?
            .bytes(&request_id)
            .map_err(encode_error)?
            .u8(2)
            .map_err(encode_error)?
            .u64(deadline)
            .map_err(encode_error)?
            .u8(3)
            .map_err(encode_error)?
            .u8(operation)
            .map_err(encode_error)?
            .u8(4)
            .map_err(encode_error)?;
        match request {
            ControlRequest::Prepare(value) => encode_prepare(&mut encoder, &value.body)?,
            ControlRequest::OpenSession(value) => encode_open(&mut encoder, &value.body)?,
            ControlRequest::Exchange(value) => encode_exchange(&mut encoder, &value.body)?,
            ControlRequest::CloseSession(value) => encode_close_session(&mut encoder, &value.body)?,
            ControlRequest::CloseLease(value) => encode_close_lease(&mut encoder, &value.body)?,
        }
    }
    let cap = match request {
        ControlRequest::OpenSession(_) => OPEN_SESSION_CBOR_CAP,
        ControlRequest::Exchange(_) => EXCHANGE_REQUEST_CBOR_CAP,
        _ => NON_EXCHANGE_CBOR_CAP,
    };
    let packet = frame(cbor, cap)?;
    if decode_request_packet(&packet)? != *request {
        return Err(ControlFailure::ipc(CloseReason::InternalError));
    }
    Ok(packet)
}

fn encode_prepare(
    encoder: &mut Encoder<&mut Vec<u8>>,
    body: &PrepareBody,
) -> Result<(), ControlFailure> {
    encoder
        .map(5)
        .map_err(encode_error)?
        .u8(0)
        .map_err(encode_error)?
        .u8(body.platform as u8)
        .map_err(encode_error)?
        .u8(1)
        .map_err(encode_error)?
        .str(&body.device_selector)
        .map_err(encode_error)?
        .u8(2)
        .map_err(encode_error)?
        .str(&body.app_id)
        .map_err(encode_error)?
        .u8(3)
        .map_err(encode_error)?
        .str(&body.app_artifact)
        .map_err(encode_error)?
        .u8(4)
        .map_err(encode_error)?
        .bytes(&body.app_artifact_sha256)
        .map_err(encode_error)?;
    Ok(())
}

fn encode_open(
    encoder: &mut Encoder<&mut Vec<u8>>,
    body: &OpenSessionBody,
) -> Result<(), ControlFailure> {
    encoder
        .map(if body.session_id.is_some() { 3 } else { 4 })
        .map_err(encode_error)?
        .u8(0)
        .map_err(encode_error)?
        .bytes(&body.target_token)
        .map_err(encode_error)?;
    if let Some(session_id) = &body.session_id {
        encoder
            .u8(1)
            .map_err(encode_error)?
            .str(session_id)
            .map_err(encode_error)?;
    }
    encoder
        .u8(2)
        .map_err(encode_error)?
        .array(body.required_capabilities.len() as u64)
        .map_err(encode_error)?;
    for capability in &body.required_capabilities {
        encoder.str(capability).map_err(encode_error)?;
    }
    if body.session_id.is_none() {
        encoder
            .u8(3)
            .map_err(encode_error)?
            .bytes(
                body.session_open_request
                    .as_deref()
                    .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?,
            )
            .map_err(encode_error)?
            .u8(4)
            .map_err(encode_error)?
            .bytes(
                &body
                    .session_open_request_sha256
                    .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?,
            )
            .map_err(encode_error)?;
    }
    Ok(())
}

fn encode_exchange(
    encoder: &mut Encoder<&mut Vec<u8>>,
    body: &ExchangeBody,
) -> Result<(), ControlFailure> {
    encoder
        .map(7)
        .map_err(encode_error)?
        .u8(0)
        .map_err(encode_error)?
        .bytes(&body.target_token)
        .map_err(encode_error)?
        .u8(1)
        .map_err(encode_error)?
        .str(&body.session_id)
        .map_err(encode_error)?
        .u8(2)
        .map_err(encode_error)?
        .u64(body.process_generation)
        .map_err(encode_error)?
        .u8(3)
        .map_err(encode_error)?
        .u64(body.listener_epoch)
        .map_err(encode_error)?
        .u8(4)
        .map_err(encode_error)?
        .bytes(&body.message)
        .map_err(encode_error)?
        .u8(5)
        .map_err(encode_error)?
        .bytes(&body.message_sha256)
        .map_err(encode_error)?
        .u8(6)
        .map_err(encode_error)?
        .u8(body.side_effect as u8)
        .map_err(encode_error)?;
    Ok(())
}

fn encode_close_session(
    encoder: &mut Encoder<&mut Vec<u8>>,
    body: &CloseSessionBody,
) -> Result<(), ControlFailure> {
    encoder
        .map(5)
        .map_err(encode_error)?
        .u8(0)
        .map_err(encode_error)?
        .bytes(&body.target_token)
        .map_err(encode_error)?
        .u8(1)
        .map_err(encode_error)?
        .str(&body.session_id)
        .map_err(encode_error)?
        .u8(2)
        .map_err(encode_error)?
        .u64(body.process_generation)
        .map_err(encode_error)?
        .u8(3)
        .map_err(encode_error)?
        .u64(body.listener_epoch)
        .map_err(encode_error)?
        .u8(4)
        .map_err(encode_error)?
        .u8(body.reason as u8)
        .map_err(encode_error)?;
    Ok(())
}

fn encode_close_lease(
    encoder: &mut Encoder<&mut Vec<u8>>,
    body: &CloseLeaseBody,
) -> Result<(), ControlFailure> {
    encoder
        .map(2)
        .map_err(encode_error)?
        .u8(0)
        .map_err(encode_error)?
        .bytes(&body.target_token)
        .map_err(encode_error)?
        .u8(1)
        .map_err(encode_error)?
        .u8(body.reason as u8)
        .map_err(encode_error)?;
    Ok(())
}

pub fn decode_result_packet(packet: &[u8]) -> Result<ControlResult, ControlFailure> {
    let cbor = unframe(packet)?;
    validate_deterministic_cbor(cbor)?;
    let mut decoder = Decoder::new(cbor);
    require_map(&mut decoder, 4)?;
    require_key(&mut decoder, 0)?;
    if decoder.u8().map_err(decode_error)? != 1 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(&mut decoder, 1)?;
    let request_id = fixed(decoder.bytes().map_err(decode_error)?)?;
    require_key(&mut decoder, 2)?;
    let succeeded = decoder.bool().map_err(decode_error)?;
    require_key(&mut decoder, 3)?;
    let result = if succeeded {
        ControlResult::Success {
            request_id,
            result: decode_success_body(&mut decoder)?,
        }
    } else {
        ControlResult::Failure {
            request_id,
            error: decode_failure_body(&mut decoder)?,
        }
    };
    if decoder.position() != cbor.len() {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(result)
}

fn decode_success_body(decoder: &mut Decoder<'_>) -> Result<ControlSuccess, ControlFailure> {
    let count = decoder
        .map()
        .map_err(decode_error)?
        .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?;
    require_key(decoder, 0)?;
    let kind = decoder.u8().map_err(decode_error)?;
    match kind {
        0 if count == 6 => {
            require_key(decoder, 1)?;
            let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
            require_key(decoder, 3)?;
            let process_generation = decoder.u64().map_err(decode_error)?;
            require_safe_integer(process_generation)?;
            require_key(decoder, 4)?;
            let listener_epoch = decoder.u64().map_err(decode_error)?;
            require_safe_integer(listener_epoch)?;
            require_key(decoder, 5)?;
            let issued_at_unix_ms = decoder.u64().map_err(decode_error)?;
            require_safe_integer(issued_at_unix_ms)?;
            require_key(decoder, 6)?;
            let expires_at_unix_ms = decoder.u64().map_err(decode_error)?;
            require_safe_integer(expires_at_unix_ms)?;
            if issued_at_unix_ms.checked_add(READY_REFERENCE_TTL_MS) != Some(expires_at_unix_ms) {
                return Err(ControlFailure::ipc(CloseReason::InternalError));
            }
            Ok(ControlSuccess::TargetReady(ReadyTarget {
                target_token,
                process_generation,
                listener_epoch,
                issued_at_unix_ms,
                expires_at_unix_ms,
            }))
        }
        1 if count == 7 => {
            require_key(decoder, 1)?;
            let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
            require_key(decoder, 2)?;
            let response = decoder.bytes().map_err(decode_error)?.to_vec();
            if response.is_empty() || response.len() > 65_536 {
                return Err(ControlFailure::ipc(CloseReason::Oversize));
            }
            require_key(decoder, 3)?;
            let response_sha256 = fixed(decoder.bytes().map_err(decode_error)?)?;
            if response_sha256 != <[u8; 32]>::from(sha2::Sha256::digest(&response)) {
                return Err(ControlFailure::ipc(CloseReason::BindingMismatch));
            }
            require_key(decoder, 4)?;
            let process_generation = decoder.u64().map_err(decode_error)?;
            require_safe_integer(process_generation)?;
            require_key(decoder, 5)?;
            let listener_epoch = decoder.u64().map_err(decode_error)?;
            require_safe_integer(listener_epoch)?;
            require_key(decoder, 6)?;
            let handoff = decode_handoff(decoder.u8().map_err(decode_error)?)?;
            Ok(ControlSuccess::SessionOpened(SessionOpened {
                target_token,
                response,
                response_sha256,
                process_generation,
                listener_epoch,
                handoff,
            }))
        }
        2 if count == 8 => {
            require_key(decoder, 1)?;
            let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
            require_key(decoder, 2)?;
            let session_id = decoder.str().map_err(decode_error)?.to_owned();
            require_session_id(&session_id)?;
            require_key(decoder, 3)?;
            let process_generation = decoder.u64().map_err(decode_error)?;
            require_safe_integer(process_generation)?;
            require_key(decoder, 4)?;
            let listener_epoch = decoder.u64().map_err(decode_error)?;
            require_safe_integer(listener_epoch)?;
            require_key(decoder, 5)?;
            let message = decoder.bytes().map_err(decode_error)?.to_vec();
            if message.is_empty() || message.len() > 67_108_864 {
                return Err(ControlFailure::ipc(CloseReason::Oversize));
            }
            require_key(decoder, 6)?;
            let message_sha256 = fixed(decoder.bytes().map_err(decode_error)?)?;
            if message_sha256 != <[u8; 32]>::from(sha2::Sha256::digest(&message)) {
                return Err(ControlFailure::ipc(CloseReason::BindingMismatch));
            }
            require_key(decoder, 7)?;
            let handoff = decode_handoff(decoder.u8().map_err(decode_error)?)?;
            Ok(ControlSuccess::ExchangeComplete(ExchangeComplete {
                target_token,
                session_id,
                process_generation,
                listener_epoch,
                message,
                message_sha256,
                handoff,
            }))
        }
        3 if matches!(count, 3 | 4) => {
            require_key(decoder, 1)?;
            let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
            let mut key = decoder.u8().map_err(decode_error)?;
            let session_id = if key == 2 {
                let value = decoder.str().map_err(decode_error)?.to_owned();
                require_session_id(&value)?;
                key = decoder.u8().map_err(decode_error)?;
                Some(value)
            } else {
                None
            };
            if key != 7 {
                return Err(ControlFailure::ipc(CloseReason::Malformed));
            }
            let handoff = decode_handoff(decoder.u8().map_err(decode_error)?)?;
            Ok(ControlSuccess::Closed(Closed {
                target_token,
                session_id,
                handoff,
            }))
        }
        _ => Err(ControlFailure::ipc(CloseReason::Malformed)),
    }
}

fn decode_failure_body(decoder: &mut Decoder<'_>) -> Result<ControlFailure, ControlFailure> {
    require_map(decoder, 6)?;
    require_key(decoder, 0)?;
    let kind = match decoder.u8().map_err(decode_error)? {
        0 => ErrorKind::TargetSelectionRequired,
        1 => ErrorKind::SessionExpired,
        2 => ErrorKind::TransportAuthenticationRequired,
        3 => ErrorKind::Timeout,
        4 => ErrorKind::InternalError,
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    require_key(decoder, 1)?;
    let message = decoder.str().map_err(decode_error)?;
    let stock_message = match message {
        "expired" => "expired",
        "Broker control deadline expired" => "Broker control deadline expired",
        "Broker control authentication failed" => "Broker control authentication failed",
        "Broker control failed" => "Broker control failed",
        "Broker control packet rejected" => "Broker control packet rejected",
        "Target selection is invalid" => "Target selection is invalid",
        "Target session expired" => "Target session expired",
        "Target transport authentication failed" => "Target transport authentication failed",
        "Broker operation timed out" => "Broker operation timed out",
        "Broker operation failed" => "Broker operation failed",
        #[cfg(feature = "internal-diagnostics")]
        INTERNAL_BOOTSTRAP_ADAPTER_REJECTED => INTERNAL_BOOTSTRAP_ADAPTER_REJECTED,
        #[cfg(feature = "internal-diagnostics")]
        INTERNAL_BOOTSTRAP_ACK_BINDING_MISMATCH => INTERNAL_BOOTSTRAP_ACK_BINDING_MISMATCH,
        #[cfg(feature = "internal-diagnostics")]
        INTERNAL_TARGET_NO_SESSION_FRAMES => INTERNAL_TARGET_NO_SESSION_FRAMES,
        #[cfg(feature = "internal-diagnostics")]
        INTERNAL_LEASE_TERMINAL_BEFORE_SESSION_COMMIT => {
            INTERNAL_LEASE_TERMINAL_BEFORE_SESSION_COMMIT
        }
        _ if !message.is_empty()
            && message.len() <= 256
            && !message
                .bytes()
                .any(|byte| matches!(byte, 0 | b'\r' | b'\n')) =>
        {
            "Peer returned a safe Broker failure"
        }
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    require_key(decoder, 2)?;
    let retryable = decoder.bool().map_err(decode_error)?;
    require_key(decoder, 3)?;
    let stage = match decoder.u8().map_err(decode_error)? {
        0 => ErrorStage::Ipc,
        1 => ErrorStage::Prepare,
        2 => ErrorStage::Bootstrap,
        3 => ErrorStage::SessionHandshake,
        4 => ErrorStage::SessionOpen,
        5 => ErrorStage::Exchange,
        6 => ErrorStage::Close,
        7 => ErrorStage::Cleanup,
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    require_key(decoder, 4)?;
    let handoff = decode_handoff(decoder.u8().map_err(decode_error)?)?;
    require_key(decoder, 5)?;
    let close_reason = close_reason(decoder.u8().map_err(decode_error)?)?;
    Ok(ControlFailure {
        kind,
        message: stock_message,
        retryable,
        stage,
        handoff,
        close_reason,
    })
}

fn decode_handoff(value: u8) -> Result<HandoffState, ControlFailure> {
    match value {
        0 => Ok(HandoffState::NotHandedOff),
        1 => Ok(HandoffState::HandoffPossibleOrConfirmed),
        _ => Err(ControlFailure::ipc(CloseReason::Malformed)),
    }
}

pub fn encode_success_packet(
    request_id: [u8; 16],
    success: ControlSuccess,
) -> Result<Vec<u8>, ControlFailure> {
    validate_success(&success)?;
    let mut cbor = Vec::new();
    {
        let mut encoder = Encoder::new(&mut cbor);
        encoder.map(4).map_err(encode_error)?;
        encoder
            .u8(0)
            .map_err(encode_error)?
            .u8(1)
            .map_err(encode_error)?;
        encoder
            .u8(1)
            .map_err(encode_error)?
            .bytes(&request_id)
            .map_err(encode_error)?;
        encoder
            .u8(2)
            .map_err(encode_error)?
            .bool(true)
            .map_err(encode_error)?;
        encoder.u8(3).map_err(encode_error)?;
        encode_success_body(&mut encoder, &success)?;
    }
    let cap = match success {
        ControlSuccess::SessionOpened(_) => OPEN_SESSION_CBOR_CAP,
        ControlSuccess::ExchangeComplete(_) => GLOBAL_CBOR_CAP,
        _ => NON_EXCHANGE_CBOR_CAP,
    };
    let packet = frame(cbor, cap)?;
    if decode_result_packet(&packet)?
        != (ControlResult::Success {
            request_id,
            result: success,
        })
    {
        return Err(ControlFailure::ipc(CloseReason::InternalError));
    }
    Ok(packet)
}

fn validate_success(success: &ControlSuccess) -> Result<(), ControlFailure> {
    match success {
        ControlSuccess::TargetReady(ready) => {
            require_safe_integer(ready.process_generation)?;
            require_safe_integer(ready.listener_epoch)?;
            require_safe_integer(ready.issued_at_unix_ms)?;
            require_safe_integer(ready.expires_at_unix_ms)?;
            if ready.issued_at_unix_ms.checked_add(READY_REFERENCE_TTL_MS)
                != Some(ready.expires_at_unix_ms)
            {
                return Err(ControlFailure::ipc(CloseReason::BindingMismatch));
            }
        }
        ControlSuccess::SessionOpened(opened) => {
            if opened.response.is_empty() || opened.response.len() > 65_536 {
                return Err(ControlFailure::ipc(CloseReason::Oversize));
            }
            if opened.response_sha256 != <[u8; 32]>::from(sha2::Sha256::digest(&opened.response)) {
                return Err(ControlFailure::ipc(CloseReason::BindingMismatch));
            }
            require_safe_integer(opened.process_generation)?;
            require_safe_integer(opened.listener_epoch)?;
        }
        ControlSuccess::ExchangeComplete(exchange) => {
            require_session_id(&exchange.session_id)?;
            require_safe_integer(exchange.process_generation)?;
            require_safe_integer(exchange.listener_epoch)?;
            if exchange.message.is_empty() || exchange.message.len() > 67_108_864 {
                return Err(ControlFailure::ipc(CloseReason::Oversize));
            }
            if exchange.message_sha256 != <[u8; 32]>::from(sha2::Sha256::digest(&exchange.message))
            {
                return Err(ControlFailure::ipc(CloseReason::BindingMismatch));
            }
        }
        ControlSuccess::Closed(closed) => {
            if let Some(session_id) = &closed.session_id {
                require_session_id(session_id)?;
            }
        }
    }
    Ok(())
}

pub fn encode_failure_packet(
    request_id: [u8; 16],
    failure: &ControlFailure,
) -> Result<Vec<u8>, ControlFailure> {
    if failure.message.is_empty()
        || failure.message.len() > 256
        || failure
            .message
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
    {
        return Err(ControlFailure::ipc(CloseReason::InternalError));
    }
    let mut cbor = Vec::new();
    let mut encoder = Encoder::new(&mut cbor);
    encoder.map(4).map_err(encode_error)?;
    encoder
        .u8(0)
        .map_err(encode_error)?
        .u8(1)
        .map_err(encode_error)?;
    encoder
        .u8(1)
        .map_err(encode_error)?
        .bytes(&request_id)
        .map_err(encode_error)?;
    encoder
        .u8(2)
        .map_err(encode_error)?
        .bool(false)
        .map_err(encode_error)?;
    encoder
        .u8(3)
        .map_err(encode_error)?
        .map(6)
        .map_err(encode_error)?;
    encoder
        .u8(0)
        .map_err(encode_error)?
        .u8(failure.kind as u8)
        .map_err(encode_error)?;
    encoder
        .u8(1)
        .map_err(encode_error)?
        .str(failure.message)
        .map_err(encode_error)?;
    encoder
        .u8(2)
        .map_err(encode_error)?
        .bool(failure.retryable)
        .map_err(encode_error)?;
    encoder
        .u8(3)
        .map_err(encode_error)?
        .u8(failure.stage as u8)
        .map_err(encode_error)?;
    encoder
        .u8(4)
        .map_err(encode_error)?
        .u8(failure.handoff as u8)
        .map_err(encode_error)?;
    encoder
        .u8(5)
        .map_err(encode_error)?
        .u8(failure.close_reason as u8)
        .map_err(encode_error)?;
    frame(cbor, NON_EXCHANGE_CBOR_CAP)
}

fn encode_success_body(
    encoder: &mut Encoder<&mut Vec<u8>>,
    success: &ControlSuccess,
) -> Result<(), ControlFailure> {
    match success {
        ControlSuccess::TargetReady(ready) => {
            encoder.map(6).map_err(encode_error)?;
            encoder
                .u8(0)
                .map_err(encode_error)?
                .u8(0)
                .map_err(encode_error)?;
            encoder
                .u8(1)
                .map_err(encode_error)?
                .bytes(&ready.target_token)
                .map_err(encode_error)?;
            encoder
                .u8(3)
                .map_err(encode_error)?
                .u64(ready.process_generation)
                .map_err(encode_error)?;
            encoder
                .u8(4)
                .map_err(encode_error)?
                .u64(ready.listener_epoch)
                .map_err(encode_error)?;
            encoder
                .u8(5)
                .map_err(encode_error)?
                .u64(ready.issued_at_unix_ms)
                .map_err(encode_error)?;
            encoder
                .u8(6)
                .map_err(encode_error)?
                .u64(ready.expires_at_unix_ms)
                .map_err(encode_error)?;
        }
        ControlSuccess::SessionOpened(opened) => {
            encoder.map(7).map_err(encode_error)?;
            encoder
                .u8(0)
                .map_err(encode_error)?
                .u8(1)
                .map_err(encode_error)?;
            encoder
                .u8(1)
                .map_err(encode_error)?
                .bytes(&opened.target_token)
                .map_err(encode_error)?;
            encoder
                .u8(2)
                .map_err(encode_error)?
                .bytes(&opened.response)
                .map_err(encode_error)?;
            encoder
                .u8(3)
                .map_err(encode_error)?
                .bytes(&opened.response_sha256)
                .map_err(encode_error)?;
            encoder
                .u8(4)
                .map_err(encode_error)?
                .u64(opened.process_generation)
                .map_err(encode_error)?;
            encoder
                .u8(5)
                .map_err(encode_error)?
                .u64(opened.listener_epoch)
                .map_err(encode_error)?;
            encoder
                .u8(6)
                .map_err(encode_error)?
                .u8(opened.handoff as u8)
                .map_err(encode_error)?;
        }
        ControlSuccess::ExchangeComplete(exchange) => {
            encoder.map(8).map_err(encode_error)?;
            encoder
                .u8(0)
                .map_err(encode_error)?
                .u8(2)
                .map_err(encode_error)?;
            encoder
                .u8(1)
                .map_err(encode_error)?
                .bytes(&exchange.target_token)
                .map_err(encode_error)?;
            encoder
                .u8(2)
                .map_err(encode_error)?
                .str(&exchange.session_id)
                .map_err(encode_error)?;
            encoder
                .u8(3)
                .map_err(encode_error)?
                .u64(exchange.process_generation)
                .map_err(encode_error)?;
            encoder
                .u8(4)
                .map_err(encode_error)?
                .u64(exchange.listener_epoch)
                .map_err(encode_error)?;
            encoder
                .u8(5)
                .map_err(encode_error)?
                .bytes(&exchange.message)
                .map_err(encode_error)?;
            encoder
                .u8(6)
                .map_err(encode_error)?
                .bytes(&exchange.message_sha256)
                .map_err(encode_error)?;
            encoder
                .u8(7)
                .map_err(encode_error)?
                .u8(exchange.handoff as u8)
                .map_err(encode_error)?;
        }
        ControlSuccess::Closed(closed) => {
            encoder
                .map(if closed.session_id.is_some() { 4 } else { 3 })
                .map_err(encode_error)?;
            encoder
                .u8(0)
                .map_err(encode_error)?
                .u8(3)
                .map_err(encode_error)?;
            encoder
                .u8(1)
                .map_err(encode_error)?
                .bytes(&closed.target_token)
                .map_err(encode_error)?;
            if let Some(session_id) = &closed.session_id {
                encoder
                    .u8(2)
                    .map_err(encode_error)?
                    .str(session_id)
                    .map_err(encode_error)?;
            }
            encoder
                .u8(7)
                .map_err(encode_error)?
                .u8(closed.handoff as u8)
                .map_err(encode_error)?;
        }
    }
    Ok(())
}

fn unframe(packet: &[u8]) -> Result<&[u8], ControlFailure> {
    if packet.len() < 4 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    let declared = u32::from_be_bytes(packet[..4].try_into().expect("prefix length")) as usize;
    if declared == 0 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    if declared > GLOBAL_CBOR_CAP
        || declared
            .checked_add(4)
            .is_none_or(|n| n > GLOBAL_PACKET_CAP)
    {
        return Err(ControlFailure::ipc(CloseReason::Oversize));
    }
    if packet.len() != declared + 4 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(&packet[4..])
}

fn frame(cbor: Vec<u8>, cap: usize) -> Result<Vec<u8>, ControlFailure> {
    if cbor.is_empty() {
        return Err(ControlFailure::ipc(CloseReason::InternalError));
    }
    if cbor.len() > cap || cbor.len() > GLOBAL_CBOR_CAP {
        return Err(ControlFailure::ipc(CloseReason::Oversize));
    }
    let mut packet = Vec::with_capacity(cbor.len() + 4);
    packet.extend_from_slice(&(cbor.len() as u32).to_be_bytes());
    packet.extend_from_slice(&cbor);
    Ok(packet)
}

fn request_operation(cbor: &[u8]) -> Result<u8, ControlFailure> {
    let mut decoder = Decoder::new(cbor);
    require_map(&mut decoder, 5)?;
    require_key(&mut decoder, 0)?;
    if decoder.u8().map_err(decode_error)? != 1 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(&mut decoder, 1)?;
    let request_id = decoder.bytes().map_err(decode_error)?;
    if request_id.len() != 16 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(&mut decoder, 2)?;
    require_safe_integer(decoder.u64().map_err(decode_error)?)?;
    require_key(&mut decoder, 3)?;
    let operation = decoder.u8().map_err(decode_error)?;
    if operation > 4 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(operation)
}

fn decode_request(cbor: &[u8], operation: u8) -> Result<ControlRequest, ControlFailure> {
    let mut decoder = Decoder::new(cbor);
    require_map(&mut decoder, 5)?;
    require_key(&mut decoder, 0)?;
    if decoder.u8().map_err(decode_error)? != 1 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(&mut decoder, 1)?;
    let request_id = fixed::<16>(decoder.bytes().map_err(decode_error)?)?;
    require_key(&mut decoder, 2)?;
    let deadline_unix_ms = decoder.u64().map_err(decode_error)?;
    require_safe_integer(deadline_unix_ms)?;
    require_key(&mut decoder, 3)?;
    if decoder.u8().map_err(decode_error)? != operation {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(&mut decoder, 4)?;
    let request = match operation {
        0 => ControlRequest::Prepare(Request {
            request_id,
            deadline_unix_ms,
            body: decode_prepare(&mut decoder)?,
        }),
        1 => ControlRequest::OpenSession(Request {
            request_id,
            deadline_unix_ms,
            body: decode_open(&mut decoder)?,
        }),
        2 => ControlRequest::Exchange(Request {
            request_id,
            deadline_unix_ms,
            body: decode_exchange(&mut decoder)?,
        }),
        3 => ControlRequest::CloseSession(Request {
            request_id,
            deadline_unix_ms,
            body: decode_close_session(&mut decoder)?,
        }),
        4 => ControlRequest::CloseLease(Request {
            request_id,
            deadline_unix_ms,
            body: decode_close_lease(&mut decoder)?,
        }),
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    if decoder.position() != cbor.len() {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(request)
}

fn decode_prepare(decoder: &mut Decoder<'_>) -> Result<PrepareBody, ControlFailure> {
    require_map(decoder, 5)?;
    require_key(decoder, 0)?;
    let platform = match decoder.u8().map_err(decode_error)? {
        0 => Platform::IosSimulator,
        1 => Platform::AndroidEmulator,
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    require_key(decoder, 1)?;
    let device_selector = decoder.str().map_err(decode_error)?.to_owned();
    if !(1..=256).contains(&device_selector.len())
        || !device_selector.bytes().all(valid_selector_byte)
    {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(decoder, 2)?;
    let app_id = decoder.str().map_err(decode_error)?.to_owned();
    if !valid_app_id(&app_id) {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(decoder, 3)?;
    let app_artifact = decoder.str().map_err(decode_error)?.to_owned();
    if app_artifact.len() > 4096
        || !app_artifact.starts_with('/')
        || app_artifact.len() < 2
        || app_artifact
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
    {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(decoder, 4)?;
    Ok(PrepareBody {
        platform,
        device_selector,
        app_id,
        app_artifact,
        app_artifact_sha256: fixed(decoder.bytes().map_err(decode_error)?)?,
    })
}

fn decode_open(decoder: &mut Decoder<'_>) -> Result<OpenSessionBody, ControlFailure> {
    let count = decoder
        .map()
        .map_err(decode_error)?
        .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?;
    if !matches!(count, 3 | 4) {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    require_key(decoder, 0)?;
    let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
    let mut next_key = decoder.u8().map_err(decode_error)?;
    let session_id = if next_key == 1 {
        let session = decoder.str().map_err(decode_error)?.to_owned();
        require_session_id(&session)?;
        next_key = decoder.u8().map_err(decode_error)?;
        Some(session)
    } else {
        None
    };
    if next_key != 2 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    let required_capabilities = decode_capabilities(decoder)?;
    let (session_open_request, session_open_request_sha256) = if session_id.is_none() {
        require_key(decoder, 3)?;
        let request = decoder.bytes().map_err(decode_error)?.to_vec();
        if request.is_empty() || request.len() > 65_536 {
            return Err(ControlFailure::ipc(CloseReason::Oversize));
        }
        require_key(decoder, 4)?;
        (
            Some(request),
            Some(fixed(decoder.bytes().map_err(decode_error)?)?),
        )
    } else {
        (None, None)
    };
    Ok(OpenSessionBody {
        target_token,
        session_id,
        required_capabilities,
        session_open_request,
        session_open_request_sha256,
    })
}

fn decode_exchange(decoder: &mut Decoder<'_>) -> Result<ExchangeBody, ControlFailure> {
    require_map(decoder, 7)?;
    require_key(decoder, 0)?;
    let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
    require_key(decoder, 1)?;
    let session_id = decoder.str().map_err(decode_error)?.to_owned();
    require_session_id(&session_id)?;
    require_key(decoder, 2)?;
    let process_generation = decoder.u64().map_err(decode_error)?;
    require_safe_integer(process_generation)?;
    require_key(decoder, 3)?;
    let listener_epoch = decoder.u64().map_err(decode_error)?;
    require_safe_integer(listener_epoch)?;
    require_key(decoder, 4)?;
    let message = decoder.bytes().map_err(decode_error)?.to_vec();
    if message.is_empty() || message.len() > 16_777_216 {
        return Err(ControlFailure::ipc(CloseReason::Oversize));
    }
    require_key(decoder, 5)?;
    let message_sha256 = fixed(decoder.bytes().map_err(decode_error)?)?;
    require_key(decoder, 6)?;
    let side_effect = match decoder.u8().map_err(decode_error)? {
        0 => SideEffect::ReadOnly,
        1 => SideEffect::LocalWrite,
        2 => SideEffect::AppMutation,
        3 => SideEffect::DeviceMutation,
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    Ok(ExchangeBody {
        target_token,
        session_id,
        process_generation,
        listener_epoch,
        message,
        message_sha256,
        side_effect,
    })
}

fn decode_close_session(decoder: &mut Decoder<'_>) -> Result<CloseSessionBody, ControlFailure> {
    require_map(decoder, 5)?;
    require_key(decoder, 0)?;
    let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
    require_key(decoder, 1)?;
    let session_id = decoder.str().map_err(decode_error)?.to_owned();
    require_session_id(&session_id)?;
    require_key(decoder, 2)?;
    let process_generation = decoder.u64().map_err(decode_error)?;
    require_safe_integer(process_generation)?;
    require_key(decoder, 3)?;
    let listener_epoch = decoder.u64().map_err(decode_error)?;
    require_safe_integer(listener_epoch)?;
    require_key(decoder, 4)?;
    let reason = close_reason(decoder.u8().map_err(decode_error)?)?;
    Ok(CloseSessionBody {
        target_token,
        session_id,
        process_generation,
        listener_epoch,
        reason,
    })
}

fn decode_close_lease(decoder: &mut Decoder<'_>) -> Result<CloseLeaseBody, ControlFailure> {
    require_map(decoder, 2)?;
    require_key(decoder, 0)?;
    let target_token = fixed(decoder.bytes().map_err(decode_error)?)?;
    require_key(decoder, 1)?;
    let reason = close_reason(decoder.u8().map_err(decode_error)?)?;
    Ok(CloseLeaseBody {
        target_token,
        reason,
    })
}

fn decode_capabilities(decoder: &mut Decoder<'_>) -> Result<Vec<String>, ControlFailure> {
    let count = decoder
        .array()
        .map_err(decode_error)?
        .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?;
    if !(1..=32).contains(&count) {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    let mut capabilities: Vec<String> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let capability = decoder.str().map_err(decode_error)?.to_owned();
        if !valid_capability(&capability)
            || capabilities
                .last()
                .is_some_and(|previous| previous.as_bytes() >= capability.as_bytes())
        {
            return Err(ControlFailure::ipc(CloseReason::Malformed));
        }
        capabilities.push(capability);
    }
    Ok(capabilities)
}

fn require_map(decoder: &mut Decoder<'_>, expected: u64) -> Result<(), ControlFailure> {
    if decoder.map().map_err(decode_error)? != Some(expected) {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(())
}

fn require_key(decoder: &mut Decoder<'_>, key: u8) -> Result<(), ControlFailure> {
    if decoder.u8().map_err(decode_error)? != key {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(())
}

fn require_safe_integer(value: u64) -> Result<(), ControlFailure> {
    if !(1..=MAX_SAFE_INTEGER).contains(&value) {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(())
}

fn fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N], ControlFailure> {
    bytes
        .try_into()
        .map_err(|_| ControlFailure::ipc(CloseReason::Malformed))
}

fn require_session_id(value: &str) -> Result<(), ControlFailure> {
    if !(16..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'))
    {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(())
}

fn valid_selector_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
}

fn valid_app_id(value: &str) -> bool {
    (3..=255).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn valid_capability(value: &str) -> bool {
    (3..=128).contains(&value.len())
        && value.split('.').count() >= 2
        && value.split('.').all(|part| {
            part.len() >= 2
                && part.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn close_reason(value: u8) -> Result<CloseReason, ControlFailure> {
    Ok(match value {
        0 => CloseReason::Normal,
        1 => CloseReason::AuthenticationFailed,
        2 => CloseReason::BindingMismatch,
        3 => CloseReason::Stale,
        4 => CloseReason::Timeout,
        5 => CloseReason::Oversize,
        6 => CloseReason::Malformed,
        7 => CloseReason::SequenceViolation,
        8 => CloseReason::RecordLimit,
        9 => CloseReason::PeerClosed,
        10 => CloseReason::BrokerLost,
        11 => CloseReason::EligibilityLost,
        12 => CloseReason::CleanupFailed,
        13 => CloseReason::InternalError,
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    })
}

fn decode_error(_: minicbor::decode::Error) -> ControlFailure {
    ControlFailure::ipc(CloseReason::Malformed)
}

fn encode_error(_: minicbor::encode::Error<std::convert::Infallible>) -> ControlFailure {
    ControlFailure::ipc(CloseReason::InternalError)
}

fn validate_deterministic_cbor(bytes: &[u8]) -> Result<(), ControlFailure> {
    let mut offset = 0;
    validate_item(bytes, &mut offset, 0)?;
    if offset != bytes.len() {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(())
}

fn validate_item(bytes: &[u8], offset: &mut usize, depth: u8) -> Result<(), ControlFailure> {
    if depth >= 64 {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    let initial = take(bytes, offset)?;
    let major = initial >> 5;
    let info = initial & 31;
    match major {
        0 => {
            read_argument(bytes, offset, info)?;
        }
        2 | 3 => {
            let length = read_argument(bytes, offset, info)?;
            let length =
                usize::try_from(length).map_err(|_| ControlFailure::ipc(CloseReason::Oversize))?;
            let end = offset
                .checked_add(length)
                .ok_or_else(|| ControlFailure::ipc(CloseReason::Oversize))?;
            let value = bytes
                .get(*offset..end)
                .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?;
            if major == 3 && core::str::from_utf8(value).is_err() {
                return Err(ControlFailure::ipc(CloseReason::Malformed));
            }
            *offset = end;
        }
        4 => {
            let count = read_argument(bytes, offset, info)?;
            for _ in 0..count {
                validate_item(bytes, offset, depth + 1)?;
            }
        }
        5 => {
            let count = read_argument(bytes, offset, info)?;
            let mut previous = None;
            for _ in 0..count {
                let key_initial = take(bytes, offset)?;
                if key_initial >> 5 != 0 {
                    return Err(ControlFailure::ipc(CloseReason::Malformed));
                }
                let key = read_argument(bytes, offset, key_initial & 31)?;
                if previous.is_some_and(|old| old >= key) {
                    return Err(ControlFailure::ipc(CloseReason::Malformed));
                }
                previous = Some(key);
                validate_item(bytes, offset, depth + 1)?;
            }
        }
        7 if matches!(info, 20 | 21) => {}
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    }
    Ok(())
}

fn take(bytes: &[u8], offset: &mut usize) -> Result<u8, ControlFailure> {
    let value = *bytes
        .get(*offset)
        .ok_or_else(|| ControlFailure::ipc(CloseReason::Malformed))?;
    *offset += 1;
    Ok(value)
}

fn read_argument(bytes: &[u8], offset: &mut usize, info: u8) -> Result<u64, ControlFailure> {
    let (value, minimum) = match info {
        0..=23 => return Ok(u64::from(info)),
        24 => (u64::from(take(bytes, offset)?), 24),
        25 => {
            let raw = [take(bytes, offset)?, take(bytes, offset)?];
            (u64::from(u16::from_be_bytes(raw)), 256)
        }
        26 => {
            let raw = [
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
            ];
            (u64::from(u32::from_be_bytes(raw)), 65_536)
        }
        27 => {
            let raw = [
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
                take(bytes, offset)?,
            ];
            (u64::from_be_bytes(raw), 4_294_967_296)
        }
        _ => return Err(ControlFailure::ipc(CloseReason::Malformed)),
    };
    if value < minimum {
        return Err(ControlFailure::ipc(CloseReason::Malformed));
    }
    Ok(value)
}
