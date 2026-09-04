use crate::{
    CloseReason, Closed, ControlFailure, ControlRequest, ControlSuccess, ErrorKind, ErrorStage,
    ExchangeComplete, HandoffState, Platform, PrepareBody, READY_REFERENCE_TTL_MS, ReadyReference,
    ReadyTarget, Request, SessionOpened,
    adapter::{
        AbsoluteDeadline, Cancellation, CleanupReceipt, PlatformFailure, PlatformFailureKind,
        PlatformTargetAdapter, PublicLaunchDescriptor, RawConnector, RawDuplex, TargetSelection,
    },
    raw_transport::{self, BootstrapSuccess, TransportFailure},
};
#[cfg(feature = "internal-diagnostics")]
use crate::{
    INTERNAL_BOOTSTRAP_ACK_BINDING_MISMATCH, INTERNAL_BOOTSTRAP_ADAPTER_REJECTED,
    raw_transport::BootstrapFailureOrigin,
};
use apppilotkit_transport_crypto_core::{
    BootstrapBinding, BrokerLeaseConnection, BrokerSession, BrokerStaticKeypair,
    ProcessBootstrapSecret, SessionBinding,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SAFE: u64 = 9_007_199_254_740_991;
// Prepare has two distinct bounded phases. Android install, Activity start,
// and adb forwarding must not consume the Target's authenticated bootstrap
// budget, which begins only after the adapter has connected the raw stream.
const IOS_PREPARE_LAUNCH_MS: u64 = 10_000;
const ANDROID_PREPARE_LAUNCH_MS: u64 = 20_000;
const PREPARE_BOOTSTRAP_MS: u64 = 10_000;
const CLEANUP_MS: u64 = 2_000;
const SESSION_IDLE_MS: u64 = 30_000;
const LEASE_IDLE_MS: u64 = 120_000;
const LEASE_ABSOLUTE_MS: u64 = 900_000;
const HEARTBEAT_INTERVAL_MS: u64 = 30_000;
const HEARTBEAT_MISSES: u8 = 4;

const fn prepare_launch_budget(platform: Platform) -> u64 {
    match platform {
        Platform::IosSimulator => IOS_PREPARE_LAUNCH_MS,
        Platform::AndroidEmulator => ANDROID_PREPARE_LAUNCH_MS,
    }
}

fn prepare_deadline(
    started: u64,
    caller_deadline: u64,
    platform: Platform,
) -> Result<u64, ControlFailure> {
    let budget = started
        .checked_add(prepare_launch_budget(platform))
        .and_then(|deadline| deadline.checked_add(PREPARE_BOOTSTRAP_MS))
        .ok_or_else(|| fail(ErrorStage::Prepare, CloseReason::InternalError))?;
    Ok(caller_deadline.min(budget))
}

fn launch_deadline(
    started: u64,
    prepare_deadline: u64,
    platform: Platform,
) -> Result<u64, ControlFailure> {
    let launch_limit = started
        .checked_add(prepare_launch_budget(platform))
        .ok_or_else(|| fail(ErrorStage::Prepare, CloseReason::InternalError))?;
    Ok(prepare_deadline.min(launch_limit))
}

fn bootstrap_deadline(connected: u64, prepare_deadline: u64) -> Result<u64, ControlFailure> {
    let bootstrap_limit = connected
        .checked_add(PREPARE_BOOTSTRAP_MS)
        .ok_or_else(|| fail(ErrorStage::Bootstrap, CloseReason::InternalError))?;
    Ok(prepare_deadline.min(bootstrap_limit))
}

const fn commit_rejection_reason(
    completed: u64,
    deadline: u64,
    binding_matches: bool,
) -> Option<CloseReason> {
    if completed >= deadline {
        Some(CloseReason::Timeout)
    } else if !binding_matches {
        Some(CloseReason::BindingMismatch)
    } else {
        None
    }
}

trait Clock: Send + Sync {
    fn now(&self) -> Result<u64, ControlFailure>;
}

struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Result<u64, ControlFailure> {
        now()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PrepareKey {
    platform: Platform,
    device: String,
    app: String,
    digest: [u8; 32],
}
impl PrepareKey {
    fn new(body: &PrepareBody) -> Self {
        Self {
            platform: body.platform,
            device: body.device_selector.clone(),
            app: body.app_id.clone(),
            digest: body.app_artifact_sha256,
        }
    }

    fn selection(&self) -> SelectionKey {
        SelectionKey {
            platform: self.platform,
            device: self.device.clone(),
            app: self.app.clone(),
        }
    }
}

/// Platform ownership is selected before artifact identity is considered.
/// A different artifact for the same selected process conflicts with the
/// existing lease; it must never create a second launch attempt.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SelectionKey {
    platform: Platform,
    device: String,
    app: String,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum LeaseState {
    Preparing,
    Ready,
    Stale,
    Closing,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum RefState {
    Pending,
    Minted,
    Redeeming,
    Consumed,
    Stale,
}
struct Reference {
    state: RefState,
    issued: u64,
    expires: u64,
    session_slot: Option<u64>,
    session_id: Option<String>,
}
struct Session {
    id: Option<String>,
    raw: Arc<dyn RawDuplex>,
    crypto: Option<BrokerSession>,
    reader: Option<raw_transport::RawFrameReader>,
    response: Vec<u8>,
    response_digest: [u8; 32],
    handoff: HandoffState,
    last: u64,
}

/// One lease-wide ownership ledger.  Every operation which can block on raw
/// I/O enters this ledger before releasing `Core`; terminal cleanup first
/// closes the gate and then waits for the ledger to drain.
struct LeaseOperations {
    state: Mutex<OperationState>,
    changed: std::sync::Condvar,
}
struct OperationState {
    accepting: bool,
    active: usize,
    completed: Option<Result<(), CloseReason>>,
    next_id: u64,
    cancellations: HashMap<u64, Cancellation>,
}
struct LeaseOperation {
    operations: Arc<LeaseOperations>,
    id: u64,
}
impl LeaseOperations {
    fn new() -> Self {
        Self {
            state: Mutex::new(OperationState {
                accepting: true,
                active: 0,
                completed: None,
                next_id: 1,
                cancellations: HashMap::new(),
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    fn enter(self: &Arc<Self>, cancellation: Cancellation) -> Result<LeaseOperation, CloseReason> {
        let mut state = self.state.lock().expect("lease operation ledger");
        if !state.accepting {
            return Err(CloseReason::Stale);
        }
        let id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(CloseReason::InternalError)?;
        state.active += 1;
        state.cancellations.insert(id, cancellation);
        Ok(LeaseOperation {
            operations: Arc::clone(self),
            id,
        })
    }

    /// Returns true for the unique terminal owner. Followers must wait for
    /// that owner instead of reporting a premature successful close.
    fn begin_close(&self) -> Option<Vec<Cancellation>> {
        let mut state = self.state.lock().expect("lease operation ledger");
        if !state.accepting {
            return None;
        }
        state.accepting = false;
        Some(state.cancellations.values().cloned().collect())
    }

    fn wait_for_drain_until(&self, deadline: Instant) -> bool {
        let mut state = self.state.lock().expect("lease operation ledger");
        while state.active != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("lease operation wait");
            state = next;
            if result.timed_out() && state.active != 0 {
                return false;
            }
        }
        true
    }

    fn wait_for_drain(&self) {
        let mut state = self.state.lock().expect("lease operation ledger");
        while state.active != 0 {
            state = self.changed.wait(state).expect("lease operation wait");
        }
    }

    fn finish_close(&self, result: Result<(), CloseReason>) {
        let mut state = self.state.lock().expect("lease operation ledger");
        state.completed = Some(result);
        self.changed.notify_all();
    }

    fn wait_for_close_until(&self, deadline: Instant) -> Result<(), CloseReason> {
        let mut state = self.state.lock().expect("lease operation ledger");
        while state.completed.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CloseReason::CleanupFailed);
            }
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("lease close wait");
            state = next;
            if timeout.timed_out() && state.completed.is_none() {
                return Err(CloseReason::CleanupFailed);
            }
        }
        state.completed.expect("terminal result")
    }

    /// Shutdown is the process-lifetime boundary.  Unlike a request-scoped
    /// close, it must not let the process exit while an already-started
    /// terminal reaper still owns the only cleanup receipt.
    fn wait_for_close(&self) -> Result<(), CloseReason> {
        let mut state = self.state.lock().expect("lease operation ledger");
        while state.completed.is_none() {
            state = self.changed.wait(state).expect("lease close wait");
        }
        state.completed.expect("terminal result")
    }
}
impl Drop for LeaseOperation {
    fn drop(&mut self) {
        let mut state = self
            .operations
            .state
            .lock()
            .expect("lease operation ledger");
        state.active = state.active.checked_sub(1).expect("active lease operation");
        state.cancellations.remove(&self.id);
        self.operations.changed.notify_all();
    }
}

struct Lease {
    key: PrepareKey,
    state: LeaseState,
    lease_id: [u8; 16],
    generation: u64,
    epoch: u64,
    nk_hash: [u8; 32],
    pbs: Option<Arc<ProcessBootstrapSecret>>,
    control: Option<Arc<dyn RawDuplex>>,
    lease_crypto: Option<BrokerLeaseConnection>,
    control_reader: Option<raw_transport::RawFrameReader>,
    connector: Option<Arc<dyn RawConnector>>,
    cleanup: Option<Box<dyn CleanupReceipt>>,
    _adapter: Arc<dyn PlatformTargetAdapter>,
    references: HashMap<[u8; 32], Reference>,
    sessions: HashMap<u64, Session>,
    next_session: u64,
    created: u64,
    last_heartbeat_attempt: u64,
    last_heartbeat_success: u64,
    heartbeat_counter: u64,
    heartbeat_misses: u8,
    last_client_activity: u64,
    terminal_cleanup_failed: bool,
    terminal_reason: Option<CloseReason>,
    terminal_handoff: HandoffState,
    handoff: HandoffState,
    operations: Arc<LeaseOperations>,
}
struct Core {
    // Slots never move while an I/O worker holds an index. A terminal reap
    // drops the lease from its slot instead of shifting another lease under
    // that worker.
    leases: Vec<Option<Lease>>,
    failed: HashMap<SelectionKey, ([u8; 32], CloseReason)>,
    stale_tokens: HashMap<[u8; 32], u64>,
    ios: Arc<dyn PlatformTargetAdapter>,
    android: Arc<dyn PlatformTargetAdapter>,
    // Shutdown seals admission before collecting slot indices, so a Prepare
    // cannot publish a new lease after the shutdown sweep has begun.
    shutting_down: bool,
    entropy: File,
}
pub struct SessionBroker {
    inner: Arc<Mutex<Core>>,
    pending_reapers: Arc<AtomicUsize>,
    clock: Arc<dyn Clock>,
}
struct Reservation {
    index: usize,
    key: PrepareKey,
    body: PrepareBody,
    token: [u8; 32],
    digest: [u8; 32],
    lease_id: [u8; 16],
    nonce: [u8; 32],
    private: apppilotkit_transport_crypto_core::BrokerStaticPrivateKey,
    public: [u8; 32],
    pbs: ProcessBootstrapSecret,
    deadline: u64,
    launch_deadline: u64,
    adapter: Arc<dyn PlatformTargetAdapter>,
    launch_cancel: Cancellation,
    _operation: LeaseOperation,
}
struct Prepared {
    success: BootstrapSuccess,
    pbs: ProcessBootstrapSecret,
    operation: LeaseOperation,
}
struct CompletionReservation {
    index: usize,
    key: PrepareKey,
    token: [u8; 32],
    digest: [u8; 32],
    lease_id: [u8; 16],
    deadline: u64,
}

impl SessionBroker {
    pub fn new(
        ios: Arc<dyn PlatformTargetAdapter>,
        android: Arc<dyn PlatformTargetAdapter>,
    ) -> Result<Self, ControlFailure> {
        Self::with_clock(ios, android, Arc::new(SystemClock))
    }

    fn with_clock(
        ios: Arc<dyn PlatformTargetAdapter>,
        android: Arc<dyn PlatformTargetAdapter>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ControlFailure> {
        Ok(Self {
            inner: Arc::new(Mutex::new(Core {
                leases: Vec::new(),
                failed: HashMap::new(),
                stale_tokens: HashMap::new(),
                ios,
                android,
                shutting_down: false,
                entropy: File::open("/dev/urandom")
                    .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?,
            })),
            pending_reapers: Arc::new(AtomicUsize::new(0)),
            clock,
        })
    }
    pub fn handle(&self, request: ControlRequest) -> Result<ControlSuccess, ControlFailure> {
        let current = self.clock.now()?;
        self.prune(current)?;
        if request.deadline_unix_ms() <= current {
            return Err(fail(ErrorStage::Ipc, CloseReason::Timeout));
        }
        match request {
            ControlRequest::Prepare(request) => self.prepare(request),
            ControlRequest::OpenSession(request) => self.open(request),
            ControlRequest::Exchange(request) => self.exchange(request),
            ControlRequest::CloseSession(request) => self.close_session(request),
            ControlRequest::CloseLease(request) => self.close_lease(request),
        }
    }

    /// Explicitly terminates every lease owned by this Broker before its host
    /// process exits.  The admission gate and the slot snapshot share `Core`'s
    /// lock so a concurrent Prepare either appears in this sweep or is
    /// rejected before it can reserve platform resources.
    pub fn shutdown(&self, reason: CloseReason) -> Result<(), ControlFailure> {
        let indices = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            core.shutting_down = true;
            core.leases
                .iter()
                .enumerate()
                .filter_map(|(index, lease)| lease.as_ref().map(|_| index))
                .collect::<Vec<_>>()
        };
        let mut first_failure = None;
        for index in indices {
            let operations = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?
                .leases
                .get(index)
                .and_then(Option::as_ref)
                .map(|lease| Arc::clone(&lease.operations));
            if let Err(error) = self.terminalize_lease(index, reason) {
                if error.close_reason == CloseReason::CleanupFailed
                    && let Some(operations) = operations
                {
                    // `terminalize_lease` starts its existing reaper after a
                    // bounded request-time drain. Keep the process alive at
                    // shutdown until that reaper has consumed the receipt.
                    let _ = operations.wait_for_close();
                }
                // A concurrent terminal owner may already have completed and
                // reaped its slot. Its receipt was still consumed by the
                // shared terminal path, so there is no cleanup left to retry.
                if error.close_reason != CloseReason::Stale {
                    first_failure.get_or_insert(error);
                }
            }
        }
        first_failure.map_or(Ok(()), Err)
    }

    pub fn maintain(&self) -> Result<(), ControlFailure> {
        let current = self.clock.now()?;
        self.prune(current)?;
        let (expired_sessions, expired_leases, broker_losses, due_heartbeats) = {
            let core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let mut sessions = Vec::new();
            let mut leases = Vec::new();
            let mut losses = Vec::new();
            let mut heartbeats = Vec::new();
            for (index, lease) in core.leases.iter().enumerate() {
                let Some(lease) = lease.as_ref() else {
                    continue;
                };
                if lease.state != LeaseState::Ready {
                    continue;
                }
                if let Some(reason) = lease_terminal_reason(
                    current,
                    lease.created,
                    lease.last_client_activity,
                    lease.heartbeat_misses,
                    lease.last_heartbeat_success,
                ) {
                    match reason {
                        CloseReason::BrokerLost => losses.push(index),
                        _ => leases.push(index),
                    }
                    continue;
                }
                sessions.extend(
                    lease
                        .sessions
                        .iter()
                        .filter(|(_, session)| {
                            session.crypto.is_some()
                                && expired_at(current, session.last, SESSION_IDLE_MS)
                        })
                        .map(|(slot, _)| (index, *slot)),
                );
                if expired_at(current, lease.last_heartbeat_attempt, HEARTBEAT_INTERVAL_MS) {
                    heartbeats.push(index);
                }
            }
            (sessions, leases, losses, heartbeats)
        };
        let mut first_failure = None;
        for (index, slot) in expired_sessions {
            if let Err(error) = self.expire_session(index, slot) {
                first_failure.get_or_insert(error);
            }
        }
        for index in expired_leases {
            if let Err(error) = self.terminalize_lease(index, CloseReason::Stale) {
                first_failure.get_or_insert(error);
            }
        }
        for index in broker_losses {
            match self.terminalize_lease(index, CloseReason::BrokerLost) {
                Ok(()) => {
                    first_failure.get_or_insert(fail(ErrorStage::Close, CloseReason::BrokerLost))
                }
                Err(error) => first_failure.get_or_insert(error),
            };
        }
        for index in due_heartbeats {
            if let Err(error) = self.heartbeat(
                index,
                current.saturating_add(HEARTBEAT_INTERVAL_MS).min(MAX_SAFE),
            ) {
                first_failure.get_or_insert(error);
            }
        }
        first_failure.map_or(Ok(()), Err)
    }

    fn prune(&self, current: u64) -> Result<(), ControlFailure> {
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        core.stale_tokens.retain(|_, expires| current < *expires);
        for lease in core.leases.iter_mut().filter_map(Option::as_mut) {
            let live_slots = lease.sessions.keys().copied().collect::<Vec<_>>();
            lease.references.retain(|_, reference| {
                reference.state == RefState::Consumed
                    && reference
                        .session_slot
                        .is_some_and(|slot| live_slots.contains(&slot))
                    || current < reference.expires
            });
        }
        Ok(())
    }
    fn prepare(&self, request: Request<PrepareBody>) -> Result<ControlSuccess, ControlFailure> {
        let started = self.clock.now()?;
        let launch_cancel = Cancellation::new();
        let deadline = prepare_deadline(started, request.deadline_unix_ms, request.body.platform)?;
        let launch_deadline = launch_deadline(started, deadline, request.body.platform)?;
        let reservation = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let key = PrepareKey::new(&request.body);
            let selection = key.selection();
            if core.leases.iter().any(|lease| {
                lease.as_ref().is_some_and(|lease| {
                    lease.key.selection() == selection && lease.key.digest != key.digest
                })
            }) {
                return Err(fail(ErrorStage::Prepare, CloseReason::BindingMismatch));
            }
            if let Some((failed_digest, reason)) = core.failed.get(&selection).copied() {
                return Err(fail(
                    ErrorStage::Cleanup,
                    if failed_digest == key.digest {
                        reason
                    } else {
                        CloseReason::BindingMismatch
                    },
                ));
            }
            if core.shutting_down {
                return Err(fail(ErrorStage::Ipc, CloseReason::BrokerLost));
            }
            if let Some(index) = core.leases.iter().position(|lease| {
                lease
                    .as_ref()
                    .is_some_and(|lease| lease.key == key && lease.state == LeaseState::Ready)
            }) {
                let terminal = core.leases[index].as_ref().and_then(|lease| {
                    lease_terminal_reason(
                        started,
                        lease.created,
                        lease.last_client_activity,
                        lease.heartbeat_misses,
                        lease.last_heartbeat_success,
                    )
                });
                if let Some(reason) = terminal {
                    // Seal under the same decision lock; cleanup runs below
                    // the lock before another same-key prepare can mint.
                    let lease = core.leases[index].as_mut().expect("ready lease");
                    lease.state = LeaseState::Closing;
                    lease.terminal_reason = Some(reason);
                    for reference in lease.references.values_mut() {
                        reference.state = RefState::Stale;
                    }
                    drop(core);
                    self.terminalize_lease(index, reason)?;
                    return Err(fail(ErrorStage::Prepare, reason));
                }
                return mint(&mut core, index, started);
            }
            if core.leases.iter().any(|lease| {
                lease
                    .as_ref()
                    .is_some_and(|lease| lease.key == key && lease.state == LeaseState::Preparing)
            }) {
                return Err(fail(ErrorStage::Prepare, CloseReason::BindingMismatch));
            }
            if let Some(reason) = core.leases.iter().find_map(|lease| {
                lease.as_ref().and_then(|lease| {
                    (lease.key == key && lease.state != LeaseState::Ready)
                        .then_some(lease.terminal_reason.unwrap_or(CloseReason::Stale))
                })
            }) {
                return Err(fail(ErrorStage::Prepare, reason));
            }
            let token = random::<32>(&mut core.entropy)?;
            let lease_id = random::<16>(&mut core.entropy)?;
            let nonce = random::<32>(&mut core.entropy)?;
            let digest =
                Sha256::digest(ReadyReference::from_token(token).to_string().as_bytes()).into();
            let keypair = BrokerStaticKeypair::generate()
                .map_err(|e| fail(ErrorStage::Bootstrap, e.close_reason()))?;
            let public = keypair.public_key();
            let pbs = ProcessBootstrapSecret::generate()
                .map_err(|e| fail(ErrorStage::Bootstrap, e.close_reason()))?;
            let adapter = match request.body.platform {
                Platform::IosSimulator => Arc::clone(&core.ios),
                Platform::AndroidEmulator => Arc::clone(&core.android),
            };
            let mut references = HashMap::new();
            references.insert(
                token,
                Reference {
                    state: RefState::Pending,
                    issued: 0,
                    expires: deadline,
                    session_slot: None,
                    session_id: None,
                },
            );
            core.leases.push(Some(Lease {
                key: key.clone(),
                state: LeaseState::Preparing,
                lease_id,
                generation: 0,
                epoch: 0,
                nk_hash: [0; 32],
                pbs: None,
                control: None,
                lease_crypto: None,
                control_reader: None,
                connector: None,
                cleanup: None,
                _adapter: Arc::clone(&adapter),
                references,
                sessions: HashMap::new(),
                next_session: 1,
                created: started,
                last_heartbeat_attempt: started,
                last_heartbeat_success: started,
                heartbeat_counter: 1,
                heartbeat_misses: 0,
                last_client_activity: started,
                terminal_cleanup_failed: false,
                terminal_reason: None,
                terminal_handoff: HandoffState::NotHandedOff,
                handoff: HandoffState::NotHandedOff,
                operations: Arc::new(LeaseOperations::new()),
            }));
            let operation = core
                .leases
                .last()
                .expect("inserted lease")
                .as_ref()
                .expect("inserted lease")
                .operations
                .enter(launch_cancel.clone())
                .map_err(|reason| fail(ErrorStage::Prepare, reason))?;
            Reservation {
                index: core.leases.len() - 1,
                key,
                body: request.body,
                token,
                digest,
                lease_id,
                nonce,
                private: keypair.into_private_key(),
                public,
                pbs,
                deadline,
                launch_deadline,
                adapter,
                launch_cancel,
                _operation: operation,
            }
        };
        let completion = CompletionReservation {
            index: reservation.index,
            key: reservation.key.clone(),
            token: reservation.token,
            digest: reservation.digest,
            lease_id: reservation.lease_id,
            deadline: reservation.deadline,
        };
        let absolute = abs(reservation.launch_deadline)?;
        let selection = TargetSelection::new(
            reservation.body.platform,
            reservation.body.device_selector.clone(),
            reservation.body.app_id.clone(),
            reservation.body.app_artifact.clone(),
            reservation.body.app_artifact_sha256,
        )
        .map_err(|_| fail(ErrorStage::Prepare, CloseReason::BindingMismatch))?;
        let pending = reservation.adapter.begin_launch(selection, absolute);
        let descriptor = match raw_transport::encode_launch_descriptor(
            reservation.body.platform,
            pending.endpoint(),
            reservation.lease_id,
            reservation.nonce,
            reservation.body.app_artifact_sha256,
            reservation.public,
            deadline,
            reservation.digest,
        ) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                let abort = pending.abort(Cancellation::new(), cleanup_absolute_deadline()?);
                return self.reject(
                    completion,
                    if abort.is_err() {
                        CloseReason::CleanupFailed
                    } else {
                        error.close_reason
                    },
                );
            }
        };
        let descriptor = match PublicLaunchDescriptor::from_d2_canonical_bytes(descriptor) {
            Ok(descriptor) => descriptor,
            Err(_) => {
                let abort = pending.abort(Cancellation::new(), cleanup_absolute_deadline()?);
                return self.reject(
                    completion,
                    if abort.is_err() {
                        CloseReason::CleanupFailed
                    } else {
                        CloseReason::InternalError
                    },
                );
            }
        };
        let binding = BootstrapBinding {
            target_reference_digest: reservation.digest,
            lease_id: reservation.lease_id,
            target_nonce: reservation.nonce,
            app_artifact_digest: reservation.body.app_artifact_sha256,
            expiry_ms: deadline,
        };
        let worker_cancel = reservation.launch_cancel.clone();
        let reapers = Arc::clone(&self.pending_reapers);
        let reaper_core = Arc::clone(&self.inner);
        let reaper_key = completion.key.clone();
        let reaper_index = completion.index;
        let (producer, operation) =
            crate::owned_op::OwnedOp::new(move |result: Result<Prepared, TransportFailure>| {
                let cleanup_failed = match result {
                    Ok(prepared) => {
                        prepared.success.bootstrap.cancel();
                        let BootstrapSuccess { cleanup, .. } = prepared.success;
                        !cleanup_absolute_deadline().is_ok_and(|deadline| {
                            cleanup.cleanup(Cancellation::new(), deadline).is_ok()
                        })
                    }
                    Err(error) => error.cleanup_failed,
                };
                if let Ok(mut core) = reaper_core.lock() {
                    if let Some(slot) = core.leases.get_mut(reaper_index) {
                        if slot
                            .as_ref()
                            .is_some_and(|lease| lease.state == LeaseState::Stale)
                        {
                            *slot = None;
                        } else if let Some(lease) = slot.as_mut() {
                            for reference in lease.references.values_mut() {
                                reference.state = RefState::Stale;
                            }
                        }
                    }
                    if cleanup_failed {
                        core.failed.insert(
                            reaper_key.selection(),
                            (reaper_key.digest, CloseReason::CleanupFailed),
                        );
                    }
                }
                reapers.fetch_sub(1, Ordering::AcqRel);
                !cleanup_failed
            });
        let launch_deadline = abs(reservation.launch_deadline)?;
        let bootstrap_limit = reservation.deadline;
        let worker_clock = Arc::clone(&self.clock);
        let worker = ReservationWorker {
            private: reservation.private,
            pbs: reservation.pbs,
            binding,
            operation: reservation._operation,
        };
        std::thread::spawn(move || {
            let result = pending
                .launch(descriptor, worker_cancel, launch_deadline)
                .map_err(platform_launch_failure)
                .and_then(|launched| {
                    let (raw, connector, cleanup) = launched.into_parts();
                    let bootstrap_deadline = worker_clock
                        .now()
                        .and_then(|connected| bootstrap_deadline(connected, bootstrap_limit))
                        .and_then(abs)
                        .map_err(|failure| TransportFailure {
                            close_reason: failure.close_reason,
                            handoff: HandoffState::NotHandedOff,
                            cleanup_failed: false,
                            #[cfg(feature = "internal-diagnostics")]
                            bootstrap_origin: None,
                        })?;
                    raw_transport::bootstrap(
                        raw,
                        connector,
                        cleanup,
                        worker.private,
                        &worker.pbs,
                        worker.binding,
                        bootstrap_deadline,
                    )
                    .map(|success| Prepared {
                        success,
                        pbs: worker.pbs,
                        operation: worker.operation,
                    })
                });
            let _ = producer.publish(result);
        });
        self.pending_reapers.fetch_add(1, Ordering::AcqRel);
        let result = operation
            .wait_until(Instant::now() + Duration::from_millis(deadline.saturating_sub(started)));
        if result.is_some() {
            self.pending_reapers.fetch_sub(1, Ordering::AcqRel);
        }
        match result {
            Some(Ok(prepared)) => self.commit(completion, prepared),
            Some(Err(error)) => self.reject_transport(completion, error),
            None => {
                reservation.launch_cancel.cancel();
                self.reject(completion, CloseReason::Timeout)
            }
        }
    }
    fn commit(
        &self,
        reservation: CompletionReservation,
        prepared: Prepared,
    ) -> Result<ControlSuccess, ControlFailure> {
        let completed = self.clock.now()?;
        let binding_matches = prepared.success.ack.lease_id == reservation.lease_id
            && prepared.success.ack.target_reference_digest == reservation.digest
            && prepared.success.ack.listener_epoch == 1;
        if let Some(reason) =
            commit_rejection_reason(completed, reservation.deadline, binding_matches)
        {
            #[cfg(feature = "internal-diagnostics")]
            if let Some(origin) = commit_rejection_origin(reason) {
                return self
                    .discard_prepared(reservation, prepared, reason)
                    .map_err(|failure| mark_bootstrap_origin(failure, origin));
            }
            return self.discard_prepared(reservation, prepared, reason);
        }
        let expires = completed
            .checked_add(READY_REFERENCE_TTL_MS)
            .filter(|v| *v <= MAX_SAFE)
            .ok_or_else(|| fail(ErrorStage::Prepare, CloseReason::InternalError))?;
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        let Some(lease) = core
            .leases
            .get_mut(reservation.index)
            .and_then(Option::as_mut)
            .filter(|l| l.key == reservation.key && l.state == LeaseState::Preparing)
        else {
            drop(core);
            return self.discard_prepared(reservation, prepared, CloseReason::Stale);
        };
        let Some(reference) = lease
            .references
            .get_mut(&reservation.token)
            .filter(|r| r.state == RefState::Pending)
        else {
            drop(core);
            return self.discard_prepared(reservation, prepared, CloseReason::Stale);
        };
        let Prepared {
            success,
            pbs,
            operation: _operation,
        } = prepared;
        reference.state = RefState::Minted;
        reference.issued = completed;
        reference.expires = expires;
        lease.generation = success.ack.process_generation;
        lease.epoch = success.ack.listener_epoch;
        lease.nk_hash = success.ack.nk_handshake_hash;
        lease.pbs = Some(Arc::new(pbs));
        lease.control = Some(success.bootstrap);
        lease.lease_crypto = Some(success.lease);
        lease.control_reader = Some(success.reader);
        lease.connector = Some(success.connector);
        lease.cleanup = Some(success.cleanup);
        lease.state = LeaseState::Ready;
        lease.created = completed;
        lease.last_heartbeat_attempt = completed;
        lease.last_heartbeat_success = completed;
        Ok(ControlSuccess::TargetReady(ReadyTarget {
            target_token: reservation.token,
            process_generation: lease.generation,
            listener_epoch: lease.epoch,
            issued_at_unix_ms: completed,
            expires_at_unix_ms: expires,
        }))
    }

    /// A late bootstrap result has transferred launch ownership to the Broker,
    /// even if the lease was sealed before it reached commit. Consume that
    /// ownership before observing the stale terminal state.
    fn discard_prepared(
        &self,
        reservation: CompletionReservation,
        prepared: Prepared,
        reason: CloseReason,
    ) -> Result<ControlSuccess, ControlFailure> {
        let Prepared {
            success, operation, ..
        } = prepared;
        success.bootstrap.cancel();
        let BootstrapSuccess { cleanup, .. } = success;
        let cleanup_failed = cleanup
            .cleanup(Cancellation::new(), cleanup_absolute_deadline()?)
            .is_err();
        if cleanup_failed
            && let Ok(mut core) = self.inner.lock()
            && let Some(lease) = core
                .leases
                .get_mut(reservation.index)
                .and_then(Option::as_mut)
        {
            lease.terminal_cleanup_failed = true;
        }
        let result = self.reject_with_cleanup(reservation, reason, cleanup_failed);
        // Publish a cleanup tombstone before releasing the lease-wide guard:
        // terminal reaping observes that sticky result rather than success.
        drop(operation);
        result
    }
    fn reject(
        &self,
        reservation: CompletionReservation,
        reason: CloseReason,
    ) -> Result<ControlSuccess, ControlFailure> {
        self.reject_with_cleanup(reservation, reason, reason == CloseReason::CleanupFailed)
    }

    fn reject_with_cleanup(
        &self,
        reservation: CompletionReservation,
        reason: CloseReason,
        cleanup_failed: bool,
    ) -> Result<ControlSuccess, ControlFailure> {
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        if let Some(lease) = core
            .leases
            .get_mut(reservation.index)
            .and_then(Option::as_mut)
        {
            // A concurrent terminal owner has already sealed this lease and
            // will consume its ledger after the late bootstrap guard drops.
            // Do not erase `Closing`, or that owner could no longer complete.
            if lease.state != LeaseState::Closing {
                lease.state = LeaseState::Stale;
            }
            lease.terminal_reason.get_or_insert(reason);
            lease.references.remove(&reservation.token);
        }
        if cleanup_failed {
            core.failed.insert(
                reservation.key.selection(),
                (reservation.key.digest, CloseReason::CleanupFailed),
            );
        }
        Err(fail(
            if reason == CloseReason::CleanupFailed {
                ErrorStage::Cleanup
            } else {
                ErrorStage::Bootstrap
            },
            reason,
        ))
    }

    fn reject_transport(
        &self,
        reservation: CompletionReservation,
        error: TransportFailure,
    ) -> Result<ControlSuccess, ControlFailure> {
        match self.reject_with_cleanup(reservation, error.close_reason, error.cleanup_failed) {
            Err(mut failure) => {
                failure.handoff = error.handoff;
                #[cfg(feature = "internal-diagnostics")]
                if let Some(origin) = error.bootstrap_origin {
                    failure = mark_bootstrap_origin(failure, origin);
                }
                Err(failure)
            }
            Ok(_) => unreachable!("bootstrap rejection cannot succeed"),
        }
    }

    fn open(
        &self,
        request: Request<crate::OpenSessionBody>,
    ) -> Result<ControlSuccess, ControlFailure> {
        let current = self.clock.now()?;
        if let Some(id) = request.body.session_id.as_deref() {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease_index = core
                .leases
                .iter()
                .position(|lease| {
                    lease.as_ref().is_some_and(|lease| {
                        lease.references.contains_key(&request.body.target_token)
                    })
                })
                .ok_or_else(|| {
                    token_failure(&core, ErrorStage::SessionOpen, &request.body.target_token)
                })?;
            if let Some(reason) = core.leases[lease_index].as_ref().and_then(|lease| {
                lease_terminal_reason(
                    current,
                    lease.created,
                    lease.last_client_activity,
                    lease.heartbeat_misses,
                    lease.last_heartbeat_success,
                )
            }) {
                drop(core);
                let _ = self.terminalize_lease(lease_index, reason);
                return Err(fail(ErrorStage::SessionOpen, reason));
            }
            let lease = core.leases[lease_index].as_mut().expect("indexed lease");
            if lease.state != LeaseState::Ready {
                return Err(fail(ErrorStage::SessionOpen, CloseReason::Stale));
            }
            let slot = matched_session_slot(
                lease,
                &request.body.target_token,
                id,
                ErrorStage::SessionOpen,
                false,
            )?;
            let (response, response_sha256, process_generation, listener_epoch, handoff) = {
                let session = lease
                    .sessions
                    .get(&slot)
                    .filter(|s| s.id.as_deref() == Some(id) && s.crypto.is_some())
                    .ok_or_else(|| fail(ErrorStage::SessionOpen, CloseReason::Stale))?;
                (
                    session.response.clone(),
                    session.response_digest,
                    lease.generation,
                    lease.epoch,
                    session.handoff,
                )
            };
            lease.last_client_activity = self.clock.now()?;
            return Ok(ControlSuccess::SessionOpened(SessionOpened {
                target_token: request.body.target_token,
                response,
                response_sha256,
                process_generation,
                listener_epoch,
                handoff,
            }));
        }
        let payload = request
            .body
            .session_open_request
            .as_deref()
            .ok_or_else(|| fail(ErrorStage::SessionOpen, CloseReason::Malformed))?;
        if request.body.session_open_request_sha256 != Some(Sha256::digest(payload).into()) {
            return Err(fail(ErrorStage::SessionOpen, CloseReason::BindingMismatch));
        }
        let current = self.clock.now()?;
        let (
            lease_index,
            slot,
            connector,
            pbs,
            binding,
            cancellation,
            effective_deadline,
            _operation,
        ) = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease_index = core
                .leases
                .iter()
                .position(|lease| {
                    lease.as_ref().is_some_and(|lease| {
                        lease.references.contains_key(&request.body.target_token)
                    })
                })
                .ok_or_else(|| {
                    token_failure(&core, ErrorStage::SessionOpen, &request.body.target_token)
                })?;
            if let Some(reason) = core.leases[lease_index].as_ref().and_then(|lease| {
                lease_terminal_reason(
                    current,
                    lease.created,
                    lease.last_client_activity,
                    lease.heartbeat_misses,
                    lease.last_heartbeat_success,
                )
            }) {
                drop(core);
                let _ = self.terminalize_lease(lease_index, reason);
                return Err(fail(ErrorStage::SessionOpen, reason));
            }
            let lease = core.leases[lease_index].as_mut().expect("indexed lease");
            if lease.state != LeaseState::Ready {
                return Err(fail(ErrorStage::SessionOpen, CloseReason::Stale));
            }
            let reference = lease
                .references
                .get_mut(&request.body.target_token)
                .filter(|r| r.state == RefState::Minted && current < r.expires)
                .ok_or_else(|| fail(ErrorStage::SessionOpen, CloseReason::Stale))?;
            let cancellation = Cancellation::new();
            let operation = lease
                .operations
                .enter(cancellation.clone())
                .map_err(|reason| fail(ErrorStage::SessionOpen, reason))?;
            reference.state = RefState::Redeeming;
            let slot = lease.next_session;
            lease.next_session = slot
                .checked_add(1)
                .ok_or_else(|| fail(ErrorStage::SessionOpen, CloseReason::InternalError))?;
            reference.session_slot = Some(slot);
            let binding = SessionBinding {
                lease_id: lease.lease_id,
                process_generation: lease.generation,
                listener_epoch: lease.epoch,
                nk_handshake_hash: lease.nk_hash,
            };
            (
                lease_index,
                slot,
                lease
                    .connector
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| fail(ErrorStage::SessionHandshake, CloseReason::Stale))?,
                lease
                    .pbs
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| fail(ErrorStage::SessionHandshake, CloseReason::Stale))?,
                binding,
                cancellation,
                abs(request.deadline_unix_ms.min(
                    lease
                        .created
                        .saturating_add(LEASE_ABSOLUTE_MS)
                        .min(MAX_SAFE),
                ))?,
                operation,
            )
        };
        let opened = raw_transport::open_session(
            connector.as_ref(),
            cancellation,
            binding.clone(),
            pbs.as_ref(),
            payload,
            effective_deadline,
        );
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        let completed = self.clock.now()?;
        if let Some(reason) = core.leases[lease_index].as_ref().and_then(|lease| {
            lease_terminal_reason(
                completed,
                lease.created,
                lease.last_client_activity,
                lease.heartbeat_misses,
                lease.last_heartbeat_success,
            )
        }) {
            if let Ok(opened) = &opened {
                opened.raw.cancel();
            }
            drop(core);
            drop(_operation);
            let _ = self.terminalize_lease(lease_index, reason);
            return Err(fail(ErrorStage::SessionOpen, reason));
        }
        let lease = core.leases[lease_index]
            .as_mut()
            .ok_or_else(|| fail(ErrorStage::SessionOpen, CloseReason::Stale))?;
        match opened {
            Ok(opened) => {
                if lease.state != LeaseState::Ready
                    || lease.lease_id != binding.lease_id
                    || lease.generation != binding.process_generation
                    || lease.epoch != binding.listener_epoch
                    || lease.nk_hash != binding.nk_handshake_hash
                {
                    opened.raw.cancel();
                    if let Some(reference) = lease.references.get_mut(&request.body.target_token) {
                        reference.state = RefState::Stale;
                    }
                    return Err(fail(ErrorStage::SessionOpen, CloseReason::Stale));
                }
                let reference = lease
                    .references
                    .get_mut(&request.body.target_token)
                    .filter(|r| r.state == RefState::Redeeming && r.session_slot == Some(slot))
                    .ok_or_else(|| fail(ErrorStage::SessionOpen, CloseReason::Stale))?;
                reference.state = RefState::Consumed;
                let digest = Sha256::digest(&opened.response).into();
                lease.sessions.insert(
                    slot,
                    Session {
                        id: None,
                        raw: opened.raw,
                        crypto: Some(opened.session),
                        reader: Some(opened.reader),
                        response: opened.response.clone(),
                        response_digest: digest,
                        handoff: opened.handoff,
                        last: self.clock.now()?,
                    },
                );
                lease.last_client_activity = self.clock.now()?;
                Ok(ControlSuccess::SessionOpened(SessionOpened {
                    target_token: request.body.target_token,
                    response: opened.response,
                    response_sha256: digest,
                    process_generation: lease.generation,
                    listener_epoch: lease.epoch,
                    handoff: opened.handoff,
                }))
            }
            Err(error) => {
                if let Some(r) = lease.references.get_mut(&request.body.target_token) {
                    r.state = RefState::Stale;
                }
                Err(transport_fail(ErrorStage::SessionOpen, error))
            }
        }
    }

    fn exchange(
        &self,
        request: Request<crate::ExchangeBody>,
    ) -> Result<ControlSuccess, ControlFailure> {
        let request_digest: [u8; 32] = Sha256::digest(&request.body.message).into();
        if request.body.message_sha256 != request_digest {
            return Err(fail(ErrorStage::Exchange, CloseReason::BindingMismatch));
        }
        let current = self.clock.now()?;
        let (lease_index, slot, raw, mut reader, mut crypto, effective_deadline, _operation) = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease_index = core
                .leases
                .iter()
                .position(|lease| {
                    lease.as_ref().is_some_and(|lease| {
                        lease.references.contains_key(&request.body.target_token)
                    })
                })
                .ok_or_else(|| {
                    token_failure(&core, ErrorStage::Exchange, &request.body.target_token)
                })?;
            if let Some(reason) = core.leases[lease_index].as_ref().and_then(|lease| {
                lease_terminal_reason(
                    current,
                    lease.created,
                    lease.last_client_activity,
                    lease.heartbeat_misses,
                    lease.last_heartbeat_success,
                )
            }) {
                drop(core);
                let _ = self.terminalize_lease(lease_index, reason);
                return Err(fail(ErrorStage::Exchange, reason));
            }
            let lease = core.leases[lease_index].as_mut().expect("indexed lease");
            if lease.state != LeaseState::Ready
                || lease.generation != request.body.process_generation
                || lease.epoch != request.body.listener_epoch
            {
                return Err(fail(ErrorStage::Exchange, CloseReason::Stale));
            }
            let slot = matched_session_slot(
                lease,
                &request.body.target_token,
                &request.body.session_id,
                ErrorStage::Exchange,
                true,
            )?;
            let operation = lease
                .operations
                .enter(Cancellation::new())
                .map_err(|reason| fail(ErrorStage::Exchange, reason))?;
            if lease
                .sessions
                .get(&slot)
                .is_some_and(|session| session.id.is_none())
            {
                lease
                    .references
                    .get_mut(&request.body.target_token)
                    .expect("matched reference owns session")
                    .session_id = Some(request.body.session_id.clone());
            }
            let session = lease
                .sessions
                .get_mut(&slot)
                .expect("matched session remains locked");
            if session.id.is_none() {
                session.id = Some(request.body.session_id.clone());
            }
            (
                lease_index,
                slot,
                Arc::clone(&session.raw),
                session
                    .reader
                    .take()
                    .ok_or_else(|| fail(ErrorStage::Exchange, CloseReason::Stale))?,
                session
                    .crypto
                    .take()
                    .ok_or_else(|| fail(ErrorStage::Exchange, CloseReason::Stale))?,
                abs(request.deadline_unix_ms.min(
                    lease
                        .created
                        .saturating_add(LEASE_ABSOLUTE_MS)
                        .min(MAX_SAFE),
                ))?,
                operation,
            )
        };
        let result = raw_transport::exchange(
            raw.as_ref(),
            &mut reader,
            &mut crypto,
            &request.body.message,
            effective_deadline,
        );
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        let lease = core.leases[lease_index]
            .as_mut()
            .ok_or_else(|| fail(ErrorStage::Exchange, CloseReason::Stale))?;
        let completed = self.clock.now()?;
        if let Some(reason) = lease_terminal_reason(
            completed,
            lease.created,
            lease.last_client_activity,
            lease.heartbeat_misses,
            lease.last_heartbeat_success,
        ) {
            raw.cancel();
            drop(core);
            drop(_operation);
            let _ = self.terminalize_lease(lease_index, reason);
            return Err(fail(ErrorStage::Exchange, reason));
        }
        if lease.state != LeaseState::Ready
            || lease.generation != request.body.process_generation
            || lease.epoch != request.body.listener_epoch
        {
            raw.cancel();
            return Err(fail(ErrorStage::Exchange, CloseReason::Stale));
        }
        let session = lease
            .sessions
            .get_mut(&slot)
            .ok_or_else(|| fail(ErrorStage::Exchange, CloseReason::Stale))?;
        match result {
            Ok(message) => {
                session.crypto = Some(crypto);
                session.reader = Some(reader);
                session.handoff = HandoffState::HandoffPossibleOrConfirmed;
                lease.handoff = HandoffState::HandoffPossibleOrConfirmed;
                session.last = self.clock.now()?;
                lease.last_client_activity = session.last;
                Ok(ControlSuccess::ExchangeComplete(ExchangeComplete {
                    target_token: request.body.target_token,
                    session_id: request.body.session_id,
                    process_generation: lease.generation,
                    listener_epoch: lease.epoch,
                    message_sha256: Sha256::digest(&message).into(),
                    message,
                    handoff: session.handoff,
                }))
            }
            Err(error) => {
                raw.cancel();
                drop(crypto);
                drop(reader);
                let reference = lease
                    .references
                    .get_mut(&request.body.target_token)
                    .expect("failed session belongs to consumed reference");
                reference.session_id = Some(request.body.session_id.clone());
                reference.expires = session_tombstone_expires(self.clock.now()?)?;
                lease.sessions.remove(&slot);
                Err(transport_fail(ErrorStage::Exchange, error))
            }
        }
    }

    fn close_session(
        &self,
        request: Request<crate::CloseSessionBody>,
    ) -> Result<ControlSuccess, ControlFailure> {
        let current = self.clock.now()?;
        let (lease_index, raw, mut crypto, handoff, effective_deadline, operation) = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease_index = core
                .leases
                .iter()
                .position(|lease| {
                    lease.as_ref().is_some_and(|lease| {
                        lease.references.contains_key(&request.body.target_token)
                    })
                })
                .ok_or_else(|| {
                    token_failure(&core, ErrorStage::Close, &request.body.target_token)
                })?;
            if let Some(reason) = core.leases[lease_index].as_ref().and_then(|lease| {
                lease_terminal_reason(
                    current,
                    lease.created,
                    lease.last_client_activity,
                    lease.heartbeat_misses,
                    lease.last_heartbeat_success,
                )
            }) {
                drop(core);
                let _ = self.terminalize_lease(lease_index, reason);
                return Err(fail(ErrorStage::Close, reason));
            }
            let lease = core.leases[lease_index].as_mut().expect("indexed lease");
            if lease.state != LeaseState::Ready {
                return Err(fail(ErrorStage::Close, CloseReason::Stale));
            }
            if lease.generation != request.body.process_generation
                || lease.epoch != request.body.listener_epoch
            {
                return Err(fail(ErrorStage::Close, CloseReason::Stale));
            }
            let slot = matched_session_slot(
                lease,
                &request.body.target_token,
                &request.body.session_id,
                ErrorStage::Close,
                true,
            )?;
            let operation = lease
                .operations
                .enter(Cancellation::new())
                .map_err(|reason| fail(ErrorStage::Close, reason))?;
            if lease
                .sessions
                .get(&slot)
                .is_some_and(|session| session.id.is_none())
            {
                lease
                    .references
                    .get_mut(&request.body.target_token)
                    .expect("matched reference owns session")
                    .session_id = Some(request.body.session_id.clone());
            }
            let session = lease
                .sessions
                .get_mut(&slot)
                .expect("matched session remains locked");
            if session.id.is_none() {
                session.id = Some(request.body.session_id.clone());
            }
            (
                lease_index,
                Arc::clone(&session.raw),
                session
                    .crypto
                    .take()
                    .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?,
                session.handoff,
                abs(request.deadline_unix_ms.min(
                    lease
                        .created
                        .saturating_add(LEASE_ABSOLUTE_MS)
                        .min(MAX_SAFE),
                ))?,
                operation,
            )
        };
        let result = raw_transport::close_session(
            raw.as_ref(),
            &mut crypto,
            request.body.reason,
            handoff,
            effective_deadline,
        );
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        let completed = self.clock.now()?;
        if let Some(reason) = core.leases[lease_index].as_ref().and_then(|lease| {
            lease_terminal_reason(
                completed,
                lease.created,
                lease.last_client_activity,
                lease.heartbeat_misses,
                lease.last_heartbeat_success,
            )
        }) {
            raw.cancel();
            drop(crypto);
            drop(core);
            drop(operation);
            let _ = self.terminalize_lease(lease_index, reason);
            return Err(fail(ErrorStage::Close, reason));
        }
        let lease = core.leases[lease_index]
            .as_mut()
            .filter(|lease| {
                lease.state == LeaseState::Ready
                    && lease.generation == request.body.process_generation
                    && lease.epoch == request.body.listener_epoch
            })
            .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
        let slot = lease
            .references
            .get(&request.body.target_token)
            .and_then(|reference| reference.session_slot)
            .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
        let reference = lease
            .references
            .get_mut(&request.body.target_token)
            .expect("closed session belongs to consumed reference");
        reference.session_id = Some(request.body.session_id.clone());
        reference.expires = session_tombstone_expires(self.clock.now()?)?;
        lease.sessions.remove(&slot);
        match result {
            Ok(()) => Ok(ControlSuccess::Closed(Closed {
                target_token: request.body.target_token,
                session_id: Some(request.body.session_id),
                handoff,
            })),
            Err(error) => Err(transport_fail(ErrorStage::Close, error)),
        }
    }

    fn close_lease(
        &self,
        request: Request<crate::CloseLeaseBody>,
    ) -> Result<ControlSuccess, ControlFailure> {
        let (index, handoff) = {
            let core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let index = core
                .leases
                .iter()
                .position(|l| {
                    l.as_ref().is_some_and(|lease| {
                        lease.references.contains_key(&request.body.target_token)
                    })
                })
                .ok_or_else(|| {
                    token_failure(&core, ErrorStage::Close, &request.body.target_token)
                })?;
            let lease = core.leases[index].as_ref().expect("indexed lease");
            let handoff = if lease.handoff == HandoffState::HandoffPossibleOrConfirmed
                || lease
                    .sessions
                    .values()
                    .any(|s| s.handoff == HandoffState::HandoffPossibleOrConfirmed)
            {
                HandoffState::HandoffPossibleOrConfirmed
            } else {
                HandoffState::NotHandedOff
            };
            (index, handoff)
        };
        self.terminalize_lease(index, request.body.reason)?;
        Ok(ControlSuccess::Closed(Closed {
            target_token: request.body.target_token,
            session_id: None,
            handoff,
        }))
    }

    /// Atomically seals a lease, cancels every registered/raw operation, and
    /// gives exactly one owner the cleanup receipt.  A concurrent terminal
    /// caller waits for that owner; it cannot manufacture a successful close.
    fn terminalize_lease(&self, index: usize, reason: CloseReason) -> Result<(), ControlFailure> {
        let terminal_now = self.clock.now()?;
        let session_tombstone_expires = session_tombstone_expires(terminal_now)?;
        let (key, operations, raw_links) = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease = core
                .leases
                .get_mut(index)
                .and_then(Option::as_mut)
                .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
            let operations = Arc::clone(&lease.operations);
            let Some(cancellations) = operations.begin_close() else {
                drop(core);
                return operations
                    .wait_for_close_until(cleanup_deadline()?)
                    .map_err(|reason| fail(ErrorStage::Cleanup, reason));
            };
            lease.state = LeaseState::Closing;
            lease.terminal_reason.get_or_insert(reason);
            lease.terminal_handoff = if lease.handoff == HandoffState::HandoffPossibleOrConfirmed
                || lease
                    .sessions
                    .values()
                    .any(|session| session.handoff == HandoffState::HandoffPossibleOrConfirmed)
            {
                HandoffState::HandoffPossibleOrConfirmed
            } else {
                HandoffState::NotHandedOff
            };
            for reference in lease.references.values_mut() {
                reference.state = RefState::Stale;
                if reference.session_id.is_some() {
                    reference.expires = session_tombstone_expires;
                }
            }
            let mut raw_links = lease
                .sessions
                .values()
                .map(|session| Arc::clone(&session.raw))
                .collect::<Vec<_>>();
            if let Some(raw) = &lease.control {
                raw_links.push(Arc::clone(raw));
            }
            for cancellation in cancellations {
                cancellation.cancel();
            }
            (lease.key.clone(), operations, raw_links)
        };
        for raw in raw_links {
            raw.cancel();
        }

        let cleanup_deadline = cleanup_deadline()?;
        if !operations.wait_for_drain_until(cleanup_deadline) {
            self.inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?
                .failed
                .insert(key.selection(), (key.digest, CloseReason::CleanupFailed));
            let inner = Arc::clone(&self.inner);
            std::thread::spawn(move || {
                operations.wait_for_drain();
                let _ = reap_terminal_lease(inner, index, key, operations);
            });
            return Err(fail(ErrorStage::Cleanup, CloseReason::CleanupFailed));
        }
        reap_terminal_lease(Arc::clone(&self.inner), index, key, operations)
            .map_err(|reason| fail(ErrorStage::Cleanup, reason))?;
        Ok(())
    }

    /// Session idle is intentionally narrower than lease expiry: it destroys
    /// only that connection's C1 state and leaves sibling sessions live.
    fn expire_session(&self, index: usize, slot: u64) -> Result<(), ControlFailure> {
        let (raw, mut crypto, handoff, id, _operation) = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease = core
                .leases
                .get_mut(index)
                .and_then(Option::as_mut)
                .filter(|lease| lease.state == LeaseState::Ready)
                .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
            let session = lease
                .sessions
                .get_mut(&slot)
                .filter(|session| session.crypto.is_some())
                .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
            let operation = lease
                .operations
                .enter(Cancellation::new())
                .map_err(|reason| fail(ErrorStage::Close, reason))?;
            (
                Arc::clone(&session.raw),
                session.crypto.take().expect("checked session crypto"),
                session.handoff,
                session.id.clone(),
                operation,
            )
        };
        let _ = raw_transport::close_session(
            raw.as_ref(),
            &mut crypto,
            CloseReason::Stale,
            handoff,
            cleanup_absolute_deadline()?,
        );
        raw.cancel();
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        if let Some(lease) = core.leases.get_mut(index).and_then(Option::as_mut) {
            if let Some(id) = id
                && let Some(reference) = lease
                    .references
                    .values_mut()
                    .find(|reference| reference.session_slot == Some(slot))
            {
                reference.session_id = Some(id);
                reference.expires = session_tombstone_expires(self.clock.now()?)?;
            }
            lease.sessions.remove(&slot);
        }
        Ok(())
    }

    fn heartbeat(&self, index: usize, deadline_ms: u64) -> Result<(), ControlFailure> {
        let (raw, mut reader, mut crypto, counter, misses, deadline, operation) = {
            let mut core = self
                .inner
                .lock()
                .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
            let lease = core
                .leases
                .get_mut(index)
                .and_then(Option::as_mut)
                .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
            if lease.state != LeaseState::Ready {
                return Err(fail(ErrorStage::Close, CloseReason::Stale));
            }
            let operation = lease
                .operations
                .enter(Cancellation::new())
                .map_err(|reason| fail(ErrorStage::Close, reason))?;
            (
                lease
                    .control
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?,
                lease
                    .control_reader
                    .take()
                    .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?,
                lease
                    .lease_crypto
                    .take()
                    .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?,
                lease.heartbeat_counter,
                lease.heartbeat_misses,
                abs(deadline_ms.min(
                    lease
                        .created
                        .saturating_add(LEASE_ABSOLUTE_MS)
                        .min(MAX_SAFE),
                ))?,
                operation,
            )
        };
        // Once a heartbeat request is emitted, its C1 state has a pending
        // authenticated reply. Later intervals must only consume that reply;
        // writing a second request would violate the lease record sequence.
        let heartbeat = if heartbeat_attempt(misses) == HeartbeatAttempt::RequestAndReply {
            raw_transport::heartbeat(raw.as_ref(), &mut reader, &mut crypto, counter, deadline)
        } else {
            raw_transport::heartbeat_reply(raw.as_ref(), &mut reader, &mut crypto, deadline)
                .and_then(|received| {
                    if received == counter {
                        Ok(())
                    } else {
                        Err(TransportFailure {
                            close_reason: CloseReason::SequenceViolation,
                            handoff: HandoffState::NotHandedOff,
                            cleanup_failed: false,
                            #[cfg(feature = "internal-diagnostics")]
                            bootstrap_origin: None,
                        })
                    }
                })
        };
        let successful = match heartbeat {
            Ok(()) => true,
            Err(error) if error.close_reason == CloseReason::Timeout => false,
            Err(error) => {
                let mut core = self
                    .inner
                    .lock()
                    .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
                let lease = core
                    .leases
                    .get_mut(index)
                    .and_then(Option::as_mut)
                    .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
                if lease.state == LeaseState::Ready {
                    lease.lease_crypto = Some(crypto);
                    lease.control_reader = Some(reader);
                    lease.handoff = if lease.handoff == HandoffState::HandoffPossibleOrConfirmed
                        || error.handoff == HandoffState::HandoffPossibleOrConfirmed
                    {
                        HandoffState::HandoffPossibleOrConfirmed
                    } else {
                        HandoffState::NotHandedOff
                    };
                }
                drop(core);
                drop(operation);
                // Cleanup failure is sticky for future prepares, but it must
                // not rewrite the terminal C1 observation of this request.
                let _ = self.terminalize_lease(index, error.close_reason);
                return Err(transport_fail(ErrorStage::Close, error));
            }
        };
        let mut core = self
            .inner
            .lock()
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        let lease = core
            .leases
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| fail(ErrorStage::Close, CloseReason::Stale))?;
        if successful {
            if lease.state != LeaseState::Ready {
                return Err(fail(ErrorStage::Close, CloseReason::Stale));
            }
            lease.lease_crypto = Some(crypto);
            lease.control_reader = Some(reader);
            let completed = self.clock.now()?;
            if let Some(reason) = lease_terminal_reason(
                completed,
                lease.created,
                lease.last_client_activity,
                lease.heartbeat_misses,
                lease.last_heartbeat_success,
            ) {
                drop(core);
                drop(operation);
                let _ = self.terminalize_lease(index, reason);
                return Err(fail(ErrorStage::Close, reason));
            }
            lease.last_heartbeat_success = completed;
            lease.last_heartbeat_attempt = completed;
            lease.heartbeat_misses = 0;
            lease.heartbeat_counter = counter
                .checked_add(1)
                .ok_or_else(|| fail(ErrorStage::Close, CloseReason::InternalError))?;
            Ok(())
        } else {
            if lease.state != LeaseState::Ready {
                return Err(fail(ErrorStage::Close, CloseReason::Stale));
            }
            // A missed reply is not Broker loss until the exact fourth
            // consecutive interval. Keep the crypto state only until the
            // terminal owner seals the lease. Do not cancel here: a late
            // authenticated reply is still the only legal next C1 event.
            lease.lease_crypto = Some(crypto);
            lease.control_reader = Some(reader);
            lease.last_heartbeat_attempt = deadline_ms.saturating_sub(HEARTBEAT_INTERVAL_MS);
            lease.heartbeat_misses = lease.heartbeat_misses.saturating_add(1);
            let terminal = heartbeat_lost(lease.heartbeat_misses);
            drop(core);
            drop(operation);
            if terminal {
                // Cleanup failure remains a sticky selection tombstone, but
                // cannot rewrite this fourth actual timeout as anything but
                // Broker loss for the current request.
                let _ = self.terminalize_lease(index, CloseReason::BrokerLost);
                return Err(fail(ErrorStage::Close, CloseReason::BrokerLost));
            }
            Ok(())
        }
    }
}
struct ReservationWorker {
    private: apppilotkit_transport_crypto_core::BrokerStaticPrivateKey,
    pbs: ProcessBootstrapSecret,
    binding: BootstrapBinding,
    operation: LeaseOperation,
}

fn reap_terminal_lease(
    inner: Arc<Mutex<Core>>,
    index: usize,
    key: PrepareKey,
    operations: Arc<LeaseOperations>,
) -> Result<(), CloseReason> {
    let (
        raw,
        mut crypto,
        cleanup,
        cleanup_already_failed,
        terminal_already_failed,
        reason,
        handoff,
        stale_tokens,
    ) = {
        let mut core = inner.lock().map_err(|_| CloseReason::InternalError)?;
        let terminal_already_failed =
            core.failed
                .get(&key.selection())
                .is_some_and(|(digest, reason)| {
                    *digest == key.digest && *reason == CloseReason::CleanupFailed
                });
        let lease = core
            .leases
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(CloseReason::Stale)?;
        if lease.key != key || lease.state != LeaseState::Closing {
            return Err(CloseReason::Stale);
        }
        let stale_tokens = lease
            .references
            .iter()
            .map(|(token, reference)| (*token, reference.expires))
            .collect::<Vec<_>>();
        (
            lease.control.take(),
            lease.lease_crypto.take(),
            lease.cleanup.take(),
            lease.terminal_cleanup_failed,
            terminal_already_failed,
            lease.terminal_reason.unwrap_or(CloseReason::Stale),
            lease.terminal_handoff,
            stale_tokens,
        )
    };
    if let (Some(raw), Some(crypto)) = (raw.as_ref(), crypto.as_mut()) {
        let _ = raw_transport::close_lease(
            raw.as_ref(),
            crypto,
            reason,
            handoff,
            cleanup_absolute_deadline().map_err(|_| CloseReason::CleanupFailed)?,
        );
    }
    let cleanup_result = if cleanup_already_failed {
        Err(CloseReason::CleanupFailed)
    } else {
        match cleanup {
            Some(cleanup) => cleanup
                .cleanup(
                    Cancellation::new(),
                    cleanup_absolute_deadline().map_err(|_| CloseReason::CleanupFailed)?,
                )
                .map_err(|_| CloseReason::CleanupFailed),
            None => Ok(()),
        }
    };
    let result = if terminal_already_failed {
        Err(CloseReason::CleanupFailed)
    } else {
        cleanup_result
    };
    if let Ok(mut core) = inner.lock() {
        for (token, expires) in stale_tokens {
            core.stale_tokens.insert(token, expires);
        }
        if result.is_err() {
            core.failed
                .insert(key.selection(), (key.digest, CloseReason::CleanupFailed));
        }
        if core
            .leases
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|lease| lease.key == key && lease.state == LeaseState::Closing)
        {
            core.leases[index] = None;
        }
    }
    operations.finish_close(result);
    result
}

fn cleanup_deadline() -> Result<Instant, ControlFailure> {
    // Absolute deadlines cross the raw seam; this monotonic deadline bounds
    // only the broker's wait/reap scheduling and cannot inherit an already
    // expired caller operation deadline.
    let _ = cleanup_absolute_deadline()?;
    Ok(Instant::now() + Duration::from_millis(CLEANUP_MS))
}

fn cleanup_absolute_deadline() -> Result<AbsoluteDeadline, ControlFailure> {
    abs(now()?.saturating_add(CLEANUP_MS).min(MAX_SAFE))
}

fn session_tombstone_expires(now: u64) -> Result<u64, ControlFailure> {
    now.checked_add(SESSION_IDLE_MS)
        .filter(|expires| *expires <= MAX_SAFE)
        .ok_or_else(|| fail(ErrorStage::Cleanup, CloseReason::InternalError))
}

const fn expired_at(current: u64, observed: u64, ttl: u64) -> bool {
    current.saturating_sub(observed) >= ttl
}

const fn heartbeat_lost(misses: u8) -> bool {
    misses >= HEARTBEAT_MISSES
}

const fn lease_terminal_reason(
    current: u64,
    created: u64,
    last_client_activity: u64,
    misses: u8,
    _last_heartbeat_success: u64,
) -> Option<CloseReason> {
    if expired_at(current, created, LEASE_ABSOLUTE_MS) {
        Some(CloseReason::Stale)
    } else if heartbeat_lost(misses) {
        Some(CloseReason::BrokerLost)
    } else if expired_at(current, last_client_activity, LEASE_IDLE_MS) {
        Some(CloseReason::Stale)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeartbeatAttempt {
    RequestAndReply,
    ReplyOnly,
}

const fn heartbeat_attempt(misses: u8) -> HeartbeatAttempt {
    if misses == 0 {
        HeartbeatAttempt::RequestAndReply
    } else {
        HeartbeatAttempt::ReplyOnly
    }
}

fn mint(core: &mut Core, index: usize, now: u64) -> Result<ControlSuccess, ControlFailure> {
    let token = random::<32>(&mut core.entropy)?;
    let expires = now
        .checked_add(READY_REFERENCE_TTL_MS)
        .filter(|v| *v <= MAX_SAFE)
        .ok_or_else(|| fail(ErrorStage::Prepare, CloseReason::InternalError))?;
    let lease = core.leases[index]
        .as_mut()
        .ok_or_else(|| fail(ErrorStage::Prepare, CloseReason::Stale))?;
    lease.references.insert(
        token,
        Reference {
            state: RefState::Minted,
            issued: now,
            expires,
            session_slot: None,
            session_id: None,
        },
    );
    Ok(ControlSuccess::TargetReady(ReadyTarget {
        target_token: token,
        process_generation: lease.generation,
        listener_epoch: lease.epoch,
        issued_at_unix_ms: now,
        expires_at_unix_ms: expires,
    }))
}
fn random<const N: usize>(file: &mut File) -> Result<[u8; N], ControlFailure> {
    for _ in 0..8 {
        let mut b = [0; N];
        file.read_exact(&mut b)
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?;
        if b != [0; N] {
            return Ok(b);
        }
    }
    Err(fail(ErrorStage::Ipc, CloseReason::InternalError))
}
fn now() -> Result<u64, ControlFailure> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))?
            .as_millis(),
    )
    .map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))
}
fn abs(v: u64) -> Result<AbsoluteDeadline, ControlFailure> {
    if !(1..=MAX_SAFE).contains(&v) {
        return Err(fail(ErrorStage::Ipc, CloseReason::InternalError));
    }
    AbsoluteDeadline::new(v).map_err(|_| fail(ErrorStage::Ipc, CloseReason::InternalError))
}
fn fail(stage: ErrorStage, reason: CloseReason) -> ControlFailure {
    let (kind, message) = match reason {
        CloseReason::AuthenticationFailed => (
            ErrorKind::TransportAuthenticationRequired,
            "Target transport authentication failed",
        ),
        CloseReason::Timeout => (ErrorKind::Timeout, "Broker operation timed out"),
        CloseReason::InternalError | CloseReason::CleanupFailed => {
            (ErrorKind::InternalError, "Broker operation failed")
        }
        _ => (ErrorKind::SessionExpired, "Target session expired"),
    };
    ControlFailure {
        kind,
        message,
        retryable: false,
        stage,
        handoff: HandoffState::NotHandedOff,
        close_reason: reason,
    }
}
fn selection(stage: ErrorStage) -> ControlFailure {
    ControlFailure {
        kind: ErrorKind::TargetSelectionRequired,
        message: "Target selection is invalid",
        retryable: false,
        stage,
        handoff: HandoffState::NotHandedOff,
        close_reason: CloseReason::BindingMismatch,
    }
}
fn matched_session_slot(
    lease: &Lease,
    token: &[u8; 32],
    session_id: &str,
    stage: ErrorStage,
    allow_unbound: bool,
) -> Result<u64, ControlFailure> {
    let reference = lease
        .references
        .get(token)
        .filter(|reference| reference.state == RefState::Consumed);
    let Some(reference) = reference else {
        return Err(selection(stage));
    };
    let Some(slot) = reference.session_slot else {
        return Err(selection(stage));
    };
    if reference
        .session_id
        .as_deref()
        .is_some_and(|bound| bound != session_id)
    {
        return Err(selection(stage));
    }
    let Some(session) = lease.sessions.get(&slot) else {
        return Err(if reference.session_id.as_deref() == Some(session_id) {
            fail(stage, CloseReason::Stale)
        } else {
            selection(stage)
        });
    };
    if session
        .id
        .as_deref()
        .is_some_and(|bound| bound != session_id)
    {
        return Err(selection(stage));
    }
    if !allow_unbound && reference.session_id.is_none() {
        return Err(selection(stage));
    }
    Ok(slot)
}
fn token_failure(core: &Core, stage: ErrorStage, token: &[u8; 32]) -> ControlFailure {
    if core.stale_tokens.contains_key(token) {
        fail(stage, CloseReason::Stale)
    } else {
        selection(stage)
    }
}
fn transport_fail(stage: ErrorStage, error: TransportFailure) -> ControlFailure {
    let mut result = fail(stage, error.close_reason);
    result.handoff = error.handoff;
    result
}

#[cfg(feature = "internal-diagnostics")]
fn commit_rejection_origin(reason: CloseReason) -> Option<BootstrapFailureOrigin> {
    if reason == CloseReason::BindingMismatch {
        Some(BootstrapFailureOrigin::AckBindingMismatch)
    } else {
        None
    }
}

#[cfg(feature = "internal-diagnostics")]
fn mark_bootstrap_origin(
    mut failure: ControlFailure,
    origin: BootstrapFailureOrigin,
) -> ControlFailure {
    failure.message = match origin {
        BootstrapFailureOrigin::AdapterRejected => INTERNAL_BOOTSTRAP_ADAPTER_REJECTED,
        BootstrapFailureOrigin::AckBindingMismatch => INTERNAL_BOOTSTRAP_ACK_BINDING_MISMATCH,
    };
    failure
}

const fn platform_launch_reason(kind: PlatformFailureKind) -> CloseReason {
    match kind {
        PlatformFailureKind::CleanupFailed => CloseReason::CleanupFailed,
        PlatformFailureKind::TimedOut | PlatformFailureKind::Cancelled => CloseReason::Timeout,
        PlatformFailureKind::Rejected => CloseReason::BindingMismatch,
        PlatformFailureKind::Eof => CloseReason::PeerClosed,
        PlatformFailureKind::Unavailable | PlatformFailureKind::Internal => {
            CloseReason::InternalError
        }
    }
}

fn platform_launch_failure(failure: PlatformFailure) -> TransportFailure {
    TransportFailure {
        close_reason: platform_launch_reason(failure.primary_kind()),
        handoff: HandoffState::NotHandedOff,
        cleanup_failed: failure.cleanup_failed(),
        #[cfg(feature = "internal-diagnostics")]
        bootstrap_origin: if failure.primary_kind() == PlatformFailureKind::Rejected {
            Some(BootstrapFailureOrigin::AdapterRejected)
        } else {
            None
        },
    }
}

#[cfg(test)]
mod platform_launch_failure_tests {
    use super::*;

    #[test]
    fn cleanup_failure_preserves_primary_close_reason_for_host_decisions() {
        let failure = platform_launch_failure(PlatformFailure::cleanup_failed_after(
            PlatformFailureKind::TimedOut,
        ));
        assert_eq!(failure.close_reason, CloseReason::Timeout);
        assert!(failure.cleanup_failed);
    }

    #[cfg(feature = "internal-diagnostics")]
    #[test]
    fn adapter_rejection_and_defensive_ack_comparison_have_distinct_origins() {
        assert_eq!(
            platform_launch_failure(PlatformFailure::cleanup_failed_after(
                PlatformFailureKind::Rejected,
            ))
            .bootstrap_origin,
            Some(BootstrapFailureOrigin::AdapterRejected)
        );
        assert_eq!(
            commit_rejection_origin(CloseReason::BindingMismatch),
            Some(BootstrapFailureOrigin::AckBindingMismatch)
        );
        assert_eq!(commit_rejection_origin(CloseReason::Timeout), None);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use apppilotkit_transport_crypto_core::{BrokerBootstrap, TargetBootstrap, TargetSession};
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Barrier,
            atomic::{AtomicU64, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
    };

    struct TestClock(AtomicU64);
    impl Clock for TestClock {
        fn now(&self) -> Result<u64, ControlFailure> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    struct NeverAdapter;
    impl PlatformTargetAdapter for NeverAdapter {
        fn begin_launch(
            &self,
            _: TargetSelection,
            _: AbsoluteDeadline,
        ) -> Box<dyn crate::adapter::PendingLaunch> {
            panic!("terminal-only test never launches")
        }
    }

    struct CountingConnector(Arc<AtomicUsize>);
    impl RawConnector for CountingConnector {
        fn connect(
            &self,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<Arc<dyn RawDuplex>, crate::adapter::PlatformFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::adapter::PlatformFailure::new(
                PlatformFailureKind::Internal,
            ))
        }
    }

    struct CountingRaw(Arc<AtomicUsize>);
    impl RawDuplex for CountingRaw {
        fn read(
            &self,
            _: &mut [u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::adapter::PlatformFailure::new(
                PlatformFailureKind::Internal,
            ))
        }
        fn write(
            &self,
            _: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::adapter::PlatformFailure::new(
                PlatformFailureKind::Internal,
            ))
        }
        fn cancel(&self) {}
    }

    struct SessionRaw {
        writes: Mutex<Vec<Vec<u8>>>,
        cancels: AtomicUsize,
        fail_reads: bool,
    }
    impl RawDuplex for SessionRaw {
        fn read(
            &self,
            _: &mut [u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            Err(crate::adapter::PlatformFailure::new(if self.fail_reads {
                PlatformFailureKind::Internal
            } else {
                PlatformFailureKind::Eof
            }))
        }

        fn write(
            &self,
            input: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.writes
                .lock()
                .expect("session writes")
                .push(input.to_vec());
            Ok(input.len())
        }

        fn cancel(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ClockAdvancingRaw {
        writes: AtomicUsize,
        cancels: AtomicUsize,
        clock: Arc<TestClock>,
        advance_to: u64,
    }
    impl RawDuplex for ClockAdvancingRaw {
        fn read(
            &self,
            _: &mut [u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            Err(crate::adapter::PlatformFailure::new(
                PlatformFailureKind::Eof,
            ))
        }

        fn write(
            &self,
            input: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.clock.0.store(self.advance_to, Ordering::SeqCst);
            Ok(input.len())
        }

        fn cancel(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct BlockingCleanup {
        calls: Arc<AtomicUsize>,
        started: Arc<Barrier>,
        release: Arc<Barrier>,
    }
    impl CleanupReceipt for BlockingCleanup {
        fn cleanup(
            self: Box<Self>,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<(), crate::adapter::PlatformFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.wait();
            self.release.wait();
            Ok(())
        }
    }

    struct CountCleanup(Arc<AtomicUsize>);
    impl CleanupReceipt for CountCleanup {
        fn cleanup(
            self: Box<Self>,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<(), crate::adapter::PlatformFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingCleanup(Arc<AtomicUsize>);
    impl CleanupReceipt for FailingCleanup {
        fn cleanup(
            self: Box<Self>,
            _: Cancellation,
            _: AbsoluteDeadline,
        ) -> Result<(), crate::adapter::PlatformFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::adapter::PlatformFailure::new(
                PlatformFailureKind::CleanupFailed,
            ))
        }
    }

    struct MissingHeartbeatRaw {
        writes: Mutex<Vec<Vec<u8>>>,
        reads: AtomicUsize,
        deadlines: Mutex<Vec<u64>>,
    }

    /// A real C1 peer accepts the request, then returns a deliberately
    /// corrupted authenticated heartbeat frame. This distinguishes C1
    /// authentication failure from a platform timeout/missing reply.
    struct CorruptHeartbeatRaw {
        target: Mutex<apppilotkit_transport_crypto_core::TargetLeaseConnection>,
        incoming: Mutex<VecDeque<u8>>,
        writes: AtomicUsize,
    }

    struct ClosingHeartbeatRaw {
        target: Mutex<apppilotkit_transport_crypto_core::TargetLeaseConnection>,
        incoming: Mutex<VecDeque<u8>>,
        writes: AtomicUsize,
    }

    struct ReplyingHeartbeatRaw {
        target: Mutex<apppilotkit_transport_crypto_core::TargetLeaseConnection>,
        incoming: Mutex<VecDeque<u8>>,
        writes: AtomicUsize,
    }
    impl RawDuplex for ReplyingHeartbeatRaw {
        fn read(
            &self,
            output: &mut [u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            let mut incoming = self.incoming.lock().expect("heartbeat input");
            let count = output.len().min(incoming.len());
            for byte in &mut output[..count] {
                *byte = incoming.pop_front().expect("bounded heartbeat input");
            }
            Ok(count)
        }

        fn write(
            &self,
            input: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let mut target = self.target.lock().expect("target lease");
            if let Ok(counter) = target.read_heartbeat_request(input) {
                let reply = target
                    .write_heartbeat_reply(counter)
                    .expect("real C1 heartbeat reply");
                self.incoming.lock().expect("heartbeat input").extend(reply);
            }
            Ok(input.len())
        }

        fn cancel(&self) {}
    }
    impl RawDuplex for ClosingHeartbeatRaw {
        fn read(
            &self,
            output: &mut [u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            let mut incoming = self.incoming.lock().expect("heartbeat input");
            let count = output.len().min(incoming.len());
            for byte in &mut output[..count] {
                *byte = incoming.pop_front().expect("bounded heartbeat input");
            }
            Ok(count)
        }

        fn write(
            &self,
            input: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let mut target = self.target.lock().expect("target lease");
            if target.read_heartbeat_request(input).is_ok() {
                let close = target
                    .write_close(
                        CloseReason::Normal,
                        HandoffState::HandoffPossibleOrConfirmed,
                    )
                    .expect("pending heartbeat permits authenticated C1 Close");
                self.incoming.lock().expect("heartbeat input").extend(close);
            }
            Ok(input.len())
        }

        fn cancel(&self) {}
    }
    impl RawDuplex for CorruptHeartbeatRaw {
        fn read(
            &self,
            output: &mut [u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            let mut incoming = self.incoming.lock().expect("heartbeat input");
            let count = output.len().min(incoming.len());
            for byte in &mut output[..count] {
                *byte = incoming.pop_front().expect("bounded heartbeat input");
            }
            Ok(count)
        }

        fn write(
            &self,
            input: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let mut target = self.target.lock().expect("target lease");
            if let Ok(counter) = target.read_heartbeat_request(input) {
                let mut reply = target
                    .write_heartbeat_reply(counter)
                    .expect("real C1 heartbeat reply");
                *reply.last_mut().expect("nonempty C1 frame") ^= 1;
                self.incoming.lock().expect("heartbeat input").extend(reply);
            }
            Ok(input.len())
        }

        fn cancel(&self) {}
    }
    impl RawDuplex for MissingHeartbeatRaw {
        fn read(
            &self,
            _: &mut [u8],
            deadline: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.deadlines
                .lock()
                .expect("heartbeat deadlines")
                .push(deadline.value());
            Err(crate::adapter::PlatformFailure::new(
                PlatformFailureKind::TimedOut,
            ))
        }

        fn write(
            &self,
            input: &[u8],
            _: AbsoluteDeadline,
        ) -> Result<usize, crate::adapter::PlatformFailure> {
            self.writes
                .lock()
                .expect("heartbeat writes")
                .push(input.to_vec());
            Ok(input.len())
        }

        fn cancel(&self) {}
    }

    fn real_lease_connections() -> (
        BrokerLeaseConnection,
        apppilotkit_transport_crypto_core::TargetLeaseConnection,
    ) {
        let binding = BootstrapBinding {
            target_reference_digest: [1; 32],
            lease_id: [2; 16],
            target_nonce: [3; 32],
            app_artifact_digest: [4; 32],
            expiry_ms: 10_000,
        };
        let keypair = BrokerStaticKeypair::generate().expect("broker keypair");
        let mut target =
            TargetBootstrap::new(binding.clone(), keypair.public_key()).expect("target bootstrap");
        let m1 = target.write_m1().expect("target M1");
        let pbs = ProcessBootstrapSecret::generate().expect("bootstrap secret");
        let broker = BrokerBootstrap::new(binding, keypair.into_private_key(), &pbs)
            .expect("broker bootstrap");
        let (m2, receiver) = broker.read_m1_write_m2(&m1).expect("broker M2");
        let (sender, _) = target.read_m2(&m2, 1, 1).expect("target reads M2");
        let (ack, target_lease) = sender.write_ack().expect("target ACK");
        let (_, broker_lease) = receiver.read_ack(&ack).expect("broker reads ACK");
        (broker_lease, target_lease)
    }

    fn real_session_connections(binding: SessionBinding) -> (BrokerSession, TargetSession) {
        let pbs = ProcessBootstrapSecret::generate().expect("bootstrap secret");
        let mut target = TargetSession::new(binding.clone(), &pbs).expect("target session");
        let mut broker = BrokerSession::new(binding, &pbs).expect("broker session");
        let m1 = target.write_m1().expect("target M1");
        let m2 = broker.read_m1_write_m2(&m1).expect("broker M2");
        target.read_m2(&m2).expect("target reads M2");
        let target_finished = target.write_finished().expect("target Finished");
        broker
            .read_finished(&target_finished)
            .expect("broker reads Finished");
        let broker_finished = broker.write_finished().expect("broker Finished");
        target
            .read_finished(&broker_finished)
            .expect("target reads Finished");
        let mut opened = None;
        for frame in broker.write_session_open(b"open").expect("session.open") {
            opened = target.read_application(&frame).expect("target reads open");
        }
        assert_eq!(opened.as_deref(), Some(&b"open"[..]));
        let mut response = None;
        for frame in target
            .write_application_response(b"opened")
            .expect("open response")
        {
            response = broker
                .read_application_response(&frame)
                .expect("broker reads open response");
        }
        assert_eq!(response.as_deref(), Some(&b"opened"[..]));
        (broker, target)
    }

    fn terminal_test_broker(
        cleanup: Box<dyn CleanupReceipt>,
    ) -> (SessionBroker, Arc<LeaseOperations>, Arc<TestClock>) {
        let operations = Arc::new(LeaseOperations::new());
        let adapter: Arc<dyn PlatformTargetAdapter> = Arc::new(NeverAdapter);
        let lease = Lease {
            key: PrepareKey {
                platform: Platform::IosSimulator,
                device: "device".into(),
                app: "app".into(),
                digest: [0; 32],
            },
            state: LeaseState::Ready,
            lease_id: [0; 16],
            generation: 0,
            epoch: 0,
            nk_hash: [0; 32],
            pbs: None,
            control: None,
            lease_crypto: None,
            control_reader: None,
            connector: None,
            cleanup: Some(cleanup),
            _adapter: Arc::clone(&adapter),
            references: HashMap::new(),
            sessions: HashMap::new(),
            next_session: 1,
            created: 0,
            last_heartbeat_attempt: 0,
            last_heartbeat_success: 0,
            heartbeat_counter: 1,
            heartbeat_misses: 0,
            last_client_activity: 0,
            terminal_cleanup_failed: false,
            terminal_reason: None,
            terminal_handoff: HandoffState::NotHandedOff,
            handoff: HandoffState::NotHandedOff,
            operations: Arc::clone(&operations),
        };
        let clock = Arc::new(TestClock(AtomicU64::new(1)));
        let broker = SessionBroker {
            inner: Arc::new(Mutex::new(Core {
                leases: vec![Some(lease)],
                failed: HashMap::new(),
                stale_tokens: HashMap::new(),
                ios: Arc::clone(&adapter),
                android: adapter,
                shutting_down: false,
                entropy: File::open("/dev/urandom").expect("entropy"),
            })),
            pending_reapers: Arc::new(AtomicUsize::new(0)),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
        };
        (broker, operations, clock)
    }

    #[test]
    fn ttl_boundaries_and_heartbeat_threshold_are_inclusive() {
        assert!(!expired_at(29_999, 0, SESSION_IDLE_MS));
        assert!(expired_at(30_000, 0, SESSION_IDLE_MS));
        assert!(!expired_at(119_999, 0, LEASE_IDLE_MS));
        assert!(expired_at(120_000, 0, LEASE_IDLE_MS));
        assert!(!expired_at(899_999, 0, LEASE_ABSOLUTE_MS));
        assert!(expired_at(900_000, 0, LEASE_ABSOLUTE_MS));
        assert!(!heartbeat_lost(HEARTBEAT_MISSES - 1));
        assert!(heartbeat_lost(HEARTBEAT_MISSES));
        assert_eq!(
            lease_terminal_reason(120_000, 0, 0, HEARTBEAT_MISSES, 0),
            Some(CloseReason::BrokerLost)
        );
        assert_eq!(
            lease_terminal_reason(900_000, 0, 0, 1, 0),
            Some(CloseReason::Stale)
        );
        assert_eq!(heartbeat_attempt(0), HeartbeatAttempt::RequestAndReply);
        assert_eq!(heartbeat_attempt(1), HeartbeatAttempt::ReplyOnly);
        assert_eq!(
            heartbeat_attempt(HEARTBEAT_MISSES - 1),
            HeartbeatAttempt::ReplyOnly
        );
    }

    #[test]
    fn slow_launch_does_not_consume_the_connected_bootstrap_budget() {
        let started = 1_000_000;
        let prepare = prepare_deadline(started, u64::MAX, Platform::AndroidEmulator)
            .expect("bounded prepare deadline");
        assert_eq!(
            prepare,
            started + ANDROID_PREPARE_LAUNCH_MS + PREPARE_BOOTSTRAP_MS
        );
        assert_eq!(
            launch_deadline(started, prepare, Platform::AndroidEmulator).expect("launch deadline"),
            started + ANDROID_PREPARE_LAUNCH_MS
        );

        let connected = started + ANDROID_PREPARE_LAUNCH_MS - 1;
        assert_eq!(
            bootstrap_deadline(connected, prepare).expect("connected bootstrap deadline"),
            connected + PREPARE_BOOTSTRAP_MS,
            "the adapter receives a full bootstrap budget after a slow launch"
        );
        assert_eq!(
            bootstrap_deadline(prepare, prepare).expect("overall cap"),
            prepare,
            "the two phase prepare cap remains fail-closed"
        );

        let ios_prepare = prepare_deadline(started, u64::MAX, Platform::IosSimulator)
            .expect("iOS prepare deadline");
        assert_eq!(
            ios_prepare,
            started + IOS_PREPARE_LAUNCH_MS + PREPARE_BOOTSTRAP_MS
        );
    }

    #[test]
    fn commit_deadline_boundary_is_timeout_before_binding_classification() {
        let clock = TestClock(AtomicU64::new(30_000));
        let deadline = 30_000;
        assert_eq!(clock.now().expect("test clock"), deadline);
        assert_eq!(
            commit_rejection_reason(clock.now().expect("test clock"), deadline, false),
            Some(CloseReason::Timeout),
            "an authenticated result at the inclusive prepare deadline is a timeout"
        );
        clock.0.store(deadline - 1, Ordering::SeqCst);
        assert_eq!(
            commit_rejection_reason(clock.now().expect("test clock"), deadline, false),
            Some(CloseReason::BindingMismatch),
            "only a timely bad acknowledgement is a binding mismatch"
        );
    }

    #[cfg(feature = "internal-diagnostics")]
    #[test]
    fn authenticated_ack_commit_mismatch_survives_broker_ipc_as_the_ack_marker() {
        let operations = Arc::new(LeaseOperations::new());
        let adapter: Arc<dyn PlatformTargetAdapter> = Arc::new(NeverAdapter);
        let key = PrepareKey {
            platform: Platform::IosSimulator,
            device: "device".into(),
            app: "app".into(),
            digest: [0x61; 32],
        };
        let token = [0x62; 32];
        let lease_id = [0x63; 16];
        let deadline = 2;
        let lease = Lease {
            key: key.clone(),
            state: LeaseState::Preparing,
            lease_id,
            generation: 0,
            epoch: 0,
            nk_hash: [0; 32],
            pbs: None,
            control: None,
            lease_crypto: None,
            control_reader: None,
            connector: None,
            cleanup: None,
            _adapter: Arc::clone(&adapter),
            references: HashMap::from([(
                token,
                Reference {
                    state: RefState::Pending,
                    issued: 0,
                    expires: deadline,
                    session_slot: None,
                    session_id: None,
                },
            )]),
            sessions: HashMap::new(),
            next_session: 1,
            created: 0,
            last_heartbeat_attempt: 0,
            last_heartbeat_success: 0,
            heartbeat_counter: 1,
            heartbeat_misses: 0,
            last_client_activity: 0,
            terminal_cleanup_failed: false,
            terminal_reason: None,
            terminal_handoff: HandoffState::NotHandedOff,
            handoff: HandoffState::NotHandedOff,
            operations: Arc::clone(&operations),
        };
        let clock = Arc::new(TestClock(AtomicU64::new(1)));
        let broker = SessionBroker {
            inner: Arc::new(Mutex::new(Core {
                leases: vec![Some(lease)],
                failed: HashMap::new(),
                stale_tokens: HashMap::new(),
                ios: Arc::clone(&adapter),
                android: adapter,
                shutting_down: false,
                entropy: File::open("/dev/urandom").expect("entropy"),
            })),
            pending_reapers: Arc::new(AtomicUsize::new(0)),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
        };
        let binding = BootstrapBinding {
            target_reference_digest: key.digest,
            lease_id,
            target_nonce: [0x64; 32],
            app_artifact_digest: [0x65; 32],
            expiry_ms: deadline,
        };
        let keypair = BrokerStaticKeypair::generate().expect("broker keypair");
        let mut target =
            TargetBootstrap::new(binding.clone(), keypair.public_key()).expect("target bootstrap");
        let pbs = ProcessBootstrapSecret::generate().expect("bootstrap secret");
        let bootstrap = BrokerBootstrap::new(binding, keypair.into_private_key(), &pbs)
            .expect("broker bootstrap");
        let m1 = target.write_m1().expect("target M1");
        let (m2, receiver) = bootstrap.read_m1_write_m2(&m1).expect("broker M2");
        let (sender, _) = target.read_m2(&m2, 7, 1).expect("target ACK sender");
        let (ack_outer, _) = sender.write_ack().expect("authenticated ACK");
        let (mut ack, lease_crypto) = receiver
            .read_ack(&ack_outer)
            .expect("broker authenticates ACK");
        assert_eq!(ack.listener_epoch, 1);
        ack.listener_epoch = 2;
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let prepared = Prepared {
            success: BootstrapSuccess {
                ack,
                lease: lease_crypto,
                bootstrap: Arc::new(CountingRaw(Arc::new(AtomicUsize::new(0)))),
                connector: Arc::new(CountingConnector(Arc::new(AtomicUsize::new(0)))),
                cleanup: Box::new(CountCleanup(Arc::clone(&cleanup_calls))),
                reader: raw_transport::RawFrameReader::new(),
            },
            pbs,
            operation: operations
                .enter(Cancellation::new())
                .expect("prepared operation"),
        };
        let failure = broker
            .commit(
                CompletionReservation {
                    index: 0,
                    key,
                    token,
                    digest: [0x61; 32],
                    lease_id,
                    deadline,
                },
                prepared,
            )
            .expect_err("defensive commit must reject the altered authenticated ACK");

        assert_eq!(failure.stage, ErrorStage::Bootstrap);
        assert_eq!(failure.close_reason, CloseReason::BindingMismatch);
        assert_eq!(failure.message, INTERNAL_BOOTSTRAP_ACK_BINDING_MISMATCH);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        let packet =
            crate::encode_failure_packet([0x66; 16], &failure).expect("Broker IPC failure");
        assert_eq!(
            crate::decode_result_packet(&packet),
            Ok(crate::ControlResult::Failure {
                request_id: [0x66; 16],
                error: failure,
            })
        );
    }

    #[test]
    fn terminal_gate_cancels_then_waits_for_the_registered_io_owner() {
        let operations = Arc::new(LeaseOperations::new());
        let cancellation = Cancellation::new();
        let operation = operations.enter(cancellation.clone()).expect("active I/O");
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker = thread::spawn(move || {
            worker_entered.wait();
            worker_release.wait();
            drop(operation);
        });
        entered.wait();
        let cancellations = operations.begin_close().expect("unique terminal owner");
        for cancellation in cancellations {
            cancellation.cancel();
        }
        assert!(cancellation.is_cancelled());
        assert!(!operations.wait_for_drain_until(Instant::now()));
        release.wait();
        worker.join().expect("I/O owner exits");
        assert!(operations.wait_for_drain_until(Instant::now()));
        assert_eq!(
            operations.wait_for_close_until(Instant::now()),
            Err(CloseReason::CleanupFailed)
        );
    }

    #[test]
    fn timed_out_terminal_reaper_never_consumes_cleanup_before_io_releases() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cleanup_started = Arc::new(Barrier::new(2));
        let cleanup_release = Arc::new(Barrier::new(2));
        let (broker, operations, _) = terminal_test_broker(Box::new(BlockingCleanup {
            calls: Arc::clone(&calls),
            started: Arc::clone(&cleanup_started),
            release: Arc::clone(&cleanup_release),
        }));
        let io_release = Arc::new(Barrier::new(2));
        let io_gate = Arc::clone(&io_release);
        let operation = operations
            .enter(Cancellation::new())
            .expect("in-flight I/O");
        let io = thread::spawn(move || {
            io_gate.wait();
            drop(operation);
        });
        let (done, received) = mpsc::channel();
        let terminal = thread::spawn(move || {
            let result = broker.terminalize_lease(0, CloseReason::Stale);
            done.send(result).expect("terminal result sent");
        });
        let result = received.recv().expect("bounded terminal returns");
        assert_eq!(result.unwrap_err().close_reason, CloseReason::CleanupFailed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        io_release.wait();
        io.join().expect("I/O releases");
        cleanup_started.wait();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cleanup_release.wait();
        assert_eq!(
            operations.wait_for_close_until(Instant::now() + Duration::from_secs(2)),
            Err(CloseReason::CleanupFailed),
            "late cleanup success cannot overwrite the bounded terminal failure"
        );
        terminal.join().expect("terminal caller joins");
    }

    #[test]
    fn closing_lease_keeps_selection_occupied_until_cleanup_finishes() {
        let cleanup_started = Arc::new(Barrier::new(2));
        let cleanup_release = Arc::new(Barrier::new(2));
        let (broker, _, _) = terminal_test_broker(Box::new(BlockingCleanup {
            calls: Arc::new(AtomicUsize::new(0)),
            started: Arc::clone(&cleanup_started),
            release: Arc::clone(&cleanup_release),
        }));
        let broker = Arc::new(broker);
        let terminal_broker = Arc::clone(&broker);
        let terminal = thread::spawn(move || {
            terminal_broker
                .terminalize_lease(0, CloseReason::Stale)
                .expect("cleanup eventually succeeds")
        });

        cleanup_started.wait();
        let conflict = broker
            .handle(same_key_prepare(2))
            .expect_err("Closing lease remains the selected owner during cleanup");
        assert_eq!(conflict.close_reason, CloseReason::Stale);
        cleanup_release.wait();
        terminal.join().expect("terminal owner joins");
    }

    #[test]
    fn shutdown_consumes_a_ready_lease_cleanup_once() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, _) =
            terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));

        broker
            .shutdown(CloseReason::BrokerLost)
            .expect("shutdown cleanup succeeds");
        broker
            .shutdown(CloseReason::BrokerLost)
            .expect("repeated shutdown has no remaining receipt");

        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            broker
                .handle(same_key_prepare(2))
                .expect_err("shutdown rejects a new launch")
                .close_reason,
            CloseReason::BrokerLost
        );
    }

    #[test]
    fn shutdown_cleanup_failure_tombstones_the_selection() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, _) =
            terminal_test_broker(Box::new(FailingCleanup(Arc::clone(&cleanup_calls))));

        let error = broker
            .shutdown(CloseReason::BrokerLost)
            .expect_err("cleanup failure is reported");
        assert_eq!(error.close_reason, CloseReason::CleanupFailed);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            broker
                .handle(same_key_prepare(2))
                .expect_err("cleanup tombstone blocks the same selection")
                .close_reason,
            CloseReason::CleanupFailed
        );
        let core = broker.inner.lock().expect("shutdown state");
        assert!(
            core.failed.contains_key(
                &PrepareKey {
                    platform: Platform::IosSimulator,
                    device: "device".into(),
                    app: "app".into(),
                    digest: [0; 32],
                }
                .selection()
            )
        );
    }

    #[test]
    fn shutdown_also_terminalizes_a_stale_lease() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, _) =
            terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
        broker.inner.lock().expect("test core").leases[0]
            .as_mut()
            .expect("lease")
            .state = LeaseState::Stale;

        broker
            .shutdown(CloseReason::BrokerLost)
            .expect("stale lease cleanup succeeds");

        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    }

    fn same_key_prepare(deadline: u64) -> ControlRequest {
        ControlRequest::Prepare(Request {
            request_id: [7; 16],
            deadline_unix_ms: deadline,
            body: PrepareBody {
                platform: Platform::IosSimulator,
                device_selector: "device".into(),
                app_id: "app".into(),
                app_artifact: "/tmp/app".into(),
                app_artifact_sha256: [0; 32],
            },
        })
    }

    #[test]
    fn prepare_samples_eligibility_before_reusing_a_ready_lease() {
        for (at, misses, expected) in [
            (LEASE_IDLE_MS, 0, CloseReason::Stale),
            (LEASE_ABSOLUTE_MS, 0, CloseReason::Stale),
            (
                HEARTBEAT_INTERVAL_MS * HEARTBEAT_MISSES as u64,
                HEARTBEAT_MISSES,
                CloseReason::BrokerLost,
            ),
        ] {
            let cleanup_calls = Arc::new(AtomicUsize::new(0));
            let (broker, _, clock) =
                terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
            clock.0.store(at, Ordering::SeqCst);
            if misses != 0 {
                let mut core = broker.inner.lock().expect("test core");
                let lease = core.leases[0].as_mut().expect("ready lease");
                lease.heartbeat_misses = misses;
            }
            let error = broker
                .handle(same_key_prepare(at + 1))
                .expect_err("expired lease is never reminted");
            assert_eq!(error.close_reason, expected);
            assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn prepare_observes_real_missing_heartbeats_before_reuse() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) =
            terminal_test_broker(Box::new(FailingCleanup(Arc::clone(&cleanup_calls))));
        let (broker_lease, mut target_lease) = real_lease_connections();
        let raw = Arc::new(MissingHeartbeatRaw {
            writes: Mutex::new(Vec::new()),
            reads: AtomicUsize::new(0),
            deadlines: Mutex::new(Vec::new()),
        });
        let base = now().expect("system clock");
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.generation = 1;
            lease.epoch = 1;
            lease.control = Some(Arc::clone(&raw) as Arc<dyn RawDuplex>);
            lease.lease_crypto = Some(broker_lease);
            lease.control_reader = Some(raw_transport::RawFrameReader::new());
            lease.created = base;
            lease.last_client_activity = base;
            lease.last_heartbeat_attempt = base;
            lease.last_heartbeat_success = base;
        }

        for interval in 1..=HEARTBEAT_MISSES {
            if interval == HEARTBEAT_MISSES {
                // A client command arrived after the third timeout; lease
                // idle cannot mask the required fourth actual heartbeat miss.
                broker.inner.lock().expect("test core").leases[0]
                    .as_mut()
                    .expect("ready lease")
                    .last_client_activity = base + HEARTBEAT_INTERVAL_MS * 3 + 1;
            }
            clock.0.store(
                base + HEARTBEAT_INTERVAL_MS * u64::from(interval),
                Ordering::SeqCst,
            );
            let result = broker.maintain();
            if interval < HEARTBEAT_MISSES {
                result.expect("miss remains below threshold");
            } else {
                assert_eq!(
                    result
                        .expect_err("the fourth actual timeout loses the Broker")
                        .close_reason,
                    CloseReason::BrokerLost
                );
            }
        }
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            raw.reads.load(Ordering::SeqCst),
            usize::from(HEARTBEAT_MISSES),
            "all four counted misses are actual platform timeout reads"
        );

        let writes = raw.writes.lock().expect("heartbeat writes");
        assert_eq!(writes.len(), 2, "one heartbeat request and one lease close");
        assert_eq!(
            target_lease
                .read_heartbeat_request(&writes[0])
                .expect("authenticated heartbeat request"),
            1
        );
        assert_eq!(
            target_lease
                .read_close(&writes[1])
                .expect("authenticated BrokerLost close"),
            (CloseReason::BrokerLost, HandoffState::NotHandedOff)
        );
        assert_eq!(
            broker
                .handle(same_key_prepare(
                    base + HEARTBEAT_INTERVAL_MS * u64::from(HEARTBEAT_MISSES) + 1,
                ))
                .expect_err("failed cleanup remains a sticky selection tombstone")
                .close_reason,
            CloseReason::CleanupFailed
        );
    }

    #[test]
    fn authenticated_heartbeat_failure_is_terminal_not_a_broker_loss_miss() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
        let (broker_lease, target_lease) = real_lease_connections();
        let raw = Arc::new(CorruptHeartbeatRaw {
            target: Mutex::new(target_lease),
            incoming: Mutex::new(VecDeque::new()),
            writes: AtomicUsize::new(0),
        });
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.control = Some(Arc::clone(&raw) as Arc<dyn RawDuplex>);
            lease.lease_crypto = Some(broker_lease);
            lease.control_reader = Some(raw_transport::RawFrameReader::new());
            let base = now().expect("system clock");
            lease.created = base;
            lease.last_client_activity = base;
            lease.last_heartbeat_attempt = base;
            lease.last_heartbeat_success = base;
            clock
                .0
                .store(base + HEARTBEAT_INTERVAL_MS, Ordering::SeqCst);
        }

        let error = broker
            .maintain()
            .expect_err("real C1 authentication failure terminates immediately");

        assert_eq!(error.close_reason, CloseReason::AuthenticationFailed);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            raw.writes.load(Ordering::SeqCst),
            1,
            "failure is not retried as a miss"
        );
    }

    #[test]
    fn heartbeat_deadline_is_clamped_to_the_absolute_lease_cap() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let (broker_lease, _) = real_lease_connections();
        let raw = Arc::new(MissingHeartbeatRaw {
            writes: Mutex::new(Vec::new()),
            reads: AtomicUsize::new(0),
            deadlines: Mutex::new(Vec::new()),
        });
        let base = now().expect("system clock");
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.control = Some(Arc::clone(&raw) as Arc<dyn RawDuplex>);
            lease.lease_crypto = Some(broker_lease);
            lease.control_reader = Some(raw_transport::RawFrameReader::new());
            lease.created = base;
            lease.last_client_activity = base + LEASE_ABSOLUTE_MS - 1;
            lease.last_heartbeat_attempt = base;
            lease.last_heartbeat_success = base;
        }
        clock
            .0
            .store(base + LEASE_ABSOLUTE_MS - 1, Ordering::SeqCst);
        broker
            .heartbeat(0, base + LEASE_ABSOLUTE_MS + HEARTBEAT_INTERVAL_MS)
            .expect("first timeout remains below the miss threshold");
        assert_eq!(
            *raw.deadlines.lock().expect("heartbeat deadlines"),
            vec![base + LEASE_ABSOLUTE_MS]
        );
    }

    #[test]
    fn authenticated_heartbeat_close_is_terminal_with_its_handoff() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
        let (broker_lease, target_lease) = real_lease_connections();
        let raw = Arc::new(ClosingHeartbeatRaw {
            target: Mutex::new(target_lease),
            incoming: Mutex::new(VecDeque::new()),
            writes: AtomicUsize::new(0),
        });
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.control = Some(Arc::clone(&raw) as Arc<dyn RawDuplex>);
            lease.lease_crypto = Some(broker_lease);
            lease.control_reader = Some(raw_transport::RawFrameReader::new());
            let base = now().expect("system clock");
            lease.created = base;
            lease.last_client_activity = base;
            lease.last_heartbeat_attempt = base;
            lease.last_heartbeat_success = base;
            clock
                .0
                .store(base + HEARTBEAT_INTERVAL_MS, Ordering::SeqCst);
        }

        let error = broker
            .maintain()
            .expect_err("first authenticated Close terminalizes the lease");
        assert_eq!(error.close_reason, CloseReason::PeerClosed);
        assert_eq!(error.handoff, HandoffState::HandoffPossibleOrConfirmed);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(raw.writes.load(Ordering::SeqCst), 1, "Close is not a miss");
    }

    #[test]
    fn authenticated_heartbeat_close_keeps_original_failure_when_cleanup_fails() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) =
            terminal_test_broker(Box::new(FailingCleanup(Arc::clone(&cleanup_calls))));
        let (broker_lease, target_lease) = real_lease_connections();
        let raw = Arc::new(ClosingHeartbeatRaw {
            target: Mutex::new(target_lease),
            incoming: Mutex::new(VecDeque::new()),
            writes: AtomicUsize::new(0),
        });
        let base = now().expect("system clock");
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.control = Some(Arc::clone(&raw) as Arc<dyn RawDuplex>);
            lease.lease_crypto = Some(broker_lease);
            lease.control_reader = Some(raw_transport::RawFrameReader::new());
            lease.created = base;
            lease.last_client_activity = base;
            lease.last_heartbeat_attempt = base;
            lease.last_heartbeat_success = base;
        }
        clock
            .0
            .store(base + HEARTBEAT_INTERVAL_MS, Ordering::SeqCst);

        let error = broker
            .maintain()
            .expect_err("authenticated Close remains visible");
        assert_eq!(error.close_reason, CloseReason::PeerClosed);
        assert_eq!(error.handoff, HandoffState::HandoffPossibleOrConfirmed);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            broker
                .handle(same_key_prepare(base + HEARTBEAT_INTERVAL_MS + 1))
                .expect_err("failed cleanup tombstones the selection")
                .close_reason,
            CloseReason::CleanupFailed
        );
    }

    #[test]
    fn healthy_authenticated_heartbeats_do_not_extend_client_idle_ttl() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
        let (broker_lease, target_lease) = real_lease_connections();
        let raw = Arc::new(ReplyingHeartbeatRaw {
            target: Mutex::new(target_lease),
            incoming: Mutex::new(VecDeque::new()),
            writes: AtomicUsize::new(0),
        });
        let base = now().expect("system clock");
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.control = Some(Arc::clone(&raw) as Arc<dyn RawDuplex>);
            lease.lease_crypto = Some(broker_lease);
            lease.control_reader = Some(raw_transport::RawFrameReader::new());
            lease.created = base;
            lease.last_client_activity = base;
            lease.last_heartbeat_attempt = base;
            lease.last_heartbeat_success = base;
        }
        for interval in 1..4 {
            clock
                .0
                .store(base + HEARTBEAT_INTERVAL_MS * interval, Ordering::SeqCst);
            broker.maintain().expect("healthy C1 heartbeat succeeds");
        }
        clock.0.store(base + LEASE_IDLE_MS, Ordering::SeqCst);
        broker
            .maintain()
            .expect("lease-idle reaper owns its successful terminal cleanup");
        assert_eq!(
            raw.writes.load(Ordering::SeqCst),
            4,
            "three heartbeats then close"
        );
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn closed_reference_is_stale_only_until_its_original_expiry() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) = terminal_test_broker(Box::new(CountCleanup(cleanup_calls)));
        let token = [9; 32];
        {
            let mut core = broker.inner.lock().expect("test core");
            core.leases[0]
                .as_mut()
                .expect("ready lease")
                .references
                .insert(
                    token,
                    Reference {
                        state: RefState::Minted,
                        issued: 1,
                        expires: 2,
                        session_slot: None,
                        session_id: None,
                    },
                );
        }
        broker
            .handle(ControlRequest::CloseLease(Request {
                request_id: [8; 16],
                deadline_unix_ms: 3,
                body: crate::CloseLeaseBody {
                    target_token: token,
                    reason: CloseReason::Normal,
                },
            }))
            .expect("close succeeds");
        let stale = broker
            .handle(ControlRequest::OpenSession(Request {
                request_id: [9; 16],
                deadline_unix_ms: 3,
                body: crate::OpenSessionBody {
                    target_token: token,
                    session_id: None,
                    required_capabilities: Vec::new(),
                    session_open_request: Some(b"open".to_vec()),
                    session_open_request_sha256: Some(Sha256::digest(b"open").into()),
                },
            }))
            .expect_err("unexpired tombstone is stale");
        assert_eq!(stale.close_reason, CloseReason::Stale);
        clock.0.store(2, Ordering::SeqCst);
        let unknown = broker
            .handle(ControlRequest::OpenSession(Request {
                request_id: [10; 16],
                deadline_unix_ms: 3,
                body: crate::OpenSessionBody {
                    target_token: token,
                    session_id: None,
                    required_capabilities: Vec::new(),
                    session_open_request: Some(b"open".to_vec()),
                    session_open_request_sha256: Some(Sha256::digest(b"open").into()),
                },
            }))
            .expect_err("expired tombstone is unknown");
        assert_eq!(unknown.close_reason, CloseReason::BindingMismatch);
    }

    #[test]
    fn closed_session_tombstone_is_match_only_and_pruned_at_its_ttl() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let token = [10; 32];
        {
            let mut core = broker.inner.lock().expect("test core");
            core.leases[0]
                .as_mut()
                .expect("ready lease")
                .references
                .insert(
                    token,
                    Reference {
                        state: RefState::Consumed,
                        issued: 1,
                        expires: 2,
                        session_slot: Some(1),
                        session_id: Some("session-a".into()),
                    },
                );
        }
        let request = || {
            ControlRequest::OpenSession(Request {
                request_id: [11; 16],
                deadline_unix_ms: 3,
                body: crate::OpenSessionBody {
                    target_token: token,
                    session_id: Some("session-a".into()),
                    required_capabilities: Vec::new(),
                    session_open_request: None,
                    session_open_request_sha256: None,
                },
            })
        };
        assert_eq!(
            broker
                .handle(request())
                .expect_err("matching closed session is stale before its TTL")
                .close_reason,
            CloseReason::Stale
        );
        clock.0.store(2, Ordering::SeqCst);
        assert_eq!(
            broker
                .handle(request())
                .expect_err("expired session tombstone no longer selects a target")
                .close_reason,
            CloseReason::BindingMismatch
        );
    }

    #[test]
    fn close_session_lazily_binds_the_target_issued_id_and_prunes_exactly() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let base = now().expect("system clock");
        let token = [21; 32];
        let bound_token = [22; 32];
        let raw = Arc::new(SessionRaw {
            writes: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            fail_reads: false,
        });
        let bound_raw = Arc::new(SessionRaw {
            writes: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            fail_reads: false,
        });
        let binding = SessionBinding {
            lease_id: [23; 16],
            process_generation: 7,
            listener_epoch: 3,
            nk_handshake_hash: [24; 32],
        };
        let (crypto, mut target) = real_session_connections(binding.clone());
        let (bound_crypto, _) = real_session_connections(binding.clone());
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.lease_id = binding.lease_id;
            lease.generation = binding.process_generation;
            lease.epoch = binding.listener_epoch;
            lease.nk_hash = binding.nk_handshake_hash;
            lease.created = base;
            lease.last_client_activity = base;
            lease.references.insert(
                token,
                Reference {
                    state: RefState::Consumed,
                    issued: base,
                    expires: base + READY_REFERENCE_TTL_MS,
                    session_slot: Some(1),
                    session_id: None,
                },
            );
            lease.references.insert(
                bound_token,
                Reference {
                    state: RefState::Consumed,
                    issued: base,
                    expires: base + READY_REFERENCE_TTL_MS,
                    session_slot: Some(2),
                    session_id: Some("already-bound".into()),
                },
            );
            lease.sessions.insert(
                1,
                Session {
                    id: None,
                    raw: Arc::clone(&raw) as Arc<dyn RawDuplex>,
                    crypto: Some(crypto),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: b"opened".to_vec(),
                    response_digest: Sha256::digest(b"opened").into(),
                    handoff: HandoffState::HandoffPossibleOrConfirmed,
                    last: base,
                },
            );
            lease.sessions.insert(
                2,
                Session {
                    id: Some("already-bound".into()),
                    raw: Arc::clone(&bound_raw) as Arc<dyn RawDuplex>,
                    crypto: Some(bound_crypto),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: b"opened".to_vec(),
                    response_digest: Sha256::digest(b"opened").into(),
                    handoff: HandoffState::NotHandedOff,
                    last: base,
                },
            );
        }
        clock.0.store(base, Ordering::SeqCst);
        let close = |target_token, session_id: &str| {
            broker.handle(ControlRequest::CloseSession(Request {
                request_id: [25; 16],
                deadline_unix_ms: base + 5_000,
                body: crate::CloseSessionBody {
                    target_token,
                    session_id: session_id.into(),
                    process_generation: binding.process_generation,
                    listener_epoch: binding.listener_epoch,
                    reason: CloseReason::Normal,
                },
            }))
        };

        assert_eq!(
            close([99; 32], "target-session")
                .expect_err("unknown token does not select the lease")
                .close_reason,
            CloseReason::BindingMismatch
        );
        assert_eq!(
            close(bound_token, "different")
                .expect_err("an already-bound session cannot be rebound")
                .close_reason,
            CloseReason::BindingMismatch
        );
        assert!(matches!(
            close(token, "target-session").expect("first operation may be close"),
            ControlSuccess::Closed(_)
        ));
        let stale = close(token, "target-session")
            .expect_err("the explicitly closed session remains stale");
        assert_eq!(stale.kind, ErrorKind::SessionExpired);
        assert_eq!(stale.close_reason, CloseReason::Stale);
        let mismatch = close(token, "different")
            .expect_err("a different session id does not select the close tombstone");
        assert_eq!(mismatch.kind, ErrorKind::TargetSelectionRequired);
        assert_eq!(mismatch.close_reason, CloseReason::BindingMismatch);
        assert_eq!(raw.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(bound_raw.cancels.load(Ordering::SeqCst), 0);
        let writes = raw.writes.lock().expect("session writes");
        assert_eq!(writes.len(), 1);
        assert_eq!(
            target.read_close(&writes[0]).expect("authenticated close"),
            (
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
        drop(writes);
        {
            let core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_ref().expect("ready lease");
            assert!(!lease.sessions.contains_key(&1));
            assert!(lease.sessions.contains_key(&2));
            let reference = lease.references.get(&token).expect("close tombstone");
            assert_eq!(reference.session_id.as_deref(), Some("target-session"));
            assert_eq!(reference.expires, base + SESSION_IDLE_MS);
        }
        let match_only = || {
            broker.handle(ControlRequest::OpenSession(Request {
                request_id: [26; 16],
                deadline_unix_ms: base + SESSION_IDLE_MS + 1,
                body: crate::OpenSessionBody {
                    target_token: token,
                    session_id: Some("target-session".into()),
                    required_capabilities: Vec::new(),
                    session_open_request: None,
                    session_open_request_sha256: None,
                },
            }))
        };
        clock.0.store(base + SESSION_IDLE_MS - 1, Ordering::SeqCst);
        assert_eq!(
            match_only()
                .expect_err("tombstone is stale before its TTL")
                .close_reason,
            CloseReason::Stale
        );
        clock.0.store(base + SESSION_IDLE_MS, Ordering::SeqCst);
        assert_eq!(
            match_only()
                .expect_err("tombstone prunes at the exact TTL")
                .close_reason,
            CloseReason::BindingMismatch
        );
    }

    #[test]
    fn idle_expired_session_is_stale_for_exchange_and_close() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let base = now().expect("system clock");
        let token = [27; 32];
        let raw = Arc::new(SessionRaw {
            writes: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            fail_reads: false,
        });
        let binding = SessionBinding {
            lease_id: [28; 16],
            process_generation: 8,
            listener_epoch: 4,
            nk_handshake_hash: [29; 32],
        };
        let (crypto, _) = real_session_connections(binding.clone());
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.lease_id = binding.lease_id;
            lease.generation = binding.process_generation;
            lease.epoch = binding.listener_epoch;
            lease.nk_hash = binding.nk_handshake_hash;
            lease.created = base;
            lease.last_client_activity = base;
            lease.last_heartbeat_attempt = base + 1;
            lease.last_heartbeat_success = base;
            lease.references.insert(
                token,
                Reference {
                    state: RefState::Consumed,
                    issued: base,
                    expires: base + READY_REFERENCE_TTL_MS,
                    session_slot: Some(1),
                    session_id: Some("idle-session".into()),
                },
            );
            lease.sessions.insert(
                1,
                Session {
                    id: Some("idle-session".into()),
                    raw: Arc::clone(&raw) as Arc<dyn RawDuplex>,
                    crypto: Some(crypto),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: b"opened".to_vec(),
                    response_digest: Sha256::digest(b"opened").into(),
                    handoff: HandoffState::HandoffPossibleOrConfirmed,
                    last: base,
                },
            );
        }
        let expired = base + SESSION_IDLE_MS;
        clock.0.store(expired, Ordering::SeqCst);
        broker.maintain().expect("session idle expiry succeeds");

        let exchange = broker
            .handle(ControlRequest::Exchange(Request {
                request_id: [30; 16],
                deadline_unix_ms: expired + 5_000,
                body: crate::ExchangeBody {
                    target_token: token,
                    session_id: "idle-session".into(),
                    process_generation: binding.process_generation,
                    listener_epoch: binding.listener_epoch,
                    message: b"request".to_vec(),
                    message_sha256: Sha256::digest(b"request").into(),
                    side_effect: crate::SideEffect::ReadOnly,
                },
            }))
            .expect_err("idle-expired exchange remains stale");
        assert_eq!(exchange.kind, ErrorKind::SessionExpired);
        assert_eq!(exchange.close_reason, CloseReason::Stale);

        let close = broker
            .handle(ControlRequest::CloseSession(Request {
                request_id: [31; 16],
                deadline_unix_ms: expired + 5_000,
                body: crate::CloseSessionBody {
                    target_token: token,
                    session_id: "idle-session".into(),
                    process_generation: binding.process_generation,
                    listener_epoch: binding.listener_epoch,
                    reason: CloseReason::Normal,
                },
            }))
            .expect_err("idle-expired close remains stale");
        assert_eq!(close.kind, ErrorKind::SessionExpired);
        assert_eq!(close.close_reason, CloseReason::Stale);
        assert!(raw.cancels.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn close_session_terminal_boundaries_seal_the_lease_before_session_io() {
        for terminal_offset in [LEASE_IDLE_MS, LEASE_ABSOLUTE_MS] {
            let cleanup_calls = Arc::new(AtomicUsize::new(0));
            let (broker, _, clock) =
                terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
            let base = now().expect("system clock");
            let current = base + terminal_offset;
            let token = [41; 32];
            let sibling_token = [42; 32];
            let session_raw = Arc::new(SessionRaw {
                writes: Mutex::new(Vec::new()),
                cancels: AtomicUsize::new(0),
                fail_reads: false,
            });
            let sibling_raw = Arc::new(SessionRaw {
                writes: Mutex::new(Vec::new()),
                cancels: AtomicUsize::new(0),
                fail_reads: false,
            });
            let control_raw = Arc::new(SessionRaw {
                writes: Mutex::new(Vec::new()),
                cancels: AtomicUsize::new(0),
                fail_reads: false,
            });
            let binding = SessionBinding {
                lease_id: [43; 16],
                process_generation: 13,
                listener_epoch: 5,
                nk_handshake_hash: [44; 32],
            };
            let (crypto, _) = real_session_connections(binding.clone());
            let (sibling_crypto, _) = real_session_connections(binding.clone());
            {
                let mut core = broker.inner.lock().expect("test core");
                let lease = core.leases[0].as_mut().expect("ready lease");
                lease.lease_id = binding.lease_id;
                lease.generation = binding.process_generation;
                lease.epoch = binding.listener_epoch;
                lease.nk_hash = binding.nk_handshake_hash;
                lease.created = base;
                lease.last_client_activity = if terminal_offset == LEASE_IDLE_MS {
                    base
                } else {
                    current - 1
                };
                lease.control = Some(Arc::clone(&control_raw) as Arc<dyn RawDuplex>);
                lease.references.insert(
                    token,
                    Reference {
                        state: RefState::Consumed,
                        issued: base,
                        expires: current + 1,
                        session_slot: Some(1),
                        session_id: Some("session".into()),
                    },
                );
                lease.references.insert(
                    sibling_token,
                    Reference {
                        state: RefState::Consumed,
                        issued: base,
                        expires: current + 1,
                        session_slot: Some(2),
                        session_id: Some("sibling".into()),
                    },
                );
                lease.sessions.insert(
                    1,
                    Session {
                        id: Some("session".into()),
                        raw: Arc::clone(&session_raw) as Arc<dyn RawDuplex>,
                        crypto: Some(crypto),
                        reader: Some(raw_transport::RawFrameReader::new()),
                        response: b"opened".to_vec(),
                        response_digest: Sha256::digest(b"opened").into(),
                        handoff: HandoffState::HandoffPossibleOrConfirmed,
                        last: base,
                    },
                );
                lease.sessions.insert(
                    2,
                    Session {
                        id: Some("sibling".into()),
                        raw: Arc::clone(&sibling_raw) as Arc<dyn RawDuplex>,
                        crypto: Some(sibling_crypto),
                        reader: Some(raw_transport::RawFrameReader::new()),
                        response: b"opened".to_vec(),
                        response_digest: Sha256::digest(b"opened").into(),
                        handoff: HandoffState::HandoffPossibleOrConfirmed,
                        last: base,
                    },
                );
            }
            clock.0.store(current, Ordering::SeqCst);

            let error = broker
                .handle(ControlRequest::CloseSession(Request {
                    request_id: [45; 16],
                    deadline_unix_ms: current + 1,
                    body: crate::CloseSessionBody {
                        target_token: token,
                        session_id: "session".into(),
                        process_generation: binding.process_generation,
                        listener_epoch: binding.listener_epoch,
                        reason: CloseReason::Normal,
                    },
                }))
                .expect_err("terminal lease wins before session close I/O");

            assert_eq!(error.close_reason, CloseReason::Stale);
            assert!(
                session_raw
                    .writes
                    .lock()
                    .expect("session writes")
                    .is_empty()
            );
            assert!(
                sibling_raw
                    .writes
                    .lock()
                    .expect("sibling writes")
                    .is_empty()
            );
            assert!(
                control_raw
                    .writes
                    .lock()
                    .expect("control writes")
                    .is_empty()
            );
            assert_eq!(session_raw.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(sibling_raw.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(control_raw.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
            assert!(
                broker
                    .inner
                    .lock()
                    .expect("test core")
                    .leases
                    .iter()
                    .all(Option::is_none)
            );
        }
    }

    #[test]
    fn close_session_rechecks_terminal_state_after_raw_io() {
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::clone(&cleanup_calls))));
        let base = now().expect("system clock");
        let terminal = base + LEASE_IDLE_MS;
        let token = [51; 32];
        let sibling_token = [52; 32];
        let session_raw = Arc::new(ClockAdvancingRaw {
            writes: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            clock: Arc::clone(&clock),
            advance_to: terminal,
        });
        let sibling_raw = Arc::new(SessionRaw {
            writes: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            fail_reads: false,
        });
        let control_raw = Arc::new(SessionRaw {
            writes: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            fail_reads: false,
        });
        let binding = SessionBinding {
            lease_id: [53; 16],
            process_generation: 17,
            listener_epoch: 6,
            nk_handshake_hash: [54; 32],
        };
        let (crypto, _) = real_session_connections(binding.clone());
        let (sibling_crypto, _) = real_session_connections(binding.clone());
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.lease_id = binding.lease_id;
            lease.generation = binding.process_generation;
            lease.epoch = binding.listener_epoch;
            lease.nk_hash = binding.nk_handshake_hash;
            lease.created = base;
            lease.last_client_activity = base;
            lease.control = Some(Arc::clone(&control_raw) as Arc<dyn RawDuplex>);
            lease.references.insert(
                token,
                Reference {
                    state: RefState::Consumed,
                    issued: base,
                    expires: terminal + 1,
                    session_slot: Some(1),
                    session_id: Some("session".into()),
                },
            );
            lease.references.insert(
                sibling_token,
                Reference {
                    state: RefState::Consumed,
                    issued: base,
                    expires: terminal + 1,
                    session_slot: Some(2),
                    session_id: Some("sibling".into()),
                },
            );
            lease.sessions.insert(
                1,
                Session {
                    id: Some("session".into()),
                    raw: Arc::clone(&session_raw) as Arc<dyn RawDuplex>,
                    crypto: Some(crypto),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: b"opened".to_vec(),
                    response_digest: Sha256::digest(b"opened").into(),
                    handoff: HandoffState::HandoffPossibleOrConfirmed,
                    last: base,
                },
            );
            lease.sessions.insert(
                2,
                Session {
                    id: Some("sibling".into()),
                    raw: Arc::clone(&sibling_raw) as Arc<dyn RawDuplex>,
                    crypto: Some(sibling_crypto),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: b"opened".to_vec(),
                    response_digest: Sha256::digest(b"opened").into(),
                    handoff: HandoffState::HandoffPossibleOrConfirmed,
                    last: base,
                },
            );
        }
        clock.0.store(terminal - 1, Ordering::SeqCst);

        let error = broker
            .handle(ControlRequest::CloseSession(Request {
                request_id: [55; 16],
                deadline_unix_ms: terminal + 1,
                body: crate::CloseSessionBody {
                    target_token: token,
                    session_id: "session".into(),
                    process_generation: binding.process_generation,
                    listener_epoch: binding.listener_epoch,
                    reason: CloseReason::Normal,
                },
            }))
            .expect_err("post-I/O terminal observation wins over session close success");

        assert_eq!(error.close_reason, CloseReason::Stale);
        assert_eq!(session_raw.writes.load(Ordering::SeqCst), 1);
        assert!(session_raw.cancels.load(Ordering::SeqCst) >= 1);
        assert_eq!(sibling_raw.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(control_raw.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);
        assert!(
            broker
                .inner
                .lock()
                .expect("test core")
                .leases
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn failed_exchanges_release_only_their_session_slots_and_remain_bounded() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let base = now().expect("system clock");
        let binding = SessionBinding {
            lease_id: [31; 16],
            process_generation: 11,
            listener_epoch: 4,
            nk_handshake_hash: [32; 32],
        };
        let sibling_token = [200; 32];
        let sibling_raw = Arc::new(SessionRaw {
            writes: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            fail_reads: false,
        });
        let (sibling_crypto, mut sibling_target) = real_session_connections(binding.clone());
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.lease_id = binding.lease_id;
            lease.generation = binding.process_generation;
            lease.epoch = binding.listener_epoch;
            lease.nk_hash = binding.nk_handshake_hash;
            lease.created = base;
            lease.last_client_activity = base;
            lease.references.insert(
                sibling_token,
                Reference {
                    state: RefState::Consumed,
                    issued: base,
                    expires: base + READY_REFERENCE_TTL_MS,
                    session_slot: Some(100),
                    session_id: None,
                },
            );
            lease.sessions.insert(
                100,
                Session {
                    id: None,
                    raw: Arc::clone(&sibling_raw) as Arc<dyn RawDuplex>,
                    crypto: Some(sibling_crypto),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: b"opened".to_vec(),
                    response_digest: Sha256::digest(b"opened").into(),
                    handoff: HandoffState::HandoffPossibleOrConfirmed,
                    last: base,
                },
            );
        }
        clock.0.store(base, Ordering::SeqCst);

        for slot in 1_u64..=8 {
            let token = [slot as u8; 32];
            let raw = Arc::new(SessionRaw {
                writes: Mutex::new(Vec::new()),
                cancels: AtomicUsize::new(0),
                fail_reads: true,
            });
            let (crypto, _) = real_session_connections(binding.clone());
            {
                let mut core = broker.inner.lock().expect("test core");
                let lease = core.leases[0].as_mut().expect("ready lease");
                lease.references.insert(
                    token,
                    Reference {
                        state: RefState::Consumed,
                        issued: base,
                        expires: base + READY_REFERENCE_TTL_MS,
                        session_slot: Some(slot),
                        session_id: None,
                    },
                );
                lease.sessions.insert(
                    slot,
                    Session {
                        id: None,
                        raw: Arc::clone(&raw) as Arc<dyn RawDuplex>,
                        crypto: Some(crypto),
                        reader: Some(raw_transport::RawFrameReader::new()),
                        response: b"opened".to_vec(),
                        response_digest: Sha256::digest(b"opened").into(),
                        handoff: HandoffState::HandoffPossibleOrConfirmed,
                        last: base,
                    },
                );
            }
            let session_id = format!("failed-{slot}");
            let error = broker
                .handle(ControlRequest::Exchange(Request {
                    request_id: [slot as u8; 16],
                    deadline_unix_ms: base + 5_000,
                    body: crate::ExchangeBody {
                        target_token: token,
                        session_id: session_id.clone(),
                        process_generation: binding.process_generation,
                        listener_epoch: binding.listener_epoch,
                        message: b"request".to_vec(),
                        message_sha256: Sha256::digest(b"request").into(),
                        side_effect: crate::SideEffect::ReadOnly,
                    },
                }))
                .expect_err("raw/C1 failure terminates only the current session");
            assert_eq!(error.close_reason, CloseReason::InternalError);
            assert_eq!(error.handoff, HandoffState::HandoffPossibleOrConfirmed);
            assert_eq!(raw.cancels.load(Ordering::SeqCst), 1);
            let core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_ref().expect("ready lease");
            assert_eq!(lease.sessions.len(), 1, "failed slots never accumulate");
            assert!(lease.sessions.contains_key(&100), "sibling remains live");
            let reference = lease.references.get(&token).expect("failure tombstone");
            assert_eq!(reference.session_id.as_deref(), Some(session_id.as_str()));
            assert_eq!(reference.expires, base + SESSION_IDLE_MS);
            drop(core);

            let repeat = |requested_session_id: &str| {
                broker.handle(ControlRequest::Exchange(Request {
                    request_id: [slot as u8 + 8; 16],
                    deadline_unix_ms: base + 5_000,
                    body: crate::ExchangeBody {
                        target_token: token,
                        session_id: requested_session_id.into(),
                        process_generation: binding.process_generation,
                        listener_epoch: binding.listener_epoch,
                        message: b"request".to_vec(),
                        message_sha256: Sha256::digest(b"request").into(),
                        side_effect: crate::SideEffect::ReadOnly,
                    },
                }))
            };
            let stale = repeat(&session_id).expect_err("the failed session remains stale");
            assert_eq!(stale.kind, ErrorKind::SessionExpired);
            assert_eq!(stale.close_reason, CloseReason::Stale);
            let mismatch = repeat("different-session")
                .expect_err("a different session id does not select the tombstone");
            assert_eq!(mismatch.kind, ErrorKind::TargetSelectionRequired);
            assert_eq!(mismatch.close_reason, CloseReason::BindingMismatch);
        }

        assert!(matches!(
            broker
                .handle(ControlRequest::CloseSession(Request {
                    request_id: [201; 16],
                    deadline_unix_ms: base + 5_000,
                    body: crate::CloseSessionBody {
                        target_token: sibling_token,
                        session_id: "sibling".into(),
                        process_generation: binding.process_generation,
                        listener_epoch: binding.listener_epoch,
                        reason: CloseReason::Normal,
                    },
                }))
                .expect("sibling remains usable after repeated failures"),
            ControlSuccess::Closed(_)
        ));
        let sibling_writes = sibling_raw.writes.lock().expect("sibling writes");
        assert_eq!(sibling_writes.len(), 1);
        assert_eq!(
            sibling_target
                .read_close(&sibling_writes[0])
                .expect("authenticated sibling close"),
            (
                CloseReason::Normal,
                HandoffState::HandoffPossibleOrConfirmed
            )
        );
    }

    #[test]
    fn terminal_lease_renews_bound_session_tombstone_before_reaping() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let token = [11; 32];
        {
            let mut core = broker.inner.lock().expect("test core");
            core.leases[0]
                .as_mut()
                .expect("ready lease")
                .references
                .insert(
                    token,
                    Reference {
                        state: RefState::Consumed,
                        issued: 1,
                        expires: 2,
                        session_slot: Some(1),
                        session_id: Some("session-a".into()),
                    },
                );
        }
        broker
            .handle(ControlRequest::CloseLease(Request {
                request_id: [12; 16],
                deadline_unix_ms: 2,
                body: crate::CloseLeaseBody {
                    target_token: token,
                    reason: CloseReason::Stale,
                },
            }))
            .expect("terminal lease closes");
        let request = || {
            ControlRequest::OpenSession(Request {
                request_id: [13; 16],
                deadline_unix_ms: 30_002,
                body: crate::OpenSessionBody {
                    target_token: token,
                    session_id: Some("session-a".into()),
                    required_capabilities: Vec::new(),
                    session_open_request: None,
                    session_open_request_sha256: None,
                },
            })
        };
        clock.0.store(2, Ordering::SeqCst);
        assert_eq!(
            broker
                .handle(request())
                .expect_err("bound session remains stale after the ReadyRef TTL")
                .close_reason,
            CloseReason::Stale
        );
        clock.0.store(SESSION_IDLE_MS + 1, Ordering::SeqCst);
        assert_eq!(
            broker
                .handle(request())
                .expect_err("terminal tombstone prunes at its new exact TTL")
                .close_reason,
            CloseReason::BindingMismatch
        );
    }

    #[test]
    fn absolute_lease_cap_rejects_new_open_before_connector_io() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let token = [12; 32];
        let connections = Arc::new(AtomicUsize::new(0));
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.connector = Some(Arc::new(CountingConnector(Arc::clone(&connections))));
            lease.references.insert(
                token,
                Reference {
                    state: RefState::Minted,
                    issued: 0,
                    expires: LEASE_ABSOLUTE_MS + 1,
                    session_slot: None,
                    session_id: None,
                },
            );
        }
        clock.0.store(LEASE_ABSOLUTE_MS, Ordering::SeqCst);
        let error = broker
            .handle(ControlRequest::OpenSession(Request {
                request_id: [14; 16],
                deadline_unix_ms: LEASE_ABSOLUTE_MS + 1,
                body: crate::OpenSessionBody {
                    target_token: token,
                    session_id: None,
                    required_capabilities: Vec::new(),
                    session_open_request: Some(b"open".to_vec()),
                    session_open_request_sha256: Some(Sha256::digest(b"open").into()),
                },
            }))
            .expect_err("absolute cap seals before connector I/O");
        assert_eq!(error.close_reason, CloseReason::Stale);
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn absolute_lease_cap_rejects_exchange_before_raw_io() {
        let (broker, _, clock) =
            terminal_test_broker(Box::new(CountCleanup(Arc::new(AtomicUsize::new(0)))));
        let token = [13; 32];
        let io = Arc::new(AtomicUsize::new(0));
        {
            let mut core = broker.inner.lock().expect("test core");
            let lease = core.leases[0].as_mut().expect("ready lease");
            lease.generation = 1;
            lease.epoch = 1;
            lease.references.insert(
                token,
                Reference {
                    state: RefState::Consumed,
                    issued: 0,
                    expires: LEASE_ABSOLUTE_MS + 1,
                    session_slot: Some(1),
                    session_id: Some("session-a".into()),
                },
            );
            let pbs = ProcessBootstrapSecret::generate().expect("test pbs");
            lease.sessions.insert(
                1,
                Session {
                    id: Some("session-a".into()),
                    raw: Arc::new(CountingRaw(Arc::clone(&io))),
                    crypto: Some(
                        BrokerSession::new(
                            SessionBinding {
                                lease_id: lease.lease_id,
                                process_generation: lease.generation,
                                listener_epoch: lease.epoch,
                                nk_handshake_hash: lease.nk_hash,
                            },
                            &pbs,
                        )
                        .expect("test session"),
                    ),
                    reader: Some(raw_transport::RawFrameReader::new()),
                    response: Vec::new(),
                    response_digest: [0; 32],
                    handoff: HandoffState::NotHandedOff,
                    last: 0,
                },
            );
        }
        clock.0.store(LEASE_ABSOLUTE_MS, Ordering::SeqCst);
        let error = broker
            .handle(ControlRequest::Exchange(Request {
                request_id: [15; 16],
                deadline_unix_ms: LEASE_ABSOLUTE_MS + 1,
                body: crate::ExchangeBody {
                    target_token: token,
                    session_id: "session-a".into(),
                    process_generation: 1,
                    listener_epoch: 1,
                    message: b"request".to_vec(),
                    message_sha256: Sha256::digest(b"request").into(),
                    side_effect: crate::SideEffect::ReadOnly,
                },
            }))
            .expect_err("absolute cap seals before raw exchange I/O");
        assert_eq!(error.close_reason, CloseReason::Stale);
        assert_eq!(io.load(Ordering::SeqCst), 0);
    }
}
