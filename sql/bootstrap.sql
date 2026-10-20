CREATE SCHEMA jsonschema;
GRANT USAGE ON SCHEMA jsonschema TO PUBLIC;

CREATE TABLE jsonschema.registry (
    id          bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        text        NOT NULL CHECK (name <> ''),
    version     int         NOT NULL CHECK (version > 0),
    schema      jsonb       NOT NULL,
    draft       text        NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    created_by  text        NOT NULL DEFAULT current_user,
    UNIQUE (name, version)
);

CREATE TABLE jsonschema.ids (
    uri        text   PRIMARY KEY,
    schema_id  bigint NOT NULL REFERENCES jsonschema.registry(id) ON DELETE CASCADE
);

CREATE TABLE jsonschema.refs (
    schema_id  bigint NOT NULL REFERENCES jsonschema.registry(id) ON DELETE CASCADE,
    ref_id     bigint NOT NULL REFERENCES jsonschema.registry(id),
    PRIMARY KEY (schema_id, ref_id)
);
CREATE INDEX refs_ref_id_idx ON jsonschema.refs (ref_id);

CREATE VIEW jsonschema.schemas AS
SELECT r.name, r.version, r.draft, i.uri AS id_uri, r.created_at, r.created_by,
       (SELECT array_agg(d.name || '/' || d.version ORDER BY d.name, d.version)
          FROM jsonschema.refs f JOIN jsonschema.registry d ON d.id = f.ref_id
         WHERE f.schema_id = r.id) AS refs
  FROM jsonschema.registry r
  LEFT JOIN jsonschema.ids i ON i.schema_id = r.id;

GRANT SELECT ON jsonschema.registry, jsonschema.ids, jsonschema.refs, jsonschema.schemas TO PUBLIC;

SELECT pg_catalog.pg_extension_config_dump('jsonschema.registry', '');
SELECT pg_catalog.pg_extension_config_dump('jsonschema.registry_id_seq', '');
SELECT pg_catalog.pg_extension_config_dump('jsonschema.ids', '');
SELECT pg_catalog.pg_extension_config_dump('jsonschema.refs', '');
