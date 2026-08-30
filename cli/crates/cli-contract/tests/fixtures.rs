use jsonschema::Retrieve;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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
fn fixture_manifest_is_exhaustive_and_every_case_has_the_expected_validity() {
    let fixture_root = contract_root().join("fixtures");
    let manifest: Value = serde_json::from_slice(
        &fs::read(fixture_root.join("cases.json")).expect("fixture manifest is checked in"),
    )
    .expect("fixture manifest is JSON");
    let cases = manifest["cases"].as_array().expect("manifest cases array");

    let declared = cases
        .iter()
        .map(|case| case["fixture"].as_str().expect("fixture path").to_owned())
        .collect::<BTreeSet<_>>();
    let discovered = discover_json_fixtures(&fixture_root);
    assert_eq!(
        declared, discovered,
        "manifest must list every fixture once"
    );
    assert_eq!(cases.len(), declared.len(), "fixture paths must be unique");

    let registry = schema_registry();
    for case in cases {
        let fixture = case["fixture"].as_str().expect("fixture path");
        let schema_id = case["schema"].as_str().expect("schema id");
        let expected_valid = case["valid"].as_bool().expect("valid boolean");
        let instance: Value = serde_json::from_slice(
            &fs::read(fixture_root.join(fixture)).expect("declared fixture exists"),
        )
        .expect("fixture is JSON");
        let validator = jsonschema::draft202012::options()
            .with_registry(&registry)
            .with_retriever(RejectExternalReferences)
            .build(&serde_json::json!({"$ref": schema_id}))
            .expect("fixture validator compiles offline");
        assert_eq!(
            validator.is_valid(&instance),
            expected_valid,
            "unexpected validity for {fixture} against {schema_id}"
        );
    }
}

#[test]
fn fixture_manifest_covers_the_frozen_contract_dimensions() {
    let manifest: Value = serde_json::from_slice(
        &fs::read(contract_root().join("fixtures/cases.json"))
            .expect("fixture manifest is checked in"),
    )
    .expect("fixture manifest is JSON");
    let cases = manifest["cases"].as_array().expect("manifest cases array");

    assert_eq!(
        coverage(cases, "status"),
        set(["cancelled", "failed", "succeeded"])
    );
    assert_eq!(
        coverage(cases, "output_mode"),
        set(["human", "json", "jsonl"])
    );
    assert_eq!(
        coverage(cases, "exit_code"),
        set(["0", "1", "130", "2", "3", "4", "5", "6", "7"])
    );
    assert_eq!(
        coverage(cases, "side_effect"),
        set([
            "app_mutation",
            "device_mutation",
            "local_write",
            "read_only"
        ])
    );
    assert_eq!(
        coverage(cases, "retry_safety"),
        set([
            "requires_artifact_conflict_policy",
            "requires_idempotency_key",
            "safe",
            "unsafe_after_ambiguous_result",
        ])
    );
    assert_eq!(
        coverage(cases, "shape"),
        set([
            "artifact",
            "disclosure_complete",
            "disclosure_truncated",
            "error",
            "next_action",
        ])
    );
}

fn contract_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/v1")
}

fn discover_json_fixtures(root: &Path) -> BTreeSet<String> {
    ["valid", "invalid"]
        .into_iter()
        .flat_map(|directory| {
            fs::read_dir(root.join(directory))
                .expect("fixture directory exists")
                .map(move |entry| {
                    let entry = entry.expect("fixture directory entry");
                    format!("{directory}/{}", entry.file_name().to_string_lossy())
                })
        })
        .filter(|path| path.ends_with(".json"))
        .collect()
}

fn schema_registry() -> jsonschema::Registry<'static> {
    let mut registry = jsonschema::Registry::new().retriever(RejectExternalReferences);
    let mut schemas = BTreeMap::new();
    for source in SCHEMAS {
        let schema: Value = serde_json::from_str(source).expect("checked-in schema JSON");
        let id = schema["$id"].as_str().expect("schema id").to_owned();
        assert!(schemas.insert(id.clone(), schema.clone()).is_none());
        registry = registry.add(id, schema).expect("schema registers");
    }
    registry.prepare().expect("offline registry prepares")
}

fn coverage(cases: &[Value], dimension: &str) -> BTreeSet<String> {
    cases
        .iter()
        .filter(|case| case["valid"] == true)
        .filter_map(|case| case["coverage"].get(dimension))
        .flat_map(|value| match value {
            Value::Array(values) => values.clone(),
            value => vec![value.clone()],
        })
        .map(|value| match value {
            Value::String(value) => value,
            Value::Number(value) => value.to_string(),
            _ => panic!("coverage values must be strings or numbers"),
        })
        .collect()
}

fn set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}
