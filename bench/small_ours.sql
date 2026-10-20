SELECT count(*) FROM docs_small WHERE jsonschema.valid(doc, 'small', 1);
