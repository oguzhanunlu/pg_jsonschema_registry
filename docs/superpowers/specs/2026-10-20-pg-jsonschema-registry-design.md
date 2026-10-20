# pg_jsonschema_registry — design (v0.1)

Date: 2026-10-20. Status: approved 2026-10-20.

## Intent

PostgreSQL has native `jsonb` but no native JSON Schema support. The existing
extensions all validate a document against a schema passed inline on every
call. None of them store schemas, version them, resolve `$ref` between stored
schemas, or give good error output. pg_jsonschema_registry fills that gap: a
registry of named, versioned, immutable JSON Schemas living inside the
database, with validation functions that are cheap to call repeatedly and
pleasant to use in `CHECK` constraints.

The validator itself is not our problem to solve. The Rust `jsonschema` crate
passes the official test suite for drafts 4, 6, 7, 2019-09 and 2020-12,
validates Postgres `jsonb` in place, and exposes hooks to resolve `$ref` from
our own table. Every hour goes into the registry.

## Goals (v0.1)

1. Register a schema under a name; the registry assigns an immutable version.
2. Validate a `jsonb` document against `(name, version)` with a boolean, with
   a list of errors, or by raising a descriptive `ERROR` (for constraints).
3. `$ref` between registered schemas, resolved from the registry, never from
   the network or the filesystem.
4. Compile each `(name, version)` once per backend and reuse it; the cache
   can never serve a stale validator.
5. Survive `pg_dump` / `pg_restore`, including databases whose tables carry
   `CHECK` constraints that call the registry.
6. Measured, published performance against pg_jsonschema built from its
   `master` branch (the version with compiled-schema caching), on the same
   machine and data.

## Non-goals (v0.1)

- Remote references (`http://`, `file://`). Never, not just not yet.
- Schema evolution rules (backward/forward compatibility checks between
  versions). Planned for later; the data model must not prevent it.
- A per-callsite cache in `fn_extra` (pg_jsonschema's "L1"). Only if the
  benchmark shows the per-backend cache is not enough.
- The text `json` type. `jsonb` only.
- Custom formats, custom vocabularies, configurable regex engines.
- A GUI, HTTP API, or anything outside SQL.

## Anti-goals

- Being a thin wrapper that recompiles per call. If the cache does not work,
  the project has no reason to exist.
- Silent behaviour: registering an invalid schema, referencing a missing
  schema, or deleting a schema another one depends on must all fail loudly.

## Constraints

- Rust + pgrx 0.19.x; `jsonschema` crate 0.59+ with `default-features =
  false, features = ["jsonb", "arbitrary-precision"]` (no HTTP/TLS stack in a
  Postgres backend).
- PostgreSQL 15 through 18 declared (pgrx features `pg15`..`pg18`);
  developed and tested locally on 18; the CI matrix covers the rest.
- PostgreSQL License.
- All functions that read the registry are `STABLE`, never `IMMUTABLE`, and
  are honest about it in the documentation of `CHECK` constraint usage.

## SQL surface

Everything lives in a schema named `jsonschema` created by the extension.
(My call: a schema avoids name clashes with pg_jsonschema, EDB's
json_schema_validate and the PGXN `jsonschema` extension, all of which put
similarly named functions in `public`. `jsonschema.valid(...)` reads well
in a constraint.)

### Registering

```sql
jsonschema.register(name text, schema jsonb) RETURNS int          -- next version
jsonschema.register(name text, version int, schema jsonb) RETURNS int
```

- Fails if `schema` is not a valid schema for its draft (draft from
  `$schema`, default 2020-12 when absent), if any `$ref` it contains cannot
  be resolved from the registry, or if it carries an `$id` already
  registered under a different `(name, version)`.
- Without `version`: assigns `max(version) + 1` for that name, starting at 1.
- With `version`: must be greater than the current maximum for that name
  (gaps allowed; this lets users keep their own numbering).
- Returns the version. The row is immutable from this point.

### Validating

```sql
jsonschema.valid(doc jsonb, name text, version int) RETURNS boolean
jsonschema.errors(doc jsonb, name text, version int) RETURNS jsonb
jsonschema.validate(doc jsonb, name text, version int) RETURNS boolean
```

- `valid`: true/false. Stops at the first failure.
- `errors`: a JSON array, empty when valid, otherwise one object per failure:
  `{"instance_path": "/items/2/price", "schema_path": "/properties/items/items/properties/price/minimum", "keyword": "minimum", "message": "..."}`.
- `validate`: returns true, or raises `ERROR` with SQLSTATE `23514`
  (check_violation), the first error in the message and the full list in the
  `DETAIL`, so a `CHECK (jsonschema.validate(payload, 'order', 1))` tells
  the user *what* was wrong instead of "violates check constraint".
- All three: `STRICT`, `STABLE`, `PARALLEL SAFE`. Unknown `(name, version)`
  raises an error (never returns false).

Convenience overloads without `version` resolve the **latest** version at
call time. They are for application queries, not for constraints, and the
documentation says so.

### Reading and managing

```sql
jsonschema.get(name text, version int) RETURNS jsonb
jsonschema.latest_version(name text) RETURNS int
jsonschema.unregister(name text, version int) RETURNS void
```

- `unregister` fails if another registered schema `$ref`s this one. It does
  not detect `CHECK` constraints that reference it (see Open risks). After
  `unregister`, the same `(name, version)` may be registered again with a
  different document; every backend sees the new one (see Caching).

Plus a view `jsonschema.schemas` (name, version, draft, id_uri, created_at,
created_by, refs) for humans.

## Data model

```sql
CREATE TABLE jsonschema.registry (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, -- never reused
    name        text    NOT NULL,
    version     int     NOT NULL CHECK (version > 0),
    schema      jsonb   NOT NULL,
    draft       text    NOT NULL,            -- '2020-12', '2019-09', '7', '6', '4'
    created_at  timestamptz NOT NULL DEFAULT now(),
    created_by  text NOT NULL DEFAULT current_user,
    UNIQUE (name, version)
);

-- URIs under which a registered document can be found by $ref
CREATE TABLE jsonschema.ids (
    uri         text   PRIMARY KEY,
    schema_id   bigint NOT NULL REFERENCES jsonschema.registry(id) ON DELETE CASCADE
);

CREATE TABLE jsonschema.refs (
    schema_id   bigint NOT NULL REFERENCES jsonschema.registry(id) ON DELETE CASCADE,
    ref_id      bigint NOT NULL REFERENCES jsonschema.registry(id),
    PRIMARY KEY (schema_id, ref_id)
);
```

All three tables and the identity sequence are registered with
`pg_extension_config_dump` so `pg_dump` includes their rows and a restored
database does not reuse ids. Rows in `registry` are never updated; the
functions never issue an UPDATE.

In v0.1 `jsonschema.ids` holds exactly one row per registered schema: its
top-level `$id` if present. (My call: embedded `$id`s in subschemas are a
real-world pattern, but collecting them needs a keyword-aware walk of the
document. The side table means adding that later changes no contracts; until
then a `$ref` to an embedded `$id` of another registered schema fails at
`register` with a clear message.)

**Why an integer version (my call, the one to check first).** The two
alternatives were SchemaVer (`1-0-0`, Iglu's scheme) and free text. Integer
versions make "latest" unambiguous, need no parsing, and keep v0.1 small.
SchemaVer semantics (breaking vs. compatible changes) belong to the schema
evolution feature, which can be layered on later as metadata without
changing this table. Free text was rejected because "latest" would mean
"last registered", which surprises people.

## `$ref` resolution

The crate resolves references through a `Retrieve` implementation we
supply. Ours maps a URI to a registered schema:

1. A URI present in `jsonschema.ids` resolves to that schema (top-level
   `$id` only in v0.1, see Data model).
2. A URI of the form `urn:pg-jsonschema-registry:<name>:<version>` resolves
   by name and version. (My call on the scheme: a URN avoids implying
   anything fetchable. Open to a shorter spelling.)
3. Anything else fails, and so does `register`.

`register` rejects a schema whose top-level `$id` is already in
`jsonschema.ids`.

Resolution happens at `register` time, where it is also recorded in
`jsonschema.refs`, and again at compile time in each backend. Because rows
are immutable, both resolutions always see the same documents.

## Caching

Per backend, a `thread_local!` map from `(name, version)` to
`Arc<jsonschema::Validator<Jsonb>>`, unbounded in v0.1 (a registry has tens
to hundreds of schemas, not millions; revisit if the benchmark says so).

On a miss, the function runs one SPI query (`SELECT id, schema FROM
jsonschema.registry WHERE name = $1 AND version = $2`), compiles with the
registry retriever, and inserts. Steady-state calls do no SPI at all.

**Why this cannot go stale.** Immutable rows and never-reused ids cover
"the schema I compiled changed", but not "the name now points somewhere
else": after `unregister('order', 1)` plus a re-`register` of the same
`(name, version)`, a warm backend would keep answering from the old
validator, and a `register` + `valid` followed by `ROLLBACK` would leave a
phantom entry. (Found by the builder check.) So the cache is invalidated
through Postgres's own mechanism rather than trusted:

- `register` and `unregister` call `CacheInvalidateRelcacheByRelid` on
  `jsonschema.registry`. Postgres queues that as a shared-invalidation
  message, delivers it to every backend at commit, and discards it on
  abort. It is transactional for free, needs no shared memory and no
  `shared_preload_libraries`.
- The extension registers a relcache callback (`CacheRegisterRelcacheCallback`
  in `_PG_init`) that clears the whole per-backend map whenever the
  registry's relid is invalidated. The local backend processes its own
  message at command end and on abort too, which closes the rollback case.
- A full clear is fine: registries hold tens to hundreds of schemas, and
  invalidation happens on registration events, not on validation. Spurious
  clears (VACUUM, DDL on the table) are harmless.

The cost is zero on the validation path: backends already process pending
invalidation messages at statement start. The behaviour is verified by a
test with two sessions: session A validates (warming its cache), session B
unregisters and re-registers under the same `(name, version)`, session A
must see the new schema; and a rollback variant.

Documents are validated in place through the crate's `Jsonb` representation
(no `serde_json::Value` is built), following pg_jsonschema's `RawJsonb`
argument approach.

## Privileges

Decided (2026-10-20): ordinary Postgres table privileges, no
`SECURITY DEFINER` anywhere, no extension-created roles:

- The extension script grants `SELECT` on all three tables and `USAGE` on
  the `jsonschema` schema to `PUBLIC`. Validation functions run as the
  caller, so any role that can write to a table with a
  `CHECK (jsonschema.validate(...))` can also evaluate the constraint.
- `register` and `unregister` run as the caller too; they succeed only for
  roles holding `INSERT` / `DELETE` on the registry tables. The extension
  grants those to nobody, so out of the box only superusers and the
  extension owner can register, and a DBA opens it up with ordinary
  `GRANT INSERT, DELETE ON ALL TABLES IN SCHEMA jsonschema TO schema_admins`.
- Grants made by the extension script are recreated by `CREATE EXTENSION`
  on restore; grants the DBA added afterwards are dumped as usual.
- The test suite includes a non-superuser role that can validate but not
  register.

## Dump / restore

`pg_extension_config_dump` makes pg_dump emit the registry rows. The risk is
ordering: if a user table has `CHECK (jsonschema.validate(...))` and its data
is restored before the registry rows, the restore fails. This is tested, not
assumed: a `#[pg_test]`-adjacent integration test dumps a database with such
a constraint and restores it into a fresh one. If ordering turns out wrong,
the documented mitigation is `pg_restore --section=pre-data`, registry data,
then the rest; and the README says so.

## Testing

- `#[pg_test]` coverage for every function, including: invalid schema
  rejected; unresolvable `$ref` rejected; duplicate `$id` rejected;
  `unregister` blocked by a dependent; the cache serving the second call
  (observable via a debug GUC `jsonschema.cache_stats` or a test-only
  counter); errors output shape; `validate` raising 23514 with DETAIL.
- Draft coverage smoke tests: one schema per draft registered and used.
- The dump/restore integration test above.
- Benchmark (Docker, Linux, same image for both): pg_jsonschema `master`'s
  `jsonb_matches_compiled_schema` vs. `jsonschema.valid`, over (a) a small
  schema, (b) a large real-world schema (FHIR Patient, as pg_jsonschema's own
  benchmark uses), (c) `INSERT` into a table with a `CHECK` constraint.
  Results go into `docs/benchmarks.md` with the exact commands.

## Open risks

- `CHECK` constraints calling a `STABLE` function that reads a table. It
  works in practice and Postgres does not enforce immutability for `CHECK`,
  but it is unusual. Document it plainly, and document why `IMMUTABLE` would
  be a lie here.
- `unregister` cannot see `CHECK` constraints that use the schema. v0.1
  documents this; a later version can scan `pg_constraint` for calls to our
  functions with constant arguments.
- Parallel query: functions are `PARALLEL SAFE` because reads via SPI are
  allowed in workers and the cache is per process. Verified by a test that
  forces a parallel seq scan.

## Left to the builder

- Exact Rust module layout, error types, and how `Retrieve` gets SPI access
  during compilation (likely compile inside `Spi::connect`).
- Message wording for errors and the exact `errors()` object keys beyond the
  four listed.
- Whether `jsonschema.schemas` is a view or a function returning a table.
- CI setup (GitHub Actions matrix over PG 15–18 using `pgxn/pgxn-tools`).
