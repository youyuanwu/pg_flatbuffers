-- pg_regress: extension installs, advertises a positive version,
-- and exports the documented top-level surface.
SELECT extname FROM pg_extension WHERE extname = 'pg_flatbuffers';
SELECT flatbuffers_extension_version() > 0 AS version_positive;
-- Spot-check that the documented #[pg_extern]s all exist (one row
-- per function, listed alphabetically for stable diff).
SELECT proname
FROM   pg_proc
WHERE  proname LIKE 'flatbuffers\_%' ESCAPE '\'
ORDER  BY 1;
