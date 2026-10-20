# Benchmarks: pg_jsonschema_registry vs pg_jsonschema `master`

Spec goal 6: measured performance against pg_jsonschema built from its
`master` branch (the version with the compiled-schema type and its
per-callsite + LRU cache), on the same machine and data.

## Environment

| | |
|---|---|
| Date | 2026-10-20 (run started 13:13 UTC) |
| Host | Apple M4 Pro, 14 cores (`sysctl -n hw.ncpu`), Docker Desktop 29.8.2, VM with 14 CPUs / 7.7 GiB |
| Platform | Linux arm64 (aarch64) container |
| Runtime image | `postgres:18` = `postgres@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336` (PostgreSQL 18.6, Debian trixie) |
| Builder image | `rust:1-trixie` = `rust@sha256:e259494b14672950a909b4c54527e19d29cae6ebf980672114c8bb85e5594e4a` (rustc 1.99.1), PGDG `postgresql-server-dev-18` |
| pg_jsonschema | supabase/pg_jsonschema `472830ca7b6ab5d2c9d74d68ee9fc2c07529e60e` (master, 2026-10-01), pgrx 0.19.2, jsonschema 0.58, extension version 0.3.4 |
| pg_jsonschema_registry | `1018a124f445b40a29731d5586eafe5651d1d090`, pgrx 0.19.3, jsonschema 0.59 |
| FHIR schema | HL7 FHIR R4 `fhir.schema.json` from `https://hl7.org/fhir/R4/fhir.schema.json.zip` (sha256 of the JSON `2230406893b4…6a01`, 3.4 MB, draft-06) |

Both extensions are built in release mode with their own `Cargo.toml`
profiles (both `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`).

## How to reproduce

From a clean checkout:

```sh
bench/run.sh > table.md     # table on stdout, raw pgbench/psql output on stderr
```

The run above used the defaults (`DURATION=30`, `INSERT_ROUNDS=5`). The
script builds `bench/Dockerfile`, starts a throwaway container with
`POSTGRES_HOST_AUTH_METHOD=trust`, loads `bench/setup.sql`, runs one pgbench
per scenario, runs `bench/insert_check.sql` five times, prints the table and
removes the container.

## What is measured

- **small/valid**: `SELECT count(*) FROM docs_small WHERE <validation>` over
  100 000 small documents (`{"id": int, "name": text}`), all valid.
  Theirs: `jsonb_matches_compiled_schema((SELECT s FROM theirs_small), doc)`.
  Ours: `jsonschema.valid(doc, 'small', 1)`.
- **fhir/valid**: the same over 10 000 FHIR R4 Patient resources validated
  against the full FHIR R4 schema (`fhir/1` registered in ours; the compiled
  `jsonschema` value stored in a one-row table for theirs). `setup.sql` checks
  that both extensions accept every fixture and reject a broken document
  before anything is timed.
- **insert+check**: `INSERT INTO ins_x SELECT doc FROM docs_small` (100 000
  rows) into a table whose `CHECK` calls the validator, timed with psql
  `\timing`; median of 5 rounds, each in a fresh session on empty tables.
  A `CHECK` cannot hold a subquery, so theirs embeds the small schema as a
  `'...'::json::jsonschema` constant.
- Context rows: theirs' uncached `jsonb_matches_schema(json, jsonb)`, which
  compiles the schema for every row, and an insert into a table with no
  `CHECK`.

pgbench runs `-n -c 1 -T 30 -r` (one client, simple query protocol) with
`max_parallel_workers_per_gather = 0`, so each number is single-core
per-call cost, not throughput under concurrency. The value is pgbench's
per-statement average latency for one full-table query; µs/row is that
divided by the row count (it includes the scan, not only validation).

## Results

| scenario | rows | pg_jsonschema master (ms) | pg_jsonschema_registry (ms) | registry / master |
|---|---:|---:|---:|---:|
| small/valid | 100000 | 8.259 (0.08 µs/row) | 11.476 (0.11 µs/row) | 1.39 |
| fhir/valid | 10000 | 7.443 (0.74 µs/row) | 4.601 (0.46 µs/row) | 0.62 |
| insert+check (median of 5) | 100000 | 34.078 | 38.350 | 1.13 |
| *context:* small/valid uncached (`jsonb_matches_schema`, recompiles per row) | 100000 | 272.739 (2.73 µs/row) | n/a | |
| *context:* insert, no CHECK | 100000 | 27.389 | 27.389 | |

Raw insert rounds (ms): plain 30.429 27.071 27.389 27.497 27.132; theirs
36.261 34.078 34.450 32.940 33.265; ours 47.508 39.316 38.350 37.725 37.401.

## Interpretation

Both cached paths are 25–35× faster than recompiling per row (2.73 µs/row
uncached vs 0.08–0.11 µs/row cached on the small schema). On the small schema the registry is slower: 32 ns more per
call in the scan (1.39×) and, after subtracting the 27.4 ms plain-insert
cost, 11.0 ms vs 6.7 ms of CHECK overhead per 100 000 inserts (about 43 ns
more per row). When validation itself is that cheap, fixed per-call overhead
dominates, and ours has more of it: `AcceptInvalidationMessages()` on every
call, converting the `name` text argument and building a `String` key, and a
hash-map lookup, where theirs reuses the validator from the call site's
`fn_extra` slot. On the FHIR schema the registry is faster (0.62×, 0.28 µs
less per document). This run does not isolate why; the two builds differ in
jsonschema crate version (0.59 vs 0.58) and in how the schema reaches the
function (a registry lookup by name vs a 3.4 MB `jsonschema` value passed as
an InitPlan parameter), and either could account for it. First insert rounds are
slower for all three tables (cold session); ours most (47.5 ms), since that
round also loads and compiles the schema once.
