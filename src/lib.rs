use pgrx::prelude::*;

::pgrx::pg_module_magic!(name, version);

mod api;
mod cache;
mod compile;
mod depth;
mod errors;
mod raw;
mod registry;
mod retriever;

extension_sql_file!("../sql/bootstrap.sql", name = "bootstrap", bootstrap);

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    cache::register_callbacks();
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn bootstrap_creates_empty_registry_tables() {
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.registry"),
            Ok(Some(0))
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.ids"),
            Ok(Some(0))
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM jsonschema.refs"),
            Ok(Some(0))
        );
    }

    #[pg_test]
    fn bootstrap_registers_config_tables_for_dump() {
        let n = Spi::get_one::<i32>(
            "SELECT cardinality(extconfig) FROM pg_extension WHERE extname = 'pg_jsonschema_registry'");
        assert_eq!(n, Ok(Some(4)));
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec![]
    }
}
