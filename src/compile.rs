//! Schema compilation with registry-backed `$ref` resolution.

use crate::retriever::RegistryRetriever;
use jsonschema::json::Jsonb;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct Compiled {
    pub validator: jsonschema::Validator<Jsonb>,
    pub draft: &'static str,
    pub refs: Vec<i64>,
}

pub fn compile(schema: &serde_json::Value) -> Result<Compiled, String> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let validator = jsonschema::options_for::<Jsonb>()
        .with_retriever(RegistryRetriever {
            seen: Arc::clone(&seen),
        })
        .build(schema)
        .map_err(|e| e.to_string())?;
    let mut refs: Vec<i64> = Vec::new();
    for id in seen.lock().unwrap().iter() {
        if !refs.contains(id) {
            refs.push(*id);
        }
    }
    Ok(Compiled {
        draft: draft_label(validator.draft()),
        validator,
        refs,
    })
}

pub fn draft_label(d: jsonschema::Draft) -> &'static str {
    match d {
        jsonschema::Draft::Draft4 => "4",
        jsonschema::Draft::Draft6 => "6",
        jsonschema::Draft::Draft7 => "7",
        jsonschema::Draft::Draft201909 => "2019-09",
        jsonschema::Draft::Draft202012 => "2020-12",
        _ => "2020-12",
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn compile_resolves_ref_by_urn_and_records_it() {
        let addr_id = crate::registry::insert_schema(
            "address",
            1,
            &serde_json::json!({"type":"object","required":["city"]}),
            "2020-12",
        );
        let order = serde_json::json!({"properties":{"ship_to":{"$ref":"urn:pg-jsonschema-registry:address:1"}}});
        let c = crate::compile::compile(&order).unwrap();
        assert_eq!(c.refs, vec![addr_id]);
        assert_eq!(c.draft, "2020-12");
    }

    #[pg_test]
    fn compile_resolves_ref_by_id_uri_with_fragment() {
        let id = crate::registry::insert_schema(
            "address",
            1,
            &serde_json::json!({"$id":"https://acme.example/address","$defs":{"street":{"type":"string","minLength":1}}}),
            "2020-12",
        );
        crate::registry::insert_id("https://acme.example/address", id).unwrap();
        let s = serde_json::json!({"properties":{"street":{"$ref":"https://acme.example/address#/$defs/street"}}});
        let c = crate::compile::compile(&s).unwrap();
        assert_eq!(c.refs, vec![id]);
        assert_eq!(c.draft, "2020-12");
    }

    #[pg_test]
    fn compile_fails_on_unresolvable_ref() {
        let s = serde_json::json!({"$ref":"https://nowhere.example/x"});
        let err = crate::compile::compile(&s).unwrap_err();
        assert!(err.contains("https://nowhere.example/x"), "{err}");
    }

    #[pg_test]
    fn draft_is_detected_from_schema_keyword() {
        let s = serde_json::json!({"$schema":"http://json-schema.org/draft-04/schema#","type":"object"});
        assert_eq!(crate::compile::compile(&s).unwrap().draft, "4");
        let s = serde_json::json!({"$schema":"http://json-schema.org/draft-07/schema#"});
        assert_eq!(crate::compile::compile(&s).unwrap().draft, "7");
    }
}
