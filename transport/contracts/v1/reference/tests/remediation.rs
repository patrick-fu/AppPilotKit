use std::{path::PathBuf, process::Command};

fn contract_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("reference crate has a contract parent")
        .to_path_buf()
}

fn verify_fixture(name: &str, expected: &str) {
    let fixture = contract_root().join("reference/fixtures").join(name);
    let output = Command::new(env!(
        "CARGO_BIN_EXE_apppilotkit-transport-contract-reference"
    ))
    .args(["verify", "--fixture"])
    .arg(fixture)
    .output()
    .expect("run public verify command");
    assert!(
        output.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("verify stdout is UTF-8");
    assert!(
        stdout.contains(expected),
        "verify did not execute fixture; stdout: {stdout}"
    );
}

#[test]
fn d0_1_rejects_packet_cap_plus_one() {
    verify_fixture(
        "d0-1-packet-cap-plus-one.json",
        "verified fixture d0-1-packet-cap-plus-one: rejected/oversize",
    );
}

#[test]
fn d0_2_rejects_catalog_list_evidence_with_unobservable_value_fields() {
    verify_fixture(
        "d0-2-impossible-catalog-list-evidence.json",
        "verified fixture d0-2-impossible-catalog-list-evidence: rejected/malformed",
    );
}

#[test]
fn d0_3_rejects_client_rewritten_ready_timestamps() {
    verify_fixture(
        "d0-3-client-rewritten-ready-timestamps.json",
        "verified fixture d0-3-client-rewritten-ready-timestamps: rejected/bindingMismatch",
    );
}

#[test]
fn d0_4_rejects_unicode_path_over_utf8_byte_cap() {
    verify_fixture(
        "d0-4-unicode-path-byte-cap.json",
        "verified fixture d0-4-unicode-path-byte-cap: rejected/oversize",
    );
}

#[test]
fn d0_4_round_trips_broker_json_diagnostic_and_cbor_bytes() {
    verify_fixture(
        "d0-4b-broker-json-cbor-roundtrip.json",
        "verified fixture d0-4b-broker-json-cbor-roundtrip: accepted/none",
    );
}

#[test]
fn d0_4_rejects_error_message_over_the_cddl_utf8_byte_cap() {
    verify_fixture(
        "d0-4c-error-message-byte-cap.json",
        "verified fixture d0-4c-error-message-byte-cap: rejected/oversize",
    );
}

#[test]
fn d0_5_classifies_cross_binding_handshake_aead_as_authentication_failed() {
    verify_fixture(
        "d0-5-cross-binding-handshake-aead.json",
        "verified fixture d0-5-cross-binding-handshake-aead: rejected/authenticationFailed",
    );
}

#[test]
fn d0_5_executes_cross_target_m1_against_the_wrong_prologue() {
    verify_fixture(
        "d0-5b-cross-target-wrong-prologue.json",
        "verified fixture d0-5b-cross-target-wrong-prologue: rejected/authenticationFailed",
    );
}

#[test]
fn d0_6a_rejects_duplicate_key_in_raw_deterministic_cbor() {
    verify_fixture(
        "d0-6a-cbor-duplicate-key.json",
        "verified fixture d0-6a-cbor-duplicate-key: rejected/malformed",
    );
}

#[test]
fn d0_6b_rejects_truncated_raw_outer_frame() {
    verify_fixture(
        "d0-6b-outer-frame-truncated.json",
        "verified fixture d0-6b-outer-frame-truncated: rejected/malformed",
    );
}

#[test]
fn d0_6c_rejects_gap_in_raw_record_reassembly() {
    verify_fixture(
        "d0-6c-record-gap.json",
        "verified fixture d0-6c-record-gap: rejected/sequenceViolation",
    );
}

#[test]
fn d0_6d_rejects_second_target_only_redeem() {
    verify_fixture(
        "d0-6d-target-ref-second-redeem.json",
        "verified fixture d0-6d-target-ref-second-redeem: rejected/stale",
    );
}

#[test]
fn d0_6e_scanner_rejects_actual_canary_hit() {
    verify_fixture(
        "d0-6e-secret-surface-canary-hit.json",
        "verified fixture d0-6e-secret-surface-canary-hit: rejected/internalError",
    );
}

#[test]
fn d0_6e_scanner_accepts_only_when_both_canaries_are_absent() {
    verify_fixture(
        "d0-6e2-secret-surface-both-canaries-absent.json",
        "verified fixture d0-6e2-secret-surface-both-canaries-absent: accepted/none",
    );
}

#[test]
fn d0_6f_replays_finished_at_the_expected_nonce() {
    verify_fixture(
        "d0-6f-immediate-finished-replay.json",
        "verified fixture d0-6f-immediate-finished-replay: rejected/authenticationFailed",
    );
}

#[test]
fn d0_7_rejects_incomplete_installed_and_surface_evidence() {
    verify_fixture(
        "d0-7-missing-helper-and-smoke-artifact.json",
        "verified fixture d0-7-missing-helper-and-smoke-artifact: rejected/malformed",
    );
}

#[test]
fn d0_7_rejects_zero_byte_build_artifact_captures() {
    verify_fixture(
        "d0-7b-zero-byte-build-artifacts.json",
        "verified fixture d0-7b-zero-byte-build-artifacts: rejected/malformed",
    );
}
