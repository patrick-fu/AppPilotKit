use crate::registry::SCHEMA_IDS;
use jsonschema::Retrieve;
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;

const SCHEMA_SOURCES: &[&str] = &[
    include_str!("../../../contracts/v1/schema/artifact.schema.json"),
    include_str!("../../../contracts/v1/schema/capability-manifest.schema.json"),
    include_str!("../../../contracts/v1/schema/disclosure.schema.json"),
    include_str!("../../../contracts/v1/schema/discovery.schema.json"),
    include_str!("../../../contracts/v1/schema/error.schema.json"),
    include_str!("../../../contracts/v1/schema/jsonl-event.schema.json"),
    include_str!("../../../contracts/v1/schema/machine-result.schema.json"),
    include_str!("../../../contracts/v1/schema/next-action.schema.json"),
];

#[derive(Debug)]
pub(crate) struct ContractCatalog {
    registry: jsonschema::Registry<'static>,
    schemas: BTreeMap<String, Value>,
}

impl ContractCatalog {
    pub(crate) fn new() -> Result<Self, String> {
        let mut registry = jsonschema::Registry::new().retriever(RejectExternalReferences);
        let mut schemas = BTreeMap::new();
        for source in SCHEMA_SOURCES {
            let schema = parse_strict_json(source).map_err(|error| error.to_string())?;
            jsonschema::draft202012::meta::validate(&schema).map_err(|error| error.to_string())?;
            if schema["$schema"] != "https://json-schema.org/draft/2020-12/schema" {
                return Err("embedded CLI schema is not Draft 2020-12".to_owned());
            }
            let id = schema["$id"]
                .as_str()
                .ok_or_else(|| "embedded CLI schema has no $id".to_owned())?
                .to_owned();
            if schemas.insert(id.clone(), schema.clone()).is_some() {
                return Err(format!("duplicate embedded CLI schema $id: {id}"));
            }
            registry = registry
                .add(id, schema)
                .map_err(|error| error.to_string())?;
        }
        if schemas
            .keys()
            .map(String::as_str)
            .ne(SCHEMA_IDS.iter().copied())
        {
            return Err("embedded CLI schemas do not match the command registry".to_owned());
        }
        let registry = registry.prepare().map_err(|error| error.to_string())?;
        Ok(Self { registry, schemas })
    }

    pub(crate) fn schema_ids(&self) -> impl Iterator<Item = &str> {
        self.schemas.keys().map(String::as_str)
    }

    pub(crate) fn schema(&self, id: &str) -> Option<&Value> {
        let candidate = if let Some(schema) = self.schemas.get(id) {
            schema
        } else {
            let (base_id, fragment) = id.split_once('#')?;
            let schema = self.schemas.get(base_id)?;
            if fragment.is_empty() {
                schema
            } else if fragment.starts_with('/') {
                schema.pointer(fragment)?
            } else {
                return None;
            }
        };
        candidate.is_object().then_some(candidate)
    }

    pub(crate) fn validate(&self, schema_id: &str, instance: &Value) -> Result<(), String> {
        let validator = jsonschema::draft202012::options()
            .with_registry(&self.registry)
            .with_retriever(RejectExternalReferences)
            .build(&serde_json::json!({"$ref": schema_id}))
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

fn parse_strict_json(input: &str) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = StrictValueSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
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

#[cfg(test)]
mod tests {
    use super::parse_strict_json;

    #[test]
    fn embedded_contract_parser_rejects_duplicate_keys_at_every_depth() {
        for source in [
            r#"{"$id":"first","$id":"second"}"#,
            r#"{"properties":{"status":true,"status":false}}"#,
            r#"[{"required":[],"required":["status"]}]"#,
        ] {
            assert!(parse_strict_json(source).is_err());
        }
        assert!(parse_strict_json(r#"{"properties":{"status":true}}"#).is_ok());
    }
}
