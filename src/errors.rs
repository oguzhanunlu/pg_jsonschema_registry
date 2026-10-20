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

/// The keyword of `err`: `"false"` for a `false` schema (whose path ends in the
/// property or index that holds it, not in a keyword), else the last path segment.
fn keyword_of<'a>(err: &jsonschema::ValidationError<'_>, schema_path: &'a str) -> &'a str {
    match err.kind() {
        jsonschema::error::ValidationErrorKind::FalseSchema => "false",
        _ => keyword(schema_path),
    }
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
                "keyword": keyword_of(&err, &schema_path),
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

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn keyword_is_the_last_schema_path_segment() {
        use crate::errors::keyword;
        assert_eq!(keyword(""), "");
        assert_eq!(keyword("/a/b"), "b");
        assert_eq!(keyword("/properties/a~1b/minimum"), "minimum");
    }

    #[pg_test]
    fn false_schema_error_has_keyword_false() {
        Spi::get_one::<i32>("SELECT jsonschema.register('f', '{\"properties\":{\"a\":false}}')")
            .unwrap();
        let e = Spi::get_one::<pgrx::JsonB>("SELECT jsonschema.errors('{\"a\":1}', 'f', 1)")
            .unwrap()
            .unwrap();
        assert_eq!(e.0[0]["keyword"], "false", "{}", e.0);
        assert_eq!(e.0[0]["schema_path"], "/properties/a", "{}", e.0);
    }
}
