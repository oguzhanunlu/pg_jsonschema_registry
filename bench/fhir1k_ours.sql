SELECT count(*) FROM docs_fhir_1k WHERE jsonschema.valid(doc, 'fhir', 1);
