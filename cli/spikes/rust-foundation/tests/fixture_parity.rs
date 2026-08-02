use apppilotkit_rust_foundation_spike::ContractSuite;
use std::path::PathBuf;

#[test]
fn every_protocol_fixture_matches_its_schema_and_semantic_expectation() {
    let protocol_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../protocol");
    let report = ContractSuite::new()
        .verify_fixtures(&protocol_root)
        .expect("fixture verification should run");

    assert_eq!(report.checked, 83);
    assert!(
        report.is_success(),
        "fixture verification failures: {:#?}",
        report.failures
    );
}
