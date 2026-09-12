//! Exact-serial Android physical-device adapter for the publish-disabled Host raw SPI.

mod apk;
mod process;
mod raw;

use std::{
    env,
    ffi::OsString,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use apppilotkit_host_runtime::{
    Platform,
    adapter::{
        AbsoluteDeadline, Cancellation, CleanupReceipt, LaunchEndpoint, LaunchedTargetIo,
        PendingLaunch, PlatformFailure, PlatformFailureKind, PlatformTargetAdapter,
        PublicLaunchDescriptor, RawConnector, TargetSelection,
    },
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use process::{CommandRunner, ProcessOutput, SystemCommandRunner, ensure_active, failure};
use raw::LoopbackConnector;

const BOOTSTRAP_ACTIVITY: &str = "/.AppPilotKitBootstrapActivity";
const DESCRIPTOR_EXTRA: &str = "dev.apppilotkit.transport.DESCRIPTOR";
const LOCALABSTRACT_PREFIX: &str = "apppilotkit-android-";
const DESCRIPTOR_LIMIT: usize = 4 * 1024;
const ARTIFACT_LIMIT: u64 = 1024 * 1024 * 1024;
const FORWARD_ROLLBACK_MS: u64 = 2_000;

/// Adapter for exactly one caller-selected Android physical Target at a time.
pub struct AndroidDeviceAdapter {
    adb_path: Option<PathBuf>,
    runner: Arc<dyn CommandRunner>,
}

impl AndroidDeviceAdapter {
    pub fn new(adb_path: impl Into<PathBuf>) -> Self {
        Self {
            adb_path: Some(adb_path.into()),
            runner: Arc::new(SystemCommandRunner),
        }
    }

    /// Resolves `adb` only when a physical Android Target is launched so an
    /// iOS-only host can still construct the production Broker.
    pub fn production() -> Self {
        Self {
            adb_path: None,
            runner: Arc::new(SystemCommandRunner),
        }
    }

    #[cfg(test)]
    fn with_runner(adb_path: impl Into<PathBuf>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            adb_path: Some(adb_path.into()),
            runner,
        }
    }
}

impl PlatformTargetAdapter for AndroidDeviceAdapter {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        deadline: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch> {
        let adb_path = match &self.adb_path {
            Some(path) => path.clone(),
            None => match resolve_production_adb() {
                Ok(path) => path,
                Err(_) => return Box::new(UnavailableAndroidLaunch::new()),
            },
        };
        let (localabstract, random_failure) = random_localabstract();
        let validation = validate_selection(&selection, &adb_path, deadline)
            .and(random_failure.map_or(Ok(()), Err));
        let endpoint = LaunchEndpoint::android_local_abstract(localabstract.clone())
            .unwrap_or_else(|_| unreachable!("adapter-generated endpoint is valid"));
        Box::new(AndroidDevicePendingLaunch {
            adb_path,
            runner: Arc::clone(&self.runner),
            serial: selection.device_selector().to_owned(),
            package: selection.app_id().to_owned(),
            artifact: PathBuf::from(selection.artifact_path()),
            artifact_digest: selection.artifact_digest(),
            localabstract,
            endpoint,
            validation: validation.err().map(|error| error.kind()),
        })
    }
}

struct AndroidDevicePendingLaunch {
    adb_path: PathBuf,
    runner: Arc<dyn CommandRunner>,
    serial: String,
    package: String,
    artifact: PathBuf,
    artifact_digest: [u8; 32],
    localabstract: String,
    endpoint: LaunchEndpoint,
    validation: Option<PlatformFailureKind>,
}

impl PendingLaunch for AndroidDevicePendingLaunch {
    fn endpoint(&self) -> &LaunchEndpoint {
        &self.endpoint
    }

    fn launch(
        self: Box<Self>,
        descriptor: PublicLaunchDescriptor,
        cancellation: Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<LaunchedTargetIo, PlatformFailure> {
        if let Some(kind) = self.validation {
            return Err(failure(kind));
        }
        ensure_active(&cancellation, deadline)?;
        if descriptor.canonical_bytes().len() > DESCRIPTOR_LIMIT {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let client = AdbClient {
            executable: &self.adb_path,
            serial: &self.serial,
            runner: self.runner.as_ref(),
        };
        let snapshot = ArtifactSnapshot::create(
            &self.artifact,
            self.artifact_digest,
            &self.package,
            &cancellation,
            deadline,
        )?;
        let mut attempted_install = false;
        let install = (|| {
            client.probe(&cancellation, deadline)?;
            client.require_online(&cancellation, deadline)?;
            client.require_usb_transport(&cancellation, deadline)?;
            if client.package_present(&self.package, &cancellation, deadline)? {
                // The on-device APK cannot be hashed, so a present package is
                // not identity-equivalent to this Host artifact.
                return Err(failure(PlatformFailureKind::Rejected));
            }
            attempted_install = true;
            client.install(&snapshot.path, &cancellation, deadline)
        })();
        let snapshot_cleanup = snapshot.cleanup();
        if snapshot_cleanup.is_err() {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        if let Err(original) = install {
            return Err(if attempted_install {
                revert_failed_install(&client, &self.package, original)
            } else {
                original
            });
        }

        if let Err(original) = client.force_stop(&self.package, &cancellation, deadline) {
            return Err(revert_failed_install(&client, &self.package, original));
        }
        if let Err(original) = client.require_pid_absent(&self.package, &cancellation, deadline) {
            return Err(revert_failed_install(&client, &self.package, original));
        }

        let component = format!("{}{BOOTSTRAP_ACTIVITY}", self.package);
        let encoded = URL_SAFE_NO_PAD.encode(descriptor.canonical_bytes());
        if let Err(original) = client.start(&component, &encoded, &cancellation, deadline) {
            return rollback_after_started(
                original,
                &client,
                &self.package,
                None,
                true,
                |_| Ok(()),
            );
        }
        let pid = match client.prove_launch_pid(&self.package, &cancellation, deadline) {
            Ok(pid) => pid,
            Err(original) => {
                return rollback_after_started(
                    original,
                    &client,
                    &self.package,
                    None,
                    true,
                    |_| Ok(()),
                );
            }
        };
        if let Err(original) =
            client.require_remote_absent(&self.localabstract, &cancellation, deadline)
        {
            return rollback_after_started(
                original,
                &client,
                &self.package,
                Some(pid),
                true,
                |_| Ok(()),
            );
        }

        let port = match client.create_forward(&self.localabstract, &cancellation, deadline) {
            Ok(port) => port,
            Err(original) => {
                return rollback_after_started(
                    original,
                    &client,
                    &self.package,
                    Some(pid),
                    true,
                    |cleanup_deadline| {
                        client.remove_by_remote(&self.localabstract, cleanup_deadline)
                    },
                );
            }
        };
        if let Err(original) =
            client.require_exact_forward(port, &self.localabstract, &cancellation, deadline)
        {
            return rollback_after_started(
                original,
                &client,
                &self.package,
                Some(pid),
                true,
                |cleanup_deadline| client.remove_exact(port, &self.localabstract, cleanup_deadline),
            );
        }

        let bootstrap = match raw::connect(port, &cancellation, deadline) {
            Ok(raw) => Arc::new(raw),
            Err(original) => {
                return rollback_after_started(
                    original,
                    &client,
                    &self.package,
                    Some(pid),
                    true,
                    |cleanup_deadline| {
                        client.remove_exact(port, &self.localabstract, cleanup_deadline)
                    },
                );
            }
        };
        let connector: Arc<dyn RawConnector> = Arc::new(LoopbackConnector::new(port));
        let cleanup = Box::new(AndroidDeviceCleanup {
            adb_path: self.adb_path.clone(),
            runner: Arc::clone(&self.runner),
            serial: self.serial.clone(),
            package: self.package.clone(),
            localabstract: self.localabstract.clone(),
            port,
            pid,
            installed_by_lease: true,
        });
        Ok(LaunchedTargetIo::new(bootstrap, connector, cleanup))
    }

    fn abort(
        self: Box<Self>,
        _cancellation: Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        Ok(())
    }
}

fn rollback_after(
    original: PlatformFailure,
    rollback: impl FnOnce() -> Result<(), PlatformFailure>,
) -> Result<LaunchedTargetIo, PlatformFailure> {
    let result = match rollback() {
        Ok(()) => original,
        Err(_) => PlatformFailure::cleanup_failed_after(original.primary_kind()),
    };
    Err(result)
}

fn rollback_after_started(
    original: PlatformFailure,
    client: &AdbClient<'_>,
    package: &str,
    pid: Option<u32>,
    installed_by_lease: bool,
    remove_forward: impl FnOnce(AbsoluteDeadline) -> Result<(), PlatformFailure>,
) -> Result<LaunchedTargetIo, PlatformFailure> {
    rollback_after(original, || {
        let cleanup_deadline = rollback_deadline()?;
        let cancellation = Cancellation::new();
        let forward = remove_forward(cleanup_deadline);
        let stopped = match pid {
            Some(pid) => client.terminate_owned_pid(package, pid, &cancellation, cleanup_deadline),
            None => client.require_absent_for_uninstall(package, &cancellation, cleanup_deadline),
        };
        let uninstalled = if installed_by_lease {
            client.uninstall_owned(package, &cancellation, cleanup_deadline)
        } else {
            Ok(())
        };
        forward.and(stopped).and(uninstalled)
    })
}

fn revert_failed_install(
    client: &AdbClient<'_>,
    package: &str,
    original: PlatformFailure,
) -> PlatformFailure {
    let Ok(deadline) = rollback_deadline() else {
        return failure(PlatformFailureKind::CleanupFailed);
    };
    let cancellation = Cancellation::new();
    if client
        .uninstall_owned(package, &cancellation, deadline)
        .is_ok()
    {
        original
    } else {
        failure(PlatformFailureKind::CleanupFailed)
    }
}

struct AndroidDeviceCleanup {
    adb_path: PathBuf,
    runner: Arc<dyn CommandRunner>,
    serial: String,
    package: String,
    localabstract: String,
    port: u16,
    pid: u32,
    installed_by_lease: bool,
}

impl CleanupReceipt for AndroidDeviceCleanup {
    fn cleanup(
        self: Box<Self>,
        cancellation: Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let client = AdbClient {
            executable: &self.adb_path,
            serial: &self.serial,
            runner: self.runner.as_ref(),
        };
        client
            .remove_exact_with_cancellation(self.port, &self.localabstract, &cancellation, deadline)
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
        client.terminate_owned_pid(&self.package, self.pid, &cancellation, deadline)?;
        if self.installed_by_lease {
            client.uninstall_owned(&self.package, &cancellation, deadline)?;
        }
        Ok(())
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
    ) -> Result<LaunchedTargetIo, PlatformFailure> {
        Err(failure(PlatformFailureKind::Unavailable))
    }

    fn abort(
        self: Box<Self>,
        _cancellation: Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        Ok(())
    }
}

struct AdbClient<'a> {
    executable: &'a Path,
    serial: &'a str,
    runner: &'a dyn CommandRunner,
}

impl AdbClient<'_> {
    fn run(
        &self,
        arguments: &[OsString],
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<String, PlatformFailure> {
        let output = self.invoke(arguments, cancellation, deadline)?;
        if !output.success {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        decode_stdout(output)
    }

    fn run_query(
        &self,
        arguments: &[OsString],
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<String, PlatformFailure> {
        let output = self.invoke(arguments, cancellation, deadline)?;
        let success = output.success;
        let stdout = decode_stdout(output)?;
        if success {
            return Ok(stdout);
        }
        if strip_one_line_ending(&stdout)? == "" {
            Ok(String::new())
        } else {
            Err(failure(PlatformFailureKind::Unavailable))
        }
    }

    fn invoke(
        &self,
        arguments: &[OsString],
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure> {
        self.runner.run(
            self.executable,
            self.serial,
            arguments,
            cancellation,
            deadline,
        )
    }

    fn probe(
        &self,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let output = self.run(&strings(&["help"]), cancellation, deadline)?;
        if output.contains("tcp:0") && output.contains("localabstract:") {
            Ok(())
        } else {
            Err(failure(PlatformFailureKind::Rejected))
        }
    }

    fn require_online(
        &self,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let output = self.run(&strings(&["get-state"]), cancellation, deadline)?;
        require_exact_line(&output, "device")
    }

    fn require_usb_transport(
        &self,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let output = self.run(&strings(&["devices", "-l"]), cancellation, deadline)?;
        parse_devices_l_usb(&output, self.serial)
    }

    fn package_present(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<bool, PlatformFailure> {
        let output = self.run(
            &strings(&["shell", "pm", "list", "packages", package]),
            cancellation,
            deadline,
        )?;
        parse_package_present(&output, package)
    }

    fn pidof(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Vec<u32>, PlatformFailure> {
        let output = self.run_query(
            &strings(&["shell", "pidof", package]),
            cancellation,
            deadline,
        )?;
        parse_pidof(&output)
    }

    fn require_pid_absent(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        if self.pidof(package, cancellation, deadline)?.is_empty() {
            Ok(())
        } else {
            Err(failure(PlatformFailureKind::Rejected))
        }
    }

    fn prove_launch_pid(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<u32, PlatformFailure> {
        let pids = self.pidof(package, cancellation, deadline)?;
        match pids.as_slice() {
            [pid] => Ok(*pid),
            _ => Err(failure(PlatformFailureKind::Rejected)),
        }
    }

    fn terminate_owned_pid(
        &self,
        package: &str,
        pid: u32,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let pids = self
            .pidof(package, cancellation, deadline)
            .map_err(into_cleanup_failed)?;
        if !pids.contains(&pid) {
            return if pids.is_empty() {
                Ok(())
            } else {
                Err(failure(PlatformFailureKind::CleanupFailed))
            };
        }
        if pids.as_slice() != [pid] {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        self.force_stop(package, cancellation, deadline)
            .map_err(into_cleanup_failed)?;
        let after = self
            .pidof(package, cancellation, deadline)
            .map_err(into_cleanup_failed)?;
        if after.is_empty() {
            Ok(())
        } else {
            Err(failure(PlatformFailureKind::CleanupFailed))
        }
    }

    fn require_absent_for_uninstall(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        match self.pidof(package, cancellation, deadline) {
            Ok(pids) if pids.is_empty() => Ok(()),
            _ => Err(failure(PlatformFailureKind::CleanupFailed)),
        }
    }

    fn uninstall_owned(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        self.require_absent_for_uninstall(package, cancellation, deadline)?;
        let present = self
            .package_present(package, cancellation, deadline)
            .map_err(into_cleanup_failed)?;
        if !present {
            return Ok(());
        }
        let output = self
            .run(&strings(&["uninstall", package]), cancellation, deadline)
            .map_err(into_cleanup_failed)?;
        parse_install(&output).map_err(into_cleanup_failed)
    }

    fn install(
        &self,
        artifact: &Path,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let arguments = vec![
            OsString::from("install"),
            OsString::from("-t"),
            artifact.as_os_str().to_owned(),
        ];
        let output = self.run(&arguments, cancellation, deadline)?;
        parse_install(&output)
    }

    fn start(
        &self,
        component: &str,
        descriptor: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let output = self.run(
            &strings(&[
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                component,
                "--es",
                DESCRIPTOR_EXTRA,
                descriptor,
            ]),
            cancellation,
            deadline,
        )?;
        parse_start(&output, component)
    }

    fn force_stop(
        &self,
        package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let output = self.run(
            &strings(&["shell", "am", "force-stop", package]),
            cancellation,
            deadline,
        )?;
        require_exact_line(&output, "")
    }

    fn create_forward(
        &self,
        localabstract: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<u16, PlatformFailure> {
        let remote = format!("localabstract:{localabstract}");
        let output = self.run(
            &strings(&["forward", "tcp:0", &remote]),
            cancellation,
            deadline,
        )?;
        parse_port(&output)
    }

    fn require_exact_forward(
        &self,
        port: u16,
        localabstract: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let entries = self.forward_list(cancellation, deadline)?;
        let local = format!("tcp:{port}");
        let remote = format!("localabstract:{localabstract}");
        let local_entries: Vec<_> = entries
            .iter()
            .filter(|entry| entry.serial == self.serial && entry.local == local)
            .collect();
        if local_entries.len() == 1 && local_entries[0].remote == remote {
            Ok(())
        } else {
            Err(failure(PlatformFailureKind::Rejected))
        }
    }

    fn require_remote_absent(
        &self,
        localabstract: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let entries = self.forward_list(cancellation, deadline)?;
        let remote = format!("localabstract:{localabstract}");
        if entries
            .iter()
            .any(|entry| entry.serial == self.serial && entry.remote == remote)
        {
            Err(failure(PlatformFailureKind::Rejected))
        } else {
            Ok(())
        }
    }

    fn forward_list(
        &self,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Vec<ForwardEntry>, PlatformFailure> {
        let output = self.run(&strings(&["forward", "--list"]), cancellation, deadline)?;
        parse_forward_list(&output)
    }

    fn remove_by_remote(
        &self,
        localabstract: &str,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let cancellation = Cancellation::new();
        let entries = self.forward_list(&cancellation, deadline)?;
        let remote = format!("localabstract:{localabstract}");
        let mut matches = entries
            .iter()
            .filter(|entry| entry.serial == self.serial && entry.remote == remote);
        let first = matches
            .next()
            .map(|entry| parse_local_port(&entry.local))
            .transpose()?;
        let Some(port) = first else {
            return Ok(());
        };
        if matches.next().is_some() {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        self.remove_exact(port, localabstract, deadline)
    }

    fn remove_exact(
        &self,
        port: u16,
        localabstract: &str,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        self.remove_exact_with_cancellation(port, localabstract, &Cancellation::new(), deadline)
    }

    fn remove_exact_with_cancellation(
        &self,
        port: u16,
        localabstract: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let entries = self.forward_list(cancellation, deadline)?;
        let local = format!("tcp:{port}");
        let remote = format!("localabstract:{localabstract}");
        let local_entries: Vec<_> = entries
            .iter()
            .filter(|entry| entry.serial == self.serial && entry.local == local)
            .collect();
        if local_entries.is_empty() {
            return Ok(());
        }
        if local_entries.len() != 1 || local_entries[0].remote != remote {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        let output = self.run(
            &strings(&["forward", "--remove", &local]),
            cancellation,
            deadline,
        )?;
        require_exact_line(&output, "")?;
        let after = self.forward_list(cancellation, deadline)?;
        if after
            .iter()
            .any(|entry| entry.serial == self.serial && entry.local == local)
        {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        Ok(())
    }
}

struct ForwardEntry {
    serial: String,
    local: String,
    remote: String,
}

fn decode_stdout(output: ProcessOutput) -> Result<String, PlatformFailure> {
    if !benign_stderr(&output.stderr) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    String::from_utf8(output.stdout).map_err(|_| failure(PlatformFailureKind::Rejected))
}

fn parse_forward_list(output: &str) -> Result<Vec<ForwardEntry>, PlatformFailure> {
    let normalized = normalize_forward_list(output)?;
    if normalized.is_empty() {
        return Ok(Vec::new());
    }
    normalized
        .split('\n')
        .map(|line| {
            let fields: Vec<_> = line.split(' ').collect();
            if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            if fields.iter().any(|field| !safe_tool_field(field)) {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            Ok(ForwardEntry {
                serial: fields[0].to_owned(),
                local: fields[1].to_owned(),
                remote: fields[2].to_owned(),
            })
        })
        .collect()
}

fn normalize_forward_list(output: &str) -> Result<&str, PlatformFailure> {
    // `adb forward --list` may append one empty record after either an empty
    // list or one or more mappings. Keep that exception scoped to this parser.
    if let Some(entries) = output.strip_suffix("\n\n") {
        if entries.ends_with('\n') {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(entries)
    } else {
        strip_one_line_ending(output)
    }
}

fn parse_port(output: &str) -> Result<u16, PlatformFailure> {
    let line = strip_one_line_ending(output)?;
    if line.is_empty() || !line.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    line.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))
}

fn parse_local_port(local: &str) -> Result<u16, PlatformFailure> {
    local
        .strip_prefix("tcp:")
        .ok_or_else(|| failure(PlatformFailureKind::CleanupFailed))
        .and_then(|port| {
            port.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| failure(PlatformFailureKind::CleanupFailed))
        })
}

fn parse_install(output: &str) -> Result<(), PlatformFailure> {
    let value = strip_one_line_ending(output)?;
    if matches!(
        value,
        "Success"
            | "Performing Streamed Install\nSuccess"
            | "Performing Push Install\nSuccess"
            | "Performing Incremental Install\nSuccess"
    ) {
        Ok(())
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

fn parse_start(output: &str, component: &str) -> Result<(), PlatformFailure> {
    let value = strip_one_line_ending(output)?;
    let mut status = 0;
    let mut activity = 0;
    let mut complete = 0;
    let mut launch_state = 0;
    let mut this_time = 0;
    let mut total_time = 0;
    let mut wait_time = 0;
    for line in value.split('\n') {
        if line == "Status: ok" {
            status += 1;
        } else if line == format!("Activity: {component}") {
            activity += 1;
        } else if line == "Complete" {
            complete += 1;
        } else if line == "LaunchState: COLD" || unknown_launch_state(line) {
            launch_state += 1;
        } else if numeric_field(line, "ThisTime: ") {
            this_time += 1;
        } else if numeric_field(line, "TotalTime: ") {
            total_time += 1;
        } else if numeric_field(line, "WaitTime: ") {
            wait_time += 1;
        } else if line.starts_with("Stopping: ") || line.starts_with("Starting: Intent { ") {
            continue;
        } else {
            return Err(failure(PlatformFailureKind::Rejected));
        }
    }
    // Older Android releases report ThisTime instead of LaunchState. Require
    // their complete timing tuple, not an incomplete modern transcript.
    let legacy = launch_state == 0 && this_time == 1 && total_time == 1 && wait_time == 1;
    let modern = launch_state == 1 && this_time == 0;
    if status == 1 && activity == 1 && complete == 1 && (modern || legacy) {
        Ok(())
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

fn numeric_field(line: &str, prefix: &str) -> bool {
    line.strip_prefix(prefix)
        .is_some_and(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
}

fn unknown_launch_state(line: &str) -> bool {
    line.strip_prefix("LaunchState: UNKNOWN (")
        .and_then(|value| value.strip_suffix(')'))
        .is_some_and(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
}

fn parse_package_present(output: &str, package: &str) -> Result<bool, PlatformFailure> {
    let value = strip_one_line_ending(output)?;
    if value.is_empty() {
        return Ok(false);
    }
    if value == format!("package:{package}") {
        Ok(true)
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

fn parse_pidof(output: &str) -> Result<Vec<u32>, PlatformFailure> {
    let value = strip_one_line_ending(output)?;
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut pids = Vec::new();
    for field in value.split(' ') {
        if field.is_empty() {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let pid = field
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        if pids.contains(&pid) {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        pids.push(pid);
    }
    Ok(pids)
}

fn parse_devices_l_usb(output: &str, serial: &str) -> Result<(), PlatformFailure> {
    let value = strip_one_line_ending(output)?;
    let rest = value
        .strip_prefix("List of devices attached")
        .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest);
    if rest.is_empty() {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let mut matches = 0_u8;
    let mut usb_ok = false;
    for line in rest.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some((found, tail)) = line.split_once(|byte: char| byte.is_ascii_whitespace()) else {
            return Err(failure(PlatformFailureKind::Rejected));
        };
        if found != serial {
            continue;
        }
        matches = matches
            .checked_add(1)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        let tail = tail.trim_start();
        let mut fields = tail.split_whitespace();
        let state = fields
            .next()
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        if state != "device" {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        let mut saw_usb = false;
        for field in fields {
            let Some((key, val)) = field.split_once(':') else {
                return Err(failure(PlatformFailureKind::Rejected));
            };
            if val.is_empty() || !val.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(failure(PlatformFailureKind::Rejected));
            }
            match key {
                "usb" => saw_usb = true,
                "product" | "model" | "device" | "transport_id" => {}
                _ => return Err(failure(PlatformFailureKind::Rejected)),
            }
        }
        usb_ok = saw_usb;
    }
    if matches == 0 {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    if matches != 1 {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    if !usb_ok {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    Ok(())
}

fn require_exact_line(output: &str, expected: &str) -> Result<(), PlatformFailure> {
    if strip_one_line_ending(output)? == expected {
        Ok(())
    } else {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

fn strip_one_line_ending(output: &str) -> Result<&str, PlatformFailure> {
    let value = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if value.ends_with('\r') || value.contains('\0') || value.ends_with('\n') {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(value)
}

fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn benign_stderr(stderr: &[u8]) -> bool {
    stderr.is_empty()
        || stderr
            == b"* daemon not running; starting now at tcp:5037\n* daemon started successfully\n"
}

fn validate_selection(
    selection: &TargetSelection,
    adb_path: &Path,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    if selection.platform() != Platform::AndroidDevice
        || !android_usb_serial(selection.device_selector())
        || !android_package(selection.app_id())
        || adb_path.as_os_str().is_empty()
        || !Path::new(selection.artifact_path()).is_absolute()
        || deadline.value() == 0
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn android_usb_serial(value: &str) -> bool {
    // Syntactic filter only. USB transport is proven later by `adb devices -l`
    // `usb:`; wireless/TCP serials can look identical here.
    (1..=255).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_graphic())
        && !value.starts_with("emulator-")
        && !is_coredevice_uuid(value)
        && !is_ios_udid(value)
}

fn is_coredevice_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn is_ios_udid(value: &str) -> bool {
    let dashed = value.len() == 25
        && value.as_bytes().get(8) == Some(&b'-')
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 8 {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    let classic = value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    dashed || classic
}

fn android_package(value: &str) -> bool {
    if !(3..=255).contains(&value.len()) || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    value.split('.').all(|segment| {
        segment
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn safe_tool_field(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn resolve_production_adb() -> Result<PathBuf, PlatformFailure> {
    if let Some(adb) = env::var_os("APPPILOTKIT_ANDROID_ADB") {
        return resolve_adb_executable(Path::new(&adb));
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
        .ok_or_else(|| failure(PlatformFailureKind::Unavailable))?;
    if !sdk_root.is_absolute() {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    resolve_adb_executable(&sdk_root.join("platform-tools/adb"))
}

fn resolve_adb_executable(adb: &Path) -> Result<PathBuf, PlatformFailure> {
    if !adb.is_absolute() {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    let adb = fs::canonicalize(adb).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    let metadata = fs::metadata(&adb).map_err(|_| failure(PlatformFailureKind::Unavailable))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(failure(PlatformFailureKind::Unavailable));
    }
    Ok(adb)
}

struct ArtifactSnapshot {
    directory: PathBuf,
    path: PathBuf,
}

impl ArtifactSnapshot {
    fn create(
        source: &Path,
        expected: [u8; 32],
        expected_package: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Self, PlatformFailure> {
        ensure_active(cancellation, deadline)?;
        let mut source = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)
            .map_err(|_| failure(PlatformFailureKind::Rejected))?;
        let length = source
            .metadata()
            .map_err(|_| failure(PlatformFailureKind::Rejected))?;
        if !length.file_type().is_file() || length.len() == 0 || length.len() > ARTIFACT_LIMIT {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let length = length.len();
        let directory = create_snapshot_directory(cancellation, deadline)?;
        let path = directory.join("selected.apk");
        let mut destination = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(_) => {
                return if fs::remove_dir(&directory).is_ok() {
                    Err(failure(PlatformFailureKind::Rejected))
                } else {
                    Err(failure(PlatformFailureKind::CleanupFailed))
                };
            }
        };
        let copy = copy_verified(
            &mut source,
            &mut destination,
            length,
            expected,
            cancellation,
            deadline,
        )
        .and_then(|()| {
            destination
                .flush()
                .map_err(|_| failure(PlatformFailureKind::Rejected))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400))
                .map_err(|_| failure(PlatformFailureKind::Rejected))
        });
        drop(destination);
        let verified = copy.and_then(|()| {
            apk::validate_apk_manifest(&path, expected_package)
                .map_err(|()| failure(PlatformFailureKind::Rejected))
        });
        if let Err(original) = verified {
            return match cleanup_snapshot(&directory, &path) {
                Ok(()) => Err(original),
                Err(()) => Err(failure(PlatformFailureKind::CleanupFailed)),
            };
        }
        Ok(Self { directory, path })
    }

    fn cleanup(self) -> Result<(), PlatformFailure> {
        cleanup_snapshot(&self.directory, &self.path)
            .map_err(|()| failure(PlatformFailureKind::CleanupFailed))
    }
}

fn rollback_deadline() -> Result<AbsoluteDeadline, PlatformFailure> {
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?
            .as_millis(),
    )
    .map_err(|_| failure(PlatformFailureKind::CleanupFailed))?;
    AbsoluteDeadline::new(now.saturating_add(FORWARD_ROLLBACK_MS))
        .map_err(|_| failure(PlatformFailureKind::CleanupFailed))
}

fn create_snapshot_directory(
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<PathBuf, PlatformFailure> {
    for _ in 0..4 {
        ensure_active(cancellation, deadline)?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| failure(PlatformFailureKind::Internal))?;
        let suffix = hex(&random);
        let directory = std::env::temp_dir().join(format!(
            "apppilotkit-android-device-adapter-{}-{suffix}",
            std::process::id()
        ));
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(failure(PlatformFailureKind::Rejected)),
        }
    }
    Err(failure(PlatformFailureKind::Rejected))
}

fn copy_verified(
    source: &mut File,
    destination: &mut File,
    length: u64,
    expected: [u8; 32],
    cancellation: &Cancellation,
    deadline: AbsoluteDeadline,
) -> Result<(), PlatformFailure> {
    let mut total = 0_u64;
    let mut digest = Sha256::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        ensure_active(cancellation, deadline)?;
        let count = source
            .read(&mut chunk)
            .map_err(|_| failure(PlatformFailureKind::Rejected))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count).map_err(|_| failure(PlatformFailureKind::Internal))?)
            .ok_or_else(|| failure(PlatformFailureKind::Rejected))?;
        if total > ARTIFACT_LIMIT {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        destination
            .write_all(&chunk[..count])
            .map_err(|_| failure(PlatformFailureKind::Rejected))?;
        digest.update(&chunk[..count]);
    }
    ensure_active(cancellation, deadline)?;
    if total != length || <[u8; 32]>::from(digest.finalize()) != expected {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn cleanup_snapshot(directory: &Path, path: &Path) -> Result<(), ()> {
    let file = fs::remove_file(path);
    let directory = fs::remove_dir(directory);
    if file.is_ok() && directory.is_ok() {
        Ok(())
    } else {
        Err(())
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn random_localabstract() -> (String, Option<PlatformFailure>) {
    let mut random = [0_u8; 16];
    let error = getrandom::fill(&mut random)
        .err()
        .map(|_| failure(PlatformFailureKind::Internal));
    let mut name = String::with_capacity(LOCALABSTRACT_PREFIX.len() + random.len() * 2);
    name.push_str(LOCALABSTRACT_PREFIX);
    name.push_str(&hex(&random));
    (name, error)
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
