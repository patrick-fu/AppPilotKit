use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};

fn contract_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("reference crate has a contract parent")
        .to_path_buf()
}

fn verify_fixture(name: &str) {
    let output = Command::new(env!(
        "CARGO_BIN_EXE_apppilotkit-transport-contract-reference"
    ))
    .args(["verify", "--fixture"])
    .arg(contract_root().join("reference/fixtures").join(name))
    .output()
    .expect("run public verify command");
    assert!(output.status.success(), "fixture verification failed");
    assert_eq!(output.stdout, b"verified fixture\n");
}

#[test]
fn public_verify_enforces_retained_machine_session_and_inventory_contracts() {
    let output = Command::new(env!(
        "CARGO_BIN_EXE_apppilotkit-transport-contract-reference"
    ))
    .arg("verify")
    .output()
    .expect("run public verify command");
    assert!(output.status.success(), "contract verification failed");
    assert_eq!(output.stdout, b"verified transport contract v1\n");
    let manifest: Value = serde_json::from_slice(
        &fs::read(contract_root().join("manifest.json")).expect("read root manifest"),
    )
    .expect("parse root manifest");
    let paths = manifest["files"]
        .as_array()
        .expect("root manifest files")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(paths.len(), 50);
    assert!(paths.contains("reference/src/main.rs"));
    assert!(paths.contains("reference/src/verifier.rs"));
    assert!(paths.contains("vectors/manifest.json"));
}

#[test]
fn frozen_session_wire_removes_broker_only_values() {
    let source =
        fs::read_to_string(contract_root().join("wire/session.cddl")).expect("read session CDDL");
    assert!(!source.contains("target-reference-digest"));
    assert!(!source.contains("agent-binding"));
    assert!(source.contains("handoff-state"));
    assert!(source.contains("opaque application"));
}

#[test]
fn lifecycle_vector_pins_the_g0_case_inventory() {
    let vector: Value = serde_json::from_slice(
        &fs::read(contract_root().join("vectors/lifecycle-dispatch.json"))
            .expect("read lifecycle vector"),
    )
    .expect("parse lifecycle vector");
    let ids = vector["vectors"]
        .as_array()
        .expect("lifecycle cases")
        .iter()
        .filter_map(|case| case["id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for required in [
        "prepare-no-lease-launch-bootstrap",
        "prepare-eligible-owned-lease-mints-ref-no-launch-no-bootstrap",
        "prepare-live-conflicting-build-fails-no-relaunch",
        "two-fresh-refs-independent-redemption",
        "two-agent-fresh-noise-and-target-session-ids",
        "concurrent-read-both-complete",
        "close-session-a-session-b-remains-open",
        "session-a-idle-expiry-session-b-remains-open",
        "lease-loss-stales-both",
        "epoch-loss-stales-both",
        "process-loss-stales-both",
        "broker-lost-pre-send-read",
        "broker-lost-partial-invoke",
        "broker-lost-full-before-response-read",
        "broker-lost-full-before-response-invoke",
        "broker-lost-safe-response-lost-invoke",
        "broker-lost-response-partial-eof-invoke",
    ] {
        assert!(
            ids.contains(required),
            "missing frozen lifecycle case {required}"
        );
    }
}

#[test]
fn case_id_cannot_be_rerouted_by_a_fixture_validator_label() {
    verify_fixture("d0-final-validator-reroute.json");
}

#[test]
fn wrong_psk_is_an_executable_handshake_negative() {
    verify_fixture("d0-final-wrong-psk.json");
}

#[test]
fn deterministic_cbor_has_a_nesting_limit() {
    verify_fixture("d0-final-cbor-depth.json");
}

#[test]
fn retained_evidence_is_recomputed_from_capture_bytes() {
    verify_fixture("d0-final-inconsistent-evidence.json");
}

#[test]
fn canary_controls_distinguish_real_hits_from_dishonest_counts() {
    verify_fixture("d0-6e-secret-surface-canary-hit.json");
    verify_fixture("d0-final-dishonest-canary-count.json");
}

#[test]
fn schemas_do_not_require_an_uninstalled_format_plugin() {
    let source = fs::read_to_string(contract_root().join("schema/transport-evidence.schema.json"))
        .expect("read evidence schema");
    assert!(!source.contains("\"format\": \"date-time\""));
    assert!(source.contains("recorded_at"));
}

#[test]
fn root_manifest_pins_the_complete_source_inventory() {
    let manifest: Value = serde_json::from_slice(
        &fs::read(contract_root().join("manifest.json")).expect("read root manifest"),
    )
    .expect("parse root manifest");
    let actual = manifest["files"]
        .as_array()
        .expect("root manifest files")
        .iter()
        .map(|entry| entry["path"].as_str().expect("root manifest path"))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        "../../../docs/adr/0009-private-broker-bootstrap-and-target-transport.md",
        "README.md",
        "dependencies.lock.json",
        "reference/Cargo.lock",
        "reference/Cargo.toml",
        "reference/fixtures/d0-1-packet-cap-plus-one.json",
        "reference/fixtures/d0-2-impossible-catalog-list-evidence.json",
        "reference/fixtures/d0-3-client-rewritten-ready-timestamps.json",
        "reference/fixtures/d0-4-unicode-path-byte-cap.json",
        "reference/fixtures/d0-4b-broker-json-cbor-roundtrip.json",
        "reference/fixtures/d0-4c-error-message-byte-cap.json",
        "reference/fixtures/d0-5-cross-binding-handshake-aead.json",
        "reference/fixtures/d0-5b-cross-target-wrong-prologue.json",
        "reference/fixtures/d0-6a-cbor-duplicate-key.json",
        "reference/fixtures/d0-6b-outer-frame-truncated.json",
        "reference/fixtures/d0-6c-record-gap.json",
        "reference/fixtures/d0-6d-target-ref-second-redeem.json",
        "reference/fixtures/d0-6e-secret-surface-canary-hit.json",
        "reference/fixtures/d0-6e2-secret-surface-both-canaries-absent.json",
        "reference/fixtures/d0-6f-immediate-finished-replay.json",
        "reference/fixtures/d0-7-missing-helper-and-smoke-artifact.json",
        "reference/fixtures/d0-7b-zero-byte-build-artifacts.json",
        "reference/fixtures/d0-final-cbor-depth.json",
        "reference/fixtures/d0-final-dishonest-canary-count.json",
        "reference/fixtures/d0-final-inconsistent-evidence.json",
        "reference/fixtures/d0-final-validator-reroute.json",
        "reference/fixtures/d0-final-wrong-psk.json",
        "reference/src/main.rs",
        "reference/src/verifier.rs",
        "reference/tests/d0_four_p1_remediation.rs",
        "reference/tests/final_remediation.rs",
        "reference/tests/ios_app_artifact_tree.rs",
        "reference/tests/remediation.rs",
        "schema/broker-control.schema.json",
        "schema/target-prepare.schema.json",
        "schema/target-ready.schema.json",
        "schema/transport-evidence.schema.json",
        "vectors/binding-replay-failures.json",
        "vectors/bootstrap-android-descriptor.json",
        "vectors/bootstrap-nk-success.json",
        "vectors/broker-ipc-boundaries.json",
        "vectors/frame-failures.json",
        "vectors/ios-app-artifact-tree.json",
        "vectors/lifecycle-dispatch.json",
        "vectors/manifest.json",
        "vectors/secret-surface-canaries.json",
        "vectors/session-nnpsk0-success.json",
        "wire/bootstrap.cddl",
        "wire/broker-ipc.cddl",
        "wire/session.cddl",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected);
}
