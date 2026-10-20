SELECT count(*) FROM docs_small_10k WHERE jsonschema.valid(doc, 'small', 1);
