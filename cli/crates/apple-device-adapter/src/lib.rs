//! Publish-disabled Apple physical-device raw transport adapter.
//!
//! This crate owns exact `devicectl` launch/cleanup and USB usbmux byte
//! streams. It does not own bootstrap cryptography, framing, Target proof,
//! Protocol sessions, or runtime handoff.

use apppilotkit_apple_simulator_adapter::{PreparedIosAppSnapshot, prepare_ios_app_snapshot};
use apppilotkit_host_runtime::Platform;
use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, CleanupReceipt, LaunchEndpoint, LaunchedTargetIo,
    PendingLaunch, PlatformFailure, PlatformFailureKind, PlatformTargetAdapter,
    PublicLaunchDescriptor, RawConnector, RawDuplex, TargetSelection,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod usbmux;

use usbmux::{SystemUsbMux, UsbMux, UsbMuxConnectError};

const DESCRIPTOR_ENV: &str = "DEVICECTL_CHILD_APPPILOTKIT_TRANSPORT_DESCRIPTOR";
const DEVICECTL_CHILD_PREFIX: &str = "DEVICECTL_CHILD_";
const TOOL_OUTPUT_CAP: usize = 1_048_576;
const LAUNCH_OUTPUT_CAP: usize = 65_536;
const DESCRIPTOR_CAP: usize = 8_192;
const REAP_POLL: Duration = Duration::from_millis(5);
const TERM_GRACE: Duration = Duration::from_millis(250);
const CLEANUP_BUDGET_MS: u64 = 2_000;
const DEFAULT_XCRUN: &str = "/usr/bin/xcrun";

/// Exact-target Apple physical device adapter backed by one explicit `xcrun`
/// path and one usbmux socket path.
pub struct AppleDeviceAdapter {
    runner: Arc<dyn ToolRunner>,
    usbmux: Arc<dyn UsbMux>,
}

impl AppleDeviceAdapter {
    /// Creates an adapter. Relative tool paths are rejected when a launch is
    /// attempted so the process can never be selected through `PATH` lookup.
    pub fn new(xcrun: PathBuf, usbmux_path: PathBuf) -> Self {
        Self {
            runner: Arc::new(ProcessToolRunner { xcrun }),
            usbmux: Arc::new(SystemUsbMux::new(usbmux_path)),
        }
    }
}

impl Default for AppleDeviceAdapter {
    fn default() -> Self {
        Self::new(
            PathBuf::from(DEFAULT_XCRUN),
            PathBuf::from(usbmux::DEFAULT_USBMUXD),
        )
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

impl PlatformTargetAdapter for AppleDeviceAdapter {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        absolute_deadline: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch> {
        let validation_failure = validate_selection(&selection).err();
        let target_key = validation_failure
            .is_none()
            .then(|| TargetKey::from_selection(&selection));
        let (endpoint, owner, reservation_failure) =
            match reserve_device_port(target_key, absolute_deadline) {
                Ok(reserved) => reserved,
                Err(failure) => (
                    LaunchEndpoint::ios_loopback(49_152)
                        .unwrap_or_else(|_| unreachable!("constant endpoint is valid")),
                    None,
                    Some(failure),
                ),
            };
        Box::new(AppleDevicePendingLaunch {
            endpoint,
            selection,
            runner: Arc::clone(&self.runner),
            usbmux: Arc::clone(&self.usbmux),
            reservation: owner,
            failure: reservation_failure.or(validation_failure),
        })
    }
}

struct AppleDevicePendingLaunch {
    endpoint: LaunchEndpoint,
    selection: TargetSelection,
    runner: Arc<dyn ToolRunner>,
    usbmux: Arc<dyn UsbMux>,
    reservation: Option<AdapterReservation>,
    failure: Option<PlatformFailure>,
}

impl PendingLaunch for AppleDevicePendingLaunch {
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
            usb_gate(
                self.usbmux.as_ref(),
                self.selection.device_selector(),
                &cancellation,
                absolute_deadline,
            )?;
            let snapshot =
                prepare_host_snapshot(&self.selection, &cancellation, absolute_deadline)?;
            feature_probe(self.runner.as_ref(), &cancellation, absolute_deadline)?;
            Ok((
                URL_SAFE_NO_PAD.encode(descriptor.canonical_bytes()),
                snapshot,
            ))
        })();
        let (descriptor, snapshot) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => return Err(self.prelaunch_failure(error)),
        };
        if let Err(error) = reject_if_occupied(
            self.runner.as_ref(),
            self.selection.device_selector(),
            ProcessIdentity::Suffix(&snapshot.occupancy_suffix),
            &cancellation,
            absolute_deadline,
        ) {
            return Err(self.prelaunch_failure(error));
        }
        let candidate = match establish_on_device_identity(
            self.runner.as_ref(),
            &self.selection,
            &snapshot,
            &cancellation,
            absolute_deadline,
        ) {
            Ok(candidate) => candidate,
            Err(error) => return Err(self.prelaunch_failure(error)),
        };
        drop(snapshot);
        if let Err(error) = reject_if_occupied(
            self.runner.as_ref(),
            self.selection.device_selector(),
            ProcessIdentity::Exact(&candidate.process_path),
            &cancellation,
            absolute_deadline,
        ) {
            return Err(self.fail_after_install(&candidate, error));
        }
        self.launch_installed(candidate, descriptor, cancellation, absolute_deadline)
    }

    fn abort(
        mut self: Box<Self>,
        _cancellation: Cancellation,
        _absolute_deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        self.release_reservation()
    }
}

impl AppleDevicePendingLaunch {
    fn prelaunch_failure(&mut self, original: PlatformFailure) -> PlatformFailure {
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

    fn fail_after_install(
        &mut self,
        candidate: &LaunchCandidate,
        original: PlatformFailure,
    ) -> PlatformFailure {
        let Ok(deadline) = cleanup_deadline() else {
            return failure(PlatformFailureKind::CleanupFailed);
        };
        if cleanup_owned_installation(
            self.runner.as_ref(),
            self.selection.device_selector(),
            self.selection.app_id(),
            candidate,
            &Cancellation::new(),
            deadline,
        )
        .is_ok()
            && self.release_reservation().is_ok()
        {
            original
        } else {
            failure(PlatformFailureKind::CleanupFailed)
        }
    }

    fn after_uncertain_launch_failure(
        &mut self,
        candidate: &LaunchCandidate,
        pid: Option<u32>,
        original: PlatformFailure,
    ) -> PlatformFailure {
        let Ok(deadline) = cleanup_deadline() else {
            return failure(PlatformFailureKind::CleanupFailed);
        };
        let cancellation = Cancellation::new();
        let process_cleaned = if let Some(pid) = pid {
            terminate_owned_pid(
                self.runner.as_ref(),
                self.selection.device_selector(),
                pid,
                &candidate.process_path,
                &cancellation,
                deadline,
            )
            .is_ok()
        } else {
            matches!(
                matching_processes(
                    self.runner.as_ref(),
                    self.selection.device_selector(),
                    ProcessIdentity::Exact(&candidate.process_path),
                    &cancellation,
                    deadline,
                ),
                Ok(processes) if processes.is_empty()
            )
        };
        if process_cleaned
            && cleanup_owned_installation(
                self.runner.as_ref(),
                self.selection.device_selector(),
                self.selection.app_id(),
                candidate,
                &cancellation,
                deadline,
            )
            .is_ok()
            && self.release_reservation().is_ok()
        {
            original
        } else {
            failure(PlatformFailureKind::CleanupFailed)
        }
    }

    fn launch_installed(
        mut self: Box<Self>,
        candidate: LaunchCandidate,
        descriptor: String,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<LaunchedTargetIo, PlatformFailure> {
        let json_path = match json_output_path() {
            Ok(path) => path,
            Err(error) => {
                return Err(self.after_uncertain_launch_failure(&candidate, None, error));
            }
        };
        let request = ToolRequest::launch(
            self.runner.program(),
            self.selection.device_selector(),
            self.selection.app_id(),
            json_path.clone(),
            descriptor,
        );
        let launched =
            match self
                .runner
                .run(request, LAUNCH_OUTPUT_CAP, &cancellation, absolute_deadline)
            {
                Ok(output) if output.status.success() && !output.oversized => output,
                Ok(_) => {
                    let _ = fs::remove_file(&json_path);
                    return Err(self.after_uncertain_launch_failure(
                        &candidate,
                        None,
                        failure(PlatformFailureKind::Rejected),
                    ));
                }
                Err(error) => {
                    let _ = fs::remove_file(&json_path);
                    return Err(self.after_uncertain_launch_failure(&candidate, None, error));
                }
            };
        let pid = match read_launch_pid(&json_path) {
            Ok(pid) => pid,
            Err(error) => {
                let _ = fs::remove_file(&json_path);
                return Err(self.after_uncertain_launch_failure(&candidate, None, error));
            }
        };
        let _ = fs::remove_file(&json_path);
        let _ = launched;
        if let Err(error) = prove_exact_owner(
            self.runner.as_ref(),
            self.selection.device_selector(),
            &candidate.process_path,
            pid,
            &cancellation,
            absolute_deadline,
        ) {
            return Err(self.after_uncertain_launch_failure(&candidate, None, error));
        }
        let port = match self.endpoint.ios_port() {
            Some(port) => port,
            None => {
                return Err(self.after_uncertain_launch_failure(
                    &candidate,
                    Some(pid),
                    failure(PlatformFailureKind::Internal),
                ));
            }
        };
        let bootstrap = match connect_usbmux(
            self.usbmux.as_ref(),
            self.selection.device_selector(),
            port,
            &cancellation,
            absolute_deadline,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                return Err(self.after_uncertain_launch_failure(&candidate, Some(pid), error));
            }
        };
        let registry = Arc::new(ConnectionRegistry::default());
        registry.register(&bootstrap);
        let connector = Arc::new(AppleDeviceConnector {
            usbmux: Arc::clone(&self.usbmux),
            serial: self.selection.device_selector().to_owned(),
            port,
            registry: Arc::clone(&registry),
        });
        let cleanup = Box::new(AppleDeviceCleanup {
            runner: Arc::clone(&self.runner),
            udid: self.selection.device_selector().to_owned(),
            app_id: self.selection.app_id().to_owned(),
            process_path: candidate.process_path.clone(),
            owned_app_url: candidate.owned_app_url.clone(),
            source_app_name: candidate.source_app_name.clone(),
            executable: candidate.executable.clone(),
            installed_by_lease: candidate.installed_by_lease,
            pid,
            reservation: self.reservation.take(),
            registry,
        });
        Ok(LaunchedTargetIo::new(bootstrap, connector, cleanup))
    }
}

struct AppleDeviceConnector {
    usbmux: Arc<dyn UsbMux>,
    serial: String,
    port: u16,
    registry: Arc<ConnectionRegistry>,
}

impl RawConnector for AppleDeviceConnector {
    fn connect(
        &self,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
        let stream = connect_usbmux(
            self.usbmux.as_ref(),
            &self.serial,
            self.port,
            &cancellation,
            absolute_deadline,
        )?;
        self.registry.register(&stream);
        Ok(stream)
    }
}

#[derive(Default)]
struct ConnectionRegistry {
    streams: Mutex<Vec<std::sync::Weak<dyn RawDuplex>>>,
}

impl ConnectionRegistry {
    fn register(&self, stream: &Arc<dyn RawDuplex>) {
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
        for stream in streams.iter().filter_map(std::sync::Weak::upgrade) {
            stream.cancel();
        }
        Ok(())
    }
}

struct AppleDeviceCleanup {
    runner: Arc<dyn ToolRunner>,
    udid: String,
    app_id: String,
    process_path: String,
    owned_app_url: String,
    source_app_name: String,
    executable: String,
    installed_by_lease: bool,
    pid: u32,
    reservation: Option<AdapterReservation>,
    registry: Arc<ConnectionRegistry>,
}

impl CleanupReceipt for AppleDeviceCleanup {
    fn cleanup(
        mut self: Box<Self>,
        cancellation: Cancellation,
        absolute_deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        self.registry.cancel_all()?;
        terminate_owned_pid(
            self.runner.as_ref(),
            &self.udid,
            self.pid,
            &self.process_path,
            &cancellation,
            absolute_deadline,
        )
        .map_err(into_cleanup_failed)?;
        cleanup_owned_installation(
            self.runner.as_ref(),
            &self.udid,
            &self.app_id,
            &LaunchCandidate {
                process_path: self.process_path.clone(),
                owned_app_url: self.owned_app_url.clone(),
                source_app_name: self.source_app_name.clone(),
                executable: self.executable.clone(),
                installed_by_lease: self.installed_by_lease,
            },
            &cancellation,
            absolute_deadline,
        )?;
        let Some(mut reservation) = self.reservation.take() else {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        };
        reservation.release()
    }
}

fn reserve_device_port(
    target: Option<TargetKey>,
    deadline: AbsoluteDeadline,
) -> Result<
    (
        LaunchEndpoint,
        Option<AdapterReservation>,
        Option<PlatformFailure>,
    ),
    PlatformFailure,
> {
    remaining(deadline)?;
    let mut entropy =
        fs::File::open("/dev/urandom").map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    for _ in 0..1_024 {
        let mut bytes = [0_u8; 2];
        entropy
            .read_exact(&mut bytes)
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let port = 49_152 + (u16::from_le_bytes(bytes) & 0x3FFF);
        let Ok(endpoint) = LaunchEndpoint::ios_loopback(port) else {
            continue;
        };
        let mut ledger = reservation_ledger()
            .lock()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        if ledger.ports.contains(&port) {
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
            endpoint,
            Some(AdapterReservation {
                port,
                target: owned_target,
                released: false,
            }),
            target_conflict.then_some(failure(PlatformFailureKind::Rejected)),
        ));
    }
    Err(failure(PlatformFailureKind::Unavailable))
}

fn validate_selection(selection: &TargetSelection) -> Result<(), PlatformFailure> {
    let artifact = Path::new(selection.artifact_path());
    if selection.platform() != Platform::IosDevice
        || !is_hardware_udid(selection.device_selector())
        || is_coredevice_uuid(selection.device_selector())
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

fn is_hardware_udid(value: &str) -> bool {
    value.len() == 25
        && value.as_bytes().get(8) == Some(&b'-')
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 8 {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)
            }
        })
}

fn is_coredevice_uuid(value: &str) -> bool {
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

fn prepare_host_snapshot(
    selection: &TargetSelection,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<HostPreparedArtifact, PlatformFailure> {
    let source = Path::new(selection.artifact_path());
    let snapshot = prepare_ios_app_snapshot(
        source,
        selection.app_id(),
        &selection.artifact_digest(),
        cancellation,
        deadline,
    )?;
    let occupancy_suffix = host_process_suffix(source, snapshot.executable())?;
    let source_app_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?
        .to_owned();
    Ok(HostPreparedArtifact {
        snapshot,
        occupancy_suffix,
        source_app_name,
    })
}

struct HostPreparedArtifact {
    snapshot: PreparedIosAppSnapshot,
    occupancy_suffix: String,
    source_app_name: String,
}

fn host_process_suffix(app: &Path, executable: &str) -> Result<String, PlatformFailure> {
    let name = app
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    if !name.ends_with(".app")
        || name.len() <= 4
        || name.contains('/')
        || name.as_bytes().contains(&0)
        || executable.is_empty()
        || executable.contains('/')
        || executable.contains('\0')
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(format!("/{name}/{executable}"))
}

fn usb_gate(
    usbmux: &dyn UsbMux,
    serial: &str,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    usbmux
        .find_usb_device(serial, cancellation, deadline)
        .map(|_| ())
        .map_err(|error| {
            if error.kind() == PlatformFailureKind::Rejected {
                error
            } else {
                failure(PlatformFailureKind::Unavailable)
            }
        })
}

fn feature_probe(
    runner: &dyn ToolRunner,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    for args in [
        ["devicectl", "help", "device", "install", "app"].as_slice(),
        ["devicectl", "help", "device", "process", "launch"].as_slice(),
        ["devicectl", "help", "device", "process", "terminate"].as_slice(),
        ["devicectl", "help", "device", "info", "processes"].as_slice(),
        ["devicectl", "help", "device", "info", "apps"].as_slice(),
    ] {
        let output = runner.run(
            ToolRequest::plain(runner.program(), args.iter().copied()),
            TOOL_OUTPUT_CAP,
            cancellation,
            deadline,
        )?;
        if output.oversized || !output.status.success() {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        let text = strict_help_text(&output)?;
        if !text.contains("--json-output") || !text.contains("--device") {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        if args.last() == Some(&"launch") && !text.contains("DEVICECTL_CHILD_") {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
    }
    Ok(())
}

struct LaunchCandidate {
    process_path: String,
    owned_app_url: String,
    source_app_name: String,
    executable: String,
    installed_by_lease: bool,
}

fn establish_on_device_identity(
    runner: &dyn ToolRunner,
    selection: &TargetSelection,
    snapshot: &HostPreparedArtifact,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<LaunchCandidate, PlatformFailure> {
    if installed_app_present(runner, selection, cancellation, deadline)? {
        // The on-device tree cannot be hashed, so a present same-bundle app is
        // not identity-equivalent to this Host artifact.
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let json_path = json_output_path()?;
    let installed = match install_host_artifact(
        runner,
        selection.device_selector(),
        snapshot.snapshot.app_path(),
        json_path,
        cancellation,
        deadline,
    ) {
        Ok(()) => bind_installed_identity(runner, selection, snapshot, cancellation, deadline),
        Err(original) => Err(original),
    };
    match installed {
        Ok(candidate) => Ok(candidate),
        Err(original) => Err(revert_failed_install_attempt(
            runner,
            selection.device_selector(),
            selection.app_id(),
            &snapshot.source_app_name,
            snapshot.snapshot.executable(),
            original,
        )),
    }
}

fn bind_installed_identity(
    runner: &dyn ToolRunner,
    selection: &TargetSelection,
    snapshot: &HostPreparedArtifact,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<LaunchCandidate, PlatformFailure> {
    let bytes = run_json(
        runner,
        [
            OsString::from("devicectl"),
            OsString::from("device"),
            OsString::from("info"),
            OsString::from("apps"),
            OsString::from("--device"),
            OsString::from(selection.device_selector()),
            OsString::from("--bundle-id"),
            OsString::from(selection.app_id()),
        ],
        cancellation,
        deadline,
    )?;
    let installed = parse_owned_installed_app(&bytes, selection.app_id())?
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let basename = Path::new(&installed.app_path)
        .file_name()
        .and_then(|name| name.to_str());
    if basename != Some(snapshot.source_app_name.as_str()) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(LaunchCandidate {
        process_path: format!("{}/{}", installed.app_path, snapshot.snapshot.executable()),
        owned_app_url: installed.url,
        source_app_name: snapshot.source_app_name.clone(),
        executable: snapshot.snapshot.executable().to_owned(),
        installed_by_lease: true,
    })
}

fn revert_failed_install_attempt(
    runner: &dyn ToolRunner,
    udid: &str,
    app_id: &str,
    source_app_name: &str,
    executable: &str,
    original: PlatformFailure,
) -> PlatformFailure {
    let Ok(deadline) = cleanup_deadline() else {
        return failure(PlatformFailureKind::CleanupFailed);
    };
    if revert_install_attempt(
        runner,
        udid,
        app_id,
        source_app_name,
        executable,
        None,
        &Cancellation::new(),
        deadline,
    )
    .is_ok()
    {
        original
    } else {
        failure(PlatformFailureKind::CleanupFailed)
    }
}

fn installed_app_present(
    runner: &dyn ToolRunner,
    selection: &TargetSelection,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<bool, PlatformFailure> {
    let bytes = run_json(
        runner,
        [
            OsString::from("devicectl"),
            OsString::from("device"),
            OsString::from("info"),
            OsString::from("apps"),
            OsString::from("--device"),
            OsString::from(selection.device_selector()),
            OsString::from("--bundle-id"),
            OsString::from(selection.app_id()),
        ],
        cancellation,
        deadline,
    )?;
    parse_installed_app_present(&bytes, selection.app_id())
}

fn parse_installed_app_present(bytes: &[u8], app_id: &str) -> Result<bool, PlatformFailure> {
    let value = parse_json_object(bytes)?;
    let apps = value
        .pointer("/result/apps")
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let apps = apps
        .as_array()
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    if apps.is_empty() {
        return Ok(false);
    }
    let mut matched = false;
    for app in apps {
        match bundle_id_field(app) {
            Some(found) if found == app_id => matched = true,
            Some(_) => {}
            None => matched = true,
        }
    }
    Ok(matched)
}

struct OwnedInstalledApp {
    url: String,
    app_path: String,
}

fn parse_owned_installed_app(
    bytes: &[u8],
    app_id: &str,
) -> Result<Option<OwnedInstalledApp>, PlatformFailure> {
    let value = parse_json_object(bytes)?;
    let apps = value
        .pointer("/result/apps")
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let apps = apps
        .as_array()
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    if apps.is_empty() {
        return Ok(None);
    }
    if apps.len() != 1 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let app = &apps[0];
    let Some(found) = bundle_id_field(app) else {
        return Err(failure(PlatformFailureKind::Rejected));
    };
    if found != app_id {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let url = app
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    Ok(Some(OwnedInstalledApp {
        url: url.to_owned(),
        app_path: parse_device_app_url(url)?,
    }))
}

fn parse_device_app_url(url: &str) -> Result<String, PlatformFailure> {
    let rest = url
        .strip_prefix("file://")
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let path = if let Some(path) = rest.strip_prefix("localhost") {
        path
    } else {
        rest
    };
    if !path.starts_with('/')
        || path.contains('\0')
        || path.contains('?')
        || path.contains('#')
        || path.contains('%')
        || path.contains("//")
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let path = path.trim_end_matches('/');
    if !path.ends_with(".app") {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    for component in path.split('/') {
        if component == "." || component == ".." {
            return Err(failure(PlatformFailureKind::Rejected));
        }
    }
    let last = path
        .rsplit('/')
        .next()
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    if last.len() <= 4 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(path.to_owned())
}

fn install_host_artifact(
    runner: &dyn ToolRunner,
    udid: &str,
    snapshot_path: &Path,
    json_path: PathBuf,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let result = runner.run(
        ToolRequest::plain(
            runner.program(),
            [
                OsString::from("devicectl"),
                OsString::from("device"),
                OsString::from("install"),
                OsString::from("app"),
                OsString::from("--device"),
                OsString::from(udid),
                OsString::from(snapshot_path.as_os_str()),
                OsString::from("--json-output"),
                json_path.clone().into_os_string(),
            ],
        ),
        TOOL_OUTPUT_CAP,
        cancellation,
        deadline,
    );
    let bytes = fs::read(&json_path);
    let _ = fs::remove_file(&json_path);
    let output = result?;
    if output.oversized || !output.status.success() {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    bytes
        .map(|_| ())
        .map_err(|_| failure(PlatformFailureKind::Rejected))
}

fn reject_if_occupied(
    runner: &dyn ToolRunner,
    udid: &str,
    identity: ProcessIdentity<'_>,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let processes = matching_processes(runner, udid, identity, cancellation, deadline)?;
    if processes.is_empty() {
        Ok(())
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

fn prove_exact_owner(
    runner: &dyn ToolRunner,
    udid: &str,
    process_path: &str,
    pid: u32,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let processes = matching_processes(
        runner,
        udid,
        ProcessIdentity::Exact(process_path),
        cancellation,
        deadline,
    )?;
    if processes.as_slice() == [pid] {
        Ok(())
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

#[derive(Clone, Copy)]
enum ProcessIdentity<'a> {
    Suffix(&'a str),
    Exact(&'a str),
}

impl ProcessIdentity<'_> {
    fn matches(self, executable: &str) -> bool {
        match self {
            Self::Suffix(suffix) => executable.ends_with(suffix),
            Self::Exact(path) => executable == path,
        }
    }
}

fn matching_processes(
    runner: &dyn ToolRunner,
    udid: &str,
    identity: ProcessIdentity<'_>,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<Vec<u32>, PlatformFailure> {
    let bytes = run_json(
        runner,
        [
            OsString::from("devicectl"),
            OsString::from("device"),
            OsString::from("info"),
            OsString::from("processes"),
            OsString::from("--device"),
            OsString::from(udid),
        ],
        cancellation,
        deadline,
    )?;
    parse_matching_processes(&bytes, identity)
}

fn parse_matching_processes(
    bytes: &[u8],
    identity: ProcessIdentity<'_>,
) -> Result<Vec<u32>, PlatformFailure> {
    let processes = parse_device_processes(bytes)?;
    let mut matches = Vec::new();
    for process in processes {
        if identity.matches(&process.executable) {
            matches.push(process.pid);
        }
    }
    matches.sort_unstable();
    matches.dedup();
    Ok(matches)
}

struct DeviceProcess {
    pid: u32,
    executable: String,
}

fn parse_device_processes(bytes: &[u8]) -> Result<Vec<DeviceProcess>, PlatformFailure> {
    let value = parse_json_object(bytes)?;
    let processes = value
        .pointer("/result/runningProcesses")
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let processes = processes
        .as_array()
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let mut parsed = Vec::with_capacity(processes.len());
    for process in processes {
        let pid = process
            .get("processIdentifier")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        let pid = u32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        let executable = process
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        parsed.push(DeviceProcess {
            pid,
            executable: executable.to_owned(),
        });
    }
    Ok(parsed)
}

fn bundle_id_field(value: &serde_json::Value) -> Option<&str> {
    value
        .get("bundleIdentifier")
        .and_then(serde_json::Value::as_str)
}

fn parse_json_object(bytes: &[u8]) -> Result<serde_json::Value, PlatformFailure> {
    if bytes.is_empty() || bytes.len() > TOOL_OUTPUT_CAP {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    serde_json::from_slice(bytes).map_err(|_| failure(PlatformFailureKind::Rejected))
}

fn run_json<I, A>(
    runner: &dyn ToolRunner,
    args: I,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<Vec<u8>, PlatformFailure>
where
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let json_path = json_output_path()?;
    let mut args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    args.push(OsString::from("--json-output"));
    args.push(json_path.clone().into_os_string());
    let result = runner.run(
        ToolRequest::plain(runner.program(), args),
        TOOL_OUTPUT_CAP,
        cancellation,
        deadline,
    );
    let bytes = fs::read(&json_path);
    let _ = fs::remove_file(&json_path);
    let output = result?;
    if output.oversized || !output.status.success() {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    bytes.map_err(|_| failure(PlatformFailureKind::Rejected))
}

fn cleanup_owned_installation(
    runner: &dyn ToolRunner,
    udid: &str,
    app_id: &str,
    candidate: &LaunchCandidate,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    if !candidate.installed_by_lease {
        return Ok(());
    }
    revert_install_attempt(
        runner,
        udid,
        app_id,
        &candidate.source_app_name,
        &candidate.executable,
        Some(candidate),
        cancellation,
        deadline,
    )
}

#[allow(clippy::too_many_arguments)]
fn revert_install_attempt(
    runner: &dyn ToolRunner,
    udid: &str,
    app_id: &str,
    source_app_name: &str,
    executable: &str,
    owned: Option<&LaunchCandidate>,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let bytes = run_json(
        runner,
        [
            OsString::from("devicectl"),
            OsString::from("device"),
            OsString::from("info"),
            OsString::from("apps"),
            OsString::from("--device"),
            OsString::from(udid),
            OsString::from("--bundle-id"),
            OsString::from(app_id),
        ],
        cancellation,
        deadline,
    )
    .map_err(into_cleanup_failed)?;
    let installed = parse_owned_installed_app(&bytes, app_id).map_err(into_cleanup_failed)?;
    let Some(installed) = installed else {
        return Ok(());
    };
    let basename = Path::new(&installed.app_path)
        .file_name()
        .and_then(|name| name.to_str());
    if basename != Some(source_app_name) {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    if let Some(candidate) = owned
        && candidate.owned_app_url != installed.url
    {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    let process_path = owned
        .map(|candidate| candidate.process_path.clone())
        .unwrap_or_else(|| format!("{}/{}", installed.app_path, executable));
    let processes = matching_processes(
        runner,
        udid,
        ProcessIdentity::Exact(&process_path),
        cancellation,
        deadline,
    )
    .map_err(into_cleanup_failed)?;
    if !processes.is_empty() {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    run_json(
        runner,
        [
            OsString::from("devicectl"),
            OsString::from("device"),
            OsString::from("uninstall"),
            OsString::from("app"),
            OsString::from("--device"),
            OsString::from(udid),
            OsString::from(app_id),
        ],
        cancellation,
        deadline,
    )
    .map_err(into_cleanup_failed)?;
    Ok(())
}

fn connect_usbmux(
    usbmux: &dyn UsbMux,
    serial: &str,
    port: u16,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<Arc<dyn RawDuplex>, PlatformFailure> {
    match connect_usbmux_attempt(usbmux, serial, port, cancellation, deadline) {
        Ok(stream) => Ok(stream),
        Err(UsbMuxConnectError::ConnectionRefused) => {
            Err(failure(PlatformFailureKind::Unavailable))
        }
        Err(UsbMuxConnectError::Failed(error)) => Err(error),
    }
}

fn connect_usbmux_attempt(
    usbmux: &dyn UsbMux,
    serial: &str,
    port: u16,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<Arc<dyn RawDuplex>, UsbMuxConnectError> {
    check_cancel_deadline(cancellation, deadline)?;
    let device_id = usbmux.find_usb_device(serial, cancellation, deadline)?;
    usbmux.connect(device_id, port, cancellation, deadline)
}

fn terminate_owned_pid(
    runner: &dyn ToolRunner,
    udid: &str,
    pid: u32,
    process_path: &str,
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let processes = parse_device_processes(
        &run_json(
            runner,
            [
                OsString::from("devicectl"),
                OsString::from("device"),
                OsString::from("info"),
                OsString::from("processes"),
                OsString::from("--device"),
                OsString::from(udid),
            ],
            cancellation,
            deadline,
        )
        .map_err(into_cleanup_failed)?,
    )
    .map_err(into_cleanup_failed)?;
    let Some(process) = processes.iter().find(|process| process.pid == pid) else {
        return Ok(());
    };
    let exact_matches = processes
        .iter()
        .filter(|candidate| candidate.executable == process_path)
        .map(|candidate| candidate.pid)
        .collect::<Vec<_>>();
    if process.executable != process_path || exact_matches.as_slice() != [pid] {
        return Err(failure(PlatformFailureKind::CleanupFailed));
    }
    if run_json(
        runner,
        [
            OsString::from("devicectl"),
            OsString::from("device"),
            OsString::from("process"),
            OsString::from("terminate"),
            OsString::from("--device"),
            OsString::from(udid),
            OsString::from("--pid"),
            OsString::from(pid.to_string()),
        ],
        cancellation,
        deadline,
    )
    .is_ok()
    {
        return Ok(());
    }
    let still_listed = parse_device_processes(
        &run_json(
            runner,
            [
                OsString::from("devicectl"),
                OsString::from("device"),
                OsString::from("info"),
                OsString::from("processes"),
                OsString::from("--device"),
                OsString::from(udid),
            ],
            cancellation,
            deadline,
        )
        .map_err(into_cleanup_failed)?,
    )
    .map_err(into_cleanup_failed)?;
    if still_listed.iter().any(|process| process.pid == pid) {
        Err(failure(PlatformFailureKind::CleanupFailed))
    } else {
        Ok(())
    }
}

fn json_output_path() -> Result<PathBuf, PlatformFailure> {
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    Ok(std::env::temp_dir().join(format!(
        "apppilotkit-devicectl-{}-{:016x}.json",
        std::process::id(),
        u64::from_le_bytes(bytes)
    )))
}

fn read_launch_pid(path: &Path) -> Result<u32, PlatformFailure> {
    let bytes = fs::read(path).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    parse_launch_pid(&bytes)
}

fn parse_launch_pid(bytes: &[u8]) -> Result<u32, PlatformFailure> {
    if bytes.is_empty() || bytes.len() > TOOL_OUTPUT_CAP {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| failure(PlatformFailureKind::Rejected))?;
    let pid = value
        .pointer("/result/process/processIdentifier")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))
}

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
    scrub_devicectl_child: bool,
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
            scrub_devicectl_child: false,
        }
    }

    fn launch(
        program: &Path,
        udid: &str,
        app_id: &str,
        json_output: PathBuf,
        descriptor: String,
    ) -> Self {
        Self {
            program: program.to_path_buf(),
            args: [
                OsString::from("devicectl"),
                OsString::from("device"),
                OsString::from("process"),
                OsString::from("launch"),
                OsString::from("--device"),
                OsString::from(udid),
                OsString::from("--json-output"),
                json_output.into_os_string(),
                OsString::from(app_id),
            ]
            .into(),
            descriptor_env: Some(descriptor.into()),
            scrub_devicectl_child: true,
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
        if !request.program.is_absolute() {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        check_cancel_deadline(cancellation, deadline)?;
        let child = spawn_captured(request, cap)?;
        wait_for_output(child, cancellation, deadline)
    }
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
    if request.scrub_devicectl_child {
        for key in inherited_keys {
            if key
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(DEVICECTL_CHILD_PREFIX.as_bytes())
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
    Ok(CapturedChild {
        child,
        stdout: Some(drain(stdout, cap, Arc::clone(&oversized))),
        stderr: Some(drain(stderr, cap, Arc::clone(&oversized))),
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
        // SAFETY: `pid` is the exact child returned by Command::spawn.
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

fn into_cleanup_failed(error: PlatformFailure) -> PlatformFailure {
    if error.kind() == PlatformFailureKind::CleanupFailed {
        error
    } else {
        failure(PlatformFailureKind::CleanupFailed)
    }
}

#[cfg(test)]
mod tests;
