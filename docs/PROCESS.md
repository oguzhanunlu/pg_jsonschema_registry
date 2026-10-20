# How v0.1 was built: research, decisions, process, progress

This is the record of the two sessions (2026-10-19 and 2026-10-20) in which
v0.1 went from an empty repository to the `feat/registry-v0.1` branch. It
exists so that the work can be picked up after a pause without re-deriving
anything. The design itself is in
`docs/superpowers/specs/2026-10-20-pg-jsonschema-registry-design.md`, the
task breakdown in
`docs/superpowers/plans/2026-10-20-pg-jsonschema-registry-v0.1.md`.

## 1. Research (2026-10-19)

Two surveys were run before any decision. Their findings, with the dates
they were true:

**Existing PostgreSQL JSON Schema extensions.**

| Extension | Language | Drafts | Stores schemas | Versioning | Cross-schema `$ref` | Notes |
|---|---|---|---|---|---|---|
| supabase/pg_jsonschema (release v0.3.4, 2026-02-11) | Rust/pgrx | per crate | no | no | no | compiles the schema on every call |
| supabase/pg_jsonschema `master` (unreleased) | Rust/pgrx 0.19.2, jsonschema 0.58 | per crate | `jsonschema` column type | no | no | compiled type with per-callsite `fn_extra` slot + per-backend LRU; validates jsonb in place via the crate's `jsonb` representation (added to the crate 2026-09-25) |
| theory/pg-jsonschema-boon (`jsonschema` on PGXN, v0.1.10) | Rust/pgrx, boon | 4–2020-12 | no | no | by `$id`, all schemas passed per call | recompiles per call |
| furstenheim/is_jsonb_valid | C | 4, 7 | no | no | root-anchored only | no `format`; active (PG 19 CI) |
| adunstan/json_schema_validate (EDB blog 2026-03-10) | C | draft-7 subset | `jsonschema_compiled` type | no | local pointers only | dormant since 2026-03 |
| gavinwahl/postgres-json-schema | PL/pgSQL | 4 | no | no | root only | unmaintained since 2023 |

Nobody had a registry: named, versioned, stored schemas with `$ref`
between them. Nothing in PostgreSQL core or any commitfest (56–62) touches
JSON Schema. Distribution: PGXN v1 is alive; Tembo's trunk (`pgt.dev`) and
pgxman were unreachable. Pigsty packages pgrx extensions for PG 14–18.

**Validator libraries.** The Rust `jsonschema` crate (0.59.0, 2026-10-16)
and `boon` (0.6.1) both score 100% on Bowtie for drafts 4, 6, 7, 2019-09
and 2020-12. The `jsonschema` crate also has an in-place Postgres `jsonb`
instance representation and a `Retrieve` trait for custom `$ref`
resolution. No maintained, spec-complete C validator exists (WJElement is
LGPL and draft-04 era; the rest are dead).

## 2. Decisions, in the order they were made

1. **C with a from-scratch validator** was the first choice (2026-10-19):
   Postgres-native, zero dependencies, packagable anywhere, a possible path
   to contrib, and the author's prior validator experience. Scope grew to
   "all five drafts, full compliance, usable as a standalone C library",
   which means owning an ECMA-262 regex engine, decimal arithmetic and all
   `format`s: roughly 15–25k lines. That cost was judged not worth paying.
2. **pgrx + the `jsonschema` crate** was chosen on 2026-10-20 as the
   shortest path to value: validation is a solved problem; the registry is
   the gap. The pure-SQL-on-top-of-pg_jsonschema route was rejected because
   it gives no control over caching or `$ref` resolution.
3. **Integer versions, immutable rows, schema `jsonschema`, plain Postgres
   grants** (no `SECURITY DEFINER`, no extension-created roles): author's
   call, confirmed.
4. **Cache coherence via relcache invalidation** rather than content
   hashing or a DDL-event sequence: `register`/`unregister` call
   `CacheInvalidateRelcacheByRelid`; every backend clears its map in a
   relcache callback. Transactional for free, no shared memory, no
   `shared_preload_libraries`. The snapshot rules on top of it were added
   after reviews found holes (section 4).

## 3. Process

- **Brainstorm → design document → builder check.** A fresh reviewer read
  the design as a builder and asked three questions. One exposed a real
  hole (a name re-registered under the same version would be served stale
  from a cache keyed by row id). That led to the invalidation design.
- **Plan.** Eleven tasks, each with its tests written in the plan, every
  pgrx and crate API name checked against the 0.19.3 / 0.59.0 sources on
  disk before the plan was written.
- **Subagent-driven execution.** For each task: a fresh implementer with
  only its task brief and the interfaces of earlier tasks; a fresh reviewer
  with the diff; fix rounds with scoped re-reviews; a ledger of every
  decision. Then one whole-branch review, one fix wave, one re-review.
- **Rulings.** Whenever a reviewer's finding conflicted with the plan or
  the plan was silent, the controller ruled and recorded why and what it
  would cost if wrong. All rulings are listed in section 5.

## 4. Progress log (all commits are on `feat/registry-v0.1`)

| Task | Commits | Review outcome |
|---|---|---|
| 1 scaffold | 785e007 | clean |
| 2 SPI layer | a2fa9a1, 4840895 | 1 fix round: `insert_id` must not catch a unique violation without a subtransaction |
| 3 retriever + compile | 76b9654 | clean |
| 4 `register` | 6ce42ac | clean |
| 5 cache + `valid` | de9f100, adaf30b, 4cbd668, 31ec134 | 3 fix rounds, see below |
| 6 `errors`, `validate` | ccb2fe9 | clean |
| 7 `get`, `latest_version`, `unregister`, overloads | 5719748 | clean |
| 8 privilege + draft tests | 8da0815 | clean (pg_tests run in parallel backends; a GRANT vs `LOCK TABLE` ordering deadlock was fixed in the test) |
| 9 integration + dump/restore | 1813c76, 1018a12 | **found a defect**: registry reads used pgrx's read-write SPI path and failed in parallel workers (SQLSTATE 25000); fixed, with a regression test |
| 10 benchmark | 1658a7f, e6e6f54 | 1 fix round: separate fixed vs marginal cost on FHIR |
| 11 README, CI, lint | 196b3ef, 4fe7b33, f559013 | 1 fix round: a clippy-driven loop rewrite had broken `dependents()` to one row |
| whole-branch review fix wave | 128627f, a285ef6, de0704b, 91ff1b9, 7d41329, 34570a4 | re-review clean; three minor residuals parked (section 6) |

**The cache's history, because it is the heart of the extension.**

1. First version: clear the map in the relcache callback; immutable rows
   never go stale. Review found: a backend whose transaction snapshot
   predates a change can process the invalidation, miss, re-read the old
   row under its old snapshot, and cache a stale validator (REPEATABLE
   READ, or a read-only statement whose snapshot SPI reuses).
2. Fix: a per-transaction "do not cache" flag set by the callback,
   `AcceptInvalidationMessages()` on the hit path. Residual: a fresh backend
   does not yet know the registry relid.
3. Fix: `invalidate()` stores the relid and marks its own message
   (`OWN_PENDING`) so own changes never disable caching; unknown relid is
   treated conservatively. Residual: the mark survived a subtransaction
   abort. Fix: a subtransaction callback.
4. Whole-branch review found the critical one: a backend's own committed
   message comes back from the shared queue at the next transaction start
   and was treated as foreign, so the transaction right after any
   registration cached nothing (measured ~800× on register-then-validate).
   Fix: set the flag only when `IsTransactionState() && FirstSnapshotSet`,
   justified by commit visibility (`ProcArrayEndTransaction`) preceding
   invalidation send (`AtEOXact_Inval`) in xact.c, verified for REL_15 and
   REL_18.

Every one of those rules has a test that fails when the rule is removed
(`src/api.rs` pg_tests and `tests/integration.rs`).

**Verification at HEAD 34570a4:** `cargo fmt --check` clean;
`cargo clippy --features pg18,pg_test --tests -- -D warnings` clean;
`cargo pgrx test pg18` 74 passed; `scripts/integration-test.sh` 8 passed;
`scripts/dump-restore-test.sh` OK. CI has never run (branch not pushed
before this record was written); PostgreSQL 15–17 builds are untested.

## 5. Rulings made during execution

Each with what it costs if wrong.

1. `Cargo.lock` is committed. Cost: a `.gitignore` line.
2. The generated `.cargo/config.toml` stays (macOS linking). Cost: none.
3. `insert_id` pre-checks the existing owner instead of catching a unique
   violation (catching a Postgres ERROR without a subtransaction, then
   running more SPI, is unsafe). Cost: a theoretical race only if a caller
   bypasses `register`'s `SHARE ROW EXCLUSIVE` lock.
4. Snapshot rule for the cache (flag + `AcceptInvalidationMessages`), spec
   reworded to the precise guarantee. Cost: per-call recompiles for the
   rest of a transaction that observed a concurrent change.
5. Own-vs-foreign attribution and the conservative unknown-relid rule.
   Cost: a narrow misattribution window if a lock is ever taken between
   `invalidate()` and command end.
6. Task 9 was allowed to change `src/registry.rs` to make every read use
   the read-only SPI path. Cost: none.
7. Dump/restore ordering is documented and tested, not "fixed": pg_dump
   restores data in schema/name order and this extension cannot change
   that. Cost: users whose schema sorts before `jsonschema` need the
   documented recipe.
8. No GitHub issue was opened by agents for the slower benchmark rows;
   outward actions are the author's.
9. Final fix-wave scope (section 4, item 4; depth caps; `$id`
   normalisation; triggers). Accepted deviations: `$id` stored RFC 3986
   normalised because the crate normalises every `$ref` before lookup;
   schema depth cap 100 because pgrx's jsonb-to-serde conversion stops at
   127 first; `invalidate_cache()` requires INSERT or DELETE on the
   registry so arbitrary roles cannot flush every backend's cache.

## 6. Parked with rulings (not fixed in v0.1)

- `SET TRANSACTION SNAPSHOT` with a snapshot older than a processed
  same-version re-registration can cache a stale validator. Exotic;
  candidate fix: set the flag on snapshot import.
- No `AFTER TRUNCATE` trigger: a `TRUNCATE jsonschema.registry` leaves warm
  caches elsewhere until the next invalidation. Owner/superuser only.
- `UPDATE` on `jsonschema.ids` / `jsonschema.refs` is neither refused nor
  invalidating. Not a supported operation.
- Triggers are `ENABLE`, not `ENABLE ALWAYS`: logical-replication apply
  and `pg_restore --disable-triggers` bypass them.
- The 4096-level document cap counts levels, not stack frames; a schema
  that recurses through several keywords per level could still overflow
  below it. The real fix is a recursion guard in the crate (see
  `docs/ROADMAP.md` for the issue draft).
- Review-noted test gaps: `errors(doc, name)` / `validate(doc, name)`
  overloads, unregister observed from a second backend, a TOASTed schema
  (only the benchmark covers it), the dump script's INSERT-rejection check
  accepting any error.

## 7. Benchmark, in one paragraph

Against pg_jsonschema `master` (commit 472830ca) in Docker on an M4 Pro,
single client: about 50 ns/row slower on a tiny schema (0.118 vs 0.068
µs/row), comparable per document on the 3.4 MB FHIR schema (0.455 vs 0.531
µs/row; their side pays a fixed ~2.8 ms per statement, so per statement we
are faster), 1.15× slower on insert with a CHECK constraint, and 24–33×
faster than the uncached `jsonb_matches_schema`. One run per row; an
earlier run differed by ~20% on the small scenario. Everything, including
the raw numbers and the fit, is in `docs/benchmarks.md`.
