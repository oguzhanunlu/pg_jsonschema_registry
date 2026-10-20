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
}
