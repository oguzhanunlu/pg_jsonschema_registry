# Roadmap

Status on 2026-10-20: v0.1 is complete on branch `feat/registry-v0.1`,
reviewed and verified (see `docs/PROCESS.md`), not merged, not released.
Nothing here is scheduled; it is the ordered list of what to do when work
resumes.

## Before merging v0.1

1. **Push and let CI run.** `.github/workflows/ci.yml` has never executed.
   Expect to iterate on the `pgxn/pgxn-tools` matrix (PG 15–18) and on the
   lint job's `pg_config` path assumption.
2. **Verify PostgreSQL 15, 16, 17 builds.** Only 18 has been built. The
   `pg_module_magic!(name, version)` form and every `pg_sys` binding used
   (`FirstSnapshotSet`, `RegisterSubXactCallback`, relcache callbacks) must
   compile against each.
3. **Decide the slower rows.** The benchmark shows ~50 ns/row more than
   pg_jsonschema's compiled path on a tiny schema. Likely causes (not
   profiled): `AcceptInvalidationMessages()` per call, a `String` cache key
   per call, hash lookup vs their `fn_extra` callsite slot. Options: profile
   first; add a per-callsite `fn_extra` slot (the spec's deferred "L1");
   key the cache by `(name, version)` without allocating. Opening a GitHub
   issue for this is the author's call.
4. **Merge** `feat/registry-v0.1` into `main`.

## v0.2 candidates, most valuable first

1. **Schema evolution checks.** The reason the registry has versions:
   compare version N+1 with N and report whether the change is backward
   compatible (new optional property), additive, or breaking (new required
   property, narrowed type). SchemaVer semantics (`MODEL-REVISION-ADDITION`)
   can be layered on the integer version as metadata without changing the
   table. This is the feature no other extension has and the one closest
   to the author's Iglu background.
2. **Embedded `$id`s.** `jsonschema.ids` already has one row per URI; a
   keyword-aware walk at `register` can add every subschema `$id` so a
   `$ref` to an embedded resource of another registered schema resolves.
3. **Parked correctness items** (from `docs/PROCESS.md` §6): `AFTER
   TRUNCATE` trigger; refuse `UPDATE` on `ids`/`refs` or make it
   invalidating; `ENABLE ALWAYS` triggers if replicating the registry is
   supported; set the "do not cache" flag on snapshot import.
4. **Warm-up function.** `jsonschema.warm_cache()` compiling every
   registered schema in the current backend, for connection-pool start-up
   hooks.
5. **`unregister` awareness of CHECK constraints.** Scan `pg_constraint`
   for calls to `jsonschema.valid`/`validate` with constant arguments and
   refuse to unregister a schema a constraint still uses.
6. **Deprecation flag** on a version (`jsonschema.deprecate(name, version)`)
   that `latest_version` skips.
7. **Upgrade script** (`pg_jsonschema_registry--0.1.0--0.2.0.sql`). Note
   that `registry_immutable` must be disabled inside any upgrade that
   has to rewrite rows.

## Release

- PGXN v1 (`META.json`, a source zip from `cargo pgrx package`); the
  `pgxn/pgxn-tools` image already used by CI has `pgxn-bundle`/release
  helpers. Pigsty packages pgrx extensions for PG 14–18 and is the practical
  route to RPM/DEB.
- Bowtie is not applicable (it benchmarks validators, and ours is the
  crate's).

## Outward actions left to the author

Agents never open issues, push to shared branches, or publish on the
author's behalf. These two were identified during v0.1 and are drafted here
so they can be filed in a minute when the time comes.

### 1. Repository issue: per-row overhead on small schemas (not filed)

Context: the v0.1 benchmark (`docs/benchmarks.md`, run2, 2026-10-20,
Docker on an M4 Pro, single client) measured our marginal cost per row at
0.118 µs against 0.068 µs for pg_jsonschema `master`'s
`jsonb_matches_compiled_schema` on a two-property schema; that is ~50 ns per
row, 1.6–1.7× in that scenario, and 1.15× on a 100k-row `INSERT` with a
`CHECK` constraint (11.1 vs 6.1 ms of constraint overhead). On the 3.4 MB
FHIR schema the per-document costs are comparable (0.455 vs 0.531 µs).
The plan's benchmark step said to open an issue for any row where we are
slower.

Suggested title: "valid() is ~50 ns/row slower than pg_jsonschema's
compiled path on small schemas".

Suggested body: the numbers above with a link to `docs/benchmarks.md`
and the exact `bench/run.sh` invocation; the likely, unprofiled causes in
`src/cache.rs::get_or_compile`: (a) `AcceptInvalidationMessages()` on
every call, (b) a `String` allocation for the `(name, version)` cache key
on every call, (c) a `HashMap` probe where pg_jsonschema uses a
per-callsite `fn_extra` slot; proposed steps: profile with `perf` inside
the benchmark container first, then try a borrowed-key lookup
(`HashMap<(String, i32), _>` probed via `(&str, i32)` with the `hashbrown`
raw-entry API or a two-level map), then an `fn_extra` callsite cache
holding the `Arc<Validator>` plus the generation it was taken at, which
would also remove the per-call `AcceptInvalidationMessages()` for the
constant-argument case by piggybacking on the generation counter.
Acceptance: the small-schema marginal cost within 10% of pg_jsonschema's,
with the two-session coherence tests still passing.

### 2. Upstream issue: `jsonschema` crate recursion guard (not filed)

Target: github.com/Stranger6667/jsonschema (crate `jsonschema` 0.59.0).

Suggested title: "No recursion limit during validation: deeply nested
instances overflow the stack".

Suggested body follows. The crate validates recursively with no depth guard. With
`{"type":"array","items":{"$ref":"#"}}`, `iter_errors` overflowed an 8 MB
stack at about 19 000 nesting levels and `is_valid` between 30 000 and
40 000 (release build, standalone probe). Inside PostgreSQL a stack
overflow is a SIGSEGV that restarts every session, and PostgreSQL's own
`jsonb_in` allows ~18 000 levels at the default `max_stack_depth`, so the
margin is close to zero. This extension caps document nesting at 4096
levels before validating, but the cap counts levels, not stack frames.
Request: a configurable maximum nesting depth (or a `stacker`-style
guard) in `Validator::is_valid`/`iter_errors`/`evaluate`, returning an
error instead of overflowing. Include the standalone reproduction: a
release-build binary that builds the validator for the recursive schema
above and calls `iter_errors` on a `serde_json::Value` of nested arrays at
increasing depth until it crashes; and note the Postgres angle (any
database role that can insert into a table whose CHECK uses a recursive
schema can trigger it).
