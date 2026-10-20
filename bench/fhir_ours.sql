SELECT count(*) FROM docs_fhir WHERE jsonschema.valid(doc, 'fhir', 1);
