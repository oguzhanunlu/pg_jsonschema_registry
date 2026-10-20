# Benchmarks: pg_jsonschema_registry vs pg_jsonschema `master`

Spec goal 6: measured performance against pg_jsonschema built from its
`master` branch (the version with the compiled-schema type and its
per-callsite + LRU cache), on the same machine and data.

## Environment

| | |
|---|---|
| Date | 2026-10-20 (run started 13:22 UTC) |
| Host | Apple M4 Pro, 14 cores (`sysctl -n hw.ncpu`), Docker Desktop 29.8.2, VM with 14 CPUs / 7.7 GiB |
| Platform | Linux arm64 (aarch64) container |
| Runtime image | `postgres:18` = `postgres@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336` (PostgreSQL 18.6, Debian trixie) |
| Builder image | `rust:1-trixie` = `rust@sha256:e259494b14672950a909b4c54527e19d29cae6ebf980672114c8bb85e5594e4a` (rustc 1.99.1), PGDG `postgresql-server-dev-18` |
| pg_jsonschema | supabase/pg_jsonschema `472830ca7b6ab5d2c9d74d68ee9fc2c07529e60e` (master, 2026-10-01), pgrx 0.19.2, jsonschema 0.58, extension version 0.3.4 |
| pg_jsonschema_registry | `1018a124f445b40a29731d5586eafe5651d1d090`, pgrx 0.19.3, jsonschema 0.59 |
| FHIR schema | HL7 FHIR R4 `fhir.schema.json` from `https://hl7.org/fhir/R4/fhir.schema.json.zip` (sha256 of the JSON `2230406893b4…6a01`, 3.4 MB, draft-06) |

Both base images are pinned by these digests in `bench/Dockerfile`
(`RUST_IMAGE`, `PG_IMAGE` build args), so the tags floating does not change a
re-run.

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
  before anything is timed (the negative FHIR case is a fixture-shaped
  Patient whose only defect is an invalid `gender` enum value).
- **Two row counts per scenario.** Each validation query also runs over the
  first 10 000 small / first 1 000 FHIR rows (`docs_small_10k`,
  `docs_fhir_1k`, same rows, same plan shape). Fitting
  t(n) = fixed + n × marginal through the two points separates a
  per-statement fixed cost from the per-row marginal cost (marginal =
  (t_big − t_small) / (n_big − n_small); fixed = t_small − n_small × marginal).
  Marginal includes the scan's per-row work as well as validation.
- **insert+check**: `INSERT INTO ins_x SELECT doc FROM docs_small` (100 000
  rows) into a table whose `CHECK` calls the validator, timed with psql
  `\timing`; median of 5 rounds, each in a fresh psql session on empty
  tables. CHECK overhead is the difference of medians against the no-CHECK
  baseline table.
  A `CHECK` cannot hold a subquery, so theirs embeds the small schema as a
  `'...'::json::jsonschema` constant.
- Context rows: theirs' uncached `jsonb_matches_schema(json, jsonb)`, which
  compiles the schema for every row, and an insert into a table with no
  `CHECK`.

pgbench runs `-n -c 1 -T 30 -r` (one client, simple query protocol) with
`max_parallel_workers_per_gather = 0`, so each number is single-core
per-call cost, not throughput under concurrency. The value is pgbench's
per-statement average latency for one full-table query.

Caveats: one run per row (no repeated-run variance). Run order is fixed:
theirs before ours in every pgbench pair, and plain, theirs, ours within each
insert round. No separate warm-up: each pgbench connection's first
transaction compiles the schema (theirs and ours alike) inside the 30 s
window. The FHIR compile is the largest (a spot check with psql `\timing`
in a fresh session: first single-row call 32.6 ms ours, 33.6 ms theirs), so
the effect is under 0.2% of a 30 s run.

## Results

Per-statement latency (ms), one `SELECT count(*) ... WHERE <validation>` over the whole table:

| scenario | rows | pg_jsonschema master (ms) | pg_jsonschema_registry (ms) | registry / master |
|---|---:|---:|---:|---:|
| small/valid | 100000 | 6.926 | 11.862 | 1.71 |
| small/valid | 10000 | 0.766 | 1.221 | 1.59 |
| fhir/valid | 10000 | 8.086 | 4.574 | 0.57 |
| fhir/valid | 1000 | 3.308 | 0.480 | 0.15 |
| insert+check (median of 5) | 100000 | 33.923 | 38.964 | 1.15 |
| *context:* small/valid uncached (`jsonb_matches_schema`, recompiles per row) | 100000 | 269.151 (2.692 µs/row) | n/a | |
| *context:* insert, no CHECK (shared baseline) | 100000 | 27.815 (same table for both) | | |

Two-point fit, t(n) = fixed + n × marginal (fixed includes the scan's per-statement work; marginal includes scan + validation per row):

| scenario | master fixed (ms/stmt) | master marginal (µs/row) | registry fixed (ms/stmt) | registry marginal (µs/row) |
|---|---:|---:|---:|---:|
| small/valid (10k, 100k) | 0.082 | 0.068 | 0.039 | 0.118 |
| fhir/valid (1k, 10k) | 2.777 | 0.531 | 0.025 | 0.455 |

CHECK overhead per 100k inserts (difference of medians vs the no-CHECK baseline): master 6.108 ms, registry 11.149 ms.

pgbench: `-n -c 1 -T 30 -r` (single client, simple protocol, `max_parallel_workers_per_gather = 0`), one run per row.

Raw insert rounds (ms): plain 39.238 29.765 27.437 27.815 27.217; theirs
40.832 36.280 33.592 33.923 33.693; ours 53.375 42.275 38.773 38.964 38.274.

## Interpretation

Both cached paths are about 23–39× faster than recompiling per row (2.69
µs/row uncached vs 0.069–0.119 µs/row cached on the 100k-row small table,
per-statement time divided by rows).

**Small schema: the registry is slower.** Per row it costs 0.118 µs vs
0.068 µs (about 50 ns more per call); per-statement fixed cost is negligible
on both sides. The CHECK path shows the same gap: 11.1 ms vs 6.1 ms of
overhead per 100 000 inserts (about 50 ns more per row). When validation
itself is this cheap, fixed per-call overhead dominates and ours has more of
it. Likely causes (not profiled): `AcceptInvalidationMessages()` on every
call, converting the `name` text argument and building a `String` cache key,
and a hash-map lookup, where theirs reuses the validator from the call
site's `fn_extra` slot.

**FHIR schema: the 10k-row gap is mostly a per-statement cost on theirs.**
Theirs has a fixed cost of 2.78 ms per statement against 0.03 ms for ours;
the likely cause (not profiled) is that theirs receives the 3.4 MB compiled
`jsonschema` value through an InitPlan once per statement, so each statement
detoasts it and recovers its validator from the cache with a fresh
`fn_extra`. The same spot check points the same way: after warm-up a
single-row FHIR statement took 3.19 ms on theirs and 0.38 ms on ours. Per
document the costs are close: 0.455 µs for ours vs 0.531 µs
for theirs (about 0.08 µs, 14% less). From a single two-point run that is
not enough to claim the registry is meaningfully faster per document on
FHIR; it is faster per statement, by a margin that shrinks as tables grow
(0.15× at 1 000 rows, 0.57× at 10 000). The two builds also differ in
jsonschema crate version (0.59 vs 0.58).

Insert round 1 is the slowest for all three tables, ours included; cause not
investigated.
