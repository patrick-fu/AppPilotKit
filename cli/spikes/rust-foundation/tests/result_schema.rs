use apppilotkit_rust_foundation_spike::{SpikeOutcome, SpikeResult, spike_result_schema};
use serde_json::json;

#[test]
fn spike_owned_result_schema_matches_the_checked_in_golden() {
    let generated = spike_result_schema().expect("spike result schema should serialize");
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("golden/spike-result.schema.json"))
            .expect("golden schema should be JSON");

    assert_eq!(generated, golden);
    jsonschema::draft202012::meta::validate(&generated)
        .expect("generated schema should be valid Draft 2020-12");

    let validator =
        jsonschema::draft202012::new(&generated).expect("generated result schema should compile");
    let valid = serde_json::to_value(SpikeResult {
        schema_version: 1,
        outcome: SpikeOutcome::Succeeded,
        summary: "offline fixture parity passed".to_owned(),
    })
    .expect("result should serialize");
    assert!(validator.is_valid(&valid));
    assert!(!validator.is_valid(&json!({
        "schema_version": 1,
        "outcome": "unknown",
        "summary": "bad outcome"
    })));
}
