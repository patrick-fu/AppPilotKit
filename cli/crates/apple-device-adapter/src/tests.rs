use super::*;
use apppilotkit_apple_simulator_adapter::inspect_ios_app_tree_digest;
use apppilotkit_host_runtime::adapter::PlatformTargetAdapter;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const HARDWARE_UDID: &str = "00008150-001E60460130401C";
const COREDEVICE_ID: &str = "ACEB2C1B-F075-523A-B80B-B2F0D463C313";
const PROCESS_SUFFIX: &str = "/DeviceHost.app/DeviceHost";
const OWNED_APP_PATH: &str = "/private/var/containers/Bundle/Application/AAAA/DeviceHost.app";
const OWNED_PROCESS_PATH: &str =
    "/private/var/containers/Bundle/Application/AAAA/DeviceHost.app/DeviceHost";
const COLLIDING_PROCESS_PATH: &str =
    "/private/var/containers/Bundle/Application/BBBB/DeviceHost.app/DeviceHost";
static NEXT_TEST_ID: AtomicUsize = AtomicUsize::new(1);

fn deadline_after(ms: u64) -> AbsoluteDeadline {
    require(AbsoluteDeadline::new(require(unix_ms()) + ms))
}

fn require<T>(result: Result<T, PlatformFailure>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("valid test carrier"),
    }
}

fn require_err<T>(result: Result<T, PlatformFailure>) -> PlatformFailure {
    match result {
        Ok(_) => panic!("expected platform failure"),
        Err(error) => error,
    }
}

fn success(stdout: Vec<u8>) -> ToolOutput {
    ToolOutput {
        status: ExitStatus::from_raw(0),
        stdout,
        stderr: Vec::new(),
        oversized: false,
    }
}

fn success_stderr(stderr: Vec<u8>) -> ToolOutput {
    ToolOutput {
        status: ExitStatus::from_raw(0),
        stdout: Vec::new(),
        stderr,
        oversized: false,
    }
}

fn failed() -> ToolOutput {
    ToolOutput {
        status: ExitStatus::from_raw(1 << 8),
        stdout: Vec::new(),
        stderr: Vec::new(),
        oversized: false,
    }
}

struct TempArtifact {
    root: PathBuf,
    app: PathBuf,
}

impl TempArtifact {
    fn new(label: &str) -> Self {
        Self::with_app_id(label, "dev.apppilotkit.DeviceHost")
    }

    fn with_app_id(label: &str, app_id: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let temp = fs::canonicalize(std::env::temp_dir()).expect("canonical test temp");
        let root = temp.join(format!(
            "apppilotkit-ios-device-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temp root");
        let app = simple_bundle(&root, app_id);
        Self { root, app }
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn simple_bundle(root: &Path, app_id: &str) -> PathBuf {
    let app = root.join("DeviceHost.app");
    fs::create_dir(&app).expect("app dir");
    fs::write(
        app.join("Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{app_id}</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>1</string><key>CFBundleExecutable</key><string>DeviceHost</string></dict></plist>"
        ),
    )
    .expect("plist");
    fs::write(app.join("DeviceHost"), b"MACHO").expect("executable");
    fs::set_permissions(app.join("DeviceHost"), fs::Permissions::from_mode(0o755))
        .expect("executable mode");
    app
}

fn digest_for(app: &Path, app_id: &str) -> [u8; 32] {
    require(inspect_ios_app_tree_digest(
        app,
        app_id,
        &Cancellation::new(),
        deadline_after(5_000),
    ))
}

fn selection(path: &Path, app_id: &str, digest: [u8; 32]) -> TargetSelection {
    require(TargetSelection::new(
        Platform::IosDevice,
        HARDWARE_UDID.to_owned(),
        app_id.to_owned(),
        path.to_string_lossy().into_owned(),
        digest,
    ))
}

#[derive(Clone)]
struct RecordedRequest {
    args: Vec<OsString>,
    descriptor_env: Option<OsString>,
    scrub: bool,
}

impl RecordedRequest {
    fn arg_strings(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn contains(&self, value: &str) -> bool {
        self.args.iter().any(|arg| arg == value)
    }
}

#[derive(Clone)]
struct FakeProcess {
    pid: u32,
    executable: String,
}

struct FakeRunner {
    program: PathBuf,
    app_id: String,
    requests: Mutex<Vec<RecordedRequest>>,
    pid: AtomicU32,
    launch_calls: AtomicUsize,
    install_calls: AtomicUsize,
    uninstall_calls: AtomicUsize,
    terminate_calls: AtomicUsize,
    apps_present: AtomicBool,
    install_fails: AtomicBool,
    uninstall_fails: AtomicBool,
    processes: Mutex<Vec<FakeProcess>>,
    terminate_status_zero: AtomicBool,
    installed_app_path: Mutex<String>,
    last_install_path: Mutex<Option<PathBuf>>,
    launch_executable: Mutex<Option<String>>,
    invalid_processes_json: AtomicBool,
    respawn_exact_after_terminate: AtomicBool,
}

impl FakeRunner {
    fn new(app_id: &str) -> Arc<Self> {
        Arc::new(Self {
            program: PathBuf::from("/fake/xcrun"),
            app_id: app_id.to_owned(),
            requests: Mutex::new(Vec::new()),
            pid: AtomicU32::new(42_424),
            launch_calls: AtomicUsize::new(0),
            install_calls: AtomicUsize::new(0),
            uninstall_calls: AtomicUsize::new(0),
            terminate_calls: AtomicUsize::new(0),
            apps_present: AtomicBool::new(false),
            install_fails: AtomicBool::new(false),
            uninstall_fails: AtomicBool::new(false),
            processes: Mutex::new(Vec::new()),
            terminate_status_zero: AtomicBool::new(true),
            installed_app_path: Mutex::new(OWNED_APP_PATH.to_owned()),
            last_install_path: Mutex::new(None),
            launch_executable: Mutex::new(None),
            invalid_processes_json: AtomicBool::new(false),
            respawn_exact_after_terminate: AtomicBool::new(false),
        })
    }

    fn owned_process_path(&self) -> String {
        format!(
            "{}/DeviceHost",
            self.installed_app_path.lock().expect("app path")
        )
    }

    fn apps_url(&self) -> String {
        format!(
            "file://{}",
            self.installed_app_path.lock().expect("app path")
        )
    }

    fn seed_occupied(&self) {
        self.processes.lock().expect("processes").push(FakeProcess {
            pid: 7_001,
            executable: OWNED_PROCESS_PATH.to_owned(),
        });
    }
}

fn json_output_path_from(args: &[String]) -> Option<PathBuf> {
    args.windows(2)
        .find(|window| window[0] == "--json-output")
        .map(|window| PathBuf::from(&window[1]))
}

fn write_json(path: &Path, body: &str) {
    fs::write(path, body).expect("json output");
}

impl ToolRunner for Arc<FakeRunner> {
    fn program(&self) -> &Path {
        &self.program
    }

    fn run(
        &self,
        request: ToolRequest,
        _cap: usize,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ToolOutput, PlatformFailure> {
        check_cancel_deadline(cancellation, deadline)?;
        self.requests
            .lock()
            .expect("requests")
            .push(RecordedRequest {
                args: request.args.clone(),
                descriptor_env: request.descriptor_env.clone(),
                scrub: request.scrub_devicectl_child,
            });
        let args = request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if args.get(0).map(String::as_str) == Some("devicectl")
            && args.get(1).map(String::as_str) == Some("help")
        {
            return Ok(success_stderr(
                b"DEVICECTL_CHILD_ --json-output --device <identifier>\n".to_vec(),
            ));
        }
        let json_path = json_output_path_from(&args);
        if args.get(1..4) == Some(&["device".into(), "info".into(), "apps".into()]) {
            let body = if self.apps_present.load(Ordering::SeqCst) {
                format!(
                    "{{\"result\":{{\"apps\":[{{\"bundleIdentifier\":\"{}\",\"url\":\"{}\"}}]}}}}",
                    self.app_id,
                    self.apps_url()
                )
            } else {
                "{\"result\":{\"apps\":[],\"matchingBundleIdentifier\":\"dev.apppilotkit.DeviceHost\"}}".to_owned()
            };
            write_json(json_path.as_ref().expect("apps json"), &body);
            return Ok(success(Vec::new()));
        }
        if args.get(1..4) == Some(&["device".into(), "install".into(), "app".into()]) {
            let install_path = args
                .iter()
                .find(|arg| arg.ends_with(".app"))
                .map(PathBuf::from)
                .expect("install requires the host .app");
            *self.last_install_path.lock().expect("install path") = Some(install_path);
            self.install_calls.fetch_add(1, Ordering::SeqCst);
            self.apps_present.store(true, Ordering::SeqCst);
            write_json(json_path.as_ref().expect("install json"), "{\"result\":{}}");
            if self.install_fails.load(Ordering::SeqCst) {
                return Ok(failed());
            }
            return Ok(success(Vec::new()));
        }
        if args.get(1..4) == Some(&["device".into(), "uninstall".into(), "app".into()]) {
            self.uninstall_calls.fetch_add(1, Ordering::SeqCst);
            write_json(
                json_path.as_ref().expect("uninstall json"),
                "{\"result\":{}}",
            );
            if self.uninstall_fails.load(Ordering::SeqCst) {
                return Ok(failed());
            }
            self.apps_present.store(false, Ordering::SeqCst);
            return Ok(success(Vec::new()));
        }
        if args.get(1..4) == Some(&["device".into(), "info".into(), "processes".into()]) {
            if self.invalid_processes_json.load(Ordering::SeqCst) {
                write_json(json_path.as_ref().expect("process json"), "not-json");
                return Ok(success(Vec::new()));
            }
            let processes = self.processes.lock().expect("processes");
            let entries = processes
                .iter()
                .map(|process| {
                    format!(
                        "{{\"processIdentifier\":{},\"executable\":\"{}\"}}",
                        process.pid, process.executable
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            write_json(
                json_path.as_ref().expect("process json"),
                &format!("{{\"result\":{{\"runningProcesses\":[{entries}]}}}}"),
            );
            return Ok(success(Vec::new()));
        }
        if args.get(1..4) == Some(&["device".into(), "process".into(), "launch".into()]) {
            assert!(
                !args.iter().any(|arg| arg == "--terminate-existing"),
                "ownership is the launch PID, not terminate-existing"
            );
            self.launch_calls.fetch_add(1, Ordering::SeqCst);
            let pid = self.pid.load(Ordering::Acquire);
            let executable = self
                .launch_executable
                .lock()
                .expect("launch executable")
                .clone()
                .unwrap_or_else(|| self.owned_process_path());
            self.processes
                .lock()
                .expect("processes")
                .push(FakeProcess { pid, executable });
            write_json(
                json_path.as_ref().expect("launch json"),
                &format!("{{\"result\":{{\"process\":{{\"processIdentifier\":{pid}}}}}}}"),
            );
            return Ok(success(Vec::new()));
        }
        if args.get(1..4) == Some(&["device".into(), "process".into(), "terminate".into()]) {
            self.terminate_calls.fetch_add(1, Ordering::SeqCst);
            let pid = self.pid.load(Ordering::Acquire);
            assert_eq!(
                args.iter()
                    .position(|arg| arg == "--pid")
                    .and_then(|index| args.get(index + 1)),
                Some(&pid.to_string())
            );
            if self.terminate_status_zero.load(Ordering::SeqCst) {
                self.processes
                    .lock()
                    .expect("processes")
                    .retain(|process| process.pid != pid);
                if self.respawn_exact_after_terminate.load(Ordering::SeqCst) {
                    self.processes.lock().expect("processes").push(FakeProcess {
                        pid: pid.saturating_add(1),
                        executable: self.owned_process_path(),
                    });
                }
                write_json(
                    json_path.as_ref().expect("terminate json"),
                    "{\"result\":{}}",
                );
                return Ok(success(Vec::new()));
            }
            write_json(
                json_path.as_ref().expect("terminate json"),
                "{\"result\":{}}",
            );
            return Ok(failed());
        }
        panic!("unexpected fake request: {args:?}");
    }
}

struct FakeUsbMux {
    serial: String,
    connection: Mutex<String>,
    device_id: u32,
    list_calls: AtomicUsize,
    connects: Mutex<Vec<(u32, u16)>>,
    refuse_remaining: AtomicUsize,
    peer: Mutex<Option<UnixStream>>,
}

impl FakeUsbMux {
    fn usb() -> Arc<Self> {
        Arc::new(Self {
            serial: HARDWARE_UDID.to_owned(),
            connection: Mutex::new("USB".to_owned()),
            device_id: 7,
            list_calls: AtomicUsize::new(0),
            connects: Mutex::new(Vec::new()),
            refuse_remaining: AtomicUsize::new(0),
            peer: Mutex::new(None),
        })
    }

    fn set_connection(&self, value: &str) {
        *self.connection.lock().expect("connection") = value.to_owned();
    }

    fn refuse_first_connect(&self) {
        self.refuse_remaining.store(1, Ordering::SeqCst);
    }
}

impl UsbMux for Arc<FakeUsbMux> {
    fn find_usb_device(
        &self,
        serial: &str,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<u32, PlatformFailure> {
        check_cancel_deadline(cancellation, deadline)?;
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if serial != self.serial {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        if self.connection.lock().expect("connection").as_str() != "USB" {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        Ok(self.device_id)
    }

    fn connect(
        &self,
        device_id: u32,
        port: u16,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<Arc<dyn RawDuplex>, super::usbmux::UsbMuxConnectError> {
        check_cancel_deadline(cancellation, deadline)?;
        self.connects
            .lock()
            .expect("connects")
            .push((device_id, port));
        if device_id != self.device_id {
            return Err(failure(PlatformFailureKind::Unavailable).into());
        }
        if self
            .refuse_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            return Err(super::usbmux::UsbMuxConnectError::ConnectionRefused);
        }
        let (client, mut server) =
            UnixStream::pair().map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        server
            .write_all(b"device-bootstrap")
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        *self.peer.lock().expect("peer") = Some(server);
        Ok(Arc::new(super::usbmux::UnixRawDuplex::new(client)?))
    }
}

fn test_adapter(runner: Arc<FakeRunner>, usbmux: Arc<FakeUsbMux>) -> AppleDeviceAdapter {
    AppleDeviceAdapter {
        runner: Arc::new(runner),
        usbmux: Arc::new(usbmux),
    }
}

fn launch_once(
    adapter: &AppleDeviceAdapter,
    artifact: &TempArtifact,
    app_id: &str,
    digest: [u8; 32],
) -> Result<LaunchedTargetIo, PlatformFailure> {
    adapter
        .begin_launch(
            selection(&artifact.app, app_id, digest),
            deadline_after(5_000),
        )
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(
                b"public".to_vec(),
            )),
            Cancellation::new(),
            deadline_after(5_000),
        )
}

#[test]
fn coredevice_uuid_selector_is_rejected_without_usb() {
    let artifact = TempArtifact::new("coredevice");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let invalid = require(TargetSelection::new(
        Platform::IosDevice,
        COREDEVICE_ID.to_owned(),
        app_id.to_owned(),
        artifact.app.to_string_lossy().into_owned(),
        digest,
    ));
    let failure = require_err(adapter.begin_launch(invalid, deadline_after(1_000)).launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(1_000),
    ));
    assert_eq!(failure.kind(), PlatformFailureKind::Rejected);
    assert_eq!(usb.list_calls.load(Ordering::SeqCst), 0);
    assert!(runner.requests.lock().expect("requests").is_empty());
}

#[test]
fn usb_miss_and_network_only_are_unavailable() {
    let artifact = TempArtifact::new("usb-miss");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    usb.set_connection("Network");
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(
        adapter
            .begin_launch(
                selection(&artifact.app, app_id, digest),
                deadline_after(1_000),
            )
            .launch(
                require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
                Cancellation::new(),
                deadline_after(1_000),
            ),
    );
    assert_eq!(failure.kind(), PlatformFailureKind::Unavailable);
    assert_eq!(usb.list_calls.load(Ordering::SeqCst), 1);
    assert!(runner.requests.lock().expect("requests").is_empty());
}

#[test]
fn digest_mismatch_is_rejected_after_usb_gate() {
    let artifact = TempArtifact::new("digest");
    let app_id = "dev.apppilotkit.DeviceHost";
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(
        adapter
            .begin_launch(
                selection(&artifact.app, app_id, [0x11; 32]),
                deadline_after(1_000),
            )
            .launch(
                require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
                Cancellation::new(),
                deadline_after(1_000),
            ),
    );
    assert_eq!(failure.kind(), PlatformFailureKind::Rejected);
    assert_eq!(usb.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn occupied_process_list_is_rejected_without_launch() {
    let artifact = TempArtifact::new("occupied");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    runner.seed_occupied();
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(launch_once(&adapter, &artifact, app_id, digest));
    assert_eq!(failure.kind(), PlatformFailureKind::Rejected);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn launch_installs_proves_pid_and_cleans_lease() {
    let artifact = TempArtifact::new("launch");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let pending = adapter.begin_launch(
        selection(&artifact.app, app_id, digest),
        deadline_after(5_000),
    );
    let port = pending.endpoint().ios_port().expect("iOS loopback");
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(
            b"public".to_vec(),
        )),
        Cancellation::new(),
        deadline_after(5_000),
    ));
    let (bootstrap, connector, cleanup) = launched.into_parts();
    let mut buffer = [0_u8; 32];
    let read = require(bootstrap.read(&mut buffer, deadline_after(1_000)));
    assert_eq!(&buffer[..read], b"device-bootstrap");
    require(cleanup.cleanup(Cancellation::new(), deadline_after(2_000)));
    let _ = connector;
    let requests = runner.requests.lock().expect("requests").clone();
    assert!(
        requests
            .iter()
            .any(|request| request.arg_strings().get(1..4)
                == Some(&["device".into(), "info".into(), "apps".into()]))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.arg_strings().get(1..4)
                == Some(&["device".into(), "install".into(), "app".into()]))
    );
    let launch = requests
        .iter()
        .find(|request| {
            request.scrub
                && request.contains("launch")
                && request.contains("--json-output")
                && !request.contains("help")
        })
        .expect("launch request");
    assert!(launch.scrub);
    assert_eq!(
        launch.descriptor_env.as_deref(),
        Some(OsStr::new(&URL_SAFE_NO_PAD.encode(b"public")))
    );
    assert!(!launch.contains("--terminate-existing"));
    assert_eq!(
        launch
            .descriptor_env
            .as_ref()
            .map(|value| value.to_string_lossy().into_owned()),
        Some(URL_SAFE_NO_PAD.encode(b"public"))
    );
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*usb.connects.lock().expect("connects"), [(7, port)]);
    assert!(usb.list_calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn launch_takeover_does_not_retry_connrefused() {
    let artifact = TempArtifact::new("launch-takeover");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    usb.refuse_first_connect();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let pending = adapter.begin_launch(
        selection(&artifact.app, app_id, digest),
        deadline_after(5_000),
    );
    let port = pending.endpoint().ios_port().expect("iOS loopback");
    let failure = require_err(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(
            b"public".to_vec(),
        )),
        Cancellation::new(),
        deadline_after(5_000),
    ));
    assert_eq!(failure.kind(), PlatformFailureKind::Unavailable);
    assert_eq!(*usb.connects.lock().expect("connects"), [(7, port)]);
    assert_eq!(usb.list_calls.load(Ordering::SeqCst), 2);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn present_app_is_rejected_without_install_or_launch() {
    let artifact = TempArtifact::new("present");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    runner.apps_present.store(true, Ordering::SeqCst);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(launch_once(&adapter, &artifact, app_id, digest));
    assert_eq!(failure.kind(), PlatformFailureKind::Rejected);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 0);
    assert!(usb.connects.lock().expect("connects").is_empty());
}

#[test]
fn command_environment_scrubs_other_devicectl_child_secrets() {
    let request = ToolRequest::launch(
        Path::new("/fake/xcrun"),
        HARDWARE_UDID,
        "dev.apppilotkit.DeviceHost",
        PathBuf::from("/tmp/out.json"),
        URL_SAFE_NO_PAD.encode(b"public descriptor"),
    );
    let command = command_for_with_environment(
        request,
        [
            OsString::from("DEVICECTL_CHILD_PROCESS_BOOTSTRAP_SECRET"),
            OsString::from("PATH"),
        ],
    );
    let overrides = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
        .collect::<Vec<_>>();
    assert!(overrides.iter().any(|(key, value)| {
        key == OsStr::new("DEVICECTL_CHILD_PROCESS_BOOTSTRAP_SECRET") && value.is_none()
    }));
    assert!(overrides.iter().any(|(key, value)| {
        key == OsStr::new(DESCRIPTOR_ENV)
            && value.as_deref() == Some(OsStr::new(&URL_SAFE_NO_PAD.encode(b"public descriptor")))
    }));
}

#[test]
fn production_defaults_are_explicit_paths() {
    let adapter = AppleDeviceAdapter::default();
    assert_eq!(adapter.runner.program(), Path::new("/usr/bin/xcrun"));
    assert_eq!(super::usbmux::DEFAULT_USBMUXD, "/var/run/usbmuxd");
}

#[test]
fn parse_launch_pid_requires_the_json_output_process() {
    assert_eq!(
        require(parse_launch_pid(
            br#"{"result":{"process":{"processIdentifier":42424}}}"#
        )),
        42_424
    );
    assert_eq!(
        require_err(parse_launch_pid(br#"{"result":{"processIdentifier":99}}"#)).kind(),
        PlatformFailureKind::Rejected
    );
    assert_eq!(
        require_err(parse_launch_pid(
            br#"{"info":{"process":{"processIdentifier":42424}}}"#
        ))
        .kind(),
        PlatformFailureKind::Rejected
    );
    assert_eq!(
        require_err(parse_launch_pid(br#"{"result":{"process":{}}}"#)).kind(),
        PlatformFailureKind::Rejected
    );
}

#[test]
fn terminate_of_already_exited_pid_is_ok() {
    let runner = FakeRunner::new("dev.apppilotkit.DeviceHost");
    require(terminate_owned_pid(
        &runner,
        HARDWARE_UDID,
        42_424,
        OWNED_PROCESS_PATH,
        &Cancellation::new(),
        deadline_after(1_000),
    ));
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn matching_processes_use_app_bundle_executable_suffix() {
    let matches = require(parse_matching_processes(
        br#"{"result":{"runningProcesses":[
            {"processIdentifier":1,"executable":"/var/containers/Bundle/Application/X/DeviceHost.app/DeviceHost"},
            {"processIdentifier":2,"executable":"/usr/bin/DeviceHost"},
            {"processIdentifier":3,"executable":"/private/var/containers/Bundle/Application/Y/Other.app/DeviceHost"}
        ]}}"#,
        ProcessIdentity::Suffix(PROCESS_SUFFIX),
    ));
    assert_eq!(matches, [1]);
}

#[test]
fn matching_processes_require_exact_apps_url_path() {
    let matches = require(parse_matching_processes(
        br#"{"result":{"runningProcesses":[
            {"processIdentifier":1,"executable":"/private/var/containers/Bundle/Application/AAAA/DeviceHost.app/DeviceHost"},
            {"processIdentifier":2,"executable":"/private/var/containers/Bundle/Application/BBBB/DeviceHost.app/DeviceHost"}
        ]}}"#,
        ProcessIdentity::Exact(OWNED_PROCESS_PATH),
    ));
    assert_eq!(matches, [1]);
}

#[test]
fn absent_apps_catalog_is_empty_result_apps() {
    assert!(!require(parse_installed_app_present(
        br#"{"result":{"apps":[],"matchingBundleIdentifier":"dev.apppilotkit.DeviceHost"}}"#,
        "dev.apppilotkit.DeviceHost",
    )));
}

#[test]
fn failed_install_that_leaves_the_app_is_uninstalled() {
    let artifact = TempArtifact::new("install-fail");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    runner.install_fails.store(true, Ordering::SeqCst);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(launch_once(&adapter, &artifact, app_id, digest));
    assert_eq!(failure.kind(), PlatformFailureKind::Rejected);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 0);
    assert!(!runner.apps_present.load(Ordering::SeqCst));
}

#[test]
fn failed_install_leftover_uninstall_failure_is_cleanup_failed() {
    let app_id = "dev.apppilotkit.DeviceHostLeftover";
    let artifact = TempArtifact::with_app_id("install-fail-cleanup", app_id);
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    runner.install_fails.store(true, Ordering::SeqCst);
    runner.uninstall_fails.store(true, Ordering::SeqCst);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(launch_once(&adapter, &artifact, app_id, digest));
    assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.install_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn terminate_of_pid_with_different_executable_does_not_kill() {
    let runner = FakeRunner::new("dev.apppilotkit.DeviceHost");
    runner
        .processes
        .lock()
        .expect("processes")
        .push(FakeProcess {
            pid: 42_424,
            executable: "/sbin/launchd".to_owned(),
        });
    let failure = require_err(terminate_owned_pid(
        &runner,
        HARDWARE_UDID,
        42_424,
        OWNED_PROCESS_PATH,
        &Cancellation::new(),
        deadline_after(1_000),
    ));
    assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn session_reconnect_does_not_retry_connrefused() {
    let artifact = TempArtifact::new("session-reconnect");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let launched = require(launch_once(&adapter, &artifact, app_id, digest));
    let (bootstrap, connector, cleanup) = launched.into_parts();
    usb.refuse_first_connect();
    let before = usb.connects.lock().expect("connects").len();
    let failure = require_err(connector.connect(Cancellation::new(), deadline_after(2_000)));
    assert_eq!(failure.kind(), PlatformFailureKind::Unavailable);
    assert_eq!(usb.connects.lock().expect("connects").len(), before + 1);
    require(cleanup.cleanup(Cancellation::new(), deadline_after(2_000)));
    let _ = bootstrap;
}

#[test]
fn install_uses_snapshot_path_not_the_caller_path() {
    let artifact = TempArtifact::new("snapshot-path");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let launched = require(launch_once(&adapter, &artifact, app_id, digest));
    require(
        launched
            .into_parts()
            .2
            .cleanup(Cancellation::new(), deadline_after(2_000)),
    );
    let installed = runner
        .last_install_path
        .lock()
        .expect("install path")
        .clone()
        .expect("install path recorded");
    assert_ne!(installed, artifact.app);
    assert_eq!(
        installed.file_name().and_then(|name| name.to_str()),
        Some("DeviceHost.app")
    );
    assert_ne!(
        installed.file_name().and_then(|name| name.to_str()),
        Some("snapshot.app")
    );
}

#[test]
fn post_install_prove_requires_exact_apps_url_path() {
    let artifact = TempArtifact::new("exact-prove");
    let app_id = "dev.apppilotkit.DeviceHost";
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    *runner.launch_executable.lock().expect("launch executable") =
        Some(COLLIDING_PROCESS_PATH.to_owned());
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let failure = require_err(launch_once(&adapter, &artifact, app_id, digest));
    assert_eq!(failure.kind(), PlatformFailureKind::Rejected);
    assert_eq!(runner.launch_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn terminate_refuses_suffix_collision_at_a_different_url() {
    let runner = FakeRunner::new("dev.apppilotkit.DeviceHost");
    runner
        .processes
        .lock()
        .expect("processes")
        .push(FakeProcess {
            pid: 42_424,
            executable: COLLIDING_PROCESS_PATH.to_owned(),
        });
    let failure = require_err(terminate_owned_pid(
        &runner,
        HARDWARE_UDID,
        42_424,
        OWNED_PROCESS_PATH,
        &Cancellation::new(),
        deadline_after(1_000),
    ));
    assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn uninstall_is_skipped_when_owned_apps_url_drifted() {
    let app_id = "dev.apppilotkit.DeviceHostUrlDrift";
    let artifact = TempArtifact::with_app_id("url-drift", app_id);
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let launched = require(launch_once(&adapter, &artifact, app_id, digest));
    *runner.installed_app_path.lock().expect("app path") =
        "/private/var/containers/Bundle/Application/CCCC/DeviceHost.app".to_owned();
    let failure = require_err(
        launched
            .into_parts()
            .2
            .cleanup(Cancellation::new(), deadline_after(2_000)),
    );
    assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn uninstall_is_skipped_when_exact_path_process_still_listed() {
    let app_id = "dev.apppilotkit.DeviceHostExactListed";
    let artifact = TempArtifact::with_app_id("exact-still-listed", app_id);
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    runner
        .respawn_exact_after_terminate
        .store(true, Ordering::SeqCst);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let launched = require(launch_once(&adapter, &artifact, app_id, digest));
    let failure = require_err(
        launched
            .into_parts()
            .2
            .cleanup(Cancellation::new(), deadline_after(2_000)),
    );
    assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn cleanup_maps_process_json_failure_to_cleanup_failed() {
    let app_id = "dev.apppilotkit.DeviceHostCleanupJson";
    let artifact = TempArtifact::with_app_id("cleanup-json", app_id);
    let digest = digest_for(&artifact.app, app_id);
    let runner = FakeRunner::new(app_id);
    let usb = FakeUsbMux::usb();
    let adapter = test_adapter(Arc::clone(&runner), Arc::clone(&usb));
    let launched = require(launch_once(&adapter, &artifact, app_id, digest));
    runner.invalid_processes_json.store(true, Ordering::SeqCst);
    let failure = require_err(
        launched
            .into_parts()
            .2
            .cleanup(Cancellation::new(), deadline_after(2_000)),
    );
    assert_eq!(failure.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.terminate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runner.uninstall_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn parse_device_app_url_accepts_file_url_from_apps_wire() {
    assert_eq!(
        require(parse_device_app_url(
            "file:///private/var/containers/Bundle/Application/AAAA/DeviceHost.app"
        )),
        OWNED_APP_PATH
    );
    assert_eq!(
        require_err(parse_device_app_url(
            "/private/var/containers/Bundle/Application/AAAA/DeviceHost.app"
        ))
        .kind(),
        PlatformFailureKind::Rejected
    );
}
