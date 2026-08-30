use apppilotkit_cli_contract::{CliConfig, CliCore};
use serde_json::Value;

const MACHINE_RESULT_ID: &str = "https://apppilotkit.dev/cli/v1/machine-result.schema.json";

#[test]
fn agent_can_list_and_show_the_exact_embedded_schemas() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");

    let listed = core.run(["fixture-cli", "schema", "list", "--output", "json"]);
    assert_eq!(listed.exit_code, 0);
    assert!(listed.stderr.is_empty());
    let listed: Value = serde_json::from_slice(&listed.stdout).expect("schema list result");
    let ids = listed["data"]["schemas"]
        .as_array()
        .expect("schema identifiers")
        .iter()
        .map(|id| id.as_str().expect("schema identifier"))
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 9);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(ids.contains(&MACHINE_RESULT_ID));

    let shown = core.run([
        "fixture-cli",
        "schema",
        "show",
        MACHINE_RESULT_ID,
        "--output",
        "json",
    ]);
    assert_eq!(shown.exit_code, 0);
    assert!(shown.stderr.is_empty());
    let shown: Value = serde_json::from_slice(&shown.stdout).expect("schema show result");
    assert_eq!(shown["command"], serde_json::json!(["schema", "show"]));
    assert_eq!(shown["data"]["schema_id"], MACHINE_RESULT_ID);
    let embedded: Value = serde_json::from_str(include_str!(
        "../../../contracts/v1/schema/machine-result.schema.json"
    ))
    .expect("checked-in schema JSON");
    assert_eq!(shown["data"]["schema"], embedded);
}

#[test]
fn every_result_schema_identifier_from_capabilities_is_showable_offline() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");
    let capabilities = core.run(["fixture-cli", "capabilities", "--output", "json"]);
    let capabilities: Value =
        serde_json::from_slice(&capabilities.stdout).expect("capability manifest result");

    for schema_id in capabilities["data"]["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .filter_map(|command| command["result_schema_id"].as_str())
    {
        let shown = core.run([
            "fixture-cli",
            "schema",
            "show",
            schema_id,
            "--output",
            "json",
        ]);
        assert_eq!(shown.exit_code, 0, "result schema is showable: {schema_id}");
        let shown: Value = serde_json::from_slice(&shown.stdout).expect("schema show result");
        assert_eq!(shown["data"]["schema_id"], schema_id);
        assert!(shown["data"]["schema"].is_object());
    }
}

#[test]
fn schema_show_rejects_non_schema_fragments_without_panicking_or_echoing_input() {
    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");
    let scalar_fragment = format!("{MACHINE_RESULT_ID}#/title");
    let output = core.run([
        "fixture-cli",
        "schema",
        "show",
        &scalar_fragment,
        "--output",
        "json",
    ]);

    assert_eq!(output.exit_code, 2);
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).expect("structured usage failure");
    assert_eq!(result["error"]["kind"], "cli.invalidInvocation");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("#/title"));
}
