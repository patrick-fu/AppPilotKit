use apppilotkit_rust_foundation_spike::ContractSuite;

const REQUEST_SCHEMA: &str =
    "https://apppilotkit.dev/protocol/v1/envelope.schema.json#/$defs/request";

#[test]
fn embedded_schema_accepts_valid_fixture_and_rejects_invalid_fixture() {
    let suite = ContractSuite::new();
    let valid = suite
        .parse_strict_json(include_str!(
            "../../../../protocol/v1/fixtures/valid/session-open-request.json"
        ))
        .expect("valid fixture should parse");
    let invalid = suite
        .parse_strict_json(include_str!(
            "../../../../protocol/v1/fixtures/invalid/numeric-request-id.json"
        ))
        .expect("invalid contract fixture is still valid JSON");

    suite
        .validate(REQUEST_SCHEMA, &valid)
        .expect("valid request fixture should pass");
    assert!(suite.validate(REQUEST_SCHEMA, &invalid).is_err());
}

#[test]
fn schema_resolution_never_retrieves_an_unembedded_uri() {
    let suite = ContractSuite::new();
    let instance = suite
        .parse_strict_json(r#"{"status":"ok"}"#)
        .expect("test instance should parse");

    let error = suite
        .validate(
            "https://example.invalid/not-embedded.schema.json",
            &instance,
        )
        .expect_err("missing schema must fail without network resolution");

    assert!(error.to_string().contains("offline schema registry"));
}

#[test]
fn embedded_schema_manifest_is_complete_and_unique() {
    let ids = ContractSuite::new().embedded_schema_ids();
    assert_eq!(ids.len(), 6);
    assert_eq!(
        ids,
        vec![
            "https://apppilotkit.dev/protocol/v1.1/envelope.schema.json",
            "https://apppilotkit.dev/protocol/v1.1/session.schema.json",
            "https://apppilotkit.dev/protocol/v1.1/ui.schema.json",
            "https://apppilotkit.dev/protocol/v1/disclosure.schema.json",
            "https://apppilotkit.dev/protocol/v1/envelope.schema.json",
            "https://apppilotkit.dev/protocol/v1/session.schema.json",
        ]
    );
}
