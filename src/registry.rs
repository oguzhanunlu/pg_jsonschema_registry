//! SQL access to the registry tables. This is the only module that runs SQL
//! against `jsonschema.registry`, `jsonschema.ids` and `jsonschema.refs`.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use pgrx::JsonB;

pub struct Row {
    pub id: i64,
    pub schema: serde_json::Value,
}

fn select_row(query: &str, args: &[DatumWithOid]) -> Option<Row> {
    Spi::connect(|client| {
        let table = client.select(query, Some(1), args)?;
        for row in table {
            let id = row.get::<i64>(1)?.expect("id is NOT NULL");
            let JsonB(schema) = row.get::<JsonB>(2)?.expect("schema is NOT NULL");
            return Ok::<_, pgrx::spi::Error>(Some(Row { id, schema }));
        }
        Ok(None)
    })
    .expect("registry lookup failed")
}

pub fn lookup(name: &str, version: i32) -> Option<Row> {
    select_row(
        "SELECT id, schema FROM jsonschema.registry WHERE name = $1 AND version = $2",
        &[DatumWithOid::from(name), DatumWithOid::from(version)],
    )
}

pub fn lookup_by_uri(uri: &str) -> Option<Row> {
    select_row(
        "SELECT r.id, r.schema FROM jsonschema.ids i \
         JOIN jsonschema.registry r ON r.id = i.schema_id WHERE i.uri = $1",
        &[DatumWithOid::from(uri)],
    )
}

pub fn max_version(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "SELECT COALESCE(max(version), 0)::int FROM jsonschema.registry WHERE name = $1",
        &[DatumWithOid::from(name)],
    )
    .expect("registry max_version failed")
    .unwrap_or(0)
}

pub fn insert_schema(name: &str, version: i32, schema: &serde_json::Value, draft: &str) -> i64 {
    Spi::get_one_with_args::<i64>(
        "INSERT INTO jsonschema.registry (name, version, schema, draft) \
         VALUES ($1, $2, $3, $4) RETURNING id",
        &[
            DatumWithOid::from(name),
            DatumWithOid::from(version),
            DatumWithOid::from(JsonB(schema.clone())),
            DatumWithOid::from(draft),
        ],
    )
    .expect("registry insert_schema failed")
    .expect("insert_schema returned no id")
}

/// Registers `uri` for `schema_id`. Pre-checks for an existing owner instead of
/// catching the unique violation. The check-then-insert is race-free only when
/// the caller holds `lock_for_register()` (SHARE ROW EXCLUSIVE), which `register`
/// does; a caller that bypasses the lock gets the raw unique violation.
pub fn insert_id(uri: &str, schema_id: i64) -> Result<(), String> {
    let owner = Spi::connect(|client| {
        let table = client.select(
            "SELECT r.name, r.version FROM jsonschema.ids i \
             JOIN jsonschema.registry r ON r.id = i.schema_id WHERE i.uri = $1",
            Some(1),
            &[DatumWithOid::from(uri)],
        )?;
        for row in table {
            let name = row.get::<String>(1)?.expect("name is NOT NULL");
            let version = row.get::<i32>(2)?.expect("version is NOT NULL");
            return Ok::<_, pgrx::spi::Error>(Some((name, version)));
        }
        Ok(None)
    })
    .expect("registry owner lookup failed");
    if let Some((name, version)) = owner {
        return Err(format!("$id {uri} is already registered by {name}/{version}"));
    }
    Spi::run_with_args(
        "INSERT INTO jsonschema.ids (uri, schema_id) VALUES ($1, $2)",
        &[DatumWithOid::from(uri), DatumWithOid::from(schema_id)],
    )
    .expect("registry insert_id failed");
    Ok(())
}

pub fn insert_ref(schema_id: i64, ref_id: i64) {
    Spi::run_with_args(
        "INSERT INTO jsonschema.refs (schema_id, ref_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        &[DatumWithOid::from(schema_id), DatumWithOid::from(ref_id)],
    )
    .expect("registry insert_ref failed");
}

pub fn dependents(id: i64) -> Vec<(String, i32)> {
    Spi::connect(|client| {
        let table = client.select(
            "SELECT r.name, r.version FROM jsonschema.refs f \
             JOIN jsonschema.registry r ON r.id = f.schema_id \
             WHERE f.ref_id = $1 ORDER BY r.name, r.version",
            None,
            &[DatumWithOid::from(id)],
        )?;
        let mut out = Vec::new();
        for row in table {
            let name = row.get::<String>(1)?.expect("name is NOT NULL");
            let version = row.get::<i32>(2)?.expect("version is NOT NULL");
            out.push((name, version));
        }
        Ok::<_, pgrx::spi::Error>(out)
    })
    .expect("registry dependents failed")
}

pub fn delete(name: &str, version: i32) -> Option<i64> {
    Spi::connect_mut(|client| {
        let table = client.update(
            "DELETE FROM jsonschema.registry WHERE name = $1 AND version = $2 RETURNING id",
            Some(1),
            &[DatumWithOid::from(name), DatumWithOid::from(version)],
        )?;
        for row in table {
            return Ok::<_, pgrx::spi::Error>(row.get::<i64>(1)?);
        }
        Ok(None)
    })
    .expect("registry delete failed")
}

pub fn lock_for_register() {
    Spi::run("LOCK TABLE jsonschema.registry IN SHARE ROW EXCLUSIVE MODE")
        .expect("registry lock failed");
}

pub fn relid() -> pg_sys::Oid {
    Spi::get_one::<pg_sys::Oid>("SELECT 'jsonschema.registry'::regclass::oid")
        .expect("registry relid failed")
        .expect("registry table not found")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn lookup_returns_none_for_unknown() {
        assert!(crate::registry::lookup("nope", 1).is_none());
        assert_eq!(crate::registry::max_version("nope"), 0);
    }

    #[pg_test]
    fn insert_then_lookup_roundtrips_schema_and_id() {
        let schema = serde_json::json!({"type": "object"});
        let id = crate::registry::insert_schema("order", 1, &schema, "2020-12");
        let row = crate::registry::lookup("order", 1).unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.schema, schema);
        assert_eq!(crate::registry::max_version("order"), 1);
    }

    #[pg_test]
    fn insert_id_rejects_duplicate_uri_with_owner_in_message() {
        let a = crate::registry::insert_schema("a", 1, &serde_json::json!({}), "2020-12");
        let b = crate::registry::insert_schema("b", 1, &serde_json::json!({}), "2020-12");
        assert!(crate::registry::insert_id("https://x.example/s", a).is_ok());
        let err = crate::registry::insert_id("https://x.example/s", b).unwrap_err();
        assert!(err.contains("a/1"), "{err}");
        assert_eq!(crate::registry::lookup_by_uri("https://x.example/s").unwrap().id, a);
    }

    #[pg_test]
    fn refs_dependents_and_delete() {
        let a = crate::registry::insert_schema("a", 1, &serde_json::json!({}), "2020-12");
        let b = crate::registry::insert_schema("b", 1, &serde_json::json!({}), "2020-12");
        crate::registry::insert_ref(b, a);
        assert_eq!(crate::registry::dependents(a), vec![("b".to_string(), 1)]);
        assert_eq!(crate::registry::delete("b", 1), Some(b));
        assert!(crate::registry::dependents(a).is_empty());
        assert_eq!(crate::registry::delete("b", 1), None);
    }

    #[pg_test]
    fn relid_is_the_registry_table() {
        let oid = crate::registry::relid();
        assert_eq!(Spi::get_one::<pg_sys::Oid>("SELECT 'jsonschema.registry'::regclass::oid"), Ok(Some(oid)));
    }
}
