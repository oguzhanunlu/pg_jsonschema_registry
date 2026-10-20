SELECT count(*) FROM docs_small WHERE jsonb_matches_compiled_schema((SELECT s FROM theirs_small), doc);
