use apppilotkit_cli_contract::{CliConfig, CliCore};
use serde_json::Value;

#[test]
fn doctor_checks_only_the_local_contract_and_skips_uninstalled_adapters() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");

    let output = core.run([
        "fixture-cli",
        "doctor",
        "--output",
        "json",
        "--non-interactive",
    ]);

    assert_eq!(output.exit_code, 0);
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("doctor Machine Result");
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["command"], serde_json::json!(["doctor"]));
    let checks = result["data"]["checks"].as_array().expect("doctor checks");
    assert_eq!(
        checks
            .iter()
            .map(|check| (
                check["id"].as_str().unwrap(),
                check["status"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("cli.runtime", "passed"),
            ("contract.schemas", "passed"),
            ("credentials", "skipped"),
            ("device.connection", "skipped"),
            ("platform.android_tools", "unavailable"),
            ("platform.apple_tools", "unavailable"),
            ("transport", "skipped"),
        ]
    );
    assert_eq!(result["disclosure"]["returned_items"], checks.len());
    assert!(result["next_actions"].as_array().is_some_and(Vec::is_empty));
}
