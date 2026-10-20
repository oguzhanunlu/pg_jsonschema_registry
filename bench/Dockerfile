# syntax=docker/dockerfile:1
#
# Benchmark image: postgres:18 with both pg_jsonschema (supabase, master) and
# pg_jsonschema_registry (this repo) installed, plus the HL7 FHIR R4 JSON Schema.
#
#   docker build -t pgjsr-bench -f bench/Dockerfile .
#
# The builder uses Debian trixie so that the headers, glibc and PGDG packages
# match the postgres:18 runtime image (also trixie) exactly.

ARG PG_MAJOR=18
ARG PG_JSONSCHEMA_COMMIT=472830ca7b6ab5d2c9d74d68ee9fc2c07529e60e

############################################################################
FROM rust:1-trixie AS base
ARG PG_MAJOR
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg unzip \
 && install -d /usr/share/postgresql-common/pgdg \
 && curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
      https://www.postgresql.org/media/keys/ACCC4CF8.asc \
 && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt trixie-pgdg main" \
      > /etc/apt/sources.list.d/pgdg.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      postgresql-server-dev-${PG_MAJOR} postgresql-${PG_MAJOR} \
      build-essential clang libclang-dev pkg-config libssl-dev \
      libreadline-dev zlib1g-dev git \
 && rm -rf /var/lib/apt/lists/*
ENV PG_CONFIG=/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

############################################################################
# HL7 FHIR R4 JSON Schema (the file pg_jsonschema's own benchmark discussion
# refers to). Checksummed so a changed upstream file fails the build.
FROM base AS fhir
RUN mkdir -p /bench \
 && curl -fsSL -o /tmp/fhir.schema.json.zip https://hl7.org/fhir/R4/fhir.schema.json.zip \
 && echo "75e5560da3cf503895a44c8ca7af17a83b4cca6c2cb5ba1883d2aec0d1cb5ac6  /tmp/fhir.schema.json.zip" | sha256sum -c - \
 && unzip -q /tmp/fhir.schema.json.zip -d /bench \
 && echo "2230406893b4cf002a4ee1e5e2bbeca22ac5d2d4931b3e9ef7b9594bbc376a01  /bench/fhir.schema.json" | sha256sum -c -

############################################################################
FROM base AS theirs
ARG PG_MAJOR
ARG PG_JSONSCHEMA_COMMIT
RUN cargo install --locked cargo-pgrx --version 0.19.2
RUN cargo pgrx init --pg${PG_MAJOR} ${PG_CONFIG}
RUN git clone https://github.com/supabase/pg_jsonschema /build \
 && cd /build && git checkout ${PG_JSONSCHEMA_COMMIT}
WORKDIR /build
RUN cargo pgrx package --pg-config ${PG_CONFIG} --no-default-features --features pg${PG_MAJOR}

############################################################################
FROM base AS ours
ARG PG_MAJOR
RUN cargo install --locked cargo-pgrx --version 0.19.3
RUN cargo pgrx init --pg${PG_MAJOR} ${PG_CONFIG}
COPY . /src
WORKDIR /src
RUN cargo pgrx package --pg-config ${PG_CONFIG} --no-default-features --features pg${PG_MAJOR}

############################################################################
FROM postgres:18
COPY --from=theirs /build/target/release/pg_jsonschema-pg18/usr/ /usr/
COPY --from=ours   /src/target/release/pg_jsonschema_registry-pg18/usr/ /usr/
COPY --from=fhir   /bench/fhir.schema.json /bench/fhir.schema.json
COPY bench/*.sql /bench/
