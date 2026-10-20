//! Multi-session integration tests against a running Postgres with the extension
//! installed. Two `Client`s are two backends, which is what the per-backend cache
//! coherence claims are about; `#[pg_test]` cannot express that.
//!
//! Ignored by default. Run with `scripts/integration-test.sh`, or by hand:
//! `PGJSR_TEST_DSN="host=localhost port=5432 dbname=pgjsr_it" cargo test --test integration -- --ignored --test-threads=1`.
//!
//! Every test uses its own schema and table names, but run them with
//! `--test-threads=1` (the script does): a backend that reads a registry
//! invalidation from another backend skips the cache fill for the rest of that
//! transaction, so a concurrent test's commit can keep a cache cold and turn a
//! hit-path check into a miss-path check, which would pass vacuously.
//!
//! For the same reason a backend first [`absorb`]s pending invalidations (its own
//! earlier commits come back through the shared queue too) before a call that
//! is meant to fill its cache.

use postgres::error::SqlState;
use postgres::{Client, NoTls};

fn client() -> Client {
    let dsn = std::env::var("PGJSR_TEST_DSN").expect("PGJSR_TEST_DSN must be set");
    Client::connect(&dsn, NoTls).expect("connect to PGJSR_TEST_DSN")
}

/// `jsonschema.valid(doc, 'p', 1)` in the given backend.
fn valid(c: &mut Client, doc: &str) -> bool {
    c.query_one("SELECT jsonschema.valid($1::text::jsonb, 'p', 1)", &[&doc])
        .unwrap()
        .get(0)
}

/// Unregisters `(name, 1)` for each name, ignoring "not registered" (42704), so reruns start clean.
fn cleanup(c: &mut Client, names: &[&str]) {
    for name in names {
        c.batch_execute(&format!(
            "DO $$ BEGIN PERFORM jsonschema.unregister('{name}', 1); \
             EXCEPTION WHEN undefined_object THEN NULL; END $$"
        ))
        .unwrap();
    }
}

/// Reads pending shared invalidations in a transaction of its own (locking the
/// registry does `AcceptInvalidationMessages`), so the next transaction starts
/// with nothing pending and its first miss fills the cache.
fn absorb(c: &mut Client) {
    c.batch_execute("SELECT count(*) FROM jsonschema.registry").unwrap();
}

fn sqlstate(e: &postgres::Error) -> Option<&SqlState> {
    e.code()
}

#[test]
#[ignore]
fn other_backend_sees_reregistered_schema() {
    let mut a = client();
    let mut b = client();
    cleanup(&mut a, &["p"]);
    a.execute("SELECT jsonschema.register('p', 1, '{\"type\":\"object\"}')", &[]).unwrap();
    absorb(&mut b);
    assert!(!valid(&mut b, "[]")); // b's cache now holds the object schema
    a.execute("SELECT jsonschema.unregister('p', 1)", &[]).unwrap();
    a.execute("SELECT jsonschema.register('p', 1, '{\"type\":\"array\"}')", &[]).unwrap();
    assert!(valid(&mut b, "[]"), "b must not answer from its stale cached validator");
}

#[test]
#[ignore]
fn rolled_back_register_leaves_no_phantom() {
    let mut a = client();
    let mut b = client();
    cleanup(&mut a, &["ph"]);
    absorb(&mut a);
    let mut tx = a.transaction().unwrap();
    // a's own change: visible to its snapshot, so the next call fills the cache.
    tx.execute("SELECT jsonschema.register('ph', 1, '{\"type\":\"array\"}')", &[]).unwrap();
    assert!(tx.query_one("SELECT jsonschema.valid('[]', 'ph', 1)", &[]).unwrap().get::<_, bool>(0));
    tx.rollback().unwrap();
    let err = a
        .query_one("SELECT jsonschema.valid('[]', 'ph', 1)", &[])
        .expect_err("rolled-back schema must not be served from the cache");
    assert_eq!(sqlstate(&err), Some(&SqlState::UNDEFINED_OBJECT), "{err:?}");
    b.execute("SELECT jsonschema.register('ph', 1, '{\"type\":\"object\"}')", &[]).unwrap();
    assert!(
        !a.query_one("SELECT jsonschema.valid('[]', 'ph', 1)", &[]).unwrap().get::<_, bool>(0),
        "a must apply b's object schema, not the rolled-back array schema"
    );
}

#[test]
#[ignore]
fn repeatable_read_sees_its_snapshot_then_the_new_schema() {
    let mut a = client();
    let mut b = client();
    cleanup(&mut a, &["rr"]);
    a.execute("SELECT jsonschema.register('rr', 1, '{\"type\":\"object\"}')", &[]).unwrap();
    let mut tx = a.transaction().unwrap(); // BEGIN
    // Simple-protocol statements: SET TRANSACTION must be the first statement, and
    // the first query takes the snapshot.
    tx.batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").unwrap();
    tx.batch_execute("SELECT 1").unwrap(); // snapshot taken
    let iso: String = tx.query_one("SHOW transaction_isolation", &[]).unwrap().get(0);
    assert_eq!(iso, "repeatable read");
    b.execute("SELECT jsonschema.unregister('rr', 1)", &[]).unwrap();
    b.execute("SELECT jsonschema.register('rr', 1, '{\"type\":\"array\"}')", &[]).unwrap();
    let old: bool = tx.query_one("SELECT jsonschema.valid('[]', 'rr', 1)", &[]).unwrap().get(0);
    assert!(!old, "inside the RR transaction the OLD schema (object) must apply");
    tx.commit().unwrap();
    let new: bool = a.query_one("SELECT jsonschema.valid('[]', 'rr', 1)", &[]).unwrap().get(0);
    assert!(new, "after the RR transaction the NEW schema (array) must apply, not a cached stale one");
}

#[test]
#[ignore]
fn open_read_committed_transaction_sees_committed_change_on_hit_path() {
    let mut a = client();
    let mut b = client();
    cleanup(&mut a, &["rc"]);
    a.execute("SELECT jsonschema.register('rc', 1, '{\"type\":\"object\"}')", &[]).unwrap();
    absorb(&mut a);
    let mut tx = a.transaction().unwrap();
    // Miss with nothing pending: compiles and caches; the next call is a hit.
    let warm: bool = tx.query_one("SELECT jsonschema.valid('[]', 'rc', 1)", &[]).unwrap().get(0);
    assert!(!warm);
    b.execute("SELECT jsonschema.unregister('rc', 1)", &[]).unwrap();
    b.execute("SELECT jsonschema.register('rc', 1, '{\"type\":\"array\"}')", &[]).unwrap();
    let after: bool = tx.query_one("SELECT jsonschema.valid('[]', 'rc', 1)", &[]).unwrap().get(0);
    assert!(after, "a hit inside an open READ COMMITTED transaction must observe B's committed change");
    tx.commit().unwrap();
}

#[test]
#[ignore]
fn valid_runs_in_parallel_workers() {
    let mut c = client();
    cleanup(&mut c, &["par"]);
    c.batch_execute(
        "SELECT jsonschema.register('par', 1, '{\"type\":\"object\",\"required\":[\"i\"]}'); \
         DROP TABLE IF EXISTS many; \
         CREATE TABLE many AS SELECT jsonb_build_object('i', g) AS d FROM generate_series(1, 50000) g; \
         INSERT INTO many SELECT '{}'::jsonb FROM generate_series(1, 10); ANALYZE many; \
         SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0; SET min_parallel_table_scan_size = 0;",
    )
    .unwrap();
    let plan = c
        .query("EXPLAIN SELECT count(*) FROM many WHERE jsonschema.valid(d, 'par', 1)", &[])
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan.contains("Parallel Seq Scan"), "{plan}");
    let analyzed = c
        .query("EXPLAIN (ANALYZE, COSTS OFF) SELECT count(*) FROM many WHERE jsonschema.valid(d, 'par', 1)", &[])
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n");
    let launched: u32 = analyzed
        .lines()
        .find_map(|l| l.trim().strip_prefix("Workers Launched: "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    assert!(launched > 0, "no parallel worker ran valid():\n{analyzed}");
    let n: i64 = c
        .query_one("SELECT count(*) FROM many WHERE jsonschema.valid(d, 'par', 1)", &[])
        .unwrap()
        .get(0);
    assert_eq!(n, 50000);
}
