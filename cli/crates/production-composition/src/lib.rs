//! Private production composition for the installed AppPilotKit executables.
//!
//! This crate is deliberately the only place where the public CLI contract,
//! current-user Broker and platform adapters meet.  It adds no CLI command or
//! protocol surface: all Broker control traffic is the frozen CBOR contract
//! exported by `apppilotkit-host-runtime`.

use apppilotkit_cli_contract::{
    CatalogDispatchPhase, CatalogExchangeError, CatalogExchangeFailure, CatalogRuntime,
    CatalogSelectError, OpenedProtocolSession, SessionSelector,
};
use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, LaunchEndpoint, PendingLaunch, PlatformFailure,
    PlatformFailureKind, PlatformTargetAdapter, PublicLaunchDescriptor, TargetSelection,
};
use apppilotkit_host_runtime::{
    BrokerInstance, CloseReason, ControlFailure, ControlPacketDecoder, ControlRequest,
    ControlResult, ControlSuccess, ErrorKind, ExchangeBody, OpenSessionBody, Platform, PrepareBody,
    ReadyReference, Request, RuntimePaths, SessionBroker, SideEffect, decode_result_packet,
    encode_failure_packet, encode_request_packet, encode_success_packet,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag as signal_flag,
};
use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "internal-diagnostics")]
mod internal_diagnostics {
    use super::{CloseReason, ControlFailure, File, Mutex, Value, Write};
    use apppilotkit_host_runtime::ErrorStage;
    use std::{
        env,
        os::fd::{FromRawFd, RawFd},
        sync::OnceLock,
    };

    const PREPARE_FAILURE_FD: &str = "APPPILOTKIT_INTERNAL_PREPARE_FAILURE_FD";

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PrepareFailureDiagnostic {
        stage: &'static str,
        close_reason: &'static str,
        reason_code: &'static str,
    }

    impl PrepareFailureDiagnostic {
        fn from_failure(failure: &ControlFailure) -> Self {
            let stage = match failure.stage {
                ErrorStage::Ipc => "ipc",
                ErrorStage::Prepare => "prepare",
                ErrorStage::Bootstrap => "bootstrap",
                ErrorStage::SessionHandshake => "session_handshake",
                ErrorStage::SessionOpen => "session_open",
                ErrorStage::Exchange => "exchange",
                ErrorStage::Close => "close",
                ErrorStage::Cleanup => "cleanup",
            };
            let close_reason = match failure.close_reason {
                CloseReason::Normal => "normal",
                CloseReason::AuthenticationFailed => "authentication_failed",
                CloseReason::BindingMismatch => "binding_mismatch",
                CloseReason::Stale => "stale",
                CloseReason::Timeout => "timeout",
                CloseReason::Oversize => "oversize",
                CloseReason::Malformed => "malformed",
                CloseReason::SequenceViolation => "sequence_violation",
                CloseReason::RecordLimit => "record_limit",
                CloseReason::PeerClosed => "peer_closed",
                CloseReason::BrokerLost => "broker_lost",
                CloseReason::EligibilityLost => "eligibility_lost",
                CloseReason::CleanupFailed => "cleanup_failed",
                CloseReason::InternalError => "internal_error",
            };
            let reason_code = match (failure.stage, failure.close_reason) {
                // The private control result does not preserve which Prepare
                // branch produced BindingMismatch, so this code deliberately
                // groups an in-progress Prepare with other prepare bindings.
                (ErrorStage::Prepare, CloseReason::BindingMismatch) => "prepare_binding_mismatch",
                (_, CloseReason::Stale | CloseReason::EligibilityLost) => "lease_stale",
                (_, CloseReason::BrokerLost) => "broker_lost",
                (_, CloseReason::BindingMismatch) => "binding_mismatch",
                (_, CloseReason::PeerClosed) => "peer_closed",
                (_, CloseReason::Malformed | CloseReason::SequenceViolation) => "malformed",
                (_, CloseReason::Timeout) => "timeout",
                (_, CloseReason::InternalError | CloseReason::CleanupFailed) => "internal",
                _ => "other",
            };
            Self {
                stage,
                close_reason,
                reason_code,
            }
        }

        fn as_value(self) -> Value {
            serde_json::json!({
                "stage": self.stage,
                "close_reason": self.close_reason,
                "reason_code": self.reason_code,
            })
        }
    }

    fn sink() -> Option<&'static Mutex<File>> {
        static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
        SINK.get_or_init(|| {
            let fd = env::var(PREPARE_FAILURE_FD)
                .ok()
                .and_then(|value| value.parse::<RawFd>().ok())
                .filter(|fd| *fd > 2)?;
            // Diagnostics may never delay the helper's public result.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return None;
            }
            // The Debug/Internal launcher owns this inherited descriptor.
            Some(Mutex::new(unsafe { File::from_raw_fd(fd) }))
        })
        .as_ref()
    }

    pub(super) fn record_prepare_failure(failure: &ControlFailure) {
        let Some(sink) = sink() else { return };
        let Ok(mut sink) = sink.lock() else { return };
        let _ = write_diagnostic(&mut *sink, PrepareFailureDiagnostic::from_failure(failure));
    }

    fn write_diagnostic(
        writer: &mut dyn Write,
        diagnostic: PrepareFailureDiagnostic,
    ) -> std::io::Result<()> {
        serde_json::to_writer(&mut *writer, &diagnostic.as_value())?;
        writer.write_all(b"\n")
    }

    #[cfg(test)]
    pub(super) fn diagnostic_value(failure: &ControlFailure) -> Value {
        PrepareFailureDiagnostic::from_failure(failure).as_value()
    }

    #[cfg(test)]
    pub(super) fn diagnostic_bytes(failure: &ControlFailure) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_diagnostic(&mut bytes, PrepareFailureDiagnostic::from_failure(failure))
            .expect("diagnostic writes to memory");
        bytes
    }
}

#[cfg(feature = "internal-diagnostics")]
fn record_prepare_failure(failure: &ControlFailure) {
    internal_diagnostics::record_prepare_failure(failure);
}

#[cfg(not(feature = "internal-diagnostics"))]
fn record_prepare_failure(_: &ControlFailure) {}

const IPC_READ_CAP: usize = 67_109_124;
const IPC_BUFFER: usize = 16 * 1024;
const IPC_STARTUP_BUDGET: Duration = Duration::from_millis(1_000);
const IPC_READ_POLL_INTERVAL: Duration = Duration::from_millis(10);
// Android `prepare` has a 20-second install/start/forward/connect phase and
// a separate 10-second connected bootstrap phase. This maximum is also the
// private IPC read budget; iOS remains bounded by its shorter platform budget.
const OPERATION_BUDGET_MS: u64 = 30_000;
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(100);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SESSION_OPEN_CAPABILITY: &str = "semantic.catalog";

#[derive(Clone)]
pub struct BrokerControlClient {
    connector: Arc<dyn ControlConnector>,
    clock: Arc<dyn MonotonicClock>,
}

trait ControlConnector: Send + Sync {
    fn connect(&self) -> io::Result<UnixStream>;
}

trait MonotonicClock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemMonotonicClock;

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Default)]
struct CurrentUserConnector {
    broker_path: Option<PathBuf>,
}

impl CurrentUserConnector {
    fn new(broker_path: Option<PathBuf>) -> Self {
        Self { broker_path }
    }
}

impl ControlConnector for CurrentUserConnector {
    fn connect(&self) -> io::Result<UnixStream> {
        match RuntimePaths::current_user()?.connect_verified() {
            Ok(stream) => Ok(stream),
            Err(initial) => {
                let Some(broker_path) = self.broker_path.as_ref() else {
                    return Err(initial);
                };
                // The sidecar receives only its fixed serve flag.  In
                // particular, no Target locator or secret crosses argv/env.
                Command::new(broker_path)
                    .arg("--serve")
                    .env_clear()
                    .envs(android_host_environment())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
                let until = std::time::Instant::now() + IPC_STARTUP_BUDGET;
                loop {
                    match RuntimePaths::current_user()?.connect_verified() {
                        Ok(stream) => return Ok(stream),
                        Err(_) if std::time::Instant::now() < until => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return Err(initial),
                    }
                }
            }
        }
    }
}

/// Preserve only Android host configuration for an env-cleared sidecar. The
/// sidecar validates and resolves it only if an Android target is prepared.
fn android_host_environment() -> impl Iterator<Item = (OsString, OsString)> {
    [
        "APPPILOTKIT_ANDROID_ADB",
        "ANDROID_SDK_ROOT",
        "ANDROID_HOME",
        "HOME",
    ]
    .into_iter()
    .filter_map(|name| env::var_os(name).map(|value| (OsString::from(name), value)))
}

impl BrokerControlClient {
    pub fn current_user() -> Self {
        Self {
            connector: Arc::new(CurrentUserConnector::new(installed_broker_path())),
            clock: Arc::new(SystemMonotonicClock),
        }
    }

    #[cfg(test)]
    fn with_connector(connector: Arc<dyn ControlConnector>) -> Self {
        Self::with_connector_and_clock(connector, Arc::new(SystemMonotonicClock))
    }

    #[cfg(test)]
    fn with_connector_and_clock(
        connector: Arc<dyn ControlConnector>,
        clock: Arc<dyn MonotonicClock>,
    ) -> Self {
        Self { connector, clock }
    }

    fn call(&self, request: ControlRequest) -> Result<ControlSuccess, ControlFailure> {
        let deadline = control_deadline(request.deadline_unix_ms());
        self.call_until(request, deadline)
    }

    fn call_until(
        &self,
        request: ControlRequest,
        deadline: Instant,
    ) -> Result<ControlSuccess, ControlFailure> {
        let mut stream = self.connector.connect().map_err(io_failure)?;
        let packet = encode_request_packet(&request)?;
        write_control_packet_with_clock(&mut stream, &packet, deadline, self.clock.as_ref())
            .map_err(io_failure)?;
        stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(io_failure)?;
        let result = read_control_result_with_clock(&mut stream, deadline, self.clock.as_ref())?;
        match result {
            ControlResult::Success { request_id, result } if request_id == request.request_id() => {
                Ok(result)
            }
            ControlResult::Failure { request_id, error } if request_id == request.request_id() => {
                Err(error)
            }
            _ => Err(ControlFailure::ipc(
                apppilotkit_host_runtime::CloseReason::BindingMismatch,
            )),
        }
    }
}

/// Bounds an IPC operation by both its protocol deadline and the local maximum
/// operation budget.  The protocol deadline is wall-clock based, while the
/// socket operations need a monotonic deadline.
fn control_deadline(deadline_unix_ms: u64) -> Instant {
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let remaining_ms = u64::try_from(now_unix_ms)
        .ok()
        .and_then(|now| deadline_unix_ms.checked_sub(now))
        .unwrap_or_default()
        .min(OPERATION_BUDGET_MS);
    Instant::now()
        .checked_add(Duration::from_millis(remaining_ms))
        .unwrap_or_else(Instant::now)
}

fn remaining_timeout_with_clock(
    deadline: Instant,
    clock: &dyn MonotonicClock,
) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(clock.now());
    // macOS socket timeouts reject sub-millisecond precision. Rounding down
    // keeps every read and write inside the shared absolute deadline.
    let milliseconds = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
    if milliseconds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Broker control deadline expired",
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

/// Write one exact control packet without allowing partial progress to extend
/// the operation deadline.  macOS applies the timeout to an individual socket
/// write, so it must be recomputed after every partial write.
fn write_control_packet(
    stream: &mut UnixStream,
    packet: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    write_control_packet_with_clock(stream, packet, deadline, &SystemMonotonicClock)
}

fn write_control_packet_with_clock(
    stream: &mut UnixStream,
    mut packet: &[u8],
    deadline: Instant,
    clock: &dyn MonotonicClock,
) -> io::Result<()> {
    while !packet.is_empty() {
        stream.set_write_timeout(Some(remaining_timeout_with_clock(deadline, clock)?))?;
        match stream.write(packet) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Broker control packet write made no progress",
                ));
            }
            Ok(written) => packet = &packet[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn installed_broker_path() -> Option<PathBuf> {
    let executable = env::current_exe().ok()?;
    let parent = executable.parent()?;
    let prefix = match parent.file_name()?.to_str()? {
        "bin" => parent.parent()?,
        "libexec" => parent.parent()?,
        _ => return None,
    };
    Some(prefix.join("libexec").join("apppilotkit-broker"))
}

fn io_failure(error: io::Error) -> ControlFailure {
    let reason = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => CloseReason::Timeout,
        _ => CloseReason::InternalError,
    };
    ControlFailure::ipc(reason)
}

fn read_control_result_with_clock(
    stream: &mut UnixStream,
    deadline: Instant,
    clock: &dyn MonotonicClock,
) -> Result<ControlResult, ControlFailure> {
    // macOS rejects some valid-looking dynamically recomputed SO_RCVTIMEO
    // values. Nonblocking reads plus a monotonic deadline avoid granting a
    // second fixed read budget after the request write has consumed time.
    stream.set_nonblocking(true).map_err(io_failure)?;
    let mut packet = Vec::new();
    let mut buffer = [0_u8; IPC_BUFFER];
    loop {
        let remaining = remaining_timeout_with_clock(deadline, clock).map_err(io_failure)?;
        let read = match stream.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(remaining.min(IPC_READ_POLL_INTERVAL));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(io_failure)?,
        };
        if read == 0 {
            break;
        }
        if packet.len().saturating_add(read) > IPC_READ_CAP {
            return Err(ControlFailure::ipc(
                apppilotkit_host_runtime::CloseReason::Oversize,
            ));
        }
        packet.extend_from_slice(&buffer[..read]);
    }
    decode_result_packet(&packet)
}

/// Serve one authenticated current-user IPC connection.
///
/// The `BrokerInstance` listener itself performs peer-euid checks.  This
/// function then decodes one exact CBOR request and writes one exact result.
pub fn serve_connection(stream: &mut UnixStream, broker: &SessionBroker) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(OPERATION_BUDGET_MS)))?;
    let mut decoder = ControlPacketDecoder::new();
    let mut buffer = [0_u8; IPC_BUFFER];
    let request = loop {
        match stream.read(&mut buffer) {
            Ok(0) => return decoder.eof().map(|_| ()).map_err(control_io),
            Ok(read) => match decoder.push(&buffer[..read]) {
                Ok(Some(request)) => break request,
                Ok(None) => continue,
                Err(_) => return Ok(()),
            },
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                let _ = decoder.timeout();
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    };
    let request_id = request.request_id();
    let deadline = control_deadline(request.deadline_unix_ms());
    let packet = match broker.handle(request) {
        Ok(success) => encode_success_packet(request_id, success),
        Err(error) => encode_failure_packet(request_id, &error),
    }
    .map_err(control_io)?;
    write_control_packet(stream, &packet, deadline)
}

fn control_io(_: ControlFailure) -> io::Error {
    io::Error::other("Broker control packet rejected")
}

pub fn run_broker() -> io::Result<()> {
    let instance = BrokerInstance::acquire_current_user()?;
    // Polling keeps shutdown bounded even when SIGTERM is delivered to a
    // worker rather than the thread that would otherwise block in accept.
    instance.set_nonblocking(true)?;
    let broker = Arc::new(
        SessionBroker::new(
            Arc::new(apppilotkit_apple_simulator_adapter::AppleSimulatorAdapter::default()),
            Arc::new(LazyAndroidAdapter::production()),
        )
        .map_err(control_io)?,
    );
    let stop = Arc::new(AtomicBool::new(false));
    install_shutdown_signals(Arc::clone(&stop))?;
    let maintenance = spawn_maintenance(Arc::clone(&broker), Arc::clone(&stop));
    let serving_broker = Arc::clone(&broker);
    let shutdown_stop = Arc::clone(&stop);
    run_broker_accept_loop(
        || instance.accept_verified(),
        stop.as_ref(),
        move |mut stream| {
            let broker = Arc::clone(&serving_broker);
            thread::spawn(move || {
                let _ = serve_connection(&mut stream, broker.as_ref());
            });
        },
        move || {
            shutdown_stop.store(true, Ordering::Release);
            let _ = maintenance.join();
            broker.shutdown(CloseReason::BrokerLost).map_err(control_io)
        },
    )
}

/// Resolves Android host tooling only after an Android target was selected.
/// This keeps an iOS-only Broker available on hosts without an Android SDK.
struct LazyAndroidAdapter {
    resolve_adb: Arc<dyn Fn() -> io::Result<PathBuf> + Send + Sync>,
}

impl LazyAndroidAdapter {
    fn production() -> Self {
        Self {
            resolve_adb: Arc::new(android_adb_path),
        }
    }

    #[cfg(test)]
    fn with_resolver(
        resolve_adb: impl Fn() -> io::Result<PathBuf> + Send + Sync + 'static,
    ) -> Self {
        Self {
            resolve_adb: Arc::new(resolve_adb),
        }
    }
}

impl PlatformTargetAdapter for LazyAndroidAdapter {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        deadline: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch> {
        match (self.resolve_adb)() {
            Ok(adb) => apppilotkit_android_emulator_adapter::AndroidEmulatorAdapter::new(adb)
                .begin_launch(selection, deadline),
            // Adapter resolution is host configuration, not target input. Do
            // not admit an Android launch when it cannot be resolved.
            Err(_) => Box::new(UnavailableAndroidLaunch::new()),
        }
    }
}

struct UnavailableAndroidLaunch {
    endpoint: LaunchEndpoint,
}

impl UnavailableAndroidLaunch {
    fn new() -> Self {
        Self {
            endpoint: LaunchEndpoint::android_local_abstract(
                "apppilotkit-android-adb-unavailable".to_owned(),
            )
            .unwrap_or_else(|_| unreachable!("constant Android endpoint is valid")),
        }
    }
}

impl PendingLaunch for UnavailableAndroidLaunch {
    fn endpoint(&self) -> &LaunchEndpoint {
        &self.endpoint
    }

    fn launch(
        self: Box<Self>,
        _descriptor: PublicLaunchDescriptor,
        _cancellation: Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<apppilotkit_host_runtime::adapter::LaunchedTargetIo, PlatformFailure> {
        Err(PlatformFailure::new(PlatformFailureKind::Unavailable))
    }

    fn abort(
        self: Box<Self>,
        _cancellation: Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        Ok(())
    }
}

/// Registers only a flag-setting signal action.  `signal-hook` keeps the
/// handler async-signal-safe; cleanup remains on the Broker's serving thread.
/// Repeated SIGINT/SIGTERM coalesce into the same graceful shutdown. SIGKILL
/// and crash signals are intentionally left to their normal process behavior.
fn install_shutdown_signals(stop: Arc<AtomicBool>) -> io::Result<()> {
    signal_flag::register(SIGINT, Arc::clone(&stop))?;
    signal_flag::register(SIGTERM, stop)?;
    Ok(())
}

/// Runs the accepting half of the Broker lifetime and invokes the finalizer
/// exactly once after that loop exits.  Keeping this boundary small makes the
/// production shutdown ordering testable without a real current-user socket.
fn run_broker_accept_loop(
    mut accept: impl FnMut() -> io::Result<UnixStream>,
    stop: &AtomicBool,
    mut serve: impl FnMut(UnixStream),
    shutdown: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let accept_result = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        match accept() {
            // A signal may arrive after the pre-accept check. Do not admit
            // another request once graceful shutdown has started.
            Ok(_stream) if stop.load(Ordering::Acquire) => break Ok(()),
            Ok(stream) => serve(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error)
                if error.kind() == io::ErrorKind::Interrupted && stop.load(Ordering::Acquire) =>
            {
                break Ok(());
            }
            Err(error) => break Err(error),
        }
    };
    let shutdown_result = shutdown();
    match (accept_result, shutdown_result) {
        (_, Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn android_adb_path() -> io::Result<PathBuf> {
    if let Some(adb) = env::var_os("APPPILOTKIT_ANDROID_ADB") {
        return resolve_android_adb_executable(Path::new(&adb));
    }
    let sdk_root = env::var_os("ANDROID_SDK_ROOT")
        .or_else(|| env::var_os("ANDROID_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join("Library/Android/sdk"))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Android SDK root unavailable"))?;
    resolve_android_adb_path(&sdk_root)
}

fn resolve_android_adb_path(sdk_root: &Path) -> io::Result<PathBuf> {
    if !sdk_root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Android SDK root must be absolute",
        ));
    }
    resolve_android_adb_executable(&sdk_root.join("platform-tools/adb"))
}

fn resolve_android_adb_executable(adb: &Path) -> io::Result<PathBuf> {
    if !adb.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Android adb path must be absolute",
        ));
    }
    let adb = fs::canonicalize(adb)?;
    let metadata = fs::metadata(&adb)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Android SDK adb must be an executable regular file",
        ));
    }
    Ok(adb)
}

fn spawn_maintenance(broker: Arc<SessionBroker>, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    spawn_maintenance_task(stop, move || broker.maintain())
}

fn spawn_maintenance_task(
    stop: Arc<AtomicBool>,
    mut maintain: impl FnMut() -> Result<(), ControlFailure> + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || run_maintenance_loop(&stop, &mut maintain))
}

fn run_maintenance_loop(
    stop: &AtomicBool,
    mut maintain: impl FnMut() -> Result<(), ControlFailure>,
) {
    while !stop.load(Ordering::Acquire) {
        // `maintain` owns heartbeat, session/lease expiry, and reaping.  A
        // terminal lease failure is recorded by HostRuntime; this loop must
        // keep serving independent leases instead of terminating the UDS.
        let _ = maintain();
        thread::sleep(MAINTENANCE_INTERVAL);
    }
}

#[derive(Clone)]
struct SessionBindingState {
    opened: OpenedProtocolSession,
    listener_epoch: u64,
}

/// Private bridge from the frozen two-method `CatalogRuntime` to Broker IPC.
pub struct BrokerCatalogRuntime {
    client: BrokerControlClient,
    sessions: Mutex<HashMap<(String, String), SessionBindingState>>,
    next_request: AtomicU64,
}

impl BrokerCatalogRuntime {
    pub fn current_user() -> Self {
        Self::new(BrokerControlClient::current_user())
    }

    pub fn new(client: BrokerControlClient) -> Self {
        Self {
            client,
            sessions: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
        }
    }

    fn next_control_request<T>(&self, body: T) -> Request<T> {
        let sequence = self.next_request.fetch_add(1, Ordering::Relaxed);
        Request {
            request_id: sequence
                .to_be_bytes()
                .repeat(2)
                .try_into()
                .expect("two u64 values"),
            deadline_unix_ms: deadline(),
            body,
        }
    }

    fn open_session(
        &self,
        target: ReadyReference,
        target_text: &str,
        requested: Option<&str>,
    ) -> Result<OpenedProtocolSession, CatalogSelectError> {
        let (session_id, session_open_request, session_open_request_sha256, expected_open_id) =
            match requested {
                Some(session) => (Some(session.to_owned()), None, None, None),
                None => {
                    let id = format!("open-{}", self.next_request.load(Ordering::Relaxed));
                    let bytes = serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "session.open",
                    "params": {
                        "client": { "name": "apppilotkit", "version": env!("CARGO_PKG_VERSION") },
                        "protocol": { "major": 1, "minMinor": 2, "maxMinor": 2 },
                        "requiredCapabilities": [SESSION_OPEN_CAPABILITY],
                    }
                }))
                .map_err(|_| CatalogSelectError::SessionExpired)?;
                    let digest: [u8; 32] = Sha256::digest(&bytes).into();
                    (None, Some(bytes), Some(digest), Some(id))
                }
            };
        let request = ControlRequest::OpenSession(self.next_control_request(OpenSessionBody {
            target_token: target.token(),
            session_id,
            required_capabilities: vec![SESSION_OPEN_CAPABILITY.to_owned()],
            session_open_request,
            session_open_request_sha256,
        }));
        let opened = match self.client.call(request) {
            Ok(ControlSuccess::SessionOpened(opened)) if opened.target_token == target.token() => {
                opened
            }
            Ok(_) => return Err(CatalogSelectError::SessionExpired),
            Err(error) => return Err(select_error(error)),
        };
        if opened.response_sha256 != <[u8; 32]>::from(Sha256::digest(&opened.response)) {
            return Err(CatalogSelectError::SessionExpired);
        }
        let session = parse_opened_session(
            &opened.response,
            target_text,
            opened.process_generation,
            expected_open_id.as_deref(),
        )
        .ok_or(CatalogSelectError::SessionExpired)?;
        let key = (target_text.to_owned(), session.session_id.clone());
        self.sessions.lock().expect("Broker session state").insert(
            key,
            SessionBindingState {
                opened: session.clone(),
                listener_epoch: opened.listener_epoch,
            },
        );
        Ok(session)
    }
}

impl CatalogRuntime for BrokerCatalogRuntime {
    fn select(
        &self,
        selector: SessionSelector<'_>,
    ) -> Result<OpenedProtocolSession, CatalogSelectError> {
        let Some(target_text) = selector.target else {
            return Err(CatalogSelectError::SessionSelectionRequired {
                candidates: Vec::new(),
            });
        };
        let target = ReadyReference::parse(target_text).map_err(|_| {
            CatalogSelectError::TargetSelectionRequired {
                candidates: Vec::new(),
            }
        })?;
        self.open_session(target, target_text, selector.session)
    }

    fn exchange(
        &self,
        session: &OpenedProtocolSession,
        request: &Value,
    ) -> Result<Vec<u8>, CatalogExchangeError> {
        let target = ReadyReference::parse(&session.target_id)
            .map_err(|_| exchange_error(None, CatalogExchangeFailure::TransportInternal))?;
        let binding = self
            .sessions
            .lock()
            .expect("Broker session state")
            .get(&(session.target_id.clone(), session.session_id.clone()))
            .cloned()
            .filter(|binding| binding.opened == *session)
            .ok_or_else(|| exchange_error(None, CatalogExchangeFailure::SessionExpired))?;
        let message = serde_json::to_vec(request)
            .map_err(|_| exchange_error(None, CatalogExchangeFailure::TransportInternal))?;
        let side_effect = match request.get("method").and_then(Value::as_str) {
            Some("semantic.invoke") => SideEffect::AppMutation,
            Some("semantic.list" | "semantic.show" | "semantic.schema" | "semantic.query") => {
                SideEffect::ReadOnly
            }
            _ => {
                return Err(exchange_error(
                    None,
                    CatalogExchangeFailure::TransportInternal,
                ));
            }
        };
        let request = ControlRequest::Exchange(self.next_control_request(ExchangeBody {
            target_token: target.token(),
            session_id: session.session_id.clone(),
            process_generation: session.generation,
            listener_epoch: binding.listener_epoch,
            message: message.clone(),
            message_sha256: Sha256::digest(&message).into(),
            side_effect,
        }));
        match self.client.call(request) {
            Ok(ControlSuccess::ExchangeComplete(result))
                if result.target_token == target.token()
                    && result.session_id == session.session_id
                    && result.process_generation == session.generation
                    && result.listener_epoch == binding.listener_epoch
                    && result.message_sha256
                        == <[u8; 32]>::from(Sha256::digest(&result.message)) =>
            {
                Ok(result.message)
            }
            Ok(_) => Err(exchange_error(None, CatalogExchangeFailure::SessionExpired)),
            Err(error) => Err(exchange_error(
                Some(error),
                CatalogExchangeFailure::TransportInternal,
            )),
        }
    }
}

fn select_error(error: ControlFailure) -> CatalogSelectError {
    match error.kind {
        ErrorKind::TargetSelectionRequired => CatalogSelectError::TargetSelectionRequired {
            candidates: Vec::new(),
        },
        ErrorKind::TransportAuthenticationRequired => CatalogSelectError::AuthenticationRequired,
        ErrorKind::SessionExpired | ErrorKind::Timeout | ErrorKind::InternalError => {
            CatalogSelectError::SessionExpired
        }
    }
}

fn exchange_error(
    error: Option<ControlFailure>,
    fallback: CatalogExchangeFailure,
) -> CatalogExchangeError {
    let Some(error) = error else {
        return CatalogExchangeError::pre_dispatch(fallback);
    };
    let phase =
        if error.handoff == apppilotkit_host_runtime::HandoffState::HandoffPossibleOrConfirmed {
            CatalogDispatchPhase::PostDispatch
        } else {
            CatalogDispatchPhase::PreDispatch
        };
    let failure = match error.kind {
        ErrorKind::Timeout => CatalogExchangeFailure::Timeout,
        ErrorKind::TransportAuthenticationRequired => {
            CatalogExchangeFailure::AuthenticationRequired
        }
        ErrorKind::SessionExpired | ErrorKind::TargetSelectionRequired => {
            CatalogExchangeFailure::SessionExpired
        }
        ErrorKind::InternalError => CatalogExchangeFailure::TransportInternal,
    };
    CatalogExchangeError { phase, failure }
}

fn parse_opened_session(
    response: &[u8],
    target: &str,
    generation: u64,
    expected_open_id: Option<&str>,
) -> Option<OpenedProtocolSession> {
    let value: Value = serde_json::from_slice(response).ok()?;
    if value.get("jsonrpc")?.as_str()? != "2.0"
        || expected_open_id
            .is_some_and(|expected| value.get("id").and_then(Value::as_str) != Some(expected))
    {
        return None;
    }
    let result = value.get("result")?;
    let context = result.get("context")?;
    let limits = result.get("limits")?;
    let protocol = result.get("protocol")?;
    let session_id = context.get("id")?.as_str()?.to_owned();
    let response_generation = context.get("generation")?.as_u64()?;
    let protocol_major = protocol.get("major")?.as_u64()?.try_into().ok()?;
    let protocol_minor = protocol.get("minor")?.as_u64()?.try_into().ok()?;
    let capabilities = result
        .get("capabilities")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    Some(OpenedProtocolSession {
        session_id,
        generation: response_generation,
        target_id: target.to_owned(),
        protocol_major,
        protocol_minor,
        capabilities,
        max_request_bytes: limits.get("maxRequestBytes")?.as_u64()?.try_into().ok()?,
        max_response_bytes: limits.get("maxResponseBytes")?.as_u64()?.try_into().ok()?,
        max_page_items: limits.get("maxPageItems")?.as_u64()?.try_into().ok()?,
    })
    .filter(|session| session.generation == generation)
}

fn deadline() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(now)
        .unwrap_or(u64::MAX - OPERATION_BUDGET_MS)
        .saturating_add(OPERATION_BUDGET_MS)
}

#[derive(Debug)]
pub enum PrepareError {
    InvalidInvocation,
    Io,
    Broker(ControlFailure),
}

pub struct PreparedTarget {
    pub target: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

/// Executes the setup-only helper request after parsing its non-secret JSON.
pub fn prepare_target(
    input: &[u8],
    client: &BrokerControlClient,
) -> Result<PreparedTarget, PrepareError> {
    let value: Value =
        serde_json::from_slice(input).map_err(|_| PrepareError::InvalidInvocation)?;
    let object = value.as_object().ok_or(PrepareError::InvalidInvocation)?;
    if object.len() != 6 || object.get("schema_version").and_then(Value::as_str) != Some("1.0") {
        return Err(PrepareError::InvalidInvocation);
    }
    let platform = match object.get("platform").and_then(Value::as_str) {
        Some("ios-simulator") => Platform::IosSimulator,
        Some("android-emulator") => Platform::AndroidEmulator,
        _ => return Err(PrepareError::InvalidInvocation),
    };
    let selector = safe_selector(object.get("device_selector").and_then(Value::as_str))?;
    let app_id = safe_app_id(object.get("app_id").and_then(Value::as_str))?;
    let artifact = absolute_path(object.get("app_artifact").and_then(Value::as_str))?;
    let encoding = object.get("artifact_encoding").and_then(Value::as_str);
    let digest = match (platform, encoding) {
        (Platform::AndroidEmulator, Some("raw-file-v1")) => {
            digest_regular_file(Path::new(artifact))?
        }
        (Platform::IosSimulator, Some("ios-app-tree-v1")) => {
            let deadline = AbsoluteDeadline::new(deadline()).map_err(|_| PrepareError::Io)?;
            apppilotkit_apple_simulator_adapter::inspect_ios_app_tree_digest(
                Path::new(artifact),
                app_id,
                &Cancellation::new(),
                deadline,
            )
            .map_err(|failure| match failure.kind() {
                PlatformFailureKind::Rejected => PrepareError::InvalidInvocation,
                _ => PrepareError::Io,
            })?
        }
        _ => return Err(PrepareError::InvalidInvocation),
    };
    let request = ControlRequest::Prepare(Request {
        request_id: [0x50; 16],
        deadline_unix_ms: deadline(),
        body: PrepareBody {
            platform,
            device_selector: selector.to_owned(),
            app_id: app_id.to_owned(),
            app_artifact: artifact.to_owned(),
            app_artifact_sha256: digest,
        },
    });
    match client.call(request).map_err(PrepareError::Broker)? {
        ControlSuccess::TargetReady(ready) => Ok(PreparedTarget {
            target: ReadyReference::from_token(ready.target_token).to_string(),
            issued_at_unix_ms: ready.issued_at_unix_ms,
            expires_at_unix_ms: ready.expires_at_unix_ms,
        }),
        _ => Err(PrepareError::Broker(ControlFailure::ipc(
            apppilotkit_host_runtime::CloseReason::BindingMismatch,
        ))),
    }
}

fn safe_selector(value: Option<&str>) -> Result<&str, PrepareError> {
    let value = value.ok_or(PrepareError::InvalidInvocation)?;
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(PrepareError::InvalidInvocation);
    }
    Ok(value)
}
fn safe_app_id(value: Option<&str>) -> Result<&str, PrepareError> {
    let value = value.ok_or(PrepareError::InvalidInvocation)?;
    if !(3..=255).contains(&value.len())
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PrepareError::InvalidInvocation);
    }
    Ok(value)
}
fn absolute_path(value: Option<&str>) -> Result<&str, PrepareError> {
    let value = value.ok_or(PrepareError::InvalidInvocation)?;
    if !(2..=4096).contains(&value.len())
        || !value.starts_with('/')
        || value.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
    {
        return Err(PrepareError::InvalidInvocation);
    }
    Ok(value)
}
fn digest_regular_file(path: &Path) -> Result<[u8; 32], PrepareError> {
    let metadata = path.metadata().map_err(|_| PrepareError::Io)?;
    if !metadata.is_file() {
        return Err(PrepareError::InvalidInvocation);
    }
    let mut file = File::open(path).map_err(|_| PrepareError::Io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| PrepareError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

pub fn layout_paths(prefix: &Path) -> [PathBuf; 3] {
    [
        prefix.join("bin").join("apppilotkit"),
        prefix.join("libexec").join("apppilotkit-broker"),
        prefix.join("libexec").join("apppilotkit-target-prepare"),
    ]
}

/// Copy already-built executable artifacts into the immutable installed layout.
/// The caller supplies the Cargo profile directory, keeping build selection out
/// of the installed program's argv and environment.
pub fn stage_install(prefix: &Path, built_bin_dir: &Path) -> io::Result<[PathBuf; 3]> {
    let destinations = layout_paths(prefix);
    let sources = [
        built_bin_dir.join("apppilotkit"),
        built_bin_dir.join("apppilotkit-broker"),
        built_bin_dir.join("apppilotkit-target-prepare"),
    ];
    for source in &sources {
        if !source.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "built executable missing",
            ));
        }
    }
    for destination in &destinations {
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::other("install parent"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;
    }
    for (source, destination) in sources.iter().zip(&destinations) {
        std::fs::copy(source, destination)?;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(destinations)
}

pub fn render_prepare_error(error: &PrepareError) -> Value {
    if let PrepareError::Broker(failure) = error {
        record_prepare_failure(failure);
    }
    let (kind, message, stage) = match error {
        PrepareError::InvalidInvocation => (
            "cli.invalidInvocation",
            "Invalid target prepare invocation.",
            "input",
        ),
        PrepareError::Io => (
            "internalError",
            "Target artifact cannot be read safely.",
            "input",
        ),
        PrepareError::Broker(error) => match error.kind {
            ErrorKind::TargetSelectionRequired => {
                ("target.selectionRequired", error.message, "broker")
            }
            ErrorKind::SessionExpired => ("sessionExpired", error.message, "broker"),
            ErrorKind::TransportAuthenticationRequired => (
                "transport.authenticationRequired",
                error.message,
                "bootstrap",
            ),
            ErrorKind::Timeout => ("timeout", error.message, "broker"),
            ErrorKind::InternalError => ("internalError", error.message, "broker"),
        },
    };
    serde_json::json!({"schema_version":"1.0","status":"failed","error":{"kind":kind,"message":message,"retryable":false,"stage":stage}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use apppilotkit_host_runtime::ErrorStage;
    use apppilotkit_host_runtime::adapter::{
        AbsoluteDeadline, Cancellation, LaunchEndpoint, PendingLaunch, PlatformFailure,
        PlatformFailureKind, PlatformTargetAdapter, PublicLaunchDescriptor, TargetSelection,
    };
    use std::{
        collections::VecDeque,
        os::unix::{fs::PermissionsExt, net::UnixStream},
    };

    fn broker_failure(stage: ErrorStage, close_reason: CloseReason) -> ControlFailure {
        ControlFailure {
            kind: match close_reason {
                CloseReason::Timeout => ErrorKind::Timeout,
                CloseReason::InternalError | CloseReason::CleanupFailed => ErrorKind::InternalError,
                _ => ErrorKind::SessionExpired,
            },
            message: "target-fixture session-fixture token-fixture /private/fixture next-action-fixture opaque-fixture",
            retryable: false,
            stage,
            handoff: apppilotkit_host_runtime::HandoffState::NotHandedOff,
            close_reason,
        }
    }

    #[test]
    fn prepare_failure_rendering_keeps_the_public_contract() {
        let failure = broker_failure(ErrorStage::Prepare, CloseReason::BindingMismatch);
        assert_eq!(
            render_prepare_error(&PrepareError::Broker(failure)),
            serde_json::json!({
                "schema_version": "1.0",
                "status": "failed",
                "error": {
                    "kind": "sessionExpired",
                    "message": "target-fixture session-fixture token-fixture /private/fixture next-action-fixture opaque-fixture",
                    "retryable": false,
                    "stage": "broker",
                }
            })
        );
    }

    #[cfg(feature = "internal-diagnostics")]
    #[test]
    fn internal_prepare_failure_reason_codes_are_closed_and_redacted() {
        let cases = [
            (
                ErrorStage::Prepare,
                CloseReason::BindingMismatch,
                "prepare_binding_mismatch",
            ),
            (
                ErrorStage::Bootstrap,
                CloseReason::BindingMismatch,
                "binding_mismatch",
            ),
            (ErrorStage::Prepare, CloseReason::Stale, "lease_stale"),
            (ErrorStage::Ipc, CloseReason::BrokerLost, "broker_lost"),
            (
                ErrorStage::Bootstrap,
                CloseReason::PeerClosed,
                "peer_closed",
            ),
            (ErrorStage::Prepare, CloseReason::Malformed, "malformed"),
            (ErrorStage::Ipc, CloseReason::Timeout, "timeout"),
            (ErrorStage::Cleanup, CloseReason::InternalError, "internal"),
        ];
        for (stage, close_reason, reason_code) in cases {
            let failure = broker_failure(stage, close_reason);
            let diagnostic = internal_diagnostics::diagnostic_value(&failure);
            assert_eq!(diagnostic["reason_code"], reason_code);
            assert_eq!(diagnostic.as_object().map(|value| value.len()), Some(3));
            let output = String::from_utf8(internal_diagnostics::diagnostic_bytes(&failure))
                .expect("diagnostic UTF-8");
            for marker in [
                "target-fixture",
                "session-fixture",
                "token-fixture",
                "/private/fixture",
                "next-action-fixture",
                "opaque-fixture",
            ] {
                assert!(!output.contains(marker), "diagnostic leaked {marker}");
            }
        }
    }

    mod existing_host_adapter {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../host-runtime/tests/adapter_interface.rs"
        ));

        pub fn successful()
        -> std::sync::Arc<dyn apppilotkit_host_runtime::adapter::PlatformTargetAdapter> {
            std::sync::Arc::new(FakeAdapter {
                cleanup_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                launches: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                connections: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            })
        }
    }

    struct PairConnector(Mutex<Option<UnixStream>>);
    impl ControlConnector for PairConnector {
        fn connect(&self) -> io::Result<UnixStream> {
            self.0
                .lock()
                .expect("pair connector")
                .take()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "used"))
        }
    }

    struct RequestWriteClock {
        before_write: Instant,
        after_write: Instant,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl MonotonicClock for RequestWriteClock {
        fn now(&self) -> Instant {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.before_write
            } else {
                self.after_write
            }
        }
    }

    fn large_exchange_request(message: Vec<u8>) -> ControlRequest {
        ControlRequest::Exchange(Request {
            request_id: [0x73; 16],
            deadline_unix_ms: deadline(),
            body: ExchangeBody {
                target_token: [0x31; 32],
                session_id: "session_test_012345".to_owned(),
                process_generation: 1,
                listener_epoch: 1,
                message_sha256: Sha256::digest(&message).into(),
                message,
                side_effect: SideEffect::ReadOnly,
            },
        })
    }

    #[test]
    fn client_write_to_an_unread_peer_respects_the_operation_deadline() {
        const CANARY: &str = "control-ipc-write-canary";
        let (client_stream, _unread_peer) = UnixStream::pair().expect("pair");
        let client = BrokerControlClient::with_connector(Arc::new(PairConnector(Mutex::new(
            Some(client_stream),
        ))));
        let message = CANARY.as_bytes().repeat(400_000);
        let started = Instant::now();
        let error = client
            .call_until(
                large_exchange_request(message),
                started + Duration::from_millis(500),
            )
            .expect_err("an unread peer must not block the control operation indefinitely");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "unread peer exceeded the control operation deadline"
        );
        assert_eq!(error.stage, apppilotkit_host_runtime::ErrorStage::Ipc);
        assert_eq!(error.kind, ErrorKind::Timeout);
        assert_eq!(error.close_reason, CloseReason::Timeout);
        assert!(!format!("{error:?}").contains(CANARY));
    }

    #[test]
    fn client_call_does_not_refresh_deadline_after_writing_a_complete_request() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("pair");
        let request = large_exchange_request(b"deadline-test-request".to_vec());
        let ControlRequest::Exchange(exchange) = &request else {
            panic!("expected exchange request");
        };
        let response =
            ControlSuccess::ExchangeComplete(apppilotkit_host_runtime::ExchangeComplete {
                target_token: exchange.body.target_token,
                session_id: exchange.body.session_id.clone(),
                process_generation: exchange.body.process_generation,
                listener_epoch: exchange.body.listener_epoch,
                message: b"deadline-test-response".to_vec(),
                message_sha256: Sha256::digest(b"deadline-test-response").into(),
                handoff: apppilotkit_host_runtime::HandoffState::NotHandedOff,
            });
        server_stream
            .write_all(
                &encode_success_packet(request.request_id(), response).expect("exchange success"),
            )
            .expect("write complete response");
        server_stream
            .shutdown(std::net::Shutdown::Write)
            .expect("close complete response");

        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let client = BrokerControlClient::with_connector_and_clock(
            Arc::new(PairConnector(Mutex::new(Some(client_stream)))),
            Arc::new(RequestWriteClock {
                before_write: started,
                after_write: deadline + Duration::from_millis(1),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
        );
        let error = client
            .call_until(request, deadline)
            .expect_err("a completed response must not receive a refreshed read deadline");
        assert_eq!(error.stage, apppilotkit_host_runtime::ErrorStage::Ipc);
        assert_eq!(error.kind, ErrorKind::Timeout);
    }

    #[test]
    fn half_closed_partial_request_is_rejected_without_a_response() {
        let (mut client_stream, mut server_stream) = UnixStream::pair().expect("pair");
        let broker = Arc::new(
            SessionBroker::new(Arc::new(RejectAdapter), Arc::new(RejectAdapter))
                .expect("broker construction"),
        );
        let server = thread::spawn(move || serve_connection(&mut server_stream, broker.as_ref()));
        let packet = encode_request_packet(&large_exchange_request(vec![0x74; 1_024]))
            .expect("prepare exchange request");
        client_stream
            .write_all(&packet[..packet.len() / 2])
            .expect("write partial request");
        client_stream
            .shutdown(std::net::Shutdown::Write)
            .expect("half-close partial request");
        let mut response = Vec::new();
        client_stream
            .read_to_end(&mut response)
            .expect("read server close");

        assert!(
            server.join().expect("server must not panic").is_err(),
            "server must reject a partial control packet"
        );
        assert!(
            response.is_empty(),
            "a partial request must not receive a control response"
        );
    }

    #[test]
    fn ipc_timeout_mapping_is_opaque_for_socket_errors() {
        let error = io_failure(io::Error::new(
            io::ErrorKind::WouldBlock,
            "control-ipc-timeout-canary",
        ));

        assert_eq!(error.stage, apppilotkit_host_runtime::ErrorStage::Ipc);
        assert_eq!(error.kind, ErrorKind::Timeout);
        assert_eq!(error.close_reason, CloseReason::Timeout);
        assert!(!format!("{error:?}").contains("control-ipc-timeout-canary"));
        assert!(!error.to_string().contains("control-ipc-timeout-canary"));
    }

    #[test]
    fn server_write_to_an_unread_peer_returns_a_bounded_opaque_io_error() {
        const CANARY: &str = "control-ipc-response-canary";
        let (mut server_stream, _unread_peer) = UnixStream::pair().expect("pair");
        let packet = CANARY.as_bytes().repeat(400_000);
        let started = Instant::now();
        let error = write_control_packet(
            &mut server_stream,
            &packet,
            started + Duration::from_millis(500),
        )
        .expect_err("an unread client must not block the Broker response indefinitely");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "unread peer exceeded the control operation deadline"
        );
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "expected a socket timeout, got {error:?}"
        );
        assert!(!error.to_string().contains(CANARY));
    }

    #[test]
    fn android_adb_path_is_a_canonical_absolute_executable() {
        let root = std::env::temp_dir().join(format!(
            "apppilotkit-production-composition-adb-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let adb = root.join("platform-tools/adb");
        fs::create_dir_all(adb.parent().expect("platform-tools directory"))
            .expect("create platform-tools");
        fs::write(&adb, b"#!/bin/sh\nexit 0\n").expect("write adb");
        fs::set_permissions(&adb, fs::Permissions::from_mode(0o755)).expect("mark executable");

        assert_eq!(
            resolve_android_adb_path(&root).expect("resolved adb"),
            fs::canonicalize(&adb).expect("canonical adb")
        );

        fs::remove_dir_all(root).expect("remove adb fixture");
    }

    #[test]
    fn android_adb_path_rejects_a_missing_absolute_executable() {
        let missing = std::env::temp_dir()
            .join(format!(
                "apppilotkit-production-composition-missing-adb-{}",
                std::process::id()
            ))
            .join("adb");
        let _ = fs::remove_dir_all(&missing);
        assert!(resolve_android_adb_executable(&missing).is_err());
    }

    #[test]
    fn android_adb_resolution_is_deferred_and_unavailable_prepare_fails_closed() {
        let resolutions = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&resolutions);
        let adapter = LazyAndroidAdapter::with_resolver(move || {
            counter.fetch_add(1, Ordering::AcqRel);
            Err(io::Error::new(io::ErrorKind::NotFound, "missing adb"))
        });
        let broker = SessionBroker::new(Arc::new(RejectAdapter), Arc::new(adapter))
            .expect("Broker construction");
        assert_eq!(resolutions.load(Ordering::Acquire), 0);

        let error = match broker.handle(ControlRequest::Prepare(Request {
            request_id: [0x75; 16],
            deadline_unix_ms: deadline(),
            body: PrepareBody {
                platform: Platform::AndroidEmulator,
                device_selector: "emulator-5554".into(),
                app_id: "example.app".into(),
                app_artifact: "/tmp/example.apk".into(),
                app_artifact_sha256: [7; 32],
            },
        })) {
            Ok(_) => panic!("missing Android host tooling must reject the prepare"),
            Err(error) => error,
        };
        assert_eq!(resolutions.load(Ordering::Acquire), 1);
        assert_eq!(error.stage, apppilotkit_host_runtime::ErrorStage::Bootstrap);
        assert_eq!(error.close_reason, CloseReason::InternalError);
    }

    struct QueueConnector(Mutex<VecDeque<UnixStream>>);
    impl ControlConnector for QueueConnector {
        fn connect(&self) -> io::Result<UnixStream> {
            self.0
                .lock()
                .expect("queue connector")
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "used"))
        }
    }

    struct RejectAdapter;
    impl PlatformTargetAdapter for RejectAdapter {
        fn begin_launch(
            &self,
            _selection: TargetSelection,
            _deadline: AbsoluteDeadline,
        ) -> Box<dyn PendingLaunch> {
            Box::new(RejectPending {
                endpoint: LaunchEndpoint::ios_loopback(49_152)
                    .unwrap_or_else(|_| panic!("test endpoint")),
            })
        }
    }

    struct RejectPending {
        endpoint: LaunchEndpoint,
    }
    impl PendingLaunch for RejectPending {
        fn endpoint(&self) -> &LaunchEndpoint {
            &self.endpoint
        }
        fn launch(
            self: Box<Self>,
            _descriptor: PublicLaunchDescriptor,
            _cancellation: Cancellation,
            _deadline: AbsoluteDeadline,
        ) -> Result<apppilotkit_host_runtime::adapter::LaunchedTargetIo, PlatformFailure> {
            Err(PlatformFailure::new(PlatformFailureKind::Unavailable))
        }
        fn abort(
            self: Box<Self>,
            _cancellation: Cancellation,
            _deadline: AbsoluteDeadline,
        ) -> Result<(), PlatformFailure> {
            Ok(())
        }
    }

    #[test]
    fn private_ipc_server_reaches_the_adapter_and_rejects_prepare() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("pair");
        let connector = Arc::new(PairConnector(Mutex::new(Some(client_stream))));
        let client = BrokerControlClient::with_connector(connector);
        let broker = Arc::new(
            SessionBroker::new(Arc::new(RejectAdapter), Arc::new(RejectAdapter))
                .expect("Broker construction"),
        );
        let handle = thread::spawn(move || {
            serve_connection(&mut server_stream, broker.as_ref()).expect("serve connection");
        });
        let error = client
            .call(ControlRequest::Prepare(Request {
                request_id: [7; 16],
                deadline_unix_ms: deadline(),
                body: PrepareBody {
                    platform: Platform::IosSimulator,
                    device_selector: "device-1".into(),
                    app_id: "example.app".into(),
                    app_artifact: "/tmp/example.app".into(),
                    app_artifact_sha256: [9; 32],
                },
            }))
            .expect_err("reject");
        assert_eq!(error.stage, apppilotkit_host_runtime::ErrorStage::Bootstrap);
        assert_eq!(error.kind, ErrorKind::InternalError);
        assert_eq!(
            error.close_reason,
            apppilotkit_host_runtime::CloseReason::InternalError
        );
        handle.join().expect("server");
    }

    #[test]
    fn real_ipc_broker_adapter_completes_prepare_open_and_exchange() {
        let adapter = existing_host_adapter::successful();
        let broker = Arc::new(SessionBroker::new(Arc::clone(&adapter), adapter).expect("broker"));
        let mut clients = VecDeque::new();
        let mut servers = Vec::new();
        for _ in 0..3 {
            let (client, server) = UnixStream::pair().expect("pair");
            clients.push_back(client);
            servers.push(server);
        }
        let client =
            BrokerControlClient::with_connector(Arc::new(QueueConnector(Mutex::new(clients))));
        let mut tasks = Vec::new();
        for mut stream in servers {
            let broker = Arc::clone(&broker);
            tasks.push(thread::spawn(move || {
                serve_connection(&mut stream, broker.as_ref()).expect("serve")
            }));
        }
        let ready = match client
            .call(ControlRequest::Prepare(Request {
                request_id: [1; 16],
                deadline_unix_ms: deadline(),
                body: PrepareBody {
                    platform: Platform::IosSimulator,
                    device_selector: "device-1".into(),
                    app_id: "example.app".into(),
                    app_artifact: "/tmp/example.app".into(),
                    app_artifact_sha256: [7; 32],
                },
            }))
            .expect("prepare")
        {
            ControlSuccess::TargetReady(value) => value,
            _ => panic!("ready"),
        };
        let opened = match client
            .call(ControlRequest::OpenSession(Request {
                request_id: [2; 16],
                deadline_unix_ms: deadline(),
                body: OpenSessionBody {
                    target_token: ready.target_token,
                    session_id: None,
                    required_capabilities: vec![SESSION_OPEN_CAPABILITY.to_owned()],
                    session_open_request: Some(b"open".to_vec()),
                    session_open_request_sha256: Some(Sha256::digest(b"open").into()),
                },
            }))
            .expect("open")
        {
            ControlSuccess::SessionOpened(value) => value,
            _ => panic!("opened"),
        };
        let message = b"request".to_vec();
        let exchanged = client
            .call(ControlRequest::Exchange(Request {
                request_id: [3; 16],
                deadline_unix_ms: deadline(),
                body: ExchangeBody {
                    target_token: ready.target_token,
                    session_id: "session_test_012345".into(),
                    process_generation: ready.process_generation,
                    listener_epoch: ready.listener_epoch,
                    message: message.clone(),
                    message_sha256: Sha256::digest(&message).into(),
                    side_effect: SideEffect::ReadOnly,
                },
            }))
            .expect("exchange");
        assert!(
            matches!(exchanged, ControlSuccess::ExchangeComplete(value) if value.message == b"reply")
        );
        assert_eq!(opened.process_generation, ready.process_generation);
        for task in tasks {
            task.join().expect("server");
        }
    }

    #[test]
    fn runtime_opens_then_exchanges_only_for_its_bound_ready_target() {
        let target = ReadyReference::from_token([4; 32]).to_string();
        let (open_client, mut open_server) = UnixStream::pair().expect("open pair");
        let (exchange_client, mut exchange_server) = UnixStream::pair().expect("exchange pair");
        let client = BrokerControlClient::with_connector(Arc::new(QueueConnector(Mutex::new(
            VecDeque::from([open_client, exchange_client]),
        ))));
        let runtime = BrokerCatalogRuntime::new(client);
        let token = ReadyReference::parse(&target)
            .expect("ready reference")
            .token();
        let open_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "open-1",
            "result": {
                "context": {"id": "session_test_0123456789", "generation": 17},
                "protocol": {"major": 1, "minor": 2},
                "capabilities": ["semantic.catalog"],
                "limits": {"maxRequestBytes": 1024, "maxResponseBytes": 1024, "maxPageItems": 1}
            }
        });
        let open_bytes = serde_json::to_vec(&open_response).expect("open response");
        let open_server_task = thread::spawn(move || {
            let mut packet = Vec::new();
            open_server.read_to_end(&mut packet).expect("open request");
            let ControlRequest::OpenSession(request) =
                apppilotkit_host_runtime::decode_request_packet(&packet).expect("decode open")
            else {
                panic!("open request")
            };
            assert_eq!(request.body.target_token, token);
            assert_eq!(
                request.body.required_capabilities,
                [SESSION_OPEN_CAPABILITY]
            );
            assert!(request.body.session_open_request.is_some());
            let success = ControlSuccess::SessionOpened(apppilotkit_host_runtime::SessionOpened {
                target_token: token,
                response_sha256: Sha256::digest(&open_bytes).into(),
                response: open_bytes,
                process_generation: 17,
                listener_epoch: 3,
                handoff: apppilotkit_host_runtime::HandoffState::NotHandedOff,
            });
            open_server
                .write_all(
                    &encode_success_packet(request.request_id, success).expect("open success"),
                )
                .expect("open response");
        });
        let session = runtime
            .select(SessionSelector {
                session: None,
                target: Some(&target),
            })
            .expect("opened session");
        open_server_task.join().expect("open server");

        let exchange_server_task = thread::spawn(move || {
            let mut packet = Vec::new();
            exchange_server
                .read_to_end(&mut packet)
                .expect("exchange request");
            let ControlRequest::Exchange(request) =
                apppilotkit_host_runtime::decode_request_packet(&packet).expect("decode exchange")
            else {
                panic!("exchange request")
            };
            assert_eq!(request.body.target_token, token);
            assert_eq!(request.body.session_id, "session_test_0123456789");
            assert_eq!(request.body.process_generation, 17);
            assert_eq!(request.body.listener_epoch, 3);
            let response = br#"{"jsonrpc":"2.0","id":"catalog-1","result":{}}"#.to_vec();
            let success =
                ControlSuccess::ExchangeComplete(apppilotkit_host_runtime::ExchangeComplete {
                    target_token: token,
                    session_id: request.body.session_id,
                    process_generation: 17,
                    listener_epoch: 3,
                    message_sha256: Sha256::digest(&response).into(),
                    message: response,
                    handoff: apppilotkit_host_runtime::HandoffState::NotHandedOff,
                });
            exchange_server
                .write_all(
                    &encode_success_packet(request.request_id, success).expect("exchange success"),
                )
                .expect("exchange response");
        });
        let response = runtime
            .exchange(&session, &serde_json::json!({"method":"semantic.list"}))
            .expect("exchange");
        assert!(response.starts_with(b"{\"jsonrpc\""));
        exchange_server_task.join().expect("exchange server");
    }

    #[test]
    fn target_prepare_hashes_an_exact_android_file_before_broker_ipc() {
        let artifact_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let artifact = std::fs::canonicalize(&artifact_path)
            .expect("test artifact")
            .display()
            .to_string();
        let (client_stream, mut server_stream) = UnixStream::pair().expect("pair");
        let client = BrokerControlClient::with_connector(Arc::new(PairConnector(Mutex::new(
            Some(client_stream),
        ))));
        let artifact_for_server = artifact_path.clone();
        let server = thread::spawn(move || {
            let mut packet = Vec::new();
            server_stream
                .read_to_end(&mut packet)
                .expect("prepare request");
            let ControlRequest::Prepare(request) =
                apppilotkit_host_runtime::decode_request_packet(&packet).expect("decode prepare")
            else {
                panic!("prepare request")
            };
            assert_eq!(request.body.platform, Platform::AndroidEmulator);
            assert_eq!(request.body.app_artifact, artifact);
            let expected_digest: [u8; 32] =
                Sha256::digest(fs::read(&artifact_for_server).expect("read test artifact")).into();
            assert_eq!(request.body.app_artifact_sha256, expected_digest);
            let success = ControlSuccess::TargetReady(apppilotkit_host_runtime::ReadyTarget {
                target_token: [8; 32],
                process_generation: 19,
                listener_epoch: 1,
                issued_at_unix_ms: 100,
                expires_at_unix_ms: 30_100,
            });
            server_stream
                .write_all(
                    &encode_success_packet(request.request_id, success).expect("prepare success"),
                )
                .expect("prepare response");
        });
        let request = serde_json::json!({
            "schema_version": "1.0",
            "platform": "android-emulator",
            "device_selector": "emulator-5554",
            "app_id": "dev.apppilotkit.smoke",
            "app_artifact": std::fs::canonicalize(&artifact_path).expect("test artifact").display().to_string(),
            "artifact_encoding": "raw-file-v1"
        });
        let ready = prepare_target(
            &serde_json::to_vec(&request).expect("request json"),
            &client,
        )
        .expect("prepared target");
        assert_eq!(
            ready.target,
            ReadyReference::from_token([8; 32]).to_string()
        );
        server.join().expect("prepare server");
    }

    fn ios_tree_artifact(app_id: &str, label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "apppilotkit-production-composition-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let app = root.join("SmokeHost.app");
        std::fs::create_dir_all(&app).expect("create app tree");
        std::fs::write(
            app.join("Info.plist"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{app_id}</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>1</string><key>CFBundleExecutable</key><string>SmokeHost</string></dict></plist>"
            ),
        )
        .expect("write plist");
        let executable = app.join("SmokeHost");
        std::fs::write(&executable, b"MACHO").expect("write executable");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("mark executable");
        app
    }

    #[test]
    fn target_prepare_uses_exact_ios_tree_digest_before_broker_ipc() {
        let app_id = "dev.apppilotkit.smoke";
        let artifact_path = ios_tree_artifact(app_id, "ios-tree-digest");
        let artifact = std::fs::canonicalize(&artifact_path)
            .expect("test artifact")
            .display()
            .to_string();
        let scanner_deadline = match AbsoluteDeadline::new(deadline()) {
            Ok(value) => value,
            Err(_) => panic!("valid deadline"),
        };
        let expected = match apppilotkit_apple_simulator_adapter::inspect_ios_app_tree_digest(
            Path::new(&artifact),
            app_id,
            &Cancellation::new(),
            scanner_deadline,
        ) {
            Ok(value) => value,
            Err(_) => panic!("valid iOS app tree"),
        };
        let (client_stream, mut server_stream) = UnixStream::pair().expect("pair");
        let client = BrokerControlClient::with_connector(Arc::new(PairConnector(Mutex::new(
            Some(client_stream),
        ))));
        let server = thread::spawn(move || {
            let mut packet = Vec::new();
            server_stream
                .read_to_end(&mut packet)
                .expect("prepare request");
            let ControlRequest::Prepare(request) =
                apppilotkit_host_runtime::decode_request_packet(&packet).expect("decode prepare")
            else {
                panic!("prepare request")
            };
            assert_eq!(request.body.platform, Platform::IosSimulator);
            assert_eq!(request.body.app_artifact, artifact);
            assert_eq!(request.body.app_artifact_sha256, expected);
            let success = ControlSuccess::TargetReady(apppilotkit_host_runtime::ReadyTarget {
                target_token: [6; 32],
                process_generation: 23,
                listener_epoch: 1,
                issued_at_unix_ms: 100,
                expires_at_unix_ms: 30_100,
            });
            server_stream
                .write_all(
                    &encode_success_packet(request.request_id, success).expect("prepare success"),
                )
                .expect("prepare response");
        });
        let request = serde_json::json!({
            "schema_version": "1.0",
            "platform": "ios-simulator",
            "device_selector": "SIMULATOR",
            "app_id": app_id,
            "app_artifact": std::fs::canonicalize(&artifact_path).expect("test artifact").display().to_string(),
            "artifact_encoding": "ios-app-tree-v1"
        });
        let ready = prepare_target(
            &serde_json::to_vec(&request).expect("request json"),
            &client,
        )
        .expect("prepared target");
        assert_eq!(
            ready.target,
            ReadyReference::from_token([6; 32]).to_string()
        );
        server.join().expect("prepare server");
        std::fs::remove_dir_all(artifact_path.parent().expect("app parent"))
            .expect("remove app tree");
    }

    #[test]
    fn target_prepare_rejects_invalid_ios_tree_before_broker_ipc() {
        let root = std::env::temp_dir().join(format!(
            "apppilotkit-production-composition-invalid-ios-tree-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create invalid app tree");
        let request = serde_json::json!({
            "schema_version": "1.0",
            "platform": "ios-simulator",
            "device_selector": "SIMULATOR",
            "app_id": "dev.apppilotkit.smoke",
            "app_artifact": root.display().to_string(),
            "artifact_encoding": "ios-app-tree-v1"
        });
        let unused = BrokerControlClient::with_connector(Arc::new(PairConnector(Mutex::new(None))));
        assert!(matches!(
            prepare_target(
                &serde_json::to_vec(&request).expect("request json"),
                &unused
            ),
            Err(PrepareError::InvalidInvocation)
        ));
        std::fs::remove_dir_all(root).expect("remove invalid app tree");
    }

    #[test]
    fn maintenance_loop_drives_idle_broker_maintenance_before_stopping() {
        let broker = SessionBroker::new(Arc::new(RejectAdapter), Arc::new(RejectAdapter))
            .expect("Broker construction");
        let stop = AtomicBool::new(false);
        let calls = AtomicU64::new(0);
        run_maintenance_loop(&stop, || {
            calls.fetch_add(1, Ordering::SeqCst);
            let result = broker.maintain();
            stop.store(true, Ordering::Release);
            result
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn spawned_maintenance_obeys_external_stop_and_joins() {
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicU64::new(0));
        let stop_from_task = Arc::clone(&stop);
        let calls_from_task = Arc::clone(&calls);
        let task = spawn_maintenance_task(stop, move || {
            calls_from_task.fetch_add(1, Ordering::SeqCst);
            stop_from_task.store(true, Ordering::Release);
            Ok(())
        });
        task.join().expect("maintenance joins after external stop");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn production_maintenance_entry_accepts_an_external_stop_and_joins() {
        let broker = Arc::new(
            SessionBroker::new(Arc::new(RejectAdapter), Arc::new(RejectAdapter))
                .expect("Broker construction"),
        );
        let stop = Arc::new(AtomicBool::new(true));
        spawn_maintenance(broker, stop)
            .join()
            .expect("production maintenance joins");
    }

    #[test]
    fn accept_loop_finalizes_the_broker_after_listener_exit() {
        let shutdowns = Arc::new(AtomicU64::new(0));
        let observed_shutdowns = Arc::clone(&shutdowns);
        let stop = AtomicBool::new(false);

        let error = run_broker_accept_loop(
            || Err(io::Error::other("listener closed")),
            &stop,
            |_| panic!("listener never accepted a stream"),
            move || {
                observed_shutdowns.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect_err("listener error remains visible after finalization");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn accept_loop_stop_token_runs_the_finalizer_exactly_once() {
        let stop = AtomicBool::new(false);
        let shutdowns = Arc::new(AtomicU64::new(0));
        let observed_shutdowns = Arc::clone(&shutdowns);

        run_broker_accept_loop(
            || {
                stop.store(true, Ordering::Release);
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            },
            &stop,
            |_| panic!("stop arrives before an accepted stream"),
            move || {
                observed_shutdowns.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("stop token is a graceful listener exit");

        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn accept_loop_does_not_serve_a_connection_after_stop_arrives() {
        let stop = AtomicBool::new(false);
        let served = Arc::new(AtomicU64::new(0));
        let observed_served = Arc::clone(&served);
        let shutdowns = Arc::new(AtomicU64::new(0));
        let observed_shutdowns = Arc::clone(&shutdowns);

        run_broker_accept_loop(
            || {
                stop.store(true, Ordering::Release);
                UnixStream::pair().map(|(stream, _)| stream)
            },
            &stop,
            move |_| {
                observed_served.fetch_add(1, Ordering::SeqCst);
            },
            move || {
                observed_shutdowns.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("stop after accept is still a graceful listener exit");

        assert_eq!(served.load(Ordering::SeqCst), 0);
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stage_install_writes_the_real_three_entry_layout() {
        let root = std::env::temp_dir().join(format!(
            "apppilotkit-composition-stage-{}",
            std::process::id()
        ));
        let built = root.join("built");
        std::fs::create_dir_all(&built).expect("built dir");
        let fixture = std::env::current_exe().expect("test executable");
        for name in [
            "apppilotkit",
            "apppilotkit-broker",
            "apppilotkit-target-prepare",
        ] {
            std::fs::copy(&fixture, built.join(name)).expect("fixture binary");
        }
        let installed = stage_install(&root.join("prefix"), &built).expect("stage install");
        assert!(installed.iter().all(|path| path.is_file()));
        assert_eq!(installed, layout_paths(&root.join("prefix")));
        std::fs::remove_dir_all(root).expect("cleanup stage root");
    }

    #[test]
    fn runtime_requires_explicit_ready_target() {
        let runtime = BrokerCatalogRuntime::new(BrokerControlClient::with_connector(Arc::new(
            PairConnector(Mutex::new(None)),
        )));
        assert!(
            matches!(runtime.select(SessionSelector { session: None, target: None }), Err(CatalogSelectError::SessionSelectionRequired { candidates }) if candidates.is_empty())
        );
        assert!(
            matches!(runtime.select(SessionSelector { session: None, target: Some("not-a-target") }), Err(CatalogSelectError::TargetSelectionRequired { candidates }) if candidates.is_empty())
        );
    }

    #[test]
    fn layout_is_fixed_and_private_helpers_are_not_public_bins() {
        assert_eq!(
            layout_paths(Path::new("/prefix")),
            [
                PathBuf::from("/prefix/bin/apppilotkit"),
                PathBuf::from("/prefix/libexec/apppilotkit-broker"),
                PathBuf::from("/prefix/libexec/apppilotkit-target-prepare")
            ]
        );
    }
}
