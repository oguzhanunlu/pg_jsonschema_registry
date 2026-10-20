//! SQL-facing functions in the `jsonschema` schema.

use pgrx::prelude::*;

#[pg_schema]
mod jsonschema {
    use crate::{compile, registry};
    use pgrx::prelude::*;
    use pgrx::JsonB;

    #[pg_extern(volatile, strict, parallel_unsafe, name = "register")]
    fn register_next(name: &str, schema: JsonB) -> i32 {
        do_register(name, None, schema.0)
    }

    #[pg_extern(volatile, strict, parallel_unsafe, name = "register")]
    fn register_version(name: &str, version: i32, schema: JsonB) -> i32 {
        do_register(name, Some(version), schema.0)
    }

    fn fail(code: PgSqlErrorCode, msg: String) -> ! {
        pgrx::pg_sys::panic::ErrorReport::new(code, msg, function_name!()).report(PgLogLevel::ERROR);
        unreachable!("ERROR does not return")
    }

    fn do_register(name: &str, version: Option<i32>, schema: serde_json::Value) -> i32 {
        const BAD: PgSqlErrorCode = PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE;
        if name.is_empty() {
            fail(BAD, "name must not be empty".into());
        }
        registry::lock_for_register();
        let max = registry::max_version(name);
        let version = match version {
            None => max + 1,
            Some(v) if v <= 0 => fail(BAD, "version must be positive".into()),
            Some(v) if v <= max => fail(BAD, format!("version must be greater than {max}")),
            Some(v) => v,
        };
        if let Err(e) = ::jsonschema::meta::validate(&schema) {
            fail(BAD, format!("invalid JSON Schema: {e}"));
        }
        let compiled = match compile::compile(&schema) {
            Ok(c) => c,
            Err(e) => fail(BAD, format!("cannot compile schema: {e}")),
        };
        let id = registry::insert_schema(name, version, &schema, compiled.draft);
        if let Some(uri) = schema.get("$id").and_then(|v| v.as_str()) {
            if let Err(msg) = registry::insert_id(uri, id) {
                fail(PgSqlErrorCode::ERRCODE_DUPLICATE_OBJECT, msg);
            }
        }
        for r in compiled.refs.iter().filter(|r| **r != id) {
            registry::insert_ref(id, *r);
        }
        version
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::pg_sys::panic::CaughtError;
    use pgrx::prelude::*;

    fn reg(sql: &str) -> Result<Option<i32>, pgrx::spi::Error> { Spi::get_one::<i32>(sql) }

    #[pg_test]
    fn register_assigns_sequential_versions() {
        assert_eq!(reg("SELECT jsonschema.register('order', '{\"type\":\"object\"}')"), Ok(Some(1)));
        assert_eq!(reg("SELECT jsonschema.register('order', '{\"type\":\"object\"}')"), Ok(Some(2)));
        assert_eq!(reg("SELECT jsonschema.register('order', 10, '{\"type\":\"object\"}')"), Ok(Some(10)));
        assert_eq!(reg("SELECT jsonschema.register('order', '{}')"), Ok(Some(11)));
        assert_eq!(Spi::get_one::<String>("SELECT draft FROM jsonschema.registry WHERE name='order' AND version=1"), Ok(Some("2020-12".into())));
    }

    /// Runs `sql` expecting an ERROR and returns its message. (`#[pg_test(error = ..)]`
    /// compares the whole message exactly in pgrx 0.19.3, so substring checks go here.)
    fn error_message(sql: &str) -> String {
        PgTryBuilder::new(|| {
            reg(sql).unwrap();
            panic!("expected an error from: {sql}");
        })
        .catch_others(|e| match e {
            CaughtError::PostgresError(r) | CaughtError::ErrorReport(r) | CaughtError::RustPanic { ereport: r, .. } => {
                r.message().to_string()
            }
        })
        .execute()
    }

    #[pg_test]
    fn register_accepts_boolean_schema() {
        assert_eq!(reg("SELECT jsonschema.register('any', 'true')"), Ok(Some(1)));
    }

    #[pg_test(error = "version must be greater than 2")]
    fn register_rejects_version_not_above_max() {
        reg("SELECT jsonschema.register('o', 2, '{}')").unwrap();
        reg("SELECT jsonschema.register('o', 2, '{}')").unwrap();
    }

    #[pg_test(error = "version must be positive")]
    fn register_rejects_zero_version() { reg("SELECT jsonschema.register('o', 0, '{}')").unwrap(); }

    #[pg_test(error = "name must not be empty")]
    fn register_rejects_empty_name() { reg("SELECT jsonschema.register('', '{}')").unwrap(); }

    #[pg_test]
    fn register_rejects_schema_failing_metaschema() {
        let msg = error_message("SELECT jsonschema.register('bad', '{\"type\":\"nope\"}')");
        assert!(msg.contains("invalid JSON Schema"), "{msg}");
    }

    #[pg_test]
    fn register_rejects_unresolvable_ref() {
        let msg = error_message("SELECT jsonschema.register('bad', '{\"$ref\":\"https://nowhere.example/x\"}')");
        assert!(msg.contains("cannot compile schema"), "{msg}");
    }

    #[pg_test]
    fn register_rejects_duplicate_id() {
        reg("SELECT jsonschema.register('a', '{\"$id\":\"https://x.example/s\"}')").unwrap();
        let msg = error_message("SELECT jsonschema.register('b', '{\"$id\":\"https://x.example/s\"}')");
        assert!(msg.contains("already registered by a/1"), "{msg}");
    }

    #[pg_test]
    fn register_records_refs_and_id() {
        reg("SELECT jsonschema.register('address', '{\"$id\":\"https://acme.example/address\",\"type\":\"object\"}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"properties\":{\"a\":{\"$ref\":\"https://acme.example/address\"},\"b\":{\"$ref\":\"urn:pg-jsonschema-registry:address:1\"}}}')").unwrap();
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.refs"), Ok(Some(1)));
        assert_eq!(Spi::get_one::<String>("SELECT id_uri FROM jsonschema.schemas WHERE name='address'"), Ok(Some("https://acme.example/address".into())));
        assert_eq!(Spi::get_one::<Vec<String>>("SELECT refs FROM jsonschema.schemas WHERE name='order'"), Ok(Some(vec!["address/1".into()])));
    }

    #[pg_test]
    fn register_sqlstate_for_bad_version_is_22023() {
        let code = PgTryBuilder::new(|| { reg("SELECT jsonschema.register('o', -1, '{}')").unwrap(); None })
            .catch_when(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, |_| Some(()))
            .execute();
        assert_eq!(code, Some(()));
    }
}
