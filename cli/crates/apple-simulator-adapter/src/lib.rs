//! Publish-disabled Apple Simulator raw transport adapter.
//!
//! This crate owns exact `simctl` launch/cleanup and loopback byte streams. It
//! deliberately does not own bootstrap cryptography, framing, Target proof,
//! process generations, listener epochs, Protocol sessions, or runtime handoff.

use apppilotkit_host_runtime::Platform;
use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, CleanupReceipt, LaunchEndpoint, LaunchedTargetIo,
    PendingLaunch, PlatformFailure, PlatformFailureKind, PlatformTargetAdapter,
    PublicLaunchDescriptor, RawConnector, RawDuplex, TargetSelection,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DESCRIPTOR_ENV: &str = "SIMCTL_CHILD_APPPILOTKIT_TRANSPORT_DESCRIPTOR";
const SIMCTL_CHILD_PREFIX: &str = "SIMCTL_CHILD_";
const TOOL_OUTPUT_CAP: usize = 1_048_576;
const LAUNCH_OUTPUT_CAP: usize = 65_536;
const DESCRIPTOR_CAP: usize = 8_192;
const CONNECT_SLICE: Duration = Duration::from_millis(25);
const REAP_POLL: Duration = Duration::from_millis(5);
const TERM_GRACE: Duration = Duration::from_millis(250);
const CLEANUP_BUDGET_MS: u64 = 2_000;

mod artifact;

/// Computes the canonical `ios-app-tree-v1` digest for one selected Simulator
/// app bundle.
///
/// This is a host-private preparation bridge. It intentionally exposes only
/// the existing scanner's digest and closed platform failure carrier; launch
/// ownership remains exclusively behind `PlatformTargetAdapter`.
pub fn inspect_ios_app_tree_digest(
    app_path: &Path,
    app_id: &str,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<[u8; 32], PlatformFailure> {
    Ok(artifact::inspect_bundle(app_path, app_id, cancellation, deadline)?.digest)
}

/// Exact-target Apple Simulator adapter backed by one explicit `xcrun` path.
pub struct AppleSimulatorAdapter {
    runner: Arc<dyn ToolRunner>,
    artifact_verifier: Arc<dyn ArtifactVerifier>,
    process_identity: Arc<dyn ProcessIdentityProbe>,
    loopback: Arc<dyn LoopbackConnector>,
}

impl AppleSimulatorAdapter {
    /// Creates an adapter. Relative tool paths are rejected when a launch is
    /// attempted so the process can never be selected through `PATH` lookup.
    pub fn new(xcrun: PathBuf) -> Self {
        Self {
            runner: Arc::new(ProcessToolRunner { xcrun }),
            artifact_verifier: Arc::new(D0ArtifactVerifier),
            process_identity: Arc::new(DarwinProcessIdentityProbe),
            loopback: Arc::new(SystemLoopbackConnector),
        }
    }
}

impl Default for AppleSimulatorAdapter {
    fn default() -> Self {
        Self::new(PathBuf::from("/usr/bin/xcrun"))
    }
}

trait ArtifactVerifier: Send + Sync {
    fn prepare(
        &self,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<PreparedLaunchArtifact, PlatformFailure>;
    fn assert_snapshot_unchanged(
        &self,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure>;
    fn verify_installed(
        &self,
        installed: &Path,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure>;
}

struct PreparedLaunchArtifact {
    snapshot_path: PathBuf,
    digest: [u8; 32],
    executable: String,
    production: Option<artifact::PreparedArtifact>,
}

struct D0ArtifactVerifier;

impl ArtifactVerifier for D0ArtifactVerifier {
    fn prepare(
        &self,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<PreparedLaunchArtifact, PlatformFailure> {
        let production = artifact::prepare_snapshot(
            Path::new(selection.artifact_path()),
            selection.app_id(),
            &selection.artifact_digest(),
            cancellation,
            deadline,
        )?;
        Ok(PreparedLaunchArtifact {
            snapshot_path: production.app_path().to_path_buf(),
            digest: production.identity.digest,
            executable: production.identity.executable.clone(),
            production: Some(production),
        })
    }

    fn assert_snapshot_unchanged(
        &self,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let production = prepared
            .production
            .as_ref()
            .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
        production.assert_unchanged(selection.app_id(), cancellation, deadline)
    }

    fn verify_installed(
        &self,
        installed: &Path,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let identity =
            artifact::inspect_bundle(installed, selection.app_id(), cancellation, deadline)?;
        if identity.digest != prepared.digest
            || identity.executable != prepared.executable
            || identity.build
                != prepared
                    .production
                    .as_ref()
                    .ok_or_else(|| failure(PlatformFailureKind::Internal))?
                    .identity
                    .build
        {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(())
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct TargetKey {
    udid: String,
    app_id: String,
}

impl TargetKey {
    fn from_selection(selection: &TargetSelection) -> Self {
        Self {
            udid: selection.device_selector().to_owned(),
            app_id: selection.app_id().to_owned(),
        }
    }
}

#[derive(Default)]
struct ReservationLedger {
    ports: HashSet<u16>,
    source_ports: HashMap<u16, usize>,
    targets: HashSet<TargetKey>,
}

fn reservation_ledger() -> &'static Mutex<ReservationLedger> {
    static LEDGER: OnceLock<Mutex<ReservationLedger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(ReservationLedger::default()))
}

struct AdapterReservation {
    port: u16,
    target: Option<TargetKey>,
    released: bool,
}

struct SourcePortReservation {
    port: u16,
}

struct ConnectedLoopback {
    stream: TcpStream,
    source_port: SourcePortReservation,
}

trait LoopbackConnector: Send + Sync {
    fn connect(
        &self,
        address: SocketAddr,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ConnectedLoopback, PlatformFailure>;
}

struct SystemLoopbackConnector;

impl LoopbackConnector for SystemLoopbackConnector {
    fn connect(
        &self,
        address: SocketAddr,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ConnectedLoopback, PlatformFailure> {
        connect_reserved_loopback(address, cancellation, deadline)
    }
}

impl SourcePortReservation {
    fn new(port: u16) -> Result<Self, PlatformFailure> {
        let mut ledger = reservation_ledger()
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        if ledger.ports.contains(&port) {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        *ledger.source_ports.entry(port).or_default() += 1;
        Ok(Self { port })
    }
}

impl Drop for SourcePortReservation {
    fn drop(&mut self) {
        let Ok(mut ledger) = reservation_ledger().lock() else {
            return;
        };
        let Some(count) = ledger.source_ports.get_mut(&self.port) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            ledger.source_ports.remove(&self.port);
        }
    }
}

impl AdapterReservation {
    fn release(&mut self) -> Result<(), PlatformFailure> {
        if self.released {
            return Ok(());
        }
        let mut ledger = reservation_ledger()
            .lock()
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        if !ledger.ports.remove(&self.port) {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        if let Some(target) = &self.target
            && !ledger.targets.remove(target)
        {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        self.released = true;
        Ok(())
    }
}

impl PlatformTargetAdapter for AppleSimulatorAdapter {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        absolute_deadline: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch> {
        let validation_failure = validate_selection(&selection).err();
        let target_key = validation_failure
            .is_none()
            .then(|| TargetKey::from_selection(&selection));
        let reservation = reserve_dynamic_loopback(target_key, absolute_deadline);
        let (listener, endpoint, owner, reservation_failure) = match reservation {
            Ok((listener, endpoint, owner, failure)) => {
                (Some(listener), endpoint, Some(owner), failure)
            }
            Err(failure) => (
                None,
                LaunchEndpoint::ios_loopback(49_152)
                    .unwrap_or_else(|_| unreachable!("constant endpoint is valid")),
                None,
                Some(failure),
            ),
        };
        Box::new(ApplePendingLaunch {
            endpoint,
            listener,
            selection,
            runner: Arc::clone(&self.runner),
            artifact_verifier: Arc::clone(&self.artifact_verifier),
            process_identity: Arc::clone(&self.process_identity),
            loopback: Arc::clone(&self.loopback),
            reservation: owner,
            failure: reservation_failure.or(validation_failure),
        })
    }
}

struct ApplePendingLaunch {
    endpoint: LaunchEndpoint,
    listener: Option<TcpListener>,
    selection: TargetSelection,
    runner: Arc<dyn ToolRunner>,
    artifact_verifier: Arc<dyn ArtifactVerifier>,
    process_identity: Arc<dyn ProcessIdentityProbe>,
    loopback: Arc<dyn LoopbackConnector>,
    reservation: Option<AdapterReservation>,
    failure: Option<PlatformFailure>,
}

impl PendingLaunch for ApplePendingLaunch {
    fn endpoint(&self) -> &LaunchEndpoint {
        &self.endpoint
    }

    fn launch(
        mut self: Box<Self>,
        descriptor: PublicLaunchDescriptor,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<LaunchedTargetIo, PlatformFailure> {
        if let Some(failure) = self.failure {
            return Err(self.prelaunch_failure(failure));
        }
        let preparation = (|| {
            check_cancel_deadline(&cancellation, absolute_deadline)?;
            if descriptor.canonical_bytes().len() > DESCRIPTOR_CAP {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            let artifact = self.artifact_verifier.prepare(
                &self.selection,
                &cancellation,
                absolute_deadline,
            )?;
            feature_probe(self.runner.as_ref(), &cancellation, absolute_deadline)?;
            let candidate = verify_exact_candidate(
                self.runner.as_ref(),
                self.artifact_verifier.as_ref(),
                &self.selection,
                &artifact,
                &cancellation,
                absolute_deadline,
            )?;
            self.artifact_verifier.assert_snapshot_unchanged(
                &artifact,
                &self.selection,
                &cancellation,
                absolute_deadline,
            )?;
            Ok((
                URL_SAFE_NO_PAD.encode(descriptor.canonical_bytes()),
                candidate,
                artifact,
            ))
        })();
        let (descriptor, candidate, artifact) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(self.prelaunch_failure(error));
            }
        };
        let request = ToolRequest::launch(
            self.runner.program(),
            self.selection.device_selector(),
            self.selection.app_id(),
            descriptor,
        );

        // The descriptor already contains this frozen endpoint. Release the
        // probe immediately before the exact launch and never substitute it.
        // Every connector binds and reserves its source port before connect,
        // so an unrelated Target cannot consume this reserved endpoint while
        // the Simulator process takes over the listener.
        drop(self.listener.take());
        let launched =
            match self
                .runner
                .run(request, LAUNCH_OUTPUT_CAP, &cancellation, absolute_deadline)
            {
                Ok(output) if output.status.success() && !output.oversized => output,
                Ok(_) => {
                    return Err(self.after_uncertain_launch_failure(
                        &candidate,
                        &artifact,
                        failure(PlatformFailureKind::Rejected),
                    ));
                }
                Err(error) => {
                    return Err(self.after_uncertain_launch_failure(&candidate, &artifact, error));
                }
            };
        let pid = match parse_launch_pid(&launched.stdout, self.selection.app_id()) {
            Ok(pid) => pid,
            Err(error) => {
                return Err(self.after_uncertain_launch_failure(&candidate, &artifact, error));
            }
        };
        let owner = match prove_exact_owner(
            self.runner.as_ref(),
            self.process_identity.as_ref(),
            &self.selection,
            &candidate.app_path,
            pid,
            &cancellation,
            absolute_deadline,
        ) {
            Ok(owner) => owner,
            Err(error) => {
                return Err(self.after_uncertain_launch_failure(&candidate, &artifact, error));
            }
        };
        if self
            .artifact_verifier
            .assert_snapshot_unchanged(&artifact, &self.selection, &cancellation, absolute_deadline)
            .and_then(|()| {
                self.artifact_verifier.verify_installed(
                    &candidate.app_path,
                    &artifact,
                    &self.selection,
                    &cancellation,
                    absolute_deadline,
                )
            })
            .is_err()
        {
            return Err(clean_after_owned_launch_failure(
                OwnedLaunchFailure {
                    runner: self.runner.as_ref(),
                    process_identity: self.process_identity.as_ref(),
                    artifact_verifier: self.artifact_verifier.as_ref(),
                    selection: &self.selection,
                    artifact: &artifact,
                    candidate: &candidate,
                    owner: &owner,
                },
                self.reservation.as_mut(),
                failure(PlatformFailureKind::Rejected),
            ));
        }

        let address = SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            self.endpoint
                .ios_port()
                .ok_or_else(|| failure(PlatformFailureKind::Internal))?,
        ));
        let bootstrap = match self
            .loopback
            .connect(address, &cancellation, absolute_deadline)
        {
            Ok(connection) => connection,
            // Without endpoint takeover, `simctl launch` may have attached to
            // an existing process and its returned PID is not an owned Target.
            // Never kill it; retain the reservation tombstone and fail closed.
            Err(error) => {
                return Err(self.after_uncertain_launch_failure(&candidate, &artifact, error));
            }
        };
        let bootstrap = match TcpRawDuplex::new(bootstrap) {
            Ok(bootstrap) => bootstrap,
            Err(original) => {
                return Err(clean_after_owned_launch_failure(
                    OwnedLaunchFailure {
                        runner: self.runner.as_ref(),
                        process_identity: self.process_identity.as_ref(),
                        artifact_verifier: self.artifact_verifier.as_ref(),
                        selection: &self.selection,
                        artifact: &artifact,
                        candidate: &candidate,
                        owner: &owner,
                    },
                    self.reservation.as_mut(),
                    original,
                ));
            }
        };
        if self
            .artifact_verifier
            .assert_snapshot_unchanged(&artifact, &self.selection, &cancellation, absolute_deadline)
            .and_then(|()| {
                self.artifact_verifier.verify_installed(
                    &candidate.app_path,
                    &artifact,
                    &self.selection,
                    &cancellation,
                    absolute_deadline,
                )
            })
            .is_err()
        {
            bootstrap.cancel();
            return Err(clean_after_owned_launch_failure(
                OwnedLaunchFailure {
                    runner: self.runner.as_ref(),
                    process_identity: self.process_identity.as_ref(),
                    artifact_verifier: self.artifact_verifier.as_ref(),
                    selection: &self.selection,
                    artifact: &artifact,
                    candidate: &candidate,
                    owner: &owner,
                },
                self.reservation.as_mut(),
                failure(PlatformFailureKind::Rejected),
            ));
        }
        let registry = Arc::new(ConnectionRegistry::default());
        let bootstrap = Arc::new(bootstrap);
        registry.register(&bootstrap);
        let connector = Arc::new(AppleRawConnector {
            address,
            registry: Arc::clone(&registry),
            loopback: Arc::clone(&self.loopback),
        });
        let cleanup = Box::new(AppleCleanup {
            runner: Arc::clone(&self.runner),
            process_identity: Arc::clone(&self.process_identity),
            artifact_verifier: Arc::clone(&self.artifact_verifier),
            selection: self.selection,
            artifact,
            candidate,
            owner,
            reservation: self.reservation.take(),
            registry,
        });
        Ok(LaunchedTargetIo::new(bootstrap, connector, cleanup))
    }

    fn abort(
        mut self: Box<Self>,
        _cancellation: Cancellation,
        _absolute_deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        drop(self.listener.take());
        self.release_reservation()
    }
}

impl ApplePendingLaunch {
    fn prelaunch_failure(&mut self, original: PlatformFailure) -> PlatformFailure {
        drop(self.listener.take());
        if original.kind() == PlatformFailureKind::CleanupFailed {
            return original;
        }
        if self.release_reservation().is_err() {
            failure(PlatformFailureKind::CleanupFailed)
        } else {
            original
        }
    }

    fn release_reservation(&mut self) -> Result<(), PlatformFailure> {
        if let Some(mut reservation) = self.reservation.take() {
            reservation.release()?;
        }
        Ok(())
    }

    fn after_uncertain_launch_failure(
        &mut self,
        candidate: &LaunchCandidate,
        artifact: &PreparedLaunchArtifact,
        original: PlatformFailure,
    ) -> PlatformFailure {
        let Ok(cleanup_deadline) = cleanup_deadline() else {
            return failure(PlatformFailureKind::CleanupFailed);
        };
        let cleanup_cancellation = Cancellation::new();
        let cleanup = matching_processes(
            self.runner.as_ref(),
            self.selection.device_selector(),
            &candidate.app_path,
            &cleanup_cancellation,
            cleanup_deadline,
        );
        if cleanup.is_ok_and(|processes| processes.is_empty())
            && cleanup_owned_installation(
                self.runner.as_ref(),
                self.artifact_verifier.as_ref(),
                &self.selection,
                artifact,
                candidate,
                &cleanup_cancellation,
                cleanup_deadline,
            )
            .is_ok()
            && self.release_reservation().is_ok()
        {
            original
        } else {
            failure(PlatformFailureKind::CleanupFailed)
        }
    }
}

struct AppleRawConnector {
    address: SocketAddr,
    registry: Arc<ConnectionRegistry>,
    loopback: Arc<dyn LoopbackConnector>,
}

impl RawConnector for AppleRawConnector {
    fn connect(
        &self,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
        let connection = self
            .loopback
            .connect(self.address, &cancellation, absolute_deadline)?;
        let raw = Arc::new(TcpRawDuplex::new(connection)?);
        self.registry.register(&raw);
        Ok(raw)
    }
}

struct TcpRawDuplex {
    reader: Mutex<TcpStream>,
    writer: Mutex<TcpStream>,
    shutdown: TcpStream,
    cancelled: std::sync::atomic::AtomicBool,
    _source_port: SourcePortReservation,
}

impl TcpRawDuplex {
    fn new(connection: ConnectedLoopback) -> Result<Self, PlatformFailure> {
        let ConnectedLoopback {
            stream,
            source_port,
        } = connection;
        if !stream
            .peer_addr()
            .is_ok_and(|address| address.ip().is_loopback())
            || !stream
                .local_addr()
                .is_ok_and(|address| address.ip().is_loopback())
        {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        stream
            .set_nodelay(true)
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let writer = stream
            .try_clone()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let shutdown = stream
            .try_clone()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        Ok(Self {
            reader: Mutex::new(stream),
            writer: Mutex::new(writer),
            shutdown,
            cancelled: std::sync::atomic::AtomicBool::new(false),
            _source_port: source_port,
        })
    }

    fn ensure_active(&self) -> Result<(), PlatformFailure> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            Err(failure(PlatformFailureKind::Cancelled))
        } else {
            Ok(())
        }
    }
}

impl RawDuplex for TcpRawDuplex {
    fn read(
        &self,
        output: &mut [u8],
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure> {
        self.ensure_active()?;
        let timeout = remaining(absolute_deadline)?;
        let mut stream = self
            .reader
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        match stream.read(output) {
            Ok(count) => {
                self.ensure_active()?;
                Ok(count)
            }
            Err(error) => Err(io_failure(&error, &self.cancelled)),
        }
    }

    fn write(
        &self,
        input: &[u8],
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<usize, PlatformFailure> {
        self.ensure_active()?;
        let timeout = remaining(absolute_deadline)?;
        let mut stream = self
            .writer
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        match stream.write(input) {
            Ok(count) => {
                self.ensure_active()?;
                Ok(count)
            }
            Err(error) => Err(io_failure(&error, &self.cancelled)),
        }
    }

    fn cancel(&self) {
        if !self
            .cancelled
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let _ = self.shutdown.shutdown(Shutdown::Both);
        }
    }
}

#[derive(Default)]
struct ConnectionRegistry {
    streams: Mutex<Vec<Weak<TcpRawDuplex>>>,
}

impl ConnectionRegistry {
    fn register(&self, stream: &Arc<TcpRawDuplex>) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.retain(|candidate| candidate.strong_count() > 0);
            streams.push(Arc::downgrade(stream));
        }
    }

    fn cancel_all(&self) -> Result<(), PlatformFailure> {
        let streams = self
            .streams
            .lock()
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        for stream in streams.iter().filter_map(Weak::upgrade) {
            stream.cancel();
        }
        Ok(())
    }
}

struct AppleCleanup {
    runner: Arc<dyn ToolRunner>,
    process_identity: Arc<dyn ProcessIdentityProbe>,
    artifact_verifier: Arc<dyn ArtifactVerifier>,
    selection: TargetSelection,
    artifact: PreparedLaunchArtifact,
    candidate: LaunchCandidate,
    owner: TargetOwner,
    reservation: Option<AdapterReservation>,
    registry: Arc<ConnectionRegistry>,
}

impl CleanupReceipt for AppleCleanup {
    fn cleanup(
        mut self: Box<Self>,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        self.registry.cancel_all()?;
        terminate_exact_owner(
            self.runner.as_ref(),
            self.process_identity.as_ref(),
            &self.owner,
            &cancellation,
            absolute_deadline,
        )
        .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        cleanup_owned_installation(
            self.runner.as_ref(),
            self.artifact_verifier.as_ref(),
            &self.selection,
            &self.artifact,
            &self.candidate,
            &cancellation,
            absolute_deadline,
        )
        .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        let Some(mut reservation) = self.reservation.take() else {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        };
        reservation.release()
    }
}

fn reserve_dynamic_loopback(
    target: Option<TargetKey>,
    deadline: AbsoluteDeadline,
) -> Result<
    (
        TcpListener,
        LaunchEndpoint,
        AdapterReservation,
        Option<PlatformFailure>,
    ),
    PlatformFailure,
> {
    remaining(deadline)?;
    // Darwin allocates ephemeral ports in the contract range. Retrying keeps
    // this an OS-random selection and never falls back to a fixed port.
    for _ in 0..1_024 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let port = listener
            .local_addr()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?
            .port();
        if let Ok(endpoint) = LaunchEndpoint::ios_loopback(port) {
            let mut ledger = reservation_ledger()
                .lock()
                .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
            if ledger.ports.contains(&port) || ledger.source_ports.contains_key(&port) {
                continue;
            }
            let target_conflict = target
                .as_ref()
                .is_some_and(|candidate| ledger.targets.contains(candidate));
            let owned_target = if target_conflict {
                None
            } else {
                target.clone().inspect(|candidate| {
                    ledger.targets.insert(candidate.clone());
                })
            };
            ledger.ports.insert(port);
            drop(ledger);
            return Ok((
                listener,
                endpoint,
                AdapterReservation {
                    port,
                    target: owned_target,
                    released: false,
                },
                target_conflict.then(|| failure(PlatformFailureKind::Rejected)),
            ));
        }
    }
    Err(failure(PlatformFailureKind::Unavailable))
}

fn validate_selection(selection: &TargetSelection) -> Result<(), PlatformFailure> {
    let artifact = Path::new(selection.artifact_path());
    if selection.platform() != Platform::IosSimulator
        || !is_exact_udid(selection.device_selector())
        || !is_bundle_id(selection.app_id())
        || !artifact.is_absolute()
        || artifact.extension() != Some(std::ffi::OsStr::new("app"))
        || selection.artifact_path().as_bytes().contains(&0)
        || selection.app_id().as_bytes().contains(&0)
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn is_exact_udid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)
            }
        })
}

fn is_bundle_id(value: &str) -> bool {
    (3..=255).contains(&value.len())
        && value.contains('.')
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(is_bundle_byte))
}

fn is_bundle_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn feature_probe(
    runner: &dyn ToolRunner,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let list = run_success(
        runner,
        ToolRequest::plain(runner.program(), ["simctl", "help", "list"]),
        cancellation,
        deadline,
    )?;
    let list = strict_help_text(&list)?;
    if !list.contains("--json") || !list.contains("devices") {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let launch = run_success(
        runner,
        ToolRequest::plain(runner.program(), ["simctl", "help", "launch"]),
        cancellation,
        deadline,
    )?;
    let launch = strict_help_text(&launch)?;
    if !launch.contains("SIMCTL_CHILD_")
        || !launch.contains("<device>")
        || !launch.contains("bundle identifier")
    {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let spawn = run_success(
        runner,
        ToolRequest::plain(runner.program(), ["simctl", "help", "spawn"]),
        cancellation,
        deadline,
    )?;
    if !strict_help_text(&spawn)?.contains("Spawn a process") {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    for (command, expected) in [
        ("listapps", "Show the installed applications"),
        ("install", "Install an app"),
        ("uninstall", "Uninstall an app"),
    ] {
        let output = run_success(
            runner,
            ToolRequest::plain(runner.program(), ["simctl", "help", command]),
            cancellation,
            deadline,
        )?;
        if !strict_help_text(&output)?.contains(expected) {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
    }
    Ok(())
}

fn verify_exact_candidate(
    runner: &dyn ToolRunner,
    artifact_verifier: &dyn ArtifactVerifier,
    selection: &TargetSelection,
    artifact: &PreparedLaunchArtifact,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<LaunchCandidate, PlatformFailure> {
    let devices = run_success(
        runner,
        ToolRequest::plain(
            runner.program(),
            [
                "simctl",
                "list",
                "--json",
                "devices",
                selection.device_selector(),
            ],
        ),
        cancellation,
        deadline,
    )?;
    verify_device_json(&devices.stdout, selection.device_selector())?;

    let present = installed_app_present(runner, selection, cancellation, deadline)?;
    let installed_by_lease = if present {
        false
    } else {
        artifact_verifier.assert_snapshot_unchanged(artifact, selection, cancellation, deadline)?;
        run_success(
            runner,
            ToolRequest::plain(
                runner.program(),
                [
                    OsString::from("simctl"),
                    OsString::from("install"),
                    OsString::from(selection.device_selector()),
                    artifact.snapshot_path.as_os_str().to_owned(),
                ],
            ),
            cancellation,
            deadline,
        )
        .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        if !installed_app_present(runner, selection, cancellation, deadline)? {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        true
    };
    let installed = installed_app_path(runner, selection, cancellation, deadline)?;
    if artifact_verifier
        .verify_installed(&installed, artifact, selection, cancellation, deadline)
        .is_err()
    {
        return Err(if installed_by_lease {
            failure(PlatformFailureKind::CleanupFailed)
        } else {
            failure(PlatformFailureKind::Rejected)
        });
    }
    if !matching_processes(
        runner,
        selection.device_selector(),
        &installed,
        cancellation,
        deadline,
    )?
    .is_empty()
    {
        return Err(failure(if installed_by_lease {
            PlatformFailureKind::CleanupFailed
        } else {
            PlatformFailureKind::Rejected
        }));
    }
    Ok(LaunchCandidate {
        app_path: installed,
        installed_by_lease,
    })
}

struct LaunchCandidate {
    app_path: PathBuf,
    installed_by_lease: bool,
}

fn installed_app_present(
    runner: &dyn ToolRunner,
    selection: &TargetSelection,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<bool, PlatformFailure> {
    let output = run_success(
        runner,
        ToolRequest::plain(
            runner.program(),
            ["simctl", "listapps", selection.device_selector()],
        ),
        cancellation,
        deadline,
    )?;
    parse_listapps_contains(&output.stdout, selection.app_id())
}

fn parse_listapps_contains(bytes: &[u8], app_id: &str) -> Result<bool, PlatformFailure> {
    if bytes.is_empty() || bytes.len() > TOOL_OUTPUT_CAP {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let bytes = strict_utf8(bytes)?.as_bytes();
    let keys = OpenStepParser::new(bytes).parse_root_dictionary()?;
    Ok(keys.contains(app_id))
}

const OPENSTEP_DEPTH_CAP: usize = 128;
const OPENSTEP_NODE_CAP: usize = 65_536;

struct OpenStepParser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    nodes: usize,
}

impl<'a> OpenStepParser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            nodes: 0,
        }
    }

    fn parse_root_dictionary(mut self) -> Result<HashSet<String>, PlatformFailure> {
        self.skip_trivia()?;
        let keys = self.parse_dictionary(0)?;
        self.skip_trivia()?;
        if self.cursor != self.bytes.len() {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(keys)
    }

    fn parse_dictionary(&mut self, depth: usize) -> Result<HashSet<String>, PlatformFailure> {
        self.enter(depth)?;
        self.require(b'{')?;
        let mut keys = HashSet::new();
        loop {
            self.skip_trivia()?;
            if self.consume(b'}') {
                return Ok(keys);
            }
            let key = self.parse_string(true)?;
            if !keys.insert(key) {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            self.skip_trivia()?;
            self.require(b'=')?;
            self.parse_value(depth + 1)?;
            self.skip_trivia()?;
            self.require(b';')?;
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<(), PlatformFailure> {
        self.enter(depth)?;
        self.skip_trivia()?;
        match self.bytes.get(self.cursor) {
            Some(b'{') => {
                self.parse_dictionary(depth)?;
            }
            Some(b'(') => self.parse_array(depth)?,
            Some(b'<') => self.parse_data()?,
            Some(_) => {
                self.parse_string(false)?;
            }
            None => return Err(failure(PlatformFailureKind::Rejected)),
        }
        Ok(())
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), PlatformFailure> {
        self.require(b'(')?;
        self.skip_trivia()?;
        if self.consume(b')') {
            return Ok(());
        }
        loop {
            self.parse_value(depth + 1)?;
            self.skip_trivia()?;
            if self.consume(b')') {
                return Ok(());
            }
            self.require(b',')?;
            self.skip_trivia()?;
            if self.consume(b')') {
                return Ok(());
            }
        }
    }

    fn parse_data(&mut self) -> Result<(), PlatformFailure> {
        self.require(b'<')?;
        let mut digits = 0_usize;
        loop {
            match self.bytes.get(self.cursor).copied() {
                Some(b'>') if digits.is_multiple_of(2) => {
                    self.cursor += 1;
                    return Ok(());
                }
                Some(byte) if byte.is_ascii_hexdigit() => {
                    digits += 1;
                    self.cursor += 1;
                }
                Some(byte) if byte.is_ascii_whitespace() => self.cursor += 1,
                _ => return Err(failure(PlatformFailureKind::Rejected)),
            }
        }
    }

    fn parse_string(&mut self, capture: bool) -> Result<String, PlatformFailure> {
        self.skip_trivia()?;
        if self.consume(b'"') {
            let mut segment = self.cursor;
            let mut captured = capture.then(String::new);
            while let Some(byte) = self.bytes.get(self.cursor).copied() {
                match byte {
                    b'"' => {
                        if let Some(value) = &mut captured {
                            value.push_str(strict_utf8(&self.bytes[segment..self.cursor])?);
                        }
                        self.cursor += 1;
                        let value = captured.unwrap_or_default();
                        if capture && value.is_empty() {
                            return Err(failure(PlatformFailureKind::Rejected));
                        }
                        return Ok(value);
                    }
                    b'\\' => {
                        if let Some(value) = &mut captured {
                            value.push_str(strict_utf8(&self.bytes[segment..self.cursor])?);
                        }
                        self.cursor += 1;
                        let escaped = self.parse_escape()?;
                        if let Some(value) = &mut captured {
                            value.push(escaped);
                        }
                        segment = self.cursor;
                    }
                    0x00..=0x1f => {
                        return Err(failure(PlatformFailureKind::Rejected));
                    }
                    _ => self.cursor += 1,
                }
            }
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let start = self.cursor;
        while self.bytes.get(self.cursor).is_some_and(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(*byte, b'_' | b'$' | b'/' | b':' | b'.' | b'-' | b'+')
        }) {
            self.cursor += 1;
        }
        if start == self.cursor {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let value = strict_utf8(&self.bytes[start..self.cursor])?;
        Ok(if capture {
            value.to_owned()
        } else {
            String::new()
        })
    }

    fn parse_escape(&mut self) -> Result<char, PlatformFailure> {
        let byte = self
            .bytes
            .get(self.cursor)
            .copied()
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        self.cursor += 1;
        match byte {
            b'"' => Ok('"'),
            b'\\' => Ok('\\'),
            b'a' => Ok('\u{7}'),
            b'b' => Ok('\u{8}'),
            b'f' => Ok('\u{c}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'v' => Ok('\u{b}'),
            b'U' => {
                let end = self
                    .cursor
                    .checked_add(4)
                    .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
                let digits = self
                    .bytes
                    .get(self.cursor..end)
                    .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
                if !digits.iter().all(u8::is_ascii_hexdigit) {
                    return Err(failure(PlatformFailureKind::Rejected));
                }
                self.cursor = end;
                let digits = strict_utf8(digits)?;
                let scalar = u32::from_str_radix(digits, 16)
                    .map_err(|_| failure(PlatformFailureKind::Rejected))?;
                char::from_u32(scalar).ok_or_else(|| failure(PlatformFailureKind::Rejected))
            }
            b'0'..=b'7' => {
                let mut scalar = u32::from(byte - b'0');
                for _ in 0..2 {
                    let Some(next @ b'0'..=b'7') = self.bytes.get(self.cursor).copied() else {
                        break;
                    };
                    scalar = scalar * 8 + u32::from(next - b'0');
                    self.cursor += 1;
                }
                char::from_u32(scalar).ok_or_else(|| failure(PlatformFailureKind::Rejected))
            }
            _ => Err(failure(PlatformFailureKind::Rejected)),
        }
    }

    fn enter(&mut self, depth: usize) -> Result<(), PlatformFailure> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        if depth > OPENSTEP_DEPTH_CAP || self.nodes > OPENSTEP_NODE_CAP {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(())
    }

    fn skip_trivia(&mut self) -> Result<(), PlatformFailure> {
        loop {
            skip_ascii_whitespace(self.bytes, &mut self.cursor);
            if self.bytes.get(self.cursor..self.cursor.saturating_add(2)) == Some(b"//") {
                self.cursor += 2;
                while self
                    .bytes
                    .get(self.cursor)
                    .is_some_and(|byte| *byte != b'\n' && *byte != b'\r')
                {
                    self.cursor += 1;
                }
                continue;
            }
            if self.bytes.get(self.cursor..self.cursor.saturating_add(2)) == Some(b"/*") {
                self.cursor += 2;
                let mut closed = false;
                while self.cursor < self.bytes.len() {
                    if self.bytes.get(self.cursor..self.cursor.saturating_add(2)) == Some(b"*/") {
                        self.cursor += 2;
                        closed = true;
                        break;
                    }
                    self.cursor += 1;
                }
                if !closed {
                    return Err(failure(PlatformFailureKind::Rejected));
                }
                continue;
            }
            return Ok(());
        }
    }

    fn require(&mut self, expected: u8) -> Result<(), PlatformFailure> {
        require_byte(self.bytes, &mut self.cursor, expected)
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.cursor) == Some(&expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }
}

fn skip_ascii_whitespace(bytes: &[u8], cursor: &mut usize) {
    while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
        *cursor += 1;
    }
}

fn require_byte(bytes: &[u8], cursor: &mut usize, expected: u8) -> Result<(), PlatformFailure> {
    if bytes.get(*cursor) != Some(&expected) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    *cursor += 1;
    Ok(())
}

fn installed_app_path(
    runner: &dyn ToolRunner,
    selection: &TargetSelection,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<PathBuf, PlatformFailure> {
    let app = run_success(
        runner,
        ToolRequest::plain(
            runner.program(),
            [
                "simctl",
                "get_app_container",
                selection.device_selector(),
                selection.app_id(),
                "app",
            ],
        ),
        cancellation,
        deadline,
    )?;
    let installed = single_line_path(&app.stdout)?;
    let installed =
        fs::canonicalize(installed).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    if !installed.is_dir() {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(installed)
}

#[derive(Clone)]
struct TargetOwner {
    udid: String,
    app_path: PathBuf,
    pid: u32,
    identity: ProcessIdentity,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ProcessIdentity {
    start_seconds: u64,
    start_microseconds: u64,
}

trait ProcessIdentityProbe: Send + Sync {
    fn identity(&self, pid: u32, installed_app: &Path) -> Result<ProcessIdentity, PlatformFailure>;
}

struct DarwinProcessIdentityProbe;

#[repr(C)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [libc::c_char; 16],
    pbi_name: [libc::c_char; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffer_size: libc::c_int,
    ) -> libc::c_int;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffer_size: u32) -> libc::c_int;
}

impl ProcessIdentityProbe for DarwinProcessIdentityProbe {
    fn identity(&self, pid: u32, installed_app: &Path) -> Result<ProcessIdentity, PlatformFailure> {
        let pid = i32::try_from(pid).map_err(|_| failure(PlatformFailureKind::Rejected))?;
        let mut info = MaybeUninit::<ProcBsdInfo>::uninit();
        let info_size = i32::try_from(std::mem::size_of::<ProcBsdInfo>())
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        // SAFETY: `info` points to `info_size` writable bytes and the flavor's
        // ABI layout is frozen by Darwin's public `proc_bsdinfo` definition.
        let written = unsafe {
            proc_pidinfo(
                pid,
                3, // PROC_PIDTBSDINFO
                0,
                info.as_mut_ptr().cast(),
                info_size,
            )
        };
        if written != info_size {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        // SAFETY: a full `ProcBsdInfo` was written above.
        let info = unsafe { info.assume_init() };
        if info.pbi_pid != u32::try_from(pid).unwrap_or_default() {
            return Err(failure(PlatformFailureKind::Rejected));
        }

        let mut path = [0_u8; 4_096];
        // SAFETY: `path` is a writable buffer with the exact advertised size.
        let path_len = unsafe {
            proc_pidpath(
                pid,
                path.as_mut_ptr().cast(),
                u32::try_from(path.len()).unwrap_or_default(),
            )
        };
        let path_len = usize::try_from(path_len)
            .ok()
            .filter(|length| *length > 0 && *length < path.len())
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        let executable = Path::new(std::ffi::OsStr::from_bytes(&path[..path_len]));
        let executable =
            fs::canonicalize(executable).map_err(|_| failure(PlatformFailureKind::Rejected))?;
        if executable.parent() != Some(installed_app) {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(ProcessIdentity {
            start_seconds: info.pbi_start_tvsec,
            start_microseconds: info.pbi_start_tvusec,
        })
    }
}

fn prove_exact_owner(
    runner: &dyn ToolRunner,
    process_identity: &dyn ProcessIdentityProbe,
    selection: &TargetSelection,
    installed_app: &Path,
    pid: u32,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<TargetOwner, PlatformFailure> {
    let processes = matching_processes(
        runner,
        selection.device_selector(),
        installed_app,
        cancellation,
        deadline,
    )?;
    if processes.as_slice() != [pid] {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let identity = process_identity.identity(pid, installed_app)?;
    Ok(TargetOwner {
        udid: selection.device_selector().to_owned(),
        app_path: installed_app.to_path_buf(),
        pid,
        identity,
    })
}

fn matching_processes(
    runner: &dyn ToolRunner,
    udid: &str,
    installed_app: &Path,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<Vec<u32>, PlatformFailure> {
    let output = run_success(
        runner,
        ToolRequest::plain(
            runner.program(),
            ["simctl", "spawn", udid, "/bin/ps", "-axo", "pid=,command="],
        ),
        cancellation,
        deadline,
    )?;
    parse_process_table(&output.stdout, installed_app)
}

fn parse_process_table(bytes: &[u8], installed_app: &Path) -> Result<Vec<u32>, PlatformFailure> {
    if bytes.is_empty() || bytes.len() > TOOL_OUTPUT_CAP || !bytes.ends_with(b"\n") {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let prefix = format!(
        "{}/",
        installed_app
            .to_str()
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?
    );
    let mut matches = Vec::new();
    for line in strict_utf8(bytes)?.lines() {
        let line = line.trim_start_matches(' ');
        let (pid, command) = line
            .split_once(' ')
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        let pid = pid
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        let command = command.trim_start_matches(' ');
        if command.is_empty() {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        if let Some(relative_command) = command.strip_prefix(&prefix)
            && let Some(relative) = relative_command.split(' ').next()
            && !relative.is_empty()
            && !relative.contains('/')
        {
            matches.push(pid);
        }
    }
    matches.sort_unstable();
    matches.dedup();
    Ok(matches)
}

fn parse_launch_pid(bytes: &[u8], app_id: &str) -> Result<u32, PlatformFailure> {
    if bytes.is_empty() || bytes.len() > 4_096 || !bytes.ends_with(b"\n") {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let line = strict_utf8(&bytes[..bytes.len() - 1])?;
    if line.contains(['\n', '\r', '\0']) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let pid = line
        .strip_prefix(app_id)
        .and_then(|suffix| suffix.strip_prefix(": "))
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    Ok(pid)
}

fn verify_device_json(bytes: &[u8], exact_udid: &str) -> Result<(), PlatformFailure> {
    if bytes.is_empty() || bytes.len() > TOOL_OUTPUT_CAP {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let text = strict_utf8(bytes)?;
    let value = parse_strict_json(text)?;
    let devices = value
        .as_object()
        .and_then(|root| root.get("devices"))
        .and_then(Value::as_object)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let mut matches = Vec::new();
    for runtime in devices.values() {
        let entries = runtime
            .as_array()
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        for entry in entries {
            let entry = entry
                .as_object()
                .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
            let udid = entry
                .get("udid")
                .and_then(Value::as_str)
                .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
            let state = entry
                .get("state")
                .and_then(Value::as_str)
                .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
            let available = entry
                .get("isAvailable")
                .and_then(Value::as_bool)
                .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
            if udid == exact_udid {
                matches.push((state, available));
            }
        }
    }
    if matches.as_slice() != [("Booted", true)] {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn single_line_path(bytes: &[u8]) -> Result<&str, PlatformFailure> {
    if bytes.is_empty() || bytes.len() > 16_384 || !bytes.ends_with(b"\n") {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let text = strict_utf8(&bytes[..bytes.len() - 1])?;
    if text.is_empty() || text.contains(['\n', '\r', '\0']) || !Path::new(text).is_absolute() {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(text)
}

fn strict_utf8(bytes: &[u8]) -> Result<&str, PlatformFailure> {
    std::str::from_utf8(bytes).map_err(|_| failure(PlatformFailureKind::Rejected))
}

fn strict_help_text(output: &ToolOutput) -> Result<&str, PlatformFailure> {
    match (output.stdout.is_empty(), output.stderr.is_empty()) {
        (false, true) => strict_utf8(&output.stdout),
        (true, false) => strict_utf8(&output.stderr),
        _ => Err(failure(PlatformFailureKind::Rejected)),
    }
}

fn parse_strict_json(text: &str) -> Result<Value, PlatformFailure> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = StrictValue
        .deserialize(&mut deserializer)
        .map_err(|_| failure(PlatformFailureKind::Rejected))?;
    deserializer
        .end()
        .map_err(|_| failure(PlatformFailureKind::Rejected))?;
    Ok(value)
}

struct StrictValue;

impl<'de> DeserializeSeed<'de> for StrictValue {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object members")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }
    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StrictValue.deserialize(deserializer)
    }
    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictValue)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate object member"));
            }
            values.insert(key, object.next_value_seed(StrictValue)?);
        }
        Ok(Value::Object(values))
    }
}

fn connect_reserved_loopback(
    address: SocketAddr,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<ConnectedLoopback, PlatformFailure> {
    if !address.ip().is_loopback() || address.port() < 49_152 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    loop {
        check_cancel_deadline(cancellation, deadline)?;
        match connect_once_with_reserved_source(address, cancellation, deadline) {
            Ok(connection) => return Ok(connection),
            Err(error) if error.kind() == PlatformFailureKind::Unavailable => {}
            Err(error) => return Err(error),
        }
    }
}

fn connect_once_with_reserved_source(
    address: SocketAddr,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<ConnectedLoopback, PlatformFailure> {
    let SocketAddr::V4(address) = address else {
        return Err(failure(PlatformFailureKind::Rejected));
    };
    // SAFETY: the descriptor is immediately adopted by OwnedFd, and every
    // sockaddr/poll/getsockopt pointer below refers to initialized storage for
    // the duration of the syscall.
    let socket = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        OwnedFd::from_raw_fd(fd)
    };
    let local = darwin_sockaddr(Ipv4Addr::LOCALHOST, 0);
    // SAFETY: local is a valid initialized sockaddr_in.
    if unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&local as *const libc::sockaddr_in).cast(),
            u32::try_from(std::mem::size_of_val(&local)).unwrap_or_default(),
        )
    } != 0
    {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let source_port = bound_port(&socket)?;
    let source_port = SourcePortReservation::new(source_port)?;
    let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let remote = darwin_sockaddr(*address.ip(), address.port());
    // SAFETY: remote is a valid initialized sockaddr_in.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&remote as *const libc::sockaddr_in).cast(),
            u32::try_from(std::mem::size_of_val(&remote)).unwrap_or_default(),
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(connect_io_failure(&error));
        }
        wait_for_connect(&socket, cancellation, deadline)?;
    }
    // SAFETY: restoring the original descriptor flags is independent of the
    // socket's connection state.
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let stream = TcpStream::from(socket);
    Ok(ConnectedLoopback {
        stream,
        source_port,
    })
}

fn darwin_sockaddr(ip: Ipv4Addr, port: u16) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_len: u8::try_from(std::mem::size_of::<libc::sockaddr_in>()).unwrap_or_default(),
        sin_family: libc::AF_INET as u8,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(ip.octets()),
        },
        sin_zero: [0; 8],
    }
}

fn bound_port(socket: &OwnedFd) -> Result<u16, PlatformFailure> {
    let mut local = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut length = u32::try_from(std::mem::size_of::<libc::sockaddr_in>()).unwrap_or_default();
    // SAFETY: local has sufficient initialized storage and length describes it.
    if unsafe { libc::getsockname(socket.as_raw_fd(), local.as_mut_ptr().cast(), &mut length) } != 0
        || usize::try_from(length).ok() != Some(std::mem::size_of::<libc::sockaddr_in>())
    {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    // SAFETY: getsockname succeeded and reported the full structure size.
    Ok(u16::from_be(unsafe { local.assume_init() }.sin_port))
}

fn wait_for_connect(
    socket: &OwnedFd,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    loop {
        check_cancel_deadline(cancellation, deadline)?;
        let slice = remaining(deadline)?.min(CONNECT_SLICE);
        let timeout = i32::try_from(slice.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut pollfd = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: pollfd points to one initialized entry for the call.
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if result == 0 {
            continue;
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        let mut socket_error: libc::c_int = 0;
        let mut length = u32::try_from(std::mem::size_of_val(&socket_error)).unwrap_or_default();
        // SAFETY: socket_error and length describe writable initialized storage.
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        if socket_error == 0 {
            return Ok(());
        }
        return Err(connect_io_failure(&io::Error::from_raw_os_error(
            socket_error,
        )));
    }
}

fn connect_io_failure(_error: &io::Error) -> PlatformFailure {
    failure(PlatformFailureKind::Unavailable)
}

struct OwnedLaunchFailure<'a> {
    runner: &'a dyn ToolRunner,
    process_identity: &'a dyn ProcessIdentityProbe,
    artifact_verifier: &'a dyn ArtifactVerifier,
    selection: &'a TargetSelection,
    artifact: &'a PreparedLaunchArtifact,
    candidate: &'a LaunchCandidate,
    owner: &'a TargetOwner,
}

fn clean_after_owned_launch_failure(
    context: OwnedLaunchFailure<'_>,
    reservation: Option<&mut AdapterReservation>,
    original: PlatformFailure,
) -> PlatformFailure {
    let Ok(deadline) = cleanup_deadline() else {
        return failure(PlatformFailureKind::CleanupFailed);
    };
    if terminate_exact_owner(
        context.runner,
        context.process_identity,
        context.owner,
        &Cancellation::new(),
        deadline,
    )
    .is_err()
        || cleanup_owned_installation(
            context.runner,
            context.artifact_verifier,
            context.selection,
            context.artifact,
            context.candidate,
            &Cancellation::new(),
            deadline,
        )
        .is_err()
        || reservation.is_none_or(|reservation| reservation.release().is_err())
    {
        failure(PlatformFailureKind::CleanupFailed)
    } else {
        original
    }
}

fn cleanup_owned_installation(
    runner: &dyn ToolRunner,
    artifact_verifier: &dyn ArtifactVerifier,
    selection: &TargetSelection,
    artifact: &PreparedLaunchArtifact,
    candidate: &LaunchCandidate,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    if !candidate.installed_by_lease {
        return Ok(());
    }
    if !installed_app_present(runner, selection, cancellation, deadline)? {
        return Ok(());
    }
    let current = installed_app_path(runner, selection, cancellation, deadline)?;
    if current != candidate.app_path
        || artifact_verifier
            .verify_installed(&current, artifact, selection, cancellation, deadline)
            .is_err()
        || !matching_processes(
            runner,
            selection.device_selector(),
            &current,
            cancellation,
            deadline,
        )?
        .is_empty()
    {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    run_success(
        runner,
        ToolRequest::plain(
            runner.program(),
            [
                "simctl",
                "uninstall",
                selection.device_selector(),
                selection.app_id(),
            ],
        ),
        cancellation,
        deadline,
    )
    .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
    if installed_app_present(runner, selection, cancellation, deadline)? {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    Ok(())
}

fn terminate_exact_owner(
    runner: &dyn ToolRunner,
    process_identity: &dyn ProcessIdentityProbe,
    owner: &TargetOwner,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let current = matching_processes(runner, &owner.udid, &owner.app_path, cancellation, deadline)?;
    if current.is_empty() {
        return Ok(());
    }
    if current.as_slice() != [owner.pid] {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    if process_identity.identity(owner.pid, &owner.app_path)? != owner.identity {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    let pid = owner.pid.to_string();
    run_success(
        runner,
        ToolRequest::plain(
            runner.program(),
            [
                "simctl",
                "spawn",
                owner.udid.as_str(),
                "/bin/kill",
                "-TERM",
                pid.as_str(),
            ],
        ),
        cancellation,
        deadline,
    )
    .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
    let term_deadline = unix_ms()?
        .saturating_add(u64::try_from(TERM_GRACE.as_millis()).unwrap_or(CLEANUP_BUDGET_MS));
    let mut sent_kill = false;
    loop {
        let current =
            matching_processes(runner, &owner.udid, &owner.app_path, cancellation, deadline)?;
        if current.is_empty() {
            return Ok(());
        }
        if current.as_slice() != [owner.pid] {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        if process_identity.identity(owner.pid, &owner.app_path)? != owner.identity {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        if !sent_kill && unix_ms()? >= term_deadline {
            run_success(
                runner,
                ToolRequest::plain(
                    runner.program(),
                    [
                        "simctl",
                        "spawn",
                        owner.udid.as_str(),
                        "/bin/kill",
                        "-KILL",
                        pid.as_str(),
                    ],
                ),
                cancellation,
                deadline,
            )
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
            sent_kill = true;
        }
        if remaining(deadline).is_err() {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        thread::sleep(REAP_POLL);
    }
}

fn io_failure(error: &io::Error, cancelled: &std::sync::atomic::AtomicBool) -> PlatformFailure {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        failure(PlatformFailureKind::Cancelled)
    } else if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        failure(PlatformFailureKind::TimedOut)
    } else if matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    ) {
        failure(PlatformFailureKind::Eof)
    } else {
        failure(PlatformFailureKind::Unavailable)
    }
}

fn check_cancel_deadline(
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    if cancellation.is_cancelled() {
        Err(failure(PlatformFailureKind::Cancelled))
    } else {
        remaining(deadline).map(|_| ())
    }
}

fn remaining(deadline: AbsoluteDeadline) -> Result<Duration, PlatformFailure> {
    let now = unix_ms()?;
    let millis = deadline.value().saturating_sub(now);
    if millis == 0 {
        Err(failure(PlatformFailureKind::TimedOut))
    } else {
        Ok(Duration::from_millis(millis))
    }
}

fn cleanup_deadline() -> Result<AbsoluteDeadline, PlatformFailure> {
    AbsoluteDeadline::new(unix_ms()?.saturating_add(CLEANUP_BUDGET_MS))
}

fn unix_ms() -> Result<u64, PlatformFailure> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| failure(PlatformFailureKind::Internal))?
            .as_millis(),
    )
    .map_err(|_| failure(PlatformFailureKind::Internal))
}

fn failure(kind: PlatformFailureKind) -> PlatformFailure {
    PlatformFailure::new(kind)
}

#[derive(Debug)]
struct ToolOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    oversized: bool,
}

struct ToolRequest {
    program: PathBuf,
    args: Vec<OsString>,
    descriptor_env: Option<OsString>,
    scrub_simctl_child: bool,
}

impl ToolRequest {
    fn plain<P, I, A>(program: P, args: I) -> Self
    where
        P: Into<PathBuf>,
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            descriptor_env: None,
            scrub_simctl_child: false,
        }
    }

    fn launch(program: &Path, udid: &str, app_id: &str, descriptor: String) -> Self {
        Self {
            program: program.to_path_buf(),
            args: [
                OsString::from("simctl"),
                OsString::from("launch"),
                OsString::from(udid),
                OsString::from(app_id),
            ]
            .into(),
            descriptor_env: Some(descriptor.into()),
            scrub_simctl_child: true,
        }
    }
}

trait ToolRunner: Send + Sync {
    fn program(&self) -> &Path;
    fn run(
        &self,
        request: ToolRequest,
        cap: usize,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ToolOutput, PlatformFailure>;
}

trait ToolChild: Send {
    fn terminate(&mut self, deadline: AbsoluteDeadline) -> Result<(), PlatformFailure>;
}

struct ProcessToolRunner {
    xcrun: PathBuf,
}

impl ToolRunner for ProcessToolRunner {
    fn program(&self) -> &Path {
        &self.xcrun
    }

    fn run(
        &self,
        request: ToolRequest,
        cap: usize,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ToolOutput, PlatformFailure> {
        validate_tool_request(&request)?;
        check_cancel_deadline(cancellation, deadline)?;
        let child = spawn_captured(request, cap)?;
        wait_for_output(child, cancellation, deadline)
    }
}

fn validate_tool_request(request: &ToolRequest) -> Result<(), PlatformFailure> {
    if !request.program.is_absolute() {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn command_for(request: ToolRequest) -> Command {
    command_for_with_environment(request, std::env::vars_os().map(|(key, _)| key))
}

fn command_for_with_environment<I>(request: ToolRequest, inherited_keys: I) -> Command
where
    I: IntoIterator<Item = OsString>,
{
    let mut command = Command::new(request.program);
    command.args(request.args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    if request.scrub_simctl_child {
        for key in inherited_keys {
            if key
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(SIMCTL_CHILD_PREFIX.as_bytes())
            {
                command.env_remove(key);
            }
        }
    }
    if let Some(descriptor) = request.descriptor_env {
        command.env(DESCRIPTOR_ENV, descriptor);
    }
    command
}

struct CapturedChild {
    child: Child,
    stdout: Option<JoinHandle<Capture>>,
    stderr: Option<JoinHandle<Capture>>,
    oversized: Arc<std::sync::atomic::AtomicBool>,
}

struct Capture {
    bytes: Vec<u8>,
    failed: bool,
}

fn spawn_captured(request: ToolRequest, cap: usize) -> Result<CapturedChild, PlatformFailure> {
    let mut child = command_for(request)
        .spawn()
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
    let oversized = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_thread = drain(stdout, cap, Arc::clone(&oversized));
    let stderr_thread = drain(stderr, cap, Arc::clone(&oversized));
    Ok(CapturedChild {
        child,
        stdout: Some(stdout_thread),
        stderr: Some(stderr_thread),
        oversized,
    })
}

fn drain<R: Read + Send + 'static>(
    mut reader: R,
    cap: usize,
    oversized: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<Capture> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 8_192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    let remaining = cap.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&chunk[..count.min(remaining)]);
                    if count > remaining {
                        oversized.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                Err(_) => {
                    return Capture {
                        bytes,
                        failed: true,
                    };
                }
            }
        }
        Capture {
            bytes,
            failed: false,
        }
    })
}

fn wait_for_output(
    mut child: CapturedChild,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<ToolOutput, PlatformFailure> {
    loop {
        if cancellation.is_cancelled() {
            child
                .terminate(cleanup_deadline()?)
                .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
            return Err(failure(PlatformFailureKind::Cancelled));
        }
        if remaining(deadline).is_err() {
            child
                .terminate(cleanup_deadline()?)
                .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
            return Err(failure(PlatformFailureKind::TimedOut));
        }
        if let Some(status) = child
            .child
            .try_wait()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?
        {
            let (stdout, stderr) = child.join_captures()?;
            return Ok(ToolOutput {
                status,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
                oversized: child.oversized.load(std::sync::atomic::Ordering::Acquire)
                    || stdout.failed
                    || stderr.failed,
            });
        }
        thread::sleep(REAP_POLL);
    }
}

impl CapturedChild {
    fn join_captures(&mut self) -> Result<(Capture, Capture), PlatformFailure> {
        let Some(stdout) = self.stdout.take() else {
            return Ok((
                Capture {
                    bytes: Vec::new(),
                    failed: false,
                },
                Capture {
                    bytes: Vec::new(),
                    failed: false,
                },
            ));
        };
        let stderr = self
            .stderr
            .take()
            .ok_or_else(|| failure(PlatformFailureKind::Internal))?;
        let stdout = stdout
            .join()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        let stderr = stderr
            .join()
            .map_err(|_| failure(PlatformFailureKind::Internal))?;
        Ok((stdout, stderr))
    }

    fn signal_term(&self) -> Result<(), PlatformFailure> {
        let pid = i32::try_from(self.child.id())
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        // SAFETY: `pid` is the exact child returned by Command::spawn. No
        // negative/process-group identifier or discovered PID is ever used.
        let result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(failure(PlatformFailureKind::CleanupFailed))
        }
    }

    fn wait_until(&mut self, deadline: Instant) -> Result<bool, PlatformFailure> {
        loop {
            if self
                .child
                .try_wait()
                .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?
                .is_some()
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(REAP_POLL);
        }
    }
}

impl ToolChild for CapturedChild {
    fn terminate(&mut self, deadline: AbsoluteDeadline) -> Result<(), PlatformFailure> {
        if self
            .child
            .try_wait()
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?
            .is_none()
        {
            self.signal_term()?;
            let hard_deadline = Instant::now() + remaining(deadline).unwrap_or_default();
            let graceful_deadline = (Instant::now() + TERM_GRACE).min(hard_deadline);
            if !self.wait_until(graceful_deadline)? {
                self.child
                    .kill()
                    .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
                if !self.wait_until(hard_deadline)? {
                    return Err(failure(PlatformFailureKind::CleanupFailed));
                }
            }
        }
        let _ = self
            .join_captures()
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        Ok(())
    }
}

fn run_success(
    runner: &dyn ToolRunner,
    request: ToolRequest,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<ToolOutput, PlatformFailure> {
    let output = runner.run(request, TOOL_OUTPUT_CAP, cancellation, deadline)?;
    if output.oversized || !output.status.success() {
        Err(failure(PlatformFailureKind::Rejected))
    } else {
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
