SELECT count(*) FROM docs_small WHERE jsonb_matches_schema('{"type":"object","required":["id","name"],"properties":{"id":{"type":"integer"},"name":{"type":"string"}}}'::json, doc);
