use apppilotkit_rust_foundation_spike::ContractSuite;
use std::path::PathBuf;

#[test]
fn every_protocol_contract_case_matches_the_node_suite() {
    let protocol_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../protocol");
    let report = ContractSuite::new()
        .verify_repository(&protocol_root)
        .expect("repository verification should run");

    assert_eq!(report.checked, 104);
    assert!(
        report.is_success(),
        "repository verification failures: {:#?}",
        report.failures
    );
}
