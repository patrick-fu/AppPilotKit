//! Sans-I/O cryptographic transport core for the private Target/Broker link.
//!
//! The public API intentionally has no sockets, runtime callbacks, JSON-RPC,
//! IPC, or platform lifecycle hooks.  Callers own I/O and pass complete byte
//! slices through the role-specific state machines below.
//!
//! PBS/private-key wrappers and owned plaintext buffers are explicitly zeroized.
//! On every terminal transition the opaque `snow` states are dropped immediately;
//! `snow` 0.10 does not expose its internal key bytes for stronger caller-side wiping.

use core::fmt;
use minicbor::{Decoder as MiniDecoder, Encoder as MiniEncoder};
use snow::{
    Builder, HandshakeState, TransportState,
    params::NoiseParams,
    resolvers::{CryptoResolver, DefaultResolver},
};
use zeroize::Zeroize;

pub mod target_transport;

#[cfg(test)]
thread_local! {
    static SECRET_DROP_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static RECORD_DROP_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static DETACHED_PLAINTEXT_DROP_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn secret_drop_count() -> u64 {
    SECRET_DROP_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_drop_count() -> u64 {
    RECORD_DROP_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn detached_plaintext_drop_count() -> u64 {
    DETACHED_PLAINTEXT_DROP_COUNT.with(std::cell::Cell::get)
}

struct Decoder<'b>(MiniDecoder<'b>);
impl<'b> Decoder<'b> {
    fn new(bytes: &'b [u8]) -> Self {
        Self(MiniDecoder::new(bytes))
    }
    fn map(&mut self) -> Result<Option<u64>, Error> {
        self.0.map().map_err(cbor_decode_error)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        self.0.u8().map_err(cbor_decode_error)
    }
    fn u64(&mut self) -> Result<u64, Error> {
        self.0.u64().map_err(cbor_decode_error)
    }
    fn bytes(&mut self) -> Result<&'b [u8], Error> {
        self.0.bytes().map_err(cbor_decode_error)
    }
    fn position(&self) -> usize {
        self.0.position()
    }
}

struct Encoder<'a>(MiniEncoder<&'a mut Vec<u8>>);
impl<'a> Encoder<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self(MiniEncoder::new(out))
    }
    fn array(&mut self, value: u64) -> Result<&mut Self, Error> {
        self.0.array(value).map_err(cbor_encode_error)?;
        Ok(self)
    }
    fn map(&mut self, value: u64) -> Result<&mut Self, Error> {
        self.0.map(value).map_err(cbor_encode_error)?;
        Ok(self)
    }
    fn str(&mut self, value: &str) -> Result<&mut Self, Error> {
        self.0.str(value).map_err(cbor_encode_error)?;
        Ok(self)
    }
    fn bytes(&mut self, value: &[u8]) -> Result<&mut Self, Error> {
        self.0.bytes(value).map_err(cbor_encode_error)?;
        Ok(self)
    }
    fn u8(&mut self, value: u8) -> Result<&mut Self, Error> {
        self.0.u8(value).map_err(cbor_encode_error)?;
        Ok(self)
    }
    fn u64(&mut self, value: u64) -> Result<&mut Self, Error> {
        self.0.u64(value).map_err(cbor_encode_error)?;
        Ok(self)
    }
}

const OUTER_MAX: usize = u16::MAX as usize;
const RECORD_HEADER_LEN: usize = 12;
const RECORD_DATA_MAX: usize = 65_507;
const RECORD_LIMIT: u64 = 1u64 << 32;
const BYTE_LIMIT: u64 = 1u64 << 40;

/// The only terminal reasons emitted by this core.  Numeric values are wire values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CloseReason {
    Normal = 0,
    AuthenticationFailed = 1,
    BindingMismatch = 2,
    Stale = 3,
    Timeout = 4,
    Oversize = 5,
    Malformed = 6,
    SequenceViolation = 7,
    RecordLimit = 8,
    PeerClosed = 9,
    BrokerLost = 10,
    EligibilityLost = 11,
    CleanupFailed = 12,
    InternalError = 13,
}

impl CloseReason {
    fn from_wire(value: u8) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Normal),
            1 => Ok(Self::AuthenticationFailed),
            2 => Ok(Self::BindingMismatch),
            3 => Ok(Self::Stale),
            4 => Ok(Self::Timeout),
            5 => Ok(Self::Oversize),
            6 => Ok(Self::Malformed),
            7 => Ok(Self::SequenceViolation),
            8 => Ok(Self::RecordLimit),
            9 => Ok(Self::PeerClosed),
            10 => Ok(Self::BrokerLost),
            11 => Ok(Self::EligibilityLost),
            12 => Ok(Self::CleanupFailed),
            13 => Ok(Self::InternalError),
            _ => Err(Error::new(CloseReason::Malformed)),
        }
    }
}

/// Conservative application-dispatch state used in a close record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HandoffState {
    NotHandedOff = 0,
    HandoffPossibleOrConfirmed = 1,
}

impl HandoffState {
    fn from_wire(value: u8) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::NotHandedOff),
            1 => Ok(Self::HandoffPossibleOrConfirmed),
            _ => Err(Error::new(CloseReason::Malformed)),
        }
    }
    const fn conservative_max(self, other: Self) -> Self {
        if matches!(
            (self, other),
            (Self::HandoffPossibleOrConfirmed, _) | (_, Self::HandoffPossibleOrConfirmed)
        ) {
            Self::HandoffPossibleOrConfirmed
        } else {
            Self::NotHandedOff
        }
    }
}

/// A stable, secret-free error. Details deliberately never contain peer bytes or keys.
pub struct Error {
    reason: CloseReason,
    peer_close: Option<(CloseReason, HandoffState)>,
    close_frame: Option<Vec<u8>>,
}
impl Error {
    const fn new(reason: CloseReason) -> Self {
        Self {
            reason,
            peer_close: None,
            close_frame: None,
        }
    }
    const fn peer_close(reason: CloseReason, handoff: HandoffState) -> Self {
        Self {
            reason: CloseReason::PeerClosed,
            peer_close: Some((reason, handoff)),
            close_frame: None,
        }
    }
    fn record_limit(close_frame: Option<Vec<u8>>) -> Self {
        Self {
            reason: CloseReason::RecordLimit,
            peer_close: None,
            close_frame,
        }
    }
    pub const fn close_reason(&self) -> CloseReason {
        self.reason
    }
    pub const fn peer_close_details(&self) -> Option<(CloseReason, HandoffState)> {
        self.peer_close
    }
    /// Takes the one authenticated Close frame generated before a terminal limit transition.
    pub fn take_close_frame(&mut self) -> Option<Vec<u8>> {
        self.close_frame.take()
    }
}
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("reason", &self.reason)
            .field("peer_close", &self.peer_close)
            .field("has_close_frame", &self.close_frame.is_some())
            .finish()
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "transport closed: {:?}", self.reason)
    }
}
impl std::error::Error for Error {}
trait SnowResultExt<T> {
    fn core(self) -> Result<T, Error>;
}
impl<T> SnowResultExt<T> for Result<T, snow::Error> {
    fn core(self) -> Result<T, Error> {
        self.map_err(|_| Error::new(CloseReason::AuthenticationFailed))
    }
}
fn cbor_decode_error(_: minicbor::decode::Error) -> Error {
    Error::new(CloseReason::Malformed)
}
fn cbor_encode_error(_: minicbor::encode::Error<std::convert::Infallible>) -> Error {
    Error::new(CloseReason::InternalError)
}

/// Memory-only 32-byte PBS. It intentionally implements neither `Debug` nor `Display`.
pub struct ProcessBootstrapSecret([u8; 32]);
impl ProcessBootstrapSecret {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
    fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 32 {
            return Err(Error::new(CloseReason::Malformed));
        }
        let mut secret = Self([0; 32]);
        secret.0.copy_from_slice(bytes);
        Ok(secret)
    }
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = [0_u8; 32];
        let mut rng = DefaultResolver
            .resolve_rng()
            .ok_or(Error::new(CloseReason::InternalError))?;
        rng.try_fill_bytes(&mut bytes)
            .map_err(|_| Error::new(CloseReason::InternalError))?;
        Ok(Self(bytes))
    }
}
impl Drop for ProcessBootstrapSecret {
    fn drop(&mut self) {
        self.0.zeroize();
        #[cfg(test)]
        SECRET_DROP_COUNT.with(|count| count.set(count.get() + 1));
    }
}

/// One-time Broker NK private key. It intentionally implements neither `Debug` nor `Display`.
pub struct BrokerStaticPrivateKey([u8; 32]);
impl BrokerStaticPrivateKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
impl Drop for BrokerStaticPrivateKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Fresh one-time Broker NK keypair generated by the configured OS CSPRNG.
pub struct BrokerStaticKeypair {
    private: BrokerStaticPrivateKey,
    public: [u8; 32],
}
impl BrokerStaticKeypair {
    pub fn generate() -> Result<Self, Error> {
        let params: NoiseParams = "Noise_NK_25519_ChaChaPoly_SHA256"
            .parse()
            .map_err(|_| Error::new(CloseReason::InternalError))?;
        let mut keypair = Builder::new(params).generate_keypair().core()?;
        let private = BrokerStaticPrivateKey::new(array32(&keypair.private)?);
        let public = array32(&keypair.public)?;
        keypair.private.zeroize();
        Ok(Self { private, public })
    }
    pub const fn public_key(&self) -> [u8; 32] {
        self.public
    }
    pub fn into_private_key(self) -> BrokerStaticPrivateKey {
        self.private
    }
}

/// Incremental parser for the u16-big-endian outer envelope.
#[derive(Default)]
pub struct OuterFrameDecoder {
    pending: Vec<u8>,
    expected: Option<usize>,
    failed: bool,
}
impl OuterFrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if self.failed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        self.pending.extend_from_slice(input);
        let mut frames = Vec::new();
        loop {
            if self.expected.is_none() {
                if self.pending.len() < 2 {
                    break;
                }
                let n = u16::from_be_bytes([self.pending[0], self.pending[1]]) as usize;
                self.pending.drain(..2);
                if n == 0 {
                    self.clear();
                    self.failed = true;
                    return Err(Error::new(CloseReason::Malformed));
                }
                self.expected = Some(n);
            }
            let n = self.expected.expect("set above");
            if self.pending.len() < n {
                break;
            }
            let payload: Vec<u8> = self.pending.drain(..n).collect();
            frames.push(encode_outer(&payload)?);
            self.expected = None;
        }
        Ok(frames)
    }
    pub(crate) fn is_incomplete(&self) -> bool {
        self.expected.is_some() || !self.pending.is_empty()
    }
    pub fn timeout(&mut self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        if self.expected.is_some() || !self.pending.is_empty() {
            self.clear();
            self.failed = true;
            Err(Error::new(CloseReason::Timeout))
        } else {
            Ok(())
        }
    }
    pub fn eof(&mut self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        if self.expected.is_some() || !self.pending.is_empty() {
            self.clear();
            self.failed = true;
            Err(Error::new(CloseReason::Malformed))
        } else {
            Ok(())
        }
    }
    pub fn clear(&mut self) {
        self.pending.zeroize();
        self.pending.clear();
        self.expected = None;
    }
}
impl Drop for OuterFrameDecoder {
    fn drop(&mut self) {
        self.clear();
    }
}
pub fn encode_outer(payload: &[u8]) -> Result<Vec<u8>, Error> {
    if payload.is_empty() {
        return Err(Error::new(CloseReason::Malformed));
    }
    if payload.len() > OUTER_MAX {
        return Err(Error::new(CloseReason::Oversize));
    }
    let mut out = Vec::with_capacity(payload.len() + 2);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RecordKind {
    Application = 1,
    Finished = 2,
    Heartbeat = 3,
    Close = 4,
}
impl RecordKind {
    fn from_wire(v: u8) -> Result<Self, Error> {
        match v {
            1 => Ok(Self::Application),
            2 => Ok(Self::Finished),
            3 => Ok(Self::Heartbeat),
            4 => Ok(Self::Close),
            _ => Err(Error::new(CloseReason::Malformed)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub kind: RecordKind,
    pub start: bool,
    pub end: bool,
    pub total_len: u32,
    pub offset: u32,
    pub data: Vec<u8>,
}
impl Record {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.data.len() > RECORD_DATA_MAX {
            return Err(Error::new(CloseReason::Oversize));
        }
        let flags = (u8::from(self.start)) | (u8::from(self.end) << 1);
        let mut out = Vec::with_capacity(RECORD_HEADER_LEN + self.data.len());
        out.push(self.kind as u8);
        out.push(flags);
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&self.total_len.to_be_bytes());
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.data);
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < RECORD_HEADER_LEN {
            return Err(Error::new(CloseReason::Malformed));
        }
        if bytes.len() - RECORD_HEADER_LEN > RECORD_DATA_MAX {
            return Err(Error::new(CloseReason::Oversize));
        }
        let flags = bytes[1];
        if flags & !3 != 0 || bytes[2] != 0 || bytes[3] != 0 {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        Ok(Self {
            kind: RecordKind::from_wire(bytes[0])?,
            start: flags & 1 != 0,
            end: flags & 2 != 0,
            total_len: u32::from_be_bytes(bytes[4..8].try_into().expect("length checked")),
            offset: u32::from_be_bytes(bytes[8..12].try_into().expect("length checked")),
            data: bytes[12..].to_vec(),
        })
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        let had_data = !self.data.is_empty();
        self.data.zeroize();
        #[cfg(test)]
        {
            assert!(self.data.iter().all(|byte| *byte == 0));
            if had_data {
                RECORD_DROP_COUNT.with(|count| count.set(count.get() + 1));
            }
        }
        #[cfg(not(test))]
        let _ = had_data;
    }
}

struct DetachedPlaintext(Vec<u8>);

impl DetachedPlaintext {
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for DetachedPlaintext {
    fn drop(&mut self) {
        let had_data = !self.0.is_empty();
        self.0.zeroize();
        #[cfg(test)]
        {
            assert!(self.0.iter().all(|byte| *byte == 0));
            if had_data {
                DETACHED_PLAINTEXT_DROP_COUNT.with(|count| count.set(count.get() + 1));
            }
        }
        #[cfg(not(test))]
        let _ = had_data;
    }
}

/// One non-interleaved record reassembly. Callers select the current peer turn.
pub struct RecordReassembler {
    cap: usize,
    active: Option<(RecordKind, usize, Vec<u8>)>,
    records: u64,
    bytes: u64,
    failed: bool,
}
impl RecordReassembler {
    pub fn new(max_message_bytes: usize) -> Result<Self, Error> {
        if max_message_bytes == 0 {
            return Err(Error::new(CloseReason::InternalError));
        }
        Ok(Self {
            cap: max_message_bytes,
            active: None,
            records: 0,
            bytes: 0,
            failed: false,
        })
    }
    pub fn accept(&mut self, mut record: Record) -> Result<Option<(RecordKind, Vec<u8>)>, Error> {
        if self.failed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let result = (|| {
            self.records = self
                .records
                .checked_add(1)
                .ok_or(Error::new(CloseReason::RecordLimit))?;
            self.bytes = self
                .bytes
                .checked_add((RECORD_HEADER_LEN + record.data.len()) as u64)
                .ok_or(Error::new(CloseReason::RecordLimit))?;
            if self.records >= RECORD_LIMIT || self.bytes >= BYTE_LIMIT {
                self.clear();
                return Err(Error::new(CloseReason::RecordLimit));
            }
            if record.start {
                if record.total_len as usize > self.cap {
                    return Err(Error::new(CloseReason::Oversize));
                }
                if self.active.is_some() || record.offset != 0 {
                    return Err(Error::new(CloseReason::SequenceViolation));
                }
                if record.total_len == 0 && !record.end {
                    return Err(Error::new(CloseReason::Malformed));
                }
                let mut data = DetachedPlaintext(std::mem::take(&mut record.data));
                if data.0.len() > record.total_len as usize {
                    return Err(Error::new(CloseReason::Malformed));
                }
                if record.end {
                    if data.0.len() != record.total_len as usize {
                        return Err(Error::new(CloseReason::Malformed));
                    }
                    validate_control_record(record.kind, &data.0)?;
                    return Ok(Some((record.kind, data.take())));
                }
                self.active = Some((record.kind, record.total_len as usize, data.take()));
                return Ok(None);
            }
            let Some((kind, total, assembled)) = self.active.take() else {
                return Err(Error::new(CloseReason::SequenceViolation));
            };
            let mut assembled = DetachedPlaintext(assembled);
            if kind != record.kind
                || record.total_len != 0
                || record.offset as usize != assembled.0.len()
            {
                return Err(Error::new(CloseReason::SequenceViolation));
            }
            if assembled.0.len().saturating_add(record.data.len()) > total {
                return Err(Error::new(CloseReason::Malformed));
            }
            assembled.0.extend_from_slice(&record.data);
            if record.end {
                if assembled.0.len() != total {
                    return Err(Error::new(CloseReason::Malformed));
                }
                validate_control_record(kind, &assembled.0)?;
                return Ok(Some((kind, assembled.take())));
            }
            self.active = Some((kind, total, assembled.take()));
            Ok(None)
        })();
        if result.is_err() {
            self.clear();
            self.failed = true;
        }
        result
    }
    pub fn eof(&mut self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        if self.active.is_some() {
            self.clear();
            self.failed = true;
            Err(Error::new(CloseReason::Malformed))
        } else {
            Ok(())
        }
    }
    fn set_cap(&mut self, cap: usize) -> Result<(), Error> {
        if self.failed || cap == 0 || self.active.is_some() {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        self.cap = cap;
        Ok(())
    }
    #[cfg(test)]
    fn set_usage(&mut self, records: u64, plaintext_bytes: u64) {
        self.records = records;
        self.bytes = plaintext_bytes;
    }
    pub fn clear(&mut self) {
        if let Some((_, _, mut bytes)) = self.active.take() {
            bytes.zeroize();
        }
    }
}
impl Drop for RecordReassembler {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationTurn {
    Local,
    Peer,
}

/// Enforces the one-message-at-a-time application turn independently of I/O.
pub struct HalfDuplex {
    turn: ApplicationTurn,
}
impl HalfDuplex {
    pub const fn new(turn: ApplicationTurn) -> Self {
        Self { turn }
    }
    pub const fn turn(&self) -> ApplicationTurn {
        self.turn
    }
    pub fn local_message_sent(&mut self) -> Result<(), Error> {
        if self.turn != ApplicationTurn::Local {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        self.turn = ApplicationTurn::Peer;
        Ok(())
    }
    pub fn accept_peer_record(
        &mut self,
        reassembler: &mut RecordReassembler,
        record: Record,
    ) -> Result<Option<(RecordKind, Vec<u8>)>, Error> {
        if self.turn != ApplicationTurn::Peer {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let message = reassembler.accept(record)?;
        if message.is_some() {
            self.turn = ApplicationTurn::Local;
        }
        Ok(message)
    }
}

fn validate_control_record(kind: RecordKind, bytes: &[u8]) -> Result<(), Error> {
    if kind == RecordKind::Close {
        parse_close(bytes).map(|_| ())
    } else {
        Ok(())
    }
}

fn parse_close(bytes: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
    let mut decoder = Decoder::new(bytes);
    if decoder.map()? != Some(3) || decoder.u8()? != 0 || decoder.u8()? != 1 || decoder.u8()? != 1 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let reason = CloseReason::from_wire(decoder.u8()?)?;
    if decoder.u8()? != 2 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let handoff = HandoffState::from_wire(decoder.u8()?)?;
    if decoder.position() != bytes.len() || close_payload(reason, handoff)? != bytes {
        return Err(Error::new(CloseReason::Malformed));
    }
    Ok((reason, handoff))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionLimits {
    request_bytes: usize,
    response_bytes: usize,
    first_open_bytes: usize,
}
impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            request_bytes: 16 * 1024 * 1024,
            response_bytes: 64 * 1024 * 1024,
            first_open_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionBinding {
    pub lease_id: [u8; 16],
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub nk_handshake_hash: [u8; 32],
}
impl SessionBinding {
    fn valid(&self) -> bool {
        (1..=9_007_199_254_740_991).contains(&self.process_generation)
            && (1..=9_007_199_254_740_991).contains(&self.listener_epoch)
    }
}

fn session_prologue(binding: &SessionBinding, limits: SessionLimits) -> Result<Vec<u8>, Error> {
    if !binding.valid() {
        return Err(Error::new(CloseReason::BindingMismatch));
    }
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out);
    enc.array(12)?
        .str("apppilotkit.transport")?
        .u8(1)?
        .str("session")?
        .u8(0)?
        .u8(1)?
        .bytes(&binding.lease_id)?
        .u64(binding.process_generation)?
        .u64(binding.listener_epoch)?
        .u64(limits.request_bytes as u64)?
        .u64(limits.response_bytes as u64)?
        .u64(8192)?
        .bytes(&binding.nk_handshake_hash)?;
    Ok(out)
}

fn bootstrap_prologue(binding: &BootstrapBinding) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out);
    enc.array(10)?
        .str("apppilotkit.transport")?
        .u8(1)?
        .str("bootstrap")?
        .u8(0)?
        .u8(1)?
        .bytes(&binding.target_reference_digest)?
        .bytes(&binding.lease_id)?
        .bytes(&binding.target_nonce)?
        .bytes(&binding.app_artifact_digest)?
        .u64(binding.expiry_ms)?;
    Ok(out)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapBinding {
    pub target_reference_digest: [u8; 32],
    pub lease_id: [u8; 16],
    pub target_nonce: [u8; 32],
    pub app_artifact_digest: [u8; 32],
    pub expiry_ms: u64,
}
impl BootstrapBinding {
    pub fn m1_payload(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut e = Encoder::new(&mut out);
        e.map(4)?
            .u8(0)?
            .u8(1)?
            .u8(1)?
            .bytes(&self.target_reference_digest)?
            .u8(2)?
            .bytes(&self.lease_id)?
            .u8(3)?
            .bytes(&self.target_nonce)?;
        Ok(out)
    }
    fn expected_m2(&self, secret: &ProcessBootstrapSecret) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut e = Encoder::new(&mut out);
        e.map(7)?
            .u8(0)?
            .u8(1)?
            .u8(1)?
            .bytes(secret.bytes())?
            .u8(2)?
            .bytes(&self.target_reference_digest)?
            .u8(3)?
            .bytes(&self.lease_id)?
            .u8(4)?
            .bytes(&self.target_nonce)?
            .u8(5)?
            .u64(self.expiry_ms)?
            .u8(6)?
            .bytes(&self.app_artifact_digest)?;
        Ok(out)
    }
}

/// Target-only NK bootstrap initiator. It cannot be constructed as a responder.
pub struct TargetBootstrap {
    state: HandshakeState,
    binding: BootstrapBinding,
}
impl TargetBootstrap {
    pub fn new(binding: BootstrapBinding, broker_static_public: [u8; 32]) -> Result<Self, Error> {
        Self::build(binding, broker_static_public, None)
    }
    fn build(
        binding: BootstrapBinding,
        broker_static_public: [u8; 32],
        ephemeral: Option<&[u8; 32]>,
    ) -> Result<Self, Error> {
        let prologue = bootstrap_prologue(&binding)?;
        let builder = Builder::new(
            "Noise_NK_25519_ChaChaPoly_SHA256"
                .parse()
                .map_err(|_| Error::new(CloseReason::InternalError))?,
        )
        .remote_public_key(&broker_static_public)
        .core()?
        .prologue(&prologue)
        .core()?;
        let state = match ephemeral {
            Some(key) => builder.fixed_ephemeral_key_for_testing_only(key),
            None => builder,
        }
        .build_initiator()
        .core()?;
        Ok(Self { state, binding })
    }
    #[cfg(test)]
    fn new_test(
        binding: BootstrapBinding,
        broker_static_public: [u8; 32],
        ephemeral: &[u8; 32],
    ) -> Result<Self, Error> {
        Self::build(binding, broker_static_public, Some(ephemeral))
    }
    pub fn write_m1(&mut self) -> Result<Vec<u8>, Error> {
        write_handshake(&mut self.state, &self.binding.m1_payload()?)
    }
    pub fn read_m2(
        mut self,
        outer: &[u8],
        process_generation: u64,
        listener_epoch: u64,
    ) -> Result<(TargetBootstrapAckSender, ProcessBootstrapSecret), Error> {
        if !valid_generation(process_generation) || listener_epoch != 1 {
            return Err(Error::new(CloseReason::BindingMismatch));
        }
        let mut payload = read_handshake_mut(&mut self.state, outer)?;
        let hash: [u8; 32] = self
            .state
            .get_handshake_hash()
            .try_into()
            .expect("SHA256 size");
        let secret = parse_m2(&self.binding, &payload);
        payload.zeroize();
        let secret = secret?;
        let ack = TargetBootstrapAck {
            target_reference_digest: self.binding.target_reference_digest,
            lease_id: self.binding.lease_id,
            process_generation,
            listener_epoch,
            nk_handshake_hash: hash,
        };
        Ok((
            TargetBootstrapAckSender {
                transport: Some(self.state.into_transport_mode().core()?),
                ack,
            },
            secret,
        ))
    }
}

pub struct TargetBootstrapAckSender {
    transport: Option<TransportState>,
    ack: TargetBootstrapAck,
}
impl TargetBootstrapAckSender {
    pub fn write_ack(mut self) -> Result<(Vec<u8>, TargetLeaseConnection), Error> {
        let payload = bootstrap_ack_payload(&self.ack)?;
        let mut plaintext = complete_record(RecordKind::Finished, &payload)?.encode()?;
        let mut transport = self
            .transport
            .take()
            .ok_or(Error::new(CloseReason::SequenceViolation))?;
        let result = encrypt_transport(&mut transport, &plaintext);
        let plaintext_len = plaintext.len() as u64;
        plaintext.zeroize();
        let frame = result?;
        let binding = LeaseBinding::from_ack(&self.ack);
        Ok((
            frame,
            TargetLeaseConnection {
                core: LeaseCore::new(transport, binding, 1, plaintext_len, 0, 0),
            },
        ))
    }
}

/// Broker-only NK bootstrap responder. It cannot be constructed as an initiator.
pub struct BrokerBootstrap<'pbs> {
    state: HandshakeState,
    binding: BootstrapBinding,
    secret: &'pbs ProcessBootstrapSecret,
}
impl<'pbs> BrokerBootstrap<'pbs> {
    pub fn new(
        binding: BootstrapBinding,
        broker_static_private: BrokerStaticPrivateKey,
        secret: &'pbs ProcessBootstrapSecret,
    ) -> Result<Self, Error> {
        Self::build(binding, broker_static_private.bytes(), secret, None)
    }
    fn build(
        binding: BootstrapBinding,
        broker_static_private: &[u8; 32],
        secret: &'pbs ProcessBootstrapSecret,
        ephemeral: Option<&[u8; 32]>,
    ) -> Result<Self, Error> {
        let prologue = bootstrap_prologue(&binding)?;
        let builder = Builder::new(
            "Noise_NK_25519_ChaChaPoly_SHA256"
                .parse()
                .map_err(|_| Error::new(CloseReason::InternalError))?,
        )
        .local_private_key(broker_static_private)
        .core()?
        .prologue(&prologue)
        .core()?;
        let state = match ephemeral {
            Some(key) => builder.fixed_ephemeral_key_for_testing_only(key),
            None => builder,
        }
        .build_responder()
        .core()?;
        Ok(Self {
            state,
            binding,
            secret,
        })
    }
    #[cfg(test)]
    fn new_test(
        binding: BootstrapBinding,
        broker_static_private: &[u8; 32],
        secret: &'pbs ProcessBootstrapSecret,
        ephemeral: &[u8; 32],
    ) -> Result<Self, Error> {
        Self::build(binding, broker_static_private, secret, Some(ephemeral))
    }
    pub fn read_m1_write_m2(
        mut self,
        outer: &[u8],
    ) -> Result<(Vec<u8>, BrokerBootstrapAckReceiver), Error> {
        let payload = read_handshake_mut(&mut self.state, outer)?;
        if payload != self.binding.m1_payload()? {
            return Err(Error::new(CloseReason::AuthenticationFailed));
        }
        let mut m2_payload = self.binding.expected_m2(self.secret)?;
        let m2 = write_handshake(&mut self.state, &m2_payload);
        m2_payload.zeroize();
        let m2 = m2?;
        let hash = self
            .state
            .get_handshake_hash()
            .try_into()
            .expect("SHA256 size");
        let receiver = BrokerBootstrapAckReceiver {
            transport: Some(self.state.into_transport_mode().core()?),
            binding: self.binding,
            nk_handshake_hash: hash,
        };
        Ok((m2, receiver))
    }
}

pub struct BrokerBootstrapAckReceiver {
    transport: Option<TransportState>,
    binding: BootstrapBinding,
    nk_handshake_hash: [u8; 32],
}
impl BrokerBootstrapAckReceiver {
    pub fn read_ack(
        mut self,
        outer: &[u8],
    ) -> Result<(TargetBootstrapAck, BrokerLeaseConnection), Error> {
        let mut transport = self
            .transport
            .take()
            .ok_or(Error::new(CloseReason::SequenceViolation))?;
        let mut plaintext = decrypt_transport(&mut transport, outer)?;
        let plaintext_len = plaintext.len() as u64;
        let record = Record::decode(&plaintext);
        plaintext.zeroize();
        let record = record?;
        validate_complete_record(&record, RecordKind::Finished)?;
        let ack = parse_bootstrap_ack(&record.data, &self.binding, self.nk_handshake_hash)?;
        let lease = BrokerLeaseConnection {
            core: LeaseCore::new(
                transport,
                LeaseBinding::from_ack(&ack),
                0,
                0,
                1,
                plaintext_len,
            ),
        };
        Ok((ack, lease))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetBootstrapAck {
    pub target_reference_digest: [u8; 32],
    pub lease_id: [u8; 16],
    pub process_generation: u64,
    pub listener_epoch: u64,
    pub nk_handshake_hash: [u8; 32],
}

#[derive(Clone)]
struct LeaseBinding {
    lease_id: [u8; 16],
    process_generation: u64,
    listener_epoch: u64,
}
impl LeaseBinding {
    fn from_ack(ack: &TargetBootstrapAck) -> Self {
        Self {
            lease_id: ack.lease_id,
            process_generation: ack.process_generation,
            listener_epoch: ack.listener_epoch,
        }
    }
}

/// Target side of the post-ACK NK lease-control connection.
pub struct TargetLeaseConnection {
    core: LeaseCore,
}
/// Broker side of the post-ACK NK lease-control connection.
pub struct BrokerLeaseConnection {
    core: LeaseCore,
}

impl TargetLeaseConnection {
    pub fn read_heartbeat_request(&mut self, outer: &[u8]) -> Result<u64, Error> {
        let result = (|| {
            if self.core.pending_heartbeat.is_some() {
                return Err(Error::new(CloseReason::SequenceViolation));
            }
            let counter = self.core.read_heartbeat(outer, 1)?;
            if counter <= self.core.last_heartbeat {
                return Err(Error::new(CloseReason::SequenceViolation));
            }
            self.core.pending_heartbeat = Some(counter);
            Ok(counter)
        })();
        self.core.finish_result(result)
    }
    pub fn write_heartbeat_reply(&mut self, counter: u64) -> Result<Vec<u8>, Error> {
        let result = (|| {
            if self.core.pending_heartbeat != Some(counter) {
                return Err(Error::new(CloseReason::SequenceViolation));
            }
            let frame = self.core.write_heartbeat(2, counter)?;
            self.core.pending_heartbeat = None;
            self.core.last_heartbeat = counter;
            Ok(frame)
        })();
        self.core.finish_result(result)
    }
    pub fn write_close(
        &mut self,
        reason: CloseReason,
        handoff: HandoffState,
    ) -> Result<Vec<u8>, Error> {
        let result = self.core.write_close(reason, handoff);
        self.core.finish_result(result)
    }
    pub fn read_close(&mut self, outer: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
        let result = self.core.read_close(outer);
        self.core.finish_result(result)
    }
}

impl BrokerLeaseConnection {
    pub fn write_heartbeat_request(&mut self, counter: u64) -> Result<Vec<u8>, Error> {
        let result = (|| {
            if self.core.pending_heartbeat.is_some() || counter <= self.core.last_heartbeat {
                return Err(Error::new(CloseReason::SequenceViolation));
            }
            let frame = self.core.write_heartbeat(1, counter)?;
            self.core.pending_heartbeat = Some(counter);
            Ok(frame)
        })();
        self.core.finish_result(result)
    }
    pub fn read_heartbeat_reply(&mut self, outer: &[u8]) -> Result<u64, Error> {
        let result = (|| {
            let counter = self.core.read_heartbeat(outer, 2)?;
            if self.core.pending_heartbeat != Some(counter) {
                return Err(Error::new(CloseReason::SequenceViolation));
            }
            self.core.pending_heartbeat = None;
            self.core.last_heartbeat = counter;
            Ok(counter)
        })();
        self.core.finish_result(result)
    }
    pub fn write_close(
        &mut self,
        reason: CloseReason,
        handoff: HandoffState,
    ) -> Result<Vec<u8>, Error> {
        let result = self.core.write_close(reason, handoff);
        self.core.finish_result(result)
    }
    pub fn read_close(&mut self, outer: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
        let result = self.core.read_close(outer);
        self.core.finish_result(result)
    }
}

struct LeaseCore {
    transport: Option<TransportState>,
    binding: LeaseBinding,
    closed: bool,
    sent_records: u64,
    sent_plaintext: u64,
    received_records: u64,
    received_plaintext: u64,
    pending_heartbeat: Option<u64>,
    last_heartbeat: u64,
}
impl LeaseCore {
    fn new(
        transport: TransportState,
        binding: LeaseBinding,
        sent_records: u64,
        sent_plaintext: u64,
        received_records: u64,
        received_plaintext: u64,
    ) -> Self {
        Self {
            transport: Some(transport),
            binding,
            closed: false,
            sent_records,
            sent_plaintext,
            received_records,
            received_plaintext,
            pending_heartbeat: None,
            last_heartbeat: 0,
        }
    }
    fn finish_result<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.close_state();
        }
        result
    }
    fn close_state(&mut self) {
        self.closed = true;
        self.transport = None;
    }
    fn transport(&mut self) -> Result<&mut TransportState, Error> {
        if self.closed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        self.transport
            .as_mut()
            .ok_or(Error::new(CloseReason::SequenceViolation))
    }
    fn write_heartbeat(&mut self, role: u8, counter: u64) -> Result<Vec<u8>, Error> {
        let payload = heartbeat_payload(role, &self.binding, counter)?;
        self.write_record(RecordKind::Heartbeat, &payload)
    }
    fn read_heartbeat(&mut self, outer: &[u8], role: u8) -> Result<u64, Error> {
        let mut plaintext = self.decrypt_counted(outer)?;
        let record = Record::decode(&plaintext);
        plaintext.zeroize();
        let record = record?;
        if record.kind == RecordKind::Close {
            validate_complete_record(&record, RecordKind::Close)?;
            let (reason, handoff) = parse_close(&record.data)?;
            return Err(Error::peer_close(reason, handoff));
        }
        validate_complete_record(&record, RecordKind::Heartbeat)?;
        parse_heartbeat(&record.data, role, &self.binding)
    }
    fn write_close(
        &mut self,
        reason: CloseReason,
        handoff: HandoffState,
    ) -> Result<Vec<u8>, Error> {
        let frame = self.write_record(RecordKind::Close, &close_payload(reason, handoff)?)?;
        self.close_state();
        Ok(frame)
    }
    fn read_close(&mut self, outer: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
        let mut plaintext = self.decrypt_counted(outer)?;
        let record = Record::decode(&plaintext);
        plaintext.zeroize();
        let record = record?;
        validate_complete_record(&record, RecordKind::Close)?;
        let close = parse_close(&record.data)?;
        self.close_state();
        Ok(close)
    }
    fn write_record(&mut self, kind: RecordKind, data: &[u8]) -> Result<Vec<u8>, Error> {
        let mut plaintext = complete_record(kind, data)?.encode()?;
        let next_records = self.sent_records.checked_add(1);
        let next_bytes = self.sent_plaintext.checked_add(plaintext.len() as u64);
        let close_len = limit_close_plaintext(HandoffState::NotHandedOff)?.len() as u64;
        let limit_reached = if kind == RecordKind::Close {
            next_records.is_none_or(|records| records >= RECORD_LIMIT)
                || next_bytes.is_none_or(|bytes| bytes >= BYTE_LIMIT)
        } else {
            next_records
                .and_then(|records| records.checked_add(1))
                .is_none_or(|records_with_close| records_with_close >= RECORD_LIMIT)
                || next_bytes
                    .and_then(|bytes| bytes.checked_add(close_len))
                    .is_none_or(|bytes_with_close| bytes_with_close >= BYTE_LIMIT)
        };
        if limit_reached {
            plaintext.zeroize();
            return Err(self.limit_error());
        }
        let frame = encrypt_transport(self.transport()?, &plaintext);
        plaintext.zeroize();
        let frame = frame?;
        self.sent_records = next_records.expect("checked");
        self.sent_plaintext = next_bytes.expect("checked");
        Ok(frame)
    }
    fn decrypt_counted(&mut self, outer: &[u8]) -> Result<Vec<u8>, Error> {
        let mut plaintext = decrypt_transport(self.transport()?, outer)?;
        let next_records = self.received_records.checked_add(1);
        let next_bytes = self.received_plaintext.checked_add(plaintext.len() as u64);
        if next_records.is_none_or(|v| v >= RECORD_LIMIT)
            || next_bytes.is_none_or(|v| v >= BYTE_LIMIT)
        {
            plaintext.zeroize();
            return Err(self.limit_error());
        }
        self.received_records = next_records.expect("checked");
        self.received_plaintext = next_bytes.expect("checked");
        Ok(plaintext)
    }
    fn limit_error(&mut self) -> Error {
        let close = (|| {
            let mut plaintext = limit_close_plaintext(HandoffState::NotHandedOff)?;
            let next_records = self.sent_records.checked_add(1);
            let next_bytes = self.sent_plaintext.checked_add(plaintext.len() as u64);
            if next_records.is_none_or(|records| records >= RECORD_LIMIT)
                || next_bytes.is_none_or(|bytes| bytes >= BYTE_LIMIT)
            {
                plaintext.zeroize();
                return Err(Error::new(CloseReason::RecordLimit));
            }
            let frame = encrypt_transport(self.transport()?, &plaintext);
            plaintext.zeroize();
            let frame = frame?;
            self.sent_records = next_records.expect("checked");
            self.sent_plaintext = next_bytes.expect("checked");
            Ok(frame)
        })()
        .ok();
        self.close_state();
        Error::record_limit(close)
    }
    #[cfg(test)]
    fn set_usage(
        &mut self,
        sent_records: u64,
        sent_plaintext: u64,
        received_records: u64,
        received_plaintext: u64,
    ) {
        self.sent_records = sent_records;
        self.sent_plaintext = sent_plaintext;
        self.received_records = received_records;
        self.received_plaintext = received_plaintext;
    }
}
impl Drop for LeaseCore {
    fn drop(&mut self) {
        self.close_state();
    }
}
fn parse_m2(binding: &BootstrapBinding, bytes: &[u8]) -> Result<ProcessBootstrapSecret, Error> {
    let mut d = Decoder::new(bytes);
    if d.map()? != Some(7) {
        return Err(Error::new(CloseReason::Malformed));
    }
    let mut version = None;
    let mut secret = None;
    let mut digest = None;
    let mut lease = None;
    let mut nonce = None;
    let mut expiry = None;
    let mut artifact = None;
    for expected in 0..7 {
        if d.u8()? != expected {
            return Err(Error::new(CloseReason::Malformed));
        }
        match expected {
            0 => version = Some(d.u8()?),
            1 => secret = Some(ProcessBootstrapSecret::from_slice(d.bytes()?)?),
            2 => digest = Some(array32(d.bytes()?)?),
            3 => lease = Some(array16(d.bytes()?)?),
            4 => nonce = Some(array32(d.bytes()?)?),
            5 => expiry = Some(d.u64()?),
            6 => artifact = Some(array32(d.bytes()?)?),
            _ => unreachable!(),
        }
    }
    if d.position() != bytes.len() || version != Some(1) {
        return Err(Error::new(CloseReason::Malformed));
    }
    if digest != Some(binding.target_reference_digest)
        || lease != Some(binding.lease_id)
        || nonce != Some(binding.target_nonce)
        || expiry != Some(binding.expiry_ms)
        || artifact != Some(binding.app_artifact_digest)
    {
        return Err(Error::new(CloseReason::BindingMismatch));
    }
    let secret = secret.ok_or(Error::new(CloseReason::Malformed))?;
    let mut canonical = binding.expected_m2(&secret)?;
    let is_canonical = canonical == bytes;
    canonical.zeroize();
    if !is_canonical {
        return Err(Error::new(CloseReason::Malformed));
    }
    Ok(secret)
}

fn bootstrap_ack_payload(ack: &TargetBootstrapAck) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out);
    e.map(6)?
        .u8(0)?
        .u8(1)?
        .u8(1)?
        .bytes(&ack.target_reference_digest)?
        .u8(2)?
        .bytes(&ack.lease_id)?
        .u8(3)?
        .u64(ack.process_generation)?
        .u8(4)?
        .u64(ack.listener_epoch)?
        .u8(5)?
        .bytes(&ack.nk_handshake_hash)?;
    Ok(out)
}

fn parse_bootstrap_ack(
    bytes: &[u8],
    binding: &BootstrapBinding,
    hash: [u8; 32],
) -> Result<TargetBootstrapAck, Error> {
    let mut d = Decoder::new(bytes);
    if d.map()? != Some(6) || d.u8()? != 0 || d.u8()? != 1 || d.u8()? != 1 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let digest = array32(d.bytes()?)?;
    if d.u8()? != 2 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let lease = array16(d.bytes()?)?;
    if d.u8()? != 3 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let generation = d.u64()?;
    if d.u8()? != 4 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let epoch = d.u64()?;
    if d.u8()? != 5 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let received_hash = array32(d.bytes()?)?;
    if d.position() != bytes.len() {
        return Err(Error::new(CloseReason::Malformed));
    }
    if digest != binding.target_reference_digest
        || lease != binding.lease_id
        || received_hash != hash
        || !valid_generation(generation)
        || epoch != 1
    {
        return Err(Error::new(CloseReason::BindingMismatch));
    }
    let ack = TargetBootstrapAck {
        target_reference_digest: digest,
        lease_id: lease,
        process_generation: generation,
        listener_epoch: epoch,
        nk_handshake_hash: hash,
    };
    if bootstrap_ack_payload(&ack)? != bytes {
        return Err(Error::new(CloseReason::Malformed));
    }
    Ok(ack)
}

fn valid_generation(value: u64) -> bool {
    (1..=9_007_199_254_740_991).contains(&value)
}

/// Target-only NNpsk0 initiator. It can only receive the first opaque application.
pub struct TargetSession {
    core: SessionCore,
}
/// Broker-only NNpsk0 responder. It can only send the first opaque application.
pub struct BrokerSession {
    core: SessionCore,
}
struct SessionCore {
    handshake: Option<HandshakeState>,
    transport: Option<TransportState>,
    binding: SessionBinding,
    session_hash: Option<[u8; 32]>,
    limits: SessionLimits,
    reassembly: RecordReassembler,
    sent_finished: bool,
    got_finished: bool,
    peer_turn: bool,
    first_application: bool,
    closed: bool,
    sent_records: u64,
    sent_plaintext: u64,
    received_records: u64,
    received_plaintext: u64,
    handoff: HandoffState,
}
impl TargetSession {
    pub fn new(binding: SessionBinding, pbs: &ProcessBootstrapSecret) -> Result<Self, Error> {
        let limits = SessionLimits::default();
        Ok(Self {
            core: SessionCore::new(
                binding,
                pbs,
                limits,
                true,
                limits.first_open_bytes,
                true,
                None,
            )?,
        })
    }
    #[cfg(test)]
    fn new_test(
        binding: SessionBinding,
        pbs: &ProcessBootstrapSecret,
        limits: SessionLimits,
        ephemeral: &[u8; 32],
    ) -> Result<Self, Error> {
        Ok(Self {
            core: SessionCore::new(
                binding,
                pbs,
                limits,
                true,
                limits.first_open_bytes,
                true,
                Some(ephemeral),
            )?,
        })
    }
    pub fn write_m1(&mut self) -> Result<Vec<u8>, Error> {
        let result = self.core.write_handshake(&[]);
        self.core.finish_result(result)
    }
    pub fn read_m2(&mut self, outer: &[u8]) -> Result<(), Error> {
        let result = self.core.read_handshake_finish(outer);
        self.core.finish_result(result)
    }
    pub fn write_finished(&mut self) -> Result<Vec<u8>, Error> {
        let result = self.core.write_finished(0);
        self.core.finish_result(result)
    }
    pub fn read_finished(&mut self, outer: &[u8]) -> Result<(), Error> {
        let result = if !self.core.sent_finished {
            Err(Error::new(CloseReason::SequenceViolation))
        } else {
            self.core.read_finished(outer, 1)
        };
        self.core.finish_result(result)
    }
    pub fn read_application(&mut self, outer: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let result = self.core.read_application(outer);
        if matches!(result, Ok(Some(_))) {
            self.core.handoff = HandoffState::HandoffPossibleOrConfirmed;
        }
        if matches!(result, Ok(Some(_))) && !self.core.first_application {
            self.core.first_application = true;
            self.core
                .reassembly
                .set_cap(self.core.limits.request_bytes)?;
        }
        self.core.finish_result(result)
    }
    pub fn eof(&mut self) -> Result<(), Error> {
        self.core.eof()
    }
    pub fn write_application_response(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let result = if !self.core.first_application {
            Err(Error::new(CloseReason::SequenceViolation))
        } else {
            self.core
                .write_application(payload, self.core.limits.response_bytes)
        };
        self.core.finish_result(result)
    }
    pub fn write_close(
        &mut self,
        reason: CloseReason,
        handoff: HandoffState,
    ) -> Result<Vec<u8>, Error> {
        let result = self.core.write_close(reason, handoff);
        self.core.finish_result(result)
    }
    pub fn read_close(&mut self, outer: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
        let result = self.core.read_close(outer);
        self.core.finish_result(result)
    }
    pub fn validate_binding(
        &mut self,
        process_generation: u64,
        listener_epoch: u64,
    ) -> Result<(), Error> {
        self.core
            .validate_binding(process_generation, listener_epoch)
    }
}
impl BrokerSession {
    pub fn new(binding: SessionBinding, pbs: &ProcessBootstrapSecret) -> Result<Self, Error> {
        let limits = SessionLimits::default();
        Ok(Self {
            core: SessionCore::new(
                binding,
                pbs,
                limits,
                false,
                limits.response_bytes,
                false,
                None,
            )?,
        })
    }
    #[cfg(test)]
    fn new_test(
        binding: SessionBinding,
        pbs: &ProcessBootstrapSecret,
        limits: SessionLimits,
        ephemeral: &[u8; 32],
    ) -> Result<Self, Error> {
        Ok(Self {
            core: SessionCore::new(
                binding,
                pbs,
                limits,
                false,
                limits.response_bytes,
                false,
                Some(ephemeral),
            )?,
        })
    }
    pub fn read_m1_write_m2(&mut self, outer: &[u8]) -> Result<Vec<u8>, Error> {
        let result = (|| {
            self.core.read_handshake(outer)?;
            let message = self.core.write_handshake(&[])?;
            self.core.finish_handshake()?;
            Ok(message)
        })();
        self.core.finish_result(result)
    }
    pub fn write_finished(&mut self) -> Result<Vec<u8>, Error> {
        let result = if !self.core.got_finished {
            Err(Error::new(CloseReason::SequenceViolation))
        } else {
            self.core.write_finished(1)
        };
        self.core.finish_result(result)
    }
    pub fn read_finished(&mut self, outer: &[u8]) -> Result<(), Error> {
        let result = self.core.read_finished(outer, 0);
        self.core.finish_result(result)
    }
    pub fn write_session_open(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let result = if self.core.first_application || payload.is_empty() {
            Err(Error::new(CloseReason::SequenceViolation))
        } else if payload.len() > self.core.limits.first_open_bytes {
            Err(Error::new(CloseReason::Oversize))
        } else {
            self.core.first_application = true;
            self.core
                .write_application(payload, self.core.limits.request_bytes)
        };
        if result.is_ok() {
            self.core.handoff = HandoffState::HandoffPossibleOrConfirmed;
        }
        self.core.finish_result(result)
    }
    pub fn write_application_request(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let result = if !self.core.first_application {
            Err(Error::new(CloseReason::SequenceViolation))
        } else {
            self.core
                .write_application(payload, self.core.limits.request_bytes)
        };
        if result.is_ok() {
            self.core.handoff = HandoffState::HandoffPossibleOrConfirmed;
        }
        self.core.finish_result(result)
    }
    pub fn read_application_response(&mut self, outer: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let result = self.core.read_application(outer);
        self.core.finish_result(result)
    }
    pub fn eof(&mut self) -> Result<(), Error> {
        self.core.eof()
    }
    pub fn write_close(
        &mut self,
        reason: CloseReason,
        handoff: HandoffState,
    ) -> Result<Vec<u8>, Error> {
        let result = self.core.write_close(reason, handoff);
        self.core.finish_result(result)
    }
    pub fn read_close(&mut self, outer: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
        let result = self.core.read_close(outer);
        self.core.finish_result(result)
    }
    pub fn validate_binding(
        &mut self,
        process_generation: u64,
        listener_epoch: u64,
    ) -> Result<(), Error> {
        self.core
            .validate_binding(process_generation, listener_epoch)
    }
}
impl SessionCore {
    fn close_state(&mut self) {
        self.closed = true;
        self.transport = None;
        self.handshake = None;
        self.reassembly.clear();
        self.session_hash.zeroize();
    }
    fn finish_result<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.close_state();
        }
        result
    }
    fn new(
        binding: SessionBinding,
        pbs: &ProcessBootstrapSecret,
        limits: SessionLimits,
        initiator: bool,
        inbound_cap: usize,
        peer_turn: bool,
        ephemeral: Option<&[u8; 32]>,
    ) -> Result<Self, Error> {
        if limits != SessionLimits::default() {
            return Err(Error::new(CloseReason::BindingMismatch));
        }
        let prologue = session_prologue(&binding, limits)?;
        let params = "Noise_NNpsk0_25519_ChaChaPoly_SHA256"
            .parse()
            .map_err(|_| Error::new(CloseReason::InternalError))?;
        let builder = Builder::new(params)
            .psk(0, pbs.bytes())
            .core()?
            .prologue(&prologue)
            .core()?;
        let builder = match ephemeral {
            Some(key) => builder.fixed_ephemeral_key_for_testing_only(key),
            None => builder,
        };
        let handshake = if initiator {
            builder.build_initiator().core()?
        } else {
            builder.build_responder().core()?
        };
        Ok(Self {
            handshake: Some(handshake),
            transport: None,
            binding,
            session_hash: None,
            limits,
            reassembly: RecordReassembler::new(inbound_cap)?,
            sent_finished: false,
            got_finished: false,
            peer_turn,
            first_application: false,
            closed: false,
            sent_records: 0,
            sent_plaintext: 0,
            received_records: 0,
            received_plaintext: 0,
            handoff: HandoffState::NotHandedOff,
        })
    }
    fn write_handshake(&mut self, payload: &[u8]) -> Result<Vec<u8>, Error> {
        write_handshake(
            self.handshake
                .as_mut()
                .ok_or(Error::new(CloseReason::SequenceViolation))?,
            payload,
        )
    }
    fn read_handshake(&mut self, outer: &[u8]) -> Result<(), Error> {
        let payload = read_handshake_mut(
            self.handshake
                .as_mut()
                .ok_or(Error::new(CloseReason::SequenceViolation))?,
            outer,
        )?;
        if !payload.is_empty() {
            return Err(Error::new(CloseReason::AuthenticationFailed));
        }
        Ok(())
    }
    fn read_handshake_finish(&mut self, outer: &[u8]) -> Result<(), Error> {
        self.read_handshake(outer)?;
        self.finish_handshake()
    }
    fn finish_handshake(&mut self) -> Result<(), Error> {
        let handshake = self
            .handshake
            .take()
            .ok_or(Error::new(CloseReason::SequenceViolation))?;
        self.session_hash = Some(
            handshake
                .get_handshake_hash()
                .try_into()
                .expect("SHA256 size"),
        );
        self.transport = Some(handshake.into_transport_mode().core()?);
        Ok(())
    }
    fn transport(&mut self) -> Result<&mut TransportState, Error> {
        if self.closed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        self.transport
            .as_mut()
            .ok_or(Error::new(CloseReason::SequenceViolation))
    }
    fn write_finished(&mut self, role: u8) -> Result<Vec<u8>, Error> {
        if self.sent_finished {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let body = finished(
            &self.binding,
            role,
            self.session_hash
                .ok_or(Error::new(CloseReason::SequenceViolation))?,
        )?;
        let frame = self.write_record(RecordKind::Finished, &body)?;
        self.sent_finished = true;
        Ok(frame)
    }
    fn read_finished(&mut self, outer: &[u8], role: u8) -> Result<(), Error> {
        if self.got_finished {
            self.closed = true;
            return Err(Error::new(CloseReason::AuthenticationFailed));
        }
        let expected_hash = self
            .session_hash
            .ok_or(Error::new(CloseReason::SequenceViolation))?;
        let mut plain = self.decrypt_counted(outer)?;
        let record = Record::decode(&plain);
        plain.zeroize();
        let record = record?;
        validate_complete_record(&record, RecordKind::Finished)?;
        if parse_finished(&record.data, &self.binding, expected_hash)? != role {
            return Err(Error::new(CloseReason::AuthenticationFailed));
        }
        self.got_finished = true;
        Ok(())
    }
    fn write_application(&mut self, payload: &[u8], cap: usize) -> Result<Vec<Vec<u8>>, Error> {
        if !self.sent_finished || !self.got_finished || self.peer_turn || payload.is_empty() {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        if payload.len() > cap {
            return Err(Error::new(CloseReason::Oversize));
        }
        let mut plaintexts = fragment_records(RecordKind::Application, payload)?;
        let message_records =
            u64::try_from(plaintexts.len()).map_err(|_| Error::new(CloseReason::RecordLimit))?;
        let message_bytes = plaintexts.iter().try_fold(0_u64, |total, plaintext| {
            total.checked_add(plaintext.len() as u64)
        });
        let usage = match message_bytes {
            Some(bytes) => self.send_usage_after(message_records, bytes, true)?,
            None => None,
        };
        let Some((next_records, next_bytes)) = usage else {
            for plaintext in &mut plaintexts {
                plaintext.zeroize();
            }
            return Err(self.record_limit_error());
        };
        let mut frames = Vec::with_capacity(plaintexts.len());
        for mut plaintext in plaintexts {
            let frame = encrypt_transport(self.transport()?, &plaintext);
            plaintext.zeroize();
            frames.push(frame?);
        }
        self.sent_records = next_records;
        self.sent_plaintext = next_bytes;
        self.peer_turn = true;
        Ok(frames)
    }
    fn read_application(&mut self, outer: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if !self.sent_finished || !self.got_finished {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let mut plain = self.decrypt_counted(outer)?;
        let record = Record::decode(&plain);
        plain.zeroize();
        let record = record?;
        if record.kind == RecordKind::Close {
            validate_complete_record(&record, RecordKind::Close)?;
            let (reason, handoff) = parse_close(&record.data)?;
            self.closed = true;
            return Err(Error::peer_close(reason, handoff));
        }
        if !self.peer_turn {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        if record.kind != RecordKind::Application {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let message = self.reassembly.accept(record)?;
        if matches!(&message, Some((_, bytes)) if bytes.is_empty()) {
            return Err(Error::new(CloseReason::Malformed));
        }
        if message.is_some() {
            self.peer_turn = false;
        }
        Ok(message.map(|(_, bytes)| bytes))
    }
    fn eof(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let result = self
            .reassembly
            .eof()
            .and(Err(Error::new(CloseReason::PeerClosed)));
        self.close_state();
        result
    }
    fn read_close(&mut self, outer: &[u8]) -> Result<(CloseReason, HandoffState), Error> {
        if !self.sent_finished || !self.got_finished {
            return Err(Error::new(CloseReason::SequenceViolation));
        }
        let mut plaintext = self.decrypt_counted(outer)?;
        let record = Record::decode(&plaintext);
        plaintext.zeroize();
        let record = record?;
        validate_complete_record(&record, RecordKind::Close)?;
        let close = parse_close(&record.data)?;
        self.closed = true;
        self.transport = None;
        self.handshake = None;
        self.reassembly.clear();
        Ok(close)
    }
    fn write_record(&mut self, kind: RecordKind, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let mut plaintext = complete_record(kind, payload)?.encode()?;
        let frame = self.encrypt_counted(&plaintext);
        plaintext.zeroize();
        frame
    }
    fn encrypt_counted(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let Some((next_records, next_bytes)) =
            self.send_usage_after(1, plaintext.len() as u64, true)?
        else {
            return Err(self.record_limit_error());
        };
        let frame = encrypt_transport(self.transport()?, plaintext)?;
        self.sent_records = next_records;
        self.sent_plaintext = next_bytes;
        Ok(frame)
    }
    fn decrypt_counted(&mut self, outer: &[u8]) -> Result<Vec<u8>, Error> {
        let mut plaintext = decrypt_transport(self.transport()?, outer)?;
        let next_records = self.received_records.checked_add(1);
        let next_bytes = self.received_plaintext.checked_add(plaintext.len() as u64);
        let limit_reached = next_records.is_none_or(|count| count >= RECORD_LIMIT)
            || next_bytes.is_none_or(|bytes| bytes >= BYTE_LIMIT);
        if limit_reached {
            let error = self.record_limit_error();
            plaintext.zeroize();
            return Err(error);
        }
        self.received_records = next_records.expect("checked above");
        self.received_plaintext = next_bytes.expect("checked above");
        Ok(plaintext)
    }
    fn record_limit_error(&mut self) -> Error {
        self.record_limit_error_with_handoff(self.handoff)
    }
    fn record_limit_error_with_handoff(&mut self, handoff: HandoffState) -> Error {
        let close_frame = (|| {
            let mut plaintext = limit_close_plaintext(handoff)?;
            let Some((next_records, next_bytes)) =
                self.send_usage_after(1, plaintext.len() as u64, false)?
            else {
                plaintext.zeroize();
                return Err(Error::new(CloseReason::RecordLimit));
            };
            let frame = encrypt_transport(self.transport()?, &plaintext);
            plaintext.zeroize();
            let frame = frame?;
            self.sent_records = next_records;
            self.sent_plaintext = next_bytes;
            Ok(frame)
        })()
        .ok();
        self.close_state();
        Error::record_limit(close_frame)
    }
    fn send_usage_after(
        &self,
        additional_records: u64,
        additional_bytes: u64,
        reserve_close: bool,
    ) -> Result<Option<(u64, u64)>, Error> {
        let Some(next_records) = self.sent_records.checked_add(additional_records) else {
            return Ok(None);
        };
        let Some(next_bytes) = self.sent_plaintext.checked_add(additional_bytes) else {
            return Ok(None);
        };
        let (records_with_reserve, bytes_with_reserve) = if reserve_close {
            let close_len = limit_close_plaintext(self.handoff)?.len() as u64;
            (
                next_records.checked_add(1),
                next_bytes.checked_add(close_len),
            )
        } else {
            (Some(next_records), Some(next_bytes))
        };
        if records_with_reserve.is_none_or(|records| records >= RECORD_LIMIT)
            || bytes_with_reserve.is_none_or(|bytes| bytes >= BYTE_LIMIT)
        {
            Ok(None)
        } else {
            Ok(Some((next_records, next_bytes)))
        }
    }
    #[cfg(test)]
    fn set_usage(
        &mut self,
        sent_records: u64,
        sent_plaintext: u64,
        received_records: u64,
        received_plaintext: u64,
    ) {
        self.sent_records = sent_records;
        self.sent_plaintext = sent_plaintext;
        self.received_records = received_records;
        self.received_plaintext = received_plaintext;
    }
    fn write_close(
        &mut self,
        reason: CloseReason,
        handoff: HandoffState,
    ) -> Result<Vec<u8>, Error> {
        let effective_handoff = self.handoff.conservative_max(handoff);
        let mut plaintext = complete_record(
            RecordKind::Close,
            &close_payload(reason, effective_handoff)?,
        )?
        .encode()?;
        let Some((next_records, next_bytes)) =
            self.send_usage_after(1, plaintext.len() as u64, false)?
        else {
            plaintext.zeroize();
            return Err(self.record_limit_error_with_handoff(effective_handoff));
        };
        let frame = encrypt_transport(self.transport()?, &plaintext);
        plaintext.zeroize();
        let frame = frame?;
        self.sent_records = next_records;
        self.sent_plaintext = next_bytes;
        self.closed = true;
        self.transport = None;
        self.handshake = None;
        self.reassembly.clear();
        Ok(frame)
    }
    fn validate_binding(
        &mut self,
        process_generation: u64,
        listener_epoch: u64,
    ) -> Result<(), Error> {
        if process_generation != self.binding.process_generation
            || listener_epoch != self.binding.listener_epoch
        {
            self.close_state();
            return Err(Error::new(CloseReason::BindingMismatch));
        }
        Ok(())
    }
}

impl Drop for SessionCore {
    fn drop(&mut self) {
        self.close_state();
    }
}

fn write_handshake(state: &mut HandshakeState, payload: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = vec![0; OUTER_MAX];
    let n = state.write_message(payload, &mut out).core()?;
    out.truncate(n);
    encode_outer(&out)
}
fn read_handshake_mut(state: &mut HandshakeState, outer: &[u8]) -> Result<Vec<u8>, Error> {
    let payload = one_outer(outer)?;
    let mut out = vec![0; OUTER_MAX];
    let n = state.read_message(payload, &mut out).core()?;
    out.truncate(n);
    Ok(out)
}
fn one_outer(outer: &[u8]) -> Result<&[u8], Error> {
    if outer.len() < 2 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let n = u16::from_be_bytes([outer[0], outer[1]]) as usize;
    if n == 0 || outer.len() != n + 2 {
        return Err(Error::new(CloseReason::Malformed));
    }
    Ok(&outer[2..])
}
fn array32(bytes: &[u8]) -> Result<[u8; 32], Error> {
    bytes
        .try_into()
        .map_err(|_| Error::new(CloseReason::Malformed))
}
fn array16(bytes: &[u8]) -> Result<[u8; 16], Error> {
    bytes
        .try_into()
        .map_err(|_| Error::new(CloseReason::Malformed))
}

fn encrypt_transport(state: &mut TransportState, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    let mut ciphertext = vec![0; plaintext.len() + 16];
    let n = state.write_message(plaintext, &mut ciphertext).core()?;
    ciphertext.truncate(n);
    encode_outer(&ciphertext)
}
fn decrypt_transport(state: &mut TransportState, outer: &[u8]) -> Result<Vec<u8>, Error> {
    let ciphertext = one_outer(outer)?;
    let mut plaintext = vec![0; ciphertext.len()];
    let n = state.read_message(ciphertext, &mut plaintext).core()?;
    plaintext.truncate(n);
    Ok(plaintext)
}
fn complete_record(kind: RecordKind, payload: &[u8]) -> Result<Record, Error> {
    let total_len = u32::try_from(payload.len()).map_err(|_| Error::new(CloseReason::Oversize))?;
    Ok(Record {
        kind,
        start: true,
        end: true,
        total_len,
        offset: 0,
        data: payload.to_vec(),
    })
}
fn validate_complete_record(record: &Record, kind: RecordKind) -> Result<(), Error> {
    if record.kind != kind || !record.start || !record.end || record.offset != 0 {
        return Err(Error::new(CloseReason::SequenceViolation));
    }
    if record.total_len as usize != record.data.len() {
        return Err(Error::new(CloseReason::Malformed));
    }
    Ok(())
}
fn fragment_records(kind: RecordKind, payload: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    if payload.is_empty() || payload.len() > u32::MAX as usize {
        return Err(Error::new(CloseReason::Oversize));
    }
    let total = payload.len() as u32;
    payload
        .chunks(RECORD_DATA_MAX)
        .enumerate()
        .map(|(index, chunk)| {
            let offset = index
                .checked_mul(RECORD_DATA_MAX)
                .ok_or(Error::new(CloseReason::Oversize))?;
            Record {
                kind,
                start: index == 0,
                end: offset + chunk.len() == payload.len(),
                total_len: if index == 0 { total } else { 0 },
                offset: offset as u32,
                data: chunk.to_vec(),
            }
            .encode()
        })
        .collect()
}
fn finished(binding: &SessionBinding, role: u8, session_hash: [u8; 32]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out);
    e.map(6)?
        .u8(0)?
        .u8(1)?
        .u8(1)?
        .u8(role)?
        .u8(2)?
        .bytes(&binding.lease_id)?
        .u8(3)?
        .u64(binding.process_generation)?
        .u8(4)?
        .u64(binding.listener_epoch)?
        .u8(5)?
        .bytes(&session_hash)?;
    Ok(out)
}
fn parse_finished(
    bytes: &[u8],
    binding: &SessionBinding,
    session_hash: [u8; 32],
) -> Result<u8, Error> {
    let mut d = Decoder::new(bytes);
    if d.map()? != Some(6) {
        return Err(Error::new(CloseReason::Malformed));
    }
    let mut role = None;
    for key in 0..6 {
        if d.u8()? != key {
            return Err(Error::new(CloseReason::Malformed));
        }
        match key {
            0 => {
                if d.u8()? != 1 {
                    return Err(Error::new(CloseReason::Malformed));
                }
            }
            1 => role = Some(d.u8()?),
            2 => {
                if array16(d.bytes()?)? != binding.lease_id {
                    return Err(Error::new(CloseReason::BindingMismatch));
                }
            }
            3 => {
                if d.u64()? != binding.process_generation {
                    return Err(Error::new(CloseReason::BindingMismatch));
                }
            }
            4 => {
                if d.u64()? != binding.listener_epoch {
                    return Err(Error::new(CloseReason::BindingMismatch));
                }
            }
            5 => {
                if array32(d.bytes()?)? != session_hash {
                    return Err(Error::new(CloseReason::BindingMismatch));
                }
            }
            _ => unreachable!(),
        }
    }
    if d.position() != bytes.len()
        || finished(
            binding,
            role.ok_or(Error::new(CloseReason::Malformed))?,
            session_hash,
        )? != bytes
    {
        return Err(Error::new(CloseReason::Malformed));
    }
    match role {
        Some(0 | 1) => Ok(role.expect("matched")),
        _ => Err(Error::new(CloseReason::Malformed)),
    }
}
fn heartbeat_payload(role: u8, binding: &LeaseBinding, counter: u64) -> Result<Vec<u8>, Error> {
    if !matches!(role, 1 | 2) || counter == 0 {
        return Err(Error::new(CloseReason::SequenceViolation));
    }
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out);
    e.map(5)?
        .u8(0)?
        .u8(role)?
        .u8(1)?
        .bytes(&binding.lease_id)?
        .u8(2)?
        .u64(binding.process_generation)?
        .u8(3)?
        .u64(binding.listener_epoch)?
        .u8(4)?
        .u64(counter)?;
    Ok(out)
}
fn parse_heartbeat(bytes: &[u8], role: u8, binding: &LeaseBinding) -> Result<u64, Error> {
    let mut d = Decoder::new(bytes);
    if d.map()? != Some(5) || d.u8()? != 0 || d.u8()? != role || d.u8()? != 1 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let lease_id = array16(d.bytes()?)?;
    if d.u8()? != 2 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let generation = d.u64()?;
    if d.u8()? != 3 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let epoch = d.u64()?;
    if d.u8()? != 4 {
        return Err(Error::new(CloseReason::Malformed));
    }
    let counter = d.u64()?;
    if lease_id != binding.lease_id
        || generation != binding.process_generation
        || epoch != binding.listener_epoch
    {
        return Err(Error::new(CloseReason::BindingMismatch));
    }
    if counter == 0
        || d.position() != bytes.len()
        || heartbeat_payload(role, binding, counter)? != bytes
    {
        return Err(Error::new(CloseReason::Malformed));
    }
    Ok(counter)
}
fn close_payload(reason: CloseReason, handoff: HandoffState) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out);
    e.map(3)?
        .u8(0)?
        .u8(1)?
        .u8(1)?
        .u8(reason as u8)?
        .u8(2)?
        .u8(handoff as u8)?;
    Ok(out)
}

fn limit_close_plaintext(handoff: HandoffState) -> Result<Vec<u8>, Error> {
    complete_record(
        RecordKind::Close,
        &close_payload(CloseReason::RecordLimit, handoff)?,
    )?
    .encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, BTreeSet};

    struct LiteralFields {
        context: String,
        fields: BTreeMap<String, Value>,
    }

    impl LiteralFields {
        fn new(context: impl Into<String>, value: Value) -> Self {
            let context = context.into();
            let fields = value
                .as_object()
                .unwrap_or_else(|| panic!("{context} must be an object"))
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            Self { context, fields }
        }

        fn take(&mut self, key: &str) -> Value {
            self.fields
                .remove(key)
                .unwrap_or_else(|| panic!("{} missing field {key}", self.context))
        }

        fn string(&mut self, key: &str) -> String {
            self.take(key)
                .as_str()
                .unwrap_or_else(|| panic!("{} field {key} must be a string", self.context))
                .to_owned()
        }

        fn u64(&mut self, key: &str) -> u64 {
            self.take(key)
                .as_u64()
                .unwrap_or_else(|| panic!("{} field {key} must be a u64", self.context))
        }

        fn finish(self) {
            assert!(
                self.fields.is_empty(),
                "{} has unknown or unconsumed fields: {:?}",
                self.context,
                self.fields.keys().collect::<Vec<_>>()
            );
        }
    }

    fn assert_transcript_literals(input: &Value, fields: &mut LiteralFields) {
        assert_eq!(
            fields.string("expected_transcript_encoding"),
            "canonical_json_utf8"
        );
        let canonical = serde_json::to_vec(input).expect("canonical vector input");
        assert_eq!(hex(&fields.string("expected_transcript_hex")), canonical);
        assert_eq!(
            fields.string("expected_transcript_sha256"),
            format!("sha256:{:x}", Sha256::digest(&canonical))
        );
    }

    fn assert_case_envelope(value: Value, source: &str) -> (String, Value) {
        let mut case = LiteralFields::new(source, value);
        let id = case.string("id");
        let input = case.take("canonical_input");
        assert_eq!(case.string("classification"), "TEST-ONLY");
        assert_eq!(case.string("test_only_secret_hex"), "41".repeat(32));
        let oracle = case.string("oracle");
        assert!(matches!(
            oracle.as_str(),
            "snow" | "parser_lifecycle_contract"
        ));
        let validator = case.string("validator");
        assert!(!validator.is_empty());
        assert_transcript_literals(&input, &mut case);
        let crypto = case.take("cryptographic_expected_bytes_hex");
        let expected_result = case.string("expected_result");
        let expected_reason = case.string("expected_close_reason");
        if oracle == "snow" {
            let raw_field = match id.as_str() {
                "tamper-nk-message-2" | "replay-session-finished" => "raw_outer_hex",
                "authenticated-session-binding-mismatch" => "target_finished_outer_hex",
                "equal-generation-control"
                | "equal-role-control"
                | "cross-target"
                | "cross-lease"
                | "cross-generation"
                | "old-epoch"
                | "wrong-role" => "original_handshake_m1_outer_hex",
                _ => panic!("{id} has cryptographic bytes without closed raw-field dispatch"),
            };
            assert_eq!(
                crypto.as_str().expect("cryptographic bytes must be hex"),
                input[raw_field]
                    .as_str()
                    .unwrap_or_else(|| panic!("{id} missing cryptographic raw field {raw_field}")),
                "{id} cryptographic_expected_bytes_hex is not bound to {raw_field}"
            );
        } else {
            assert!(
                crypto.is_null(),
                "{id} fabricates cryptographic expected bytes"
            );
        }
        match id.as_str() {
            "equal-generation-control" | "equal-role-control" => {
                assert_eq!(expected_result, "accepted");
                assert_eq!(expected_reason, "none");
            }
            "close-record-valid" => {
                assert_eq!(expected_result, "accepted");
                assert_eq!(expected_reason, "none");
            }
            _ => {
                assert_eq!(expected_result, "rejected", "{id}");
                assert_ne!(expected_reason, "none", "{id}");
            }
        }
        if let Some(expected_error_kind) = case.fields.remove("expected_error_kind") {
            assert_eq!(expected_error_kind, "sessionExpired", "{id}");
        }
        case.finish();
        (id, input)
    }

    fn assert_closed_vector_documents() {
        let documents = [
            (
                "frame",
                serde_json::from_str::<Value>(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../contracts/v1/vectors/frame-failures.json"
                )))
                .expect("frame vectors"),
            ),
            (
                "binding",
                serde_json::from_str::<Value>(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../contracts/v1/vectors/binding-replay-failures.json"
                )))
                .expect("binding vectors"),
            ),
        ];
        let mut seen = BTreeSet::new();
        for (name, document) in documents {
            let mut root = LiteralFields::new(name, document);
            assert_eq!(root.string("schema_version"), "1.0");
            assert!(!root.string("suite").is_empty());
            assert!(!root.string("oracle").is_empty());
            let mut material =
                LiteralFields::new("test_only_material", root.take("test_only_material"));
            assert_eq!(material.string("classification"), "TEST-ONLY");
            assert_eq!(material.string("production_use"), "forbidden");
            let mut secrets =
                LiteralFields::new("test_only_material.material", material.take("material"));
            assert_eq!(
                secrets.string("process_bootstrap_secret_hex"),
                "41".repeat(32)
            );
            secrets.finish();
            material.finish();
            for value in root
                .take("vectors")
                .as_array()
                .expect("vector array")
                .iter()
                .cloned()
            {
                let (id, input) = assert_case_envelope(value, name);
                assert!(seen.insert(id.clone()), "duplicate vector id {id}");
                let mut input = LiteralFields::new(format!("{name}:{id}:canonical_input"), input);
                let expected_fields: &[&str] = match id.as_str() {
                    "outer-header-timeout" | "outer-body-timeout" | "outer-zero-length" => {
                        &["elapsed_ms", "raw_outer_hex"]
                    }
                    "outer-oversize" => &["declared_ciphertext_length"],
                    "record-truncated-header"
                    | "record-trailing-after-end"
                    | "record-reorder"
                    | "record-gap"
                    | "record-overlap"
                    | "record-interleave"
                    | "record-unknown-flags"
                    | "record-nonzero-reserved"
                    | "record-non-start-total-len"
                    | "close-record-valid"
                    | "close-record-invalid-reason"
                    | "close-record-missing-handoff"
                    | "close-record-non-shortest-cbor" => &["max_message_bytes", "records_hex"],
                    "half-duplex-peer-turn" => &[
                        "incoming_application_role",
                        "max_message_bytes",
                        "owner",
                        "records_hex",
                    ],
                    "cbor-duplicate-key"
                    | "cbor-non-shortest-integer"
                    | "cbor-out-of-order-key" => &["raw_hex"],
                    "request-oversize" | "response-oversize" | "session-open-oversize" => {
                        &["total_len"]
                    }
                    "nonce-record-limit" => &["accepted_records", "next_record"],
                    "plaintext-byte-limit" => &["accepted_plaintext_bytes", "next_plaintext_bytes"],
                    "equal-generation-control"
                    | "cross-lease"
                    | "cross-generation"
                    | "old-epoch" => &[
                        "authenticated_session_opened",
                        "local_binding_difference",
                        "mismatched_prologue_hex",
                        "original_handshake_m1_outer_hex",
                        "stage",
                    ],
                    "equal-role-control" | "wrong-role" => {
                        &["original_handshake_m1_outer_hex", "session_vector"]
                    }
                    "cross-target" => &[
                        "broker_static_private_hex",
                        "mismatched_prologue_hex",
                        "original_handshake_m1_outer_hex",
                    ],
                    "tamper-nk-message-2" => &["bootstrap_vector", "raw_outer_hex"],
                    "replay-session-finished" => &[
                        "raw_outer_hex",
                        "repeat_count",
                        "replay_timing",
                        "session_vector",
                    ],
                    "authenticated-session-binding-mismatch" => &[
                        "session_vector",
                        "stored_generation",
                        "target_finished_outer_hex",
                    ],
                    _ => panic!("unknown vector id {id}"),
                };
                assert_eq!(
                    input
                        .fields
                        .keys()
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>(),
                    expected_fields.iter().copied().collect::<BTreeSet<_>>(),
                    "{id} input field dispatch is not closed"
                );
                match id.as_str() {
                    "request-oversize" | "response-oversize" | "session-open-oversize" => {
                        let total_len = input.u64("total_len") as u32;
                        let cap = match id.as_str() {
                            "request-oversize" => SessionLimits::default().request_bytes,
                            "response-oversize" => SessionLimits::default().response_bytes,
                            "session-open-oversize" => SessionLimits::default().first_open_bytes,
                            _ => unreachable!(),
                        };
                        let mut reassembly = RecordReassembler::new(cap).expect("literal cap");
                        let record = Record {
                            kind: RecordKind::Application,
                            start: true,
                            end: false,
                            total_len,
                            offset: 0,
                            data: Vec::new(),
                        };
                        assert_eq!(
                            reassembly
                                .accept(record)
                                .expect_err("literal total_len exceeds cap")
                                .close_reason(),
                            CloseReason::Oversize,
                            "{id}"
                        );
                    }
                    "nonce-record-limit" => {
                        let accepted = input.u64("accepted_records");
                        let next = input.u64("next_record");
                        assert_eq!(accepted.checked_add(1), Some(next));
                        let mut reassembly = RecordReassembler::new(1).expect("literal cap");
                        reassembly.set_usage(accepted, 0);
                        assert_eq!(
                            reassembly
                                .accept(
                                    complete_record(RecordKind::Application, b"x").expect("record")
                                )
                                .expect_err("literal next record reaches cap")
                                .close_reason(),
                            CloseReason::RecordLimit
                        );
                    }
                    "plaintext-byte-limit" => {
                        let accepted = input.u64("accepted_plaintext_bytes");
                        let next = input.u64("next_plaintext_bytes");
                        assert_eq!(accepted.checked_add(next), Some(BYTE_LIMIT));
                        let (_target, mut broker) = open_test_sessions();
                        broker.core.set_usage(0, accepted, 0, 0);
                        assert_eq!(
                            broker
                                .write_session_open(b"x")
                                .expect_err("literal plaintext usage reaches cap")
                                .close_reason(),
                            CloseReason::RecordLimit
                        );
                    }
                    _ => {
                        for key in expected_fields {
                            input.take(key);
                        }
                    }
                }
                input.finish();
            }
            root.finish();
        }
    }

    fn assert_closed_positive_vector_documents() {
        let bootstrap: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/bootstrap-nk-success.json"
        )))
        .expect("bootstrap vector");
        let binding = bootstrap_binding_from_vector(&bootstrap);
        let mut root = LiteralFields::new("bootstrap", bootstrap);
        assert_eq!(root.string("schema_version"), "1.0");
        assert_eq!(root.string("suite"), "bootstrap-nk-success");
        assert_eq!(root.string("oracle"), "snow-0.10.0");
        let mut material =
            LiteralFields::new("bootstrap material", root.take("test_only_material"));
        assert_eq!(material.string("classification"), "TEST-ONLY");
        assert_eq!(material.string("production_use"), "forbidden");
        let mut keys = LiteralFields::new("bootstrap keys", material.take("material"));
        assert_eq!(keys.string("process_bootstrap_secret_hex"), "41".repeat(32));
        assert_eq!(keys.string("broker_static_private_hex"), "11".repeat(32));
        assert_eq!(keys.string("target_ephemeral_private_hex"), "21".repeat(32));
        assert_eq!(keys.string("broker_ephemeral_private_hex"), "31".repeat(32));
        assert_eq!(keys.string("target_reference_random_hex"), "61".repeat(32));
        assert_eq!(keys.string("broker_static_public_hex").len(), 64);
        keys.finish();
        material.finish();
        let mut canonical = LiteralFields::new("bootstrap canonical", root.take("canonical_input"));
        assert_eq!(
            canonical.string("noise_name"),
            "Noise_NK_25519_ChaChaPoly_SHA256"
        );
        assert_eq!(
            hex(&canonical.string("prologue_cbor_hex")),
            bootstrap_prologue(&binding).expect("bootstrap prologue")
        );
        assert_eq!(
            hex(&canonical.string("m1_payload_cbor_hex")),
            binding.m1_payload().expect("M1 payload")
        );
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        assert_eq!(
            hex(&canonical.string("m2_payload_cbor_hex")),
            binding.expected_m2(&pbs).expect("M2 payload")
        );
        assert_eq!(
            canonical.string("target_reference_digest_hex"),
            hex_encode(&binding.target_reference_digest)
        );
        assert_eq!(
            canonical.string("target_reference"),
            "target_YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE"
        );
        assert_eq!(canonical.string("launch_platform"), "ios_simulator");
        assert_eq!(
            canonical.take("launch_endpoint"),
            serde_json::json!({"host":"127.0.0.1","port":55_001})
        );
        assert!(!canonical.string("launch_descriptor_cbor_hex").is_empty());
        assert!(!canonical.string("ack_payload_cbor_hex").is_empty());
        canonical.finish();
        let mut expected = LiteralFields::new("bootstrap expected", root.take("expected"));
        let frames = [
            expected.string("m1_outer_hex"),
            expected.string("m2_outer_hex"),
            expected.string("ack_outer_hex"),
        ];
        let transcript = frames
            .iter()
            .flat_map(|frame| hex(frame))
            .collect::<Vec<_>>();
        assert_eq!(hex(&expected.string("transcript_hex")), transcript);
        assert_eq!(
            expected.string("transcript_sha256"),
            format!("sha256:{:x}", Sha256::digest(&transcript))
        );
        let handshake_hash = hex(&expected.string("noise_handshake_hash_hex"));
        assert_eq!(
            expected.string("noise_handshake_hash_sha256"),
            format!("sha256:{:x}", Sha256::digest(&handshake_hash))
        );
        assert_eq!(expected.string("result"), "process_bootstrap_acknowledged");
        assert!(expected.take("close_reason").is_null());
        expected.finish();
        root.finish();

        let session: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/session-nnpsk0-success.json"
        )))
        .expect("session vector");
        let binding = session_binding_from_vector(&session);
        let mut root = LiteralFields::new("session", session);
        assert_eq!(root.string("schema_version"), "1.0");
        assert_eq!(root.string("suite"), "session-nnpsk0-success");
        assert_eq!(root.string("oracle"), "snow-0.10.0");
        let mut material = LiteralFields::new("session material", root.take("test_only_material"));
        assert_eq!(material.string("classification"), "TEST-ONLY");
        assert_eq!(material.string("production_use"), "forbidden");
        let mut keys = LiteralFields::new("session keys", material.take("material"));
        assert_eq!(keys.string("process_bootstrap_secret_hex"), "41".repeat(32));
        assert_eq!(keys.string("target_ephemeral_private_hex"), "91".repeat(32));
        assert_eq!(keys.string("broker_ephemeral_private_hex"), "a1".repeat(32));
        keys.finish();
        material.finish();
        let mut canonical = LiteralFields::new("session canonical", root.take("canonical_input"));
        assert_eq!(
            canonical.string("noise_name"),
            "Noise_NNpsk0_25519_ChaChaPoly_SHA256"
        );
        assert_eq!(
            hex(&canonical.string("prologue_cbor_hex")),
            session_prologue(&binding, SessionLimits::default()).expect("session prologue")
        );
        assert!(canonical.string("handshake_m1_payload_hex").is_empty());
        assert!(canonical.string("handshake_m2_payload_hex").is_empty());
        for field in [
            "target_finished_cbor_hex",
            "broker_finished_cbor_hex",
            "session_open_utf8_hex",
            "session_open_response_utf8_hex",
        ] {
            assert!(!canonical.string(field).is_empty());
        }
        canonical.finish();
        let mut expected = LiteralFields::new("session expected", root.take("expected"));
        let frame_names = [
            "m1_outer_hex",
            "m2_outer_hex",
            "target_finished_outer_hex",
            "broker_finished_outer_hex",
            "session_open_outer_hex",
            "session_open_response_outer_hex",
        ];
        let transcript = frame_names
            .iter()
            .flat_map(|field| hex(&expected.string(field)))
            .collect::<Vec<_>>();
        assert_eq!(hex(&expected.string("transcript_hex")), transcript);
        assert_eq!(
            expected.string("transcript_sha256"),
            format!("sha256:{:x}", Sha256::digest(&transcript))
        );
        let handshake_hash = hex(&expected.string("noise_handshake_hash_hex"));
        assert_eq!(
            expected.string("noise_handshake_hash_sha256"),
            format!("sha256:{:x}", Sha256::digest(&handshake_hash))
        );
        assert_eq!(expected.string("result"), "opened_protocol_session");
        assert_eq!(
            expected.string("target_issued_session_id"),
            "session_test_0123456789abcdef"
        );
        assert_eq!(
            expected.u64("target_process_generation"),
            4_503_599_627_370_123
        );
        assert_eq!(
            expected.take("negotiated_protocol"),
            serde_json::json!({"major":1,"minor":2})
        );
        assert_eq!(
            expected.take("negotiated_capabilities"),
            serde_json::json!(["semantic.catalog", "session.core"])
        );
        assert_eq!(
            expected.take("negotiated_limits"),
            serde_json::json!({
                "maxRequestBytes":16_777_216,
                "maxResponseBytes":67_108_864,
                "maxPageItems":10_000
            })
        );
        assert!(expected.take("close_reason").is_null());
        expected.finish();
        root.finish();
    }

    fn hex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("vector hex"))
            .collect()
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn fixed<const N: usize>(value: &str) -> [u8; N] {
        hex(value).try_into().expect("fixed vector size")
    }

    fn bootstrap_binding() -> BootstrapBinding {
        BootstrapBinding {
            target_reference_digest: fixed(
                "791b63ed11406e77475fafbf092c8dc786d728ed0773d7662373741dea079404",
            ),
            lease_id: [0x51; 16],
            target_nonce: [0x71; 32],
            app_artifact_digest: [0x81; 32],
            expiry_ms: 1_893_456_000_000,
        }
    }

    fn session_binding() -> SessionBinding {
        SessionBinding {
            lease_id: [0x51; 16],
            process_generation: 4_503_599_627_370_123,
            listener_epoch: 1,
            nk_handshake_hash: fixed(
                "bdbe3608b37171ada0f659120e3ae5a3a88436caec378f32ac783a0c685a9e11",
            ),
        }
    }

    fn bootstrap_binding_from_vector(vector: &Value) -> BootstrapBinding {
        let bytes = hex(vector["canonical_input"]["m2_payload_cbor_hex"]
            .as_str()
            .expect("M2 payload literal"));
        let mut decoder = MiniDecoder::new(&bytes);
        assert_eq!(decoder.map().expect("M2 map"), Some(7));
        assert_eq!(decoder.u8().expect("version key"), 0);
        assert_eq!(decoder.u8().expect("version"), 1);
        assert_eq!(decoder.u8().expect("PBS key"), 1);
        assert_eq!(decoder.bytes().expect("PBS").len(), 32);
        assert_eq!(decoder.u8().expect("digest key"), 2);
        let target_reference_digest = decoder
            .bytes()
            .expect("target reference digest")
            .try_into()
            .expect("digest size");
        assert_eq!(decoder.u8().expect("lease key"), 3);
        let lease_id = decoder
            .bytes()
            .expect("lease id")
            .try_into()
            .expect("lease size");
        assert_eq!(decoder.u8().expect("nonce key"), 4);
        let target_nonce = decoder
            .bytes()
            .expect("target nonce")
            .try_into()
            .expect("nonce size");
        assert_eq!(decoder.u8().expect("expiry key"), 5);
        let expiry_ms = decoder.u64().expect("expiry");
        assert_eq!(decoder.u8().expect("artifact key"), 6);
        let app_artifact_digest = decoder
            .bytes()
            .expect("artifact digest")
            .try_into()
            .expect("artifact digest size");
        assert_eq!(decoder.position(), bytes.len());
        BootstrapBinding {
            target_reference_digest,
            lease_id,
            target_nonce,
            app_artifact_digest,
            expiry_ms,
        }
    }

    fn session_binding_from_vector(vector: &Value) -> SessionBinding {
        let bytes = hex(vector["canonical_input"]["prologue_cbor_hex"]
            .as_str()
            .expect("session prologue literal"));
        let mut decoder = MiniDecoder::new(&bytes);
        assert_eq!(decoder.array().expect("prologue array"), Some(12));
        assert_eq!(decoder.str().expect("namespace"), "apppilotkit.transport");
        assert_eq!(decoder.u8().expect("version"), 1);
        assert_eq!(decoder.str().expect("purpose"), "session");
        assert_eq!(decoder.u8().expect("initiator role"), 0);
        assert_eq!(decoder.u8().expect("responder role"), 1);
        let lease_id = decoder
            .bytes()
            .expect("lease id")
            .try_into()
            .expect("lease size");
        let process_generation = decoder.u64().expect("generation");
        let listener_epoch = decoder.u64().expect("epoch");
        assert_eq!(
            decoder.u64().expect("request cap"),
            SessionLimits::default().request_bytes as u64
        );
        assert_eq!(
            decoder.u64().expect("response cap"),
            SessionLimits::default().response_bytes as u64
        );
        assert_eq!(decoder.u64().expect("handshake cap"), 8192);
        let nk_handshake_hash = decoder
            .bytes()
            .expect("NK hash")
            .try_into()
            .expect("NK hash size");
        assert_eq!(decoder.position(), bytes.len());
        SessionBinding {
            lease_id,
            process_generation,
            listener_epoch,
            nk_handshake_hash,
        }
    }

    fn open_test_sessions() -> (TargetSession, BrokerSession) {
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0xa1; 32],
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let m2 = broker.read_m1_write_m2(&m1).expect("m2");
        target.read_m2(&m2).expect("target split");
        let finished = target.write_finished().expect("target finished");
        broker
            .read_finished(&finished)
            .expect("broker reads finished");
        let finished = broker.write_finished().expect("broker finished");
        target
            .read_finished(&finished)
            .expect("target reads finished");
        (target, broker)
    }

    fn open_test_leases() -> (TargetLeaseConnection, BrokerLeaseConnection) {
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetBootstrap::new_test(
            bootstrap_binding(),
            fixed("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13"),
            &[0x21; 32],
        )
        .expect("Target NK");
        let broker = BrokerBootstrap::new_test(bootstrap_binding(), &[0x11; 32], &pbs, &[0x31; 32])
            .expect("Broker NK");
        let m1 = target.write_m1().expect("NK M1");
        let (m2, broker_ack) = broker.read_m1_write_m2(&m1).expect("NK M2");
        let (target_ack, _) = target
            .read_m2(&m2, 4_503_599_627_370_123, 1)
            .expect("Target split");
        let (ack, target_lease) = target_ack.write_ack().expect("ACK");
        let (_, broker_lease) = broker_ack.read_ack(&ack).expect("Broker ACK");
        (target_lease, broker_lease)
    }

    #[test]
    fn outer_frames_match_accepted_literals() {
        let vectors: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/frame-failures.json"
        )))
        .expect("accepted vectors");
        for vector in vectors["vectors"].as_array().expect("vector list") {
            let input = &vector["canonical_input"];
            if let Some(raw) = input.get("raw_outer_hex").and_then(Value::as_str) {
                let mut decoder = OuterFrameDecoder::new();
                let result = decoder.push(&hex(raw));
                let expected = vector["expected_close_reason"].as_str().expect("reason");
                match expected {
                    "malformed" => assert_eq!(
                        result.expect_err("must reject").close_reason(),
                        CloseReason::Malformed,
                        "{}",
                        vector["id"]
                    ),
                    "timeout" => assert_eq!(
                        decoder.timeout().expect_err("must time out").close_reason(),
                        CloseReason::Timeout,
                        "{}",
                        vector["id"]
                    ),
                    _ => {}
                }
            }
            if let Some(length) = input
                .get("declared_ciphertext_length")
                .and_then(Value::as_u64)
            {
                assert_eq!(
                    encode_outer(&vec![0; length as usize])
                        .expect_err("oversize")
                        .close_reason(),
                    CloseReason::Oversize,
                    "{}",
                    vector["id"]
                );
            }
            if let Some(raw) = input.get("raw_hex").and_then(Value::as_str) {
                assert_eq!(
                    parse_close(&hex(raw))
                        .expect_err("non-canonical CBOR")
                        .close_reason(),
                    CloseReason::Malformed,
                    "{}",
                    vector["id"]
                );
            }
        }
    }

    #[test]
    fn record_reassembly_matches_accepted_failure_literals() {
        let vectors: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/frame-failures.json"
        )))
        .expect("accepted vectors");
        for vector in vectors["vectors"].as_array().expect("vector list") {
            let input = &vector["canonical_input"];
            let Some(records) = input.get("records_hex").and_then(Value::as_array) else {
                continue;
            };
            let cap = input["max_message_bytes"].as_u64().expect("cap") as usize;
            let mut assembler = RecordReassembler::new(cap).expect("cap valid");
            let mut turns = HalfDuplex::new(if vector["id"] == "half-duplex-peer-turn" {
                ApplicationTurn::Local
            } else {
                ApplicationTurn::Peer
            });
            let mut result = Ok(None);
            for record in records {
                result = Record::decode(&hex(record.as_str().expect("hex")))
                    .and_then(|record| turns.accept_peer_record(&mut assembler, record));
                if result.is_err() {
                    break;
                }
            }
            if vector["expected_result"] == "rejected" {
                let expected = match vector["expected_close_reason"].as_str().expect("reason") {
                    "malformed" => CloseReason::Malformed,
                    "sequenceViolation" => CloseReason::SequenceViolation,
                    "oversize" => CloseReason::Oversize,
                    "recordLimit" => CloseReason::RecordLimit,
                    other => panic!("unhandled reason {other}"),
                };
                assert_eq!(
                    result.expect_err("must reject").close_reason(),
                    expected,
                    "{}",
                    vector["id"]
                );
            } else {
                let (kind, bytes) = result
                    .expect("accepted vector must parse")
                    .expect("accepted vector must complete");
                assert_eq!(kind, RecordKind::Close, "{}", vector["id"]);
                assert_eq!(
                    parse_close(&bytes).expect("accepted close payload"),
                    (
                        CloseReason::BrokerLost,
                        HandoffState::HandoffPossibleOrConfirmed
                    ),
                    "{}",
                    vector["id"]
                );
            }
        }
    }

    #[test]
    fn bootstrap_nk_matches_accepted_vector_literals() {
        let vector: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/bootstrap-nk-success.json"
        )))
        .expect("accepted vector");
        let binding = bootstrap_binding_from_vector(&vector);
        let material = &vector["test_only_material"]["material"];
        let descriptor = hex(vector["canonical_input"]["launch_descriptor_cbor_hex"]
            .as_str()
            .expect("launch descriptor"));
        let mut descriptor_decoder = MiniDecoder::new(&descriptor);
        assert_eq!(descriptor_decoder.map().expect("descriptor map"), Some(9));
        assert_eq!(descriptor_decoder.u8().expect("version key"), 0);
        assert_eq!(descriptor_decoder.u8().expect("version"), 1);
        assert_eq!(descriptor_decoder.u8().expect("platform key"), 1);
        assert_eq!(descriptor_decoder.u8().expect("iOS platform"), 0);
        assert_eq!(descriptor_decoder.u8().expect("lease key"), 2);
        assert_eq!(descriptor_decoder.bytes().expect("lease"), binding.lease_id);
        assert_eq!(descriptor_decoder.u8().expect("nonce key"), 3);
        assert_eq!(
            descriptor_decoder.bytes().expect("nonce"),
            binding.target_nonce
        );
        assert_eq!(descriptor_decoder.u8().expect("App digest key"), 4);
        assert_eq!(
            descriptor_decoder.bytes().expect("App digest"),
            binding.app_artifact_digest
        );
        assert_eq!(descriptor_decoder.u8().expect("static key"), 5);
        assert_eq!(
            descriptor_decoder.bytes().expect("static public"),
            fixed::<32>(
                material["broker_static_public_hex"]
                    .as_str()
                    .expect("static public literal")
            )
        );
        assert_eq!(descriptor_decoder.u8().expect("endpoint key"), 6);
        assert_eq!(descriptor_decoder.map().expect("iOS endpoint"), Some(2));
        assert_eq!(descriptor_decoder.u8().expect("host key"), 0);
        assert_eq!(descriptor_decoder.str().expect("host"), "127.0.0.1");
        assert_eq!(descriptor_decoder.u8().expect("port key"), 1);
        assert_eq!(descriptor_decoder.u64().expect("port"), 55_001);
        assert_eq!(descriptor_decoder.u8().expect("expiry key"), 7);
        assert_eq!(descriptor_decoder.u64().expect("expiry"), binding.expiry_ms);
        assert_eq!(descriptor_decoder.u8().expect("reference digest key"), 8);
        assert_eq!(
            descriptor_decoder.bytes().expect("reference digest"),
            binding.target_reference_digest
        );
        assert_eq!(descriptor_decoder.position(), descriptor.len());
        assert_eq!(
            bootstrap_prologue(&binding).expect("prologue"),
            hex(vector["canonical_input"]["prologue_cbor_hex"]
                .as_str()
                .expect("hex"))
        );
        let pbs = ProcessBootstrapSecret::new(fixed(
            material["process_bootstrap_secret_hex"]
                .as_str()
                .expect("PBS literal"),
        ));
        let mut target = TargetBootstrap::new_test(
            binding.clone(),
            fixed(
                material["broker_static_public_hex"]
                    .as_str()
                    .expect("static public literal"),
            ),
            &fixed(
                material["target_ephemeral_private_hex"]
                    .as_str()
                    .expect("target ephemeral literal"),
            ),
        )
        .expect("target");
        let broker = BrokerBootstrap::new_test(
            binding.clone(),
            &fixed(
                material["broker_static_private_hex"]
                    .as_str()
                    .expect("static private literal"),
            ),
            &pbs,
            &fixed(
                material["broker_ephemeral_private_hex"]
                    .as_str()
                    .expect("broker ephemeral literal"),
            ),
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        assert_eq!(
            m1,
            hex(vector["expected"]["m1_outer_hex"].as_str().expect("hex"))
        );
        let (m2, broker_ack) = broker.read_m1_write_m2(&m1).expect("m2");
        assert_eq!(
            m2,
            hex(vector["expected"]["m2_outer_hex"].as_str().expect("hex"))
        );
        let (target_ack, delivered_pbs) = target
            .read_m2(&m2, 4_503_599_627_370_123, 1)
            .expect("accept m2");
        assert_eq!(delivered_pbs.bytes(), pbs.bytes());
        let (ack_outer, mut target_lease) = target_ack.write_ack().expect("ack");
        assert_eq!(
            ack_outer,
            hex(vector["expected"]["ack_outer_hex"].as_str().expect("hex"))
        );
        let (ack, mut broker_lease) = broker_ack.read_ack(&ack_outer).expect("verify ack");
        assert_eq!(
            ack.nk_handshake_hash,
            fixed(
                vector["expected"]["noise_handshake_hash_hex"]
                    .as_str()
                    .expect("hex")
            )
        );
        assert_eq!(ack.process_generation, 4_503_599_627_370_123);
        assert_eq!(ack.listener_epoch, 1);
        let heartbeat = broker_lease
            .write_heartbeat_request(1)
            .expect("heartbeat request");
        assert_eq!(
            target_lease
                .read_heartbeat_request(&heartbeat)
                .expect("request"),
            1
        );
        let reply = target_lease
            .write_heartbeat_reply(1)
            .expect("heartbeat reply");
        assert_eq!(broker_lease.read_heartbeat_reply(&reply).expect("reply"), 1);
        let close = broker_lease
            .write_close(CloseReason::Normal, HandoffState::NotHandedOff)
            .expect("close");
        let close_error = target_lease
            .read_heartbeat_request(&close)
            .expect_err("close while awaiting heartbeat");
        assert_eq!(
            close_error.peer_close_details(),
            Some((CloseReason::Normal, HandoffState::NotHandedOff))
        );
    }

    #[test]
    fn session_nnpsk0_matches_accepted_vector_literals() {
        let vector: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/session-nnpsk0-success.json"
        )))
        .expect("accepted vector");
        let canonical = &vector["canonical_input"];
        let expected = &vector["expected"];
        let binding = session_binding_from_vector(&vector);
        let material = &vector["test_only_material"]["material"];
        assert_eq!(
            session_prologue(&binding, SessionLimits::default()).expect("prologue"),
            hex(canonical["prologue_cbor_hex"].as_str().expect("hex"))
        );
        let pbs = ProcessBootstrapSecret::new(fixed(
            material["process_bootstrap_secret_hex"]
                .as_str()
                .expect("PBS literal"),
        ));
        let mut target = TargetSession::new_test(
            binding.clone(),
            &pbs,
            SessionLimits::default(),
            &fixed(
                material["target_ephemeral_private_hex"]
                    .as_str()
                    .expect("target ephemeral literal"),
            ),
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            binding,
            &pbs,
            SessionLimits::default(),
            &fixed(
                material["broker_ephemeral_private_hex"]
                    .as_str()
                    .expect("broker ephemeral literal"),
            ),
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        assert_eq!(m1, hex(expected["m1_outer_hex"].as_str().expect("hex")));
        let m2 = broker.read_m1_write_m2(&m1).expect("m2");
        assert_eq!(m2, hex(expected["m2_outer_hex"].as_str().expect("hex")));
        target.read_m2(&m2).expect("split target");
        let target_finished = target.write_finished().expect("target finished");
        assert_eq!(
            target_finished,
            hex(expected["target_finished_outer_hex"].as_str().expect("hex"))
        );
        broker
            .read_finished(&target_finished)
            .expect("verify target finished");
        let broker_finished = broker.write_finished().expect("broker finished");
        assert_eq!(
            broker_finished,
            hex(expected["broker_finished_outer_hex"].as_str().expect("hex"))
        );
        target
            .read_finished(&broker_finished)
            .expect("verify broker finished");
        let open = hex(canonical["session_open_utf8_hex"].as_str().expect("hex"));
        let open_frames = broker.write_session_open(&open).expect("session.open");
        assert_eq!(
            open_frames,
            vec![hex(expected["session_open_outer_hex"]
                .as_str()
                .expect("hex"))]
        );
        assert_eq!(
            target.read_application(&open_frames[0]).expect("read open"),
            Some(open)
        );
        let response = hex(canonical["session_open_response_utf8_hex"]
            .as_str()
            .expect("hex"));
        let response_json: Value = serde_json::from_slice(&response).expect("response JSON");
        assert_eq!(response_json["jsonrpc"], "2.0");
        assert_eq!(response_json["id"], "open-contract");
        assert_eq!(
            response_json["result"]["context"]["id"],
            expected["target_issued_session_id"]
        );
        assert_eq!(
            response_json["result"]["context"]["generation"],
            expected["target_process_generation"]
        );
        assert_eq!(
            response_json["result"]["protocol"],
            expected["negotiated_protocol"]
        );
        assert_eq!(
            response_json["result"]["capabilities"],
            expected["negotiated_capabilities"]
        );
        assert_eq!(
            response_json["result"]["limits"],
            expected["negotiated_limits"]
        );
        let response_frames = target
            .write_application_response(&response)
            .expect("response");
        assert_eq!(
            response_frames,
            vec![hex(expected["session_open_response_outer_hex"]
                .as_str()
                .expect("hex"))]
        );
        assert_eq!(
            broker
                .read_application_response(&response_frames[0])
                .expect("read response"),
            Some(response)
        );
    }

    #[test]
    fn tampered_bootstrap_and_wrong_session_psk_are_rejected() {
        let bootstrap: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/bootstrap-nk-success.json"
        )))
        .expect("vector");
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetBootstrap::new_test(
            bootstrap_binding(),
            fixed("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13"),
            &[0x21; 32],
        )
        .expect("target");
        let broker = BrokerBootstrap::new_test(bootstrap_binding(), &[0x11; 32], &pbs, &[0x31; 32])
            .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let (mut m2, _) = broker.read_m1_write_m2(&m1).expect("m2");
        *m2.last_mut().expect("ciphertext") ^= 1;
        assert_eq!(
            target
                .read_m2(&m2, 4_503_599_627_370_123, 1)
                .err()
                .expect("tamper rejected")
                .close_reason(),
            CloseReason::AuthenticationFailed
        );
        assert_eq!(
            m2,
            hex(bootstrap["expected"]["m2_outer_hex"].as_str().expect("hex"))
                .into_iter()
                .enumerate()
                .map(|(i, byte)| if i + 1 == m2.len() { byte ^ 1 } else { byte })
                .collect::<Vec<_>>()
        );

        let good = ProcessBootstrapSecret::new([0x41; 32]);
        let bad = ProcessBootstrapSecret::new([0x42; 32]);
        let mut target = TargetSession::new_test(
            session_binding(),
            &good,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            session_binding(),
            &bad,
            SessionLimits::default(),
            &[0xa1; 32],
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        assert_eq!(
            broker
                .read_m1_write_m2(&m1)
                .expect_err("wrong psk")
                .close_reason(),
            CloseReason::AuthenticationFailed
        );
    }

    #[test]
    fn replay_binding_caps_fragmentation_and_close_are_fail_closed() {
        let (_, mut broker) = open_test_sessions();
        assert_eq!(
            broker
                .write_application_request(b"before open")
                .expect_err("must open first")
                .close_reason(),
            CloseReason::SequenceViolation
        );
        let (_, mut broker) = open_test_sessions();
        assert_eq!(
            broker
                .write_session_open(&vec![0; 65_537])
                .expect_err("open cap")
                .close_reason(),
            CloseReason::Oversize
        );
        let (mut target, mut broker) = open_test_sessions();
        let open_frames = broker.write_session_open(b"open").expect("open");
        assert_eq!(
            target.read_application(&open_frames[0]).expect("open read"),
            Some(b"open".to_vec())
        );
        let response_frames = target
            .write_application_response(b"opened")
            .expect("open response");
        assert_eq!(
            broker
                .read_application_response(&response_frames[0])
                .expect("response read"),
            Some(b"opened".to_vec())
        );

        let request = vec![0x33; RECORD_DATA_MAX + 100];
        let request_frames = broker
            .write_application_request(&request)
            .expect("fragmented request");
        assert_eq!(request_frames.len(), 2);
        assert_eq!(
            target
                .read_application(&request_frames[0])
                .expect("first fragment"),
            None
        );
        assert_eq!(
            target
                .read_application(&request_frames[1])
                .expect("last fragment"),
            Some(request)
        );
        let response = vec![0x44; RECORD_DATA_MAX + 200];
        let response_frames = target
            .write_application_response(&response)
            .expect("fragmented response");
        assert_eq!(response_frames.len(), 2);
        assert_eq!(
            broker
                .read_application_response(&response_frames[0])
                .expect("first fragment"),
            None
        );
        assert_eq!(
            broker
                .read_application_response(&response_frames[1])
                .expect("last fragment"),
            Some(response)
        );
        assert_eq!(
            broker
                .write_application_request(&vec![0; SessionLimits::default().request_bytes + 1])
                .expect_err("request cap")
                .close_reason(),
            CloseReason::Oversize
        );

        assert_eq!(
            target
                .validate_binding(4_503_599_627_370_124, 1)
                .expect_err("generation mismatch")
                .close_reason(),
            CloseReason::BindingMismatch
        );
        assert_eq!(
            target
                .write_application_response(b"closed")
                .expect_err("terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );

        let (mut target, mut broker) = open_test_sessions();
        let close = broker
            .write_close(CloseReason::Normal, HandoffState::NotHandedOff)
            .expect("close");
        assert_eq!(
            target.read_close(&close).expect("peer close"),
            (CloseReason::Normal, HandoffState::NotHandedOff)
        );
        assert_eq!(
            broker
                .write_close(CloseReason::Normal, HandoffState::NotHandedOff)
                .expect_err("sender terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );
        assert_eq!(
            target
                .read_close(&close)
                .expect_err("receiver terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );
    }

    #[test]
    fn record_and_plaintext_usage_caps_reject_the_next_record() {
        let record = complete_record(RecordKind::Application, b"x").expect("record");
        let mut records = RecordReassembler::new(16).expect("assembler");
        records.set_usage(RECORD_LIMIT - 1, 0);
        assert_eq!(
            records
                .accept(record.clone())
                .expect_err("record cap")
                .close_reason(),
            CloseReason::RecordLimit
        );
        let mut bytes = RecordReassembler::new(16).expect("assembler");
        bytes.set_usage(0, BYTE_LIMIT - (RECORD_HEADER_LEN + 1) as u64);
        assert_eq!(
            bytes.accept(record).expect_err("byte cap").close_reason(),
            CloseReason::RecordLimit
        );
        for cap in [16 * 1024 * 1024, 64 * 1024 * 1024] {
            let mut assembler = RecordReassembler::new(cap).expect("cap");
            let oversize = Record {
                kind: RecordKind::Application,
                start: true,
                end: false,
                total_len: (cap + 1) as u32,
                offset: 0,
                data: Vec::new(),
            };
            assert_eq!(
                assembler
                    .accept(oversize)
                    .expect_err("message cap")
                    .close_reason(),
                CloseReason::Oversize
            );
        }
    }

    #[test]
    fn immediate_finished_replay_is_authentication_failure() {
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0xa1; 32],
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let m2 = broker.read_m1_write_m2(&m1).expect("m2");
        target.read_m2(&m2).expect("split");
        let finished = target.write_finished().expect("finished");
        broker.read_finished(&finished).expect("first accepted");
        assert_eq!(
            broker
                .read_finished(&finished)
                .expect_err("replay rejected")
                .close_reason(),
            CloseReason::AuthenticationFailed
        );
    }

    #[test]
    fn authentication_failure_is_terminal_without_nonce_retry() {
        let (mut target, mut broker) = open_test_sessions();
        let frame = broker.write_session_open(b"open").expect("open").remove(0);
        let mut tampered = frame.clone();
        *tampered.last_mut().expect("ciphertext") ^= 1;
        assert_eq!(
            target
                .read_application(&tampered)
                .expect_err("tamper rejected")
                .close_reason(),
            CloseReason::AuthenticationFailed
        );
        assert_eq!(
            target
                .read_application(&frame)
                .expect_err("no retry after authentication failure")
                .close_reason(),
            CloseReason::SequenceViolation
        );
    }

    #[test]
    fn cross_binding_and_wrong_role_vectors_fail_at_handshake() {
        let vectors: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/binding-replay-failures.json"
        )))
        .expect("accepted vectors");
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        for vector in vectors["vectors"].as_array().expect("vectors") {
            let id = vector["id"].as_str().expect("id");
            if !matches!(id, "cross-lease" | "cross-generation" | "old-epoch") {
                continue;
            }
            let mut binding = session_binding();
            match id {
                "cross-lease" => binding.lease_id = [0x52; 16],
                "cross-generation" => binding.process_generation += 1,
                "old-epoch" => binding.listener_epoch = 2,
                _ => unreachable!(),
            }
            let mut broker =
                BrokerSession::new_test(binding, &pbs, SessionLimits::default(), &[0xa1; 32])
                    .expect("broker");
            let m1 = hex(vector["canonical_input"]["original_handshake_m1_outer_hex"]
                .as_str()
                .expect("m1"));
            assert_eq!(
                broker
                    .read_m1_write_m2(&m1)
                    .expect_err("cross binding rejected")
                    .close_reason(),
                CloseReason::AuthenticationFailed,
                "{id}"
            );
        }

        let cross_target = vectors["vectors"]
            .as_array()
            .expect("vectors")
            .iter()
            .find(|vector| vector["id"] == "cross-target")
            .expect("cross-target vector");
        let mut wrong_binding = bootstrap_binding();
        wrong_binding.target_reference_digest =
            fixed("17354fe29503c65dbd100847ab33b2a8c9ff0d6916d2f2d20294303cdbf29399");
        let broker = BrokerBootstrap::new_test(wrong_binding, &[0x11; 32], &pbs, &[0x31; 32])
            .expect("wrong target Broker");
        let m1 = hex(
            cross_target["canonical_input"]["original_handshake_m1_outer_hex"]
                .as_str()
                .expect("m1"),
        );
        let error = match broker.read_m1_write_m2(&m1) {
            Ok(_) => panic!("cross-target accepted"),
            Err(error) => error,
        };
        assert_eq!(error.close_reason(), CloseReason::AuthenticationFailed);

        let wrong_role = vectors["vectors"]
            .as_array()
            .expect("vectors")
            .iter()
            .find(|vector| vector["id"] == "wrong-role")
            .expect("wrong-role vector");
        let m1 = hex(
            wrong_role["canonical_input"]["original_handshake_m1_outer_hex"]
                .as_str()
                .expect("m1"),
        );
        let mut target = TargetSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        target.write_m1().expect("target m1");
        assert_eq!(
            target
                .read_m2(&m1)
                .expect_err("wrong role rejected")
                .close_reason(),
            CloseReason::AuthenticationFailed
        );
    }

    #[test]
    fn control_records_require_exact_complete_headers() {
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0xa1; 32],
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let m2 = broker.read_m1_write_m2(&m1).expect("m2");
        target.read_m2(&m2).expect("split");
        let body = finished(
            &session_binding(),
            0,
            target.core.session_hash.expect("hash"),
        )
        .expect("finished");
        let invalid = Record {
            kind: RecordKind::Finished,
            start: true,
            end: true,
            total_len: body.len() as u32,
            offset: 1,
            data: body,
        }
        .encode()
        .expect("record");
        let frame = target.core.encrypt_counted(&invalid).expect("encrypt");
        assert_eq!(
            broker
                .read_finished(&frame)
                .expect_err("offset")
                .close_reason(),
            CloseReason::SequenceViolation
        );
        assert_eq!(
            broker
                .read_finished(&frame)
                .expect_err("terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );

        let (mut target, mut broker) = open_test_sessions();
        let payload =
            close_payload(CloseReason::Normal, HandoffState::NotHandedOff).expect("close");
        let invalid = Record {
            kind: RecordKind::Close,
            start: true,
            end: true,
            total_len: payload.len() as u32 + 1,
            offset: 0,
            data: payload,
        }
        .encode()
        .expect("record");
        let frame = broker.core.encrypt_counted(&invalid).expect("encrypt");
        assert_eq!(
            target
                .read_close(&frame)
                .expect_err("total length")
                .close_reason(),
            CloseReason::Malformed
        );
        assert_eq!(
            target
                .read_close(&frame)
                .expect_err("terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );
    }

    #[test]
    fn close_received_during_application_preserves_authenticated_details() {
        let (mut target, mut broker) = open_test_sessions();
        let frame = broker
            .write_close(CloseReason::Stale, HandoffState::HandoffPossibleOrConfirmed)
            .expect("close");
        let error = target.read_application(&frame).expect_err("peer close");
        assert_eq!(error.close_reason(), CloseReason::PeerClosed);
        assert_eq!(
            error.peer_close_details(),
            Some((CloseReason::Stale, HandoffState::HandoffPossibleOrConfirmed))
        );
        assert_eq!(
            target
                .read_application(&frame)
                .expect_err("terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );
    }

    #[test]
    fn transport_quotas_include_finished_and_return_authenticated_limit_close() {
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0xa1; 32],
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let m2 = broker.read_m1_write_m2(&m1).expect("m2");
        target.read_m2(&m2).expect("split");
        broker.core.set_usage(0, 0, RECORD_LIMIT - 2, 0);
        let target_finished = target.write_finished().expect("target finished");
        broker
            .read_finished(&target_finished)
            .expect("Finished consumes the penultimate receive slot");
        let broker_finished = broker.write_finished().expect("broker finished");
        target
            .read_finished(&broker_finished)
            .expect("target finished verify");
        let open = broker.write_session_open(b"open").expect("open");
        target
            .read_application(&open[0])
            .expect("target reads open");
        let response = target
            .write_application_response(b"opened")
            .expect("response");
        let mut error = broker
            .read_application_response(&response[0])
            .expect_err("next received record reaches cap");
        assert_eq!(error.close_reason(), CloseReason::RecordLimit);
        let close = error
            .take_close_frame()
            .expect("authenticated recordLimit close");
        assert_eq!(
            target
                .read_close(&close)
                .expect("target verifies limit close"),
            (
                CloseReason::RecordLimit,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
        assert!(
            error.take_close_frame().is_none(),
            "close frame is one-shot"
        );

        let (mut target, mut broker) = open_test_sessions();
        broker.core.set_usage(RECORD_LIMIT - 2, 0, 0, 0);
        let mut error = broker
            .write_session_open(b"open")
            .expect_err("outbound record cap");
        let close = error.take_close_frame().expect("outbound cap close");
        assert_eq!(
            target.read_close(&close).expect("peer verifies close").0,
            CloseReason::RecordLimit
        );
        assert_eq!(
            broker
                .write_session_open(b"retry")
                .expect_err("terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );

        let (mut target, mut broker) = open_test_sessions();
        let close_len = limit_close_plaintext(HandoffState::NotHandedOff)
            .expect("limit close")
            .len() as u64;
        broker.core.set_usage(0, BYTE_LIMIT - close_len - 1, 0, 0);
        let mut error = broker
            .write_session_open(b"open")
            .expect_err("outbound byte cap");
        let close = error.take_close_frame().expect("byte cap close");
        assert_eq!(
            target.read_close(&close).expect("peer verifies close").0,
            CloseReason::RecordLimit
        );
    }

    #[test]
    fn decoder_pipeline_empty_first_message_close_any_turn_and_eof_are_strict() {
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0x91; 32],
        )
        .expect("target");
        let mut broker = BrokerSession::new_test(
            session_binding(),
            &pbs,
            SessionLimits::default(),
            &[0xa1; 32],
        )
        .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let mut decoder = OuterFrameDecoder::new();
        assert!(decoder.push(&m1[..1]).expect("prefix").is_empty());
        let frames = decoder.push(&m1[1..]).expect("complete");
        assert_eq!(frames, vec![m1]);
        let m2 = broker
            .read_m1_write_m2(&frames[0])
            .expect("decoder output feeds session directly");
        target.read_m2(&m2).expect("split");
        let finished = target.write_finished().expect("finished");
        broker.read_finished(&finished).expect("finished");
        let finished = broker.write_finished().expect("finished");
        target.read_finished(&finished).expect("finished");
        let empty = broker
            .core
            .write_record(RecordKind::Application, &[])
            .expect("authenticated empty");
        assert_eq!(
            target
                .read_application(&empty)
                .expect_err("empty first app")
                .close_reason(),
            CloseReason::Malformed
        );

        let (mut target, mut broker) = open_test_sessions();
        let open = broker.write_session_open(b"open").expect("open");
        target.read_application(&open[0]).expect("open complete");
        let close = broker
            .write_close(CloseReason::Stale, HandoffState::NotHandedOff)
            .expect("close outside peer app turn");
        let error = target.read_application(&close).expect_err("close event");
        assert_eq!(
            error.peer_close_details(),
            Some((CloseReason::Stale, HandoffState::HandoffPossibleOrConfirmed))
        );

        let (mut target, mut broker) = open_test_sessions();
        let large = vec![7; SessionLimits::default().first_open_bytes];
        let frames = broker
            .write_session_open(&large)
            .expect("fragmented first open");
        assert_eq!(frames.len(), 2);
        assert!(
            target
                .read_application(&frames[0])
                .expect("first fragment")
                .is_none()
        );
        assert_eq!(
            target.eof().expect_err("EOF before END").close_reason(),
            CloseReason::Malformed
        );
        assert_eq!(
            target
                .read_application(&frames[1])
                .expect_err("EOF terminal")
                .close_reason(),
            CloseReason::SequenceViolation
        );
    }

    #[test]
    fn authenticated_session_eof_is_peer_closed_at_idle_and_message_boundaries() {
        let (mut idle_target, mut idle_broker) = open_test_sessions();
        assert_eq!(
            idle_target
                .eof()
                .expect_err("authenticated Target idle EOF is terminal")
                .close_reason(),
            CloseReason::PeerClosed
        );
        assert_eq!(
            idle_broker
                .eof()
                .expect_err("authenticated Broker idle EOF is terminal")
                .close_reason(),
            CloseReason::PeerClosed
        );

        let (mut target, mut broker) = open_test_sessions();
        let open = broker.write_session_open(b"open").expect("open");
        assert_eq!(
            target.read_application(&open[0]).expect("open complete"),
            Some(b"open".to_vec())
        );
        let opened = target
            .write_application_response(b"opened")
            .expect("open response");
        assert_eq!(
            broker
                .read_application_response(&opened[0])
                .expect("response complete"),
            Some(b"opened".to_vec())
        );
        assert_eq!(
            target
                .eof()
                .expect_err("Target boundary EOF is terminal")
                .close_reason(),
            CloseReason::PeerClosed
        );
        assert_eq!(
            broker
                .eof()
                .expect_err("Broker boundary EOF is terminal")
                .close_reason(),
            CloseReason::PeerClosed
        );
    }

    #[test]
    fn authenticated_close_is_terminal_before_eof_for_both_roles_and_directions() {
        for reason in [
            CloseReason::Normal,
            CloseReason::AuthenticationFailed,
            CloseReason::BindingMismatch,
            CloseReason::Stale,
            CloseReason::Timeout,
            CloseReason::Oversize,
            CloseReason::Malformed,
            CloseReason::SequenceViolation,
            CloseReason::RecordLimit,
            CloseReason::PeerClosed,
            CloseReason::BrokerLost,
            CloseReason::EligibilityLost,
            CloseReason::CleanupFailed,
            CloseReason::InternalError,
        ] {
            for handoff in [
                HandoffState::NotHandedOff,
                HandoffState::HandoffPossibleOrConfirmed,
            ] {
                let (mut target, mut broker) = open_test_sessions();
                let close = broker.write_close(reason, handoff).expect("Broker Close");
                assert_eq!(
                    broker
                        .eof()
                        .expect_err("Broker write-close terminal")
                        .close_reason(),
                    CloseReason::SequenceViolation
                );
                assert_eq!(
                    target.read_close(&close).expect("Target reads Close"),
                    (reason, handoff)
                );
                assert_eq!(
                    target
                        .eof()
                        .expect_err("Target read-close terminal")
                        .close_reason(),
                    CloseReason::SequenceViolation
                );

                let (mut target, mut broker) = open_test_sessions();
                let close = target.write_close(reason, handoff).expect("Target Close");
                assert_eq!(
                    target
                        .eof()
                        .expect_err("Target write-close terminal")
                        .close_reason(),
                    CloseReason::SequenceViolation
                );
                assert_eq!(
                    broker.read_close(&close).expect("Broker reads Close"),
                    (reason, handoff)
                );
                assert_eq!(
                    broker
                        .eof()
                        .expect_err("Broker read-close terminal")
                        .close_reason(),
                    CloseReason::SequenceViolation
                );
            }
        }
    }

    #[test]
    fn accepted_vector_documents_use_closed_literal_dispatch() {
        assert_closed_vector_documents();
        assert_closed_positive_vector_documents();
    }

    #[test]
    fn cryptographic_expected_bytes_are_bound_to_case_raw_input() {
        let vectors: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/binding-replay-failures.json"
        )))
        .expect("binding vectors");
        let mut control = vectors["vectors"]
            .as_array()
            .expect("vectors")
            .iter()
            .find(|case| case["id"] == "equal-generation-control")
            .expect("equal-generation-control")
            .clone();
        let expected = control["cryptographic_expected_bytes_hex"]
            .as_str()
            .expect("cryptographic bytes");
        let mut mutated = expected.to_owned();
        let last = mutated.pop().expect("last hex digit");
        mutated.push(if last == '0' { '1' } else { '0' });
        control["cryptographic_expected_bytes_hex"] = Value::String(mutated);
        assert!(
            std::panic::catch_unwind(|| {
                assert_case_envelope(control, "equal-generation mutation");
            })
            .is_err(),
            "mutated cryptographic_expected_bytes_hex was not bound to raw input"
        );
    }

    #[test]
    fn record_limit_close_respects_directional_quotas_and_handoff_state() {
        let close_len = complete_record(
            RecordKind::Close,
            &close_payload(CloseReason::RecordLimit, HandoffState::NotHandedOff)
                .expect("close payload"),
        )
        .expect("close record")
        .encode()
        .expect("close plaintext")
        .len() as u64;

        let (_target, mut broker) = open_test_sessions();
        broker.core.set_usage(RECORD_LIMIT - 1, 0, 0, 0);
        let mut error = broker
            .write_session_open(b"open")
            .expect_err("no nonce remains for a Close");
        assert_eq!(error.close_reason(), CloseReason::RecordLimit);
        assert!(error.take_close_frame().is_none());

        let (mut target_with_slot, mut broker_with_slot) = open_test_sessions();
        broker_with_slot.core.set_usage(RECORD_LIMIT - 2, 0, 0, 0);
        let mut error = broker_with_slot
            .write_session_open(b"open")
            .expect_err("last legal nonce is reserved for Close");
        let close = error.take_close_frame().expect("reserved nonce Close");
        assert_eq!(
            target_with_slot.read_close(&close).expect("verify Close"),
            (CloseReason::RecordLimit, HandoffState::NotHandedOff)
        );

        let (_, mut broker_no_bytes) = open_test_sessions();
        broker_no_bytes
            .core
            .set_usage(0, BYTE_LIMIT - close_len, 0, 0);
        let mut error = broker_no_bytes
            .write_session_open(b"open")
            .expect_err("no plaintext capacity remains for Close");
        assert!(error.take_close_frame().is_none());

        let (mut target, mut broker) = open_test_sessions();
        let open = broker.write_session_open(b"open").expect("open");
        target.read_application(&open[0]).expect("open read");
        let opened = target
            .write_application_response(b"opened")
            .expect("open response");
        broker
            .read_application_response(&opened[0])
            .expect("opened read");
        let request = broker
            .write_application_request(b"mutating request")
            .expect("complete request");
        assert_eq!(
            target.read_application(&request[0]).expect("request read"),
            Some(b"mutating request".to_vec())
        );
        let response = target
            .write_application_response(b"response")
            .expect("response");
        broker.core.set_usage(
            broker.core.sent_records,
            broker.core.sent_plaintext,
            RECORD_LIMIT - 1,
            0,
        );
        let mut error = broker
            .read_application_response(&response[0])
            .expect_err("receive record cap");
        let close = error.take_close_frame().expect("post-handoff Close");
        assert_eq!(
            target
                .read_close(&close)
                .expect("verify post-handoff Close"),
            (
                CloseReason::RecordLimit,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
    }

    #[test]
    fn first_session_open_moves_both_roles_past_transport_handoff() {
        let (mut target, mut broker) = open_test_sessions();
        let open = broker
            .write_session_open(b"open")
            .expect("session.open emitted");
        assert_eq!(
            target
                .read_application(&open[0])
                .expect("session.open reassembled"),
            Some(b"open".to_vec())
        );

        target.core.set_usage(RECORD_LIMIT - 2, 0, 0, 0);
        let mut target_limit = target
            .write_application_response(b"opened")
            .expect_err("Target quota reserves Close after session.open handoff");
        let target_close = target_limit
            .take_close_frame()
            .expect("Target emits authenticated recordLimit Close");
        assert_eq!(
            broker
                .read_close(&target_close)
                .expect("Broker verifies Close"),
            (
                CloseReason::RecordLimit,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );

        let (mut target, mut broker) = open_test_sessions();
        let open = broker
            .write_session_open(b"open")
            .expect("session.open emitted");
        target.read_application(&open[0]).expect("open read");
        let opened = target
            .write_application_response(b"opened")
            .expect("open response");
        broker
            .read_application_response(&opened[0])
            .expect("opened read");
        broker.core.set_usage(RECORD_LIMIT - 2, 0, 0, 0);
        let mut broker_limit = broker
            .write_application_request(b"request")
            .expect_err("Broker quota reserves Close after session.open handoff");
        let broker_close = broker_limit
            .take_close_frame()
            .expect("Broker emits authenticated recordLimit Close");
        assert_eq!(
            target
                .read_close(&broker_close)
                .expect("Target verifies Close"),
            (
                CloseReason::RecordLimit,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
    }

    #[test]
    fn cap_literals_drive_session_and_lease_core_close_reservation() {
        let vectors: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../contracts/v1/vectors/frame-failures.json"
        )))
        .expect("frame vectors");
        let cases = vectors["vectors"].as_array().expect("vectors");
        let nonce = cases
            .iter()
            .find(|case| case["id"] == "nonce-record-limit")
            .expect("nonce vector");
        let accepted_records = nonce["canonical_input"]["accepted_records"]
            .as_u64()
            .expect("accepted records");
        let next_record = nonce["canonical_input"]["next_record"]
            .as_u64()
            .expect("next record");
        assert_eq!(accepted_records.checked_add(1), Some(next_record));

        let (mut target, mut broker) = open_test_sessions();
        broker.core.set_usage(accepted_records - 1, 0, 0, 0);
        let mut session_error = broker
            .write_session_open(b"open")
            .expect_err("SessionCore reserves the final legal nonce for Close");
        let session_close = session_error
            .take_close_frame()
            .expect("SessionCore authenticated Close");
        assert_eq!(
            target
                .read_close(&session_close)
                .expect("Target verifies SessionCore Close"),
            (CloseReason::RecordLimit, HandoffState::NotHandedOff)
        );

        let (mut target_lease, mut broker_lease) = open_test_leases();
        broker_lease.core.set_usage(accepted_records - 1, 0, 0, 0);
        let mut lease_error = broker_lease
            .write_heartbeat_request(1)
            .expect_err("LeaseCore reserves the final legal nonce for Close");
        let lease_close = lease_error
            .take_close_frame()
            .expect("LeaseCore authenticated Close");
        assert_eq!(
            target_lease
                .read_close(&lease_close)
                .expect("Target verifies LeaseCore Close"),
            (CloseReason::RecordLimit, HandoffState::NotHandedOff)
        );

        let plaintext = cases
            .iter()
            .find(|case| case["id"] == "plaintext-byte-limit")
            .expect("plaintext vector");
        let accepted_bytes = plaintext["canonical_input"]["accepted_plaintext_bytes"]
            .as_u64()
            .expect("accepted bytes");
        let next_bytes = plaintext["canonical_input"]["next_plaintext_bytes"]
            .as_u64()
            .expect("next bytes");
        assert_eq!(accepted_bytes.checked_add(next_bytes), Some(BYTE_LIMIT));
        let (_target, mut broker) = open_test_sessions();
        broker.core.set_usage(0, accepted_bytes, 0, 0);
        let mut error = broker
            .write_session_open(b"open")
            .expect_err("literal byte usage leaves no Close capacity");
        assert!(error.take_close_frame().is_none());
    }

    #[test]
    fn multi_frame_application_quota_failure_preserves_close_nonce() {
        let two_frame_open = vec![0x33; SessionLimits::default().first_open_bytes];
        assert_eq!(
            fragment_records(RecordKind::Application, &two_frame_open)
                .expect("open fragments")
                .len(),
            2
        );
        let (mut exact_target, mut exact_broker) = open_test_sessions();
        exact_broker.core.set_usage(RECORD_LIMIT - 4, 0, 0, 0);
        let exact_frames = exact_broker
            .write_session_open(&two_frame_open)
            .expect("two records plus one Close fit below the exact record cap");
        assert!(
            exact_target
                .read_application(&exact_frames[0])
                .expect("first exact-reserve frame")
                .is_none()
        );
        assert_eq!(
            exact_target
                .read_application(&exact_frames[1])
                .expect("second exact-reserve frame"),
            Some(two_frame_open.clone())
        );
        let exact_close = exact_broker
            .write_close(
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed,
            )
            .expect("reserved final legal record carries Close");
        assert_eq!(
            exact_target
                .read_close(&exact_close)
                .expect("exact-reserve Close authenticates"),
            (
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );

        let (mut target, mut broker) = open_test_sessions();
        broker.core.set_usage(RECORD_LIMIT - 3, 0, 0, 0);
        let mut error = broker
            .write_session_open(&two_frame_open)
            .expect_err("whole session.open cannot fit with reserved Close nonce");
        let close = error
            .take_close_frame()
            .expect("authenticated Close uses the unconsumed application nonce");
        assert_eq!(
            target
                .read_close(&close)
                .expect("Target authenticates Close"),
            (CloseReason::RecordLimit, HandoffState::NotHandedOff)
        );

        let (mut target, mut broker) = open_test_sessions();
        let open = broker.write_session_open(b"open").expect("open");
        target.read_application(&open[0]).expect("open read");
        let opened = target
            .write_application_response(b"opened")
            .expect("opened");
        broker
            .read_application_response(&opened[0])
            .expect("opened read");
        let request = vec![0x44; RECORD_DATA_MAX + 1];
        let plaintexts = fragment_records(RecordKind::Application, &request).expect("fragments");
        assert_eq!(plaintexts.len(), 2);
        let message_bytes = plaintexts.iter().map(Vec::len).sum::<usize>() as u64;
        let close_bytes = limit_close_plaintext(HandoffState::HandoffPossibleOrConfirmed)
            .expect("Close plaintext")
            .len() as u64;
        broker
            .core
            .set_usage(0, BYTE_LIMIT - message_bytes - close_bytes, 0, 0);
        let mut error = broker
            .write_application_request(&request)
            .expect_err("whole request cannot fit with reserved Close plaintext");
        let close = error
            .take_close_frame()
            .expect("authenticated Close precedes every request fragment");
        assert_eq!(
            target
                .read_close(&close)
                .expect("Target authenticates Close"),
            (
                CloseReason::RecordLimit,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
    }

    #[test]
    fn explicit_session_close_cannot_downgrade_transport_handoff() {
        let (mut pre_target, mut pre_broker) = open_test_sessions();
        let pre_close = pre_broker
            .write_close(CloseReason::Normal, HandoffState::NotHandedOff)
            .expect("pre-handoff Close");
        assert_eq!(
            pre_target.read_close(&pre_close).expect("pre-handoff read"),
            (CloseReason::Normal, HandoffState::NotHandedOff)
        );

        let (mut conservative_target, mut conservative_broker) = open_test_sessions();
        let conservative_close = conservative_broker
            .write_close(
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed,
            )
            .expect("caller may conservatively raise handoff");
        assert_eq!(
            conservative_target
                .read_close(&conservative_close)
                .expect("conservative read"),
            (
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );

        let (mut target, mut broker) = open_test_sessions();
        let open = broker.write_session_open(b"open").expect("open");
        target.read_application(&open[0]).expect("open read");
        let opened = target
            .write_application_response(b"opened")
            .expect("opened");
        broker
            .read_application_response(&opened[0])
            .expect("opened read");
        let request = broker
            .write_application_request(b"mutating request")
            .expect("request emitted");
        assert_eq!(
            target
                .read_application(&request[0])
                .expect("mutating request reassembled"),
            Some(b"mutating request".to_vec())
        );
        let broker_close = broker
            .write_close(CloseReason::Normal, HandoffState::NotHandedOff)
            .expect("Broker post-handoff Close");
        assert_eq!(
            target
                .read_close(&broker_close)
                .expect("Target verifies Broker Close"),
            (
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
        let (mut target, mut broker) = open_test_sessions();
        let open = broker.write_session_open(b"open").expect("open");
        target.read_application(&open[0]).expect("open read");
        let opened = target
            .write_application_response(b"opened")
            .expect("opened");
        broker
            .read_application_response(&opened[0])
            .expect("opened read");
        let request = broker
            .write_application_request(b"mutating request")
            .expect("request");
        assert_eq!(
            target
                .read_application(&request[0])
                .expect("mutating request reassembled"),
            Some(b"mutating request".to_vec())
        );
        let target_close = target
            .write_close(CloseReason::Normal, HandoffState::NotHandedOff)
            .expect("Target post-handoff Close");
        assert_eq!(
            broker
                .read_close(&target_close)
                .expect("Broker verifies Target Close"),
            (
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
    }

    #[test]
    fn parse_m2_secret_owner_drops_on_early_failure_and_after_success_transfer() {
        let binding = bootstrap_binding();
        let source = ProcessBootstrapSecret::new([0x41; 32]);
        let mut mismatched = binding.expected_m2(&source).expect("M2 payload");
        let last = mismatched.last_mut().expect("artifact digest byte");
        *last ^= 1;
        let before_failure = secret_drop_count();
        let error = match parse_m2(&binding, &mismatched) {
            Ok(_) => panic!("binding mismatch after PBS decode was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.close_reason(), CloseReason::BindingMismatch);
        assert_eq!(secret_drop_count(), before_failure + 1);

        let valid = binding.expected_m2(&source).expect("valid M2 payload");
        let before_success = secret_drop_count();
        let delivered = parse_m2(&binding, &valid).expect("PBS ownership transferred");
        assert_eq!(secret_drop_count(), before_success);
        drop(delivered);
        assert_eq!(secret_drop_count(), before_success + 1);
    }

    #[test]
    fn record_plaintext_copies_zeroize_on_success_fragment_and_error_drop() {
        let before = record_drop_count();
        let encoded = fragment_records(RecordKind::Application, &vec![0xA5; 65_536])
            .expect("fragment records");
        assert!(encoded.len() > 1);
        assert!(
            record_drop_count() >= before + encoded.len() as u64,
            "outbound Record owners must zeroize after encoding"
        );

        let before_decode = record_drop_count();
        let decoded = Record::decode(&encoded[0]).expect("decode first fragment");
        drop(decoded);
        assert_eq!(record_drop_count(), before_decode + 1);

        let before_error = record_drop_count();
        let invalid = Record {
            kind: RecordKind::Application,
            start: false,
            end: true,
            total_len: 0,
            offset: 9,
            data: vec![0x5A; 4],
        };
        let mut reassembler = RecordReassembler::new(1024).expect("reassembler");
        assert!(reassembler.accept(invalid).is_err());
        assert_eq!(record_drop_count(), before_error + 1);

        let before_detached = detached_plaintext_drop_count();
        let oversized_complete = Record {
            kind: RecordKind::Application,
            start: true,
            end: true,
            total_len: 1,
            offset: 0,
            data: vec![0xC3; 4],
        };
        let mut reassembler = RecordReassembler::new(1024).expect("reassembler");
        assert!(reassembler.accept(oversized_complete).is_err());
        assert_eq!(detached_plaintext_drop_count(), before_detached + 1);

        let before_active_error = detached_plaintext_drop_count();
        let mut reassembler = RecordReassembler::new(1024).expect("reassembler");
        reassembler
            .accept(Record {
                kind: RecordKind::Application,
                start: true,
                end: false,
                total_len: 4,
                offset: 0,
                data: vec![0xD4; 2],
            })
            .expect("start fragment");
        assert!(
            reassembler
                .accept(Record {
                    kind: RecordKind::Application,
                    start: false,
                    end: true,
                    total_len: 0,
                    offset: 3,
                    data: vec![0xE5; 2],
                })
                .is_err()
        );
        assert_eq!(
            detached_plaintext_drop_count(),
            before_active_error + 1,
            "active reassembly owner must zeroize after a continuation error"
        );
    }

    #[test]
    fn broker_bootstrap_borrows_the_single_lease_scoped_pbs_owner() {
        let binding = bootstrap_binding();
        let source = ProcessBootstrapSecret::new([0x41; 32]);
        let before_failure = secret_drop_count();
        let broker = BrokerBootstrap::new_test(binding.clone(), &[0x11; 32], &source, &[0x31; 32])
            .expect("Broker bootstrap");
        assert_eq!(secret_drop_count(), before_failure);
        let error = match broker.read_m1_write_m2(&encode_outer(b"not-noise").expect("outer")) {
            Ok(_) => panic!("invalid M1 accepted"),
            Err(error) => error,
        };
        assert_eq!(error.close_reason(), CloseReason::AuthenticationFailed);
        assert_eq!(
            secret_drop_count(),
            before_failure,
            "early failure must not drop a copied Broker PBS owner"
        );

        let mut target = TargetBootstrap::new_test(
            binding.clone(),
            fixed("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13"),
            &[0x21; 32],
        )
        .expect("Target bootstrap");
        let broker = BrokerBootstrap::new_test(binding, &[0x11; 32], &source, &[0x31; 32])
            .expect("Broker bootstrap");
        let m1 = target.write_m1().expect("M1");
        let (_m2, receiver) = broker.read_m1_write_m2(&m1).expect("M2");
        assert_eq!(secret_drop_count(), before_failure);
        drop(receiver);
        assert_eq!(
            secret_drop_count(),
            before_failure,
            "successful bootstrap must leave the lease-scoped PBS owner alive"
        );
        drop(source);
        assert_eq!(secret_drop_count(), before_failure + 1);
    }

    #[test]
    fn generated_bootstrap_material_uses_matching_static_public_key() {
        let keypair = BrokerStaticKeypair::generate().expect("OS CSPRNG keypair");
        let public = keypair.public_key();
        let private = keypair.into_private_key();
        let params: NoiseParams = "Noise_NK_25519_ChaChaPoly_SHA256".parse().expect("params");
        let mut dh = DefaultResolver
            .resolve_dh(&params.dh)
            .expect("X25519 resolver");
        dh.set(private.bytes());
        assert_eq!(public.as_slice(), dh.pubkey());

        let pbs = ProcessBootstrapSecret::generate().expect("OS CSPRNG PBS");
        let mut target =
            TargetBootstrap::new_test(bootstrap_binding(), public, &[0x21; 32]).expect("target");
        let broker =
            BrokerBootstrap::new_test(bootstrap_binding(), private.bytes(), &pbs, &[0x31; 32])
                .expect("broker");
        let m1 = target.write_m1().expect("m1");
        let (m2, broker_ack) = broker.read_m1_write_m2(&m1).expect("m2");
        let (target_ack, delivered) = target.read_m2(&m2, 1, 1).expect("target m2");
        assert_eq!(delivered.bytes(), pbs.bytes());
        let (ack, _) = target_ack.write_ack().expect("ack");
        broker_ack
            .read_ack(&ack)
            .expect("matching public/private handshake");
    }

    #[test]
    fn target_rejects_noninitial_epoch_when_generating_first_bootstrap_ack() {
        let binding = bootstrap_binding();
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let mut target = TargetBootstrap::new_test(
            binding.clone(),
            fixed("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13"),
            &[0x21; 32],
        )
        .expect("Target bootstrap");
        let broker = BrokerBootstrap::new_test(binding, &[0x11; 32], &pbs, &[0x31; 32])
            .expect("Broker bootstrap");
        let m1 = target.write_m1().expect("M1");
        let (m2, _) = broker.read_m1_write_m2(&m1).expect("M2");

        let error = match target.read_m2(&m2, 1, 2) {
            Ok(_) => panic!("Target accepted a noninitial first Listener Epoch"),
            Err(error) => error,
        };
        assert_eq!(error.close_reason(), CloseReason::BindingMismatch);
    }

    #[test]
    fn broker_rejects_authenticated_first_bootstrap_ack_with_noninitial_epoch() {
        let binding = bootstrap_binding();
        let pbs = ProcessBootstrapSecret::new([0x41; 32]);
        let prologue = bootstrap_prologue(&binding).expect("bootstrap prologue");
        let params: NoiseParams = "Noise_NK_25519_ChaChaPoly_SHA256"
            .parse()
            .expect("Noise params");
        let mut target = Builder::new(params)
            .remote_public_key(&fixed::<32>(
                "7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13",
            ))
            .expect("Broker public key")
            .prologue(&prologue)
            .expect("bootstrap prologue")
            .fixed_ephemeral_key_for_testing_only(&[0x21; 32])
            .build_initiator()
            .expect("Target initiator");
        let broker = BrokerBootstrap::new_test(binding.clone(), &[0x11; 32], &pbs, &[0x31; 32])
            .expect("Broker bootstrap");
        let m1 =
            write_handshake(&mut target, &binding.m1_payload().expect("M1 payload")).expect("M1");
        let (m2, broker_ack) = broker.read_m1_write_m2(&m1).expect("M2");
        read_handshake_mut(&mut target, &m2).expect("Target reads M2");
        let handshake_hash: [u8; 32] = target
            .get_handshake_hash()
            .try_into()
            .expect("SHA256 handshake hash");
        let mut ack_payload = Vec::new();
        let mut encoder = MiniEncoder::new(&mut ack_payload);
        encoder
            .map(6)
            .expect("ACK map")
            .u8(0)
            .expect("version key")
            .u8(1)
            .expect("version")
            .u8(1)
            .expect("digest key")
            .bytes(&binding.target_reference_digest)
            .expect("reference digest")
            .u8(2)
            .expect("lease key")
            .bytes(&binding.lease_id)
            .expect("lease")
            .u8(3)
            .expect("generation key")
            .u64(1)
            .expect("generation")
            .u8(4)
            .expect("epoch key")
            .u64(2)
            .expect("noninitial epoch")
            .u8(5)
            .expect("hash key")
            .bytes(&handshake_hash)
            .expect("handshake hash");
        let plaintext = Record {
            kind: RecordKind::Finished,
            start: true,
            end: true,
            total_len: ack_payload.len() as u32,
            offset: 0,
            data: ack_payload,
        }
        .encode()
        .expect("ACK plaintext");
        let mut target_transport = target.into_transport_mode().expect("Target split");
        let mut ciphertext = vec![0; plaintext.len() + 16];
        let ciphertext_len = target_transport
            .write_message(&plaintext, &mut ciphertext)
            .expect("encrypt ACK");
        ciphertext.truncate(ciphertext_len);
        let ack_outer = encode_outer(&ciphertext).expect("ACK frame");

        let error = match broker_ack.read_ack(&ack_outer) {
            Ok(_) => panic!("Broker accepted a noninitial first Listener Epoch"),
            Err(error) => error,
        };
        assert_eq!(error.close_reason(), CloseReason::BindingMismatch);
    }

    #[test]
    fn production_constructors_complete_bootstrap_lease_and_session() {
        let keypair = BrokerStaticKeypair::generate().expect("Broker keypair");
        let public = keypair.public_key();
        let broker_pbs = ProcessBootstrapSecret::generate().expect("PBS");
        let mut target_bootstrap =
            TargetBootstrap::new(bootstrap_binding(), public).expect("Target NK");
        let broker_bootstrap =
            BrokerBootstrap::new(bootstrap_binding(), keypair.into_private_key(), &broker_pbs)
                .expect("Broker NK");
        let m1 = target_bootstrap.write_m1().expect("NK M1");
        let (m2, broker_ack) = broker_bootstrap.read_m1_write_m2(&m1).expect("NK M2");
        let (target_ack, target_pbs) = target_bootstrap.read_m2(&m2, 42, 1).expect("Target split");
        let (ack_frame, mut target_lease) = target_ack.write_ack().expect("encrypted ACK");
        let (ack, mut broker_lease) = broker_ack
            .read_ack(&ack_frame)
            .expect("Broker verifies ACK");
        let heartbeat = broker_lease
            .write_heartbeat_request(1)
            .expect("heartbeat request");
        assert_eq!(
            target_lease
                .read_heartbeat_request(&heartbeat)
                .expect("Target heartbeat"),
            1
        );
        let heartbeat = target_lease
            .write_heartbeat_reply(1)
            .expect("heartbeat reply");
        assert_eq!(
            broker_lease
                .read_heartbeat_reply(&heartbeat)
                .expect("Broker heartbeat"),
            1
        );

        let binding = SessionBinding {
            lease_id: ack.lease_id,
            process_generation: ack.process_generation,
            listener_epoch: ack.listener_epoch,
            nk_handshake_hash: ack.nk_handshake_hash,
        };
        let mut target = TargetSession::new(binding.clone(), &target_pbs).expect("Target NNpsk0");
        let mut broker = BrokerSession::new(binding, &broker_pbs).expect("Broker NNpsk0");
        let m1 = target.write_m1().expect("NN M1");
        let m2 = broker.read_m1_write_m2(&m1).expect("NN M2");
        target.read_m2(&m2).expect("Target NN split");
        let finished = target.write_finished().expect("Target Finished");
        broker
            .read_finished(&finished)
            .expect("Broker verifies Finished");
        let finished = broker.write_finished().expect("Broker Finished");
        target
            .read_finished(&finished)
            .expect("Target verifies Finished");
        let open = broker
            .write_session_open(b"opaque session.open")
            .expect("first opaque");
        assert_eq!(
            target.read_application(&open[0]).expect("Target open"),
            Some(b"opaque session.open".to_vec())
        );
        let response = target
            .write_application_response(b"opaque response")
            .expect("response");
        assert_eq!(
            broker
                .read_application_response(&response[0])
                .expect("Broker response"),
            Some(b"opaque response".to_vec())
        );
    }
}
