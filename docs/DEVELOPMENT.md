# Development environment

How the local build/test environment for `pg_jsonschema_registry` is set up
and how to use it on your own. The extension is written in Rust with
[pgrx](https://github.com/pgcentralfoundation/pgrx), and validation is done
by the [`jsonschema`](https://docs.rs/jsonschema) crate.

Everything below was set up on macOS (Apple Silicon) on 2026-10-19/20. Linux
builds happen in Docker (see the end of this document).

## What is installed

| Component | How | Notes |
|---|---|---|
| PostgreSQL 18.6 server + headers | `brew install postgresql@18` | keg-only, so it does not shadow `libpq` unless on PATH |
| Rust stable (1.99.1) | `rustup` (minimal profile + clippy, rustfmt) | lives in `~/.cargo`, `~/.rustup`; `. ~/.cargo/env` in `~/.zshrc` |
| cargo-pgrx 0.19.3 | `cargo install --locked cargo-pgrx` | the pgrx command-line tool; its version must match the `pgrx` crate version in `Cargo.toml` |
| pgrx home | `cargo pgrx init --pg18=/opt/homebrew/opt/postgresql@18/bin/pg_config` | `~/.pgrx/config.toml` + a separate data dir for the test instance |
| Apple clang 21, GNU make, libclang | Xcode Command Line Tools | already present; pgrx's bindgen needs libclang |
| libpq 18.4 (client only) | `brew install libpq` | was already present |
| Docker Desktop | already present | Linux builds, benchmarking against pg_jsonschema |

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

All of these are owned by your user, so installing the extension needs no
`sudo`.

## PATH: the one gotcha

`libpq` was already on your PATH (`/opt/homebrew/opt/libpq/bin`) with its own
`pg_config`. That `pg_config` has no server headers, so pgrx cannot build
against it. Two lines were appended to `~/.zshrc` (backup of the original at
`~/.zshrc.bak-pre-pg18`):

```sh
export PATH="/opt/homebrew/opt/postgresql@18/bin:$PATH"
. "$HOME/.cargo/env"
```

Open a new terminal (or `source ~/.zshrc`) and check:

```sh
which pg_config        # -> /opt/homebrew/opt/postgresql@18/bin/pg_config
pg_config --version    # -> PostgreSQL 18.6 (Homebrew)
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
superuser is your macOS user, no password.

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

## Version coupling to remember

- The `pgrx` version in `Cargo.toml` and the installed `cargo-pgrx` must
  match exactly. When bumping one, bump the other:
  `cargo install --locked cargo-pgrx --version <x.y.z>`.
- After a macOS / Xcode update, re-run
  `cargo pgrx init --pg18=/opt/homebrew/opt/postgresql@18/bin/pg_config`.
- After `brew upgrade postgresql@18` (minor version), rebuild and reinstall
  the extension; the headers may have changed.

## Toolchain smoke test

What was run to prove the Rust path works end to end: `cargo pgrx new
hello_pgrx` in a scratch directory, then `cargo pgrx test pg18`, which
compiled the template extension, installed it into the managed instance,
started that instance, and ran the template's `#[pg_test]`. If you ever doubt
the toolchain, repeat exactly that in a scratch directory.

(Before the switch to Rust, a throwaway C extension was also compiled with
PGXS and run against the Homebrew service, so the C toolchain and the server
headers are known good too.)

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
# then delete the two lines added to ~/.zshrc (or restore ~/.zshrc.bak-pre-pg18)
```
