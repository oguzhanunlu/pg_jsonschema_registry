//! SQL-facing functions in the `jsonschema` schema.

use pgrx::prelude::*;

/// Raises an ERROR with `code` and `msg`; never returns.
pub(crate) fn fail(code: PgSqlErrorCode, msg: String) -> ! {
    pgrx::pg_sys::panic::ErrorReport::new(code, msg, function_name!()).report(PgLogLevel::ERROR);
    unreachable!("ERROR does not return")
}

/// Raises an ERROR with `code`, `msg` and a DETAIL line; never returns.
pub(crate) fn fail_with_detail(code: PgSqlErrorCode, msg: String, detail: String) -> ! {
    pgrx::pg_sys::panic::ErrorReport::new(code, msg, function_name!())
        .set_detail(detail)
        .report(PgLogLevel::ERROR);
    unreachable!("ERROR does not return")
}

/// Raises `54001` if `doc` nests deeper than [`crate::depth::MAX_DEPTH`].
fn check_depth(doc: &crate::raw::RawJsonb) {
    use crate::depth::{exceeds_depth, MAX_DEPTH};
    if exceeds_depth(doc.as_bytes(), MAX_DEPTH) {
        fail(
            PgSqlErrorCode::ERRCODE_STATEMENT_TOO_COMPLEX,
            format!("document nesting exceeds {MAX_DEPTH} levels"),
        );
    }
}

#[pg_schema]
mod jsonschema {
    use super::{check_depth, fail, fail_with_detail};
    use crate::raw::RawJsonb;
    use crate::{cache, compile, registry};
    use jsonschema::json::Jsonb;
    use pgrx::prelude::*;
    use pgrx::JsonB;

    #[pg_extern(volatile, strict, parallel_unsafe, name = "register")]
    fn register_next(name: &str, schema: RawJsonb) -> i32 {
        do_register(name, None, schema)
    }

    #[pg_extern(volatile, strict, parallel_unsafe, name = "register")]
    fn register_version(name: &str, version: i32, schema: RawJsonb) -> i32 {
        do_register(name, Some(version), schema)
    }

    #[pg_extern(stable, strict, parallel_safe)]
    fn valid(doc: RawJsonb, name: &str, version: i32) -> bool {
        check_depth(&doc);
        cache::get_or_compile(name, version).is_valid(Jsonb::root(doc.as_bytes()))
    }

    #[pg_extern(stable, strict, parallel_safe)]
    fn errors(doc: RawJsonb, name: &str, version: i32) -> JsonB {
        check_depth(&doc);
        let v = cache::get_or_compile(name, version);
        JsonB(serde_json::Value::Array(crate::errors::collect(&v, &doc)))
    }

    /// Errors listed in `validate`'s DETAIL; `errors()` returns all of them.
    const DETAIL_MAX_ERRORS: usize = 20;

    #[pg_extern(stable, strict, parallel_safe)]
    fn validate(doc: RawJsonb, name: &str, version: i32) -> bool {
        check_depth(&doc);
        let v = cache::get_or_compile(name, version);
        if v.is_valid(Jsonb::root(doc.as_bytes())) {
            return true;
        }
        let errs = crate::errors::collect(&v, &doc);
        let first = errs.first();
        let field = |k: &str| first.and_then(|e| e[k].as_str()).unwrap_or("").to_string();
        let message = format!(
            "document does not match schema {name} version {version}: {} at {}",
            field("message"),
            field("instance_path")
        );
        let total = errs.len();
        let shown: Vec<serde_json::Value> = errs.into_iter().take(DETAIL_MAX_ERRORS).collect();
        let mut detail = format!("{total} error(s): {}", serde_json::Value::Array(shown));
        if total > DETAIL_MAX_ERRORS {
            detail.push_str(&format!(" \u{2026} and {} more", total - DETAIL_MAX_ERRORS));
        }
        fail_with_detail(PgSqlErrorCode::ERRCODE_CHECK_VIOLATION, message, detail)
    }

    #[pg_extern(stable, strict, parallel_safe, name = "valid")]
    fn valid_latest(doc: RawJsonb, name: &str) -> bool {
        valid(doc, name, latest_version(name))
    }

    #[pg_extern(stable, strict, parallel_safe, name = "errors")]
    fn errors_latest(doc: RawJsonb, name: &str) -> JsonB {
        errors(doc, name, latest_version(name))
    }

    #[pg_extern(stable, strict, parallel_safe, name = "validate")]
    fn validate_latest(doc: RawJsonb, name: &str) -> bool {
        validate(doc, name, latest_version(name))
    }

    #[pg_extern(stable, strict, parallel_safe)]
    fn get(name: &str, version: i32) -> JsonB {
        match registry::lookup(name, version) {
            Some(row) => JsonB(row.schema),
            None => fail(
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
                format!("schema {name} version {version} is not registered"),
            ),
        }
    }

    #[pg_extern(stable, strict, parallel_safe)]
    fn latest_version(name: &str) -> i32 {
        match registry::max_version(name) {
            0 => fail(
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
                format!("schema {name} has no registered versions"),
            ),
            v => v,
        }
    }

    /// Per-backend cache counters: total compiles, cached validators, and how many
    /// registry invalidations cleared the cache. Numbers are for the calling backend only.
    #[pg_extern(stable, parallel_safe)]
    fn cache_stats() -> TableIterator<
        'static,
        (
            name!(compiles, i64),
            name!(entries, i32),
            name!(invalidations, i64),
        ),
    > {
        let clamp = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        TableIterator::once((
            clamp(cache::compile_count()),
            i32::try_from(cache::entry_count()).unwrap_or(i32::MAX),
            clamp(cache::invalidation_count()),
        ))
    }

    /// Queues an invalidation of every backend's validator cache (delivered at commit).
    /// Called by the statement triggers on the registry tables, so direct `INSERT` /
    /// `DELETE` also invalidate. Requires `INSERT` or `DELETE` on `jsonschema.registry`
    /// (42501 otherwise), so that roles that cannot change the registry cannot flush
    /// every backend's cache.
    #[pg_extern(volatile, parallel_unsafe)]
    fn invalidate_cache() {
        if !registry::may_modify() {
            fail(
                PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
                "permission denied: invalidate_cache requires INSERT or DELETE on jsonschema.registry"
                    .into(),
            );
        }
        cache::invalidate();
    }

    extension_sql!(
        r#"
CREATE FUNCTION jsonschema.forbid_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'jsonschema.registry rows are immutable; unregister and register a new version instead'
        USING ERRCODE = 'feature_not_supported';
END
$$;

CREATE TRIGGER registry_immutable BEFORE UPDATE ON jsonschema.registry
    FOR EACH ROW EXECUTE FUNCTION jsonschema.forbid_update();

CREATE FUNCTION jsonschema.on_registry_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM jsonschema.invalidate_cache();
    RETURN NULL;
END
$$;

CREATE TRIGGER registry_changed AFTER INSERT OR DELETE ON jsonschema.registry
    FOR EACH STATEMENT EXECUTE FUNCTION jsonschema.on_registry_change();
CREATE TRIGGER ids_changed AFTER INSERT OR DELETE ON jsonschema.ids
    FOR EACH STATEMENT EXECUTE FUNCTION jsonschema.on_registry_change();
CREATE TRIGGER refs_changed AFTER INSERT OR DELETE ON jsonschema.refs
    FOR EACH STATEMENT EXECUTE FUNCTION jsonschema.on_registry_change();
"#,
        name = "registry_triggers",
        requires = [invalidate_cache],
    );

    /// Deletes one version, refusing while another registered schema references it.
    /// Takes the same `SHARE ROW EXCLUSIVE` lock on `jsonschema.registry` as `register`,
    /// so concurrent `register` / `unregister` calls serialize (the dependency check
    /// cannot miss a reference added concurrently), and therefore needs the same
    /// privileges as `register`.
    #[pg_extern(volatile, strict, parallel_unsafe)]
    fn unregister(name: &str, version: i32) {
        registry::lock_for_register();
        let Some(row) = registry::lookup(name, version) else {
            fail(
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
                format!("schema {name} version {version} is not registered"),
            );
        };
        let deps = registry::dependents(row.id);
        if !deps.is_empty() {
            let list: Vec<String> = deps.iter().map(|(n, v)| format!("{n}/{v}")).collect();
            fail(
                PgSqlErrorCode::ERRCODE_DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "schema {name} version {version} is referenced by {}",
                    list.join(", ")
                ),
            );
        }
        if registry::delete(name, version).is_none() {
            // Removed concurrently (e.g. by a direct DELETE) after the lookup.
            fail(
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
                format!("schema {name} version {version} is not registered"),
            );
        }
        cache::invalidate();
    }

    fn do_register(name: &str, version: Option<i32>, raw: RawJsonb) -> i32 {
        use crate::depth::{exceeds_depth, SCHEMA_MAX_DEPTH};
        const BAD: PgSqlErrorCode = PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE;
        if name.is_empty() {
            fail(BAD, "name must not be empty".into());
        }
        // Checked on the jsonb bytes, before serde_json (which stops at 127 levels) runs.
        if exceeds_depth(raw.as_bytes(), SCHEMA_MAX_DEPTH) {
            fail(
                PgSqlErrorCode::ERRCODE_STATEMENT_TOO_COMPLEX,
                format!("schema nesting exceeds {SCHEMA_MAX_DEPTH} levels"),
            );
        }
        let schema = raw.to_value();
        registry::lock_for_register();
        let max = registry::max_version(name);
        let version = match version {
            None => max + 1,
            Some(v) if v <= 0 => fail(BAD, "version must be positive".into()),
            Some(v) if v <= max => fail(BAD, format!("version must be greater than {max}")),
            Some(v) => v,
        };
        let top_level_id = match crate::retriever::top_level_id(&schema) {
            Ok(id) => id,
            Err(msg) => fail(BAD, msg),
        };
        if ::jsonschema::Draft::default().detect(&schema) == ::jsonschema::Draft::Unknown {
            let uri = schema.get("$schema").and_then(|v| v.as_str()).unwrap_or("");
            fail(
                BAD,
                format!("unsupported $schema: {uri} (custom meta-schemas are not supported)"),
            );
        }
        if let Err(e) = ::jsonschema::meta::validate(&schema) {
            fail(BAD, format!("invalid JSON Schema: {e}"));
        }
        let compiled = match compile::compile(&schema) {
            Ok(c) => c,
            Err(e) => fail(BAD, format!("cannot compile schema: {e}")),
        };
        let id = registry::insert_schema(name, version, &schema, compiled.draft);
        if let Some(uri) = top_level_id {
            if let Err(msg) = registry::insert_id(&uri, id) {
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

    fn reg(sql: &str) -> Result<Option<i32>, pgrx::spi::Error> {
        Spi::get_one::<i32>(sql)
    }

    #[pg_test]
    fn register_assigns_sequential_versions() {
        assert_eq!(
            reg("SELECT jsonschema.register('order', '{\"type\":\"object\"}')"),
            Ok(Some(1))
        );
        assert_eq!(
            reg("SELECT jsonschema.register('order', '{\"type\":\"object\"}')"),
            Ok(Some(2))
        );
        assert_eq!(
            reg("SELECT jsonschema.register('order', 10, '{\"type\":\"object\"}')"),
            Ok(Some(10))
        );
        assert_eq!(
            reg("SELECT jsonschema.register('order', '{}')"),
            Ok(Some(11))
        );
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT draft FROM jsonschema.registry WHERE name='order' AND version=1"
            ),
            Ok(Some("2020-12".into()))
        );
    }

    /// Runs `sql` expecting an ERROR and returns its message. (`#[pg_test(error = ..)]`
    /// compares the whole message exactly in pgrx 0.19.3, so substring checks go here.)
    fn error_message(sql: &str) -> String {
        PgTryBuilder::new(|| {
            reg(sql).unwrap();
            panic!("expected an error from: {sql}");
        })
        .catch_others(|e| match e {
            CaughtError::PostgresError(r)
            | CaughtError::ErrorReport(r)
            | CaughtError::RustPanic { ereport: r, .. } => r.message().to_string(),
        })
        .execute()
    }

    #[pg_test]
    fn register_accepts_boolean_schema() {
        assert_eq!(
            reg("SELECT jsonschema.register('any', 'true')"),
            Ok(Some(1))
        );
    }

    #[pg_test(error = "version must be greater than 2")]
    fn register_rejects_version_not_above_max() {
        reg("SELECT jsonschema.register('o', 2, '{}')").unwrap();
        reg("SELECT jsonschema.register('o', 2, '{}')").unwrap();
    }

    #[pg_test(error = "version must be positive")]
    fn register_rejects_zero_version() {
        reg("SELECT jsonschema.register('o', 0, '{}')").unwrap();
    }

    #[pg_test(error = "name must not be empty")]
    fn register_rejects_empty_name() {
        reg("SELECT jsonschema.register('', '{}')").unwrap();
    }

    #[pg_test]
    fn register_rejects_schema_failing_metaschema() {
        let msg = error_message("SELECT jsonschema.register('bad', '{\"type\":\"nope\"}')");
        assert!(msg.contains("invalid JSON Schema"), "{msg}");
    }

    #[pg_test]
    fn register_rejects_unresolvable_ref() {
        let msg = error_message(
            "SELECT jsonschema.register('bad', '{\"$ref\":\"https://nowhere.example/x\"}')",
        );
        assert!(msg.contains("cannot compile schema"), "{msg}");
    }

    #[pg_test]
    fn register_rejects_duplicate_id() {
        reg("SELECT jsonschema.register('a', '{\"$id\":\"https://x.example/s\"}')").unwrap();
        let msg =
            error_message("SELECT jsonschema.register('b', '{\"$id\":\"https://x.example/s\"}')");
        assert!(msg.contains("already registered by a/1"), "{msg}");
    }

    #[pg_test]
    fn register_records_refs_and_id() {
        reg("SELECT jsonschema.register('address', '{\"$id\":\"https://acme.example/address\",\"type\":\"object\"}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"properties\":{\"a\":{\"$ref\":\"https://acme.example/address\"},\"b\":{\"$ref\":\"urn:pg-jsonschema-registry:address:1\"}}}')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.refs"),
            Ok(Some(1))
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT id_uri FROM jsonschema.schemas WHERE name='address'"),
            Ok(Some("https://acme.example/address".into()))
        );
        assert_eq!(
            Spi::get_one::<Vec<String>>("SELECT refs FROM jsonschema.schemas WHERE name='order'"),
            Ok(Some(vec!["address/1".into()]))
        );
    }

    #[pg_test]
    fn id_with_empty_fragment_is_referenceable_with_and_without_it() {
        reg("SELECT jsonschema.register('a', '{\"$schema\":\"http://json-schema.org/draft-07/schema#\",\"$id\":\"http://example.com/a.json#\",\"type\":\"integer\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT id_uri FROM jsonschema.schemas WHERE name = 'a'"),
            Ok(Some("http://example.com/a.json".into()))
        );
        reg("SELECT jsonschema.register('b', '{\"$ref\":\"http://example.com/a.json\"}')").unwrap();
        reg("SELECT jsonschema.register('c', '{\"$ref\":\"http://example.com/a.json#\"}')")
            .unwrap();
        for n in ["b", "c"] {
            assert_eq!(
                Spi::get_one::<bool>(&format!("SELECT jsonschema.valid('1', '{n}', 1)")),
                Ok(Some(true))
            );
            assert_eq!(
                Spi::get_one::<bool>(&format!("SELECT jsonschema.valid('\"x\"', '{n}', 1)")),
                Ok(Some(false))
            );
        }
    }

    #[pg_test]
    fn draft4_id_is_referenceable() {
        reg("SELECT jsonschema.register('d4', '{\"$schema\":\"http://json-schema.org/draft-04/schema#\",\"id\":\"http://example.com/d4.json#\",\"type\":\"string\"}')").unwrap();
        reg("SELECT jsonschema.register('user', '{\"properties\":{\"s\":{\"$ref\":\"http://example.com/d4.json\"}}}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{\"s\":1}', 'user', 1)"),
            Ok(Some(false))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{\"s\":\"x\"}', 'user', 1)"),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn id_is_stored_normalized_and_resolves_as_written() {
        reg("SELECT jsonschema.register('n', '{\"$id\":\"HTTPS://Example.COM:443/s/../n\",\"type\":\"string\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT id_uri FROM jsonschema.schemas WHERE name = 'n'"),
            Ok(Some("https://example.com/n".into()))
        );
        reg("SELECT jsonschema.register('m', '{\"$ref\":\"HTTPS://Example.COM:443/s/../n\"}')")
            .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('1', 'm', 1)"),
            Ok(Some(false))
        );
    }

    #[pg_test]
    fn relative_id_is_rejected() {
        let msg = error_message("SELECT jsonschema.register('r', '{\"$id\":\"a.json\"}')");
        assert_eq!(msg, "top-level $id must be an absolute URI");
    }

    #[pg_test]
    fn id_with_fragment_is_rejected() {
        let msg = error_message(
            "SELECT jsonschema.register('f', '{\"$id\":\"http://x.example/a#foo\"}')",
        );
        assert_eq!(msg, "top-level $id must not contain a fragment");
    }

    #[pg_test]
    fn bad_id_sqlstate_is_22023() {
        let code = PgTryBuilder::new(|| {
            reg("SELECT jsonschema.register('r', '{\"$id\":\"a.json\"}')").unwrap();
            None
        })
        .catch_when(
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            |_| Some(()),
        )
        .execute();
        assert_eq!(code, Some(()));
    }

    #[pg_test]
    fn register_sqlstate_for_bad_version_is_22023() {
        let code = PgTryBuilder::new(|| {
            reg("SELECT jsonschema.register('o', -1, '{}')").unwrap();
            None
        })
        .catch_when(
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            |_| Some(()),
        )
        .execute();
        assert_eq!(code, Some(()));
    }

    #[pg_test]
    fn valid_answers_true_and_false() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\",\"required\":[\"id\"],\"properties\":{\"id\":{\"type\":\"integer\"}}}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{\"id\":1}', 'p', 1)"),
            Ok(Some(true))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{\"id\":\"x\"}', 'p', 1)"),
            Ok(Some(false))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)"),
            Ok(Some(false))
        );
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
        Spi::get_one::<i64>(
            "SELECT count(*) FROM generate_series(1,100) g WHERE jsonschema.valid('{}', 'p', 1)",
        )
        .unwrap();
        assert_eq!(crate::cache::compile_count(), before + 1);
    }

    #[pg_test]
    fn register_invalidates_warm_cache_in_same_backend() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(false))
        );
        let before = crate::cache::compile_count();
        Spi::run("SELECT jsonschema.unregister('p', 1)").unwrap();
        reg("SELECT jsonschema.register('p', 1, '{\"type\":\"array\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(true))
        );
        assert_eq!(crate::cache::compile_count(), before + 1);
    }

    #[pg_test(error = "schema p version 1 is not registered")]
    fn delete_in_warm_backend_makes_valid_raise() {
        reg("SELECT jsonschema.register('p', '{}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::run("SELECT jsonschema.unregister('p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
    }

    #[pg_test]
    fn valid_follows_cross_schema_ref_with_fragment() {
        reg("SELECT jsonschema.register('address', '{\"$id\":\"https://acme.example/address\",\"$defs\":{\"street\":{\"type\":\"string\",\"minLength\":1}}}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"properties\":{\"street\":{\"$ref\":\"https://acme.example/address#/$defs/street\"}}}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{\"street\":\"Main\"}', 'order', 1)"),
            Ok(Some(true))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{\"street\":\"\"}', 'order', 1)"),
            Ok(Some(false))
        );
    }

    #[pg_test]
    fn valid_handles_toasted_documents() {
        reg("SELECT jsonschema.register('big', '{\"type\":\"object\",\"properties\":{\"blob\":{\"type\":\"string\",\"minLength\":100000}}}')").unwrap();
        reg("SELECT jsonschema.register('obj', '{\"type\":\"object\",\"required\":[\"blob\"]}')")
            .unwrap();
        // Out-of-line, uncompressed (EXTERNAL), plus a small inline row with a 1-byte header.
        Spi::run("CREATE TABLE docs(id int, d jsonb)").unwrap();
        Spi::run("ALTER TABLE docs ALTER d SET STORAGE EXTERNAL").unwrap();
        Spi::run("INSERT INTO docs VALUES (1, jsonb_build_object('blob', repeat('x', 200000)))")
            .unwrap();
        Spi::run("INSERT INTO docs VALUES (2, jsonb_build_object('blob', repeat('y', 10)))")
            .unwrap();
        let toast = Spi::get_one::<String>(
            "SELECT reltoastrelid::regclass::text FROM pg_class WHERE oid = 'docs'::regclass",
        )
        .unwrap()
        .unwrap();
        assert!(
            Spi::get_one::<i64>(&format!("SELECT count(*) FROM {toast}"))
                .unwrap()
                .unwrap()
                > 0,
            "big row is not out of line"
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT pg_column_size(d) < 127 FROM docs WHERE id = 2"),
            Ok(Some(true))
        );
        // Compressed (default EXTENDED storage, repetitive data).
        Spi::run("CREATE TABLE docs_c(d jsonb)").unwrap();
        Spi::run("INSERT INTO docs_c SELECT jsonb_build_object('blob', repeat('z', 200000))")
            .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT pg_column_size(d) < octet_length(d::text) FROM docs_c"),
            Ok(Some(true))
        );

        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'big', 1) FROM docs WHERE id = 1"),
            Ok(Some(true))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'big', 1) FROM docs WHERE id = 2"),
            Ok(Some(false))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'obj', 1) FROM docs WHERE id = 2"),
            Ok(Some(true))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid(d, 'big', 1) FROM docs_c"),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn foreign_invalidation_disables_cache_fill_for_the_transaction() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        // SAFETY: simulates Postgres delivering a foreign invalidation of the registry.
        unsafe {
            crate::cache::on_relcache_invalidation(
                pg_sys::Datum::from(0usize),
                crate::registry::relid(),
            )
        };
        let before = crate::cache::compile_count();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        assert_eq!(crate::cache::compile_count(), before + 2);
    }

    #[pg_test]
    fn own_invalidation_keeps_cache_fill() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::run("DELETE FROM jsonschema.registry WHERE name='p'").unwrap(); // unregister() arrives in Task 7
        crate::cache::invalidate();
        reg("SELECT jsonschema.register('p', 1, '{\"type\":\"array\"}')").unwrap();
        let before = crate::cache::compile_count();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(true))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(true))
        );
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
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.registry WHERE name = 'q'"),
            Ok(Some(0))
        );
        // SAFETY: simulates Postgres delivering a foreign invalidation of the registry.
        unsafe {
            crate::cache::on_relcache_invalidation(
                pg_sys::Datum::from(0usize),
                crate::registry::relid(),
            )
        };
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
        unsafe {
            crate::cache::on_xact_end(pg_sys::XactEvent::XACT_EVENT_COMMIT, std::ptr::null_mut())
        };
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

    #[pg_test]
    fn cache_stats_reports_this_backends_counters() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        let stats = || {
            Spi::get_three::<i64, i32, i64>(
                "SELECT compiles, entries, invalidations FROM jsonschema.cache_stats()",
            )
            .unwrap()
        };
        let (c0, _, i0) = stats();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        let (c1, e1, i1) = stats();
        assert_eq!(c1, c0.map(|c| c + 1));
        assert_eq!(e1, Some(1));
        assert_eq!(i1, i0);
        reg("SELECT jsonschema.register('p', '{}')").unwrap();
        let (_, e2, i2) = stats();
        assert_eq!(e2, Some(0));
        assert!(i2 > i1, "{i2:?} > {i1:?}");
    }

    #[pg_test]
    fn update_of_registry_row_is_rejected() {
        reg("SELECT jsonschema.register('p', '{}')").unwrap();
        let hit = PgTryBuilder::new(|| {
            Spi::run("UPDATE jsonschema.registry SET schema = '{\"type\":\"array\"}'").unwrap();
            false
        })
        .catch_when(PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED, |_| true)
        .execute();
        assert!(hit);
    }

    #[pg_test(error = "schema p version 1 is not registered")]
    fn direct_delete_in_warm_backend_makes_valid_raise() {
        reg("SELECT jsonschema.register('p', '{}')").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
        // No unregister(): the statement trigger must invalidate the warm cache.
        Spi::run("DELETE FROM jsonschema.registry WHERE name = 'p'").unwrap();
        Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)").unwrap();
    }

    #[pg_test]
    fn direct_insert_invalidates_warm_cache() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(false))
        );
        Spi::run("DELETE FROM jsonschema.registry WHERE name = 'p'").unwrap();
        Spi::run(
            "INSERT INTO jsonschema.registry (name, version, schema, draft) \
             VALUES ('p', 1, '{\"type\":\"array\"}', '2020-12')",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn invalidate_cache_requires_registry_privileges() {
        Spi::run("CREATE ROLE pgjsr_reader").unwrap();
        Spi::run("SET ROLE pgjsr_reader").unwrap();
        // Last action: a caught ERROR without a subtransaction leaves the transaction undefined.
        let denied = PgTryBuilder::new(|| {
            Spi::run("SELECT jsonschema.invalidate_cache()").unwrap();
            false
        })
        .catch_when(PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE, |_| true)
        .execute();
        assert!(denied);
    }

    /// Runs `sql` expecting an ERROR and returns its SQLSTATE and message.
    fn error_code(sql: &str) -> (PgSqlErrorCode, String) {
        PgTryBuilder::new(|| {
            reg(sql).unwrap();
            panic!("expected an error from: {sql}");
        })
        .catch_others(|e| match e {
            CaughtError::PostgresError(r)
            | CaughtError::ErrorReport(r)
            | CaughtError::RustPanic { ereport: r, .. } => {
                (r.sql_error_code(), r.message().to_string())
            }
        })
        .execute()
    }

    #[pg_test]
    fn deeply_nested_document_is_refused() {
        reg("SELECT jsonschema.register('rec', '{\"type\":\"array\",\"items\":{\"$ref\":\"#\"}}')")
            .unwrap();
        let (code, msg) = error_code(
            "SELECT jsonschema.valid((repeat('[', 5000) || repeat(']', 5000))::jsonb, 'rec', 1)::int",
        );
        assert_eq!(code, PgSqlErrorCode::ERRCODE_STATEMENT_TOO_COMPLEX);
        assert_eq!(msg, "document nesting exceeds 4096 levels");
    }

    #[pg_test(error = "document nesting exceeds 4096 levels")]
    fn deeply_nested_document_is_refused_by_errors() {
        reg("SELECT jsonschema.register('rec', '{}')").unwrap();
        Spi::run(
            "SELECT jsonschema.errors((repeat('[', 4097) || repeat(']', 4097))::jsonb, 'rec', 1)",
        )
        .unwrap();
    }

    #[pg_test(error = "document nesting exceeds 4096 levels")]
    fn deeply_nested_document_is_refused_by_validate() {
        reg("SELECT jsonschema.register('rec', '{}')").unwrap();
        Spi::run(
            "SELECT jsonschema.validate((repeat('[', 4097) || repeat(']', 4097))::jsonb, 'rec', 1)",
        )
        .unwrap();
    }

    #[pg_test]
    fn document_at_4000_levels_validates_against_recursive_schema() {
        reg("SELECT jsonschema.register('rec', '{\"type\":\"array\",\"items\":{\"$ref\":\"#\"}}')")
            .unwrap();
        let doc = "(repeat('[', 4000) || repeat(']', 4000))::jsonb";
        assert_eq!(
            Spi::get_one::<bool>(&format!("SELECT jsonschema.valid({doc}, 'rec', 1)")),
            Ok(Some(true))
        );
        let bad = "(repeat('[', 3999) || '1' || repeat(']', 3999))::jsonb";
        assert_eq!(
            Spi::get_one::<bool>(&format!("SELECT jsonschema.valid({bad}, 'rec', 1)")),
            Ok(Some(false))
        );
        let errs =
            Spi::get_one::<pgrx::JsonB>(&format!("SELECT jsonschema.errors({bad}, 'rec', 1)"))
                .unwrap()
                .unwrap();
        assert_eq!(errs.0.as_array().map(Vec::len), Some(1));
        assert_eq!(
            Spi::get_one::<bool>(&format!("SELECT jsonschema.validate({doc}, 'rec', 1)")),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn deeply_nested_schema_is_refused() {
        let (code, msg) = error_code(
            "SELECT jsonschema.register('deep', ('{\"not\":' || repeat('[', 100) || repeat(']', 100) || '}')::jsonb)",
        );
        assert_eq!(code, PgSqlErrorCode::ERRCODE_STATEMENT_TOO_COMPLEX);
        assert_eq!(msg, "schema nesting exceeds 100 levels");
    }

    #[pg_test]
    fn schema_at_100_levels_is_accepted() {
        let nested = format!("{}{{}}{}", "{\"not\":".repeat(99), "}".repeat(99));
        assert_eq!(
            reg(&format!("SELECT jsonschema.register('deep', '{nested}')")),
            Ok(Some(1))
        );
    }

    #[pg_test]
    fn errors_is_empty_array_when_valid() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        let e = Spi::get_one::<pgrx::JsonB>("SELECT jsonschema.errors('{}', 'p', 1)")
            .unwrap()
            .unwrap();
        assert_eq!(e.0, serde_json::json!([]));
    }

    #[pg_test]
    fn errors_lists_every_failure_with_paths_and_keyword() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\",\"required\":[\"id\"],\"properties\":{\"n\":{\"minimum\":5}}}')").unwrap();
        let e = Spi::get_one::<pgrx::JsonB>("SELECT jsonschema.errors('{\"n\":1}', 'p', 1)")
            .unwrap()
            .unwrap();
        let arr = e.0.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let min = arr.iter().find(|x| x["keyword"] == "minimum").unwrap();
        assert_eq!(min["instance_path"], "/n");
        assert_eq!(min["schema_path"], "/properties/n/minimum");
        assert!(min["message"].as_str().unwrap().contains("5"));
        assert!(arr
            .iter()
            .any(|x| x["keyword"] == "required" && x["instance_path"] == ""));
    }

    #[pg_test]
    fn validate_returns_true_when_valid() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.validate('{}', 'p', 1)"),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn validate_raises_check_violation_with_detail() {
        reg("SELECT jsonschema.register('p', '{\"required\":[\"id\"]}')").unwrap();
        let caught = PgTryBuilder::new(|| {
            Spi::get_one::<bool>("SELECT jsonschema.validate('{}', 'p', 1)").unwrap();
            None
        })
        .catch_when(PgSqlErrorCode::ERRCODE_CHECK_VIOLATION, |e| match e {
            CaughtError::ErrorReport(r) | CaughtError::PostgresError(r) => Some((
                r.message().to_string(),
                r.detail().unwrap_or("").to_string(),
            )),
            _ => None,
        })
        .execute()
        .expect("expected ERRCODE_CHECK_VIOLATION");
        assert!(caught.0.contains("schema p version 1"), "{}", caught.0);
        assert!(
            caught.1.contains("\"keyword\":\"required\"") || caught.1.contains("required"),
            "{}",
            caught.1
        );
    }

    #[pg_test]
    fn validate_detail_lists_at_most_20_errors() {
        // 25 required properties missing: 25 errors.
        let req: Vec<String> = (0..25).map(|i| format!("\"p{i}\"")).collect();
        reg(&format!(
            "SELECT jsonschema.register('many', '{{\"required\":[{}]}}')",
            req.join(",")
        ))
        .unwrap();
        let errs = Spi::get_one::<pgrx::JsonB>("SELECT jsonschema.errors('{}', 'many', 1)")
            .unwrap()
            .unwrap();
        let total = errs.0.as_array().unwrap().len();
        assert!(total > 20, "{total}");
        let detail = PgTryBuilder::new(|| {
            Spi::get_one::<bool>("SELECT jsonschema.validate('{}', 'many', 1)").unwrap();
            None
        })
        .catch_when(PgSqlErrorCode::ERRCODE_CHECK_VIOLATION, |e| match e {
            CaughtError::ErrorReport(r) | CaughtError::PostgresError(r) => {
                Some(r.detail().unwrap_or("").to_string())
            }
            _ => None,
        })
        .execute()
        .expect("expected ERRCODE_CHECK_VIOLATION");
        let suffix = format!(" \u{2026} and {} more", total - 20);
        assert!(
            detail.starts_with(&format!("{total} error(s): [")),
            "{detail}"
        );
        assert!(detail.ends_with(&suffix), "{detail}");
        let list = &detail[detail.find('[').unwrap()..detail.len() - suffix.len()];
        let shown: serde_json::Value = serde_json::from_str(list).unwrap();
        assert_eq!(shown.as_array().unwrap().len(), 20);
    }

    #[pg_test]
    fn unknown_meta_schema_is_rejected_with_clear_message() {
        let (code, msg) = error_code(
            "SELECT jsonschema.register('c', '{\"$schema\":\"https://example.com/my-meta\"}')",
        );
        assert_eq!(code, PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE);
        assert_eq!(
            msg,
            "unsupported $schema: https://example.com/my-meta (custom meta-schemas are not supported)"
        );
    }

    #[pg_test]
    fn validate_works_as_check_constraint() {
        reg("SELECT jsonschema.register('p', '{\"required\":[\"id\"]}')").unwrap();
        Spi::run("CREATE TABLE t (payload jsonb CHECK (jsonschema.validate(payload, 'p', 1)))")
            .unwrap();
        Spi::run("INSERT INTO t VALUES ('{\"id\":1}')").unwrap();
        let rejected = PgTryBuilder::new(|| {
            Spi::run("INSERT INTO t VALUES ('{}')").unwrap();
            false
        })
        .catch_when(PgSqlErrorCode::ERRCODE_CHECK_VIOLATION, |_| true)
        .execute();
        assert!(rejected);
    }

    #[pg_test]
    fn get_and_latest_version() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        reg("SELECT jsonschema.register('p', '{\"type\":\"array\"}')").unwrap();
        assert_eq!(
            Spi::get_one::<i32>("SELECT jsonschema.latest_version('p')"),
            Ok(Some(2))
        );
        let s = Spi::get_one::<pgrx::JsonB>("SELECT jsonschema.get('p', 1)")
            .unwrap()
            .unwrap();
        assert_eq!(s.0, serde_json::json!({"type":"object"}));
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p')"),
            Ok(Some(true))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(false))
        );
    }

    #[pg_test(error = "schema p version 3 is not registered")]
    fn get_unknown_raises() {
        Spi::get_one::<pgrx::JsonB>("SELECT jsonschema.get('p', 3)").unwrap();
    }

    #[pg_test(error = "schema nope has no registered versions")]
    fn latest_version_unknown_raises() {
        Spi::get_one::<i32>("SELECT jsonschema.latest_version('nope')").unwrap();
    }

    #[pg_test]
    fn unregister_removes_and_allows_reregistration() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::run("SELECT jsonschema.unregister('p', 1)").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.registry WHERE name='p'"),
            Ok(Some(0))
        );
        assert_eq!(
            reg("SELECT jsonschema.register('p', 1, '{\"type\":\"array\"}')"),
            Ok(Some(1))
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('[]', 'p', 1)"),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn unregister_blocked_by_dependent() {
        reg("SELECT jsonschema.register('address', '{\"type\":\"object\"}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"$ref\":\"urn:pg-jsonschema-registry:address:1\"}')").unwrap();
        let msg = error_message("SELECT jsonschema.unregister('address', 1)");
        assert!(msg.contains("is referenced by order/1"), "{msg}");
    }

    #[pg_test]
    fn unregister_lists_every_dependent() {
        reg("SELECT jsonschema.register('address', '{\"type\":\"object\"}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"$ref\":\"urn:pg-jsonschema-registry:address:1\"}')").unwrap();
        reg("SELECT jsonschema.register('invoice', '{\"$ref\":\"urn:pg-jsonschema-registry:address:1\"}')").unwrap();
        let msg = error_message("SELECT jsonschema.unregister('address', 1)");
        assert!(msg.contains("invoice/1, order/1"), "{msg}");
    }

    #[pg_test]
    fn unregister_sqlstate_is_2bp01() {
        reg("SELECT jsonschema.register('address', '{}')").unwrap();
        reg("SELECT jsonschema.register('order', '{\"$ref\":\"urn:pg-jsonschema-registry:address:1\"}')").unwrap();
        let hit = PgTryBuilder::new(|| {
            Spi::run("SELECT jsonschema.unregister('address', 1)").unwrap();
            false
        })
        .catch_when(
            PgSqlErrorCode::ERRCODE_DEPENDENT_OBJECTS_STILL_EXIST,
            |_| true,
        )
        .execute();
        assert!(hit);
    }

    #[pg_test]
    fn non_superuser_can_validate_but_not_register() {
        reg("SELECT jsonschema.register('p', '{\"type\":\"object\"}')").unwrap();
        Spi::run("CREATE ROLE pgjsr_reader").unwrap();
        Spi::run("SET ROLE pgjsr_reader").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT jsonschema.valid('{}', 'p', 1)"),
            Ok(Some(true))
        );
        // Last action: a caught ERROR without a subtransaction leaves the transaction undefined.
        let denied = PgTryBuilder::new(|| {
            reg("SELECT jsonschema.register('q', '{}')").unwrap();
            false
        })
        .catch_when(PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE, |_| true)
        .execute();
        assert!(denied);
    }

    #[pg_test]
    fn non_superuser_can_register_after_grant() {
        // Take the registry lock first (as every register does) so the GRANT below cannot
        // deadlock against concurrent tests that lock registry before the other tables.
        reg("SELECT jsonschema.register('warmup', '{}')").unwrap();
        Spi::run("CREATE ROLE pgjsr_reader").unwrap();
        Spi::run("GRANT INSERT, DELETE ON ALL TABLES IN SCHEMA jsonschema TO pgjsr_reader")
            .unwrap();
        Spi::run("SET ROLE pgjsr_reader").unwrap();
        assert_eq!(reg("SELECT jsonschema.register('q', '{}')"), Ok(Some(1)));
        Spi::run("RESET ROLE").unwrap();
    }

    #[pg_test]
    fn every_draft_registers_and_validates() {
        for (name, uri) in [
            ("d4", "http://json-schema.org/draft-04/schema#"),
            ("d6", "http://json-schema.org/draft-06/schema#"),
            ("d7", "http://json-schema.org/draft-07/schema#"),
            ("d2019", "https://json-schema.org/draft/2019-09/schema"),
            ("d2020", "https://json-schema.org/draft/2020-12/schema"),
        ] {
            reg(&format!("SELECT jsonschema.register('{name}', '{{\"$schema\":\"{uri}\",\"type\":\"integer\",\"minimum\":1}}')")).unwrap();
            assert_eq!(
                Spi::get_one::<bool>(&format!("SELECT jsonschema.valid('1', '{name}', 1)")),
                Ok(Some(true))
            );
            assert_eq!(
                Spi::get_one::<bool>(&format!("SELECT jsonschema.valid('0', '{name}', 1)")),
                Ok(Some(false))
            );
        }
        assert_eq!(
            Spi::get_one::<String>("SELECT draft FROM jsonschema.registry WHERE name='d2019'"),
            Ok(Some("2019-09".into()))
        );
    }
}
