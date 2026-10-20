//! Resolves `$ref`s against the registry: `urn:pg-jsonschema-registry:<name>:<version>`
//! and any `$id` URI recorded in `jsonschema.ids`. No network or filesystem access.

use std::sync::{Arc, Mutex};

pub const URN_PREFIX: &str = "urn:pg-jsonschema-registry:";

/// Parses `urn:pg-jsonschema-registry:<name>:<version>`; name is everything
/// between the prefix and the last `:`.
pub fn parse_urn(uri: &str) -> Option<(String, i32)> {
    let rest = uri.strip_prefix(URN_PREFIX)?;
    let (name, version) = rest.rsplit_once(':')?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), version.parse().ok()?))
}

/// Returns the URI under which a schema's top-level identifier is recorded in
/// `jsonschema.ids`, or `None` if it has none. The identifier is `id` for draft 4 and
/// `$id` otherwise. A trailing empty fragment (`#`) is dropped; a non-empty fragment or
/// a relative URI is an error. The result is the RFC 3986 syntax-normalized form
/// (lowercase scheme and host, default port and dot segments removed), because the
/// validator normalizes every `$ref` the same way before it asks the retriever.
pub fn top_level_id(schema: &serde_json::Value) -> Result<Option<String>, String> {
    let key = match jsonschema::Draft::default().detect(schema) {
        jsonschema::Draft::Draft4 => "id",
        _ => "$id",
    };
    let Some(raw) = schema.get(key).and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let stripped = raw.strip_suffix('#').unwrap_or(raw);
    if stripped.contains('#') {
        return Err("top-level $id must not contain a fragment".into());
    }
    if !has_scheme(stripped) {
        return Err("top-level $id must be an absolute URI".into());
    }
    let uri = jsonschema::uri::from_str(stripped)
        .map_err(|e| format!("invalid top-level $id {raw}: {e}"))?;
    Ok(Some(uri.as_str().to_string()))
}

/// RFC 3986 `scheme ":"` prefix: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`.
fn has_scheme(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

pub struct RegistryRetriever {
    pub seen: Arc<Mutex<Vec<i64>>>,
}

impl jsonschema::Retrieve for RegistryRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let full = uri.as_str();
        let base = full.split('#').next().unwrap_or(full);
        let row = parse_urn(base)
            .and_then(|(name, version)| crate::registry::lookup(&name, version))
            .or_else(|| crate::registry::lookup_by_uri(base));
        match row {
            Some(row) => {
                self.seen.lock().unwrap().push(row.id);
                Ok(row.schema)
            }
            None => Err(format!("schema not found in registry: {full}").into()),
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn parse_urn_accepts_name_and_version() {
        assert_eq!(
            crate::retriever::parse_urn("urn:pg-jsonschema-registry:order:3"),
            Some(("order".into(), 3))
        );
        assert_eq!(
            crate::retriever::parse_urn("urn:pg-jsonschema-registry:com.acme:order:3"),
            Some(("com.acme:order".into(), 3))
        );
        assert_eq!(
            crate::retriever::parse_urn("urn:pg-jsonschema-registry:order"),
            None
        );
        assert_eq!(crate::retriever::parse_urn("https://x.example/order"), None);
    }

    #[pg_test]
    fn top_level_id_normalizes_and_rejects() {
        use crate::retriever::top_level_id;
        use serde_json::json;
        let ok = |v| top_level_id(&v).unwrap();
        assert_eq!(ok(json!({})), None);
        assert_eq!(ok(json!(true)), None);
        assert_eq!(
            ok(json!({"$id": "http://example.com/a.json#"})),
            Some("http://example.com/a.json".into())
        );
        assert_eq!(
            ok(json!({"$id": "HTTPS://Example.COM:443/x/../y"})),
            Some("https://example.com/y".into())
        );
        assert_eq!(ok(json!({"$id": "urn:acme:s"})), Some("urn:acme:s".into()));
        // draft 4 uses `id`; `$id` means nothing there
        let d4 = "http://json-schema.org/draft-04/schema#";
        assert_eq!(
            ok(json!({"$schema": d4, "id": "http://example.com/d4#"})),
            Some("http://example.com/d4".into())
        );
        assert_eq!(
            ok(json!({"$schema": d4, "$id": "http://example.com/d4"})),
            None
        );
        assert_eq!(ok(json!({"id": "http://example.com/x"})), None);
        let err = |v| top_level_id(&v).unwrap_err();
        assert!(err(json!({"$id": "a.json"})).contains("absolute URI"));
        assert!(err(json!({"$id": "/a.json"})).contains("absolute URI"));
        assert!(err(json!({"$id": "1http://x"})).contains("absolute URI"));
        assert!(err(json!({"$id": "http://x/a#foo"})).contains("fragment"));
        assert!(err(json!({"$id": "#foo"})).contains("fragment"));
    }
}
