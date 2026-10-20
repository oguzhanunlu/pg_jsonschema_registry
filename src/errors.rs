//! Shaping validation errors into JSON values.

use crate::raw::RawJsonb;
use jsonschema::json::Jsonb;
use jsonschema::Validator;
use pgrx::prelude::*;
use serde_json::{json, Value};

/// Last `/`-separated segment of a schema path, or `""` for the root.
fn keyword(schema_path: &str) -> &str {
    schema_path.rsplit('/').next().unwrap_or("")
}

/// Collects every validation error of `doc` as
/// `{instance_path, schema_path, keyword, message}` objects.
pub fn collect(validator: &Validator<Jsonb>, doc: &RawJsonb) -> Vec<Value> {
    let out: Vec<Value> = validator
        .iter_errors(Jsonb::root(doc.as_bytes()))
        .map(|err| {
            let schema_path = err.schema_path().to_string();
            json!({
                "instance_path": err.instance_path().to_string(),
                "keyword": keyword(&schema_path),
                "schema_path": schema_path,
                "message": err.to_string(),
            })
        })
        .collect();
    if let Some(m) = jsonschema::json::jsonb::take_pending_error() {
        error!("{m}");
    }
    out
}
