//! Per-backend cache of compiled validators keyed by `(name, version)`.
//!
//! The cache is never trusted across registry changes: `register`/`unregister`
//! call [`invalidate`], which queues a relcache invalidation for
//! `jsonschema.registry`. Postgres delivers it to every backend at commit (and
//! to this backend at command end or abort), and [`on_relcache_invalidation`]
//! clears the map.

use crate::{compile, registry};
use jsonschema::json::Jsonb;
use jsonschema::Validator;
use pgrx::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

thread_local! {
    static CACHE: RefCell<HashMap<(String, i32), Arc<Validator<Jsonb>>>> = RefCell::new(HashMap::new());
    static COMPILES: Cell<u64> = const { Cell::new(0) };
    /// Bumped by every relevant invalidation; a compile that straddles one is not cached.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// The registry's relid, resolved lazily (the callback must not run SQL).
    static REGISTRY_RELID: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
}

fn fail(code: PgSqlErrorCode, msg: String) -> ! {
    pgrx::pg_sys::panic::ErrorReport::new(code, msg, function_name!()).report(PgLogLevel::ERROR);
    unreachable!("ERROR does not return")
}

/// Returns the validator for `name`/`version`, compiling and caching it on a miss.
/// Raises `42704` if the schema is not registered.
pub fn get_or_compile(name: &str, version: i32) -> Arc<Validator<Jsonb>> {
    let key = (name.to_owned(), version);
    if let Some(v) = CACHE.with_borrow(|c| c.get(&key).cloned()) {
        return v;
    }
    if REGISTRY_RELID.get().is_none() {
        REGISTRY_RELID.set(Some(registry::relid()));
    }
    let generation = GENERATION.get();
    let Some(row) = registry::lookup(name, version) else {
        fail(
            PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
            format!("schema {name} version {version} is not registered"),
        );
    };
    // No CACHE borrow is held here: the retriever runs SPI inside compile.
    let compiled = match compile::compile(&row.schema) {
        Ok(c) => c,
        Err(e) => fail(
            PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            format!("cannot compile schema {name} version {version}: {e}"),
        ),
    };
    COMPILES.set(COMPILES.get() + 1);
    let validator = Arc::new(compiled.validator);
    if GENERATION.get() == generation {
        CACHE.with_borrow_mut(|c| c.insert(key, Arc::clone(&validator)));
    }
    validator
}

/// Drops every cached validator in this backend. Never panics (it is called from
/// the relcache callback); no borrow of the map is ever held across code that can
/// process invalidations, so the `try_borrow_mut` always succeeds.
pub fn clear() {
    CACHE.with(|c| {
        if let Ok(mut m) = c.try_borrow_mut() {
            m.clear();
        }
    });
}

/// Number of compiles done by [`get_or_compile`] in this backend (test observability).
pub fn compile_count() -> u64 {
    COMPILES.get()
}

/// Queues a relcache invalidation for `jsonschema.registry`; must run in a transaction.
pub fn invalidate() {
    let relid = registry::relid();
    // SAFETY: called inside a transaction with a valid relation oid.
    unsafe { pg_sys::CacheInvalidateRelcacheByRelid(relid) }
}

/// Registers [`on_relcache_invalidation`]; called once from `_PG_init`.
pub fn register_callback() {
    // SAFETY: the callback has the required signature and lives for the process.
    unsafe { pg_sys::CacheRegisterRelcacheCallback(Some(on_relcache_invalidation), pg_sys::Datum::from(0usize)) }
}

/// Relcache callback: runs no SQL and cannot panic; it only touches thread-locals.
/// `InvalidOid` means "all relations" (e.g. after an sinval overflow). While the
/// registry relid is not yet known every invalidation counts (the map is empty then).
#[pg_guard]
unsafe extern "C-unwind" fn on_relcache_invalidation(_arg: pg_sys::Datum, relid: pg_sys::Oid) {
    let known = REGISTRY_RELID.get();
    if relid != pg_sys::InvalidOid && known.is_some_and(|k| k != relid) {
        return;
    }
    GENERATION.set(GENERATION.get().wrapping_add(1));
    clear();
    // Forget the relid so a dropped and re-created registry is re-resolved.
    REGISTRY_RELID.set(None);
}
