#!/usr/bin/env bash
# Benchmark pg_jsonschema_registry against supabase/pg_jsonschema (master).
#
#   bench/run.sh > table.md          # table on stdout, raw output on stderr
#
# Builds the image, starts a throwaway container, loads fixtures, runs one
# single-client pgbench per scenario and timed CHECK-constraint inserts, prints
# a markdown table, and removes the container. Safe to re-run.
# Knobs: DURATION (pgbench seconds, default 30), INSERT_ROUNDS (default 5).
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=pgjsr-bench
NAME=pgjsr-bench-run
DURATION=${DURATION:-30}
INSERT_ROUNDS=${INSERT_ROUNDS:-5}

log() { printf '%s\n' "$*" >&2; }

docker build -t "$IMAGE" -f bench/Dockerfile . >&2

docker rm -f "$NAME" >/dev/null 2>&1 || true
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
docker run -d --name "$NAME" -e POSTGRES_HOST_AUTH_METHOD=trust "$IMAGE" >/dev/null

# The entrypoint runs a socket-only temporary server during initdb; the real
# server is up once TCP answers.
for _ in $(seq 1 120); do
  docker exec "$NAME" pg_isready -q -h 127.0.0.1 -U postgres && break
  sleep 1
done
docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres >&2

psql_c() { docker exec -i "$NAME" psql -X -q -U postgres -d postgres -v ON_ERROR_STOP=1 "$@"; }

log "== versions"
psql_c -At -c "SELECT version()" >&2

log "== setup"
psql_c -f /bench/setup.sql >&2
psql_c -At -c "SELECT extname || ' ' || extversion FROM pg_extension ORDER BY 1" >&2

# pgbench: one client, simple protocol, per-statement latency (-r).
# Prints the per-statement average latency in ms.
bench() {
  local script=$1 out
  log "== pgbench $script"
  out=$(docker exec "$NAME" pgbench -U postgres -n -c 1 -T "$DURATION" -r -f "/bench/$script" postgres 2>&1)
  log "$out"
  awk '/statement latencies/ {s=1; next} s && /SELECT/ {print $1; exit}' <<<"$out"
}

# Fixed run order: theirs before ours in every pair.
small_theirs=$(bench small_theirs.sql)
small_ours=$(bench small_ours.sql)
small10k_theirs=$(bench small10k_theirs.sql)
small10k_ours=$(bench small10k_ours.sql)
fhir_theirs=$(bench fhir_theirs.sql)
fhir_ours=$(bench fhir_ours.sql)
fhir1k_theirs=$(bench fhir1k_theirs.sql)
fhir1k_ours=$(bench fhir1k_ours.sql)
small_uncached=$(bench small_theirs_uncached.sql)

# Timed inserts: INSERT_ROUNDS rounds, each a fresh psql session; report the median.
plain=(); ins_theirs=(); ins_ours=()
for r in $(seq 1 "$INSERT_ROUNDS"); do
  log "== insert+check round $r"
  out=$(psql_c -f /bench/insert_check.sql 2>&1)
  log "$out"
  times=$(awk '/^(plain|theirs|ours)$/ {k=$1; next} /^Time:/ {print k, $2}' <<<"$out")
  plain+=("$(awk '$1=="plain"  {print $2}' <<<"$times")")
  ins_theirs+=("$(awk '$1=="theirs" {print $2}' <<<"$times")")
  ins_ours+=("$(awk '$1=="ours"   {print $2}' <<<"$times")")
done
median() { printf '%s\n' "$@" | sort -g | awk '{a[NR]=$1} END {if (NR%2) print a[(NR+1)/2]; else printf "%.3f\n", (a[NR/2]+a[NR/2+1])/2}'; }
plain_m=$(median "${plain[@]}")
theirs_m=$(median "${ins_theirs[@]}")
ours_m=$(median "${ins_ours[@]}")
log "insert rounds (ms): plain=${plain[*]} theirs=${ins_theirs[*]} ours=${ins_ours[*]}"

ratio() { awk -v a="$1" -v b="$2" 'BEGIN {printf "%.2f", b/a}'; }
per_row() { awk -v ms="$1" -v n="$2" 'BEGIN {printf "%.3f", ms*1000/n}'; }
# Two-point fit: t(n) = fixed + n * marginal. Args: t_small n_small t_big n_big.
marginal_us() { awk -v a="$1" -v na="$2" -v b="$3" -v nb="$4" 'BEGIN {printf "%.3f", (b-a)*1000/(nb-na)}'; }
fixed_ms()    { awk -v a="$1" -v na="$2" -v b="$3" -v nb="$4" 'BEGIN {m=(b-a)/(nb-na); printf "%.3f", a-na*m}'; }

st_m=$(marginal_us "$small10k_theirs" 10000 "$small_theirs" 100000); st_f=$(fixed_ms "$small10k_theirs" 10000 "$small_theirs" 100000)
so_m=$(marginal_us "$small10k_ours" 10000 "$small_ours" 100000);     so_f=$(fixed_ms "$small10k_ours" 10000 "$small_ours" 100000)
ft_m=$(marginal_us "$fhir1k_theirs" 1000 "$fhir_theirs" 10000);      ft_f=$(fixed_ms "$fhir1k_theirs" 1000 "$fhir_theirs" 10000)
fo_m=$(marginal_us "$fhir1k_ours" 1000 "$fhir_ours" 10000);          fo_f=$(fixed_ms "$fhir1k_ours" 1000 "$fhir_ours" 10000)

cat <<TABLE
Per-statement latency (ms), one \`SELECT count(*) ... WHERE <validation>\` over the whole table:

| scenario | rows | pg_jsonschema master (ms) | pg_jsonschema_registry (ms) | registry / master |
|---|---:|---:|---:|---:|
| small/valid | 100000 | $small_theirs | $small_ours | $(ratio "$small_theirs" "$small_ours") |
| small/valid | 10000 | $small10k_theirs | $small10k_ours | $(ratio "$small10k_theirs" "$small10k_ours") |
| fhir/valid | 10000 | $fhir_theirs | $fhir_ours | $(ratio "$fhir_theirs" "$fhir_ours") |
| fhir/valid | 1000 | $fhir1k_theirs | $fhir1k_ours | $(ratio "$fhir1k_theirs" "$fhir1k_ours") |
| insert+check (median of $INSERT_ROUNDS) | 100000 | $theirs_m | $ours_m | $(ratio "$theirs_m" "$ours_m") |
| *context:* small/valid uncached (\`jsonb_matches_schema\`, recompiles per row) | 100000 | $small_uncached ($(per_row "$small_uncached" 100000) µs/row) | n/a | |
| *context:* insert, no CHECK (shared baseline) | 100000 | $plain_m (same table for both) | | |

Two-point fit, t(n) = fixed + n × marginal (fixed includes the scan's per-statement work; marginal includes scan + validation per row):

| scenario | master fixed (ms/stmt) | master marginal (µs/row) | registry fixed (ms/stmt) | registry marginal (µs/row) |
|---|---:|---:|---:|---:|
| small/valid (10k, 100k) | $st_f | $st_m | $so_f | $so_m |
| fhir/valid (1k, 10k) | $ft_f | $ft_m | $fo_f | $fo_m |

CHECK overhead per 100k inserts (difference of medians vs the no-CHECK baseline): master $(awk -v a="$theirs_m" -v b="$plain_m" 'BEGIN{printf "%.3f", a-b}') ms, registry $(awk -v a="$ours_m" -v b="$plain_m" 'BEGIN{printf "%.3f", a-b}') ms.

pgbench: \`-n -c 1 -T $DURATION -r\` (single client, simple protocol, \`max_parallel_workers_per_gather = 0\`), one run per row.
TABLE
