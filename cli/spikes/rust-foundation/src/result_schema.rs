use schemars::{JsonSchema, generate::SchemaSettings};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize, JsonSchema)]
pub struct SpikeResult {
    pub schema_version: u32,
    pub outcome: SpikeOutcome,
    pub summary: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum SpikeOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

pub fn spike_result_schema() -> Result<Value, serde_json::Error> {
    let schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<SpikeResult>();
    serde_json::to_value(schema)
}
