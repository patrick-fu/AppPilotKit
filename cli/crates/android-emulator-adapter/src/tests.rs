use std::{
    collections::VecDeque,
    ffi::CString,
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use apppilotkit_host_runtime::adapter::{
    AbsoluteDeadline, Cancellation, CleanupReceipt, LaunchEndpoint, PlatformFailure,
    PlatformFailureKind, PlatformTargetAdapter, PublicLaunchDescriptor, TargetSelection,
};

use super::*;

const SERIAL: &str = "emulator-5554";
const PACKAGE: &str = "dev.apppilotkit.smokehost";
const COMPONENT: &str = "dev.apppilotkit.smokehost/.AppPilotKitBootstrapActivity";
const SECRET_CANARY: &str = "APPPILOTKIT_PRIVATE_SECRET_CANARY_7d14";
const SNAPSHOT_ARG: &str = "<verified-artifact-snapshot>";
const DESCRIPTOR_ARG: &str = "<public-descriptor>";

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

fn deadline_after(milliseconds: u64) -> AbsoluteDeadline {
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("timestamp");
    must(AbsoluteDeadline::new(now + milliseconds), "deadline")
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
            Platform::AndroidEmulator,
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
        Self::build("dev.apppilotkit.smokehost", true)
    }

    fn build(package: &str, include_bootstrap: bool) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "apppilotkit-android-artifact-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("artifact directory");
        let manifest = directory.join("AndroidManifest.xml");
        fs::write(
            &manifest,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="{package}">
  <uses-sdk android:minSdkVersion="26" android:targetSdkVersion="36" />
  <application android:debuggable="true" android:hasCode="false">
    {}
  </application>
</manifest>
"#,
                if include_bootstrap {
                    r#"<activity android:name=".AppPilotKitBootstrapActivity" android:exported="true" />"#
                } else {
                    ""
                }
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

#[derive(Clone)]
struct Expected {
    args: Vec<String>,
    result: Result<(Vec<u8>, Vec<u8>), PlatformFailureKind>,
}

struct MockRunner {
    expected_serial: String,
    commands: Mutex<VecDeque<Expected>>,
    seen: Mutex<Vec<Vec<String>>>,
    deadlines: Mutex<Vec<u64>>,
}

impl MockRunner {
    fn new(expected: Vec<Expected>) -> Arc<Self> {
        Arc::new(Self {
            expected_serial: SERIAL.to_owned(),
            commands: Mutex::new(expected.into()),
            seen: Mutex::new(Vec::new()),
            deadlines: Mutex::new(Vec::new()),
        })
    }

    fn assert_consumed(&self) {
        assert!(self.commands.lock().expect("commands").is_empty());
    }
}

impl CommandRunner for MockRunner {
    fn run(
        &self,
        _executable: &Path,
        serial: &str,
        arguments: &[OsString],
        _cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
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
        assert_eq!(actual.len(), expected.args.len());
        for (actual, expected) in actual.iter().zip(&expected.args) {
            if expected == SNAPSHOT_ARG {
                let path = Path::new(actual);
                assert!(path.is_absolute());
                assert_eq!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("selected.apk")
                );
                assert!(
                    path.parent()
                        .and_then(Path::file_name)
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("apppilotkit-android-adapter-"))
                );
            } else if expected == DESCRIPTOR_ARG {
                assert!(!actual.is_empty());
                assert!(!actual.contains('='));
                assert!(
                    actual.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
                    })
                );
                assert!(!actual.contains(SECRET_CANARY));
            } else {
                assert_eq!(actual, expected);
            }
        }
        self.seen.lock().expect("seen").push(actual);
        self.deadlines
            .lock()
            .expect("deadlines")
            .push(deadline.value());
        match expected.result {
            Ok((stdout, stderr)) => Ok(ProcessOutput { stdout, stderr }),
            Err(kind) => Err(PlatformFailure::new(kind)),
        }
    }
}

fn ok(args: &[&str], stdout: impl AsRef<[u8]>) -> Expected {
    Expected {
        args: args.iter().map(|value| (*value).to_owned()).collect(),
        result: Ok((stdout.as_ref().to_vec(), Vec::new())),
    }
}

fn fail(args: &[&str], kind: PlatformFailureKind) -> Expected {
    Expected {
        args: args.iter().map(|value| (*value).to_owned()).collect(),
        result: Err(kind),
    }
}

fn preflight_until_get_state(state: Expected) -> Vec<Expected> {
    vec![ok(&["help"], "forward tcp:0 localabstract:\n"), state]
}

#[test]
fn begin_launch_rejects_non_android_without_platform_io() {
    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let pending = adapter.begin_launch(
        selection(Platform::IosSimulator, SERIAL, PACKAGE, "/tmp/app.apk"),
        deadline(),
    );
    assert!(pending.endpoint().android_name().is_some());
    let error = pending
        .launch(descriptor(), Cancellation::new(), deadline())
        .err()
        .expect("wrong platform");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();
}

#[test]
fn non_emulator_serials_and_invalid_android_packages_fail_before_adb() {
    for (serial, package) in [
        (" ", PACKAGE),
        ("emulator 5554", PACKAGE),
        ("emulator-", PACKAGE),
        ("emulator-55x4", PACKAGE),
        ("R58N123456A", PACKAGE),
        (SERIAL, "bad-package"),
        (SERIAL, ".bad.package"),
        (SERIAL, "bad..package"),
    ] {
        let runner = MockRunner::new(Vec::new());
        let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
        let pending = adapter.begin_launch(
            selection(Platform::AndroidEmulator, serial, package, "/tmp/app.apk"),
            deadline(),
        );
        let error = pending
            .launch(descriptor(), Cancellation::new(), deadline())
            .err()
            .expect("invalid selection");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected);
        runner.assert_consumed();
    }

    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let error = adapter
        .begin_launch(
            selection(Platform::AndroidEmulator, SERIAL, PACKAGE, "relative.apk"),
            deadline(),
        )
        .launch(descriptor(), Cancellation::new(), deadline())
        .err()
        .expect("relative artifact");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();
}

#[test]
fn android_emulator_serial_requires_exact_prefix_and_decimal_suffix() {
    for serial in ["emulator-0", "emulator-5554", "emulator-1234567890"] {
        assert!(android_emulator_serial(serial), "rejected {serial}");
    }
    for serial in [
        "",
        "emulator-",
        "emulator--5554",
        "emulator-5554 ",
        "emulator-55x4",
        "device-5554",
        "R58N123456A",
        &format!("emulator-{}", "1".repeat(247)),
    ] {
        assert!(!android_emulator_serial(serial), "accepted {serial}");
    }
}

#[test]
fn endpoint_is_random_safe_and_not_derived_from_selection() {
    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner);
    let first = adapter.begin_launch(
        selection(
            Platform::AndroidEmulator,
            "serial;injection",
            PACKAGE,
            "/tmp/app.apk",
        ),
        deadline(),
    );
    let second = adapter.begin_launch(
        selection(
            Platform::AndroidEmulator,
            "serial;injection",
            PACKAGE,
            "/tmp/app.apk",
        ),
        deadline(),
    );
    let first = first.endpoint().android_name().expect("android endpoint");
    let second = second.endpoint().android_name().expect("android endpoint");
    assert_ne!(first, second);
    for name in [first, second] {
        assert!((32..=96).contains(&name.len()));
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert!(!name.contains("serial"));
    }
}

#[test]
fn wrong_offline_and_duplicate_serial_fail_closed() {
    let artifact = TestArtifact::real();
    for state in [
        fail(&["get-state"], PlatformFailureKind::Unavailable),
        ok(&["get-state"], "offline\n"),
        fail(&["get-state"], PlatformFailureKind::Unavailable),
    ] {
        let runner = MockRunner::new(preflight_until_get_state(state));
        let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
        let error = adapter
            .begin_launch(selection_for_artifact(&artifact), deadline())
            .launch(descriptor(), Cancellation::new(), deadline())
            .err()
            .expect("device selection fails");
        assert!(matches!(
            error.kind(),
            PlatformFailureKind::Unavailable | PlatformFailureKind::Rejected
        ));
        runner.assert_consumed();
    }
}

#[test]
fn unsupported_forward_feature_does_not_downgrade() {
    let artifact = TestArtifact::real();
    let runner = MockRunner::new(vec![ok(&["help"], "forward tcp:PORT\n")]);
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let error = adapter
        .begin_launch(selection_for_artifact(&artifact), deadline())
        .launch(descriptor(), Cancellation::new(), deadline())
        .err()
        .expect("unsupported platform tools");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();
}

#[test]
fn package_install_and_activity_start_fail_closed() {
    let artifact = TestArtifact::real();
    let cases = [
        vec![
            ok(&["help"], "forward tcp:0 localabstract:\n"),
            ok(&["get-state"], "device\n"),
            fail(
                &["install", "-r", "-t", SNAPSHOT_ARG],
                PlatformFailureKind::Unavailable,
            ),
        ],
        vec![
            ok(&["help"], "forward tcp:0 localabstract:\n"),
            ok(&["get-state"], "device\n"),
            ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
            ok(
                &[
                    "shell",
                    "am",
                    "start",
                    "-W",
                    "-S",
                    "-n",
                    COMPONENT,
                    "--es",
                    DESCRIPTOR_EXTRA,
                    DESCRIPTOR_ARG,
                ],
                "Error: Activity class does not exist.\n",
            ),
        ],
    ];
    for commands in cases {
        let runner = MockRunner::new(commands);
        let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
        assert!(
            adapter
                .begin_launch(selection_for_artifact(&artifact), deadline(),)
                .launch(descriptor(), Cancellation::new(), deadline())
                .is_err()
        );
        runner.assert_consumed();
    }
}

#[test]
fn start_side_effect_list_timeout_force_stops_only_the_expected_package_without_creating_a_forward()
{
    let artifact = TestArtifact::real();
    let start = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    let runner = MockRunner::new(vec![
        ok(&["help"], "forward tcp:0 localabstract:\n"),
        ok(&["get-state"], "device\n"),
        ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
        ok(
            &[
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                COMPONENT,
                "--es",
                DESCRIPTOR_EXTRA,
                DESCRIPTOR_ARG,
            ],
            start,
        ),
        fail(&["forward", "--list"], PlatformFailureKind::TimedOut),
        ok(&["shell", "am", "force-stop", PACKAGE], ""),
    ]);
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let launch_deadline = deadline_after(100);
    let error = adapter
        .begin_launch(selection_for_artifact(&artifact), launch_deadline)
        .launch(descriptor(), Cancellation::new(), launch_deadline)
        .err()
        .expect("post-start forward list deadline");
    assert_eq!(error.kind(), PlatformFailureKind::TimedOut);
    assert_eq!(error.primary_kind(), PlatformFailureKind::TimedOut);
    assert!(!error.cleanup_failed());
    runner.assert_consumed();
    let deadlines = runner.deadlines.lock().expect("deadlines");
    assert!(
        deadlines[..5]
            .iter()
            .all(|value| *value == launch_deadline.value())
    );
    assert_ne!(deadlines[5], launch_deadline.value());
}

#[test]
fn start_side_effect_cleanup_failure_is_fail_closed_and_retains_the_timeout_primary() {
    let artifact = TestArtifact::real();
    let start = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    let runner = MockRunner::new(vec![
        ok(&["help"], "forward tcp:0 localabstract:\n"),
        ok(&["get-state"], "device\n"),
        ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
        ok(
            &[
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                COMPONENT,
                "--es",
                DESCRIPTOR_EXTRA,
                DESCRIPTOR_ARG,
            ],
            start,
        ),
        fail(&["forward", "--list"], PlatformFailureKind::TimedOut),
        fail(
            &["shell", "am", "force-stop", PACKAGE],
            PlatformFailureKind::Unavailable,
        ),
    ]);
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let error = adapter
        .begin_launch(selection_for_artifact(&artifact), deadline_after(100))
        .launch(descriptor(), Cancellation::new(), deadline_after(100))
        .err()
        .expect("post-start cleanup failure");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(error.primary_kind(), PlatformFailureKind::TimedOut);
    assert!(error.cleanup_failed());
    runner.assert_consumed();
}

#[test]
fn replaced_or_missing_artifact_fails_before_install_side_effect() {
    let artifact = TestArtifact::real();
    let selected = selection_for_artifact(&artifact);
    fs::write(&artifact.path, b"replaced apk bytes").expect("replace artifact");
    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let error = adapter
        .begin_launch(selected, deadline())
        .launch(descriptor(), Cancellation::new(), deadline())
        .err()
        .expect("replaced artifact");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();

    let missing = selection_for_artifact(&artifact);
    fs::remove_file(&artifact.path).expect("remove artifact");
    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let error = adapter
        .begin_launch(missing, deadline())
        .launch(descriptor(), Cancellation::new(), deadline())
        .err()
        .expect("missing artifact");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();
}

#[test]
fn wrong_application_id_or_missing_bootstrap_activity_fails_before_adb() {
    for artifact in [
        TestArtifact::build("dev.apppilotkit.other", true),
        TestArtifact::build(PACKAGE, false),
    ] {
        let runner = MockRunner::new(Vec::new());
        let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
        let selected = must(
            TargetSelection::new(
                Platform::AndroidEmulator,
                SERIAL.to_owned(),
                PACKAGE.to_owned(),
                artifact.path.to_string_lossy().into_owned(),
                artifact.digest,
            ),
            "APK binding selection",
        );
        let error = adapter
            .begin_launch(selected, deadline())
            .launch(descriptor(), Cancellation::new(), deadline())
            .err()
            .expect("APK binding rejected");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected);
        runner.assert_consumed();
    }
}

#[test]
fn fifo_directory_and_symlink_artifacts_fail_before_adb() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "apppilotkit-special-artifact-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir(&directory).expect("special artifact directory");
    let fifo = directory.join("selected.apk");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
    // SAFETY: the path is a NUL-free owned CString in an exact test directory.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let symlink = directory.join("linked.apk");
    std::os::unix::fs::symlink(&fifo, &symlink).expect("artifact symlink");

    for path in [&fifo, &directory, &symlink] {
        let runner = MockRunner::new(Vec::new());
        let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
        let selected = must(
            TargetSelection::new(
                Platform::AndroidEmulator,
                SERIAL.to_owned(),
                PACKAGE.to_owned(),
                path.to_string_lossy().into_owned(),
                [0; 32],
            ),
            "special selection",
        );
        let error = adapter
            .begin_launch(selected, deadline())
            .launch(descriptor(), Cancellation::new(), deadline())
            .err()
            .expect("special artifact rejected");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected);
        runner.assert_consumed();
    }
    fs::remove_file(symlink).expect("remove symlink");
    fs::remove_file(fifo).expect("remove FIFO");
    fs::remove_dir(directory).expect("remove special directory");
}

#[test]
fn parsers_reject_malformed_ambiguous_and_partial_output() {
    for value in ["", "0\n", "65536\n", "1234\n5678\n", "port:1234\n", "1\n\n"] {
        assert!(parse_port(value).is_err(), "accepted {value:?}");
    }
    assert_eq!(must(parse_port("49152\n"), "port"), 49_152);
    assert!(must(parse_forward_list("\n\n"), "empty forward list").is_empty());
    assert!(parse_forward_list("serial tcp:1\n").is_err());
    assert!(parse_forward_list("serial  tcp:1 localabstract:name\n").is_err());
    assert!(parse_forward_list("serial tcp:1 localabstract:name\ntruncated").is_err());
    assert!(parse_forward_list("\n\n\n").is_err());
    assert!(parse_forward_list("\n\nserial tcp:1 localabstract:name\n").is_err());
    assert!(parse_install("Success\npartial").is_err());
    assert!(parse_start("Status: ok\nComplete\n", COMPONENT).is_err());
    assert!(
        parse_start(
            "Status: ok\nActivity: other/.Activity\nComplete\n",
            COMPONENT
        )
        .is_err()
    );
}

#[test]
fn start_transcript_requires_one_exact_supported_launch_state() {
    let cold = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    assert!(parse_start(&cold, COMPONENT).is_ok());
    let real_unknown = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: UNKNOWN (0)\nActivity: {COMPONENT}\nWaitTime: 2432\nComplete\n"
    );
    assert!(parse_start(&real_unknown, COMPONENT).is_ok());
    for invalid in [
        cold.replace("LaunchState: COLD\n", ""),
        cold.replace("LaunchState: COLD", "LaunchState: HOT"),
        cold.replace("LaunchState: COLD", "LaunchState: WARM"),
        cold.replace(
            "LaunchState: COLD\n",
            "LaunchState: COLD\nLaunchState: COLD\n",
        ),
        real_unknown.replace("UNKNOWN (0)", "UNKNOWN ()"),
        real_unknown.replace("UNKNOWN (0)", "UNKNOWN (-1)"),
        real_unknown.replace("UNKNOWN (0)", "UNKNOWN (0x0)"),
        real_unknown.replace("UNKNOWN (0)", "UNKNOWN (0"),
        real_unknown.replace(
            "LaunchState: UNKNOWN (0)\n",
            "LaunchState: UNKNOWN (0)\nLaunchState: COLD\n",
        ),
    ] {
        assert!(parse_start(&invalid, COMPONENT).is_err(), "{invalid}");
    }
}

#[test]
fn malformed_forward_port_removes_only_the_mapping_created_by_this_launch() {
    let artifact = TestArtifact::real();
    let localabstract = "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned();
    let remote = format!("localabstract:{localabstract}");
    let start = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    let runner = MockRunner::new(vec![
        ok(&["help"], "forward tcp:0 localabstract:\n"),
        ok(&["get-state"], "device\n"),
        ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
        ok(
            &[
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                COMPONENT,
                "--es",
                DESCRIPTOR_EXTRA,
                DESCRIPTOR_ARG,
            ],
            start,
        ),
        ok(&["forward", "--list"], ""),
        ok(&["forward", "tcp:0", &remote], "49152\n49153\n"),
        ok(
            &["forward", "--list"],
            format!("{SERIAL} tcp:49152 {remote}\n"),
        ),
        ok(
            &["forward", "--list"],
            format!("{SERIAL} tcp:49152 {remote}\n"),
        ),
        ok(&["forward", "--remove", "tcp:49152"], ""),
        ok(&["forward", "--list"], ""),
        ok(&["shell", "am", "force-stop", PACKAGE], ""),
    ]);
    let pending = AndroidPendingLaunch {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        artifact: artifact.path.clone(),
        artifact_digest: artifact.digest,
        localabstract: localabstract.clone(),
        endpoint: must(
            LaunchEndpoint::android_local_abstract(localabstract),
            "endpoint",
        ),
        validation: None,
    };
    let launch_deadline = deadline();
    let error = Box::new(pending)
        .launch(descriptor(), Cancellation::new(), launch_deadline)
        .err()
        .expect("ambiguous allocated port");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    runner.assert_consumed();
    let deadlines = runner.deadlines.lock().expect("deadlines");
    assert!(
        deadlines[..6]
            .iter()
            .all(|value| *value == launch_deadline.value())
    );
    assert!(deadlines[6..].iter().all(|value| *value == deadlines[6]));
    assert_ne!(deadlines[6], launch_deadline.value());
}

#[test]
fn timed_out_or_cancelled_forward_preserves_kind_after_independent_rollback() {
    let artifact = TestArtifact::real();
    let localabstract = "apppilotkit-android-fedcba9876543210fedcba9876543210".to_owned();
    let remote = format!("localabstract:{localabstract}");
    let start = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    for kind in [
        PlatformFailureKind::TimedOut,
        PlatformFailureKind::Cancelled,
    ] {
        let runner = MockRunner::new(vec![
            ok(&["help"], "forward tcp:0 localabstract:\n"),
            ok(&["get-state"], "device\n"),
            ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
            ok(
                &[
                    "shell",
                    "am",
                    "start",
                    "-W",
                    "-S",
                    "-n",
                    COMPONENT,
                    "--es",
                    DESCRIPTOR_EXTRA,
                    DESCRIPTOR_ARG,
                ],
                &start,
            ),
            ok(&["forward", "--list"], ""),
            fail(&["forward", "tcp:0", &remote], kind),
            ok(
                &["forward", "--list"],
                format!("{SERIAL} tcp:49152 {remote}\n"),
            ),
            ok(
                &["forward", "--list"],
                format!("{SERIAL} tcp:49152 {remote}\n"),
            ),
            ok(&["forward", "--remove", "tcp:49152"], ""),
            ok(&["forward", "--list"], ""),
            ok(&["shell", "am", "force-stop", PACKAGE], ""),
        ]);
        let pending = AndroidPendingLaunch {
            adb_path: PathBuf::from("/fake/adb"),
            runner: runner.clone(),
            serial: SERIAL.to_owned(),
            package: PACKAGE.to_owned(),
            artifact: artifact.path.clone(),
            artifact_digest: artifact.digest,
            localabstract: localabstract.clone(),
            endpoint: must(
                LaunchEndpoint::android_local_abstract(localabstract.clone()),
                "endpoint",
            ),
            validation: None,
        };
        let launch_deadline = deadline();
        let error = Box::new(pending)
            .launch(descriptor(), Cancellation::new(), launch_deadline)
            .err()
            .expect("forward failure");
        assert_eq!(error.kind(), kind);
        runner.assert_consumed();
        let deadlines = runner.deadlines.lock().expect("deadlines");
        assert!(
            deadlines[..6]
                .iter()
                .all(|value| *value == launch_deadline.value())
        );
        assert!(deadlines[6..].iter().all(|value| *value == deadlines[6]));
        assert_ne!(deadlines[6], launch_deadline.value());
    }
}

#[test]
fn verified_forward_connect_timeout_removes_only_the_exact_mapping_and_keeps_primary_failure() {
    let artifact = TestArtifact::real();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserved loopback port");
    let port = listener.local_addr().expect("loopback address").port();
    drop(listener);
    let localabstract = "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned();
    let remote = format!("localabstract:{localabstract}");
    let mapping_entry = format!("{SERIAL} tcp:{port} {remote}\n");
    let mapping = format!("{mapping_entry}\n");
    let foreign = format!("{SERIAL} tcp:49999 localabstract:foreign\n");
    let start = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    let runner = MockRunner::new(vec![
        ok(&["help"], "forward tcp:0 localabstract:\n"),
        ok(&["get-state"], "device\n"),
        ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
        ok(
            &[
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                COMPONENT,
                "--es",
                DESCRIPTOR_EXTRA,
                DESCRIPTOR_ARG,
            ],
            start,
        ),
        ok(&["forward", "--list"], ""),
        ok(&["forward", "tcp:0", &remote], format!("{port}\n")),
        ok(&["forward", "--list"], &mapping),
        ok(
            &["forward", "--list"],
            format!("{mapping_entry}{foreign}\n"),
        ),
        ok(&["forward", "--remove", &format!("tcp:{port}")], ""),
        ok(&["forward", "--list"], format!("{foreign}\n")),
        ok(&["shell", "am", "force-stop", PACKAGE], ""),
    ]);
    let pending = AndroidPendingLaunch {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        artifact: artifact.path.clone(),
        artifact_digest: artifact.digest,
        localabstract: localabstract.clone(),
        endpoint: must(
            LaunchEndpoint::android_local_abstract(localabstract),
            "endpoint",
        ),
        validation: None,
    };

    let launch_deadline = deadline_after(100);
    let error = Box::new(pending)
        .launch(descriptor(), Cancellation::new(), launch_deadline)
        .err()
        .expect("unserved verified forward times out");
    assert_eq!(error.kind(), PlatformFailureKind::TimedOut);
    assert_eq!(error.primary_kind(), PlatformFailureKind::TimedOut);
    runner.assert_consumed();
    let deadlines = runner.deadlines.lock().expect("deadlines");
    assert!(
        deadlines[..7]
            .iter()
            .all(|value| *value == launch_deadline.value())
    );
    assert!(deadlines[7..].iter().all(|value| *value == deadlines[7]));
    assert_ne!(deadlines[7], launch_deadline.value());
}

#[test]
fn failed_connect_rollback_keeps_the_timeout_primary_while_marking_cleanup_failed() {
    let artifact = TestArtifact::real();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserved loopback port");
    let port = listener.local_addr().expect("loopback address").port();
    drop(listener);
    let localabstract = "apppilotkit-android-fedcba9876543210fedcba9876543210".to_owned();
    let remote = format!("localabstract:{localabstract}");
    let mapping = format!("{SERIAL} tcp:{port} {remote}\n");
    let start = format!(
        "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
    );
    let runner = MockRunner::new(vec![
        ok(&["help"], "forward tcp:0 localabstract:\n"),
        ok(&["get-state"], "device\n"),
        ok(&["install", "-r", "-t", SNAPSHOT_ARG], "Success\n"),
        ok(
            &[
                "shell",
                "am",
                "start",
                "-W",
                "-S",
                "-n",
                COMPONENT,
                "--es",
                DESCRIPTOR_EXTRA,
                DESCRIPTOR_ARG,
            ],
            start,
        ),
        ok(&["forward", "--list"], ""),
        ok(&["forward", "tcp:0", &remote], format!("{port}\n")),
        ok(&["forward", "--list"], &mapping),
        ok(&["forward", "--list"], mapping),
        fail(
            &["forward", "--remove", &format!("tcp:{port}")],
            PlatformFailureKind::TimedOut,
        ),
        ok(&["shell", "am", "force-stop", PACKAGE], ""),
    ]);
    let pending = AndroidPendingLaunch {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        package: PACKAGE.to_owned(),
        artifact: artifact.path.clone(),
        artifact_digest: artifact.digest,
        localabstract: localabstract.clone(),
        endpoint: must(
            LaunchEndpoint::android_local_abstract(localabstract),
            "endpoint",
        ),
        validation: None,
    };

    let error = Box::new(pending)
        .launch(descriptor(), Cancellation::new(), deadline_after(100))
        .err()
        .expect("rollback failure remains fail closed");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(error.primary_kind(), PlatformFailureKind::TimedOut);
    assert!(error.cleanup_failed());
    runner.assert_consumed();
}

#[test]
fn non_utf8_and_success_stderr_are_rejected() {
    let runner = MockRunner::new(vec![Expected {
        args: vec!["help".to_owned()],
        result: Ok((vec![0xff], Vec::new())),
    }]);
    let client = AdbClient {
        executable: Path::new("/fake/adb"),
        serial: SERIAL,
        runner: runner.as_ref(),
    };
    assert_eq!(
        client
            .probe(&Cancellation::new(), deadline())
            .unwrap_err()
            .kind(),
        PlatformFailureKind::Rejected
    );
    runner.assert_consumed();

    let runner = MockRunner::new(vec![Expected {
        args: vec!["help".to_owned()],
        result: Ok((
            b"forward tcp:0 localabstract:\n".to_vec(),
            b"ambiguous warning".to_vec(),
        )),
    }]);
    let client = AdbClient {
        executable: Path::new("/fake/adb"),
        serial: SERIAL,
        runner: runner.as_ref(),
    };
    assert_eq!(
        client
            .probe(&Cancellation::new(), deadline())
            .unwrap_err()
            .kind(),
        PlatformFailureKind::Rejected
    );
    runner.assert_consumed();
}

#[test]
fn full_contract_uses_one_descriptor_extra_raw_bytes_and_owned_cleanup() {
    let artifact = TestArtifact::real();
    let artifact_path = artifact.path.to_string_lossy().into_owned();
    let artifact_bytes = fs::read(&artifact.path).expect("APK bytes");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback");
    let port = listener.local_addr().expect("address").port();
    let endpoint_marker = Arc::new(Mutex::new(None::<String>));
    let endpoint_for_runner = Arc::clone(&endpoint_marker);
    let runner = Arc::new(DynamicLifecycleRunner {
        port,
        step: Mutex::new(0),
        localabstract: endpoint_for_runner,
        artifact_path,
        artifact_bytes,
        seen: Mutex::new(Vec::new()),
    });
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("bootstrap connect");
        let mut input = [0_u8; 4];
        stream.read_exact(&mut input).expect("raw request");
        assert_eq!(&input, b"ping");
        stream.write_all(b"po").expect("partial reply 1");
        thread::sleep(Duration::from_millis(5));
        stream.write_all(b"ng").expect("partial reply 2");
    });

    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let pending = adapter.begin_launch(selection_for_artifact(&artifact), deadline());
    *endpoint_marker.lock().expect("endpoint") = Some(
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
    let (raw, _connector, cleanup) = launched.into_parts();
    assert_eq!(must(raw.write(b"ping", deadline()), "write"), 4);
    let mut reply = [0_u8; 4];
    let first = must(raw.read(&mut reply, deadline()), "partial read");
    assert!((1..=4).contains(&first));
    let mut total = first;
    while total < reply.len() {
        total += must(raw.read(&mut reply[total..], deadline()), "read");
    }
    assert_eq!(&reply, b"pong");
    raw.cancel();
    raw.cancel();
    must(
        cleanup.cleanup(Cancellation::new(), deadline()),
        "owned cleanup",
    );
    peer.join().expect("peer");
    runner.assert_complete_and_secret_free();
}

struct DynamicLifecycleRunner {
    port: u16,
    step: Mutex<usize>,
    localabstract: Arc<Mutex<Option<String>>>,
    artifact_path: String,
    artifact_bytes: Vec<u8>,
    seen: Mutex<Vec<Vec<String>>>,
}

impl DynamicLifecycleRunner {
    fn assert_complete_and_secret_free(&self) {
        assert_eq!(*self.step.lock().expect("step"), 10);
        let seen = self.seen.lock().expect("seen");
        let flattened = seen.concat().join("\n");
        assert!(!flattened.contains(SECRET_CANARY));
        assert!(!flattened.contains("PBS"));
        assert!(!flattened.contains("NNpsk0"));
        assert!(!flattened.contains("session"));
        assert!(!flattened.contains("reverse"));
        assert!(!flattened.contains("--remove-all"));
        let start = &seen[3];
        assert_eq!(start.iter().filter(|arg| *arg == "--es").count(), 1);
        assert!(!start.iter().any(|arg| arg.contains("localabstract")));
        let snapshot = seen[2].last().expect("artifact snapshot");
        assert_ne!(snapshot, &self.artifact_path);
        assert!(!Path::new(snapshot).exists());
    }
}

impl CommandRunner for DynamicLifecycleRunner {
    fn run(
        &self,
        _executable: &Path,
        serial: &str,
        arguments: &[OsString],
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<ProcessOutput, PlatformFailure> {
        assert_eq!(serial, SERIAL);
        let args: Vec<String> = arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        self.seen.lock().expect("seen").push(args.clone());
        let mut step = self.step.lock().expect("step");
        let localabstract = self
            .localabstract
            .lock()
            .expect("endpoint")
            .clone()
            .expect("endpoint set");
        let remote = format!("localabstract:{localabstract}");
        let mapping = format!("{SERIAL} tcp:{} {remote}\n", self.port);
        let output = match *step {
            0 => {
                assert_eq!(args, vec!["help"]);
                b"forward tcp:0 localabstract:\n".to_vec()
            }
            1 => {
                assert_eq!(args, vec!["get-state"]);
                b"device\n".to_vec()
            }
            2 => {
                assert_eq!(&args[..3], &["install", "-r", "-t"]);
                assert_ne!(args[3], self.artifact_path);
                assert_eq!(
                    fs::read(&args[3]).expect("snapshot bytes"),
                    self.artifact_bytes.as_slice()
                );
                fs::write(&self.artifact_path, b"replaced during install")
                    .expect("replace source during install");
                b"Performing Streamed Install\nSuccess\n".to_vec()
            }
            3 => {
                assert_eq!(
                    &args[..8],
                    &["shell", "am", "start", "-W", "-S", "-n", COMPONENT, "--es"]
                );
                assert_eq!(args[8], DESCRIPTOR_EXTRA);
                assert_eq!(args.len(), 10);
                format!(
                    "Stopping: {PACKAGE}\nStarting: Intent {{ cmp={COMPONENT} (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: {COMPONENT}\nTotalTime: 1\nWaitTime: 2\nComplete\n"
                )
                .into_bytes()
            }
            4 => {
                assert_eq!(args, vec!["forward", "--list"]);
                Vec::new()
            }
            5 => {
                assert_eq!(args, vec!["forward", "tcp:0", &remote]);
                format!("{}\n", self.port).into_bytes()
            }
            6 | 7 => {
                assert_eq!(args, vec!["forward", "--list"]);
                mapping.into_bytes()
            }
            8 => {
                assert_eq!(
                    args,
                    vec!["forward", "--remove", &format!("tcp:{}", self.port)]
                );
                Vec::new()
            }
            9 => {
                assert_eq!(args, vec!["forward", "--list"]);
                Vec::new()
            }
            _ => panic!("unexpected lifecycle step {step}"),
        };
        *step += 1;
        Ok(ProcessOutput {
            stdout: output,
            stderr: Vec::new(),
        })
    }
}

#[test]
fn cleanup_is_idempotent_when_mapping_is_already_absent() {
    let runner = MockRunner::new(vec![ok(&["forward", "--list"], "")]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
    });
    must(
        cleanup.cleanup(Cancellation::new(), deadline()),
        "missing exact mapping is already clean",
    );
    runner.assert_consumed();
}

#[test]
fn cleanup_never_removes_foreign_mapping() {
    let runner = MockRunner::new(vec![ok(
        &["forward", "--list"],
        format!("{SERIAL} tcp:49152 localabstract:someone-else\n"),
    )]);
    let cleanup: Box<dyn CleanupReceipt> = Box::new(AndroidCleanup {
        adb_path: PathBuf::from("/fake/adb"),
        runner: runner.clone(),
        serial: SERIAL.to_owned(),
        localabstract: "apppilotkit-android-0123456789abcdef0123456789abcdef".to_owned(),
        port: 49_152,
    });
    assert_eq!(
        cleanup
            .cleanup(Cancellation::new(), deadline())
            .expect_err("foreign mapping")
            .kind(),
        PlatformFailureKind::CleanupFailed
    );
    runner.assert_consumed();
}

#[test]
fn abort_is_a_no_io_idempotent_ownership_terminal() {
    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    must(
        adapter
            .begin_launch(
                selection(Platform::AndroidEmulator, SERIAL, PACKAGE, "/tmp/app.apk"),
                deadline(),
            )
            .abort(Cancellation::new(), deadline()),
        "abort",
    );
    runner.assert_consumed();
}

#[test]
fn oversized_descriptor_never_reaches_adb() {
    let runner = MockRunner::new(Vec::new());
    let adapter = AndroidEmulatorAdapter::with_runner("/fake/adb", runner.clone());
    let descriptor = must(
        PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1; DESCRIPTOR_LIMIT + 1]),
        "descriptor",
    );
    assert_eq!(
        adapter
            .begin_launch(
                selection(Platform::AndroidEmulator, SERIAL, PACKAGE, "/tmp/app.apk",),
                deadline(),
            )
            .launch(descriptor, Cancellation::new(), deadline())
            .err()
            .expect("oversized descriptor")
            .kind(),
        PlatformFailureKind::Rejected
    );
    runner.assert_consumed();
}

#[test]
fn fake_adb_process_receives_exact_serial_and_no_secret_canary() {
    let artifact = TestArtifact::real();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let fixture = FakeAdbFixture::new(port);
    let peer = thread::spawn(move || {
        let _ = listener.accept().expect("bootstrap connection");
    });
    let adapter = AndroidEmulatorAdapter::new(&fixture.script);
    let launched = must(
        adapter
            .begin_launch(selection_for_artifact(&artifact), deadline())
            .launch(descriptor(), Cancellation::new(), deadline()),
        "fake adb launch",
    );
    let (raw, _, cleanup) = launched.into_parts();
    raw.cancel();
    must(
        cleanup.cleanup(Cancellation::new(), deadline()),
        "fake adb cleanup",
    );
    peer.join().expect("peer");
    let log = fs::read_to_string(&fixture.log).expect("argv log");
    for line in log.lines() {
        assert!(line.starts_with(&format!("-s {SERIAL} ")), "{line}");
    }
    assert!(!log.contains(SECRET_CANARY));
    assert!(!log.contains(" reverse "));
    assert!(!log.contains(" --remove-all"));
}

#[test]
fn fake_adb_verified_forward_connect_timeout_removes_exact_mapping_and_keeps_primary() {
    let artifact = TestArtifact::real();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserved loopback port");
    let port = listener.local_addr().expect("loopback address").port();
    drop(listener);
    let fixture = FakeAdbFixture::new(port);
    let error = AndroidEmulatorAdapter::new(&fixture.script)
        .begin_launch(selection_for_artifact(&artifact), deadline_after(3_000))
        .launch(descriptor(), Cancellation::new(), deadline_after(3_000))
        .err()
        .expect("unserved verified forward times out");
    assert_eq!(error.kind(), PlatformFailureKind::TimedOut);
    assert_eq!(error.primary_kind(), PlatformFailureKind::TimedOut);
    let log = fs::read_to_string(&fixture.log).expect("argv log");
    assert!(log.contains(&format!("forward --remove tcp:{port}")));
    assert_eq!(log.matches("forward --list").count(), 4);
    assert!(log.contains(&format!("shell am force-stop {PACKAGE}")));
    assert!(!log.contains("--remove-all"));
}

#[test]
fn fake_adb_rollback_failure_and_foreign_mapping_are_fail_closed_without_foreign_remove() {
    let artifact = TestArtifact::real();
    for mode in [
        FakeRollbackMode::CommandFails,
        FakeRollbackMode::ForeignMapping,
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserved loopback port");
        let port = listener.local_addr().expect("loopback address").port();
        drop(listener);
        let fixture = FakeAdbFixture::with_rollback_mode(port, mode);
        let error = AndroidEmulatorAdapter::new(&fixture.script)
            .begin_launch(selection_for_artifact(&artifact), deadline_after(3_000))
            .launch(descriptor(), Cancellation::new(), deadline_after(3_000))
            .err()
            .expect("rollback failure is fail closed");
        assert_eq!(
            error.kind(),
            PlatformFailureKind::CleanupFailed,
            "rollback mode: {mode:?}"
        );
        assert_eq!(error.primary_kind(), PlatformFailureKind::TimedOut);
        assert!(error.cleanup_failed());
        let log = fs::read_to_string(&fixture.log).expect("argv log");
        match mode {
            FakeRollbackMode::CommandFails => {
                assert!(log.contains(&format!("forward --remove tcp:{port}")));
            }
            FakeRollbackMode::ForeignMapping => {
                assert!(!log.contains("forward --remove"));
            }
        }
        assert!(log.contains(&format!("shell am force-stop {PACKAGE}")));
        assert!(!log.contains("--remove-all"));
    }
}

struct FakeAdbFixture {
    directory: PathBuf,
    script: PathBuf,
    log: PathBuf,
}

#[derive(Clone, Copy, Debug)]
enum FakeRollbackMode {
    CommandFails,
    ForeignMapping,
}

impl FakeAdbFixture {
    fn new(port: u16) -> Self {
        Self::with_mode(port, None)
    }

    fn with_rollback_mode(port: u16, mode: FakeRollbackMode) -> Self {
        Self::with_mode(port, Some(mode))
    }

    fn with_mode(port: u16, rollback_mode: Option<FakeRollbackMode>) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "apppilotkit-android-adapter-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("fixture directory");
        let script = directory.join("fake-adb");
        let log = directory.join("argv.log");
        let state = directory.join("forward.state");
        let list_count = directory.join("forward-list-count");
        let rollback_mode = match rollback_mode {
            None => "none",
            Some(FakeRollbackMode::CommandFails) => "command-fails",
            Some(FakeRollbackMode::ForeignMapping) => "foreign-mapping",
        };
        let source = format!(
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{log}'
test "$1" = '-s'
test "$2" = '{serial}'
case "$3" in
  help)
    printf 'forward tcp:0 localabstract:\n'
    ;;
  get-state)
    printf 'device\n'
    ;;
  install)
    printf 'Success\n'
    ;;
  shell)
    if test "$4" = 'cmd'; then
      printf '%s\n' "${{10}}"
    elif test "$4" = 'am' && test "$5" = 'force-stop'; then
      test "$6" = '{package}'
    else
      component="$9"
      printf 'Stopping: {package}\nStarting: Intent {{ cmp=%s (has extras) }}\nStatus: ok\nLaunchState: COLD\nActivity: %s\nTotalTime: 1\nWaitTime: 2\nComplete\n' "$component" "$component"
    fi
    ;;
  forward)
    if test "$4" = 'tcp:0'; then
      printf '%s' "$5" > '{state}'
      printf '{port}\n'
    elif test "$4" = '--list'; then
      count=0
      if test -s '{list_count}'; then
        count="$(cat '{list_count}')"
      fi
      count=$((count + 1))
      printf '%s' "$count" > '{list_count}'
      if test '{rollback_mode}' = 'foreign-mapping' && test "$count" -ge 3; then
        printf '{serial} tcp:{port} localabstract:foreign\n'
      elif test -s '{state}'; then
        printf '{serial} tcp:{port} %s\n' "$(cat '{state}')"
      fi
    elif test "$4" = '--remove'; then
      if test '{rollback_mode}' = 'command-fails'; then
        exit 64
      fi
      : > '{state}'
    else
      exit 64
    fi
    ;;
  *) exit 64 ;;
esac
"#,
            log = log.display(),
            serial = SERIAL,
            package = PACKAGE,
            state = state.display(),
            list_count = list_count.display(),
            rollback_mode = rollback_mode,
        );
        fs::write(&script, source).expect("fake adb");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("permissions");
        Self {
            directory,
            script,
            log,
        }
    }
}

impl Drop for FakeAdbFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
