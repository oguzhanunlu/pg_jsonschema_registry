//! Per-backend cache of compiled validators keyed by `(name, version)`.
//!
//! Guarantee: a validator is never served from the cache after this backend has
//! processed an invalidation of `jsonschema.registry`, and a validator is only
//! inserted when no such invalidation was processed in the current transaction.
//! `register`/`unregister` call [`invalidate`], which queues a relcache
//! invalidation; Postgres delivers it to every backend at commit (and to this
//! backend at command end or abort), and [`on_relcache_invalidation`] clears the map.
//!
//! Snapshots: a transaction whose snapshot predates a registry change could re-read
//! the old row after the map was cleared. So the callback also sets a
//! per-transaction flag (reset by [`on_xact_end`]), and a validator compiled while
//! it is set is used for the current call but never cached. [`get_or_compile`]
//! calls `AcceptInvalidationMessages()` first, so the hit path observes committed
//! registry changes at the same point Postgres's own catalog caches do.
//!
//! The callbacks run no SQL and cannot panic: they only touch thread-locals,
//! through `try_with` / `try_borrow_mut`.

use crate::api::fail;
use crate::{compile, registry};
use jsonschema::json::Jsonb;
use jsonschema::Validator;
use pgrx::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr;
use std::sync::Arc;

thread_local! {
    static CACHE: RefCell<HashMap<(String, i32), Arc<Validator<Jsonb>>>> = RefCell::new(HashMap::new());
    static COMPILES: Cell<u64> = const { Cell::new(0) };
    /// Bumped by every relevant invalidation; a compile that straddles one is not cached.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// The registry's relid, refreshed on every miss (the callback must not run SQL).
    static REGISTRY_RELID: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
    /// Set when the registry was invalidated in the current transaction; reset at transaction end.
    static INVALIDATED_IN_XACT: Cell<bool> = const { Cell::new(false) };
    /// Set when `clear()` could not borrow the map; the next `get_or_compile` clears first.
    static DIRTY: Cell<bool> = const { Cell::new(false) };
}

/// Returns the validator for `name`/`version`, compiling and caching it on a miss.
/// Raises `42704` if the schema is not registered.
pub fn get_or_compile(name: &str, version: i32) -> Arc<Validator<Jsonb>> {
    // SAFETY: we are inside a function call in a transaction.
    unsafe { pg_sys::AcceptInvalidationMessages() };
    if DIRTY.get() {
        CACHE.with_borrow_mut(|c| c.clear());
        DIRTY.set(false);
    }
    let key = (name.to_owned(), version);
    if let Some(v) = CACHE.with_borrow(|c| c.get(&key).cloned()) {
        return v;
    }
    // Refreshed on every miss so a dropped and re-created registry is picked up.
    REGISTRY_RELID.set(Some(registry::relid()));
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
    if GENERATION.get() == generation && !INVALIDATED_IN_XACT.get() {
        CACHE.with_borrow_mut(|c| c.insert(key, Arc::clone(&validator)));
    }
    validator
}

/// Drops every cached validator in this backend. Never panics (it is called from
/// the relcache callback); if the map cannot be borrowed, the next
/// `get_or_compile` clears it instead.
pub fn clear() {
    let cleared = CACHE.try_with(|c| c.try_borrow_mut().map(|mut m| m.clear()).is_ok());
    if cleared == Ok(false) {
        let _ = DIRTY.try_with(|d| d.set(true));
    }
}

/// Number of compiles done by [`get_or_compile`] in this backend (test observability).
pub fn compile_count() -> u64 {
    COMPILES.get()
}

/// Queues a relcache invalidation for `jsonschema.registry`; must run in a transaction.
pub fn invalidate() {
    // SAFETY: IsTransactionState only reads backend-local transaction state.
    debug_assert!(unsafe { pg_sys::IsTransactionState() }, "invalidate() outside a transaction");
    let relid = registry::relid();
    // SAFETY: called inside a transaction with a valid relation oid.
    unsafe { pg_sys::CacheInvalidateRelcacheByRelid(relid) }
}

/// Registers the relcache and transaction callbacks; called once from `_PG_init`.
pub fn register_callbacks() {
    // SAFETY: the callback has the required signature and lives for the process.
    unsafe { pg_sys::CacheRegisterRelcacheCallback(Some(on_relcache_invalidation), pg_sys::Datum::from(0usize)) }
    // SAFETY: the callback has the required signature, lives for the process and ignores `arg`.
    unsafe { pg_sys::RegisterXactCallback(Some(on_xact_end), ptr::null_mut()) }
}

/// Relcache callback. `InvalidOid` means "all relations" (e.g. after an sinval overflow).
#[pg_guard]
unsafe extern "C-unwind" fn on_relcache_invalidation(_arg: pg_sys::Datum, relid: pg_sys::Oid) {
    let known = REGISTRY_RELID.try_with(Cell::get).ok().flatten();
    if relid != pg_sys::InvalidOid && Some(relid) != known {
        return;
    }
    let _ = GENERATION.try_with(|g| g.set(g.get().wrapping_add(1)));
    let _ = INVALIDATED_IN_XACT.try_with(|f| f.set(true));
    clear();
}

/// Transaction callback: resets the per-transaction invalidation flag at transaction end.
#[pg_guard]
unsafe extern "C-unwind" fn on_xact_end(event: pg_sys::XactEvent::Type, _arg: *mut std::ffi::c_void) {
    use pg_sys::XactEvent::*;
    if matches!(
        event,
        XACT_EVENT_COMMIT | XACT_EVENT_ABORT | XACT_EVENT_PREPARE | XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_PARALLEL_ABORT
    ) {
        let _ = INVALIDATED_IN_XACT.try_with(|f| f.set(false));
    }
}
