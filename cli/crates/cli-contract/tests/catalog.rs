use apppilotkit_cli_contract::{
    CatalogExchangeError, CatalogExchangeFailure, CliConfig, CliCore, FakeCatalogRuntime,
    OpenedProtocolSession,
};
use serde_json::Value;
use std::sync::Arc;

const SESSION: &str = "session_0123456789abcdef";
const TARGET: &str = "target_demo";
const INPUT_DIGEST: &str =
    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const VALUE_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const GRANT: &str = "grant_0123456789abcdef";
const SECRET_INPUT: &str = "{\"account\":\"opaque-target\"}";

fn semantic_session() -> OpenedProtocolSession {
    OpenedProtocolSession {
        session_id: SESSION.to_owned(),
        generation: 7,
        target_id: TARGET.to_owned(),
        protocol_major: 1,
        protocol_minor: 2,
        capabilities: vec!["session.core".to_owned(), "semantic.catalog".to_owned()],
        max_request_bytes: 4096,
        max_response_bytes: 4096,
        max_page_items: 2,
    }
}

fn core_with(runtime: FakeCatalogRuntime) -> CliCore {
    CliCore::with_catalog_runtime(CliConfig::new("fixture-cli", "0.1.0"), Arc::new(runtime))
        .expect("CLI initializes")
}

fn json_result(output: &apppilotkit_cli_contract::ProcessOutput) -> Value {
    serde_json::from_slice(&output.stdout).expect("machine result JSON")
}

fn rendered_blob(output: &apppilotkit_cli_contract::ProcessOutput) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_secrets(output: &apppilotkit_cli_contract::ProcessOutput) {
    let blob = rendered_blob(output);
    assert!(!blob.contains(GRANT), "grant must not appear in CLI output");
    assert!(
        !blob.contains("opaque-target"),
        "typed invoke input must not appear in CLI output"
    );
}

fn assert_argv_is_tokens(result: &Value) {
    for action in result["next_actions"].as_array().expect("next actions") {
        let argv = action["argv"].as_array().expect("argv array");
        assert!(!argv.is_empty());
        assert!(
            argv.iter()
                .all(|token| token.as_str().is_some_and(|value| !value.is_empty()))
        );
        assert!(
            argv.iter().all(|token| token
                .as_str()
                .is_none_or(|value| !value.contains(" && ") && !value.contains('|'))),
            "Next Action argv must not be a shell string"
        );
    }
}

#[test]
fn offline_discovery_does_not_leak_or_contact_a_target_catalog() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
    let capabilities = core.run(["fixture-cli", "capabilities", "--output", "json"]);
    assert_eq!(capabilities.exit_code, 0);
    let manifest = json_result(&capabilities);
    let blob = manifest.to_string();
    assert!(blob.contains("catalog"));
    assert!(blob.contains("https://apppilotkit.dev/cli/v1/catalog.schema.json"));
    assert!(!blob.contains("catalog_"));
    assert!(!blob.contains("config.current"));
    assert!(!blob.contains("account.delete"));
    assert!(!blob.contains("semantic.list"));

    let list = core.run(["fixture-cli", "catalog", "list", "--output", "json"]);
    assert_eq!(list.exit_code, 4);
    let result = json_result(&list);
    assert_eq!(result["error"]["kind"], "session.selectionRequired");
    assert_argv_is_tokens(&result);
    let next = result["next_actions"].as_array().unwrap();
    assert_eq!(next.len(), 2);
    assert_eq!(
        next[0]["argv"],
        serde_json::json!(["fixture-cli", "capabilities", "--output", "json"])
    );
    assert_eq!(
        next[1]["argv"],
        serde_json::json!([
            "fixture-cli",
            "doctor",
            "--output",
            "json",
            "--non-interactive"
        ])
    );

    let human = core.run(["fixture-cli", "catalog", "list"]);
    assert_eq!(human.exit_code, 4);
    assert!(human.stdout.is_empty());
    assert_eq!(
        String::from_utf8(human.stderr).expect("UTF-8 human failure"),
        "Select one opened Protocol Session; live catalog access is not available offline.\n"
    );

    let jsonl = core.run(["fixture-cli", "catalog", "list", "--output", "jsonl"]);
    assert_eq!(jsonl.exit_code, 4);
    assert!(jsonl.stderr.is_empty());
    let events: Vec<Value> = jsonl
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("JSONL event"))
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(events[0]["command"], serde_json::json!(["catalog", "list"]));
    assert_eq!(events[0]["side_effect"], "read_only");
    assert_eq!(events[0]["retry_safety"], "safe");
    assert_eq!(events[1]["type"], "run.failed");
    assert_eq!(events[1]["result"], result);
    assert!(!events[1].to_string().contains("catalog_"));
}

#[test]
fn catalog_help_is_generic_and_does_not_enumerate_live_entries() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0")).expect("CLI initializes");
    for argv in [
        vec!["fixture-cli", "catalog", "--help"],
        vec!["fixture-cli", "catalog", "list", "--help"],
        vec!["fixture-cli", "catalog", "invoke", "--help"],
        vec!["fixture-cli", "schema", "list", "--output", "json"],
    ] {
        let output = core.run(argv.clone());
        assert_eq!(output.exit_code, 0, "{argv:?}");
        let text = rendered_blob(&output);
        assert!(!text.contains("config.current"), "{argv:?}");
        assert!(!text.contains("account.delete"), "{argv:?}");
    }
}

#[test]
fn live_list_show_schema_query_project_protocol_camelcase_through_one_renderer() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|session, request| {
        assert_eq!(request["context"]["id"], session.session_id);
        assert_eq!(request["context"]["generation"], 7);
        let body = match request["method"].as_str() {
            Some("semantic.list") => serde_json::json!({
                "catalog": {"id": "catalog_12345678", "generation": 7},
                "capabilities": [
                    {"id": "config.current", "kind": "resource", "declarationRevision": 1}
                ],
                "page": {
                    "truncated": true,
                    "returnedItems": 1,
                    "appliedLimits": {"maxItems": 1, "maxBytes": 4096},
                    "reasons": ["maxItems"],
                    "nextCursor": "cursor_catalog_page_2"
                }
            }),
            Some("semantic.show") => serde_json::json!({
                "id": "account.delete",
                "kind": "action",
                "declarationRevision": 3,
                "inputSchema": {
                    "id": "schema_action0001",
                    "revision": 1,
                    "digest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                },
                "policy": {
                    "authorization": "destructiveAuthorization",
                    "retrySafety": "retryWithProofOnly"
                }
            }),
            Some("semantic.schema") => serde_json::json!({
                "schema": {
                    "id": "schema_value0001",
                    "revision": 1,
                    "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                },
                "document": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "$id": "app://config.current/value@1"
                }
            }),
            Some("semantic.query") => serde_json::json!({
                "value": {"mode": "safe"},
                "valueSchema": {
                    "id": "schema_value0001",
                    "revision": 1,
                    "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                },
                "bytes": 15
            }),
            other => panic!("live catalog commands must not send {other:?}"),
        };
        Ok(serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": body}))
    });
    let core = core_with(runtime.clone());

    let list = core.run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--target",
        TARGET,
        "--output",
        "json",
    ]);
    assert_eq!(list.exit_code, 0);
    let list_json = json_result(&list);
    assert_eq!(
        list_json["data"]["capabilities"][0]["declaration_revision"],
        1
    );
    assert!(list_json["disclosure"]["truncated"].as_bool().unwrap());
    assert_eq!(
        list_json["disclosure"]["next_cursor"],
        "cursor_catalog_page_2"
    );
    assert!(
        list_json["next_actions"][0]["argv"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|window| window == ["--cursor", "cursor_catalog_page_2"])
    );

    let human = core.run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--target",
        TARGET,
    ]);
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        "Semantic Catalog: 1 capabilities truncated.\n"
    );

    let jsonl = core.run([
        "fixture-cli",
        "catalog",
        "show",
        "--capability",
        "account.delete",
        "--declaration-revision",
        "3",
        "--session",
        SESSION,
        "--output",
        "jsonl",
    ]);
    assert_eq!(jsonl.exit_code, 0);
    let events: Vec<Value> = jsonl
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(
        events[1]["result"]["data"]["policy"]["authorization"],
        "destructive_authorization"
    );
    assert_eq!(
        events[1]["result"]["data"]["policy"]["retry_safety"],
        "retry_with_proof_only"
    );
    assert_eq!(events[1]["result"]["data"]["declaration_revision"], 3);

    let schema = core.run([
        "fixture-cli",
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
        VALUE_DIGEST,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(schema.exit_code, 0);
    assert_eq!(
        json_result(&schema)["data"]["schema"]["id"],
        "schema_value0001"
    );

    let query = core.run([
        "fixture-cli",
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
        VALUE_DIGEST,
        "--input-schema-id",
        "schema_input0001",
        "--input-schema-revision",
        "1",
        "--input-schema-digest",
        VALUE_DIGEST,
        "--input",
        "{\"scope\":\"active\"}",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(query.exit_code, 0);
    assert_eq!(json_result(&query)["data"]["bytes"], 15);
    let methods: Vec<_> = runtime
        .exchange_requests()
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        vec![
            "semantic.list",
            "semantic.list",
            "semantic.show",
            "semantic.schema",
            "semantic.query"
        ]
    );
    let query_request = runtime.exchange_requests().last().cloned().unwrap();
    assert_eq!(
        query_request["params"]["inputSchema"]["id"],
        "schema_input0001"
    );
    assert_eq!(query_request["params"]["input"]["scope"], "active");
}

#[test]
fn missing_capability_and_ambiguous_selection_never_exchange() {
    let missing = FakeCatalogRuntime::new();
    missing.add_session(OpenedProtocolSession {
        capabilities: vec!["session.core".to_owned()],
        ..semantic_session()
    });
    let core = core_with(missing.clone());
    let output = core.run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(output.exit_code, 6);
    assert_eq!(
        json_result(&output)["error"]["kind"],
        "capabilityUnavailable"
    );
    assert!(missing.exchange_requests().is_empty());

    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    let mut other = semantic_session();
    other.session_id = "session_abcdef0123456789".to_owned();
    other.target_id = "target_other".to_owned();
    runtime.add_session(other);
    let core = core_with(runtime.clone());
    let list = core.run(["fixture-cli", "catalog", "list", "--output", "json"]);
    assert_eq!(list.exit_code, 4);
    assert_eq!(
        json_result(&list)["error"]["kind"],
        "session.selectionRequired"
    );
    assert!(runtime.exchange_requests().is_empty());

    let invoke = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--output",
        "json",
    ]);
    assert_eq!(invoke.exit_code, 4);
    let invoke_json = json_result(&invoke);
    assert_eq!(invoke_json["error"]["kind"], "session.selectionRequired");
    assert_no_secrets(&invoke);
    assert_argv_is_tokens(&invoke_json);
    assert!(
        invoke_json["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|action| action["argv"]
                .as_array()
                .unwrap()
                .iter()
                .all(|token| token != "invoke"))
    );
    assert!(runtime.exchange_requests().is_empty());
}

#[test]
fn malformed_and_oversized_input_fail_before_transport() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|_, _| {
        Err(CatalogExchangeError::post_dispatch(
            CatalogExchangeFailure::TransportInternal,
        ))
    });
    let core = core_with(runtime.clone());
    let oversized = format!("{{\"k\":\"{}\"}}", "x".repeat(65_536));
    for input in ["{", "{\"a\":1,\"a\":2}", oversized.as_str()] {
        let output = core.run([
            "fixture-cli",
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
            INPUT_DIGEST,
            "--input",
            input,
            "--session",
            SESSION,
            "--output",
            "json",
        ]);
        assert_eq!(output.exit_code, 2, "input {input:.16}");
        assert_eq!(
            json_result(&output)["error"]["kind"],
            "cli.invalidInvocation"
        );
        assert_no_secrets(&output);
    }
    assert!(runtime.exchange_requests().is_empty());
}

#[test]
fn unknown_session_and_policy_denied_map_stably_without_replay() {
    let expired = FakeCatalogRuntime::new();
    expired.add_session(semantic_session());
    let core = core_with(expired.clone());
    let output = core.run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        "session_deadbeefdeadbeef",
        "--output",
        "json",
    ]);
    assert_eq!(output.exit_code, 4);
    assert_eq!(json_result(&output)["error"]["kind"], "sessionExpired");
    assert!(expired.exchange_requests().is_empty());

    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {
                "code": -32024,
                "message": "Action policy is denied",
                "data": {
                    "kind": "action.policyDenied",
                    "retryable": false,
                    "details": {"capability": "account.delete"}
                }
            }
        }))
    });
    let core = core_with(runtime.clone());
    let denied = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(denied.exit_code, 4);
    let result = json_result(&denied);
    assert_eq!(result["error"]["kind"], "action.policyDenied");
    assert_eq!(result["error"]["retryable"], false);
    assert_eq!(result["error"]["details"]["capability"], "account.delete");
    assert_no_secrets(&denied);
    assert!(
        result["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|action| action["argv"]
                .as_array()
                .unwrap()
                .iter()
                .all(|token| token != "invoke"))
    );
    assert_eq!(runtime.exchange_requests().len(), 1);
    assert_eq!(runtime.exchange_requests()[0]["method"], "semantic.invoke");
}

#[test]
fn outcome_unknown_is_not_replayable_and_drops_unsafe_details() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {
                "code": -32026,
                "message": "Action outcome is unknown",
                "data": {
                    "kind": "action.outcomeUnknown",
                    "retryable": true,
                    "details": {"capability": "account.delete", "authorizationGrant": GRANT}
                }
            }
        }))
    });
    let core = core_with(runtime.clone());
    let unknown = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(unknown.exit_code, 5);
    let result = json_result(&unknown);
    assert_eq!(result["error"]["kind"], "action.outcomeUnknown");
    assert_eq!(result["error"]["retryable"], false);
    assert_eq!(result["retry_safety"], "unsafe_after_ambiguous_result");
    assert!(
        result["error"]["details"]
            .get("authorizationGrant")
            .is_none()
    );
    assert_no_secrets(&unknown);
    let next = result["next_actions"].as_array().unwrap();
    assert!(!next.is_empty());
    assert!(next.iter().all(|action| {
        action["side_effect"] == "read_only"
            && action["argv"]
                .as_array()
                .unwrap()
                .iter()
                .all(|token| token != "invoke")
    }));
    assert!(next.iter().any(|action| {
        action["argv"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|window| window == ["catalog", "list"])
    }));
    assert_eq!(runtime.exchange_requests()[0]["method"], "semantic.invoke");
    assert_eq!(
        runtime.exchange_requests()[0]["params"]["authorizationGrant"],
        GRANT
    );
}

#[test]
fn successful_invoke_forwards_only_semantic_invoke() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "capability": "account.delete",
                "declarationRevision": 3,
                "completed": true
            }
        }))
    });
    let core = core_with(runtime.clone());
    let output = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(output.exit_code, 0);
    let result = json_result(&output);
    assert_eq!(result["side_effect"], "app_mutation");
    assert_eq!(result["retry_safety"], "requires_idempotency_key");
    assert_eq!(result["data"]["declaration_revision"], 3);
    assert_no_secrets(&output);
    assert_eq!(runtime.exchange_requests().len(), 1);
    assert_eq!(runtime.exchange_requests()[0]["method"], "semantic.invoke");
    assert_eq!(runtime.exchange_requests()[0]["jsonrpc"], "2.0");
}

fn invoke_arguments(output: &str) -> Vec<&str> {
    vec![
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        output,
    ]
}

#[test]
fn post_dispatch_failures_are_unknown_while_pre_dispatch_failures_are_not() {
    for failure in [
        CatalogExchangeFailure::Timeout,
        CatalogExchangeFailure::EndOfStream,
        CatalogExchangeFailure::SessionExpired,
        CatalogExchangeFailure::AuthenticationRequired,
        CatalogExchangeFailure::TransportInternal,
    ] {
        let runtime = FakeCatalogRuntime::new();
        runtime.add_session(semantic_session());
        runtime.set_responder(move |_, _| Err(CatalogExchangeError::post_dispatch(failure)));
        let output = core_with(runtime.clone()).run(invoke_arguments("json"));
        assert_eq!(output.exit_code, 5, "post-dispatch {failure:?}");
        let result = json_result(&output);
        assert_eq!(result["error"]["kind"], "action.outcomeUnknown");
        assert_eq!(result["error"]["retryable"], false);
        assert_eq!(result["retry_safety"], "unsafe_after_ambiguous_result");
        assert!(
            result["next_actions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|action| {
                    action["side_effect"] == "read_only"
                        && action["argv"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|token| token != "invoke")
                })
        );
        assert_no_secrets(&output);
        assert_eq!(runtime.exchange_requests().len(), 1);
    }

    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|_, _| {
        Err(CatalogExchangeError::pre_dispatch(
            CatalogExchangeFailure::SessionExpired,
        ))
    });
    let output = core_with(runtime).run(invoke_arguments("json"));
    assert_eq!(output.exit_code, 4);
    assert_eq!(json_result(&output)["error"]["kind"], "sessionExpired");
    assert_no_secrets(&output);
}

#[test]
fn malformed_correlated_and_oversized_peer_wire_data_fail_closed() {
    let malformed = FakeCatalogRuntime::new();
    malformed.add_session(semantic_session());
    malformed.set_wire_responder(|_, request| {
        Ok(format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{}},\"result\":{{\"canary\":\"peer-secret\"}}}}",
            serde_json::to_string(&request["id"]).unwrap()
        )
        .into_bytes())
    });
    let malformed_output = core_with(malformed).run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(malformed_output.exit_code, 6);
    assert_eq!(
        json_result(&malformed_output)["error"]["kind"],
        "invalidRequest"
    );
    assert!(!rendered_blob(&malformed_output).contains("peer-secret"));

    let mismatched = FakeCatalogRuntime::new();
    mismatched.add_session(semantic_session());
    mismatched.set_responder(|_, _| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": "catalog-wrong",
            "result": {
                "catalog": {"id": "catalog_12345678", "generation": 7},
                "capabilities": [],
                "page": {
                    "truncated": false,
                    "returnedItems": 0,
                    "appliedLimits": {"maxItems": 2, "maxBytes": 4096},
                    "reasons": []
                }
            }
        }))
    });
    let mismatched_output = core_with(mismatched).run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(mismatched_output.exit_code, 6);
    assert_eq!(
        json_result(&mismatched_output)["error"]["kind"],
        "invalidRequest"
    );

    let page_bytes = FakeCatalogRuntime::new();
    page_bytes.add_session(semantic_session());
    page_bytes.set_wire_responder(|_, request| {
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "catalog": {"id": "catalog_12345678", "generation": 7},
                "capabilities": [],
                "page": {
                    "truncated": false,
                    "returnedItems": 0,
                    "appliedLimits": {"maxItems": 2, "maxBytes": 1024}
                }
            }
        }))
        .unwrap();
        response.extend(std::iter::repeat_n(b' ', 1024));
        Ok(response)
    });
    let page_bytes_output = core_with(page_bytes).run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(page_bytes_output.exit_code, 6);
    assert_eq!(
        json_result(&page_bytes_output)["error"]["kind"],
        "invalidRequest"
    );

    let oversized = FakeCatalogRuntime::new();
    oversized.add_session(OpenedProtocolSession {
        max_response_bytes: 1024,
        ..semantic_session()
    });
    oversized.set_wire_responder(|_, request| {
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "catalog": {"id": "catalog_12345678", "generation": 7},
                "capabilities": [],
                "page": {
                    "truncated": false,
                    "returnedItems": 0,
                    "appliedLimits": {"maxItems": 2, "maxBytes": 1024},
                    "reasons": []
                }
            }
        }))
        .unwrap();
        response.extend(std::iter::repeat_n(b' ', 1024));
        Ok(response)
    });
    let oversized_output = core_with(oversized).run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(oversized_output.exit_code, 1);
    assert_eq!(
        json_result(&oversized_output)["error"]["kind"],
        "resourceExhausted"
    );
}

#[test]
fn method_specific_peer_echoes_handles_query_bytes_and_pages_are_enforced() {
    let page = FakeCatalogRuntime::new();
    page.add_session(OpenedProtocolSession {
        max_page_items: 1,
        ..semantic_session()
    });
    page.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "catalog": {"id": "catalog_12345678", "generation": 7},
                "capabilities": [
                    {"id": "config.current", "kind": "resource", "declarationRevision": 1},
                    {"id": "account.delete", "kind": "action", "declarationRevision": 3}
                ],
                "page": {
                    "truncated": false,
                    "returnedItems": 2,
                    "appliedLimits": {"maxItems": 2, "maxBytes": 4096},
                    "reasons": []
                }
            }
        }))
    });
    let page_output = core_with(page).run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(page_output.exit_code, 6);
    assert_eq!(json_result(&page_output)["error"]["kind"], "invalidRequest");

    let schema = FakeCatalogRuntime::new();
    schema.add_session(semantic_session());
    schema.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "schema": {
                    "id": "schema_other0001",
                    "revision": 1,
                    "digest": VALUE_DIGEST
                },
                "document": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "$id": "app://schema/other"
                }
            }
        }))
    });
    let schema_output = core_with(schema).run([
        "fixture-cli",
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
        VALUE_DIGEST,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(schema_output.exit_code, 6);
    assert_eq!(
        json_result(&schema_output)["error"]["kind"],
        "invalidRequest"
    );

    let query = FakeCatalogRuntime::new();
    query.add_session(semantic_session());
    query.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "value": {"enabled": true},
                "valueSchema": request["params"]["valueSchema"],
                "bytes": 1
            }
        }))
    });
    let query_output = core_with(query).run([
        "fixture-cli",
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
        VALUE_DIGEST,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(query_output.exit_code, 6);
    assert_eq!(
        json_result(&query_output)["error"]["kind"],
        "invalidRequest"
    );

    let invoke = FakeCatalogRuntime::new();
    invoke.add_session(semantic_session());
    invoke.set_responder(|_, request| {
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "capability": "account.other",
                "declarationRevision": 3,
                "completed": true
            }
        }))
    });
    let invoke_output = core_with(invoke).run(invoke_arguments("json"));
    assert_eq!(invoke_output.exit_code, 5);
    assert_eq!(
        json_result(&invoke_output)["error"]["kind"],
        "action.outcomeUnknown"
    );
    assert_no_secrets(&invoke_output);
}

#[test]
fn query_byte_correlation_accepts_ecmascript_canonical_numbers() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_wire_responder(|_, request| {
        Ok(format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"value\":{{\"ratio\":1.0}},\"valueSchema\":{},\"bytes\":11}}}}",
            serde_json::to_string(&request["id"]).unwrap(),
            serde_json::to_string(&request["params"]["valueSchema"]).unwrap()
        )
        .into_bytes())
    });
    let output = core_with(runtime).run([
        "fixture-cli",
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
        VALUE_DIGEST,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(output.exit_code, 0);
    assert_eq!(json_result(&output)["data"]["bytes"], 11);
}

#[test]
fn ambiguous_target_never_guesses_or_replays_invoke() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    let mut other = semantic_session();
    other.target_id = "target_other".to_owned();
    runtime.add_session(other);
    let core = core_with(runtime.clone());

    let list = core.run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(list.exit_code, 4);
    let list_json = json_result(&list);
    assert_eq!(list_json["error"]["kind"], "target.selectionRequired");
    assert_eq!(
        list_json["next_actions"][0]["argv"],
        serde_json::json!([
            "fixture-cli",
            "catalog",
            "list",
            format!("--session={SESSION}"),
            format!("--target={TARGET}"),
            "--output",
            "json"
        ])
    );
    assert_eq!(
        list_json["next_actions"][1]["argv"],
        serde_json::json!([
            "fixture-cli",
            "catalog",
            "list",
            format!("--session={SESSION}"),
            "--target=target_other",
            "--output",
            "json"
        ])
    );

    let invoke = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(invoke.exit_code, 4);
    let invoke_json = json_result(&invoke);
    assert_eq!(invoke_json["error"]["kind"], "target.selectionRequired");
    assert_no_secrets(&invoke);
    assert_argv_is_tokens(&invoke_json);
    assert!(
        invoke_json["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|action| action["argv"]
                .as_array()
                .unwrap()
                .iter()
                .all(|token| token != "invoke"))
    );
    assert!(runtime.exchange_requests().is_empty());
}

#[test]
fn incompatible_protocol_and_negotiated_oversize_fail_before_exchange() {
    let incompatible = FakeCatalogRuntime::new();
    incompatible.add_session(OpenedProtocolSession {
        protocol_minor: 1,
        ..semantic_session()
    });
    let core = core_with(incompatible.clone());
    let output = core.run([
        "fixture-cli",
        "catalog",
        "list",
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(output.exit_code, 6);
    assert_eq!(
        json_result(&output)["error"]["kind"],
        "incompatibleProtocol"
    );
    assert!(incompatible.exchange_requests().is_empty());

    let oversized = FakeCatalogRuntime::new();
    oversized.add_session(OpenedProtocolSession {
        max_request_bytes: 1024,
        ..semantic_session()
    });
    oversized.set_responder(|_, _| {
        Err(CatalogExchangeError::post_dispatch(
            CatalogExchangeFailure::TransportInternal,
        ))
    });
    let core = core_with(oversized.clone());
    let oversized_input = serde_json::to_string(&"x".repeat(1500)).unwrap();
    let output = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        &oversized_input,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(output.exit_code, 1);
    assert_eq!(json_result(&output)["error"]["kind"], "resourceExhausted");
    assert_no_secrets(&output);
    assert!(oversized.exchange_requests().is_empty());
}

#[test]
fn invoke_transport_timeout_is_unknown_and_jsonl_does_not_replay() {
    let runtime = FakeCatalogRuntime::new();
    runtime.add_session(semantic_session());
    runtime.set_responder(|_, _| {
        Err(CatalogExchangeError::post_dispatch(
            CatalogExchangeFailure::Timeout,
        ))
    });
    let core = core_with(runtime.clone());
    let json = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        "json",
    ]);
    assert_eq!(json.exit_code, 5);
    let result = json_result(&json);
    assert_eq!(result["error"]["kind"], "action.outcomeUnknown");
    assert_eq!(result["error"]["retryable"], false);
    assert_eq!(result["retry_safety"], "unsafe_after_ambiguous_result");
    assert_no_secrets(&json);
    assert!(
        result["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|action| {
                action["side_effect"] == "read_only"
                    && action["argv"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|token| token != "invoke")
            })
    );

    let jsonl = core.run([
        "fixture-cli",
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
        INPUT_DIGEST,
        "--input",
        SECRET_INPUT,
        "--authorization-grant",
        GRANT,
        "--session",
        SESSION,
        "--output",
        "jsonl",
    ]);
    assert_eq!(jsonl.exit_code, 5);
    assert!(jsonl.stderr.is_empty());
    assert_no_secrets(&jsonl);
    let events: Vec<Value> = jsonl
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("JSONL event"))
        .collect();
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(events[0]["side_effect"], "app_mutation");
    assert_eq!(events[0]["retry_safety"], "requires_idempotency_key");
    assert_eq!(events[1]["type"], "run.failed");
    assert_eq!(
        events[1]["result"]["retry_safety"],
        "unsafe_after_ambiguous_result"
    );
    assert_eq!(events[1]["result"], result);
    assert!(
        events[1]["result"]["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|action| action["argv"]
                .as_array()
                .unwrap()
                .iter()
                .all(|token| token != "invoke"))
    );
    assert_eq!(runtime.exchange_requests().len(), 2);
}
