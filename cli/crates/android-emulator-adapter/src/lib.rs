//! Exact-serial Android Emulator adapter for the publish-disabled Host raw SPI.

mod apk;
mod process;
mod raw;

use std::{
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

/// Adapter for exactly one caller-selected Android Emulator Target at a time.
pub struct AndroidEmulatorAdapter {
    adb_path: PathBuf,
    runner: Arc<dyn CommandRunner>,
}

impl AndroidEmulatorAdapter {
    pub fn new(adb_path: impl Into<PathBuf>) -> Self {
        Self {
            adb_path: adb_path.into(),
            runner: Arc::new(SystemCommandRunner),
        }
    }

    #[cfg(test)]
    fn with_runner(adb_path: impl Into<PathBuf>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            adb_path: adb_path.into(),
            runner,
        }
    }
}

impl PlatformTargetAdapter for AndroidEmulatorAdapter {
    fn begin_launch(
        &self,
        selection: TargetSelection,
        deadline: AbsoluteDeadline,
    ) -> Box<dyn PendingLaunch> {
        let (localabstract, random_failure) = random_localabstract();
        let validation = validate_selection(&selection, &self.adb_path, deadline)
            .and(random_failure.map_or(Ok(()), Err));
        let endpoint = LaunchEndpoint::android_local_abstract(localabstract.clone())
            .unwrap_or_else(|_| unreachable!("adapter-generated endpoint is valid"));
        Box::new(AndroidPendingLaunch {
            adb_path: self.adb_path.clone(),
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

struct AndroidPendingLaunch {
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

impl PendingLaunch for AndroidPendingLaunch {
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
        let install = (|| {
            client.probe(&cancellation, deadline)?;
            client.require_online(&cancellation, deadline)?;
            client.install(&snapshot.path, &cancellation, deadline)
        })();
        let snapshot_cleanup = snapshot.cleanup();
        if snapshot_cleanup.is_err() {
            return Err(failure(PlatformFailureKind::CleanupFailed));
        }
        install?;
        let component = format!("{}{BOOTSTRAP_ACTIVITY}", self.package);
        let encoded = URL_SAFE_NO_PAD.encode(descriptor.canonical_bytes());
        client.start(&component, &encoded, &cancellation, deadline)?;
        if let Err(original) =
            client.require_remote_absent(&self.localabstract, &cancellation, deadline)
        {
            return rollback_after_started(original, &client, &self.package, |_| Ok(()));
        }

        let port = match client.create_forward(&self.localabstract, &cancellation, deadline) {
            Ok(port) => port,
            Err(original) => {
                return rollback_after_started(
                    original,
                    &client,
                    &self.package,
                    |cleanup_deadline| {
                        client.remove_by_remote(&self.localabstract, cleanup_deadline)
                    },
                );
            }
        };
        if let Err(original) =
            client.require_exact_forward(port, &self.localabstract, &cancellation, deadline)
        {
            return rollback_after_started(original, &client, &self.package, |cleanup_deadline| {
                client.remove_exact(port, &self.localabstract, cleanup_deadline)
            });
        }

        let bootstrap = match raw::connect(port, &cancellation, deadline) {
            Ok(raw) => Arc::new(raw),
            Err(original) => {
                return rollback_after_started(
                    original,
                    &client,
                    &self.package,
                    |cleanup_deadline| {
                        client.remove_exact(port, &self.localabstract, cleanup_deadline)
                    },
                );
            }
        };
        let connector: Arc<dyn RawConnector> = Arc::new(LoopbackConnector::new(port));
        let cleanup = Box::new(AndroidCleanup {
            adb_path: self.adb_path.clone(),
            runner: Arc::clone(&self.runner),
            serial: self.serial.clone(),
            localabstract: self.localabstract.clone(),
            port,
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

/// Once `am start` confirms this launch, any later setup failure must leave
/// that exact app process stopped. The cleanup deadline is intentionally
/// independent of the expired launch deadline; a failed forward cleanup does
/// not skip the process cleanup.
fn rollback_after_started(
    original: PlatformFailure,
    client: &AdbClient<'_>,
    package: &str,
    remove_forward: impl FnOnce(AbsoluteDeadline) -> Result<(), PlatformFailure>,
) -> Result<LaunchedTargetIo, PlatformFailure> {
    rollback_after(original, || {
        let cleanup_deadline = rollback_deadline()?;
        let forward = remove_forward(cleanup_deadline);
        let stopped = client.force_stop(package, cleanup_deadline);
        forward.and(stopped)
    })
}

struct AndroidCleanup {
    adb_path: PathBuf,
    runner: Arc<dyn CommandRunner>,
    serial: String,
    localabstract: String,
    port: u16,
}

impl CleanupReceipt for AndroidCleanup {
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
            .map_err(|_| failure(PlatformFailureKind::CleanupFailed))
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
        let ProcessOutput { stdout, stderr } = self.runner.run(
            self.executable,
            self.serial,
            arguments,
            cancellation,
            deadline,
        )?;
        if !benign_stderr(&stderr) {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        String::from_utf8(stdout).map_err(|_| failure(PlatformFailureKind::Rejected))
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

    fn install(
        &self,
        artifact: &Path,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        let arguments = vec![
            OsString::from("install"),
            OsString::from("-r"),
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

    fn force_stop(&self, package: &str, deadline: AbsoluteDeadline) -> Result<(), PlatformFailure> {
        let output = self.run(
            &strings(&["shell", "am", "force-stop", package]),
            &Cancellation::new(),
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
    for line in value.split('\n') {
        if line == "Status: ok" {
            status += 1;
        } else if line == format!("Activity: {component}") {
            activity += 1;
        } else if line == "Complete" {
            complete += 1;
        } else if line == "LaunchState: COLD" || unknown_launch_state(line) {
            launch_state += 1;
        } else if line.starts_with("Stopping: ")
            || line.starts_with("Starting: Intent { ")
            || numeric_field(line, "TotalTime: ")
            || numeric_field(line, "WaitTime: ")
            || numeric_field(line, "ThisTime: ")
        {
            continue;
        } else {
            return Err(failure(PlatformFailureKind::Rejected));
        }
    }
    if status == 1 && activity == 1 && complete == 1 && launch_state == 1 {
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
    if selection.platform() != Platform::AndroidEmulator
        || !android_emulator_serial(selection.device_selector())
        || !android_package(selection.app_id())
        || adb_path.as_os_str().is_empty()
        || !Path::new(selection.artifact_path()).is_absolute()
        || deadline.value() == 0
    {
        return Err(failure(PlatformFailureKind::Rejected));
    }
    Ok(())
}

fn android_emulator_serial(value: &str) -> bool {
    (1..=255).contains(&value.len())
        && value
            .strip_prefix("emulator-")
            .is_some_and(|port| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
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
            "apppilotkit-android-adapter-{}-{suffix}",
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

#[cfg(test)]
mod tests;
