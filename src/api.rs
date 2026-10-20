//! SQL-facing functions in the `jsonschema` schema.

use pgrx::prelude::*;

/// Raises an ERROR with `code` and `msg`; never returns.
pub(crate) fn fail(code: PgSqlErrorCode, msg: String) -> ! {
    pgrx::pg_sys::panic::ErrorReport::new(code, msg, function_name!()).report(PgLogLevel::ERROR);
    unreachable!("ERROR does not return")
}

#[pg_schema]
mod jsonschema {
    use super::fail;
    use crate::raw::RawJsonb;
    use crate::{cache, compile, registry};
    use jsonschema::json::Jsonb;
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

    #[pg_extern(stable, strict, parallel_safe)]
    fn valid(doc: RawJsonb, name: &str, version: i32) -> bool {
        cache::get_or_compile(name, version).is_valid(Jsonb::root(doc.as_bytes()))
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
        cache::invalidate();
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

    #[pg_test]
    fn valid_answers_true_and_false() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\",\"required\":[\"id\"],\"properties\":{\"id\":{\"type\":\"integer\"}}}')").unwrap();
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('{\"id\":1}', 'p', 1)"), Ok(Some(true)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('{\"id\":\"x\"}', 'p', 1)"), Ok(Some(false)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)"), Ok(Some(false)));
    }

    #[pg_test(error = "schema p version 9 is not registered")]
    fn valid_raises_for_unknown_schema() {
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 9)").unwrap();
    }

    #[pg_test]
    fn valid_compiles_once_per_backend() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{\"a\":1}', 'p', 1)").unwrap();
        Spi::get_one::<i64>("SELECT count(*) FROM generate_series(1,100) g WHERE jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 1);
    }

    #[pg_test]
    fn register_invalidates_warm_cache_in_same_backend() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"), Ok(Some(false)));
        let before = crate::cache::compile_count();
        Spi::run("DELETE FROM jsonschema.registry WHERE name='p'").unwrap();   // unregister() arrives in Task 7
        crate::cache::invalidate();
        reg("SELECT jsonschema.register('p', 1, '{\"type\":\"array\"}')").unwrap();
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"), Ok(Some(true)));
        assert_eq!(crate::cache::compile_count(), before + 1);
    }

    #[pg_test(error = "schema p version 1 is not registered")]
    fn delete_in_warm_backend_makes_valid_raise() {
        reg("SELECT jsonschema.register('p', '{}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::run("DELETE FROM jsonschema.registry WHERE name='p'").unwrap();
        crate::cache::invalidate();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
    }

    #[pg_test]
    fn valid_follows_cross_schema_ref_with_fragment() {
        reg("SELECT jsonschema.register('address', '{\"$id\":\"https://acme.example/address\",\"$defs\":{\"street\":{\"type\":\"string\",\"minLength\":1}}}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"properties\":{\"street\":{\"$ref\":\"https://acme.example/address#/$defs/street\"}}}')").unwrap();
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('{\"street\":\"Main\"}', 'order', 1)"), Ok(Some(true)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('{\"street\":\"\"}', 'order', 1)"), Ok(Some(false)));
    }

    #[pg_test]
    fn valid_handles_toasted_documents() {
        reg("SELECT jsonschema.register('big', '{\"type\":\"object\",\"properties\":{\"blob\":{\"type\":\"string\",\"minLength\":100000}}}')").unwrap();
        reg("SELECT jsonschema.register('obj', '{\"type\":\"object\",\"required\":[\"blob\"]}')").unwrap();
        // Out-of-line, uncompressed (EXTERNAL), plus a small inline row with a 1-byte header.
        Spi::run("CREATE TABLE docs(id int, d jsonb)").unwrap();
        Spi::run("ALTER TABLE docs ALTER d SET STORAGE EXTERNAL").unwrap();
        Spi::run("INSERT INTO docs VALUES (1, jsonb_build_object('blob', repeat('x', 200000)))").unwrap();
        Spi::run("INSERT INTO docs VALUES (2, jsonb_build_object('blob', repeat('y', 10)))").unwrap();
        let toast = Spi::get_one::<String>("SELECT reltoastrelid::regclass::text FROM pg_class WHERE oid = 'docs'::regclass").unwrap().unwrap();
        assert!(Spi::get_one::<i64>(&format!("SELECT count(*) FROM {toast}")).unwrap().unwrap() > 0, "big row is not out of line");
        assert_eq!(Spi::get_one::<bool>("SELECT pg_column_size(d) < 127 FROM docs WHERE id = 2"), Ok(Some(true)));
        // Compressed (default EXTENDED storage, repetitive data).
        Spi::run("CREATE TABLE docs_c(d jsonb)").unwrap();
        Spi::run("INSERT INTO docs_c SELECT jsonb_build_object('blob', repeat('z', 200000))").unwrap();
        assert_eq!(Spi::get_one::<bool>("SELECT pg_column_size(d) < octet_length(d::text) FROM docs_c"), Ok(Some(true)));

        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'big', 1) FROM docs WHERE id = 1"), Ok(Some(true)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'big', 1) FROM docs WHERE id = 2"), Ok(Some(false)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'obj', 1) FROM docs WHERE id = 2"), Ok(Some(true)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'big', 1) FROM docs_c"), Ok(Some(true)));
    }

    #[pg_test]
    fn foreign_invalidation_disables_cache_fill_for_the_transaction() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        // SAFETY: simulates Postgres delivering a foreign invalidation of the registry.
        unsafe { crate::cache::on_relcache_invalidation(pg_sys::Datum::from(0usize), crate::registry::relid()) };
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 2);
    }

    #[pg_test]
    fn own_invalidation_keeps_cache_fill() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::run("DELETE FROM jsonschema.registry WHERE name='p'").unwrap();   // unregister() arrives in Task 7
        crate::cache::invalidate();
        reg("SELECT jsonschema.register('p', 1, '{\"type\":\"array\"}')").unwrap();
        let before = crate::cache::compile_count();
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"), Ok(Some(true)));
        assert_eq!(Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"), Ok(Some(true)));
        assert_eq!(crate::cache::compile_count(), before + 1);
    }

    #[pg_test]
    fn subxact_abort_resets_own_invalidation_marker() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        // One command that runs register (queuing its own invalidation) and then fails,
        // so the subtransaction aborts before that command's local messages are processed.
        Spi::run(
            "DO $$ BEGIN \
               PERFORM 1 / (jsonschema.register('q', '{}') - 1); \
             EXCEPTION WHEN division_by_zero THEN NULL; END $$",
        )
        .unwrap();
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.registry WHERE name = 'q'"), Ok(Some(0)));
        // SAFETY: simulates Postgres delivering a foreign invalidation of the registry.
        unsafe { crate::cache::on_relcache_invalidation(pg_sys::Datum::from(0usize), crate::registry::relid()) };
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 2);
    }

    #[pg_test]
    fn unknown_relid_treats_any_relcache_message_as_relevant() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        crate::cache::reset_for_test();
        Spi::run("CREATE TABLE other_t(x int)").unwrap();
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 2);
        // SAFETY: simulates the end of the transaction; the callback ignores `arg`.
        unsafe { crate::cache::on_xact_end(pg_sys::XactEvent::XACT_EVENT_COMMIT, std::ptr::null_mut()) };
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 1);
    }

    #[pg_test]
    fn unrelated_relcache_invalidation_keeps_cache() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::run("CREATE TABLE other_t(x int)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 1);
    }
}
