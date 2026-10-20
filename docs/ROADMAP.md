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

## Upstream: `jsonschema` crate recursion guard (draft, not filed)

The crate validates recursively with no depth guard. With
`{"type":"array","items":{"$ref":"#"}}`, `iter_errors` overflowed an 8 MB
stack at about 19 000 nesting levels and `is_valid` between 30 000 and
40 000 (release build, standalone probe). Inside PostgreSQL a stack
overflow is a SIGSEGV that restarts every session, and PostgreSQL's own
`jsonb_in` allows ~18 000 levels at the default `max_stack_depth`, so the
margin is close to zero. This extension caps document nesting at 4096
levels before validating, but the cap counts levels, not stack frames.
Request: a configurable maximum nesting depth (or a `stacker`-style
guard) in `Validator::is_valid`/`iter_errors`/`evaluate`, returning an
error instead of overflowing. Repository: github.com/Stranger6667/jsonschema.
