-- 100k inserts through a CHECK constraint, timed by psql. ins_plain (no CHECK)
-- is the baseline insert cost. Each round starts from empty tables.
\set ON_ERROR_STOP on
TRUNCATE ins_plain, ins_ours, ins_theirs;
CHECKPOINT;
\timing on
\echo plain
INSERT INTO ins_plain  SELECT doc FROM docs_small;
\echo theirs
INSERT INTO ins_theirs SELECT doc FROM docs_small;
\echo ours
INSERT INTO ins_ours   SELECT doc FROM docs_small;
\timing off
