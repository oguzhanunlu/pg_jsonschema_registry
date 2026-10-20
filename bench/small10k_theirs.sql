SELECT count(*) FROM docs_small_10k WHERE jsonb_matches_compiled_schema((SELECT s FROM theirs_small), doc);
