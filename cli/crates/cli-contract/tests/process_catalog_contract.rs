#![cfg(unix)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE_BINARY: &str = env!("CARGO_BIN_EXE_apppilotkit-cli-contract-fixture");
const SESSION: &str = "session_0123456789abcdef";
const DIGEST: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);

#[test]
fn installed_process_live_catalog_matrix_crosses_socket_parser_stdin_and_renderer() {
    let commands = vec![
        ("semantic.list", vec!["catalog", "list"]),
        (
            "semantic.show",
            vec![
                "catalog",
                "show",
                "--capability",
                "account.delete",
                "--declaration-revision",
                "3",
            ],
        ),
        (
            "semantic.schema",
            vec![
                "catalog",
                "schema",
                "--capability",
                "config.current",
                "--declaration-revision",
                "1",
                "--schema-id",
                "schema_value0001",
                "--schema-revision",
                "1",
                "--schema-digest",
                DIGEST,
            ],
        ),
        (
            "semantic.query",
            vec![
                "catalog",
                "query",
                "--capability",
                "config.current",
                "--declaration-revision",
                "1",
                "--value-schema-id",
                "schema_value0001",
                "--value-schema-revision",
                "1",
                "--value-schema-digest",
                DIGEST,
            ],
        ),
        (
            "semantic.invoke",
            vec![
                "catalog",
                "invoke",
                "--capability",
                "account.delete",
                "--declaration-revision",
                "3",
                "--input-schema-id",
                "schema_action0001",
                "--input-schema-revision",
                "1",
                "--input-schema-digest",
                DIGEST,
                "--input",
                "{}",
            ],
        ),
    ];

    for (method, base_arguments) in commands {
        for output_mode in ["human", "json", "jsonl"] {
            let mut arguments = base_arguments.clone();
            arguments.extend(["--output", output_mode]);
            let (output, request) = run_with_target(
                &arguments,
                semantic_session(2, true, 16 * 1024),
                success_response,
            );
            assert_eq!(
                output.status.code(),
                Some(0),
                "{method} {output_mode}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty(), "{method} {output_mode}");
            assert!(!output.stdout.is_empty(), "{method} {output_mode}");
            let request = request.expect("live command exchanges with the fixture Target");
            assert_eq!(request["method"], method);
            match output_mode {
                "human" => assert!(!output.stdout.starts_with(b"{")),
                "json" => {
                    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(result["status"], "succeeded");
                }
                "jsonl" => {
                    let events = jsonl_events(&output);
                    assert_eq!(events.len(), 2);
                    assert_eq!(events[0]["type"], "run.started");
                    assert_eq!(events[1]["type"], "run.succeeded");
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn installed_process_negotiation_failures_never_exchange() {
    for (session, expected) in [
        (semantic_session(1, true, 16 * 1024), "incompatibleProtocol"),
        (
            semantic_session_version(1, 3, true, 16 * 1024),
            "incompatibleProtocol",
        ),
        (
            semantic_session_version(2, 2, true, 16 * 1024),
            "incompatibleProtocol",
        ),
        (
            semantic_session(2, false, 16 * 1024),
            "capabilityUnavailable",
        ),
    ] {
        let (output, request) =
            run_with_target(&["catalog", "list", "--output", "json"], session, |_| {
                panic!("negotiation failure must not exchange")
            });
        assert_eq!(output.status.code(), Some(6));
        assert!(output.stderr.is_empty());
        let result = machine_result(&output);
        assert_eq!(result["error"]["kind"], expected);
        assert!(request.is_none());
    }
}

#[test]
fn installed_process_projects_protocol_disclosure_without_renderer_panics() {
    for output_mode in ["human", "json", "jsonl"] {
        let (output, request) = run_with_target(
            &["catalog", "list", "--output", output_mode],
            semantic_session(2, true, 16 * 1024),
            |request| {
                Some(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "catalog": {"id": "catalog_12345678", "generation": 7},
                            "capabilities": [],
                            "page": {
                                "truncated": true,
                                "returnedItems": 0,
                                "appliedLimits": {"maxItems": 2, "maxBytes": 4096},
                                "reasons": ["providerDeadline", "providerLimit"],
                                "nextCursor": "cursor_provider_resume"
                            }
                        }
                    }))
                    .unwrap(),
                )
            },
        );
        assert_eq!(output.status.code(), Some(0), "{output_mode}");
        assert!(output.stderr.is_empty(), "{output_mode}");
        assert_eq!(request.unwrap()["method"], "semantic.list");
        match output_mode {
            "human" => assert!(!output.stdout.is_empty()),
            "json" => {
                let result = machine_result(&output);
                assert_eq!(
                    result["disclosure"]["reasons"],
                    serde_json::json!(["provider_deadline", "provider_limit"])
                );
                assert_eq!(result["disclosure"]["applied_limits"]["max_items"], 2);
            }
            "jsonl" => {
                let events = jsonl_events(&output);
                assert_eq!(
                    events[1]["result"]["disclosure"]["reasons"],
                    serde_json::json!(["provider_deadline", "provider_limit"])
                );
            }
            _ => unreachable!(),
        }
    }

    let (complete, _) = run_with_target(
        &["catalog", "list", "--output", "json"],
        semantic_session(2, true, 16 * 1024),
        success_response,
    );
    assert_eq!(complete.status.code(), Some(0));
    let result = machine_result(&complete);
    assert_eq!(result["disclosure"]["truncated"], false);
    assert_eq!(result["disclosure"]["applied_limits"]["max_items"], 2);
    assert_eq!(result["disclosure"]["applied_limits"]["max_bytes"], 4096);
    assert!(result["disclosure"].get("reasons").is_none());
    assert!(result["disclosure"].get("next_cursor").is_none());

    let (unknown, _) = run_with_target(
        &["catalog", "list", "--output", "json"],
        semantic_session(2, true, 16 * 1024),
        |request| {
            Some(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": {
                        "catalog": {"id": "catalog_12345678", "generation": 7},
                        "capabilities": [],
                        "page": {
                            "truncated": true,
                            "returnedItems": 0,
                            "appliedLimits": {"maxItems": 2, "maxBytes": 4096},
                            "reasons": ["peer-secret-reason"],
                            "nextCursor": "cursor_resume"
                        }
                    }
                }))
                .unwrap(),
            )
        },
    );
    assert_eq!(unknown.status.code(), Some(6));
    assert_eq!(machine_result(&unknown)["error"]["kind"], "invalidRequest");
    assert!(!String::from_utf8_lossy(&unknown.stdout).contains("peer-secret-reason"));
}

#[test]
fn installed_process_rejects_action_only_error_for_read_method() {
    let (output, request) = run_with_target(
        &["catalog", "list", "--output", "json"],
        semantic_session(2, true, 16 * 1024),
        |request| {
            Some(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {
                        "code": -32026,
                        "message": "Action outcome is unknown",
                        "data": {
                            "kind": "action.outcomeUnknown",
                            "retryable": false
                        }
                    }
                }))
                .unwrap(),
            )
        },
    );
    assert_eq!(output.status.code(), Some(6));
    assert!(output.stderr.is_empty());
    assert_eq!(request.unwrap()["method"], "semantic.list");
    let result = machine_result(&output);
    assert_eq!(result["command"], serde_json::json!(["catalog", "list"]));
    assert_eq!(result["side_effect"], "read_only");
    assert_eq!(result["error"]["kind"], "invalidRequest");
    assert!(result["next_actions"].as_array().unwrap().is_empty());
}

#[test]
fn installed_process_rejects_malformed_and_oversized_input_before_transport() {
    let oversized = format!("{{\"peer-secret\":\"{}\"}}", "x".repeat(65_536));
    for input in [
        "{\"peer-secret\"".to_owned(),
        "{\"peer-secret\":1,\"peer-secret\":2}".to_owned(),
        oversized,
    ] {
        let output = run_without_target(&[
            "catalog",
            "invoke",
            "--capability",
            "account.delete",
            "--declaration-revision",
            "3",
            "--input-schema-id",
            "schema_action0001",
            "--input-schema-revision",
            "1",
            "--input-schema-digest",
            DIGEST,
            "--input",
            &input,
            "--authorization-grant",
            "grant_process_canary",
            "--output",
            "json",
        ]);
        assert_eq!(output.status.code(), Some(2));
        let result = machine_result(&output);
        assert_eq!(result["error"]["kind"], "cli.invalidInvocation");
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!rendered.contains("grant_process_canary"));
        assert!(!rendered.contains("peer-secret"));
    }
}

#[test]
fn installed_process_parser_diagnostics_never_echo_sensitive_values() {
    for output_mode in ["human", "json", "jsonl"] {
        let malformed_input = run_without_target(&[
            "catalog",
            "invoke",
            "--capability",
            "account.delete",
            "--declaration-revision",
            "3",
            "--input-schema-id",
            "schema_action0001",
            "--input-schema-revision",
            "1",
            "--input-schema-digest",
            DIGEST,
            "--input",
            "--authorization-grant",
            "grant_SECRET_CANARY",
            "--output",
            output_mode,
        ]);
        let malformed_cursor = run_without_target(&[
            "catalog",
            "list",
            "--cursor",
            "--session",
            "session_SECRET_CANARY",
            "--output",
            output_mode,
        ]);
        for (output, canary) in [
            (malformed_input, "grant_SECRET_CANARY"),
            (malformed_cursor, "session_SECRET_CANARY"),
        ] {
            assert_eq!(output.status.code(), Some(2), "{output_mode}");
            let rendered = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!rendered.contains(canary), "{output_mode}");
        }
    }
}

#[test]
fn installed_process_validates_all_selected_session_metadata_before_exchange() {
    let mut sessions = Vec::new();
    for (field, value, canary) in [
        ("session_id", serde_json::json!(""), None),
        (
            "session_id",
            serde_json::json!(format!("SECRET_SESSION_CANARY{}", "S".repeat(5000))),
            Some("SECRET_SESSION_CANARY"),
        ),
        ("generation", serde_json::json!(0), None),
        ("target_id", serde_json::json!(""), None),
        (
            "target_id",
            serde_json::json!(format!("SECRET_TARGET_CANARY{}", "T".repeat(5000))),
            Some("SECRET_TARGET_CANARY"),
        ),
        ("max_request_bytes", serde_json::json!(0), None),
        ("max_response_bytes", serde_json::json!(0), None),
        ("max_page_items", serde_json::json!(0), None),
    ] {
        let mut session = semantic_session(2, true, 16 * 1024);
        session[field] = value;
        sessions.push((session, canary));
    }
    let mut duplicate = semantic_session(2, true, 16 * 1024);
    duplicate["capabilities"] = serde_json::json!(["semantic.catalog", "semantic.catalog"]);
    sessions.push((duplicate, None));
    let mut invalid_capability = semantic_session(2, true, 16 * 1024);
    invalid_capability["capabilities"] = serde_json::json!(["semantic.catalog", "bad_capability"]);
    sessions.push((invalid_capability, Some("bad_capability")));

    for (session, canary) in sessions {
        let (output, request) =
            run_with_target(&["catalog", "list", "--output", "json"], session, |_| {
                panic!("invalid selected metadata must not exchange")
            });
        assert_eq!(output.status.code(), Some(6), "{canary:?}");
        assert!(output.stderr.is_empty(), "{canary:?}");
        assert!(request.is_none(), "{canary:?}");
        let result = machine_result(&output);
        assert_eq!(result["error"]["kind"], "invalidRequest", "{canary:?}");
        assert!(result["next_actions"].as_array().unwrap().is_empty());
        if let Some(canary) = canary {
            assert!(!String::from_utf8_lossy(&output.stdout).contains(canary));
        }
    }
}

#[test]
fn installed_process_accepts_unambiguous_negative_unicode_and_hyphen_values() {
    let session_id = "-session_0123456789";
    let target_id = "-target";
    let unicode_grant = format!("-{}", "授".repeat(200));
    let grant_argument = format!("--authorization-grant={unicode_grant}");
    let session_argument = format!("--session={session_id}");
    let target_argument = format!("--target={target_id}");
    let mut session = semantic_session(2, true, 16 * 1024);
    session["session_id"] = serde_json::json!(session_id);
    session["target_id"] = serde_json::json!(target_id);
    let (output, request) = run_with_target(
        &[
            "catalog",
            "invoke",
            "--capability",
            "account.delete",
            "--declaration-revision",
            "3",
            "--input-schema-id",
            "schema_action0001",
            "--input-schema-revision",
            "1",
            "--input-schema-digest",
            DIGEST,
            "--input",
            "-1",
            &grant_argument,
            &session_argument,
            &target_argument,
            "--output",
            "json",
        ],
        session,
        success_response,
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let request = request.unwrap();
    assert_eq!(request["context"]["id"], session_id);
    assert_eq!(request["params"]["input"], -1);
    assert_eq!(request["params"]["authorizationGrant"], unicode_grant);
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(!rendered.contains(&unicode_grant));
}

#[test]
fn installed_process_post_dispatch_eof_is_non_replayable() {
    let unicode_grant = format!("-{}", "授".repeat(200));
    let grant_argument = format!("--authorization-grant={unicode_grant}");
    let arguments = [
        "catalog",
        "invoke",
        "--capability",
        "account.delete",
        "--declaration-revision",
        "3",
        "--input-schema-id",
        "schema_action0001",
        "--input-schema-revision",
        "1",
        "--input-schema-digest",
        DIGEST,
        "--input",
        "{}",
        &grant_argument,
        "--output",
        "jsonl",
    ];
    let (output, request) =
        run_with_target(&arguments, semantic_session(2, true, 16 * 1024), |_| None);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    assert_eq!(request.unwrap()["method"], "semantic.invoke");
    let events = jsonl_events(&output);
    assert_eq!(events[0]["retry_safety"], "requires_idempotency_key");
    assert_eq!(
        events[1]["result"]["error"]["kind"],
        "action.outcomeUnknown"
    );
    assert_eq!(events[1]["result"]["error"]["retryable"], false);
    assert_eq!(
        events[1]["result"]["retry_safety"],
        "unsafe_after_ambiguous_result"
    );
    assert!(
        events[1]["result"]["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|action| action["side_effect"] == "read_only"
                && action["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|token| token != "invoke"
                        && token != "--authorization-grant"
                        && !token
                            .as_str()
                            .is_some_and(|token| token.contains(&unicode_grant))))
    );
}

#[test]
fn installed_process_replays_hyphen_and_unicode_cursor_next_actions_exactly() {
    for cursor in ["-resume".to_owned(), "游".repeat(4096)] {
        let session_id = "-session_0123456789";
        let target_id = "-target";
        let mut selected_session = semantic_session(2, true, 16 * 1024);
        selected_session["session_id"] = serde_json::json!(session_id);
        selected_session["target_id"] = serde_json::json!(target_id);
        let response_cursor = cursor.clone();
        let (first, first_request) = run_with_target(
            &["catalog", "list", "--output", "json"],
            selected_session.clone(),
            move |request| {
                Some(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "catalog": {"id": "catalog_12345678", "generation": 7},
                            "capabilities": [
                                {"id": "config.current", "kind": "resource", "declarationRevision": 1}
                            ],
                            "page": {
                                "truncated": true,
                                "returnedItems": 1,
                                "appliedLimits": {"maxItems": 1, "maxBytes": 16384},
                                "reasons": ["maxItems"],
                                "nextCursor": response_cursor
                            }
                        }
                    }))
                    .unwrap(),
                )
            },
        );
        assert_eq!(
            first.status.code(),
            Some(0),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(first_request.unwrap()["method"], "semantic.list");
        let first_result = machine_result(&first);
        let argv = first_result["next_actions"][0]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(
            argv.windows(2)
                .any(|tokens| tokens == ["--cursor", cursor.as_str()])
        );
        assert!(
            argv.iter()
                .any(|token| token == &format!("--session={session_id}"))
        );
        assert!(
            argv.iter()
                .any(|token| token == &format!("--target={target_id}"))
        );
        let continuation = argv[1..].iter().map(String::as_str).collect::<Vec<_>>();
        let (second, second_request) =
            run_with_target(&continuation, selected_session, success_response);
        assert_eq!(second.status.code(), Some(0));
        let second_request = second_request.unwrap();
        assert_eq!(second_request["context"]["id"], session_id);
        assert_eq!(second_request["params"]["cursor"], cursor);
    }
}

#[test]
fn installed_process_rejects_malformed_and_oversized_peer_wire() {
    let (malformed, _) = run_with_target(
        &["catalog", "list", "--output", "json"],
        semantic_session(2, true, 16 * 1024),
        |request| {
            Some(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{}},\"result\":{{\"peer\":\"canary\"}}}}",
                    serde_json::to_string(&request["id"]).unwrap()
                )
                .into_bytes(),
            )
        },
    );
    assert_eq!(malformed.status.code(), Some(6));
    assert_eq!(
        machine_result(&malformed)["error"]["kind"],
        "invalidRequest"
    );
    assert!(!String::from_utf8_lossy(&malformed.stdout).contains("canary"));

    let max_response_bytes = 1024;
    let (oversized, _) = run_with_target(
        &["catalog", "list", "--output", "json"],
        semantic_session(2, true, max_response_bytes),
        move |request| {
            let mut response = success_response(request).unwrap();
            response.resize(max_response_bytes + 1, b' ');
            Some(response)
        },
    );
    assert_eq!(oversized.status.code(), Some(1));
    assert_eq!(
        machine_result(&oversized)["error"]["kind"],
        "resourceExhausted"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostilePhase {
    Select,
    Exchange,
}

#[derive(Clone, Copy, Debug)]
enum HostileBehavior {
    ChunkedOversizedWithoutNewline,
    NeverClose,
    OversizedWithoutNewline,
}

#[test]
fn installed_process_bounds_hostile_select_and_exchange_frames() {
    for behavior in [
        HostileBehavior::ChunkedOversizedWithoutNewline,
        HostileBehavior::NeverClose,
        HostileBehavior::OversizedWithoutNewline,
    ] {
        let (output, killed) = run_with_hostile_target(
            &["catalog", "list", "--output", "json"],
            semantic_session(2, true, 1024),
            HostilePhase::Select,
            behavior,
        );
        assert!(!killed, "select {behavior:?} exceeded the parent deadline");
        assert_eq!(output.status.code(), Some(3), "select {behavior:?}");
        assert!(output.stderr.is_empty(), "select {behavior:?}");
        assert_eq!(
            machine_result(&output)["error"]["kind"],
            "transport.authenticationRequired",
            "select {behavior:?}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("peer_SECRET_CANARY"));
    }

    for (behavior, expected) in [
        (
            HostileBehavior::ChunkedOversizedWithoutNewline,
            "resourceExhausted",
        ),
        (HostileBehavior::NeverClose, "timeout"),
        (
            HostileBehavior::OversizedWithoutNewline,
            "resourceExhausted",
        ),
    ] {
        let (output, killed) = run_with_hostile_target(
            &["catalog", "list", "--output", "json"],
            semantic_session(2, true, 1024),
            HostilePhase::Exchange,
            behavior,
        );
        assert!(
            !killed,
            "exchange {behavior:?} exceeded the parent deadline"
        );
        assert!(output.stderr.is_empty(), "exchange {behavior:?}");
        assert_eq!(
            machine_result(&output)["error"]["kind"],
            expected,
            "exchange {behavior:?}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("peer_SECRET_CANARY"));
    }

    let (invoke, killed) = run_with_hostile_target(
        &[
            "catalog",
            "invoke",
            "--capability",
            "account.delete",
            "--declaration-revision",
            "3",
            "--input-schema-id",
            "schema_action0001",
            "--input-schema-revision",
            "1",
            "--input-schema-digest",
            DIGEST,
            "--input",
            "{}",
            "--output",
            "jsonl",
        ],
        semantic_session(2, true, 1024),
        HostilePhase::Exchange,
        HostileBehavior::NeverClose,
    );
    assert!(!killed, "invoke never-close exceeded the parent deadline");
    assert_eq!(invoke.status.code(), Some(5));
    assert!(invoke.stderr.is_empty());
    assert_eq!(
        jsonl_events(&invoke)[1]["result"]["error"]["kind"],
        "action.outcomeUnknown"
    );
}

fn semantic_session(minor: u64, catalog: bool, max_response_bytes: usize) -> Value {
    semantic_session_version(1, minor, catalog, max_response_bytes)
}

fn semantic_session_version(
    major: u64,
    minor: u64,
    catalog: bool,
    max_response_bytes: usize,
) -> Value {
    let mut capabilities = vec!["session.core"];
    if catalog {
        capabilities.push("semantic.catalog");
    }
    serde_json::json!({
        "session_id": SESSION,
        "generation": 7,
        "target_id": "target_demo",
        "protocol_major": major,
        "protocol_minor": minor,
        "capabilities": capabilities,
        "max_request_bytes": 131072,
        "max_response_bytes": max_response_bytes,
        "max_page_items": 2
    })
}

fn success_response(request: &Value) -> Option<Vec<u8>> {
    let result = match request["method"].as_str().unwrap() {
        "semantic.list" => serde_json::json!({
            "catalog": {"id": "catalog_12345678", "generation": 7},
            "capabilities": [
                {"id": "config.current", "kind": "resource", "declarationRevision": 1}
            ],
            "page": {
                "truncated": false,
                "returnedItems": 1,
                "appliedLimits": {"maxItems": 2, "maxBytes": 4096},
            }
        }),
        "semantic.show" => serde_json::json!({
            "id": request["params"]["capability"],
            "kind": "action",
            "declarationRevision": request["params"]["declarationRevision"],
            "inputSchema": {"id": "schema_action0001", "revision": 1, "digest": DIGEST},
            "policy": {"authorization": "none", "retrySafety": "retryWithProofOnly"}
        }),
        "semantic.schema" => serde_json::json!({
            "schema": request["params"]["schema"],
            "document": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$id": "app://schema/config-current"
            }
        }),
        "semantic.query" => {
            let value = serde_json::json!({"enabled": true});
            let bytes = serde_json::to_vec(&value).unwrap().len();
            serde_json::json!({
                "value": value,
                "valueSchema": request["params"]["valueSchema"],
                "bytes": bytes
            })
        }
        "semantic.invoke" => serde_json::json!({
            "capability": request["params"]["capability"],
            "declarationRevision": request["params"]["declarationRevision"],
            "completed": true
        }),
        method => panic!("unexpected method {method}"),
    };
    Some(
        serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": result
        }))
        .unwrap(),
    )
}

fn run_with_target<F>(arguments: &[&str], session: Value, responder: F) -> (Output, Option<Value>)
where
    F: FnOnce(&Value) -> Option<Vec<u8>> + Send + 'static,
{
    let socket = socket_path();
    let listener = UnixListener::bind(&socket).expect("fixture Target binds a private socket");
    let target = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("fixture CLI connects");
        let mut stream = BufReader::new(stream);
        let select = read_line(&mut stream).expect("select request");
        assert_eq!(select["type"], "select");
        if !select["session"].is_null() {
            assert_eq!(select["session"], session["session_id"]);
        }
        if !select["target"].is_null() {
            assert_eq!(select["target"], session["target_id"]);
        }
        write_line(stream.get_mut(), &serde_json::to_vec(&session).unwrap());
        let max_response_bytes = session["max_response_bytes"]
            .as_u64()
            .expect("fixture session carries max_response_bytes")
            as usize;
        let exchange = read_line(&mut stream)?;
        assert_eq!(exchange["type"], "exchange");
        let request = exchange["request"].clone();
        if let Some(response) = responder(&request) {
            write_exchange_frame(stream.get_mut(), &response, max_response_bytes);
        }
        Some(request)
    });
    let output = run_process(arguments, &socket);
    let request = target.join().expect("fixture Target joins");
    std::fs::remove_file(&socket).expect("fixture socket cleanup");
    (output, request)
}

fn run_without_target(arguments: &[&str]) -> Output {
    run_process(arguments, &socket_path())
}

fn run_with_hostile_target(
    arguments: &[&str],
    session: Value,
    phase: HostilePhase,
    behavior: HostileBehavior,
) -> (Output, bool) {
    let socket = socket_path();
    let listener = UnixListener::bind(&socket).expect("hostile Target binds a private socket");
    let stop = Arc::new(AtomicBool::new(false));
    let target_stop = Arc::clone(&stop);
    let target = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("fixture CLI connects");
        let mut stream = BufReader::new(stream);
        let select = read_line(&mut stream).expect("select request");
        assert_eq!(select["type"], "select");
        if phase == HostilePhase::Select {
            write_hostile_frame(stream.get_mut(), behavior, 64 * 1024, &target_stop);
            return;
        }
        write_line(stream.get_mut(), &serde_json::to_vec(&session).unwrap());
        let exchange = read_line(&mut stream).expect("exchange request");
        assert_eq!(exchange["type"], "exchange");
        write_hostile_frame(stream.get_mut(), behavior, 1024, &target_stop);
    });
    let (output, killed) = run_process_bounded(arguments, &socket, Duration::from_secs(4));
    stop.store(true, Ordering::Release);
    target.join().expect("hostile Target joins");
    std::fs::remove_file(&socket).expect("hostile fixture socket cleanup");
    (output, killed)
}

fn write_hostile_frame(
    stream: &mut UnixStream,
    behavior: HostileBehavior,
    limit: usize,
    stop: &AtomicBool,
) {
    match behavior {
        HostileBehavior::ChunkedOversizedWithoutNewline => {
            let bytes = b"peer_SECRET_CANARY";
            let mut remaining = limit + 1;
            while remaining > 0 {
                let chunk = remaining.min(bytes.len());
                if stream.write_all(&bytes[..chunk]).is_err() {
                    return;
                }
                remaining -= chunk;
            }
            let _ = stream.flush();
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        }
        HostileBehavior::NeverClose => {
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        }
        HostileBehavior::OversizedWithoutNewline => {
            let mut bytes = b"peer_SECRET_CANARY".to_vec();
            bytes.resize(limit + 1, b'X');
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn run_process_bounded(arguments: &[&str], socket: &PathBuf, timeout: Duration) -> (Output, bool) {
    let mut child = Command::new(FIXTURE_BINARY)
        .env_clear()
        .env("HOME", "/poisoned/home")
        .env("PATH", "/poisoned/tools")
        .env("APPPILOTKIT_CONTRACT_FIXTURE_SOCKET", socket)
        .current_dir(std::env::temp_dir())
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("installed fixture process starts");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin-canary-must-be-ignored\n")
        .expect("fixture stdin accepts the canary");
    let deadline = Instant::now() + timeout;
    let mut killed = false;
    loop {
        if child.try_wait().expect("fixture child status").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            killed = true;
            child.kill().expect("timed-out fixture child is killed");
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    (
        child
            .wait_with_output()
            .expect("fixture child is reaped and output drained"),
        killed,
    )
}

fn run_process(arguments: &[&str], socket: &PathBuf) -> Output {
    let mut child = Command::new(FIXTURE_BINARY)
        .env_clear()
        .env("HOME", "/poisoned/home")
        .env("PATH", "/poisoned/tools")
        .env("APPPILOTKIT_CONTRACT_FIXTURE_SOCKET", socket)
        .current_dir(std::env::temp_dir())
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("installed fixture process starts");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin-canary-must-be-ignored\n")
        .expect("fixture stdin accepts the canary");
    child.wait_with_output().expect("fixture process exits")
}

fn read_line(stream: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = Vec::new();
    if stream.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    Some(serde_json::from_slice(&line).expect("fixture transport request is JSON"))
}

fn write_line(stream: &mut UnixStream, bytes: &[u8]) {
    stream.write_all(bytes).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

fn write_exchange_frame(stream: &mut UnixStream, bytes: &[u8], max_response_bytes: usize) {
    stream.write_all(bytes).unwrap();
    if bytes.len() > max_response_bytes {
        match stream.write_all(b"\n") {
            Ok(()) => {}
            Err(err) if expected_peer_close_after_oversized(&err) => return,
            Err(err) => panic!("oversized exchange terminator failed: {err}"),
        }
        match stream.flush() {
            Ok(()) => {}
            Err(err) if expected_peer_close_after_oversized(&err) => return,
            Err(err) => panic!("oversized exchange flush failed: {err}"),
        }
        wait_for_fixture_client_eof(stream);
        return;
    }
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    wait_for_fixture_client_eof(stream);
}

fn wait_for_fixture_client_eof(stream: &mut UnixStream) {
    stream.shutdown(Shutdown::Write).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut unexpected = [0_u8; 1];
    match stream.read(&mut unexpected) {
        Ok(0) => {}
        Ok(count) => panic!("fixture client sent {count} unexpected bytes after its response"),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            panic!("fixture client did not close within the peer EOF deadline")
        }
        Err(err) => panic!("fixture client EOF read failed: {err}"),
    }
}

fn expected_peer_close_after_oversized(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
    )
}

fn socket_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "apppilotkit-cli-{}-{}.sock",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    ))
}

fn machine_result(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("Machine Result JSON")
}

fn jsonl_events(output: &Output) -> Vec<Value> {
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("JSONL event"))
        .collect()
}
