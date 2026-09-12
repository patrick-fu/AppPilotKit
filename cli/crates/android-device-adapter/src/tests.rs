use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, CleanupReceipt, PlatformFailure, PlatformFailureKind,
    PlatformTargetAdapter, PublicLaunchDescriptor, TargetSelection,
};
use sha2::{Digest, Sha256};

use super::*;

const SERIAL: &str = "R58N123456A";
const PACKAGE: &str = "dev.apppilotkit.smokehost";
const COMPONENT: &str = "dev.apppilotkit.smokehost/.AppPilotKitBootstrapActivity";
const SECRET_CANARY: &str = "APPPILOTKIT_PRIVATE_SECRET_CANARY_7d14";
const OWNED_PID: u32 = 42_424;
const FOREIGN_PID: u32 = 99;

fn deadline() -> AbsoluteDeadline {
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("timestamp");
    must(AbsoluteDeadline::new(now + 5_000), "deadline")
}

fn must<T>(result: Result<T, PlatformFailure>, message: &str) -> T {
    result.unwrap_or_else(|_| panic!("{message}"))
}

fn selection(platform: Platform, serial: &str, package: &str, artifact: &str) -> TargetSelection {
    must(
        TargetSelection::new(
            platform,
            serial.to_owned(),
            package.to_owned(),
            artifact.to_owned(),
            [7; 32],
        ),
        "selection",
    )
}

fn selection_for_artifact(artifact: &TestArtifact) -> TargetSelection {
    must(
        TargetSelection::new(
            Platform::AndroidDevice,
            SERIAL.to_owned(),
            PACKAGE.to_owned(),
            artifact.path.to_string_lossy().into_owned(),
            artifact.digest,
        ),
        "artifact selection",
    )
}

struct TestArtifact {
    directory: PathBuf,
    path: PathBuf,
    digest: [u8; 32],
}

impl TestArtifact {
    fn real() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "apppilotkit-android-device-artifact-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("artifact directory");
        let manifest = directory.join("AndroidManifest.xml");
        fs::write(
            &manifest,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="{PACKAGE}">
  <uses-sdk android:minSdkVersion="26" android:targetSdkVersion="36" />
  <application android:debuggable="true" android:hasCode="false">
    <activity android:name=".AppPilotKitBootstrapActivity" android:exported="true" />
  </application>
</manifest>
"#
            ),
        )
        .expect("manifest");
        let sdk = std::env::var_os("ANDROID_SDK_ROOT")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("ANDROID_HOME").map(PathBuf::from))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join("Library/Android/sdk"))
            })
            .expect("Android SDK");
        let path = directory.join("selected.apk");
        let status = Command::new(sdk.join("build-tools/36.0.0/aapt2"))
            .args([
                "link",
                "--debug-mode",
                "--no-compile-sdk-metadata",
                "--min-sdk-version",
                "26",
                "--target-sdk-version",
                "36",
                "-o",
            ])
            .arg(&path)
            .arg("--manifest")
            .arg(&manifest)
            .arg("-I")
            .arg(sdk.join("platforms/android-36/android.jar"))
            .status()
            .expect("aapt2 link");
        assert!(status.success(), "aapt2 link failed");
        let bytes = fs::read(&path).expect("APK bytes");
        Self {
            directory,
            path,
            digest: Sha256::digest(bytes).into(),
        }
    }
}

impl Drop for TestArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn descriptor() -> PublicLaunchDescriptor {
    must(
        PublicLaunchDescriptor::from_d2_canonical_bytes(b"canonical-public-descriptor".to_vec()),
        "descriptor",
    )
}

fn start_transcript() -> String {
    format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    )
}

fn stdout(text: impl AsRef<[u8]>) -> ProcessOutput {
    ProcessOutput {
        stdout: text.as_ref().to_vec(),
        stderr: Vec::new(),
        success: true,
    }
}

struct FakeAdb {
    serial: String,
    package_present: AtomicBool,
    pids: Mutex<Vec<u32>>,
    next_pid: AtomicU32,
    install_calls: AtomicUsize,
    uninstall_calls: AtomicUsize,
    force_stop_calls: AtomicUsize,
    start_calls: AtomicUsize,
    seen: Mutex<Vec<Vec<String>>>,
    pidof_fails: AtomicBool,
    package_query_fails: AtomicBool,
    last_install_path: Mutex<Option<String>>,
    artifact_bytes: Mutex<Option<Vec<u8>>>,
}

impl FakeAdb {
    fn new(_port: u16) -> Arc<Self> {
        Arc::new(Self {
            serial: SERIAL.to_owned(),
            package_present: AtomicBool::new(false),
            pids: Mutex::new(Vec::new()),
            next_pid: AtomicU32::new(OWNED_PID),
            install_calls: AtomicUsize::new(0),
            uninstall_calls: AtomicUsize::new(0),
            force_stop_calls: AtomicUsize::new(0),
            start_calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            pidof_fails: AtomicBool::new(false),
            package_query_fails: AtomicBool::new(false),
            last_install_path: Mutex::new(None),
            artifact_bytes: Mutex::new(None),
        })
    }
}

impl CommandRunner for FakeAdb {
    fn run(
        &self,
        _executable: &Path,
        serial: &str,
        arguments: &[OsString],
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure> {
        assert_eq!(serial, self.serial);
        let args: Vec<String> = arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.seen.lock().expect("seen").push(args.clone());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [help] if *help == "help" => Ok(stdout("forward tcp:0 localabstract:\n")),
            [state] if *state == "get-state" => Ok(stdout("device\n")),
            ["devices", "-l"] => Ok(stdout(usb_devices_l(&self.serial))),
            ["shell", "pm", "list", "packages", package] if *package == PACKAGE => {
                if self.package_query_fails.load(Ordering::SeqCst) {
                    return Err(failure(PlatformFailureKind::Unavailable));
                }
                if self.package_present.load(Ordering::SeqCst) {
                    Ok(stdout(format!("package:{PACKAGE}\n")))
                } else {
                    Ok(stdout(""))
                }
            }
            ["install", "-t", snapshot] => {
                assert!(!args.iter().any(|arg| *arg == "-r"));
                let path = Path::new(snapshot);
                assert_eq!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("selected.apk")
                );
                assert!(path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("apppilotkit-android-device-adapter-")));
                if let Some(expected) = self.artifact_bytes.lock().expect("bytes").as_ref() {
                    assert_eq!(&fs::read(path).expect("snapshot bytes"), expected);
                }
                *self.last_install_path.lock().expect("install path") =
                    Some((*snapshot).to_string());
                self.install_calls.fetch_add(1, Ordering::SeqCst);
                self.package_present.store(true, Ordering::SeqCst);
                Ok(stdout("Success\n"))
            }
            ["install", "-r", ..] => panic!("physical install must not replace with -r"),
            ["shell", "am", "force-stop", package] if *package == PACKAGE => {
                self.force_stop_calls.fetch_add(1, Ordering::SeqCst);
                self.pids.lock().expect("pids").clear();
                Ok(stdout(""))
            }
            ["shell", "pidof", package] if *package == PACKAGE => {
                if self.pidof_fails.load(Ordering::SeqCst) {
                    return Err(failure(PlatformFailureKind::Unavailable));
                }
                let pids = self.pids.lock().expect("pids");
                if pids.is_empty() {
                    Ok(ProcessOutput {
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        success: false,
                    })
                } else {
                    Ok(stdout(
                        pids.iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(" ")
                            + "\n",
                    ))
                }
            }
            [
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                component,
                "--es",
                extra,
                descriptor,
            ] if *component == COMPONENT && *extra == DESCRIPTOR_EXTRA => {
                assert!(!descriptor.is_empty());
                assert!(!descriptor.contains('='));
                assert!(
                    descriptor.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
                    })
                );
                assert!(!descriptor.contains(SECRET_CANARY));
                self.start_calls.fetch_add(1, Ordering::SeqCst);
                let pid = self.next_pid.load(Ordering::SeqCst);
                *self.pids.lock().expect("pids") = vec![pid];
                Ok(stdout(start_transcript()))
            }
            ["forward", "--list"] => {
                let remote = format!(
                    "localabstract:{}",
                    // The adapter-owned name is not stored here; tests that
                    // need an exact mapping use ScriptedRunner instead.
                    ""
                );
                let _ = remote;
                Ok(stdout(""))
            }
            ["uninstall", package] if *package == PACKAGE => {
                self.uninstall_calls.fetch_add(1, Ordering::SeqCst);
                self.package_present.store(false, Ordering::SeqCst);
                Ok(stdout("Success\n"))
            }
            other => panic!("unexpected adb command {other:?}"),
        }
    }
}

#[derive(Clone)]
struct Expected {
    args: Vec<String>,
    result: Result<(Vec<u8>, Vec<u8>, bool), PlatformFailureKind>,
}

struct ScriptedRunner {
    expected_serial: String,
    commands: Mutex<VecDeque<Expected>>,
}

impl ScriptedRunner {
    fn new(expected: Vec<Expected>) -> Arc<Self> {
        Arc::new(Self {
            expected_serial: SERIAL.to_owned(),
            commands: Mutex::new(expected.into()),
        })
    }

    fn assert_consumed(&self) {
        assert!(self.commands.lock().expect("commands").is_empty());
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(
        &self,
        _executable: &Path,
        serial: &str,
        arguments: &[OsString],
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure> {
        assert_eq!(serial, self.expected_serial);
        let actual: Vec<String> = arguments
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        let expected = self
            .commands
            .lock()
            .expect("commands")
            .pop_front()
            .expect("unexpected adb command");
        assert_eq!(actual.len(), expected.args.len(), "{actual:?}");
        for (actual, expected_arg) in actual.iter().zip(&expected.args) {
            if expected_arg == "<snapshot>" {
                let path = Path::new(actual);
                assert_eq!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("selected.apk")
                );
            } else if expected_arg == "<descriptor>" {
                assert!(!actual.is_empty());
                assert!(!actual.contains(SECRET_CANARY));
            } else if expected_arg == "<remote>" {
                assert!(actual.starts_with("localabstract:apppilotkit-android-"));
            } else {
                assert_eq!(actual, expected_arg);
            }
        }
        match expected.result {
            Ok((stdout, stderr, success)) => Ok(ProcessOutput {
                stdout,
                stderr,
                success,
            }),
            Err(kind) => Err(PlatformFailure::new(kind)),
        }
    }
}

fn usb_devices_l(serial: &str) -> String {
    format!(
        "List of devices attached\n{serial}         device usb:1-1.2 product:test model:Pixel device:pixel transport_id:1\n"
    )
}

fn wireless_devices_l(serial: &str) -> String {
    format!(
        "List of devices attached\n{serial}         device product:test model:Pixel device:pixel transport_id:1\n"
    )
}

fn ok(args: &[&str], stdout: impl AsRef<[u8]>) -> Expected {
    Expected {
        args: args.iter().map(|value| (*value).to_owned()).collect(),
        result: Ok((stdout.as_ref().to_vec(), Vec::new(), true)),
    }
}

fn fail(args: &[&str], kind: PlatformFailureKind) -> Expected {
    Expected {
        args: args.iter().map(|value| (*value).to_owned()).collect(),
        result: Err(kind),
    }
}

fn empty_pidof() -> Expected {
    Expected {
        args: vec!["shell".to_owned(), "pidof".to_owned(), PACKAGE.to_owned()],
        result: Ok((Vec::new(), Vec::new(), false)),
    }
}

#[test]
fn emulator_serial_and_foreign_selectors_are_rejected_without_adb() {
    let runner = ScriptedRunner::new(Vec::new());
    let adapter = AndroidDeviceAdapter::with_runner("/fake/adb", runner.clone());
    for (platform, serial) in [
        (Platform::AndroidEmulator, SERIAL),
        (Platform::AndroidDevice, "emulator-5554"),
        (Platform::AndroidDevice, "emulator-0"),
        (
            Platform::AndroidDevice,
            "ACEB2C1B-F075-523A-B80B-B2F0D463C313",
        ),
        (Platform::AndroidDevice, "00008150-001E60460130401C"),
        (
            Platform::AndroidDevice,
            "00008030001A258E21E8002E0000000000000000",
        ),
        (Platform::IosDevice, SERIAL),
    ] {
        let error = adapter
            .begin_launch(
                selection(platform, serial, PACKAGE, "/tmp/app.apk"),
                deadline(),
            )
            .launch(descriptor(), Cancellation::new(), deadline())
            .err()
            .expect("rejected selector");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected, "{serial}");
    }
    runner.assert_consumed();
}

#[test]
fn android_usb_serial_rejects_emulator_coredevice_and_ios_udids() {
    for serial in [
        "R58N123456A",
        "ce071717d4a6c80c05",
        "HT6C1234567",
        "1A2B3C4D",
    ] {
        assert!(android_usb_serial(serial), "rejected {serial}");
    }
    for serial in [
        "",
        "emulator-5554",
        "emulator-0",
        "ACEB2C1B-F075-523A-B80B-B2F0D463C313",
        "aceb2c1b-f075-523a-b80b-b2f0d463c313",
        "00008150-001E60460130401C",
        "00008030001A258E21E8002E0000000000000000",
        "serial with space",
    ] {
        assert!(!android_usb_serial(serial), "accepted {serial}");
    }
}

#[test]
fn wireless_or_missing_usb_transport_is_unavailable_without_install() {
    let artifact = TestArtifact::real();
    for listing in [
        wireless_devices_l(SERIAL),
        wireless_devices_l("192.168.1.8:5555"),
        format!("List of devices attached\n"),
        format!(
            "List of devices attached\n{SERIAL}         unauthorized usb:1-1.2 product:test model:Pixel device:pixel transport_id:1\n"
        ),
    ] {
        let runner = ScriptedRunner::new(vec![
            ok(&["help"], "forward tcp:0 localabstract:\n"),
            ok(&["get-state"], "device\n"),
            ok(&["devices", "-l"], listing.clone()),
        ]);
        let adapter = AndroidDeviceAdapter::with_runner("/fake/adb", runner.clone());
        let error = adapter
            .begin_launch(selection_for_artifact(&artifact), deadline())
            .launch(descriptor(), Cancellation::new(), deadline())
            .err()
            .expect("usb required");
        assert_eq!(error.kind(), PlatformFailureKind::Unavailable, "{listing}");
        runner.assert_consumed();
    }
}

#[test]
fn present_package_is_rejected_without_install() {
    let artifact = TestArtifact::real();
    let runner = ScriptedRunner::new(vec![
        ok(&["help"], "forward tcp:0 localabstract:\n"),
        ok(&["get-state"], "device\n"),
        ok(&["devices", "-l"], usb_devices_l(SERIAL)),
        ok(
            &["shell", "pm", "list", "packages", PACKAGE],
            format!("package:{PACKAGE}\n"),
        ),
    ]);
    let adapter = AndroidDeviceAdapter::with_runner("/fake/adb", runner.clone());
    let error = adapter
        .begin_launch(selection_for_artifact(&artifact), deadline())
        .launch(descriptor(), Cancellation::new(), deadline())
        .err()
        .expect("present package");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();
}

#[test]
fn launch_installs_proves_pid_forwards_and_cleans_owned_lease() {
    let artifact = TestArtifact::real();
    let artifact_bytes = fs::read(&artifact.path).expect("APK bytes");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback");
    let port = listener.local_addr().expect("address").port();
    let fake = FakeAdb::new(port);
    *fake.artifact_bytes.lock().expect("bytes") = Some(artifact_bytes);
    let remote_holder = Arc::new(Mutex::new(None::<String>));
    let scripted = ScriptedLifecycle {
        fake: Arc::clone(&fake),
        port,
        localabstract: Arc::clone(&remote_holder),
    };
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("bootstrap connect");
        let mut input = [0_u8; 4];
        stream.read_exact(&mut input).expect("raw request");
        assert_eq!(&input, b"ping");
        stream.write_all(b"pong").expect("reply");
    });

    let adapter = AndroidDeviceAdapter::with_runner("/fake/adb", Arc::new(scripted));
    let pending = adapter.begin_launch(selection_for_artifact(&artifact), deadline());
    *remote_holder.lock().expect("endpoint") = Some(
        pending
            .endpoint()
            .android_name()
            .expect("android endpoint")
            .to_owned(),
    );
    let launched = must(
        pending.launch(descriptor(), Cancellation::new(), deadline()),
        "launch",
    );
    assert_eq!(fake.install_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.start_calls.load(Ordering::SeqCst), 1);
    assert!(
        fake.last_install_path
            .lock()
            .expect("install path")
            .as_ref()
            .is_some_and(|path| Path::new(path) != artifact.path)
    );
    let (raw, _connector, cleanup) = launched.into_parts();
    assert_eq!(must(raw.write(b"ping", deadline()), "write"), 4);
    let mut reply = [0_u8; 4];
    let mut total = 0;
    while total < reply.len() {
        total += must(raw.read(&mut reply[total..], deadline()), "read");
    }
    assert_eq!(&reply, b"pong");
    raw.cancel();
    must(
        cleanup.cleanup(Cancellation::new(), deadline()),
        "owned cleanup",
    );
    peer.join().expect("peer");
    assert_eq!(fake.uninstall_calls.load(Ordering::SeqCst), 1);
    assert!(!fake.package_present.load(Ordering::SeqCst));
    let flattened = fake.seen.lock().expect("seen").concat().join("\n");
    assert!(!flattened.contains(SECRET_CANARY));
    assert!(!flattened.contains("PBS"));
}

struct ScriptedLifecycle {
    fake: Arc<FakeAdb>,
    port: u16,
    localabstract: Arc<Mutex<Option<String>>>,
}

impl CommandRunner for ScriptedLifecycle {
    fn run(
        &self,
        executable: &Path,
        serial: &str,
        arguments: &[OsString],
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure> {
        let args: Vec<String> = arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        let localabstract = self
            .localabstract
            .lock()
            .expect("endpoint")
            .clone()
            .unwrap_or_default();
        let remote = format!("localabstract:{localabstract}");
        let mapping = format!("{SERIAL} tcp:{} {remote}\n", self.port);
        match args_ref.as_slice() {
            ["forward", "--list"] => Ok(stdout(if localabstract.is_empty() {
                String::new()
            } else {
                let created = self.fake.start_calls.load(Ordering::SeqCst) > 0
                    && self
                        .fake
                        .seen
                        .lock()
                        .expect("seen")
                        .iter()
                        .any(|seen| seen.get(1).map(String::as_str) == Some("tcp:0"));
                let removed = self
                    .fake
                    .seen
                    .lock()
                    .expect("seen")
                    .iter()
                    .any(|seen| seen.get(1).map(String::as_str) == Some("--remove"));
                if created && !removed {
                    mapping
                } else {
                    String::new()
                }
            })),
            ["forward", "tcp:0", actual] if *actual == remote => {
                self.fake.seen.lock().expect("seen").push(args.clone());
                Ok(stdout(format!("{}\n", self.port)))
            }
            ["forward", "--remove", local] if *local == format!("tcp:{}", self.port) => {
                self.fake.seen.lock().expect("seen").push(args.clone());
                Ok(stdout(""))
            }
            _ => self
                .fake
                .run(executable, serial, arguments, cancellation, deadline),
        }
    }
}

#[test]
fn occupied_or_unknown_pid_is_not_killed() {
    let runner = ScriptedRunner::new(vec![
        ok(&["forward", "--list"], ""),
        ok(&["shell", "pidof", PACKAGE], format!("{FOREIGN_PID}\n")),
    ]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidDeviceCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
        pid: OWNED_PID,
        installed_by_lease: true,
    });
    let error = cleanup
        .cleanup(Cancellation::new(), deadline())
        .expect_err("unknown pid");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    runner.assert_consumed();
}

#[test]
fn colliding_pids_are_not_killed() {
    let runner = ScriptedRunner::new(vec![
        ok(&["forward", "--list"], ""),
        ok(
            &["shell", "pidof", PACKAGE],
            format!("{OWNED_PID} {FOREIGN_PID}\n"),
        ),
    ]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidDeviceCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
        pid: OWNED_PID,
        installed_by_lease: true,
    });
    let error = cleanup
        .cleanup(Cancellation::new(), deadline())
        .expect_err("collision");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    runner.assert_consumed();
}

#[test]
fn pid_query_failure_is_cleanup_failed_without_kill_or_uninstall() {
    let runner = ScriptedRunner::new(vec![
        ok(&["forward", "--list"], ""),
        fail(
            &["shell", "pidof", PACKAGE],
            PlatformFailureKind::Unavailable,
        ),
    ]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidDeviceCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
        pid: OWNED_PID,
        installed_by_lease: true,
    });
    let error = cleanup
        .cleanup(Cancellation::new(), deadline())
        .expect_err("query failure");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    runner.assert_consumed();
}

#[test]
fn package_query_failure_during_uninstall_is_cleanup_failed() {
    let runner = ScriptedRunner::new(vec![
        ok(&["forward", "--list"], ""),
        empty_pidof(),
        empty_pidof(),
        fail(
            &["shell", "pm", "list", "packages", PACKAGE],
            PlatformFailureKind::Unavailable,
        ),
    ]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidDeviceCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
        pid: OWNED_PID,
        installed_by_lease: true,
    });
    let error = cleanup
        .cleanup(Cancellation::new(), deadline())
        .expect_err("package query failure");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    runner.assert_consumed();
}

#[test]
fn exited_owned_pid_is_ok_and_still_uninstalls_lease() {
    let runner = ScriptedRunner::new(vec![
        ok(&["forward", "--list"], ""),
        empty_pidof(),
        empty_pidof(),
        ok(
            &["shell", "pm", "list", "packages", PACKAGE],
            format!("package:{PACKAGE}\n"),
        ),
        ok(&["uninstall", PACKAGE], "Success\n"),
    ]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidDeviceCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
        pid: OWNED_PID,
        installed_by_lease: true,
    });
    must(
        cleanup.cleanup(Cancellation::new(), deadline()),
        "exited pid",
    );
    runner.assert_consumed();
}

#[test]
fn parse_devices_l_requires_usb_field_for_the_selected_serial() {
    let usb = usb_devices_l(SERIAL);
    must(parse_devices_l_usb(&usb, SERIAL), "usb");
    assert_eq!(
        parse_devices_l_usb(&wireless_devices_l(SERIAL), SERIAL)
            .expect_err("no usb")
            .kind(),
        PlatformFailureKind::Unavailable
    );
    assert_eq!(
        parse_devices_l_usb(&wireless_devices_l("192.168.1.8:5555"), SERIAL)
            .expect_err("other serial")
            .kind(),
        PlatformFailureKind::Unavailable
    );
    assert_eq!(
        parse_devices_l_usb("List of devices attached\n", SERIAL)
            .expect_err("empty")
            .kind(),
        PlatformFailureKind::Unavailable
    );
}

#[test]
fn parse_pidof_and_package_presence() {
    assert!(must(parse_pidof(""), "empty").is_empty());
    assert!(must(parse_pidof("\n"), "newline").is_empty());
    assert_eq!(must(parse_pidof("42424\n"), "one"), vec![42_424]);
    assert_eq!(must(parse_pidof("1 2\n"), "two"), vec![1, 2]);
    assert_eq!(
        parse_pidof("0\n").expect_err("zero").kind(),
        PlatformFailureKind::Rejected
    );
    assert!(!must(parse_package_present("", PACKAGE), "absent"));
    assert!(must(
        parse_package_present(&format!("package:{PACKAGE}\n"), PACKAGE),
        "present"
    ));
    assert_eq!(
        parse_package_present("package:other\n", PACKAGE)
            .expect_err("other")
            .kind(),
        PlatformFailureKind::Rejected
    );
}

#[test]
fn abort_is_a_no_io_terminal() {
    let runner = ScriptedRunner::new(Vec::new());
    let adapter = AndroidDeviceAdapter::with_runner("/fake/adb", runner.clone());
    must(
        adapter
            .begin_launch(
                selection(Platform::AndroidDevice, SERIAL, PACKAGE, "/tmp/app.apk"),
                deadline(),
            )
            .abort(Cancellation::new(), deadline()),
        "abort",
    );
    runner.assert_consumed();
}
