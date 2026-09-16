EXTENSION = pg_jsonschema_registry
DATA = sql/pg_jsonschema_registry--0.1.0.sql

PG_CONFIG = pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)
