//! Offline validation for the negotiated Protocol v1.2 Semantic Catalog wire contract.

use jsonschema::Retrieve;
use serde_json::Value;

const SCHEMA_SOURCES: &[&str] = &[
    include_str!("../../../../protocol/v1/schema/disclosure.schema.json"),
    include_str!("../../../../protocol/v1/schema/envelope.schema.json"),
    include_str!("../../../../protocol/v1.1/schema/envelope.schema.json"),
    include_str!("../../../../protocol/v1.2/schema/envelope.schema.json"),
    include_str!("../../../../protocol/v1.2/schema/semantic.schema.json"),
];

const SEMANTIC_SCHEMA: &str = "https://apppilotkit.dev/protocol/v1.2/semantic.schema.json";
const ENVELOPE_SCHEMA: &str = "https://apppilotkit.dev/protocol/v1.2/envelope.schema.json";

#[derive(Debug)]
pub(crate) struct ProtocolContractCatalog {
    registry: jsonschema::Registry<'static>,
}

impl ProtocolContractCatalog {
    pub(crate) fn new() -> Result<Self, String> {
        let mut registry = jsonschema::Registry::new().retriever(RejectExternalReferences);
        for source in SCHEMA_SOURCES {
            let schema: Value = serde_json::from_str(source).map_err(|error| error.to_string())?;
            jsonschema::draft202012::meta::validate(&schema).map_err(|error| error.to_string())?;
            let id = schema["$id"]
                .as_str()
                .ok_or_else(|| "embedded protocol schema has no $id".to_owned())?
                .to_owned();
            registry = registry
                .add(id, schema)
                .map_err(|error| error.to_string())?;
        }
        Ok(Self {
            registry: registry.prepare().map_err(|error| error.to_string())?,
        })
    }

    pub(crate) fn validate_request(&self, method: &str, request: &Value) -> Result<(), String> {
        let request_definition = match method {
            "semantic.list" => "listRequest",
            "semantic.show" => "showRequest",
            "semantic.schema" => "schemaRequest",
            "semantic.query" => "queryRequest",
            "semantic.invoke" => "invokeRequest",
            _ => return Err("unsupported Semantic Catalog method".to_owned()),
        };
        self.validate_schema(
            &serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$ref": format!("{SEMANTIC_SCHEMA}#/$defs/{request_definition}")
            }),
            request,
        )
    }

    pub(crate) fn validate_response(&self, method: &str, response: &Value) -> Result<(), String> {
        let success = match method {
            "semantic.list" => "listSuccess",
            "semantic.show" => "showSuccess",
            "semantic.schema" => "schemaSuccess",
            "semantic.query" => "querySuccess",
            "semantic.invoke" => "invokeSuccess",
            _ => return Err("unsupported Semantic Catalog method".to_owned()),
        };
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "oneOf": [
                {"$ref": format!("{SEMANTIC_SCHEMA}#/$defs/{success}")},
                {"$ref": format!("{ENVELOPE_SCHEMA}#/$defs/error")}
            ]
        });
        self.validate_schema(&schema, response)
    }

    fn validate_schema(&self, schema: &Value, instance: &Value) -> Result<(), String> {
        let validator = jsonschema::draft202012::options()
            .with_registry(&self.registry)
            .with_retriever(RejectExternalReferences)
            .build(schema)
            .map_err(|error| error.to_string())?;
        validator
            .validate(instance)
            .map_err(|error| error.to_string())
    }
}

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
