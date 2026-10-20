# Development environment

How to set up a local build and test environment for `pg_jsonschema_registry`
on macOS with Homebrew, and how to use it. The extension is written in Rust with
[pgrx](https://github.com/pgcentralfoundation/pgrx), and validation is done
by the [`jsonschema`](https://docs.rs/jsonschema) crate.

The paths below are those of Homebrew on Apple Silicon (`/opt/homebrew`); on
Intel Macs Homebrew uses `/usr/local` instead. Linux builds happen in Docker
(see the end of this document).

## What to install

| Component | How | Notes |
|---|---|---|
| PostgreSQL 18 server + headers | `brew install postgresql@18` | keg-only, so it does not shadow `libpq` unless on PATH |
| Rust stable | `rustup` (minimal profile + clippy, rustfmt) | lives in `~/.cargo`, `~/.rustup`; add `. "$HOME/.cargo/env"` to your shell profile |
| cargo-pgrx 0.19.3 | `cargo install --locked cargo-pgrx` | the pgrx command-line tool; its version must match the `pgrx` crate version in `Cargo.toml` |
| pgrx home | `cargo pgrx init --pg18=/opt/homebrew/opt/postgresql@18/bin/pg_config` | `~/.pgrx/config.toml` + a separate data dir for the test instance |
| Apple clang, GNU make, libclang | Xcode Command Line Tools (`xcode-select --install`) | pgrx's bindgen needs libclang |
| libpq (client only) | `brew install libpq` | optional; see the PATH note below |
| Docker Desktop | from docker.com | optional; Linux builds, benchmarking against pg_jsonschema |

Paths that matter:

```
server binaries         /opt/homebrew/opt/postgresql@18/bin   (pg_config, psql, pg_ctl, initdb)
extension install dirs  pg_config --pkglibdir  -> /opt/homebrew/lib/postgresql@18
                        pg_config --sharedir   -> /opt/homebrew/share/postgresql@18  (…/extension/)
brew service data dir   /opt/homebrew/var/postgresql@18         (port 5432)
brew service log        /opt/homebrew/var/log/postgresql@18.log
pgrx config             ~/.pgrx/config.toml
pgrx test data dir      ~/.pgrx/data-18                          (port 28818, managed by cargo pgrx)
```

With Homebrew these are owned by your user, so installing the extension
needs no `sudo`.

## PATH: the one gotcha

If Homebrew's `libpq` is on your PATH (`/opt/homebrew/opt/libpq/bin`), its
`pg_config` comes first. That `pg_config` has no server headers, so pgrx
cannot build against it. Put the server's binaries first, for example in
`~/.zshrc`:

```sh
export PATH="/opt/homebrew/opt/postgresql@18/bin:$PATH"
. "$HOME/.cargo/env"
```

Open a new terminal (or `source ~/.zshrc`) and check:

```sh
which pg_config        # -> /opt/homebrew/opt/postgresql@18/bin/pg_config
pg_config --version    # -> PostgreSQL 18.x (Homebrew)
cargo pgrx --version   # -> cargo-pgrx 0.19.3
```

pgrx itself does not depend on PATH for the server: it reads
`~/.pgrx/config.toml`, which records the exact `pg_config` registered with
`cargo pgrx init`.

## Two Postgres instances, on purpose

1. **The Homebrew service** on port 5432. Long-lived, for ad-hoc `psql`,
   benchmarks, and `cargo pgrx install` + `CREATE EXTENSION` by hand.
2. **The pgrx-managed instance** on port 28818, data in `~/.pgrx/data-18`.
   Started and stopped by `cargo pgrx run` / `cargo pgrx test`. Throwaway.

Both come from the same binaries and share the same extension directories:
because pgrx was registered against the Homebrew `pg_config`, **`cargo pgrx
test` and `cargo pgrx run` install the extension into the Homebrew tree**
(`/opt/homebrew/lib/postgresql@18/<ext>.dylib` and
`/opt/homebrew/share/postgresql@18/extension/<ext>*`). That is normal pgrx
behaviour with a system Postgres and is why those directories must be
writable by you. The two instances' data directories are separate.

### Homebrew service

```sh
brew services start postgresql@18     # start now and at every login
brew services stop postgresql@18      # stop and disable at login
brew services restart postgresql@18   # after changing postgresql.conf
pg_isready                            # does it accept connections?
psql -d postgres -c 'select version()'
```

Connection defaults: Unix socket in `/tmp`, port 5432, database `postgres`,
superuser is the macOS user that ran `brew install`, no password.

To wipe it and start over:

```sh
brew services stop postgresql@18
rm -rf /opt/homebrew/var/postgresql@18
initdb --locale=en_US.UTF-8 -E UTF-8 /opt/homebrew/var/postgresql@18
brew services start postgresql@18
```

## The pgrx workflow

All commands run from the repository root (where `Cargo.toml` is).

```sh
cargo pgrx run pg18        # build, install into pg18, start the managed instance, open psql
cargo pgrx test pg18       # build with the pg_test feature, run #[pg_test] tests inside a real backend
cargo pgrx install         # build (debug) and install into the pg_config from ~/.pgrx/config.toml
cargo pgrx install --release
cargo pgrx package         # release build laid out as an install tree under target/release/<ext>-pg18/
cargo pgrx schema          # print the generated SQL (what CREATE EXTENSION will run)
cargo pgrx stop pg18       # stop the managed instance
```

Notes:

- `cargo pgrx install` writes into `/opt/homebrew/lib/postgresql@18` and
  `/opt/homebrew/share/postgresql@18/extension`, i.e. the Homebrew service
  picks it up. Then in `psql -d postgres`:
  `CREATE EXTENSION pg_jsonschema_registry;`
- A backend that has already loaded the old `.dylib` keeps it until that
  backend exits. After `cargo pgrx install`, reconnect (`\q` then `psql`
  again) before `DROP EXTENSION` / `CREATE EXTENSION`.
- Tests: each `#[pg_test]` runs inside a transaction that is rolled back
  afterwards, inside the managed instance. The first `cargo pgrx test` on a
  fresh checkout compiles pgrx itself and takes several minutes; later runs
  are incremental.
- `cargo pgrx test pg18 some_name` runs only tests whose name contains
  `some_name`.
- Plain `cargo build` / `cargo clippy` also work and are faster for checking
  compile errors, but they do not install or test anything.

## Integration tests

Two scripts test what `#[pg_test]` cannot: several backends at once, and a
real `pg_dump` / `pg_restore`. Both run against **the Homebrew service on port
5432**, which must be running (`brew services start postgresql@18`,
`pg_isready`). Both work from any directory and are safe to rerun.

```sh
scripts/integration-test.sh     # multi-session cache coherence + parallel workers
scripts/dump-restore-test.sh    # CHECK (jsonschema.validate(...)) survives dump/restore
```

`scripts/integration-test.sh` runs `cargo pgrx install` into the tree
`PG_CONFIG` points at, drops and recreates the database **`pgjsr_it`**, runs
`CREATE EXTENSION pg_jsonschema_registry` there, then
`cargo test --test integration -- --ignored --test-threads=1`. The tests in
`tests/integration.rs` are `#[ignore]`d, so plain `cargo test` and
`cargo pgrx test` skip them. They connect with the `postgres` crate, two
clients being two backends. `--test-threads=1` matters: a concurrent test's
registry commit can keep another test's cache cold, which would let a
hit-path check pass without exercising the hit path. To run them by hand
against an existing database with the extension installed:

```sh
PGJSR_TEST_DSN="host=localhost port=5432 dbname=pgjsr_it" \
  cargo test --test integration -- --ignored --test-threads=1
```

`scripts/dump-restore-test.sh` creates **`pgjsr_dump`** (extension, schema
`p` version 1, a table with a `CHECK (jsonschema.validate(payload, 'p', 1))`
and 3 rows), dumps it with `pg_dump -Fc`, restores into **`pgjsr_restore`**,
and checks the rows, the registry and that the constraint still rejects
`'{}'`. It drops both databases at the start and at exit, and prints
`dump/restore OK`. It runs the check twice: once with the table in `public`
and a plain `pg_restore`, and once with the table in schema `app` using the
ordered recipe (`--section=pre-data`; `--data-only --schema=jsonschema`;
`--section=data --exclude-schema=jsonschema`; `--section=post-data`). The
second case exists because pg_dump orders table data by schema name: a table
in a schema that sorts before `jsonschema` has its data restored before the
registry rows, and a plain restore then fails.

Environment variables:

| Variable | Default | Used by |
|---|---|---|
| `PG_CONFIG` | `/opt/homebrew/opt/postgresql@18/bin/pg_config` | both scripts (install target, client binaries via `--bindir`) |
| `PGJSR_TEST_DSN` | `host=localhost port=5432 dbname=pgjsr_it` | `integration-test.sh`, `tests/integration.rs` |

## Lint

CI runs these two checks; run them before pushing (both must print nothing
but the cargo progress lines):

```sh
cargo fmt --check
cargo clippy --features pg18 -- -D warnings
cargo clippy --features pg18,pg_test --tests -- -D warnings   # also lints the #[pg_test] code
```

`cargo fmt` fixes formatting in place.

## Benchmarks

`docs/benchmarks.md` has the method, results and caveats of the comparison
with pg_jsonschema `master`; `bench/run.sh` reproduces it in Docker.

## CI

`.github/workflows/ci.yml` runs `pgrx-build-test` (from the `pgxn/pgxn-tools`
image) on PostgreSQL 15 to 18, plus the lint job above. It cannot be run
locally.

## Version coupling to remember

- The `pgrx` version in `Cargo.toml` and the installed `cargo-pgrx` must
  match exactly. When bumping one, bump the other:
  `cargo install --locked cargo-pgrx --version <x.y.z>`.
- After a macOS / Xcode update, re-run
  `cargo pgrx init --pg18=/opt/homebrew/opt/postgresql@18/bin/pg_config`.
- After `brew upgrade postgresql@18` (minor version), rebuild and reinstall
  the extension; the headers may have changed.

## Toolchain smoke test

To check the toolchain end to end, run `cargo pgrx new hello_pgrx` in a
scratch directory, then `cargo pgrx test pg18` there. It compiles the template
extension, installs it into the managed instance, starts that instance, and
runs the template's `#[pg_test]`.

## Inspecting the cache

`SELECT * FROM jsonschema.cache_stats();` shows the calling backend's cache
counters: `compiles` (schema compilations so far), `entries` (validators cached
now) and `invalidations` (times a registry invalidation cleared the cache). The
numbers are per backend, so run it in the session you are looking at. The
integration tests use `compiles` to check that a transaction compiles a schema
once and then hits the cache.

## Linux builds and benchmarking (Docker)

Not set up yet. Planned: a `Dockerfile` based on the official `postgres:18`
image plus the PGDG `postgresql-server-dev-18` package, rustup and
cargo-pgrx, for (1) checking the build with gcc on Linux and (2) benchmarking
side by side with Supabase's `pg_jsonschema` built from its `master` branch,
which is the version with compiled-schema caching. The `pgxn/pgxn-tools`
image has a `pgrx-build-test` helper that does most of this.

## Removing everything

```sh
brew services stop postgresql@18 && brew uninstall postgresql@18
rustup self uninstall                       # removes ~/.cargo and ~/.rustup, including cargo-pgrx
rm -rf ~/.pgrx
# then remove the PATH and cargo lines from your shell profile
```
