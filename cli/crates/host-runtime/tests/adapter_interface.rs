use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, CleanupReceipt, LaunchEndpoint, LaunchedTargetIo,
    PendingLaunch, PlatformFailure, PlatformFailureKind, PlatformTargetAdapter,
    PublicLaunchDescriptor, RawConnector, RawDuplex, TargetSelection,
};
use apppilotkit_host_runtime::{
    CloseLeaseBody, CloseReason, CloseSessionBody, ControlRequest, ControlSuccess, ExchangeBody,
    HandoffState, OpenSessionBody, Platform, PrepareBody, Request, SessionBroker, SideEffect,
};
use apppilotkit_transport_crypto_core::{
    BootstrapBinding, OuterFrameDecoder, ProcessBootstrapSecret, SessionBinding, TargetBootstrap,
    TargetSession,
};
use minicbor::{Decoder, Encoder};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

fn require<T>(result: Result<T, PlatformFailure>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("test carrier is valid"),
    }
}

struct Channel {
    bytes: Mutex<VecDeque<u8>>,
    wake: Condvar,
}
struct MemoryRaw {
    incoming: Arc<Channel>,
    outgoing: Arc<Channel>,
    cancelled: AtomicBool,
    max_write: usize,
}
impl RawDuplex for MemoryRaw {
    fn read(&self, output: &mut [u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
        let mut bytes = self.incoming.bytes.lock().expect("raw input");
        while bytes.is_empty() && !self.cancelled.load(Ordering::Acquire) {
            bytes = self.incoming.wake.wait(bytes).expect("raw wait");
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = output.len().min(bytes.len()).min(7);
        for slot in &mut output[..count] {
            *slot = bytes.pop_front().expect("bounded input");
        }
        Ok(count)
    }
    fn write(&self, input: &[u8], _: AbsoluteDeadline) -> Result<usize, PlatformFailure> {
        let count = input.len().min(self.max_write);
        self.outgoing
            .bytes
            .lock()
            .expect("raw output")
            .extend(&input[..count]);
        self.outgoing.wake.notify_all();
        Ok(count)
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.incoming.wake.notify_all();
    }
}
fn pair() -> (Arc<dyn RawDuplex>, Arc<dyn RawDuplex>) {
    let a_to_b = Arc::new(Channel {
        bytes: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
    });
    let b_to_a = Arc::new(Channel {
        bytes: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
    });
    let a = MemoryRaw {
        incoming: Arc::clone(&b_to_a),
        outgoing: Arc::clone(&a_to_b),
        cancelled: AtomicBool::new(false),
        max_write: 5,
    };
    let b = MemoryRaw {
        incoming: a_to_b,
        outgoing: b_to_a,
        cancelled: AtomicBool::new(false),
        max_write: 3,
    };
    (Arc::new(a), Arc::new(b))
}
struct FrameReader {
    decoder: OuterFrameDecoder,
    ready: VecDeque<Vec<u8>>,
}
impl FrameReader {
    fn new() -> Self {
        Self {
            decoder: OuterFrameDecoder::new(),
            ready: VecDeque::new(),
        }
    }
    fn read(&mut self, raw: &dyn RawDuplex, deadline: AbsoluteDeadline) -> Option<Vec<u8>> {
        if let Some(frame) = self.ready.pop_front() {
            return Some(frame);
        }
        let mut chunk = [0; 4096];
        loop {
            let n = raw.read(&mut chunk, deadline).ok()?;
            if n == 0 {
                return None;
            }
            self.ready.extend(self.decoder.push(&chunk[..n]).ok()?);
            if let Some(frame) = self.ready.pop_front() {
                return Some(frame);
            }
        }
    }
}
fn write_all(raw: &dyn RawDuplex, mut bytes: &[u8], deadline: AbsoluteDeadline) {
    while !bytes.is_empty() {
        let n = match raw.write(bytes, deadline) {
            Ok(value) => value,
            Err(_) => panic!("raw write"),
        };
        assert!(n > 0 && n <= bytes.len());
        bytes = &bytes[n..];
    }
}

#[derive(Clone)]
struct DescriptorFacts {
    binding: BootstrapBinding,
    broker_public: [u8; 32],
}
fn bytes<const N: usize>(decoder: &mut Decoder<'_>) -> [u8; N] {
    decoder
        .bytes()
        .expect("descriptor bytes")
        .try_into()
        .expect("descriptor length")
}
fn decode_descriptor(canonical: &[u8]) -> DescriptorFacts {
    let mut d = Decoder::new(canonical);
    assert_eq!(d.map().expect("descriptor map"), Some(9));
    assert_eq!(d.u8().unwrap(), 0);
    assert_eq!(d.u8().unwrap(), 1);
    assert_eq!(d.u8().unwrap(), 1);
    assert_eq!(d.u8().unwrap(), 0);
    assert_eq!(d.u8().unwrap(), 2);
    let lease_id = bytes::<16>(&mut d);
    assert_eq!(d.u8().unwrap(), 3);
    let target_nonce = bytes::<32>(&mut d);
    assert_eq!(d.u8().unwrap(), 4);
    let app_artifact_digest = bytes::<32>(&mut d);
    assert_eq!(d.u8().unwrap(), 5);
    let broker_public = bytes::<32>(&mut d);
    assert_eq!(d.u8().unwrap(), 6);
    assert_eq!(d.map().unwrap(), Some(2));
    assert_eq!(d.u8().unwrap(), 0);
    assert_eq!(d.str().unwrap(), "127.0.0.1");
    assert_eq!(d.u8().unwrap(), 1);
    assert!(d.u16().unwrap() > 0);
    assert_eq!(d.u8().unwrap(), 7);
    let expiry_ms = d.u64().unwrap();
    assert!(expiry_ms > 0);
    assert_eq!(d.u8().unwrap(), 8);
    let target_reference_digest = bytes::<32>(&mut d);
    assert_eq!(d.position(), canonical.len());
    DescriptorFacts {
        binding: BootstrapBinding {
            target_reference_digest,
            lease_id,
            target_nonce,
            app_artifact_digest,
            expiry_ms,
        },
        broker_public,
    }
}
fn bootstrap_hash(
    binding: &BootstrapBinding,
    broker_public: &[u8; 32],
    m1: &[u8],
    m2: &[u8],
) -> [u8; 32] {
    let mut p = Vec::new();
    Encoder::new(&mut p)
        .array(10)
        .unwrap()
        .str("apppilotkit.transport")
        .unwrap()
        .u8(1)
        .unwrap()
        .str("bootstrap")
        .unwrap()
        .u8(0)
        .unwrap()
        .u8(1)
        .unwrap()
        .bytes(&binding.target_reference_digest)
        .unwrap()
        .bytes(&binding.lease_id)
        .unwrap()
        .bytes(&binding.target_nonce)
        .unwrap()
        .bytes(&binding.app_artifact_digest)
        .unwrap()
        .u64(binding.expiry_ms)
        .unwrap();
    let protocol = b"Noise_NK_25519_ChaChaPoly_SHA256";
    let mut initial = [0_u8; 32];
    initial[..protocol.len()].copy_from_slice(protocol);
    let mut hash = initial.to_vec();
    for data in [
        &p[..],
        &broker_public[..],
        &m1[2..34],
        &m1[34..],
        &m2[2..34],
        &m2[34..],
    ] {
        let mut input = Vec::with_capacity(32 + data.len());
        input.extend_from_slice(&hash);
        input.extend_from_slice(data);
        hash = Sha256::digest(input).to_vec();
    }
    hash.try_into().unwrap()
}

struct TargetState {
    ready: Mutex<Option<(Arc<ProcessBootstrapSecret>, [u8; 32])>>,
    wake: Condvar,
}
struct FakeAdapter {
    cleanup_calls: Arc<AtomicUsize>,
    launches: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    wrong_binding: bool,
    wrong_endpoint: bool,
    abort_fails: bool,
    launch_failure: Option<PlatformFailureKind>,
    launch_gate: Option<Arc<std::sync::Barrier>>,
    launch_started: Option<Arc<std::sync::Barrier>>,
    launch_release: Option<Arc<std::sync::Barrier>>,
    cleanup_fails: bool,
    cleanup_started: Option<Arc<std::sync::Barrier>>,
    cleanup_release: Option<Arc<std::sync::Barrier>>,
}
struct FakePending {
    endpoint: LaunchEndpoint,
    cleanup_calls: Arc<AtomicUsize>,
    launches: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    wrong_binding: bool,
    abort_fails: bool,
    launch_failure: Option<PlatformFailureKind>,
    launch_gate: Option<Arc<std::sync::Barrier>>,
    launch_started: Option<Arc<std::sync::Barrier>>,
    launch_release: Option<Arc<std::sync::Barrier>>,
    cleanup_fails: bool,
    cleanup_started: Option<Arc<std::sync::Barrier>>,
    cleanup_release: Option<Arc<std::sync::Barrier>>,
}
struct FakeConnector {
    facts: DescriptorFacts,
    state: Arc<TargetState>,
    connections: Arc<AtomicUsize>,
}
struct FakeCleanup {
    cleanup_calls: Arc<AtomicUsize>,
    fails: bool,
    started: Option<Arc<std::sync::Barrier>>,
    release: Option<Arc<std::sync::Barrier>>,
}
impl PlatformTargetAdapter for FakeAdapter {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        _: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch> {
        assert_eq!(selection.platform(), Platform::IosSimulator);
        assert_eq!(selection.device_selector(), "device-1");
        assert_eq!(selection.app_id(), "example.app");
        assert_eq!(selection.artifact_path(), "/tmp/example.app");
        Box::new(FakePending {
            endpoint: if self.wrong_endpoint {
                require(LaunchEndpoint::android_local_abstract("a".repeat(32)))
            } else {
                require(LaunchEndpoint::ios_loopback(49_152))
            },
            cleanup_calls: Arc::clone(&self.cleanup_calls),
            launches: Arc::clone(&self.launches),
            connections: Arc::clone(&self.connections),
            wrong_binding: self.wrong_binding,
            abort_fails: self.abort_fails,
            launch_failure: self.launch_failure,
            launch_gate: self.launch_gate.as_ref().map(Arc::clone),
            launch_started: self.launch_started.as_ref().map(Arc::clone),
            launch_release: self.launch_release.as_ref().map(Arc::clone),
            cleanup_fails: self.cleanup_fails,
            cleanup_started: self.cleanup_started.as_ref().map(Arc::clone),
            cleanup_release: self.cleanup_release.as_ref().map(Arc::clone),
        })
    }
}
impl PendingLaunch for FakePending {
    fn endpoint(&self) -> &LaunchEndpoint {
        &self.endpoint
    }
    fn launch(
        self: Box<Self>,
        descriptor: PublicLaunchDescriptor,
        _: Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<LaunchedTargetIo, PlatformFailure> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = &self.launch_started {
            started.wait();
        }
        if let Some(release) = &self.launch_release {
            release.wait();
        }
        if let Some(gate) = &self.launch_gate {
            gate.wait();
        }
        if let Some(failure) = self.launch_failure {
            return Err(PlatformFailure::new(failure));
        }
        let mut facts = decode_descriptor(descriptor.canonical_bytes());
        if self.wrong_binding {
            facts.binding.target_nonce[0] ^= 1;
        }
        let state = Arc::new(TargetState {
            ready: Mutex::new(None),
            wake: Condvar::new(),
        });
        let (broker, target) = pair();
        spawn_bootstrap_peer(target, facts.clone(), Arc::clone(&state), deadline);
        Ok(LaunchedTargetIo::new(
            broker,
            Arc::new(FakeConnector {
                facts,
                state,
                connections: Arc::clone(&self.connections),
            }),
            Box::new(FakeCleanup {
                cleanup_calls: self.cleanup_calls,
                fails: self.cleanup_fails,
                started: self.cleanup_started,
                release: self.cleanup_release,
            }),
        ))
    }

    fn abort(self: Box<Self>, _: Cancellation, _: AbsoluteDeadline) -> Result<(), PlatformFailure> {
        self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        if self.abort_fails {
            Err(PlatformFailure::new(PlatformFailureKind::CleanupFailed))
        } else {
            Ok(())
        }
    }
}
fn spawn_bootstrap_peer(
    raw: Arc<dyn RawDuplex>,
    facts: DescriptorFacts,
    state: Arc<TargetState>,
    deadline: AbsoluteDeadline,
) {
    thread::spawn(move || {
        let mut target = match TargetBootstrap::new(facts.binding.clone(), facts.broker_public) {
            Ok(v) => v,
            Err(_) => return,
        };
        let m1 = match target.write_m1() {
            Ok(v) => v,
            Err(_) => return,
        };
        write_all(raw.as_ref(), &m1, deadline);
        let mut reader = FrameReader::new();
        let Some(m2) = reader.read(raw.as_ref(), deadline) else {
            return;
        };
        let nk_hash = bootstrap_hash(&facts.binding, &facts.broker_public, &m1, &m2);
        let (sender, pbs) = match target.read_m2(&m2, 7, 1) {
            Ok(v) => v,
            Err(_) => return,
        };
        let (ack, mut lease) = match sender.write_ack() {
            Ok(v) => v,
            Err(_) => return,
        };
        write_all(raw.as_ref(), &ack, deadline);
        let pbs = Arc::new(pbs);
        *state.ready.lock().unwrap() = Some((Arc::clone(&pbs), nk_hash));
        state.wake.notify_all();
        while let Some(frame) = reader.read(raw.as_ref(), deadline) {
            if let Ok(counter) = lease.read_heartbeat_request(&frame) {
                if let Ok(reply) = lease.write_heartbeat_reply(counter) {
                    write_all(raw.as_ref(), &reply, deadline);
                }
            } else {
                let _ = lease.read_close(&frame);
                break;
            }
        }
    });
}
impl RawConnector for FakeConnector {
    fn connect(
        &self,
        _: Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
        let (broker, target) = pair();
        let mut ready = self.state.ready.lock().unwrap();
        while ready.is_none() {
            ready = self.state.wake.wait(ready).unwrap();
        }
        let (pbs, nk_hash) = ready.as_ref().unwrap();
        let pbs = Arc::clone(pbs);
        let binding = SessionBinding {
            lease_id: self.facts.binding.lease_id,
            process_generation: 7,
            listener_epoch: 1,
            nk_handshake_hash: *nk_hash,
        };
        let label = format!("opened-{}", self.connections.load(Ordering::Relaxed) + 1).into_bytes();
        self.connections.fetch_add(1, Ordering::Relaxed);
        thread::spawn(move || session_peer(target, binding, pbs, label, deadline));
        Ok(broker)
    }
}
fn session_peer(
    raw: Arc<dyn RawDuplex>,
    binding: SessionBinding,
    pbs: Arc<ProcessBootstrapSecret>,
    label: Vec<u8>,
    deadline: AbsoluteDeadline,
) {
    let mut target = TargetSession::new(binding, pbs.as_ref()).unwrap();
    let mut reader = FrameReader::new();
    write_all(raw.as_ref(), &target.write_m1().unwrap(), deadline);
    let m2 = reader.read(raw.as_ref(), deadline).unwrap();
    target.read_m2(&m2).unwrap();
    write_all(raw.as_ref(), &target.write_finished().unwrap(), deadline);
    let finished = reader.read(raw.as_ref(), deadline).unwrap();
    target.read_finished(&finished).unwrap();
    assert_eq!(
        read_application(&mut target, &mut reader, raw.as_ref(), deadline),
        b"open"
    );
    for frame in target.write_application_response(&label).unwrap() {
        write_all(raw.as_ref(), &frame, deadline);
    }
    assert_eq!(
        read_application(&mut target, &mut reader, raw.as_ref(), deadline),
        b"request"
    );
    for frame in target.write_application_response(b"reply").unwrap() {
        write_all(raw.as_ref(), &frame, deadline);
    }
    let close = reader.read(raw.as_ref(), deadline).unwrap();
    let (reason, handoff) = target.read_close(&close).unwrap();
    assert_eq!(reason, CloseReason::PeerClosed);
    assert_eq!(handoff, HandoffState::HandoffPossibleOrConfirmed);
}
fn read_application(
    target: &mut TargetSession,
    reader: &mut FrameReader,
    raw: &dyn RawDuplex,
    deadline: AbsoluteDeadline,
) -> Vec<u8> {
    loop {
        let frame = reader.read(raw, deadline).unwrap();
        if let Some(payload) = target.read_application(&frame).unwrap() {
            return payload;
        }
    }
}
impl CleanupReceipt for FakeCleanup {
    fn cleanup(
        self: Box<Self>,
        _: Cancellation,
        _: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = &self.started {
            started.wait();
        }
        if let Some(release) = &self.release {
            release.wait();
        }
        if self.fails {
            Err(PlatformFailure::new(PlatformFailureKind::CleanupFailed))
        } else {
            Ok(())
        }
    }
}

fn deadline() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
        + 5_000
}
fn prepare_request(id: u8, digest: [u8; 32]) -> ControlRequest {
    ControlRequest::Prepare(Request {
        request_id: [id; 16],
        deadline_unix_ms: deadline(),
        body: PrepareBody {
            platform: Platform::IosSimulator,
            device_selector: "device-1".into(),
            app_id: "example.app".into(),
            app_artifact: "/tmp/example.app".into(),
            app_artifact_sha256: digest,
        },
    })
}

#[test]
fn session_broker_observes_real_crypto_through_external_adapter() {
    let cleanup = Arc::new(AtomicUsize::new(0));
    let links = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::clone(&cleanup),
        launches: Arc::new(AtomicUsize::new(0)),
        connections: Arc::clone(&links),
        wrong_binding: false,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let digest = [7; 32];
    let ready1 = match broker.handle(prepare_request(1, digest)).unwrap() {
        ControlSuccess::TargetReady(v) => v,
        _ => panic!("target ready"),
    };
    let ready2 = match broker.handle(prepare_request(2, digest)).unwrap() {
        ControlSuccess::TargetReady(v) => v,
        _ => panic!("target ready"),
    };
    assert_ne!(ready1.target_token, ready2.target_token);
    assert_eq!(adapter.launches.load(Ordering::SeqCst), 1);
    let open = |id, token| {
        broker
            .handle(ControlRequest::OpenSession(Request {
                request_id: [id; 16],
                deadline_unix_ms: deadline(),
                body: OpenSessionBody {
                    target_token: token,
                    session_id: None,
                    required_capabilities: Vec::new(),
                    session_open_request: Some(b"open".to_vec()),
                    session_open_request_sha256: Some(Sha256::digest(b"open").into()),
                },
            }))
            .unwrap()
    };
    assert!(matches!(
        open(3, ready1.target_token),
        ControlSuccess::SessionOpened(_)
    ));
    let second_redemption = broker
        .handle(ControlRequest::OpenSession(Request {
            request_id: [30; 16],
            deadline_unix_ms: deadline(),
            body: OpenSessionBody {
                target_token: ready1.target_token,
                session_id: None,
                required_capabilities: Vec::new(),
                session_open_request: Some(b"open".to_vec()),
                session_open_request_sha256: Some(Sha256::digest(b"open").into()),
            },
        }))
        .expect_err("a Ready Reference is single redemption");
    assert_eq!(second_redemption.close_reason, CloseReason::Stale);
    assert!(matches!(
        open(4, ready2.target_token),
        ControlSuccess::SessionOpened(_)
    ));
    let exchange = |id, ready: &apppilotkit_host_runtime::ReadyTarget, session_id: &str| {
        broker
            .handle(ControlRequest::Exchange(Request {
                request_id: [id; 16],
                deadline_unix_ms: deadline(),
                body: ExchangeBody {
                    target_token: ready.target_token,
                    session_id: session_id.into(),
                    process_generation: ready.process_generation,
                    listener_epoch: ready.listener_epoch,
                    message: b"request".to_vec(),
                    message_sha256: Sha256::digest(b"request").into(),
                    side_effect: SideEffect::ReadOnly,
                },
            }))
            .unwrap()
    };
    assert!(matches!(
        exchange(5, &ready1, "session-a"),
        ControlSuccess::ExchangeComplete(_)
    ));
    broker
        .handle(ControlRequest::CloseSession(Request {
            request_id: [6; 16],
            deadline_unix_ms: deadline(),
            body: CloseSessionBody {
                target_token: ready1.target_token,
                session_id: "session-a".into(),
                process_generation: ready1.process_generation,
                listener_epoch: ready1.listener_epoch,
                reason: CloseReason::PeerClosed,
            },
        }))
        .unwrap();
    let closed_match = broker
        .handle(ControlRequest::OpenSession(Request {
            request_id: [61; 16],
            deadline_unix_ms: deadline(),
            body: OpenSessionBody {
                target_token: ready1.target_token,
                session_id: Some("session-a".into()),
                required_capabilities: Vec::new(),
                session_open_request: None,
                session_open_request_sha256: None,
            },
        }))
        .expect_err("a matching closed session remains a bounded stale tombstone");
    assert_eq!(closed_match.close_reason, CloseReason::Stale);
    assert!(matches!(
        exchange(7, &ready2, "session-b"),
        ControlSuccess::ExchangeComplete(_)
    ));
    broker
        .handle(ControlRequest::CloseSession(Request {
            request_id: [8; 16],
            deadline_unix_ms: deadline(),
            body: CloseSessionBody {
                target_token: ready2.target_token,
                session_id: "session-b".into(),
                process_generation: ready2.process_generation,
                listener_epoch: ready2.listener_epoch,
                reason: CloseReason::PeerClosed,
            },
        }))
        .unwrap();
    assert_eq!(links.load(Ordering::SeqCst), 2);
    broker
        .handle(ControlRequest::CloseLease(Request {
            request_id: [9; 16],
            deadline_unix_ms: deadline(),
            body: CloseLeaseBody {
                target_token: ready2.target_token,
                reason: CloseReason::PeerClosed,
            },
        }))
        .unwrap();
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
    let retained_tombstone = broker
        .handle(ControlRequest::OpenSession(Request {
            request_id: [31; 16],
            deadline_unix_ms: deadline(),
            body: OpenSessionBody {
                target_token: ready2.target_token,
                session_id: None,
                required_capabilities: Vec::new(),
                session_open_request: Some(b"open".to_vec()),
                session_open_request_sha256: Some(Sha256::digest(b"open").into()),
            },
        }))
        .expect_err("closed lease keeps its unexpired token stale");
    assert_eq!(retained_tombstone.close_reason, CloseReason::Stale);
}

#[test]
fn prepare_rejects_same_selection_different_digest_before_second_launch() {
    let launches = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::new(AtomicUsize::new(0)),
        launches: Arc::clone(&launches),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();

    broker.handle(prepare_request(30, [3; 32])).unwrap();
    let conflict = broker
        .handle(prepare_request(31, [4; 32]))
        .expect_err("selection conflict must not replace or relaunch the owned lease");

    assert_eq!(conflict.close_reason, CloseReason::BindingMismatch);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn prepare_rejects_conflicting_digest_while_original_launch_is_preparing() {
    let launches = Arc::new(AtomicUsize::new(0));
    let launch_started = Arc::new(std::sync::Barrier::new(2));
    let launch_release = Arc::new(std::sync::Barrier::new(2));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::new(AtomicUsize::new(0)),
        launches: Arc::clone(&launches),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: Some(Arc::clone(&launch_started)),
        launch_release: Some(Arc::clone(&launch_release)),
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker =
        Arc::new(SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap());
    let preparing = Arc::clone(&broker);
    let first = thread::spawn(move || preparing.handle(prepare_request(32, [5; 32])));
    launch_started.wait();

    let conflict = broker
        .handle(prepare_request(33, [6; 32]))
        .expect_err("Preparing selection owns the platform before NK completes");
    assert_eq!(conflict.close_reason, CloseReason::BindingMismatch);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    launch_release.wait();
    assert!(matches!(
        first.join().expect("prepare joins"),
        Ok(ControlSuccess::TargetReady(_))
    ));
}

#[test]
fn prepare_rejects_conflicting_digest_while_original_lease_is_closing() {
    let launches = Arc::new(AtomicUsize::new(0));
    let cleanup_started = Arc::new(std::sync::Barrier::new(2));
    let cleanup_release = Arc::new(std::sync::Barrier::new(2));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::new(AtomicUsize::new(0)),
        launches: Arc::clone(&launches),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: Some(Arc::clone(&cleanup_started)),
        cleanup_release: Some(Arc::clone(&cleanup_release)),
    });
    let broker =
        Arc::new(SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap());
    let ready = match broker.handle(prepare_request(34, [7; 32])).unwrap() {
        ControlSuccess::TargetReady(ready) => ready,
        _ => panic!("target ready"),
    };
    let closing = Arc::clone(&broker);
    let close = thread::spawn(move || {
        closing.handle(ControlRequest::CloseLease(Request {
            request_id: [35; 16],
            deadline_unix_ms: deadline(),
            body: CloseLeaseBody {
                target_token: ready.target_token,
                reason: CloseReason::Normal,
            },
        }))
    });
    cleanup_started.wait();

    let conflict = broker
        .handle(prepare_request(36, [8; 32]))
        .expect_err("Closing selection remains occupied through adapter cleanup");
    assert_eq!(conflict.close_reason, CloseReason::BindingMismatch);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    cleanup_release.wait();
    assert!(matches!(
        close.join().expect("close joins"),
        Ok(ControlSuccess::Closed(_))
    ));
}

#[test]
fn wrong_binding_prepare_cleans_up_once() {
    let cleanup = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::clone(&cleanup),
        launches: Arc::new(AtomicUsize::new(0)),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: true,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let error = broker.handle(prepare_request(9, [8; 32])).unwrap_err();
    assert_eq!(error.close_reason, CloseReason::AuthenticationFailed);
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
}

#[test]
fn wrong_binding_prepare_preserves_authentication_failure_when_cleanup_fails() {
    let cleanup = Arc::new(AtomicUsize::new(0));
    let launches = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::clone(&cleanup),
        launches: Arc::clone(&launches),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: true,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: true,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let digest = [18; 32];

    let first = broker
        .handle(prepare_request(40, digest))
        .expect_err("the authenticated bootstrap failure remains the current result");
    let second = broker
        .handle(prepare_request(41, digest))
        .expect_err("failed cleanup permanently tombstones the selection");

    assert_eq!(first.close_reason, CloseReason::AuthenticationFailed);
    assert_eq!(first.handoff, HandoffState::NotHandedOff);
    assert_eq!(second.close_reason, CloseReason::CleanupFailed);
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn descriptor_endpoint_rejection_consumes_pending_launch_once() {
    let cleanup = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::clone(&cleanup),
        launches: Arc::new(AtomicUsize::new(0)),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: true,
        abort_fails: false,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let error = broker.handle(prepare_request(10, [9; 32])).unwrap_err();
    assert_eq!(error.close_reason, CloseReason::BindingMismatch);
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_pending_abort_leaves_a_cleanup_tombstone() {
    let cleanup = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::clone(&cleanup),
        launches: Arc::new(AtomicUsize::new(0)),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: true,
        abort_fails: true,
        launch_failure: None,
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let first = broker.handle(prepare_request(11, [10; 32])).unwrap_err();
    assert_eq!(first.close_reason, CloseReason::CleanupFailed);
    let second = broker.handle(prepare_request(12, [10; 32])).unwrap_err();
    assert_eq!(second.close_reason, CloseReason::CleanupFailed);
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
}

#[test]
fn cleanup_failed_launch_is_tombstoned_without_a_second_launch() {
    let cleanup = Arc::new(AtomicUsize::new(0));
    let launches = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: cleanup,
        launches: Arc::clone(&launches),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: Some(PlatformFailureKind::CleanupFailed),
        launch_gate: None,
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let first = broker.handle(prepare_request(13, [11; 32])).unwrap_err();
    assert_eq!(first.close_reason, CloseReason::CleanupFailed);
    let second = broker.handle(prepare_request(14, [11; 32])).unwrap_err();
    assert_eq!(second.close_reason, CloseReason::CleanupFailed);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn late_bootstrap_cleanup_failure_tombstones_the_prepare_key() {
    let launches = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(std::sync::Barrier::new(2));
    let adapter = Arc::new(FakeAdapter {
        cleanup_calls: Arc::new(AtomicUsize::new(0)),
        launches: Arc::clone(&launches),
        connections: Arc::new(AtomicUsize::new(0)),
        wrong_binding: false,
        wrong_endpoint: false,
        abort_fails: false,
        launch_failure: Some(PlatformFailureKind::CleanupFailed),
        launch_gate: Some(Arc::clone(&gate)),
        launch_started: None,
        launch_release: None,
        cleanup_fails: false,
        cleanup_started: None,
        cleanup_release: None,
    });
    let broker = SessionBroker::new(Arc::clone(&adapter) as _, Arc::clone(&adapter) as _).unwrap();
    let digest = [12; 32];
    let mut first = prepare_request(15, digest);
    if let ControlRequest::Prepare(request) = &mut first {
        request.deadline_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 50;
    }
    let timed_out = broker
        .handle(first)
        .expect_err("caller times out before launch completes");
    assert_eq!(timed_out.close_reason, CloseReason::Timeout);
    gate.wait();
    for _ in 0..10_000 {
        if launches.load(Ordering::SeqCst) != 0 {
            break;
        }
        thread::yield_now();
    }
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    let observe_by = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let second = broker
            .handle(prepare_request(16, digest))
            .expect_err("unresolved late attempt cannot relaunch");
        if second.close_reason == CloseReason::CleanupFailed {
            break;
        }
        assert!(
            matches!(
                second.close_reason,
                CloseReason::Timeout | CloseReason::Stale
            ),
            "only the unresolved attempt may precede its cleanup tombstone"
        );
        assert!(
            std::time::Instant::now() < observe_by,
            "late cleanup tombstone was not published within its watchdog"
        );
        thread::yield_now();
    }
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}
