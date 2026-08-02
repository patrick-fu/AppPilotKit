use apppilotkit_rust_foundation_spike::ContractSuite;

#[test]
fn strict_json_rejects_duplicate_keys_at_every_depth() {
    let suite = ContractSuite::new();

    assert!(
        suite
            .parse_strict_json(r#"{"same": 1, "same": 2}"#)
            .is_err()
    );
    assert!(
        suite
            .parse_strict_json(r#"{"nested": {"same": 1, "same": 2}}"#)
            .is_err()
    );
    assert!(
        suite
            .parse_strict_json(r#"[{"same": 1, "s\u0061me": 2}]"#)
            .is_err()
    );
    assert!(
        suite
            .parse_strict_json(r#"{"ok":true} {"extra":true}"#)
            .is_err()
    );
}

#[test]
fn strict_json_preserves_valid_json_values() {
    let parsed = ContractSuite::new()
        .parse_strict_json(r#"{"status":"ok","items":[1,true,null]}"#)
        .expect("valid JSON should parse");

    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["items"][0], 1);
}
