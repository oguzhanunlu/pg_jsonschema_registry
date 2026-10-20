//! Per-backend cache of compiled validators keyed by `(name, version)`.
//!
//! Invalidation: `register`/`unregister` call [`invalidate`], which queues a relcache
//! invalidation of `jsonschema.registry`. Postgres delivers the message to every
//! backend at commit (and to this backend at command end or abort), and
//! [`on_relcache_invalidation`] clears the map and bumps a generation counter. [`get_or_compile`] calls `AcceptInvalidationMessages()` first, so the hit
//! path observes committed registry changes at the same point Postgres's own catalog
//! caches do.
//!
//! Stale snapshots: a transaction whose snapshot predates a registry change could
//! re-read the old row after the map was cleared. So a relevant invalidation that this
//! transaction may not see sets a per-transaction flag (reset by [`on_xact_end`]), and
//! a validator compiled while it is set is used for the current call but never cached.
//! The flag is set only when the message is processed *after* this transaction took its
//! first snapshot (`IsTransactionState() && FirstSnapshotSet`):
//!
//! - A committing backend makes its commit visible (`ProcArrayEndTransaction`) before it
//!   sends its invalidation messages (`AtEOXact_Inval`), so a message processed before
//!   this transaction's first snapshot belongs to a change that snapshot sees. That
//!   covers the common case, messages read at transaction start (`AtStart_Cache`, at
//!   `TRANS_START`), including this backend's own committed messages, which come back
//!   through the shared queue.
//! - At abort, `AtEOXact_Inval(false)` replays this transaction's earlier messages
//!   locally after the `XACT_EVENT_ABORT` callback, in state `TRANS_ABORT`; the gate keeps
//!   that from leaking the flag into the next transaction.
//!
//! Own vs foreign within a transaction: this backend's own change is always visible to
//! its own snapshot, so [`invalidate`] marks its message as own (`OWN_PENDING`) and the
//! callback consumes that mark instead of setting the flag when the message is processed
//! at command end. Postgres deduplicates a relid's local relcache messages within a
//! command, so one bool matches one callback firing. A transaction or subtransaction
//! abort discards the current command's messages, so the mark is reset there too.
//!
//! Relevance: `InvalidOid` (all relations) is relevant; while the registry relid is
//! still unknown in this backend every relcache message is treated as relevant
//! (conservative); once known, only messages for that relid are.
//!
//! Guarantee: a cached validator is never served after this backend has processed a
//! relevant invalidation, and a validator is inserted only if no relevant foreign
//! invalidation was processed after the current transaction's first snapshot and none
//! arrived while it was being compiled.
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

/// Compiled validators by `(name, version)`.
type ValidatorMap = HashMap<(String, i32), Arc<Validator<Jsonb>>>;

thread_local! {
    static CACHE: RefCell<ValidatorMap> = RefCell::new(HashMap::new());
    static COMPILES: Cell<u64> = const { Cell::new(0) };
    /// Number of times a relevant invalidation cleared the map.
    static INVALIDATIONS: Cell<u64> = const { Cell::new(0) };
    /// Bumped by every relevant invalidation; a compile that straddles one is not cached.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// The registry's relid, refreshed on every miss (the callback must not run SQL).
    static REGISTRY_RELID: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
    /// Set by a relevant foreign invalidation in the current transaction; reset at transaction end.
    static INVALIDATED_THIS_XACT: Cell<bool> = const { Cell::new(false) };
    /// Set by `invalidate()`: the next relevant invalidation is this backend's own.
    static OWN_PENDING: Cell<bool> = const { Cell::new(false) };
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
    if GENERATION.get() == generation && !INVALIDATED_THIS_XACT.get() {
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

/// Number of compiles done by [`get_or_compile`] in this backend.
pub fn compile_count() -> u64 {
    COMPILES.get()
}

/// Number of validators currently cached in this backend.
pub fn entry_count() -> usize {
    CACHE.with_borrow(|c| c.len())
}

/// Number of relevant invalidations this backend has processed (each cleared the map).
pub fn invalidation_count() -> u64 {
    INVALIDATIONS.get()
}

/// Queues a relcache invalidation for `jsonschema.registry`, marked as this
/// backend's own; must run in a transaction.
pub fn invalidate() {
    // SAFETY: IsTransactionState only reads backend-local transaction state.
    debug_assert!(
        unsafe { pg_sys::IsTransactionState() },
        "invalidate() outside a transaction"
    );
    let relid = registry::relid();
    REGISTRY_RELID.set(Some(relid));
    OWN_PENDING.set(true);
    // SAFETY: called inside a transaction with a valid relation oid.
    unsafe { pg_sys::CacheInvalidateRelcacheByRelid(relid) }
}

/// Registers the relcache, transaction and subtransaction callbacks; called once from `_PG_init`.
pub fn register_callbacks() {
    // SAFETY: the callback has the required signature and lives for the process.
    unsafe {
        pg_sys::CacheRegisterRelcacheCallback(
            Some(on_relcache_invalidation),
            pg_sys::Datum::from(0usize),
        )
    }
    // SAFETY: the callback has the required signature, lives for the process and ignores `arg`.
    unsafe { pg_sys::RegisterXactCallback(Some(on_xact_end), ptr::null_mut()) }
    // SAFETY: the callback has the required signature, lives for the process and ignores `arg`.
    unsafe { pg_sys::RegisterSubXactCallback(Some(on_subxact), ptr::null_mut()) }
}

/// Relcache callback. Relevant: `InvalidOid`, any relid while the registry relid is
/// unknown, else exactly the registry relid.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn on_relcache_invalidation(
    _arg: pg_sys::Datum,
    relid: pg_sys::Oid,
) {
    let known = REGISTRY_RELID.try_with(Cell::get).ok().flatten();
    let relevant = relid == pg_sys::InvalidOid || known.is_none_or(|r| r == relid);
    if !relevant {
        return;
    }
    let _ = GENERATION.try_with(|g| g.set(g.get().wrapping_add(1)));
    let _ = INVALIDATIONS.try_with(|n| n.set(n.get().wrapping_add(1)));
    clear();
    let own = OWN_PENDING.try_with(|o| o.replace(false)) == Ok(true);
    // SAFETY: both only read backend-local state; FirstSnapshotSet is a plain global.
    let after_snapshot = unsafe { pg_sys::IsTransactionState() && pg_sys::FirstSnapshotSet };
    if !own && after_snapshot {
        let _ = INVALIDATED_THIS_XACT.try_with(|f| f.set(true));
    }
}

/// Transaction callback: resets the per-transaction state at transaction end.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn on_xact_end(
    event: pg_sys::XactEvent::Type,
    _arg: *mut std::ffi::c_void,
) {
    use pg_sys::XactEvent::*;
    if matches!(
        event,
        XACT_EVENT_COMMIT
            | XACT_EVENT_ABORT
            | XACT_EVENT_PREPARE
            | XACT_EVENT_PARALLEL_COMMIT
            | XACT_EVENT_PARALLEL_ABORT
    ) {
        let _ = INVALIDATED_THIS_XACT.try_with(|f| f.set(false));
        let _ = OWN_PENDING.try_with(|o| o.set(false));
    }
}

/// Subtransaction callback: on subtransaction abort the current command's local
/// messages are discarded, so a pending own mark would never be consumed and could
/// later swallow a foreign invalidation. Reset it; a still-pending parent message is
/// then treated as foreign, which is conservative.
#[pg_guard]
unsafe extern "C-unwind" fn on_subxact(
    event: pg_sys::SubXactEvent::Type,
    _my_subid: pg_sys::SubTransactionId,
    _parent_subid: pg_sys::SubTransactionId,
    _arg: *mut std::ffi::c_void,
) {
    if event == pg_sys::SubXactEvent::SUBXACT_EVENT_ABORT_SUB {
        let _ = OWN_PENDING.try_with(|o| o.set(false));
    }
}

/// Test-only: forget the registry relid, the flags and every cached validator,
/// as in a backend that has never registered or validated.
#[cfg(any(test, feature = "pg_test"))]
pub fn reset_for_test() {
    clear();
    REGISTRY_RELID.set(None);
    INVALIDATED_THIS_XACT.set(false);
    OWN_PENDING.set(false);
}
