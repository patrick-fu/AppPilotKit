use apppilotkit_cli_contract::{CliConfig, CliCore};
use jsonschema::Retrieve;
use serde_json::Value;
use std::collections::BTreeMap;

const SCHEMAS: &[&str] = &[
    include_str!("../../../contracts/v1/schema/artifact.schema.json"),
    include_str!("../../../contracts/v1/schema/capability-manifest.schema.json"),
    include_str!("../../../contracts/v1/schema/catalog.schema.json"),
    include_str!("../../../contracts/v1/schema/disclosure.schema.json"),
    include_str!("../../../contracts/v1/schema/discovery.schema.json"),
    include_str!("../../../contracts/v1/schema/error.schema.json"),
    include_str!("../../../contracts/v1/schema/jsonl-event.schema.json"),
    include_str!("../../../contracts/v1/schema/machine-result.schema.json"),
    include_str!("../../../contracts/v1/schema/next-action.schema.json"),
];

#[derive(Debug)]
struct RejectExternalReferences;

impl Retrieve for RejectExternalReferences {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("external schema retrieval is disabled: {uri}").into())
    }
}

#[test]
fn checked_in_schemas_are_unique_valid_and_accept_the_capabilities_result() {
    let mut registry = jsonschema::Registry::new().retriever(RejectExternalReferences);
    let mut schemas = BTreeMap::new();
    for source in SCHEMAS {
        let schema: Value = serde_json::from_str(source).expect("checked-in schema JSON");
        jsonschema::draft202012::meta::validate(&schema).expect("Draft 2020-12 schema");
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        let id = schema["$id"]
            .as_str()
            .expect("schema has an identifier")
            .to_owned();
        assert!(schemas.insert(id.clone(), schema.clone()).is_none());
        registry = registry.add(id, schema).expect("schema registers");
    }
    let registry = registry.prepare().expect("offline registry prepares");

    assert_eq!(
        schemas.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "https://apppilotkit.dev/cli/v1/artifact.schema.json",
            "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json",
            "https://apppilotkit.dev/cli/v1/catalog.schema.json",
            "https://apppilotkit.dev/cli/v1/disclosure.schema.json",
            "https://apppilotkit.dev/cli/v1/discovery.schema.json",
            "https://apppilotkit.dev/cli/v1/error.schema.json",
            "https://apppilotkit.dev/cli/v1/jsonl-event.schema.json",
            "https://apppilotkit.dev/cli/v1/machine-result.schema.json",
            "https://apppilotkit.dev/cli/v1/next-action.schema.json",
        ]
    );

    let core = CliCore::new(CliConfig::new("fixture-cli", "0.1.0"))
        .expect("embedded contract should initialize");
    let output = core.run(["fixture-cli", "capabilities", "--output", "json"]);
    let result: Value = serde_json::from_slice(&output.stdout).expect("machine result JSON");
    validate(
        &registry,
        "https://apppilotkit.dev/cli/v1/machine-result.schema.json",
        &result,
    );
    validate(
        &registry,
        "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json",
        &result["data"],
    );
    for vocabulary in ["output_modes", "side_effect_classes", "retry_safety_values"] {
        let mut incomplete = result["data"].clone();
        incomplete[vocabulary]
            .as_array_mut()
            .expect("capability vocabulary")
            .pop();
        assert!(
            !is_valid(
                &registry,
                "https://apppilotkit.dev/cli/v1/capability-manifest.schema.json",
                &incomplete,
            ),
            "capability vocabulary cannot be truncated: {vocabulary}"
        );
    }
}

fn validate(registry: &jsonschema::Registry, schema_id: &str, instance: &Value) {
    assert!(is_valid(registry, schema_id, instance));
}

fn is_valid(registry: &jsonschema::Registry, schema_id: &str, instance: &Value) -> bool {
    let validator = jsonschema::draft202012::options()
        .with_registry(registry)
        .with_retriever(RejectExternalReferences)
        .build(&serde_json::json!({"$ref": schema_id}))
        .expect("validator compiles offline");
    validator.is_valid(instance)
}
