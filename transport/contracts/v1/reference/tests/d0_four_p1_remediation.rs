use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const PROCESS_GENERATION: u64 = 4_503_599_627_370_123;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn contract_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("reference crate has a contract parent")
        .to_path_buf()
}

fn run_evidence(path: &Path) -> std::process::Output {
    Command::new(env!(
        "CARGO_BIN_EXE_apppilotkit-transport-contract-reference"
    ))
    .args(["verify", "--evidence"])
    .arg(path)
    .output()
    .expect("run public evidence verifier")
}

fn run_fixture(path: &Path) -> std::process::Output {
    Command::new(env!(
        "CARGO_BIN_EXE_apppilotkit-transport-contract-reference"
    ))
    .args(["verify", "--fixture"])
    .arg(path)
    .output()
    .expect("run public fixture verifier")
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn capture(identity: &str, bytes: &[u8]) -> Value {
    json!({
        "identity": identity,
        "path": format!("/tmp/apppilotkit-evidence/{identity}.capture"),
        "sha256": digest(bytes),
        "byte_count": bytes.len(),
        "bytes_base64url": URL_SAFE_NO_PAD.encode(bytes)
    })
}

fn ios_app_tree(build: &str, executable_bytes: &[u8]) -> Vec<u8> {
    let info = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>dev.apppilotkit.smoke</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleVersion</key><string>{build}</string><key>CFBundleExecutable</key><string>SmokeHost</string></dict></plist>"
    )
    .into_bytes();
    let records = [
        (b"Info.plist".as_slice(), 0_u8, info.as_slice()),
        (b"SmokeHost".as_slice(), 1_u8, executable_bytes),
    ];
    let mut stream = b"APPPILOTKIT-IOS-APP-TREE\0\x01".to_vec();
    stream.extend_from_slice(&(records.len() as u32).to_be_bytes());
    for (path, executable_class, file) in records {
        stream.push(2);
        stream.extend_from_slice(&(path.len() as u32).to_be_bytes());
        stream.extend_from_slice(path);
        stream.push(executable_class);
        stream.extend_from_slice(&(file.len() as u64).to_be_bytes());
        stream.extend_from_slice(file);
    }
    stream
}

fn session(id: &[u8], handshake: &str, request: &[u8], response: &[u8], runtime: &[u8]) -> Value {
    let id_digest = digest(id);
    json!({
        "id_digest": id_digest,
        "noise_handshake_hash_hex": handshake,
        "target_issued": true,
        "request": {"session_id_digest": id_digest, "sha256": digest(request)},
        "response": {"session_id_digest": id_digest, "sha256": digest(response)},
        "runtime": {
            "session_id_digest": id_digest,
            "instance_digest": digest(runtime),
            "request_sha256": digest(request),
            "response_sha256": digest(response)
        }
    })
}

fn valid_evidence() -> Value {
    let smoke_app = ios_app_tree("test-build", b"smoke-app");
    let production_app = ios_app_tree("production-build", b"production-app");
    let release_app = ios_app_tree("release-build", b"release-app");
    let redacted_command_argv = json!([
        "/tmp/apppilotkit-prefix/bin/apppilotkit",
        "catalog",
        "list",
        "--target=<redacted>",
        "--output=json",
        "--non-interactive"
    ]);
    let redacted_next_argv = json!([
        "/tmp/apppilotkit-prefix/bin/apppilotkit",
        "catalog",
        "show",
        "--capability",
        "smoke.ready",
        "--declaration-revision",
        "1",
        "--session=<redacted>",
        "--target=<redacted>",
        "--output",
        "json",
        "--non-interactive"
    ]);
    let machine = json!({
        "schema_version": "1.0", "cli_version": "0.1.0", "status": "succeeded",
        "command": ["catalog", "list"], "side_effect": "read_only", "retry_safety": "safe",
        "data": {"catalog": {"id": "catalog_smoke_01234567", "generation": PROCESS_GENERATION},
            "capabilities": [{"id": "smoke.ready", "kind": "resource", "declaration_revision": 1}]},
        "disclosure": {"truncated": false, "returned_items": 1}, "artifacts": [],
        "next_actions": [{"id": "catalog.show", "argv": redacted_next_argv,
            "side_effect": "read_only", "retry_safety": "safe",
            "preconditions": ["session is still valid"],
            "reason": "Inspect the first Semantic Capability using the same Target-issued Session"}]
    });
    let mut stdout = serde_json::to_vec(&machine).expect("encode Machine Result");
    stdout.push(b'\n');
    let next_actions = serde_json::to_vec(&machine["next_actions"]).expect("encode Next Actions");
    let argv = serde_json::to_vec(&redacted_command_argv).expect("encode argv");
    let executables = [
        (
            "apppilotkit",
            "/tmp/apppilotkit-prefix/bin/apppilotkit",
            b"cli-binary".as_slice(),
        ),
        (
            "apppilotkit-broker",
            "/tmp/apppilotkit-prefix/libexec/apppilotkit-broker",
            b"broker-binary".as_slice(),
        ),
        (
            "apppilotkit-target-prepare",
            "/tmp/apppilotkit-prefix/libexec/apppilotkit-target-prepare",
            b"prepare-binary".as_slice(),
        ),
    ];
    let mut package_bytes = Vec::new();
    for (name, _, bytes) in executables {
        package_bytes.extend_from_slice(name.as_bytes());
        package_bytes.push(0);
        package_bytes.extend_from_slice(digest(bytes).as_bytes());
        package_bytes.push(b'\n');
    }
    let installed = executables.map(|(name, path, bytes)| {
        json!({
            "name": name, "path": path, "version": "0.1.0", "sha256": digest(bytes),
            "bytes_base64url": URL_SAFE_NO_PAD.encode(bytes),
            "signature": "unsigned-local-checkpoint", "build": "test-build", "arch": "arm64"
        })
    });
    let surfaces = [
        "argv",
        "environment",
        "activity_extras",
        "stdout",
        "stderr",
        "product_logs",
        "diagnostics",
        "machine_result",
        "next_actions",
        "artifacts",
        "smoke_host_build_artifact",
        "production_build_artifact",
        "release_build_artifact",
    ]
    .map(|name| {
        let bytes = match name {
            "stdout" | "machine_result" => stdout.clone(),
            "stderr" => Vec::new(),
            "next_actions" => next_actions.clone(),
            "argv" => argv.clone(),
            "smoke_host_build_artifact" => smoke_app.clone(),
            "production_build_artifact" => production_app.clone(),
            "release_build_artifact" => release_app.clone(),
            _ => format!("capture:{name}").into_bytes(),
        };
        let mut surface = json!({
            "name": name, "scanner": "apppilotkit-reference-byte-scanner",
            "scanner_version": "1.0", "operation": "literal-byte-subsequence-count",
            "capture": capture(&format!("capture:{name}"), &bytes),
            "fixed_canary_match_count": 0, "execution_canary_match_count": 0, "complete": true
        });
        if name.ends_with("_build_artifact") {
            let build = match name {
                "smoke_host_build_artifact" => "test-build",
                "production_build_artifact" => "production-build",
                "release_build_artifact" => "release-build",
                _ => unreachable!(),
            };
            let configuration = match name {
                "smoke_host_build_artifact" => "debug_internal",
                "production_build_artifact" => "production",
                "release_build_artifact" => "release",
                _ => unreachable!(),
            };
            surface["artifact_identity"] = json!({
                "app_id": "dev.apppilotkit.smoke", "build": build, "configuration": configuration,
                "artifact_encoding": "ios-app-tree-v1",
                "artifact_sha256": digest(&bytes)
            });
        }
        surface
    });
    let primary = session(
        b"session-id",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        b"session-a-request",
        b"session-a-response",
        b"session-a-runtime",
    );
    let secondary = session(
        b"session-id-b",
        "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        b"session-b-request",
        b"session-b-response",
        b"session-b-runtime",
    );
    let fixed_canary = b"APPPILOTKIT_TEST_ONLY_SECRET_CANARY_7f9c4b2e";
    let execution_canary = b"APPPILOTKIT_EXECUTION_CANARY_0123456789abcdef";
    json!({
        "schema_version": "1.0", "base_commit": "09c846d86d0a18b0ccc6ca2e3fc6f00c305425b3",
        "recorded_at": "2026-08-30T00:00:00Z", "platform": "ios-simulator",
        "form_factor": "phone", "os_version": "iOS 26.3",
        "host": {"os": "macOS 26.6.1", "arch": "arm64"},
        "tool": {"name": "simctl", "version": "Xcode 26.2"},
        "app": {"id": "dev.apppilotkit.smoke", "build": "test-build",
            "artifact_encoding": "ios-app-tree-v1", "artifact_sha256": digest(&smoke_app),
            "artifact_bytes_base64url": URL_SAFE_NO_PAD.encode(&smoke_app), "release_excluded": true},
        "installed": {"prefix": "/tmp/apppilotkit-prefix", "package_sha256": digest(&package_bytes),
            "executables": installed},
        "broker": {"pid": 123, "euid": 501, "start_mode": "on_demand_current_user",
            "runtime_dir_mode": "0700", "socket_mode": "0600", "peer_euid_verified": true},
        "target": {"reference_digest": digest(b"target-ref"), "transport": "ios_simulator_loopback_nk",
            "lease_digest": digest(b"lease"), "process_generation": PROCESS_GENERATION, "listener_epoch": 1},
        "session": primary, "concurrent_sessions": [primary, secondary],
        "session_isolation": {"fresh_handshakes": true, "fresh_session_ids": true,
            "close_a_b_remained_open": true, "idle_a_b_remained_open": true,
            "auth_a_b_remained_open": true, "lease_loss_staled_both": true},
        "protocol": {"major": 1, "minor": 2, "capabilities": ["semantic.catalog", "session.core"],
            "max_request_bytes": 16777216, "max_response_bytes": 67108864, "max_page_items": 10000},
        "command": {"redacted_argv": redacted_command_argv,
            "retained_stdout": capture("terminal:stdout", &stdout),
            "stdout_redactions": [
                {"json_pointer": "/next_actions/0/argv/7", "original_sha256": digest(b"session-id")},
                {"json_pointer": "/next_actions/0/argv/8", "original_sha256": digest(b"target-ref")}],
            "stderr_sha256": digest(b""), "exit_status": 0},
        "terminal": {"status": "succeeded", "machine_result_sha256": digest(&stdout),
            "catalog": {"id": "catalog_smoke_01234567", "generation": PROCESS_GENERATION},
            "smoke_ready_declaration": {"id": "smoke.ready", "kind": "resource", "declaration_revision": 1},
            "next_action": {"kind": "catalog.show", "target_reference_digest": digest(b"target-ref"),
                "session_id_digest": digest(b"session-id"), "capability": "smoke.ready", "declaration_revision": 1,
                "redacted_argv": ["/tmp/apppilotkit-prefix/bin/apppilotkit", "catalog", "show",
                    "--capability", "smoke.ready", "--declaration-revision", "1",
                    "--session=<redacted>", "--target=<redacted>", "--output", "json", "--non-interactive"]},
            "transport_handoff": "handoff_possible_or_confirmed"},
        "cleanup": {"status": "complete", "owned_resources_remaining": 0, "duration_ms": 100},
        "secret_surface": {"fixed_canary_digest": digest(fixed_canary),
            "fixed_canary_base64url": URL_SAFE_NO_PAD.encode(fixed_canary),
            "execution_canary_digest": digest(execution_canary),
            "execution_canary_base64url": URL_SAFE_NO_PAD.encode(execution_canary),
            "surfaces": surfaces, "complete": true},
        "evidence_class": "real_installed_smoke_host_journey"
    })
}

fn write_temp_json(label: &str, value: &Value) -> PathBuf {
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "apppilotkit-d0-{label}-{}-{id}.json",
        std::process::id()
    ));
    fs::write(&path, serde_json::to_vec(value).expect("encode evidence"))
        .expect("write temporary evidence");
    path
}

fn write_temp_bytes(label: &str, bytes: &[u8]) -> PathBuf {
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "apppilotkit-d0-{label}-{}-{id}.json",
        std::process::id()
    ));
    fs::write(&path, bytes).expect("write temporary evidence bytes");
    path
}

fn replace_retained_stdout(evidence: &mut Value, stdout: &[u8]) {
    let encoded = Value::String(URL_SAFE_NO_PAD.encode(stdout));
    let sha256 = Value::String(digest(stdout));
    for pointer in [
        "/command/retained_stdout",
        "/secret_surface/surfaces/3/capture",
        "/secret_surface/surfaces/7/capture",
    ] {
        let capture = evidence
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("missing capture at {pointer}"));
        capture["bytes_base64url"] = encoded.clone();
        capture["byte_count"] = Value::from(stdout.len());
        capture["sha256"] = sha256.clone();
    }
    evidence["terminal"]["machine_result_sha256"] = sha256;
}

#[test]
fn public_verify_accepts_a_valid_retained_evidence_file() {
    let path = write_temp_json("valid-evidence", &valid_evidence());
    let output = run_evidence(&path);
    fs::remove_file(path).expect("remove temporary evidence");
    assert!(
        output.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("verify stdout is UTF-8"),
        "verified retained transport evidence v1\n"
    );
}

#[test]
fn public_verify_rejects_duplicate_members_in_raw_evidence_at_every_depth() {
    let baseline = serde_json::to_string(&valid_evidence()).expect("encode evidence");
    let cases = [
        (
            "same-top-level",
            baseline.replacen('{', r#"{"schema_version":"1.0","#, 1),
        ),
        (
            "conflicting-top-level",
            baseline.replacen('{', r#"{"schema_version":"9.9","#, 1),
        ),
        (
            "escaped-equivalent-top-level",
            baseline.replacen('{', r#"{"\u0073chema_version":"1.0","#, 1),
        ),
        (
            "same-nested",
            baseline.replacen(r#""host":{"#, r#""host":{"arch":"arm64","#, 1),
        ),
        (
            "conflicting-nested",
            baseline.replacen(r#""host":{"#, r#""host":{"arch":"x86_64","#, 1),
        ),
    ];
    for (label, raw) in cases {
        let path = write_temp_bytes(label, raw.as_bytes());
        let output = run_evidence(&path);
        fs::remove_file(path).expect("remove temporary evidence");
        assert!(
            !output.status.success(),
            "public verifier accepted duplicate evidence member {label}"
        );
    }
}

#[test]
fn public_verify_rejects_duplicate_members_in_retained_machine_result_at_every_depth() {
    let evidence = valid_evidence();
    let encoded = evidence["command"]["retained_stdout"]["bytes_base64url"]
        .as_str()
        .expect("retained stdout bytes");
    let baseline = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).expect("decode stdout"))
        .expect("stdout is UTF-8");
    let cases = [
        (
            "machine-same-top-level",
            baseline.replacen('{', r#"{"schema_version":"1.0","#, 1),
        ),
        (
            "machine-conflicting-top-level",
            baseline.replacen('{', r#"{"schema_version":"9.9","#, 1),
        ),
        (
            "machine-same-nested",
            baseline.replacen(
                r#""disclosure":{"#,
                r#""disclosure":{"truncated":false,"#,
                1,
            ),
        ),
        (
            "machine-conflicting-nested",
            baseline.replacen(r#""disclosure":{"#, r#""disclosure":{"truncated":true,"#, 1),
        ),
    ];
    for (label, stdout) in cases {
        let mut hostile = evidence.clone();
        replace_retained_stdout(&mut hostile, stdout.as_bytes());
        let path = write_temp_json(label, &hostile);
        let output = run_evidence(&path);
        fs::remove_file(path).expect("remove temporary evidence");
        assert!(
            !output.status.success(),
            "public verifier accepted duplicate retained Machine Result member {label}"
        );
    }
}

#[test]
fn public_verify_rejects_a_decoy_smoke_host_build_capture() {
    let mut evidence = valid_evidence();
    let decoy = b"unrelated-decoy-build-artifact";
    let surface = evidence["secret_surface"]["surfaces"]
        .as_array_mut()
        .expect("scan surfaces")
        .iter_mut()
        .find(|surface| surface["name"] == "smoke_host_build_artifact")
        .expect("Smoke Host surface");
    surface["capture"] = capture("decoy-smoke-host", decoy);
    surface["artifact_identity"] = json!({
        "app_id": "dev.apppilotkit.smoke", "build": "decoy-build", "configuration": "debug_internal",
        "artifact_sha256": digest(decoy)
    });
    let path = write_temp_json("decoy-smoke-host", &evidence);
    let output = run_evidence(&path);
    fs::remove_file(path).expect("remove temporary evidence");
    assert!(
        !output.status.success(),
        "public verifier accepted a Smoke Host capture unrelated to app artifact bytes"
    );
}

fn build_surface_mut<'a>(evidence: &'a mut Value, name: &str) -> &'a mut Value {
    evidence["secret_surface"]["surfaces"]
        .as_array_mut()
        .expect("scan surfaces")
        .iter_mut()
        .find(|surface| surface["name"] == name)
        .unwrap_or_else(|| panic!("missing build surface {name}"))
}

#[test]
fn public_verify_rejects_exchanged_zero_and_missing_build_captures() {
    let mut cases = Vec::new();

    let mut exchanged_capture = valid_evidence();
    let release_capture =
        build_surface_mut(&mut exchanged_capture, "release_build_artifact")["capture"].clone();
    build_surface_mut(&mut exchanged_capture, "production_build_artifact")["capture"] =
        release_capture;
    cases.push(("exchanged-build-capture", exchanged_capture));

    let mut exchanged_identity = valid_evidence();
    let release = build_surface_mut(&mut exchanged_identity, "release_build_artifact").clone();
    let production = build_surface_mut(&mut exchanged_identity, "production_build_artifact");
    production["capture"] = release["capture"].clone();
    production["artifact_identity"] = release["artifact_identity"].clone();
    cases.push(("exchanged-build-identity", exchanged_identity));

    let mut zero = valid_evidence();
    let surface = build_surface_mut(&mut zero, "release_build_artifact");
    surface["capture"] = capture("zero-release-build", b"");
    surface["artifact_identity"]["artifact_sha256"] = Value::String(digest(b""));
    cases.push(("zero-build-capture", zero));

    let mut missing = valid_evidence();
    build_surface_mut(&mut missing, "production_build_artifact")
        .as_object_mut()
        .expect("build surface object")
        .remove("capture");
    cases.push(("missing-build-capture", missing));

    for (label, evidence) in cases {
        let path = write_temp_json(label, &evidence);
        let output = run_evidence(&path);
        fs::remove_file(path).expect("remove temporary evidence");
        assert!(
            !output.status.success(),
            "public verifier accepted hostile build evidence {label}"
        );
    }
}

#[test]
fn public_verify_rejects_smoke_host_bytes_rebranded_as_production_or_release() {
    for name in ["production_build_artifact", "release_build_artifact"] {
        let mut evidence = valid_evidence();
        let smoke_bytes = URL_SAFE_NO_PAD
            .decode(
                evidence["app"]["artifact_bytes_base64url"]
                    .as_str()
                    .expect("app artifact bytes"),
            )
            .expect("decode app artifact");
        let surface = build_surface_mut(&mut evidence, name);
        surface["capture"] = capture(&format!("rebranded-{name}"), &smoke_bytes);
        surface["artifact_identity"]["artifact_sha256"] = Value::String(digest(&smoke_bytes));
        let path = write_temp_json(&format!("rebranded-{name}"), &evidence);
        let output = run_evidence(&path);
        fs::remove_file(path).expect("remove temporary evidence");
        assert!(
            !output.status.success(),
            "public verifier accepted Smoke Host bytes relabeled as {name}"
        );
    }
}

#[test]
fn frame_failure_vectors_freeze_and_verify_the_session_expired_mapping() {
    let vector: Value = serde_json::from_slice(
        &fs::read(contract_root().join("vectors/frame-failures.json"))
            .expect("read frame failure vectors"),
    )
    .expect("parse frame failure vectors");
    let affected = vector["vectors"]
        .as_array()
        .expect("frame vectors")
        .iter()
        .filter(|case| {
            matches!(
                case["expected_close_reason"].as_str(),
                Some("malformed") | Some("sequenceViolation")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(affected.len(), 17);
    for case in &affected {
        assert_eq!(
            case["expected_error_kind"], "sessionExpired",
            "{} lacks the frozen public error mapping",
            case["id"]
        );
    }

    let source = affected
        .iter()
        .find(|case| case["id"] == "record-gap")
        .expect("record-gap vector");
    let mut wrong = json!({
        "id": source["id"],
        "validator": source["validator"],
        "input": source["canonical_input"],
        "expected_result": source["expected_result"],
        "expected_close_reason": source["expected_close_reason"],
        "expected_error_kind": "invalidRequest"
    });
    let wrong_path = write_temp_json("wrong-frame-error-kind", &wrong);
    let output = run_fixture(&wrong_path);
    fs::remove_file(wrong_path).expect("remove temporary fixture");
    assert!(
        !output.status.success(),
        "independent verifier ignored a wrong frame error mapping"
    );

    wrong["expected_error_kind"] = Value::String("sessionExpired".to_owned());
    let correct_path = write_temp_json("correct-frame-error-kind", &wrong);
    let output = run_fixture(&correct_path);
    fs::remove_file(correct_path).expect("remove temporary fixture");
    assert!(
        output.status.success(),
        "independent verifier rejected the frozen frame mapping: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
