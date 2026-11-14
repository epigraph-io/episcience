SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

-- The lock timeout: adding the CHECKs takes an ACCESS EXCLUSIVE lock on
-- `blobs` while they validate the existing rows (every row passes: the new
-- columns take their defaults or are NULL), and the running API and MCP
-- server write that table. On a busy table the migration gives up after 5 s
-- with nothing applied (re-run it) instead of queueing every upload behind it.

-- 5043_external_blobs.sql -- registered external blobs: data that stays where
-- it is (a PVC path, an object store key, a file on an instrument host),
-- registered by URI and hash with no bytes uploaded, and verified in place by
-- a worker that can reach that store.
--
-- WHAT: eight columns on `public.blobs`:
--   storage: 'local' (the bytes are in EPISCIENCE_BLOB_DIR under their BLAKE3
--     hash, as before) or 'external' (the bytes live at location_uri).
--   location_uri: where an external blob's bytes live (`<scheme>://<rest>`);
--     set exactly when storage = 'external'.
--   hash_algorithm: the algorithm of content_hash, 'blake3' or 'sha256'
--     (instrument and campaign manifests are sha256). A local blob is always
--     'blake3': the server computed it.
--   is_manifest: the blob is a manifest (a list of files and their hashes),
--     not a single data file.
--   verify_status, last_verified_at, last_verified_by, last_observed_hash:
--     the latest in-place verification of an external blob: 'ok' (the worker's
--     hash equals content_hash), 'mismatch' (it does not; the observed hash is
--     kept) or 'unreachable' (the worker could not read the bytes). The status
--     is computed by the server from the row's own hash, never taken from the
--     caller. last_verified_by is the verifying agent (no FK: an audit value).
--
-- WHY: instrument and simulation outputs of GB-TB cannot be uploaded into the
-- blob directory, so they could not be evidence without duplicating them.
--
-- EFFECT: no existing row changes (every row becomes storage 'local',
-- hash_algorithm 'blake3', is_manifest false, never verified). Row security,
-- the tenancy guards and the grants on `blobs` are unchanged.
--
-- A pre-5043 binary must never serve a database that holds external rows: it
-- reads every row as a local blob, so its download route would read
-- EPISCIENCE_BLOB_DIR by the caller-declared hash. Undo, on operator request
-- only: docs/runbooks/5043-undo.sql (refuses while an external row exists).
ALTER TABLE public.blobs
    ADD COLUMN storage text NOT NULL DEFAULT 'local',
    ADD COLUMN location_uri text,
    ADD COLUMN hash_algorithm text NOT NULL DEFAULT 'blake3',
    ADD COLUMN is_manifest boolean NOT NULL DEFAULT false,
    ADD COLUMN verify_status text,
    ADD COLUMN last_verified_at timestamp with time zone,
    ADD COLUMN last_verified_by uuid,
    ADD COLUMN last_observed_hash bytea;

ALTER TABLE public.blobs
    ADD CONSTRAINT blobs_storage_known
        CHECK (storage IN ('local', 'external')),
    ADD CONSTRAINT blobs_hash_algorithm_known
        CHECK (hash_algorithm IN ('blake3', 'sha256')),
    ADD CONSTRAINT blobs_location_by_storage
        CHECK ((storage = 'external') = (location_uri IS NOT NULL)),
    ADD CONSTRAINT blobs_location_uri_shape
        CHECK (location_uri IS NULL
               OR (length(location_uri) <= 2048
                   AND location_uri ~ '^[a-z][a-z0-9+.-]*://[^[:space:][:cntrl:]]+$')),
    ADD CONSTRAINT blobs_local_is_blake3
        CHECK (storage = 'external' OR hash_algorithm = 'blake3'),
    ADD CONSTRAINT blobs_verify_status_known
        CHECK (verify_status IS NULL OR verify_status IN ('ok', 'mismatch', 'unreachable')),
    ADD CONSTRAINT blobs_verify_external_only
        CHECK (storage = 'external' OR verify_status IS NULL),
    ADD CONSTRAINT blobs_verify_record_whole
        CHECK ((verify_status IS NULL) = (last_verified_at IS NULL)
               AND (verify_status IS NULL) = (last_verified_by IS NULL)),
    ADD CONSTRAINT blobs_verify_observation
        CHECK (CASE verify_status
                   WHEN 'ok' THEN last_observed_hash IS NOT NULL
                                  AND last_observed_hash = content_hash
                   WHEN 'mismatch' THEN last_observed_hash IS NOT NULL
                                        AND last_observed_hash <> content_hash
                                        AND octet_length(last_observed_hash) = 32
                   ELSE last_observed_hash IS NULL
               END);
