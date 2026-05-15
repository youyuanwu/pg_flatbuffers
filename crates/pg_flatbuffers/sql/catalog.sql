-- pg_flatbuffers catalog (docs/design.md §4.1)
--
-- Creates:
--   * the `flatbuffers_admin` role (cluster-wide; idempotent)
--   * the `flatbuffers_schemas` catalog table with verifying CHECK
--   * least-privilege grants: SELECT to PUBLIC, write to flatbuffers_admin
--
-- Note: PostgreSQL reserves role names beginning with `pg_`, so we use
-- `flatbuffers_admin` (not `pg_flatbuffers_admin`).

-- Create the admin role idempotently — roles are cluster-wide, so a
-- second `CREATE EXTENSION` in another database in the same cluster
-- would otherwise fail.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'flatbuffers_admin') THEN
        CREATE ROLE flatbuffers_admin NOLOGIN NOINHERIT;
    END IF;
END$$;

-- The catalog itself. `bfbs` carries the binary reflection schema
-- (output of `flatc -b --schema`); the CHECK runs the FlatBuffers
-- verifier so a malformed blob never lands in the table (let alone
-- the schema cache; see docs/design.md §6).
CREATE TABLE @extschema@.flatbuffers_schemas (
    name             text         PRIMARY KEY,
    bfbs             bytea        NOT NULL,
    root_table       text         NOT NULL,
    file_identifier  text,
    inserted_at      timestamptz  NOT NULL DEFAULT now(),
    CONSTRAINT flatbuffers_schemas_bfbs_valid
        CHECK (@extschema@.flatbuffers_validate_schema(bfbs))
);

COMMENT ON TABLE  @extschema@.flatbuffers_schemas IS
    'pg_flatbuffers: registry of binary FlatBuffers reflection schemas';
COMMENT ON COLUMN @extschema@.flatbuffers_schemas.bfbs IS
    'Output of `flatc -b --schema schema.fbs`; verified on INSERT.';
COMMENT ON COLUMN @extschema@.flatbuffers_schemas.root_table IS
    'Fully-qualified name of the schema''s root table.';
COMMENT ON COLUMN @extschema@.flatbuffers_schemas.file_identifier IS
    'Optional 4-byte FlatBuffers file_identifier; used by to_json sanity checks.';

-- Catalog ownership and grants.
ALTER TABLE @extschema@.flatbuffers_schemas OWNER TO flatbuffers_admin;

REVOKE ALL ON TABLE @extschema@.flatbuffers_schemas FROM PUBLIC;
GRANT  SELECT ON TABLE @extschema@.flatbuffers_schemas TO PUBLIC;
GRANT  INSERT, UPDATE, DELETE, TRUNCATE
       ON TABLE @extschema@.flatbuffers_schemas TO flatbuffers_admin;
