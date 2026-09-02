use super::*;
use apppilotkit_host_runtime::adapter::PlatformTargetAdapter;
use sha2::Digest as _;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};

const UDID: &str = "E28F8D8E-6211-4287-930B-1C2785D75A37";
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

fn inspect_test_bundle(
    path: &Path,
    app_id: &str,
) -> Result<artifact::TreeIdentity, PlatformFailure> {
    artifact::inspect_bundle(path, app_id, &Cancellation::new(), deadline_after(5_000))
}

#[test]
fn host_private_digest_bridge_reuses_the_canonical_bundle_scanner() {
    let artifact = TempArtifact::new("host-digest-bridge");
    let app_id = "com.example.HostDigest";
    let app = simple_bundle(&artifact.root, app_id, false);
    let expected = require(inspect_test_bundle(&app, app_id)).digest;

    assert_eq!(
        require(inspect_ios_app_tree_digest(
            &app,
            app_id,
            &Cancellation::new(),
            deadline_after(5_000),
        )),
        expected
    );
    assert!(
        inspect_ios_app_tree_digest(
            &app,
            "com.example.Other",
            &Cancellation::new(),
            deadline_after(5_000),
        )
        .is_err()
    );
}

fn prepare_test_snapshot(
    path: &Path,
    app_id: &str,
    digest: &[u8; 32],
) -> Result<artifact::PreparedArtifact, PlatformFailure> {
    artifact::prepare_snapshot(
        path,
        app_id,
        digest,
        &Cancellation::new(),
        deadline_after(5_000),
    )
}

fn simple_bundle(root: &Path, app_id: &str, binary_plist: bool) -> PathBuf {
    let app = root.join("Simple.app");
    fs::create_dir(&app).expect("simple app");
    let plist_path = app.join("Info.plist");
    fs::write(
        &plist_path,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{app_id}</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>1</string><key>CFBundleExecutable</key><string>SmokeHost</string></dict></plist>"
        ),
    )
    .expect("XML plist");
    if binary_plist {
        assert!(
            Command::new("/usr/bin/plutil")
                .args(["-convert", "binary1"])
                .arg(&plist_path)
                .status()
                .expect("plutil")
                .success()
        );
    }
    fs::write(app.join("SmokeHost"), b"MACHO").expect("executable");
    fs::set_permissions(app.join("SmokeHost"), fs::Permissions::from_mode(0o755))
        .expect("executable mode");
    app
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

struct TempArtifact {
    root: PathBuf,
    app: PathBuf,
}

impl TempArtifact {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let temp = fs::canonicalize(std::env::temp_dir()).expect("canonical test temp");
        let root = temp.join(format!(
            "apppilotkit-d3-{label}-{}-{id}",
            std::process::id()
        ));
        let app = root.join("SmokeHost.app");
        fs::create_dir_all(&app).expect("temp app");
        Self { root, app }
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn selection(path: &Path, app_id: &str, digest: [u8; 32]) -> TargetSelection {
    require(TargetSelection::new(
        Platform::IosSimulator,
        UDID.to_owned(),
        app_id.to_owned(),
        path.to_string_lossy().into_owned(),
        digest,
    ))
}

fn device_json() -> Vec<u8> {
    format!(
        r#"{{"devices":{{"runtime":[{{"udid":"{UDID}","state":"Booted","isAvailable":true}}]}}}}"#
    )
    .into_bytes()
}

#[derive(Clone)]
struct RecordedRequest {
    args: Vec<OsString>,
    descriptor_env: Option<OsString>,
    scrub: bool,
}

struct AcceptInstalledArtifact(PathBuf);

impl ArtifactVerifier for AcceptInstalledArtifact {
    fn prepare(
        &self,
        selection: &TargetSelection,
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<PreparedLaunchArtifact, PlatformFailure> {
        Ok(PreparedLaunchArtifact {
            snapshot_path: fs::canonicalize(&self.0)
                .map_err(|_| failure(PlatformFailureKind::Rejected))?,
            digest: selection.artifact_digest(),
            executable: "SmokeHost".to_owned(),
            production: None,
        })
    }

    fn assert_snapshot_unchanged(
        &self,
        _prepared: &PreparedLaunchArtifact,
        _selection: &TargetSelection,
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        Ok(())
    }

    fn verify_installed(
        &self,
        installed: &Path,
        _prepared: &PreparedLaunchArtifact,
        _selection: &TargetSelection,
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        if fs::canonicalize(installed).ok() == fs::canonicalize(&self.0).ok() {
            Ok(())
        } else {
            Err(failure(PlatformFailureKind::Rejected))
        }
    }
}

struct RejectInstalledArtifact(PathBuf);

impl ArtifactVerifier for RejectInstalledArtifact {
    fn prepare(
        &self,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<PreparedLaunchArtifact, PlatformFailure> {
        AcceptInstalledArtifact(self.0.clone()).prepare(selection, cancellation, deadline)
    }

    fn assert_snapshot_unchanged(
        &self,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        AcceptInstalledArtifact(self.0.clone()).assert_snapshot_unchanged(
            prepared,
            selection,
            cancellation,
            deadline,
        )
    }

    fn verify_installed(
        &self,
        _installed: &Path,
        _prepared: &PreparedLaunchArtifact,
        _selection: &TargetSelection,
        _cancellation: &Cancellation,
        _deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        Err(failure(PlatformFailureKind::Rejected))
    }
}

struct RejectPostLaunchArtifact {
    path: PathBuf,
    verifications: AtomicUsize,
    reject_at: usize,
}

impl ArtifactVerifier for RejectPostLaunchArtifact {
    fn prepare(
        &self,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<PreparedLaunchArtifact, PlatformFailure> {
        AcceptInstalledArtifact(self.path.clone()).prepare(selection, cancellation, deadline)
    }

    fn assert_snapshot_unchanged(
        &self,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        AcceptInstalledArtifact(self.path.clone()).assert_snapshot_unchanged(
            prepared,
            selection,
            cancellation,
            deadline,
        )
    }

    fn verify_installed(
        &self,
        installed: &Path,
        prepared: &PreparedLaunchArtifact,
        selection: &TargetSelection,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<(), PlatformFailure> {
        if self.verifications.fetch_add(1, Ordering::AcqRel) == self.reject_at {
            Err(failure(PlatformFailureKind::Rejected))
        } else {
            AcceptInstalledArtifact(self.path.clone()).verify_installed(
                installed,
                prepared,
                selection,
                cancellation,
                deadline,
            )
        }
    }
}

struct FakeRunner {
    program: PathBuf,
    app: PathBuf,
    app_id: String,
    requests: Mutex<Vec<RecordedRequest>>,
    listapps_output: Mutex<Option<Vec<u8>>>,
    port: AtomicU16,
    pid: AtomicU32,
    running: AtomicBool,
    ambiguous: AtomicBool,
    installed: AtomicBool,
    identity_token: AtomicU64,
    term_effective: AtomicBool,
    kill_effective: AtomicBool,
    bind_listener: AtomicBool,
    launch_failure: AtomicBool,
    launch_calls: AtomicUsize,
    launch_barriers: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    connect_calls: AtomicUsize,
    accepts: AtomicUsize,
    target_threads: Mutex<Vec<JoinHandle<()>>>,
}

impl FakeRunner {
    fn new(app: &Path, app_id: String) -> Arc<Self> {
        Arc::new(Self {
            program: PathBuf::from("/fake/xcrun"),
            app: fs::canonicalize(app).expect("canonical app"),
            app_id,
            requests: Mutex::new(Vec::new()),
            listapps_output: Mutex::new(None),
            port: AtomicU16::new(0),
            pid: AtomicU32::new(42_424),
            running: AtomicBool::new(false),
            ambiguous: AtomicBool::new(false),
            installed: AtomicBool::new(true),
            identity_token: AtomicU64::new(1),
            term_effective: AtomicBool::new(true),
            kill_effective: AtomicBool::new(true),
            bind_listener: AtomicBool::new(true),
            launch_failure: AtomicBool::new(false),
            launch_calls: AtomicUsize::new(0),
            launch_barriers: Mutex::new(None),
            connect_calls: AtomicUsize::new(0),
            accepts: AtomicUsize::new(1),
            target_threads: Mutex::new(Vec::new()),
        })
    }

    fn process_table(&self) -> Vec<u8> {
        let mut output = b"    1 /sbin/launchd\n".to_vec();
        if self.running.load(Ordering::Acquire) {
            output.extend_from_slice(
                format!(
                    "{} {}/SmokeHost\n",
                    self.pid.load(Ordering::Acquire),
                    self.app.display()
                )
                .as_bytes(),
            );
            if self.ambiguous.load(Ordering::Acquire) {
                output.extend_from_slice(
                    format!("52525 {}/OtherExecutable\n", self.app.display()).as_bytes(),
                );
            }
        }
        output
    }

    fn record(&self, request: &ToolRequest) {
        self.requests
            .lock()
            .expect("requests")
            .push(RecordedRequest {
                args: request.args.clone(),
                descriptor_env: request.descriptor_env.clone(),
                scrub: request.scrub_simctl_child,
            });
    }
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
        self.record(&request);
        let args = request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if args == ["simctl", "help", "list"] {
            return Ok(success_stderr(b"Usage --json devices\n".to_vec()));
        }
        if args == ["simctl", "help", "launch"] {
            return Ok(success_stderr(
                b"SIMCTL_CHILD_ --console <device> <app bundle identifier>\n".to_vec(),
            ));
        }
        if args == ["simctl", "help", "spawn"] {
            return Ok(success_stderr(b"Spawn a process on a device.\n".to_vec()));
        }
        if args == ["simctl", "help", "listapps"] {
            return Ok(success_stderr(
                b"Show the installed applications.\n".to_vec(),
            ));
        }
        if args == ["simctl", "help", "install"] {
            return Ok(success_stderr(b"Install an app on a device.\n".to_vec()));
        }
        if args == ["simctl", "help", "uninstall"] {
            return Ok(success_stderr(
                b"Uninstall an app from a device.\n".to_vec(),
            ));
        }
        if args == ["simctl", "list", "--json", "devices", UDID] {
            return Ok(success(device_json()));
        }
        if args == ["simctl", "listapps", UDID] {
            if let Some(output) = self
                .listapps_output
                .lock()
                .expect("listapps output")
                .clone()
            {
                return Ok(success(output));
            }
            let entries = if self.installed.load(Ordering::Acquire) {
                format!(
                    "    \"{}\" = {{ Bundle = \"{}\"; }};\n",
                    self.app_id, self.app_id
                )
            } else {
                String::new()
            };
            return Ok(success(format!("{{\n{entries}}}\n").into_bytes()));
        }
        if args.len() == 4 && args[..3] == ["simctl", "install", UDID] {
            if self.installed.swap(true, Ordering::AcqRel) {
                panic!("fake install would replace an existing app");
            }
            return Ok(success(Vec::new()));
        }
        if args == ["simctl", "uninstall", UDID, self.app_id.as_str()] {
            if !self.installed.swap(false, Ordering::AcqRel) {
                panic!("fake uninstall without owned installation");
            }
            return Ok(success(Vec::new()));
        }
        if args
            == [
                "simctl".to_owned(),
                "get_app_container".to_owned(),
                UDID.to_owned(),
                self.app_id.clone(),
                "app".to_owned(),
            ]
        {
            return Ok(success(format!("{}\n", self.app.display()).into_bytes()));
        }
        if args == ["simctl", "spawn", UDID, "/bin/ps", "-axo", "pid=,command="] {
            return Ok(success(self.process_table()));
        }
        if args
            == [
                "simctl".to_owned(),
                "launch".to_owned(),
                UDID.to_owned(),
                self.app_id.clone(),
            ]
        {
            self.launch_calls.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = self
                .launch_barriers
                .lock()
                .expect("launch barriers")
                .clone()
            {
                entered.wait();
                release.wait();
            }
            if self.launch_failure.load(Ordering::Acquire) {
                return Ok(ToolOutput {
                    status: ExitStatus::from_raw(1 << 8),
                    stdout: Vec::new(),
                    stderr: b"launch failed\n".to_vec(),
                    oversized: false,
                });
            }
            self.running.store(true, Ordering::Release);
            return Ok(success(
                format!("{}: {}\n", self.app_id, self.pid.load(Ordering::Acquire)).into_bytes(),
            ));
        }
        if args.len() == 6
            && args[..4]
                == [
                    "simctl".to_owned(),
                    "spawn".to_owned(),
                    UDID.to_owned(),
                    "/bin/kill".to_owned(),
                ]
            && matches!(args[4].as_str(), "-TERM" | "-KILL")
            && args[5] == self.pid.load(Ordering::Acquire).to_string()
        {
            let effective = if args[4] == "-TERM" {
                self.term_effective.load(Ordering::Acquire)
            } else {
                self.kill_effective.load(Ordering::Acquire)
            };
            if effective {
                self.running.store(false, Ordering::Release);
                let threads =
                    std::mem::take(&mut *self.target_threads.lock().expect("target threads"));
                for target in threads {
                    target.join().expect("target exit");
                }
            }
            return Ok(success(Vec::new()));
        }
        panic!("unexpected fake request: {args:?}");
    }
}

impl LoopbackConnector for Arc<FakeRunner> {
    fn connect(
        &self,
        address: SocketAddr,
        cancellation: &Cancellation,
        deadline: AbsoluteDeadline,
    ) -> Result<ConnectedLoopback, PlatformFailure> {
        if !self.bind_listener.load(Ordering::Acquire)
            || address.port() != self.port.load(Ordering::Acquire)
            || !address.ip().is_loopback()
        {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        let index = self.connect_calls.fetch_add(1, Ordering::AcqRel);
        if index >= self.accepts.load(Ordering::Acquire) {
            return Err(failure(PlatformFailureKind::Unavailable));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let peer_address = listener
            .local_addr()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        let connection = connect_reserved_loopback(peer_address, cancellation, deadline)?;
        let (mut peer, remote) = listener
            .accept()
            .map_err(|_| failure(PlatformFailureKind::Unavailable))?;
        if !remote.ip().is_loopback()
            || connection.stream.peer_addr().ok() != Some(peer_address)
            || connection.stream.local_addr().ok() != Some(remote)
        {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        let payload: &'static [u8] = if index == 0 { b"bootstrap" } else { b"session" };
        let runner = Arc::clone(self);
        let target = thread::spawn(move || {
            peer.write_all(payload).expect("controlled peer write");
            peer.set_nonblocking(true)
                .expect("controlled peer nonblocking");
            let mut input = [0_u8; 32];
            while runner.running.load(Ordering::Acquire) {
                match peer.read(&mut input) {
                    Ok(_) => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut
                                | io::ErrorKind::WouldBlock
                                | io::ErrorKind::Interrupted
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        self.target_threads
            .lock()
            .expect("target threads")
            .push(target);
        Ok(connection)
    }
}

impl ProcessIdentityProbe for Arc<FakeRunner> {
    fn identity(
        &self,
        pid: u32,
        _installed_app: &Path,
    ) -> Result<ProcessIdentity, PlatformFailure> {
        if pid != self.pid.load(Ordering::Acquire) || !self.running.load(Ordering::Acquire) {
            return Err(failure(PlatformFailureKind::Rejected));
        }
        Ok(ProcessIdentity {
            start_seconds: self.identity_token.load(Ordering::Acquire),
            start_microseconds: 0,
        })
    }
}

fn test_adapter(runner: Arc<FakeRunner>, app: &Path) -> AppleSimulatorAdapter {
    AppleSimulatorAdapter {
        runner: Arc::new(Arc::clone(&runner)),
        artifact_verifier: Arc::new(AcceptInstalledArtifact(app.to_path_buf())),
        process_identity: Arc::new(Arc::clone(&runner)),
        loopback: Arc::new(runner),
    }
}

#[test]
fn exact_pid_launch_raw_io_and_proven_cleanup() {
    let artifact = TempArtifact::new("happy");
    let app_id = format!(
        "com.example.SmokeHost.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.accepts.store(2, Ordering::Release);
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let pending = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0x81; 32]),
        deadline_after(2_000),
    );
    runner.port.store(
        pending.endpoint().ios_port().expect("ios endpoint"),
        Ordering::Release,
    );
    let descriptor_bytes = vec![0, 1, 2, 3, 0xfe, 0xff];
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(
            descriptor_bytes.clone(),
        )),
        Cancellation::new(),
        deadline_after(2_000),
    ));
    let (bootstrap, connector, cleanup) = launched.into_parts();
    let mut chunk = [0_u8; 3];
    let mut received = Vec::new();
    while received.len() < b"bootstrap".len() {
        let count = require(bootstrap.read(&mut chunk, deadline_after(1_000)));
        received.extend_from_slice(&chunk[..count]);
    }
    assert_eq!(received, b"bootstrap");
    assert_eq!(require(bootstrap.write(b"ack", deadline_after(1_000))), 3);
    let session = require(connector.connect(Cancellation::new(), deadline_after(1_000)));
    let mut bytes = [0_u8; 16];
    let count = require(session.read(&mut bytes, deadline_after(1_000)));
    assert_eq!(&bytes[..count], b"session");
    session.cancel();
    require(cleanup.cleanup(Cancellation::new(), deadline_after(1_000)));
    assert!(!runner.running.load(Ordering::Acquire));

    let requests = runner.requests.lock().expect("requests");
    let launch = requests
        .iter()
        .find(|request| request.args.get(1) == Some(&OsString::from("launch")))
        .expect("launch request");
    assert_eq!(
        launch.args,
        ["simctl", "launch", UDID, app_id.as_str()].map(OsString::from)
    );
    assert!(launch.scrub);
    assert_eq!(
        launch.descriptor_env.as_deref(),
        Some(OsStr::new(&URL_SAFE_NO_PAD.encode(descriptor_bytes)))
    );
}

#[test]
fn blocked_launch_for_target_b_does_not_delay_target_a_connector() {
    let artifact = TempArtifact::new("target-shards");
    let app_a = format!(
        "com.example.TargetA.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let app_b = format!(
        "com.example.TargetB.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner_a = FakeRunner::new(&artifact.app, app_a.clone());
    runner_a.accepts.store(2, Ordering::Release);
    let adapter_a = test_adapter(Arc::clone(&runner_a), &artifact.app);
    let pending_a = adapter_a.begin_launch(
        selection(&artifact.app, &app_a, [0; 32]),
        deadline_after(2_000),
    );
    runner_a.port.store(
        pending_a.endpoint().ios_port().expect("target A port"),
        Ordering::Release,
    );
    let launched_a = require(pending_a.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(2_000),
    ));
    let (_bootstrap_a, connector_a, cleanup_a) = launched_a.into_parts();

    let runner_b = FakeRunner::new(&artifact.app, app_b.clone());
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    *runner_b.launch_barriers.lock().expect("launch barriers") =
        Some((Arc::clone(&entered), Arc::clone(&release)));
    let adapter_b = test_adapter(Arc::clone(&runner_b), &artifact.app);
    let pending_b = adapter_b.begin_launch(
        selection(&artifact.app, &app_b, [0; 32]),
        deadline_after(10_000),
    );
    runner_b.port.store(
        pending_b.endpoint().ios_port().expect("target B port"),
        Ordering::Release,
    );

    thread::scope(|scope| {
        let blocked = scope.spawn(|| {
            let launched_b = require(pending_b.launch(
                require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![2])),
                Cancellation::new(),
                deadline_after(10_000),
            ));
            let (_, _, cleanup_b) = launched_b.into_parts();
            require(cleanup_b.cleanup(Cancellation::new(), deadline_after(1_000)));
        });
        entered.wait();
        let started = Instant::now();
        let session_a = require(connector_a.connect(Cancellation::new(), deadline_after(1_000)));
        assert!(started.elapsed() < Duration::from_secs(1));
        {
            let ledger = reservation_ledger().lock().expect("reservation ledger");
            assert!(!ledger.source_ports.is_empty());
            assert!(
                ledger
                    .source_ports
                    .keys()
                    .all(|port| !ledger.ports.contains(port))
            );
        }
        let mut payload = [0_u8; 16];
        let count = require(session_a.read(&mut payload, deadline_after(1_000)));
        assert_eq!(&payload[..count], b"session");
        session_a.cancel();
        release.wait();
        blocked.join().expect("target B launch");
    });
    require(cleanup_a.cleanup(Cancellation::new(), deadline_after(1_000)));
}

#[test]
fn running_or_ambiguous_candidate_never_launches() {
    for ambiguous in [false, true] {
        let artifact = TempArtifact::new("running");
        let app_id = format!(
            "com.example.Running.{}",
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let runner = FakeRunner::new(&artifact.app, app_id.clone());
        runner.running.store(true, Ordering::Release);
        runner.ambiguous.store(ambiguous, Ordering::Release);
        let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
        let error = adapter
            .begin_launch(
                selection(&artifact.app, &app_id, [0; 32]),
                deadline_after(1_000),
            )
            .launch(
                require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
                Cancellation::new(),
                deadline_after(1_000),
            )
            .err()
            .expect("running candidate rejected");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected);
        assert_eq!(runner.launch_calls.load(Ordering::Acquire), 0);
    }
}

#[test]
fn reservation_ledger_blocks_same_target_and_keeps_ports_unique() {
    let artifact = TempArtifact::new("ledger");
    let app_id = format!(
        "com.example.Ledger.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let first = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    let second = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    assert_ne!(first.endpoint().ios_port(), second.endpoint().ios_port());
    let error = second
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("same target rejected");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    require(first.abort(Cancellation::new(), deadline_after(1_000)));

    let ports = Arc::new(Mutex::new(Vec::new()));
    let ready = Arc::new(std::sync::Barrier::new(33));
    let release = Arc::new(std::sync::Barrier::new(33));
    thread::scope(|scope| {
        for index in 0..32 {
            let ports = Arc::clone(&ports);
            let ready = Arc::clone(&ready);
            let release = Arc::clone(&release);
            let artifact = &artifact;
            scope.spawn(move || {
                let app_id = format!("com.example.Parallel.{index}");
                let runner = FakeRunner::new(&artifact.app, app_id.clone());
                let adapter = test_adapter(runner, &artifact.app);
                let pending = adapter.begin_launch(
                    selection(&artifact.app, &app_id, [0; 32]),
                    deadline_after(1_000),
                );
                ports
                    .lock()
                    .expect("ports")
                    .push(pending.endpoint().ios_port().unwrap());
                ready.wait();
                release.wait();
                require(pending.abort(Cancellation::new(), deadline_after(1_000)));
            });
        }
        ready.wait();
        let mut snapshot = ports.lock().expect("ports").clone();
        snapshot.sort_unstable();
        snapshot.dedup();
        assert_eq!(snapshot.len(), 32);
        release.wait();
    });
    let mut ports = ports.lock().expect("ports").clone();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), 32);
}

#[test]
fn system_connector_reserves_its_bound_source_port_before_connect() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("loopback peer");
    let address = listener.local_addr().expect("peer address");
    assert!(address.port() >= 49_152);
    let connection = require(connect_reserved_loopback(
        address,
        &Cancellation::new(),
        deadline_after(1_000),
    ));
    let source = connection
        .stream
        .local_addr()
        .expect("source address")
        .port();
    let (_, remote) = listener.accept().expect("controlled peer accept");
    assert_eq!(remote.port(), source);
    {
        let ledger = reservation_ledger().lock().expect("reservation ledger");
        assert_eq!(ledger.source_ports.get(&source), Some(&1));
        assert!(!ledger.ports.contains(&source));
    }
    drop(connection);
    assert!(
        !reservation_ledger()
            .lock()
            .expect("reservation ledger")
            .source_ports
            .contains_key(&source)
    );
}

#[test]
fn proven_empty_launch_failure_releases_the_target_reservation() {
    let artifact = TempArtifact::new("launch-failure-release");
    let app_id = format!(
        "com.example.LaunchFailureRelease.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    runner.launch_failure.store(true, Ordering::Release);
    let first = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    let first_error = first
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("failed launch is reported");
    assert_eq!(first_error.kind(), PlatformFailureKind::Rejected);

    runner.launch_failure.store(false, Ordering::Release);
    let second = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    runner
        .port
        .store(second.endpoint().ios_port().unwrap(), Ordering::Release);
    let launched = require(second.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(1_000),
    ));
    let (_, _, cleanup) = launched.into_parts();
    require(cleanup.cleanup(Cancellation::new(), deadline_after(1_000)));
    assert_eq!(runner.launch_calls.load(Ordering::Acquire), 2);
}

#[test]
fn cleanup_fails_when_exact_target_survives_kill() {
    let artifact = TempArtifact::new("cleanup-proof");
    let app_id = format!(
        "com.example.Cleanup.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.term_effective.store(false, Ordering::Release);
    runner.kill_effective.store(false, Ordering::Release);
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let pending = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    runner
        .port
        .store(pending.endpoint().ios_port().unwrap(), Ordering::Release);
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(1_000),
    ));
    let (_, _, cleanup) = launched.into_parts();
    let error = cleanup
        .cleanup(Cancellation::new(), deadline_after(30))
        .expect_err("surviving target is cleanup failure");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    runner.running.store(false, Ordering::Release);
}

#[test]
fn cleanup_escalates_only_for_the_same_proven_process_instance() {
    let artifact = TempArtifact::new("cleanup-escalation");
    let app_id = format!(
        "com.example.CleanupEscalation.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.term_effective.store(false, Ordering::Release);
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let pending = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    runner
        .port
        .store(pending.endpoint().ios_port().unwrap(), Ordering::Release);
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(1_000),
    ));
    let (_, _, cleanup) = launched.into_parts();
    require(cleanup.cleanup(Cancellation::new(), deadline_after(1_000)));
    assert!(!runner.running.load(Ordering::Acquire));
    assert!(
        runner
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| { request.args.iter().any(|arg| arg == OsStr::new("-KILL")) })
    );
}

#[test]
fn cleanup_rejects_pid_reuse_without_signalling_the_new_process() {
    let artifact = TempArtifact::new("cleanup-pid-reuse");
    let app_id = format!(
        "com.example.CleanupPidReuse.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let pending = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    runner
        .port
        .store(pending.endpoint().ios_port().unwrap(), Ordering::Release);
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(1_000),
    ));
    runner.identity_token.store(2, Ordering::Release);
    let (_, _, cleanup) = launched.into_parts();
    let error = cleanup
        .cleanup(Cancellation::new(), deadline_after(1_000))
        .expect_err("reused pid is not the owned process instance");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    assert!(runner.running.load(Ordering::Acquire));
    assert!(
        !runner
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| {
                request
                    .args
                    .iter()
                    .any(|arg| arg == OsStr::new("/bin/kill"))
            })
    );
    runner.running.store(false, Ordering::Release);
}

#[test]
fn endpoint_takeover_failure_never_kills_a_possibly_attached_process() {
    let artifact = TempArtifact::new("attach-proof");
    let app_id = format!(
        "com.example.Attach.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.bind_listener.store(false, Ordering::Release);
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let pending = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(100),
    );
    let error = pending
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(100),
        )
        .err()
        .expect("endpoint takeover required");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    assert!(runner.running.load(Ordering::Acquire));
    assert!(
        !runner
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| {
                request
                    .args
                    .iter()
                    .any(|arg| arg == OsStr::new("/bin/kill"))
            })
    );
    runner.running.store(false, Ordering::Release);
}

#[test]
fn private_snapshot_digest_is_exact_and_source_is_never_reused() {
    let artifact = TempArtifact::new("artifact-golden");
    let app_id = "dev.apppilotkit.snapshot";
    let app = simple_bundle(&artifact.root, app_id, true);
    fs::create_dir(app.join("assets")).expect("assets");
    fs::write(app.join("assets/icon.png"), b"original").expect("asset");
    let digest = require(inspect_test_bundle(&app, app_id)).digest;
    let prepared = require(prepare_test_snapshot(&app, app_id, &digest));
    assert_eq!(prepared.identity.digest, digest);
    assert_eq!(prepared.identity.executable, "SmokeHost");
    let canonical = fs::read(prepared.canonical_path()).expect("retained canonical stream");
    assert!(canonical.starts_with(b"APPPILOTKIT-IOS-APP-TREE\0\x01"));
    assert_eq!(<[u8; 32]>::from(sha2::Sha256::digest(&canonical)), digest);
    let snapshot_root = prepared.snapshot_root().to_path_buf();
    fs::write(app.join("assets/icon.png"), b"mutated source").expect("source mutation");
    require(prepared.assert_unchanged(app_id, &Cancellation::new(), deadline_after(5_000)));
    drop(prepared);
    assert!(!snapshot_root.exists());
}

#[test]
fn xml_and_binary_plists_bind_bundle_fields_and_executable() {
    for binary in [false, true] {
        let artifact = TempArtifact::new(if binary { "binary" } else { "xml" });
        let app_id = format!(
            "com.example.Plist.{}",
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let app = simple_bundle(&artifact.root, &app_id, binary);
        let identity = require(inspect_test_bundle(&app, &app_id));
        assert_eq!(identity.executable, "SmokeHost");
        assert_eq!(identity.build, "1");
        assert!(inspect_test_bundle(&app, "com.example.Other").is_err());
    }
}

#[test]
fn excluded_metadata_is_digest_invariant_but_execute_class_is_not() {
    let app_id = "com.example.Metadata";
    let left = TempArtifact::new("metadata-left");
    let right = TempArtifact::new("metadata-right");
    let left_app = simple_bundle(&left.root, app_id, false);
    let right_app = simple_bundle(&right.root, app_id, false);
    fs::write(left_app.join("asset"), b"same").expect("left asset");
    fs::write(right_app.join("asset"), b"same").expect("right asset");
    fs::set_permissions(right_app.join("asset"), fs::Permissions::from_mode(0o600))
        .expect("ignored non-execute mode");
    let right_asset =
        std::ffi::CString::new(right_app.join("asset").as_os_str().as_encoded_bytes())
            .expect("asset path");
    let name = c"com.example.apppilotkit-test";
    let value = b"ignored";
    // SAFETY: pointers reference live test-owned paths and bytes.
    assert_eq!(
        unsafe {
            libc::setxattr(
                right_asset.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        },
        0
    );
    let baseline = require(inspect_test_bundle(&left_app, app_id)).digest;
    assert_eq!(
        require(inspect_test_bundle(&right_app, app_id)).digest,
        baseline
    );
    fs::set_permissions(right_app.join("asset"), fs::Permissions::from_mode(0o700))
        .expect("execute class");
    assert_ne!(
        require(inspect_test_bundle(&right_app, app_id)).digest,
        baseline
    );
}

#[test]
fn snapshot_rejects_links_special_files_resource_forks_and_deep_paths() {
    let app_id = "com.example.HostileTree";
    let hostile = |label: &str| {
        let artifact = TempArtifact::new(label);
        let app = simple_bundle(&artifact.root, app_id, false);
        (artifact, app)
    };

    let (_artifact, app) = hostile("symlink");
    std::os::unix::fs::symlink("SmokeHost", app.join("link")).expect("symlink");
    assert!(prepare_test_snapshot(&app, app_id, &[0; 32]).is_err());

    let (_artifact, app) = hostile("hardlink");
    fs::hard_link(app.join("SmokeHost"), app.join("alias")).expect("hard link");
    assert!(prepare_test_snapshot(&app, app_id, &[0; 32]).is_err());

    let (_artifact, app) = hostile("fifo");
    let fifo =
        std::ffi::CString::new(app.join("fifo").as_os_str().as_encoded_bytes()).expect("fifo path");
    // SAFETY: the path is a unique test-owned child of the temporary bundle.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(prepare_test_snapshot(&app, app_id, &[0; 32]).is_err());

    let (_artifact, app) = hostile("resource-fork");
    let executable = std::ffi::CString::new(app.join("SmokeHost").as_os_str().as_encoded_bytes())
        .expect("executable path");
    let fork_name = c"com.apple.ResourceFork";
    let fork = b"fork";
    // SAFETY: pointers and lengths reference live test-owned bytes.
    assert_eq!(
        unsafe {
            libc::setxattr(
                executable.as_ptr(),
                fork_name.as_ptr(),
                fork.as_ptr().cast(),
                fork.len(),
                0,
                0,
            )
        },
        0
    );
    assert!(prepare_test_snapshot(&app, app_id, &[0; 32]).is_err());

    let (_artifact, app) = hostile("depth");
    let mut directory = app.clone();
    for _ in 0..65 {
        directory.push("d");
        fs::create_dir(&directory).expect("deep directory");
    }
    assert!(prepare_test_snapshot(&app, app_id, &[0; 32]).is_err());
}

#[test]
fn snapshot_rejects_a_symlink_in_the_selected_root_path() {
    let artifact = TempArtifact::new("root-component-symlink");
    let app_id = "com.example.RootComponentSymlink";
    let app = simple_bundle(&artifact.root, app_id, false);
    let link = artifact.root.join("linked-parent");
    std::os::unix::fs::symlink(&artifact.root, &link).expect("parent symlink");
    let selected = link.join(app.file_name().expect("app name"));
    let digest = require(inspect_test_bundle(&app, app_id)).digest;
    assert!(prepare_test_snapshot(&selected, app_id, &digest).is_err());
}

#[test]
fn installed_different_never_installs_or_launches() {
    let artifact = TempArtifact::new("installed-different");
    let app_id = format!(
        "com.example.InstalledDifferent.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    let adapter = AppleSimulatorAdapter {
        runner: Arc::new(Arc::clone(&runner)),
        artifact_verifier: Arc::new(RejectInstalledArtifact(artifact.app.clone())),
        process_identity: Arc::new(Arc::clone(&runner)),
        loopback: Arc::new(Arc::clone(&runner)),
    };
    let error = adapter
        .begin_launch(
            selection(&artifact.app, &app_id, [0; 32]),
            deadline_after(1_000),
        )
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("different installed app fails closed");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    let requests = runner.requests.lock().expect("requests");
    assert_eq!(runner.launch_calls.load(Ordering::Acquire), 0);
    assert!(
        !requests
            .iter()
            .any(|request| request.args.get(1) == Some(&OsString::from("install")))
    );
}

#[test]
fn absent_app_installs_snapshot_and_owned_cleanup_uninstalls_exact_app() {
    let artifact = TempArtifact::new("owned-install");
    let app_id = format!(
        "com.example.OwnedInstall.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.installed.store(false, Ordering::Release);
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let pending = adapter.begin_launch(
        selection(&artifact.app, &app_id, [0; 32]),
        deadline_after(1_000),
    );
    runner
        .port
        .store(pending.endpoint().ios_port().unwrap(), Ordering::Release);
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(1_000),
    ));
    let (_, _, cleanup) = launched.into_parts();
    require(cleanup.cleanup(Cancellation::new(), deadline_after(1_000)));
    assert!(!runner.installed.load(Ordering::Acquire));
    let requests = runner.requests.lock().expect("requests");
    assert!(requests.iter().any(|request| {
        request.args.get(1) == Some(&OsString::from("install"))
            && request.args.get(2) == Some(&OsString::from(UDID))
    }));
    assert!(requests.iter().any(|request| {
        request.args == ["simctl", "uninstall", UDID, app_id.as_str()].map(OsString::from)
    }));
}

#[test]
fn post_install_mismatch_never_launches_or_uninstalls_an_unproven_app() {
    let artifact = TempArtifact::new("post-install-mismatch");
    let app_id = format!(
        "com.example.PostInstallMismatch.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.installed.store(false, Ordering::Release);
    let adapter = AppleSimulatorAdapter {
        runner: Arc::new(Arc::clone(&runner)),
        artifact_verifier: Arc::new(RejectInstalledArtifact(artifact.app.clone())),
        process_identity: Arc::new(Arc::clone(&runner)),
        loopback: Arc::new(Arc::clone(&runner)),
    };
    let error = adapter
        .begin_launch(
            selection(&artifact.app, &app_id, [0; 32]),
            deadline_after(1_000),
        )
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("post-install mismatch is uncertain cleanup");
    assert_eq!(error.kind(), PlatformFailureKind::CleanupFailed);
    assert_eq!(runner.launch_calls.load(Ordering::Acquire), 0);
    let requests = runner.requests.lock().expect("requests");
    assert!(
        requests
            .iter()
            .any(|request| request.args.get(1) == Some(&OsString::from("install")))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.args.get(1) == Some(&OsString::from("uninstall")))
    );
}

#[test]
fn post_launch_artifact_change_is_rejected_and_owned_pid_is_cleaned() {
    for reject_at in [1, 2] {
        let artifact = TempArtifact::new("post-launch-artifact");
        let app_id = format!(
            "com.example.PostLaunchArtifact.{}",
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let runner = FakeRunner::new(&artifact.app, app_id.clone());
        let adapter = AppleSimulatorAdapter {
            runner: Arc::new(Arc::clone(&runner)),
            artifact_verifier: Arc::new(RejectPostLaunchArtifact {
                path: artifact.app.clone(),
                verifications: AtomicUsize::new(0),
                reject_at,
            }),
            process_identity: Arc::new(Arc::clone(&runner)),
            loopback: Arc::new(Arc::clone(&runner)),
        };
        let pending = adapter.begin_launch(
            selection(&artifact.app, &app_id, [0; 32]),
            deadline_after(1_000),
        );
        runner
            .port
            .store(pending.endpoint().ios_port().unwrap(), Ordering::Release);
        let error = pending
            .launch(
                require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
                Cancellation::new(),
                deadline_after(1_000),
            )
            .err()
            .expect("post-launch tree mismatch");
        assert_eq!(error.kind(), PlatformFailureKind::Rejected);
        assert!(!runner.running.load(Ordering::Acquire));
    }
}

#[test]
fn production_tree_chain_accepts_same_installed_digest() {
    let artifact = TempArtifact::new("production-tree");
    let app_id = format!(
        "com.example.ProductionTree.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let app = simple_bundle(&artifact.root, &app_id, true);
    let identity = require(inspect_test_bundle(&app, &app_id));
    let runner = FakeRunner::new(&app, app_id.clone());
    let adapter = AppleSimulatorAdapter {
        runner: Arc::new(Arc::clone(&runner)),
        artifact_verifier: Arc::new(D0ArtifactVerifier),
        process_identity: Arc::new(Arc::clone(&runner)),
        loopback: Arc::new(Arc::clone(&runner)),
    };
    let pending = adapter.begin_launch(
        selection(&app, &app_id, identity.digest),
        deadline_after(2_000),
    );
    runner
        .port
        .store(pending.endpoint().ios_port().unwrap(), Ordering::Release);
    let launched = require(pending.launch(
        require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
        Cancellation::new(),
        deadline_after(2_000),
    ));
    let (_, _, cleanup) = launched.into_parts();
    require(cleanup.cleanup(Cancellation::new(), deadline_after(1_000)));
    assert!(runner.installed.load(Ordering::Acquire));
    let requests = runner.requests.lock().expect("requests");
    assert!(!requests.iter().any(|request| {
        matches!(
            request.args.get(1).and_then(|value| value.to_str()),
            Some("install" | "uninstall")
        )
    }));
}

#[test]
fn parsers_reject_lowercase_udid_and_malformed_tool_output() {
    let lowercase = "e28f8d8e-6211-4287-930b-1c2785d75a37";
    assert!(!is_exact_udid(lowercase));
    assert!(is_exact_udid(UDID));
    let root_selection = require(TargetSelection::new(
        Platform::IosSimulator,
        UDID.to_owned(),
        "com.example.Root".to_owned(),
        "/".to_owned(),
        [0; 32],
    ));
    assert!(validate_selection(&root_selection).is_err());
    let artifact = TempArtifact::new("lowercase");
    let app_id = format!(
        "com.example.Lowercase.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let root_error = adapter
        .begin_launch(root_selection, deadline_after(1_000))
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("filesystem root is not an app artifact");
    assert_eq!(root_error.kind(), PlatformFailureKind::Rejected);
    assert!(runner.requests.lock().expect("requests").is_empty());
    let lower_selection = require(TargetSelection::new(
        Platform::IosSimulator,
        lowercase.to_owned(),
        app_id,
        artifact.app.to_string_lossy().into_owned(),
        [0; 32],
    ));
    let error = adapter
        .begin_launch(lower_selection, deadline_after(1_000))
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("lowercase udid rejected");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    assert!(runner.requests.lock().expect("requests").is_empty());
    assert!(verify_device_json(br#"{"devices":{"a":["#, UDID).is_err());
    let duplicate = format!(
        r#"{{"devices":{{"a":[{{"udid":"{UDID}","state":"Booted","isAvailable":true}}],"b":[{{"udid":"{UDID}","state":"Booted","isAvailable":true}}]}}}}"#
    );
    assert!(verify_device_json(duplicate.as_bytes(), UDID).is_err());
    let app = Path::new("/tmp/SmokeHost.app");
    assert_eq!(
        require(parse_process_table(
            b"  42 /tmp/SmokeHost.app/SmokeHost\n",
            app
        )),
        [42]
    );
    assert!(parse_process_table(b"42missing-space\n", app).is_err());
    assert_eq!(
        require(parse_launch_pid(
            b"com.example.App: 42\n",
            "com.example.App"
        )),
        42
    );
    assert!(parse_launch_pid(b"com.example.App: 42\ntrailing\n", "com.example.App").is_err());
}

#[test]
fn listapps_parser_accepts_recorded_openstep_and_rejects_ambiguous_output() {
    let recorded = include_bytes!("../tests/fixtures/xcode-26.2/listapps.txt");
    assert!(require(parse_listapps_contains(
        recorded,
        "com.apple.Preferences"
    )));
    assert!(!require(parse_listapps_contains(
        recorded,
        "com.example.Absent"
    )));
    assert!(require(parse_listapps_contains(
        br#"{ "com.example.\U0041pp" = { Path = "/tmp/A\"B"; }; }"#,
        "com.example.App"
    )));
    assert!(require(parse_listapps_contains(
        b"{/* before key */ \"com.example.App\" // before equals\n = {/* nested */ Path = /tmp/App;};}// trailing",
        "com.example.App"
    )));

    for malformed in [
        b"{ com.example.App = {}; com.example.App = {}; }".as_slice(),
        b"{ com.other = { foo }; }".as_slice(),
        b"{ com.other = { foo = bar }; }".as_slice(),
        b"{ com.other = (one two); }".as_slice(),
        b"{ com.other = (one,,two); }".as_slice(),
        b"{ com.other = one@two; }".as_slice(),
        b"{ com.other = <0g>; }".as_slice(),
        b"{ com.example.App = {; }".as_slice(),
        b"{ com.example.App = {}; } trailing".as_slice(),
        b"{ \"com.example.App = {}; }".as_slice(),
        b"{ com.example.App = {};".as_slice(),
        b"{ /* unterminated comment".as_slice(),
        b"\xff".as_slice(),
    ] {
        assert!(parse_listapps_contains(malformed, "com.example.App").is_err());
    }
    assert!(parse_listapps_contains(&vec![b' '; TOOL_OUTPUT_CAP + 1], "com.example.App").is_err());
}

#[test]
fn listapps_invalid_openstep_is_rejected_before_install_and_matches_plutil_lint() {
    let artifact = TempArtifact::new("listapps-invalid");
    let app_id = format!(
        "com.example.InvalidListapps.{}",
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    );
    let invalid = [
        b"{ \"com.other\" = { foo }; }".as_slice(),
        b"{ \"com.other\" = { foo = bar } ; }".as_slice(),
        b"{ \"com.other\" = (one two); }".as_slice(),
        b"{ \"com.other\" = (one,,two); }".as_slice(),
        b"{ \"com.other\" = one@two; }".as_slice(),
        b"{ \"com.other\" = <0g>; }".as_slice(),
    ];
    for (index, bytes) in invalid.into_iter().enumerate() {
        let plist = artifact.root.join(format!("invalid-{index}.plist"));
        fs::write(&plist, bytes).expect("invalid plist fixture");
        assert!(
            !Command::new("/usr/bin/plutil")
                .args(["-lint", "--"])
                .arg(&plist)
                .status()
                .expect("plutil lint")
                .success(),
            "fixture {index} must be rejected by system plutil"
        );
        assert!(parse_listapps_contains(bytes, &app_id).is_err());
    }

    let runner = FakeRunner::new(&artifact.app, app_id.clone());
    runner.installed.store(false, Ordering::Release);
    *runner.listapps_output.lock().expect("listapps output") =
        Some(b"{ \"com.other\" = { foo }; }".to_vec());
    let adapter = test_adapter(Arc::clone(&runner), &artifact.app);
    let error = adapter
        .begin_launch(
            selection(&artifact.app, &app_id, [0; 32]),
            deadline_after(1_000),
        )
        .launch(
            require(PublicLaunchDescriptor::from_d2_canonical_bytes(vec![1])),
            Cancellation::new(),
            deadline_after(1_000),
        )
        .err()
        .expect("invalid listapps must fail closed");
    assert_eq!(error.kind(), PlatformFailureKind::Rejected);
    assert!(
        !runner
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| { request.args.get(1).is_some_and(|arg| arg == "install") })
    );
}

#[test]
fn recorded_xcode_26_2_outputs_drive_the_same_strict_parsers() {
    let device = include_bytes!("../tests/fixtures/xcode-26.2/device.json");
    require(verify_device_json(device, UDID));
    let processes = include_bytes!("../tests/fixtures/xcode-26.2/processes.txt");
    let app = Path::new(
        "/Library/Developer/CoreSimulator/Volumes/iOS_23D8133/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS 26.3.simruntime/Contents/Resources/RuntimeRoot/Applications/Preferences.app",
    );
    assert_eq!(require(parse_process_table(processes, app)), [75_916]);
    let launch = include_bytes!("../tests/fixtures/xcode-26.2/launch.txt");
    assert_eq!(
        require(parse_launch_pid(launch, "com.apple.Preferences")),
        75_916
    );
    let observations = include_str!("../tests/fixtures/xcode-26.2/observations.json");
    let value = require(parse_strict_json(observations));
    assert_eq!(value["pgrep"]["exit_code"], 3);
    assert_eq!(value["proxy_kill"]["target_remained_running"], true);
    assert_eq!(value["darwin_process_identity"]["proc_bsdinfo_size"], 136);
    assert_eq!(
        value["darwin_process_identity"]["proc_pidinfo_written"],
        136
    );
    assert!(value["darwin_process_identity"]["start_microseconds"].is_u64());
    assert!(
        value["darwin_process_identity"]["proc_pidpath"]
            .as_str()
            .is_some_and(|path| path.ends_with("/Preferences.app/Preferences"))
    );
    assert_eq!(value["initial_state"], value["restored_state"]);
}

#[test]
fn command_environment_and_process_runner_are_bounded() {
    let request = ToolRequest::launch(
        Path::new("/fake/xcrun"),
        UDID,
        "com.example.App",
        URL_SAFE_NO_PAD.encode(b"public descriptor"),
    );
    assert_eq!(
        request.args,
        ["simctl", "launch", UDID, "com.example.App"].map(OsString::from)
    );
    let command = command_for_with_environment(
        request,
        [
            OsString::from("SIMCTL_CHILD_PROCESS_BOOTSTRAP_SECRET"),
            OsString::from("PATH"),
        ],
    );
    let overrides = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
        .collect::<Vec<_>>();
    assert!(overrides.iter().any(|(key, value)| {
        key == OsStr::new("SIMCTL_CHILD_PROCESS_BOOTSTRAP_SECRET") && value.is_none()
    }));
    assert!(overrides.iter().any(|(key, value)| {
        key == OsStr::new(DESCRIPTOR_ENV)
            && value.as_deref() == Some(OsStr::new(&URL_SAFE_NO_PAD.encode(b"public descriptor")))
    }));

    let runner = ProcessToolRunner {
        xcrun: PathBuf::from("/bin/sh"),
    };
    let oversized = require(runner.run(
        ToolRequest::plain("/bin/sh", ["-c", "printf 123456789"]),
        8,
        &Cancellation::new(),
        deadline_after(1_000),
    ));
    assert!(oversized.oversized);
    let timeout = runner
        .run(
            ToolRequest::plain("/bin/sh", ["-c", "sleep 1"]),
            8,
            &Cancellation::new(),
            deadline_after(20),
        )
        .expect_err("timeout");
    assert!(matches!(
        timeout.kind(),
        PlatformFailureKind::TimedOut | PlatformFailureKind::CleanupFailed
    ));
}

#[test]
#[ignore = "requires an explicitly booted disposable Simulator and probe app"]
fn real_simulator_snapshot_install_pid_and_owned_cleanup() {
    let app = PathBuf::from(std::env::var_os("APPPILOTKIT_REAL_SIM_APP").expect("probe app path"));
    let app_id = std::env::var("APPPILOTKIT_REAL_SIM_APP_ID").expect("probe app id");
    let udid = std::env::var("APPPILOTKIT_REAL_SIM_UDID").expect("exact Simulator UDID");
    assert!(is_exact_udid(&udid));

    let cancellation = Cancellation::new();
    let deadline = deadline_after(30_000);
    let identity = require(artifact::inspect_bundle(
        &app,
        &app_id,
        &cancellation,
        deadline,
    ));
    let prepared = require(artifact::prepare_snapshot(
        &app,
        &app_id,
        &identity.digest,
        &cancellation,
        deadline,
    ));
    let selection = require(TargetSelection::new(
        Platform::IosSimulator,
        udid,
        app_id,
        app.to_string_lossy().into_owned(),
        identity.digest,
    ));
    let runner = ProcessToolRunner {
        xcrun: PathBuf::from("/usr/bin/xcrun"),
    };
    require(feature_probe(&runner, &cancellation, deadline));
    assert!(!require(installed_app_present(
        &runner,
        &selection,
        &cancellation,
        deadline,
    )));
    require(run_success(
        &runner,
        ToolRequest::plain(
            runner.program(),
            [
                OsString::from("simctl"),
                OsString::from("install"),
                OsString::from(selection.device_selector()),
                prepared.app_path().as_os_str().to_owned(),
            ],
        ),
        &cancellation,
        deadline,
    ));
    let installed = require(installed_app_path(
        &runner,
        &selection,
        &cancellation,
        deadline,
    ));
    assert_eq!(
        require(artifact::inspect_bundle(
            &installed,
            selection.app_id(),
            &cancellation,
            deadline,
        )),
        prepared.identity
    );
    assert!(
        require(matching_processes(
            &runner,
            selection.device_selector(),
            &installed,
            &cancellation,
            deadline,
        ))
        .is_empty()
    );
    let output = require(run_success(
        &runner,
        ToolRequest::plain(
            runner.program(),
            [
                "simctl",
                "launch",
                selection.device_selector(),
                selection.app_id(),
            ],
        ),
        &cancellation,
        deadline,
    ));
    let pid = require(parse_launch_pid(&output.stdout, selection.app_id()));
    let owner = require(prove_exact_owner(
        &runner,
        &DarwinProcessIdentityProbe,
        &selection,
        &installed,
        pid,
        &cancellation,
        deadline,
    ));
    require(terminate_exact_owner(
        &runner,
        &DarwinProcessIdentityProbe,
        &owner,
        &cancellation,
        deadline,
    ));
    let verifier = D0ArtifactVerifier;
    let launch_artifact = PreparedLaunchArtifact {
        snapshot_path: prepared.app_path().to_path_buf(),
        digest: prepared.identity.digest,
        executable: prepared.identity.executable.clone(),
        production: Some(prepared),
    };
    require(cleanup_owned_installation(
        &runner,
        &verifier,
        &selection,
        &launch_artifact,
        &LaunchCandidate {
            app_path: installed,
            installed_by_lease: true,
        },
        &cancellation,
        deadline,
    ));
}
