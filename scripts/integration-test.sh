#!/usr/bin/env bash
# Multi-session integration tests (tests/integration.rs) against the Homebrew
# PostgreSQL service. Installs the extension into the tree that PG_CONFIG
# points at, recreates database pgjsr_it, and runs the ignored cargo tests.
set -euo pipefail

cd "$(dirname "$0")/.."

PG_CONFIG="${PG_CONFIG:-/opt/homebrew/opt/postgresql@18/bin/pg_config}"
BINDIR="$("$PG_CONFIG" --bindir)"
DB=pgjsr_it
PGJSR_TEST_DSN="${PGJSR_TEST_DSN:-host=localhost port=5432 dbname=$DB}"

cargo pgrx install --pg-config "$PG_CONFIG"

"$BINDIR/dropdb" --if-exists "$DB"
"$BINDIR/createdb" "$DB"
"$BINDIR/psql" -X -q -d "$DB" -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION pg_jsonschema_registry'

PGJSR_TEST_DSN="$PGJSR_TEST_DSN" cargo test --test integration -- --ignored --test-threads=1
