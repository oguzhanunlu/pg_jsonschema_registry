-- Benchmark fixtures for pg_jsonschema (master) vs pg_jsonschema_registry.
-- Run once against a fresh database: psql -v ON_ERROR_STOP=1 -f /bench/setup.sql
\set ON_ERROR_STOP on

CREATE EXTENSION pg_jsonschema;
CREATE EXTENSION pg_jsonschema_registry;

-- Measure per-call cost on one core: no parallel query in any session.
ALTER DATABASE postgres SET max_parallel_workers_per_gather = 0;
SET max_parallel_workers_per_gather = 0;

\set small_schema '{"type":"object","required":["id","name"],"properties":{"id":{"type":"integer"},"name":{"type":"string"}}}'

-- ours: named, versioned schemas
SELECT jsonschema.register('small', 1, :'small_schema'::jsonb) AS small_version;
SELECT jsonschema.register('fhir', 1, pg_read_file('/bench/fhir.schema.json')::jsonb) AS fhir_version;

-- theirs: the compiled-schema type, stored in one-row tables
CREATE TABLE theirs_small(s jsonschema);
INSERT INTO theirs_small SELECT :'small_schema'::json::jsonschema;
CREATE TABLE theirs_fhir(s jsonschema);
INSERT INTO theirs_fhir SELECT pg_read_file('/bench/fhir.schema.json')::json::jsonschema;

-- 100k small documents, all valid
CREATE TABLE docs_small AS
SELECT i AS id, jsonb_build_object('id', i, 'name', 'name-' || i) AS doc
FROM generate_series(1, 100000) AS i;

-- 10k FHIR R4 Patient resources from a fixed template, varying id/name/birthDate
CREATE TABLE docs_fhir AS
SELECT i AS id,
       jsonb_build_object(
         'resourceType', 'Patient',
         'id', 'pat-' || i,
         'active', true,
         'name', jsonb_build_array(jsonb_build_object(
                   'use', 'official',
                   'family', 'Family' || i,
                   'given', jsonb_build_array('Given' || i))),
         'gender', (ARRAY['male','female','other','unknown'])[1 + i % 4],
         'birthDate', to_char(date '1950-01-01' + (i % 20000), 'YYYY-MM-DD')
       ) AS doc
FROM generate_series(1, 10000) AS i;

-- CHECK-constraint targets. A CHECK cannot contain a subquery, so theirs
-- embeds the schema as a jsonschema constant (same schema text as theirs_small).
CREATE TABLE ins_plain (payload jsonb);
CREATE TABLE ins_ours  (payload jsonb CHECK (jsonschema.valid(payload, 'small', 1)));
CREATE TABLE ins_theirs(payload jsonb CHECK (jsonb_matches_compiled_schema(:'small_schema'::json::jsonschema, payload)));

VACUUM ANALYZE docs_small, docs_fhir, theirs_small, theirs_fhir;

-- Sanity: both extensions must accept every generated document and reject a
-- broken one, otherwise the benchmark measures nothing.
DO $$
DECLARE
  bad_small jsonb := '{"id":"x","name":1}';
  bad_fhir  jsonb := '{"resourceType":"Patient","id":"pat-x","gender":"robot"}';
BEGIN
  IF (SELECT count(*) FROM docs_small WHERE jsonschema.valid(doc, 'small', 1)) <> 100000 THEN
    RAISE EXCEPTION 'ours rejects some small docs'; END IF;
  IF (SELECT count(*) FROM docs_small WHERE jsonb_matches_compiled_schema((SELECT s FROM theirs_small), doc)) <> 100000 THEN
    RAISE EXCEPTION 'theirs rejects some small docs'; END IF;
  IF (SELECT count(*) FROM docs_fhir WHERE jsonschema.valid(doc, 'fhir', 1)) <> 10000 THEN
    RAISE EXCEPTION 'ours rejects some FHIR docs'; END IF;
  IF (SELECT count(*) FROM docs_fhir WHERE jsonb_matches_compiled_schema((SELECT s FROM theirs_fhir), doc)) <> 10000 THEN
    RAISE EXCEPTION 'theirs rejects some FHIR docs'; END IF;
  IF jsonschema.valid(bad_small, 'small', 1) OR jsonb_matches_compiled_schema((SELECT s FROM theirs_small), bad_small) THEN
    RAISE EXCEPTION 'a broken small doc was accepted'; END IF;
  IF jsonschema.valid(bad_fhir, 'fhir', 1) OR jsonb_matches_compiled_schema((SELECT s FROM theirs_fhir), bad_fhir) THEN
    RAISE EXCEPTION 'a broken FHIR doc was accepted'; END IF;
  RAISE NOTICE 'sanity checks passed: both extensions accept all fixtures and reject broken docs';
END
$$;

SELECT doc AS sample_patient FROM docs_fhir WHERE id = 1;
