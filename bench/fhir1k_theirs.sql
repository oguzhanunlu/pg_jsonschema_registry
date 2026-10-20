SELECT count(*) FROM docs_fhir_1k WHERE jsonb_matches_compiled_schema((SELECT s FROM theirs_fhir), doc);
