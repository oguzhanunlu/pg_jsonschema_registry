#!/usr/bin/env bash
# Dump/restore check: a table whose CHECK constraint calls jsonschema.validate
# must survive pg_dump -Fc / pg_restore into a fresh database, with the registry
# rows restored and the constraint still enforced.
#
# Uses databases pgjsr_dump and pgjsr_restore (dropped at start and end) on the
# server that PG_CONFIG's client binaries reach with libpq defaults.
#
# Two cases:
#   1. public.t, plain `pg_restore -d`. Passes because pg_dump emits TABLE DATA
#      entries sorted by schema name and "jsonschema" sorts before "public".
#   2. app.t: "app" sorts before "jsonschema", so a plain restore loads t before
#      the registry rows and its COPY fails ("schema p version 1 is not
#      registered"). This case uses the documented recipe instead:
#        pg_restore --section=pre-data
#        pg_restore --data-only --schema=jsonschema
#        pg_restore --section=data --exclude-schema=jsonschema
#        pg_restore --section=post-data
set -euo pipefail

cd "$(dirname "$0")/.."

PG_CONFIG="${PG_CONFIG:-/opt/homebrew/opt/postgresql@18/bin/pg_config}"
BINDIR="$("$PG_CONFIG" --bindir)"
SRC=pgjsr_dump
DST=pgjsr_restore
DUMP="$(mktemp -t pgjsr_dump.XXXXXX)"

export PGOPTIONS="${PGOPTIONS:-} -c client_min_messages=warning"

psql_() { "$BINDIR/psql" -X -q -v ON_ERROR_STOP=1 "$@"; }
restore() { "$BINDIR/pg_restore" --exit-on-error -d "$DST" "$@" "$DUMP"; }
dropdbs() {
    "$BINDIR/dropdb" --if-exists "$SRC"
    "$BINDIR/dropdb" --if-exists "$DST"
}
cleanup() { rm -f "$DUMP"; dropdbs; }
fail() { echo "dump/restore FAILED: $*" >&2; exit 1; }
trap cleanup EXIT

# run_case <table schema> <plain|recipe>
run_case() {
    local ns="$1" mode="$2"
    dropdbs
    "$BINDIR/createdb" "$SRC"
    psql_ -d "$SRC" -o /dev/null <<SQL
CREATE EXTENSION pg_jsonschema_registry;
SELECT jsonschema.register('p', 1, '{"type":"object","required":["id"]}');
CREATE SCHEMA IF NOT EXISTS $ns;
CREATE TABLE $ns.t (payload jsonb CHECK (jsonschema.validate(payload, 'p', 1)));
INSERT INTO $ns.t VALUES ('{"id":1}'), ('{"id":2}'), ('{"id":3}');
SQL

    "$BINDIR/pg_dump" -Fc -f "$DUMP" "$SRC"
    "$BINDIR/createdb" "$DST"
    if [ "$mode" = plain ]; then
        restore || fail "[$ns] pg_restore exited non-zero"
    else
        restore --section=pre-data || fail "[$ns] pre-data restore failed"
        restore --data-only --schema=jsonschema || fail "[$ns] registry data restore failed"
        restore --section=data --exclude-schema=jsonschema || fail "[$ns] data restore failed"
        restore --section=post-data || fail "[$ns] post-data restore failed"
    fi

    local rows schemas
    rows="$(psql_ -d "$DST" -At -c "SELECT count(*) FROM $ns.t")"
    [ "$rows" = 3 ] || fail "[$ns] expected 3 rows in $ns.t, got $rows"
    schemas="$(psql_ -d "$DST" -At -c 'SELECT count(*) FROM jsonschema.registry')"
    [ "$schemas" = 1 ] || fail "[$ns] expected 1 registry row, got $schemas"
    if psql_ -d "$DST" -c "INSERT INTO $ns.t VALUES ('{}')" >/dev/null 2>&1; then
        fail "[$ns] INSERT of an invalid document succeeded in the restored database"
    fi
    echo "case $ns.t ($mode restore): ok"
}

run_case public plain
run_case app recipe

echo "dump/restore OK"
