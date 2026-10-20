# pg_jsonschema_registry

A PostgreSQL extension that stores JSON Schemas in a table, versions them, and validates `jsonb` documents against them. Schemas are registered by name, each registration gets a version number, schemas can `$ref` each other, and compiled validators are cached in each backend so that `CHECK (jsonschema.validate(...))` constraints and bulk validation do not recompile the schema per row. Failed validations report every error with its instance path, schema path and keyword.

It is written in Rust with [pgrx](https://github.com/pgcentralfoundation/pgrx) and validates with the [`jsonschema`](https://docs.rs/jsonschema) crate. Drafts 4, 6, 7, 2019-09 and 2020-12 are supported. Tested on PostgreSQL 15 to 18 (CI matrix; the author develops on 18).

## Install

Build and install with a Rust toolchain and the matching `cargo-pgrx` (the version pinned in `Cargo.toml`, currently 0.19.3):

```sh
cargo install --locked cargo-pgrx --version 0.19.3
cargo pgrx init --pg18=$(which pg_config)     # once; use --pg15 .. --pg17 for other versions
cargo pgrx install --release
```

Then, in each database:

```sql
CREATE EXTENSION pg_jsonschema_registry;
```

The extension creates the schema `jsonschema`. All functions and tables live there.

## Quick start

Register a schema. `register(name, schema)` returns the new version number.

```sql
SELECT jsonschema.register('address', '{
  "$id": "https://example.com/address",
  "type": "object",
  "required": ["street", "city"],
  "properties": {
    "street": {"type": "string", "minLength": 1},
    "city":   {"type": "string"}
  }
}');
-- 1
```

Reference it from another schema by its `$id`. (`urn:pg-jsonschema-registry:address:1`, that is `urn:pg-jsonschema-registry:<name>:<version>`, works as well, for schemas with or without `$id`.)

```sql
SELECT jsonschema.register('customer', '{
  "type": "object",
  "required": ["id", "address"],
  "properties": {
    "id":      {"type": "integer"},
    "address": {"$ref": "https://example.com/address"}
  }
}');
-- 1
```

Validate:

```sql
SELECT jsonschema.valid('{"id": 1, "address": {"street": "Main St", "city": "Oslo"}}', 'customer', 1);
-- t

-- without a version: the latest one
SELECT jsonschema.valid('{"id": 1, "address": {"street": "", "city": "Oslo"}}', 'customer');
-- f
```

Get every error as `jsonb`:

```sql
SELECT jsonb_pretty(jsonschema.errors('{"id": "x", "address": {"city": "Oslo"}}', 'customer', 1));
-- [
--     {
--         "keyword": "type",
--         "message": "\"x\" is not of type \"integer\"",
--         "schema_path": "/properties/id/type",
--         "instance_path": "/id"
--     },
--     {
--         "keyword": "required",
--         "message": "\"street\" is a required property",
--         "schema_path": "/required",
--         "instance_path": "/address"
--     }
-- ]
```

Enforce a schema in a constraint:

```sql
CREATE TABLE customers (
  doc jsonb NOT NULL CHECK (jsonschema.validate(doc, 'customer', 1))
);

INSERT INTO customers VALUES ('{"id": 1, "address": {"street": "Main St", "city": "Oslo"}}');   -- ok
INSERT INTO customers VALUES ('{"id": 2, "address": {"city": "Oslo"}}');
-- ERROR:  document does not match schema customer version 1: "street" is a required property at /address
-- DETAIL:  1 error(s): [{"instance_path":"/address","keyword":"required","message":"\"street\" is a required property","schema_path":"/required"}]
```

Always give constraints an explicit version, as above, so the constraint keeps meaning the same thing when a newer version is registered.

Versions are immutable. To change a schema, register a new version; to remove one, `unregister` it:

```sql
SELECT jsonschema.register('customer', '{"type": "object"}');   -- 2
SELECT jsonschema.latest_version('customer');                   -- 2
SELECT jsonschema.get('customer', 2);                           -- {"type": "object"}
SELECT * FROM jsonschema.schemas;                               -- name, version, draft, id_uri, created_at, created_by, refs
SELECT jsonschema.unregister('address', 1);
-- ERROR:  schema address version 1 is referenced by customer/1
```

## Function reference

All functions are in schema `jsonschema`, and all are `STRICT` (a NULL argument returns NULL). `doc` is `jsonb`; `name` is `text`; `version` is `int`.

| Function | Returns | Volatility | Notes |
|---|---|---|---|
| `register(name, schema jsonb)` | `int` | volatile, parallel unsafe | Registers the next version (max + 1, starting at 1) and returns it. |
| `register(name, version, schema jsonb)` | `int` | volatile, parallel unsafe | Registers an explicit version, which must be positive and greater than the current maximum for that name. Returns it. |
| `unregister(name, version)` | `void` | volatile, parallel unsafe | Deletes one version. Refused while another registered schema references it. |
| `valid(doc, name, version)` | `boolean` | stable, parallel safe | True or false. Raises if the schema is not registered. |
| `valid(doc, name)` | `boolean` | stable, parallel safe | Same, for the latest version. |
| `errors(doc, name, version)` | `jsonb` | stable, parallel safe | Array of `{keyword, message, schema_path, instance_path}`; `[]` when valid. |
| `errors(doc, name)` | `jsonb` | stable, parallel safe | Same, for the latest version. |
| `validate(doc, name, version)` | `boolean` | stable, parallel safe | Returns true, or raises a check violation. Meant for `CHECK` constraints. |
| `validate(doc, name)` | `boolean` | stable, parallel safe | Same, for the latest version. |
| `get(name, version)` | `jsonb` | stable, parallel safe | The stored schema. |
| `latest_version(name)` | `int` | stable, parallel safe | Highest registered version of `name`. |

Tables and view (all in schema `jsonschema`, readable by everyone): `registry` (one row per name and version, with the schema, detected draft, creator and creation time), `ids` (the top-level `$id` of each schema that has one), `refs` (which schema references which), and the view `schemas` that joins them.

Errors and SQLSTATEs:

| Condition | SQLSTATE | Message |
|---|---|---|
| `register`: empty name | `22023` | `name must not be empty` |
| `register`: version not positive | `22023` | `version must be positive` |
| `register`: version not above the current maximum | `22023` | `version must be greater than <max>` |
| `register`: schema is not a valid JSON Schema | `22023` | `invalid JSON Schema: ...` |
| `register`: schema does not compile (for example a `$ref` that is not registered) | `22023` | `cannot compile schema: ...` |
| `register`: the `$id` is already registered | `42710` | `$id <uri> is already registered by <name>/<version>` |
| `register` / `unregister`: caller lacks table privileges | `42501` | PostgreSQL's permission denied error |
| `unregister`: version is referenced by other schemas | `2BP01` | `schema <name> version <v> is referenced by <name>/<v>, ...` |
| `valid`, `errors`, `validate`, `get`, `unregister`: unknown name/version | `42704` | `schema <name> version <v> is not registered` |
| `latest_version` and the no-version overloads: name has no versions | `42704` | `schema <name> has no registered versions` |
| `validate`: document does not match | `23514` | `document does not match schema <name> version <v>: <first error> at <instance path>` |

For `validate`, the DETAIL line is `<n> error(s): [...]` with every error as a JSON array. For a large document with many errors it can be large.

## Privileges

- Everyone can validate. The extension grants `USAGE` on schema `jsonschema` and `SELECT` on its tables and view to `PUBLIC`, and the functions run as the caller, so any role that can write to a table with a `CHECK (jsonschema.validate(...))` can evaluate it.
- Only superusers and the extension owner can `register` or `unregister`. The functions run as the caller and need `INSERT` / `DELETE` on the registry tables, which the extension grants to nobody. A DBA opens it up with ordinary grants:

  ```sql
  GRANT INSERT, DELETE ON ALL TABLES IN SCHEMA jsonschema TO schema_admins;
  ```

  Run that `GRANT` outside heavy registration traffic. It takes strong locks on the registry tables, and in the worst case it can fail with a transient deadlock error against a concurrent `register`. Rerun it if that happens.
- Nothing uses `SECURITY DEFINER`, and the extension creates no roles.

## Caching and consistency

Each backend keeps its own cache of compiled validators, keyed by schema name and version. `register` and `unregister` queue a relcache invalidation of `jsonschema.registry`, which PostgreSQL delivers to every backend, and the backend clears its cache when it processes it.

The guarantee: the validator that is served always matches the registry row that the current transaction's snapshot sees, with the same consistency semantics PostgreSQL gives its own relcache and syscache. `register` and `unregister` are transactional, so a rolled-back registration is never served, and a registration becomes visible to other sessions when it commits. In a transaction that observes a concurrent registry change, validators are recompiled on each call for the rest of that transaction; this affects performance only, not results.

A cached validator is never served after the backend has processed an invalidation for the registry. The details are in the module documentation of [`src/cache.rs`](src/cache.rs).

## Backup and restore

`pg_dump` includes the registry rows (they are extension configuration tables, so the dump carries their data, and the dump holds `CREATE EXTENSION`, not the tables' definitions). The catch is ordering. `pg_dump` and `pg_restore` restore table data in schema-name order. If a table whose `CHECK` constraint calls `jsonschema.validate` lives in a schema that sorts before `jsonschema` (for example `app`), its data is restored before the registry rows exist, and the restore fails with `schema ... is not registered`. A table in `public` is fine, as `jsonschema` sorts first.

If any such table exists, restore a custom-format dump in this order:

```sh
pg_dump -Fc -f db.dump mydb
createdb mydb_restored

pg_restore -d mydb_restored --exit-on-error --section=pre-data db.dump
pg_restore -d mydb_restored --exit-on-error --data-only --schema=jsonschema db.dump
pg_restore -d mydb_restored --exit-on-error --section=data --exclude-schema=jsonschema db.dump
pg_restore -d mydb_restored --exit-on-error --section=post-data db.dump
```

`scripts/dump-restore-test.sh` runs both the plain and the ordered case (table in `public`, table in `app`) against a local server and checks that the constraint still rejects invalid documents afterwards.

## Benchmarks

[`docs/benchmarks.md`](docs/benchmarks.md) compares this extension with Supabase's `pg_jsonschema` built from `master` (the version with the compiled `jsonschema` type), on one machine, single client, one run per row. Headlines:

- On a tiny schema, this extension is slower per row, by about 50 ns (0.118 µs vs 0.068 µs per row; about 11 ms vs 6 ms of `CHECK` overhead per 100 000 inserts).
- On the 3.4 MB FHIR R4 schema the per-document cost is comparable (0.455 µs vs 0.531 µs, from one two-point fit; the document does not claim a meaningful difference).
- Both are far faster than validating without a cached compiled schema (2.69 µs per row for `pg_jsonschema`'s uncached `jsonb_matches_schema` on the small table).

See the document for the method, the caveats and the raw numbers.

## Comparison with other extensions

Compiled from each project's README as of 2026-10; check them for current details.

| | Stored schemas | Versioning | Cross-schema `$ref` by `$id` | Error output | Drafts | Language |
|---|---|---|---|---|---|---|
| pg_jsonschema_registry | yes, in a table | yes, per name | yes, plus `urn:pg-jsonschema-registry:<name>:<version>` | `errors()` returns every error with paths; `validate()` raises with all errors in DETAIL | 4, 6, 7, 2019-09, 2020-12 | Rust |
| [pg_jsonschema](https://github.com/supabase/pg_jsonschema) (Supabase) | no; schemas are passed as arguments (`master` adds a compiled `jsonschema` type, cached per callsite, not in a release as of 2026-10) | no | not documented | `*_matches_schema` return a boolean; `jsonschema_validation_errors` returns `text[]` of messages | whatever the `jsonschema` crate supports (README does not list them) | Rust |
| [jsonschema](https://github.com/theory/pg-jsonschema-boon) (PGXN, theory) | no; schemas are passed on every call | no | yes, within one call (variadic schemas, matched by `$id`) | boolean | 4, 6, 7, 2019-09, 2020-12 (excluding optional features) | Rust (boon) |
| [is_jsonb_valid](https://github.com/furstenheim/is_jsonb_valid) | no | no | `$ref` limited to references nested at root; no remote refs | boolean | 4 and 7; `format` not supported | C |
| [json_schema_validate](https://github.com/adunstan/json_schema_validate) (Andrew Dunstan, described in an EDB blog post) | no | no | not documented here | boolean | a large subset of draft 7 | C |

## Limitations

- Only a schema's top-level `$id` can be referenced. An `$id` embedded in a subschema is not registered, so other schemas cannot `$ref` it.
- References are resolved against the registry only. There are no remote (HTTP or file) refs, ever.
- `unregister` checks references from other registered schemas, not from `CHECK` constraints. If a table constraint calls `validate(doc, 'name', 1)` and you unregister that version, the constraint starts raising `schema ... is not registered`.
- The overloads without a version (`valid(doc, name)`, `errors(doc, name)`, `validate(doc, name)`) look up the latest version in the table on every call. They are not meant for constraints; use them for ad hoc queries.
- `register` and `unregister` are not parallel safe, by design. The validation functions are.
- `validate` puts every error in the DETAIL line, which can be large for big documents with many violations.
- The cache is per backend and rebuilt on first use, so the first call in each new connection pays the compile cost (about 33 ms for the FHIR R4 schema in the benchmark's spot check).

## Development

See [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) for the local setup, tests and lint commands.

## License

[PostgreSQL License](./LICENSE)

Copyright (c) 2026 Oguzhan Unlu.
