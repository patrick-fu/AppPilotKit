use serde::Deserialize;
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub enum ContractError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Schema(String),
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::Schema(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ContractError {}

#[derive(Debug)]
pub struct ContractSuite {
    registry: jsonschema::Registry<'static>,
    schema_ids: HashSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ContractFailure {
    pub case: String,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ContractReport {
    pub checked: usize,
    pub failures: Vec<ContractFailure>,
}

impl ContractReport {
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }
}

impl ContractSuite {
    #[must_use]
    pub fn new() -> Self {
        Self::build().expect("embedded protocol schemas must form a valid offline registry")
    }

    pub fn parse_strict_json(&self, input: &str) -> Result<Value, ContractError> {
        parse_strict_json(input)
    }

    pub fn validate(&self, schema_uri: &str, instance: &Value) -> Result<(), ContractError> {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": schema_uri,
        });
        let validator = jsonschema::draft202012::options()
            .with_registry(&self.registry)
            .with_retriever(OfflineRetriever)
            .build(&schema)
            .map_err(|error| ContractError::Schema(error.to_string()))?;
        validator
            .validate(instance)
            .map_err(|error| ContractError::Schema(error.to_string()))
    }

    #[must_use]
    pub fn embedded_schema_ids(&self) -> Vec<String> {
        let mut ids = self.schema_ids.iter().cloned().collect::<Vec<_>>();
        ids.sort();
        ids
    }

    pub fn verify_fixtures(&self, protocol_root: &Path) -> Result<ContractReport, ContractError> {
        let mut report = ContractReport::default();
        for version in ["v1", "v1.1"] {
            let fixture_root = protocol_root.join(version).join("fixtures");
            let cases: Vec<FixtureCase> = read_typed_json(&fixture_root.join("cases.json"))?;
            report.checked += cases.len();

            let listed = cases
                .iter()
                .map(|contract_case| contract_case.fixture.clone())
                .collect::<HashSet<_>>();
            let discovered = discover_fixtures(&fixture_root)?;
            if listed != discovered {
                report.failures.push(ContractFailure {
                    case: format!("{version}/fixture-manifest"),
                    detail: format!("listed {listed:?}, discovered {discovered:?}"),
                });
            }

            for contract_case in cases {
                let fixture = read_json(&fixture_root.join(&contract_case.fixture))?;
                let schema_valid = self.validate(&contract_case.schema, &fixture).is_ok();
                let semantic_valid = match contract_case.semantic.as_deref() {
                    None => true,
                    Some("versionRange") => valid_version_range(&fixture),
                    Some("returnedItems") => valid_returned_items(&fixture),
                    Some("snapshotResult") => valid_snapshot_result(&fixture),
                    Some("inspectResult") => valid_inspect_result(&fixture),
                    Some(semantic) => {
                        report.failures.push(ContractFailure {
                            case: format!("{version}/{}", contract_case.name),
                            detail: format!("unknown semantic check: {semantic}"),
                        });
                        false
                    }
                };
                let exchange_valid = match contract_case.request_fixture.as_deref() {
                    Some(request_fixture) => {
                        let request = read_json(&fixture_root.join(request_fixture))?;
                        valid_ui_exchange(&request, &fixture)
                    }
                    None => true,
                };
                let actual = schema_valid && semantic_valid && exchange_valid;
                let expectations_match = contract_case
                    .expected_schema_valid
                    .is_none_or(|expected| expected == schema_valid)
                    && contract_case
                        .expected_semantic_valid
                        .is_none_or(|expected| expected == semantic_valid)
                    && contract_case
                        .expected_exchange_valid
                        .is_none_or(|expected| expected == exchange_valid);
                let semantic_invalid_passes_schema =
                    contract_case.semantic.is_none() || contract_case.valid || schema_valid;
                if actual != contract_case.valid
                    || !expectations_match
                    || !semantic_invalid_passes_schema
                {
                    report.failures.push(ContractFailure {
                        case: format!("{version}/{}", contract_case.name),
                        detail: format!(
                            "expected valid={}, schema={schema_valid}, semantic={semantic_valid}, exchange={exchange_valid}",
                            contract_case.valid
                        ),
                    });
                }
            }
        }
        Ok(report)
    }

    pub fn verify_repository(&self, protocol_root: &Path) -> Result<ContractReport, ContractError> {
        let mut report = self.verify_fixtures(protocol_root)?;
        let repository_schema_ids = discover_schema_ids(protocol_root)?;
        if repository_schema_ids != self.schema_ids {
            report.failures.push(ContractFailure {
                case: "schema-manifest".to_owned(),
                detail: format!(
                    "embedded {:?}, repository {:?}",
                    self.schema_ids, repository_schema_ids
                ),
            });
        }

        for version in ["v1", "v1.1"] {
            let cases = read_json(&protocol_root.join(version).join("negotiation-cases.json"))?;
            for negotiation_case in cases.as_array().into_iter().flatten() {
                record_case(
                    &mut report,
                    format!("{version}/negotiation/{}", case_name(negotiation_case)),
                    valid_negotiation(negotiation_case),
                    negotiation_case["valid"].as_bool() == Some(true),
                );
            }
        }

        let fixture_root = protocol_root.join("v1.1").join("fixtures");
        let cases = read_json(&protocol_root.join("v1.1/pagination-cases.json"))?;
        for pagination_case in cases.as_array().into_iter().flatten() {
            record_case(
                &mut report,
                format!("v1.1/pagination/{}", case_name(pagination_case)),
                valid_pagination_exchange(pagination_case, &fixture_root)?,
                pagination_case["valid"].as_bool() == Some(true),
            );
        }

        let cases = read_json(&protocol_root.join("v1.1/string-matching-cases.json"))?;
        for matching_case in cases.as_array().into_iter().flatten() {
            record_case(
                &mut report,
                format!("v1.1/string-matching/{}", case_name(matching_case)),
                matches_string_predicate(&matching_case["predicate"], &matching_case["candidate"]),
                matching_case["matches"].as_bool() == Some(true),
            );
        }

        let cases = read_json(&protocol_root.join("v1/disclosure-cases.json"))?;
        for disclosure_case in cases.as_array().into_iter().flatten() {
            record_case(
                &mut report,
                format!("v1/disclosure/{}", case_name(disclosure_case)),
                valid_applied_limits(disclosure_case),
                disclosure_case["valid"].as_bool() == Some(true),
            );
        }

        Ok(report)
    }

    fn build() -> Result<Self, ContractError> {
        let mut registry = jsonschema::Registry::new().retriever(OfflineRetriever);
        let mut schema_ids = HashSet::new();
        for schema_source in EMBEDDED_SCHEMAS {
            let schema = parse_strict_json(schema_source)?;
            jsonschema::draft202012::meta::validate(&schema)
                .map_err(|error| ContractError::Schema(error.to_string()))?;
            if schema["$schema"] != "https://json-schema.org/draft/2020-12/schema" {
                return Err(ContractError::Schema(
                    "embedded schema is not Draft 2020-12".to_owned(),
                ));
            }
            let schema_id = schema
                .get("$id")
                .and_then(Value::as_str)
                .ok_or_else(|| ContractError::Schema("embedded schema has no $id".to_owned()))?
                .to_owned();
            if !schema_ids.insert(schema_id.clone()) {
                return Err(ContractError::Schema(format!(
                    "duplicate embedded schema $id: {schema_id}"
                )));
            }
            registry = registry
                .add(schema_id, schema)
                .map_err(|error| ContractError::Schema(error.to_string()))?;
        }
        let registry = registry
            .prepare()
            .map_err(|error| ContractError::Schema(error.to_string()))?;
        let expected = EXPECTED_SCHEMA_IDS
            .iter()
            .map(ToString::to_string)
            .collect::<HashSet<_>>();
        if schema_ids != expected {
            return Err(ContractError::Schema(format!(
                "embedded schema IDs do not match the expected manifest: {schema_ids:?}"
            )));
        }
        Ok(Self {
            registry,
            schema_ids,
        })
    }
}

impl Default for ContractSuite {
    fn default() -> Self {
        Self::new()
    }
}

const EMBEDDED_SCHEMAS: &[&str] = &[
    include_str!("../../../../protocol/v1/schema/disclosure.schema.json"),
    include_str!("../../../../protocol/v1/schema/envelope.schema.json"),
    include_str!("../../../../protocol/v1/schema/session.schema.json"),
    include_str!("../../../../protocol/v1.1/schema/envelope.schema.json"),
    include_str!("../../../../protocol/v1.1/schema/session.schema.json"),
    include_str!("../../../../protocol/v1.1/schema/ui.schema.json"),
];

const EXPECTED_SCHEMA_IDS: &[&str] = &[
    "https://apppilotkit.dev/protocol/v1/disclosure.schema.json",
    "https://apppilotkit.dev/protocol/v1/envelope.schema.json",
    "https://apppilotkit.dev/protocol/v1/session.schema.json",
    "https://apppilotkit.dev/protocol/v1.1/envelope.schema.json",
    "https://apppilotkit.dev/protocol/v1.1/session.schema.json",
    "https://apppilotkit.dev/protocol/v1.1/ui.schema.json",
];

#[derive(Debug)]
struct OfflineRetriever;

impl jsonschema::Retrieve for OfflineRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("offline schema registry blocked retrieval: {uri}").into())
    }
}

fn parse_strict_json(input: &str) -> Result<Value, ContractError> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = StrictValueSeed
        .deserialize(&mut deserializer)
        .map_err(ContractError::Json)?;
    deserializer.end().map_err(ContractError::Json)?;
    Ok(value)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureCase {
    name: String,
    schema: String,
    fixture: String,
    valid: bool,
    semantic: Option<String>,
    request_fixture: Option<String>,
    expected_schema_valid: Option<bool>,
    expected_semantic_valid: Option<bool>,
    expected_exchange_valid: Option<bool>,
}

fn read_json(path: &Path) -> Result<Value, ContractError> {
    let input = fs::read_to_string(path).map_err(ContractError::Io)?;
    parse_strict_json(&input)
}

fn read_typed_json<T>(path: &Path) -> Result<T, ContractError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(read_json(path)?).map_err(ContractError::Json)
}

fn discover_fixtures(fixture_root: &Path) -> Result<HashSet<String>, ContractError> {
    let mut fixtures = HashSet::new();
    for directory in ["valid", "invalid"] {
        for entry in fs::read_dir(fixture_root.join(directory)).map_err(ContractError::Io)? {
            let entry = entry.map_err(ContractError::Io)?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                fixtures.insert(format!(
                    "{directory}/{}",
                    path.file_name()
                        .expect("directory entries have file names")
                        .to_string_lossy()
                ));
            }
        }
    }
    Ok(fixtures)
}

fn discover_schema_ids(protocol_root: &Path) -> Result<HashSet<String>, ContractError> {
    let mut ids = HashSet::new();
    for version in ["v1", "v1.1"] {
        for entry in
            fs::read_dir(protocol_root.join(version).join("schema")).map_err(ContractError::Io)?
        {
            let path = entry.map_err(ContractError::Io)?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let schema = read_json(&path)?;
                let schema_id = string_field(&schema, "$id").ok_or_else(|| {
                    ContractError::Schema(format!("schema has no $id: {}", path.display()))
                })?;
                if !ids.insert(schema_id.to_owned()) {
                    return Err(ContractError::Schema(format!(
                        "duplicate repository schema $id: {schema_id}"
                    )));
                }
            }
        }
    }
    Ok(ids)
}

fn record_case(report: &mut ContractReport, case: String, actual: bool, expected: bool) {
    report.checked += 1;
    if actual != expected {
        report.failures.push(ContractFailure {
            case,
            detail: format!("expected {expected}, got {actual}"),
        });
    }
}

fn case_name(contract_case: &Value) -> &str {
    string_field(contract_case, "name").unwrap_or("unnamed case")
}

fn valid_negotiation(contract_case: &Value) -> bool {
    let client = &contract_case["client"];
    let server = &contract_case["server"];
    let response = &contract_case["response"];
    let Some(lowest_minor) = u64_field(client, "minMinor")
        .zip(u64_field(server, "minMinor"))
        .map(|(client_minimum, server_minimum)| client_minimum.max(server_minimum))
    else {
        return false;
    };
    let Some(highest_minor) = u64_field(client, "maxMinor")
        .zip(u64_field(server, "maxMinor"))
        .map(|(client_maximum, server_maximum)| client_maximum.min(server_maximum))
    else {
        return false;
    };
    let Some(available) = string_array(&server["capabilitiesByMinor"][highest_minor.to_string()])
    else {
        return false;
    };
    let Some(response_capabilities) = string_array(&response["capabilities"]) else {
        return false;
    };
    let required = string_array(&client["requiredCapabilities"]).unwrap_or_default();
    let available = available.into_iter().collect::<HashSet<_>>();
    let response_capabilities = response_capabilities.into_iter().collect::<HashSet<_>>();

    client["requestId"] == response["requestId"]
        && client["major"] == server["major"]
        && lowest_minor <= highest_minor
        && response["major"] == client["major"]
        && u64_field(response, "minor") == Some(highest_minor)
        && required
            .iter()
            .all(|capability| response_capabilities.contains(capability))
        && response_capabilities == available
}

fn valid_applied_limits(contract_case: &Value) -> bool {
    let negotiated = &contract_case["negotiated"];
    let requested = &contract_case["requested"];
    let applied = &contract_case["applied"];
    let requested_max_items =
        u64_field(requested, "maxItems").or_else(|| u64_field(negotiated, "maxPageItems"));
    let requested_max_bytes =
        u64_field(requested, "maxBytes").or_else(|| u64_field(negotiated, "maxResponseBytes"));

    u64_field(applied, "maxItems")
        .zip(requested_max_items)
        .zip(u64_field(negotiated, "maxPageItems"))
        .is_some_and(|((applied, requested), negotiated)| {
            applied <= requested && applied <= negotiated
        })
        && u64_field(applied, "maxBytes")
            .zip(requested_max_bytes)
            .zip(u64_field(negotiated, "maxResponseBytes"))
            .is_some_and(|((applied, requested), negotiated)| {
                applied <= requested && applied <= negotiated
            })
}

fn matches_string_predicate(predicate: &Value, candidate: &Value) -> bool {
    let Some(expected) = string_field(predicate, "value") else {
        return false;
    };
    let Some(candidate) = candidate.as_str() else {
        return false;
    };
    let case_sensitive = predicate["caseSensitive"].as_bool() == Some(true);
    let (expected, candidate) = if case_sensitive {
        (expected.to_owned(), candidate.to_owned())
    } else {
        (
            expected.to_ascii_lowercase(),
            candidate.to_ascii_lowercase(),
        )
    };
    match string_field(predicate, "operator") {
        Some("exact") => candidate == expected,
        Some("prefix") => candidate.starts_with(&expected),
        Some("suffix") => candidate.ends_with(&expected),
        Some("contains") => candidate.contains(&expected),
        _ => false,
    }
}

fn valid_pagination_exchange(exchange: &Value, fixture_root: &Path) -> Result<bool, ContractError> {
    let initial_request = read_json(&fixture_root.join(case_path(exchange, "initialRequest")?))?;
    let initial_response = read_json(&fixture_root.join(case_path(exchange, "initialResponse")?))?;
    let continuation_request =
        read_json(&fixture_root.join(case_path(exchange, "continuationRequest")?))?;
    let final_response = read_json(&fixture_root.join(case_path(exchange, "finalResponse")?))?;

    let initial_nodes = initial_response["result"]["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let final_nodes = final_response["result"]["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let first_nodes = initial_nodes
        .iter()
        .filter_map(|node| string_field(node, "ref").map(|reference| (reference, node)))
        .collect::<HashMap<_, _>>();
    let first_refs = first_nodes.keys().copied().collect::<HashSet<_>>();
    let new_final_refs = final_nodes
        .iter()
        .filter_map(|node| string_field(node, "ref"))
        .filter(|reference| !first_refs.contains(reference))
        .count();
    let all_refs = initial_nodes
        .iter()
        .chain(final_nodes)
        .filter_map(|node| string_field(node, "ref"))
        .collect::<HashSet<_>>();
    let repeated_nodes_are_immutable = final_nodes.iter().all(|node| {
        string_field(node, "ref").is_some_and(|reference| {
            first_nodes
                .get(reference)
                .is_none_or(|initial_node| *initial_node == node)
        })
    });
    let is_snapshot = string_field(&initial_request, "method") == Some("ui.snapshot");
    let initial_response_matches = if is_snapshot {
        valid_snapshot_exchange(&initial_request, &initial_response, true)
    } else {
        valid_inspect_exchange(&initial_request, &initial_response, true, false)
    };
    let final_response_matches = if is_snapshot {
        valid_snapshot_exchange(&initial_request, &final_response, false)
    } else {
        valid_inspect_exchange(&initial_request, &final_response, false, false)
    };
    let method_specific_result = if is_snapshot {
        final_response["result"]["sources"] == initial_response["result"]["sources"]
            && final_response["result"]["selection"] == initial_response["result"]["selection"]
            && u64_field(&initial_response["result"]["selection"], "selectedNodes")
                == Some(all_refs.len() as u64)
    } else {
        string_array(&initial_request["params"]["target"]["refs"]).is_none_or(|requested| {
            let mut matched =
                string_array(&initial_response["result"]["matchedRefs"]).unwrap_or_default();
            matched
                .extend(string_array(&final_response["result"]["matchedRefs"]).unwrap_or_default());
            same_set(&requested, &matched)
        })
    };
    let continuation_params = continuation_request["params"].as_object();

    Ok(initial_response_matches
        && final_response_matches
        && initial_response["result"]["page"]["truncated"] == true
        && continuation_request["params"]["cursor"]
            == initial_response["result"]["page"]["nextCursor"]
        && continuation_request["params"]["snapshot"] == initial_response["result"]["snapshot"]
        && continuation_params.is_some_and(|params| {
            params.len() == 2 && params.contains_key("cursor") && params.contains_key("snapshot")
        })
        && continuation_request["method"] == initial_request["method"]
        && continuation_request["context"] == initial_request["context"]
        && continuation_request["id"] == final_response["id"]
        && final_response["result"]["snapshot"] == initial_response["result"]["snapshot"]
        && final_response["result"]["page"]["truncated"] == false
        && new_final_refs > 0
        && repeated_nodes_are_immutable
        && method_specific_result)
}

fn case_path<'a>(contract_case: &'a Value, field: &str) -> Result<&'a str, ContractError> {
    string_field(contract_case, field)
        .ok_or_else(|| ContractError::Schema(format!("contract case has no {field}")))
}

fn valid_version_range(message: &Value) -> bool {
    let protocol = &message["params"]["protocol"];
    u64_field(protocol, "minMinor")
        .zip(u64_field(protocol, "maxMinor"))
        .is_some_and(|(minimum, maximum)| minimum <= maximum)
}

fn valid_returned_items(page: &Value) -> bool {
    u64_field(page, "returnedItems")
        .zip(u64_field(&page["appliedLimits"], "maxItems"))
        .is_some_and(|(returned, maximum)| returned <= maximum)
}

fn unique_node_refs(nodes: &[Value]) -> bool {
    let refs = nodes
        .iter()
        .filter_map(|node| string_field(node, "ref"))
        .collect::<HashSet<_>>();
    refs.len() == nodes.len()
}

fn valid_page(message: &Value, nodes: &[Value], page: &Value) -> bool {
    let Ok(serialized) = serde_json::to_vec(message) else {
        return false;
    };
    u64_field(page, "returnedItems") == Some(nodes.len() as u64)
        && valid_returned_items(page)
        && u64_field(&page["appliedLimits"], "maxBytes")
            .is_some_and(|maximum| serialized.len() as u64 <= maximum)
}

fn valid_snapshot_result(message: &Value) -> bool {
    let result = &message["result"];
    let Some(nodes) = result["nodes"].as_array() else {
        return false;
    };
    let Some(sources) = result["sources"].as_array() else {
        return false;
    };
    let nodes_by_ref = nodes
        .iter()
        .filter_map(|node| string_field(node, "ref").map(|reference| (reference, node)))
        .collect::<HashMap<_, _>>();
    let sources_by_id = sources
        .iter()
        .filter_map(|source| string_field(source, "id").map(|identifier| (identifier, source)))
        .collect::<HashMap<_, _>>();
    let source_order = sources
        .iter()
        .enumerate()
        .filter_map(|(index, source)| {
            string_field(source, "id").map(|identifier| (identifier, index))
        })
        .collect::<HashMap<_, _>>();
    let source_ids = sources
        .iter()
        .filter_map(|source| string_field(source, "id"))
        .collect::<Vec<_>>();
    let root_refs = sources
        .iter()
        .filter_map(|source| string_field(source, "rootRef"))
        .collect::<Vec<_>>();
    let platforms = sources
        .iter()
        .filter_map(|source| string_field(source, "platform"))
        .collect::<HashSet<_>>();
    if !unique_node_refs(nodes)
        || !valid_page(message, nodes, &result["page"])
        || source_ids.len() != sources.len()
        || source_ids.iter().copied().collect::<HashSet<_>>().len() != source_ids.len()
        || root_refs.len() != sources.len()
        || root_refs.iter().copied().collect::<HashSet<_>>().len() != root_refs.len()
        || platforms.len() != 1
    {
        return false;
    }

    let selection = &result["selection"];
    let Some(selected_nodes) = u64_field(selection, "selectedNodes") else {
        return false;
    };
    let Some(total_nodes) = u64_field(selection, "totalNodes") else {
        return false;
    };
    if selected_nodes > total_nodes
        || selected_nodes < sources.len() as u64
        || nodes.len() as u64 > selected_nodes
    {
        return false;
    }
    let criteria = selection["criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    if string_field(selection, "mode") == Some("full") {
        if selected_nodes != total_nodes || criteria != HashSet::from(["all"]) {
            return false;
        }
    } else if criteria != HashSet::from(["root", "visible", "interactive", "ancestor"]) {
        return false;
    }

    for source in sources {
        let Some(root) =
            string_field(source, "rootRef").and_then(|reference| nodes_by_ref.get(reference))
        else {
            return false;
        };
        if string_field(root, "sourceId") != string_field(source, "id")
            || u64_field(root, "depth") != Some(0)
            || has_value(root, "parentRef")
        {
            return false;
        }
    }

    let positions = nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| {
            string_field(node, "ref").map(|reference| (reference, position))
        })
        .collect::<HashMap<_, _>>();
    let mut sibling_positions = HashSet::new();
    let mut last_sibling_index = HashMap::<&str, u64>::new();
    let mut ancestry = Vec::<&str>::new();
    let mut active_source = None::<usize>;
    for (position, node) in nodes.iter().enumerate() {
        let Some(source_id) = string_field(node, "sourceId") else {
            return false;
        };
        let Some(node_source) = source_order.get(source_id).copied() else {
            return false;
        };
        if active_source.is_some_and(|active| node_source < active) {
            return false;
        }
        if active_source != Some(node_source) {
            active_source = Some(node_source);
            ancestry.clear();
        }
        let Some(depth) = u64_field(node, "depth").map(|depth| depth as usize) else {
            return false;
        };
        let Some(reference) = string_field(node, "ref") else {
            return false;
        };
        if depth == 0 {
            if has_value(node, "parentRef") || has_value(node, "childIndex") {
                return false;
            }
            ancestry.clear();
            ancestry.push(reference);
            continue;
        }
        let Some(parent_ref) = string_field(node, "parentRef") else {
            return false;
        };
        let Some(child_index) = u64_field(node, "childIndex") else {
            return false;
        };
        let Some(parent) = nodes_by_ref.get(parent_ref) else {
            return false;
        };
        let sibling_position = (parent_ref, child_index);
        if child_index >= u64_field(parent, "childCount").unwrap_or_default()
            || string_field(parent, "sourceId") != Some(source_id)
            || u64_field(parent, "depth").map(|value| value + 1) != Some(depth as u64)
            || positions
                .get(parent_ref)
                .is_none_or(|parent_position| *parent_position >= position)
            || ancestry.get(depth - 1).copied() != Some(parent_ref)
            || sibling_positions.contains(&sibling_position)
            || last_sibling_index
                .get(parent_ref)
                .is_some_and(|previous| child_index <= *previous)
        {
            return false;
        }
        sibling_positions.insert(sibling_position);
        last_sibling_index.insert(parent_ref, child_index);
        ancestry.truncate(depth);
        ancestry.push(reference);
    }

    sources.iter().all(|source| {
        let source_id = string_field(source, "id");
        nodes
            .iter()
            .filter(|node| {
                string_field(node, "sourceId") == source_id && u64_field(node, "depth") == Some(0)
            })
            .count()
            == 1
    }) && sources_by_id.len() == sources.len()
}

fn valid_inspect_result(message: &Value) -> bool {
    let result = &message["result"];
    let Some(nodes) = result["nodes"].as_array() else {
        return false;
    };
    if !unique_node_refs(nodes) || !valid_page(message, nodes, &result["page"]) {
        return false;
    }
    let refs = nodes
        .iter()
        .filter_map(|node| string_field(node, "ref"))
        .collect::<HashSet<_>>();
    let positions = nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| {
            string_field(node, "ref").map(|reference| (reference, position))
        })
        .collect::<HashMap<_, _>>();
    let nodes_by_ref = nodes
        .iter()
        .filter_map(|node| string_field(node, "ref").map(|reference| (reference, node)))
        .collect::<HashMap<_, _>>();
    let mut sibling_positions = HashSet::new();
    let mut last_sibling_index = HashMap::<&str, u64>::new();
    let mut seen_sources = HashSet::new();
    let mut roots_by_source = HashSet::new();
    let mut ancestry = Vec::<Option<&str>>::new();
    let mut active_source = None::<&str>;
    let mut active_source_nodes = 0_usize;
    for node in nodes {
        let Some(source_id) = string_field(node, "sourceId") else {
            return false;
        };
        if active_source != Some(source_id) {
            if seen_sources.contains(source_id) {
                return false;
            }
            active_source = Some(source_id);
            active_source_nodes = 0;
            seen_sources.insert(source_id);
            ancestry.clear();
        }
        let Some(depth) = u64_field(node, "depth").map(|depth| depth as usize) else {
            return false;
        };
        let Some(reference) = string_field(node, "ref") else {
            return false;
        };
        if depth == 0 && (has_value(node, "parentRef") || has_value(node, "childIndex")) {
            return false;
        }
        if depth == 0 {
            if active_source_nodes > 0 || roots_by_source.contains(source_id) {
                return false;
            }
            roots_by_source.insert(source_id);
            ancestry.push(Some(reference));
            active_source_nodes += 1;
            continue;
        }
        let Some(parent_ref) = string_field(node, "parentRef") else {
            return false;
        };
        let Some(child_index) = u64_field(node, "childIndex") else {
            return false;
        };
        let sibling_position = (parent_ref, child_index);
        if sibling_positions.contains(&sibling_position)
            || last_sibling_index
                .get(parent_ref)
                .is_some_and(|previous| child_index <= *previous)
        {
            return false;
        }
        sibling_positions.insert(sibling_position);
        last_sibling_index.insert(parent_ref, child_index);
        if let Some(parent) = nodes_by_ref.get(parent_ref)
            && (child_index >= u64_field(parent, "childCount").unwrap_or_default()
                || string_field(parent, "sourceId") != Some(source_id)
                || u64_field(parent, "depth").map(|value| value + 1) != Some(depth as u64)
                || positions.get(parent_ref) >= positions.get(reference)
                || ancestry.get(depth - 1).copied().flatten() != Some(parent_ref))
        {
            return false;
        }
        ancestry.truncate(depth);
        while ancestry.len() < depth {
            ancestry.push(None);
        }
        ancestry.push(Some(reference));
        active_source_nodes += 1;
    }

    result["matchedRefs"].as_array().is_some_and(|matched| {
        matched
            .iter()
            .filter_map(Value::as_str)
            .all(|reference| refs.contains(reference))
    })
}

fn valid_ui_exchange(request: &Value, response: &Value) -> bool {
    match string_field(request, "method") {
        Some("ui.snapshot") => valid_snapshot_exchange(request, response, true),
        Some("ui.inspect") => valid_inspect_exchange(request, response, true, true),
        _ => false,
    }
}

fn valid_snapshot_exchange(request: &Value, response: &Value, correlate_id: bool) -> bool {
    let requested_selection = string_field(&request["params"], "selection").unwrap_or("agent");
    let requested_providers = string_array(&request["params"]["providers"]);
    let response_providers = response["result"]["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|source| string_field(source, "provider"))
        .collect::<HashSet<_>>();
    (!correlate_id || request["id"] == response["id"])
        && string_field(&response["result"]["selection"], "mode") == Some(requested_selection)
        && requested_providers.is_none_or(|providers| {
            providers.into_iter().collect::<HashSet<_>>() == response_providers
        })
        && valid_requested_limits(request, response)
        && valid_detail(request, response)
}

fn valid_inspect_exchange(
    request: &Value,
    response: &Value,
    correlate_id: bool,
    correlate_refs: bool,
) -> bool {
    let requested_refs = string_array(&request["params"]["target"]["refs"]);
    (!correlate_id || request["id"] == response["id"])
        && request["params"]["snapshot"] == response["result"]["snapshot"]
        && (!correlate_refs
            || requested_refs.is_none_or(|refs| {
                string_array(&response["result"]["matchedRefs"])
                    .is_some_and(|matched| same_set(&refs, &matched))
            }))
        && valid_requested_limits(request, response)
        && valid_detail(request, response)
}

fn valid_requested_limits(request: &Value, response: &Value) -> bool {
    let requested = &request["params"]["limits"];
    let applied = &response["result"]["page"]["appliedLimits"];
    u64_field(requested, "maxItems")
        .is_none_or(|maximum| u64_field(applied, "maxItems").is_some_and(|value| value <= maximum))
        && u64_field(requested, "maxBytes").is_none_or(|maximum| {
            u64_field(applied, "maxBytes").is_some_and(|value| value <= maximum)
        })
}

fn valid_detail(request: &Value, response: &Value) -> bool {
    let detail = string_field(&request["params"], "detail").unwrap_or("compact");
    let contains_native = response["result"]["nodes"]
        .as_array()
        .is_some_and(|nodes| nodes.iter().any(|node| has_value(node, "native")));
    detail == "native" || !contains_native
}

fn same_set(left: &[&str], right: &[&str]) -> bool {
    left.len() == right.len() && left.iter().all(|value| right.contains(value))
}

fn string_array(value: &Value) -> Option<Vec<&str>> {
    value
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn u64_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}

fn has_value(value: &Value, field: &str) -> bool {
    value.get(field).is_some_and(|value| !value.is_null())
}

struct StrictValueSeed;

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON numbers must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        StrictValueSeed.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or_default());
        while let Some(value) = sequence.next_element_seed(StrictValueSeed)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object key: {key}"
                )));
            }
            values.insert(key, object.next_value_seed(StrictValueSeed)?);
        }
        Ok(Value::Object(values))
    }
}
